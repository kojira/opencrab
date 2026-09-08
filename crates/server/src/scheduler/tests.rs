use super::*;

const AGENT_UUID: &str = "11111111-1111-4111-8111-111111111111";

/// #899 / §12.6: schedule 発火ターンの応答が NO_REPLY のみなら speech を保存しない
/// （生保存経路 `run_one_schedule` も終端処理を通す）。現 tip で赤（生応答 "NO_REPLY" が
/// `content='NO_REPLY'` として保存される）。
#[tokio::test]
async fn no_reply_only_schedule_turn_persists_no_speech_899() {
    let agent = "sched-agent-899";
    let mock = std::sync::Arc::new(crate::bin_test_support::FixedTextMock::new("NO_REPLY"));
    let state = crate::bin_test_support::app_state_with_agent(mock, agent);
    let session_id = format!("schedule-{agent}");
    let out = run_one_schedule(&state, agent, &session_id, "定時メッセージ").await;
    assert!(
        out.is_some(),
        "schedule ターンが走らない（テスト前提の破綻）"
    );
    assert_eq!(
        crate::bin_test_support::count_no_reply_speech(&state, &session_id, agent),
        0,
        "schedule で NO_REPLY のみが speech 保存された（#899・§12.6）"
    );
}

/// テスト用の登録簿: **本番と同じ源**（`register_production_descriptors`）で descriptor を
/// 積む（#628）。本番へ transport を足せば scheduler テストの登録簿も自動で追随する
/// （各所での register の散らしを避ける・ブロッカー対応）。rebuild_entries は登録簿へ
/// 問い合わせて発火先を解決するので、ここは scheduler がその登録簿を正しく引くことを見る。
fn test_router() -> opencrab_actions::TimedFireRouter {
    let router = opencrab_actions::TimedFireRouter::new();
    opencrab_server::register_production_descriptors(&router);
    router
}

/// scheduler は登録簿経由で発火先を解決する（Discord は G ゲート対象・Nostr は非対象）。
// #654: cfg が要る本当の理由は nostr と discord の 2 つの `.expect()`。NostrFire / DiscordFire
// descriptor は各 feature 時のみ登録される（#651）ので、両方が揃わないと nostr/discord の
// resolve_target が None を返し `.expect()` が落ちる。web=None は feature 由来ではない
// （`web-{UUID}` は会話セグメントが無く WebFire の parse が全構成で成立しないため常に None）。
#[cfg(all(feature = "nostr", feature = "discord"))]
#[test]
fn router_resolves_and_reports_g_gate() {
    let router = test_router();
    let nostr = router
        .resolve_target(&format!("nostr-{AGENT_UUID}"), AGENT_UUID)
        .expect("nostr が解決できない");
    assert!(!router.descriptor(nostr.kind).unwrap().is_g_gated());

    let discord = router
        .resolve_target(&format!("discord-{AGENT_UUID}-1001-2002"), AGENT_UUID)
        .expect("discord が解決できない");
    assert!(router.descriptor(discord.kind).unwrap().is_g_gated());

    // 発火経路の無い種別は None（fail-closed）。
    assert!(router
        .resolve_target(&format!("web-{AGENT_UUID}"), AGENT_UUID)
        .is_none());
}

#[test]
fn later_of_picks_the_later() {
    let a = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let b = DateTime::parse_from_rfc3339("2026-01-01T01:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(later_of(Some(a), Some(b)), Some(b));
    assert_eq!(later_of(Some(b), Some(a)), Some(b));
    assert_eq!(later_of(Some(a), None), Some(a));
    assert_eq!(later_of(None, Some(b)), Some(b));
    assert_eq!(later_of(None, None), None);
}

// ---- rebuild / 発火集合の不変条件（設計 §4.2 / §5・#5） ----

use chrono::Duration;
use opencrab_db::queries::AgentScheduleRow;
// #654: SessionHeartbeatConfigRow は heartbeat 発火集合テスト（router 解決に依存＝nostr feature
// が要る・#651）専用の helper でしか使わないので、その cfg に合わせて import も囲む。
#[cfg(feature = "nostr")]
use opencrab_db::queries::SessionHeartbeatConfigRow;
use std::collections::HashMap;

// #654: 2 エージェント目は heartbeat 発火集合テスト（nostr feature 依存・#651）専用。
#[cfg(feature = "nostr")]
const AGENT_B: &str = "c56f19e0-1111-2222-3333-444455556666";

fn hb_key(session: &str) -> EntryKey {
    EntryKey::Heartbeat {
        session_id: session.to_string(),
    }
}

// #654: heartbeat 発火集合テスト（nostr feature 依存・上記 import 参照）専用の helper。
#[cfg(feature = "nostr")]
fn row(
    agent: &str,
    session: &str,
    enabled: bool,
    interval: Option<i64>,
    anchor: Option<String>,
    last: Option<String>,
) -> SessionHeartbeatConfigRow {
    SessionHeartbeatConfigRow {
        agent_id: agent.to_string(),
        session_id: session.to_string(),
        enabled,
        interval_secs: interval,
        anchor_at: anchor,
        last_fired_at: last,
    }
}

#[cfg(feature = "nostr")]
fn conn_with(rows: &[SessionHeartbeatConfigRow]) -> rusqlite::Connection {
    let conn = opencrab_db::init_memory().unwrap();
    for r in rows {
        opencrab_db::queries::upsert_session_heartbeat_config(&conn, r).unwrap();
    }
    conn
}

#[cfg(feature = "nostr")]
fn session_ids(entries: &[Entry]) -> std::collections::BTreeSet<String> {
    entries.iter().map(|e| e.session_id.clone()).collect()
}

/// 本番の発火集合（Nostr 2 + Discord 1）を模した fixture で、**live G が
/// `discord-` だけをゲートし `nostr-` は非依存**であることを固定する（不変条件 #5）。
// #654: nostr 2 + discord 1 の発火集合を検証する。NostrFire / DiscordFire descriptor は各 feature
// 時のみ登録される（#651）ので両 feature が要る。off では router が空で発火集合が常に空になる。
#[cfg(all(feature = "nostr", feature = "discord"))]
#[test]
fn firing_set_gates_discord_by_live_g_only() {
    let past = (Utc::now() - Duration::hours(12)).to_rfc3339();
    let nostr_a = format!("nostr-{AGENT_UUID}");
    let nostr_b = format!("nostr-{AGENT_B}");
    let discord_c = format!("discord-{AGENT_UUID}-1001-2002");
    let rows = [
        row(
            AGENT_UUID,
            &nostr_a,
            true,
            Some(18000),
            Some(past.clone()),
            None,
        ),
        row(
            AGENT_B,
            &nostr_b,
            true,
            Some(1200),
            Some(past.clone()),
            None,
        ),
        row(
            AGENT_UUID,
            &discord_c,
            true,
            Some(10800),
            Some(past.clone()),
            None,
        ),
        // opt-in 抑止で enabled=0 に焼かれた Discord 行（発火しない・list_enabled が返さない）。
        row(
            AGENT_B,
            &format!("discord-{AGENT_B}-1001-3003"),
            false,
            Some(600),
            None,
            None,
        ),
    ];
    let conn = conn_with(&rows);
    let attempts = HashMap::new();

    // G = true: 3 セッションすべて。
    let with_g = rebuild_entries(&test_router(), &conn, true, 1800, 300, &attempts);
    assert_eq!(
        session_ids(&with_g),
        [nostr_a.clone(), nostr_b.clone(), discord_c.clone()]
            .into_iter()
            .collect(),
        "G=true では nostr 2 + discord 1 が発火対象"
    );
    assert!(with_g
        .iter()
        .all(|e| e.next_fire_at.map(|t| t <= Utc::now()).unwrap_or(true)));

    // G = false: discord はゲートで消え、nostr 2 だけ残る（nostr は G 非依存）。
    let without_g = rebuild_entries(&test_router(), &conn, false, 1800, 300, &attempts);
    assert_eq!(
        session_ids(&without_g),
        [nostr_a, nostr_b].into_iter().collect(),
        "G=false では discord- がゲートされ nostr- のみ発火（nostr は G 非依存）"
    );
}

/// 壊れた session_id / interval は fail-closed で発火集合に入らない。
// #654: heartbeat rebuild は発火先を router で解決する。NostrFire descriptor は nostr feature 時
// のみ登録される（#651）ので、正常な nostr 行が発火集合に載ることを見るこの test は同じ cfg が要る。
#[cfg(feature = "nostr")]
#[test]
fn rebuild_skips_malformed_and_broken_interval() {
    let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
    let good = format!("nostr-{AGENT_UUID}");
    let rows = [
        row(AGENT_UUID, &good, true, Some(600), Some(past.clone()), None),
        row(
            AGENT_UUID,
            &format!("web-{AGENT_UUID}"),
            true,
            Some(600),
            Some(past.clone()),
            None,
        ),
        row(
            AGENT_B,
            &format!("nostr-{AGENT_B}"),
            true,
            Some(0),
            Some(past.clone()),
            None,
        ),
    ];
    let conn = conn_with(&rows);
    let entries = rebuild_entries(&test_router(), &conn, true, 1800, 300, &HashMap::new());
    assert_eq!(session_ids(&entries), [good].into_iter().collect());
}

// ---- ビジーループ回避（A1・#6） ----

fn entry_at(session: &str, next: Option<DateTime<Utc>>) -> Entry {
    Entry {
        key: hb_key(session),
        agent_id: AGENT_UUID.to_string(),
        session_id: session.to_string(),
        next_fire_at: next,
        kind: FireKind::Heartbeat {
            target: FireTarget {
                kind: opencrab_actions::gateway_kinds::NOSTR,
                channel_id: String::new(),
                guild_id: String::new(),
                route: String::new(),
            },
        },
    }
}

/// in-flight エントリは sleep 候補から除外され、0 秒スピンしない（A1）。
#[test]
fn sleep_excludes_in_flight_and_never_spins_to_zero() {
    let now = Utc::now();
    let mut in_flight = HashSet::new();
    in_flight.insert(hb_key("s-running"));

    let running_future = entry_at("s-running", Some(now + Duration::seconds(10)));
    let idle_future = entry_at("s-future", Some(now + Duration::seconds(120)));
    assert_eq!(
        next_sleep_secs(&[running_future, idle_future], &in_flight, now),
        120,
        "in-flight の未来エントリを除外し、非 in-flight の 120s まで眠る"
    );

    let running_stuck = entry_at("s-running", Some(now - Duration::seconds(30)));
    assert_eq!(
        next_sleep_secs(&[running_stuck], &in_flight, now),
        MAX_SLEEP_SECS,
        "走行中 due だけなら 0 秒スピンせず MAX_SLEEP（完了 wake で拾う）"
    );

    let far = entry_at("s-far", Some(now + Duration::hours(2)));
    assert_eq!(
        next_sleep_secs(&[far], &HashSet::new(), now),
        MAX_SLEEP_SECS
    );
}

/// schedule のキーは同一セッションでも別スケジュールを誤ブロックしない（EntryKey enum の要点）。
#[test]
fn schedule_keys_do_not_cross_block_same_session() {
    let now = Utc::now();
    let session = format!("nostr-{AGENT_UUID}");
    let mut in_flight = HashSet::new();
    in_flight.insert(EntryKey::Schedule { schedule_id: 1 });

    let running = Entry {
        key: EntryKey::Schedule { schedule_id: 1 },
        agent_id: AGENT_UUID.to_string(),
        session_id: session.clone(),
        next_fire_at: Some(now - Duration::seconds(5)),
        kind: FireKind::ScheduledMessage {
            message: "a".into(),
        },
    };
    // 同一セッションの別スケジュール（id=2）は in_flight に居ないので sleep 候補に残る。
    let sibling = Entry {
        key: EntryKey::Schedule { schedule_id: 2 },
        agent_id: AGENT_UUID.to_string(),
        session_id: session,
        next_fire_at: Some(now + Duration::seconds(90)),
        kind: FireKind::ScheduledMessage {
            message: "b".into(),
        },
    };
    assert_eq!(
        next_sleep_secs(&[running, sibling], &in_flight, now),
        90,
        "id=1 が走行中でも id=2（同一セッション）は別キーなので候補に残る"
    );
}

// ---- missed-run 圧縮 / アンカーの向き（§8 / §4.4 / §3.7 N-a） ----

// #654: heartbeat rebuild は router で発火先を解決する。NostrFire は nostr feature 時のみ登録
// される（#651）。off では発火集合が空になり before[0] が取れないので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[test]
fn missed_run_compresses_to_one_and_success_pushes_forward() {
    let long_ago = (Utc::now() - Duration::days(3)).to_rfc3339();
    let sid = format!("nostr-{AGENT_UUID}");
    let conn = conn_with(&[row(AGENT_UUID, &sid, true, Some(600), Some(long_ago), None)]);

    let before = rebuild_entries(&test_router(), &conn, true, 1800, 300, &HashMap::new());
    assert_eq!(before.len(), 1);
    assert!(before[0]
        .next_fire_at
        .map(|t| t <= Utc::now())
        .unwrap_or(true));

    let now_str = Utc::now().to_rfc3339();
    opencrab_db::queries::set_session_last_fired(&conn, AGENT_UUID, &sid, &now_str).unwrap();

    let after = rebuild_entries(&test_router(), &conn, true, 1800, 300, &HashMap::new());
    assert_eq!(after.len(), 1);
    assert!(
        after[0]
            .next_fire_at
            .map(|t| t > Utc::now())
            .unwrap_or(false),
        "発火成功後は next_fire が未来へ後退する（密にしない）"
    );
}

// #654: heartbeat rebuild は router で発火先を解決する。NostrFire は nostr feature 時のみ登録
// される（#651）。off では発火集合が空になり due[0] が取れないので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[test]
fn last_attempt_backoff_defers_next_fire_without_touching_last_fired() {
    let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
    let sid = format!("nostr-{AGENT_UUID}");
    let conn = conn_with(&[row(AGENT_UUID, &sid, true, Some(600), Some(past), None)]);

    let due = rebuild_entries(&test_router(), &conn, true, 1800, 300, &HashMap::new());
    assert!(due[0].next_fire_at.map(|t| t <= Utc::now()).unwrap_or(true));

    let mut attempts = HashMap::new();
    attempts.insert(hb_key(&sid), Utc::now());
    let deferred = rebuild_entries(&test_router(), &conn, true, 1800, 300, &attempts);
    assert!(
        deferred[0]
            .next_fire_at
            .map(|t| t > Utc::now())
            .unwrap_or(false),
        "last_attempt が next_fire を interval ぶん後ろへ逃がす"
    );
}

// ---- panic 経路の統合確認（§6 / §3.7 N-a） ----

#[tokio::test]
async fn panic_in_fire_clears_in_flight_keeps_attempt_and_wakes() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let in_flight: Arc<Mutex<HashSet<EntryKey>>> = Arc::new(Mutex::new(HashSet::new()));
    let attempts: Arc<Mutex<HashMap<EntryKey, DateTime<Utc>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let wake = Arc::new(tokio::sync::Notify::new());
    let key = hb_key(&format!("nostr-{AGENT_UUID}"));

    in_flight.lock().unwrap().insert(key.clone());
    attempts.lock().unwrap().insert(key.clone(), Utc::now());

    let woken = Arc::new(AtomicBool::new(false));
    {
        let w = wake.clone();
        let woken2 = woken.clone();
        tokio::spawn(async move {
            w.notified().await;
            woken2.store(true, Ordering::SeqCst);
        });
    }
    tokio::task::yield_now().await;

    let jh = {
        let in_flight = in_flight.clone();
        let wake = wake.clone();
        let key = key.clone();
        tokio::spawn(async move {
            let _guard = InFlightGuard {
                key,
                in_flight,
                wake,
            };
            panic!("boom: 発火ターン内 panic");
        })
    };
    let res = jh.await;
    assert!(res.is_err(), "spawn した発火ターンは panic するはず");

    assert!(
        !in_flight.lock().unwrap().contains(&key),
        "panic 後も in_flight に残る（Drop ガードが効いていない）"
    );
    assert!(
        attempts.lock().unwrap().contains_key(&key),
        "panic 時に attempt が消える（backoff が効かず即再発火スピンになる）"
    );
    tokio::task::yield_now().await;
    assert!(
        woken.load(Ordering::SeqCst),
        "panic 後の完了 wake が鳴っていない（rebuild が促されない）"
    );
}

// ---- #438 回帰: 固定グリッドをやめ設定 interval どおりに発火する ----

// #654: heartbeat rebuild は router で発火先を解決する。NostrFire は nostr feature 時のみ登録
// される（#651）。off では発火集合が空になり entries.len()==1 が成立しないので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[test]
fn next_fire_honors_exact_interval_without_grid_rounding() {
    let anchor = DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let sid = format!("nostr-{AGENT_UUID}");
    let conn = conn_with(&[row(
        AGENT_UUID,
        &sid,
        true,
        Some(1200),
        Some(anchor.to_rfc3339()),
        None,
    )]);
    let entries = rebuild_entries(&test_router(), &conn, true, 1800, 300, &HashMap::new());
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].next_fire_at,
        Some(anchor + Duration::seconds(1200)),
        "次回発火は anchor + 設定 interval(1200s)。1800 グリッドへ丸めない（#438）"
    );
}

#[test]
fn sleep_targets_exact_remaining_not_fixed_grid() {
    let now = Utc::now();
    let e = entry_at("nostr-x", Some(now + Duration::seconds(250)));
    assert_eq!(
        next_sleep_secs(&[e], &HashSet::new(), now),
        250,
        "残り 250 秒ちょうど眠る（固定グリッドへ戻さない・#438）"
    );
}

// ---- #455 schedule: rebuild へ載る・G 非依存・cron/@every・missed-run・enabled ----

fn sched(
    agent: &str,
    session: &str,
    cron: &str,
    enabled: bool,
    anchor: Option<&str>,
    last: Option<&str>,
) -> AgentScheduleRow {
    AgentScheduleRow {
        id: None,
        agent_id: agent.to_string(),
        session_id: session.to_string(),
        cron_expr: cron.to_string(),
        timezone: "Asia/Tokyo".to_string(),
        message: "定時のメッセージ".to_string(),
        enabled,
        anchor_at: anchor.map(|s| s.to_string()),
        last_fired_at: last.map(|s| s.to_string()),
    }
}

fn schedule_keys(entries: &[Entry]) -> std::collections::BTreeSet<i64> {
    entries
        .iter()
        .filter_map(|e| match e.key {
            EntryKey::Schedule { schedule_id } => Some(schedule_id),
            _ => None,
        })
        .collect()
}

/// enabled な cron / `@every` の両方が rebuild に載る。disabled は載らない（enabled=false で停止）。
#[test]
fn schedules_enabled_both_kinds_load_disabled_excluded() {
    let conn = opencrab_db::init_memory().unwrap();
    let sid = format!("nostr-{AGENT_UUID}");
    let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
    // cron（enabled）。
    let id_cron = opencrab_db::queries::insert_agent_schedule(
        &conn,
        &sched(AGENT_UUID, &sid, "0 7 * * *", true, Some(&past), None),
    )
    .unwrap();
    // @every（enabled）。
    let id_every = opencrab_db::queries::insert_agent_schedule(
        &conn,
        &sched(AGENT_UUID, &sid, "@every 3h", true, Some(&past), None),
    )
    .unwrap();
    // disabled は列挙されない。
    let _id_off = opencrab_db::queries::insert_agent_schedule(
        &conn,
        &sched(AGENT_UUID, &sid, "@every 1h", false, Some(&past), None),
    )
    .unwrap();

    let entries = rebuild_entries(&test_router(), &conn, true, 1800, 300, &HashMap::new());
    assert_eq!(
        schedule_keys(&entries),
        [id_cron, id_every].into_iter().collect(),
        "cron と @every の enabled 2 件だけが載る（disabled は停止）"
    );
    // 両種とも next_fire_at が算出されている（cron/@every のどちらの経路も通る）。
    // due か未来かは現在時刻に依存するので、ここでは「算出できたこと」だけを固定する
    // （missed-run の due は schedule_missed_run_compresses_to_one で固定）。
    for e in entries
        .iter()
        .filter(|e| matches!(e.key, EntryKey::Schedule { .. }))
    {
        assert!(
            e.next_fire_at.is_some(),
            "cron/@every とも next_fire_at を算出できる"
        );
    }
}

/// schedule は live G の対象外（G=false でも発火対象に残る・統括裁定 §10.1）。
#[test]
fn schedules_are_not_gated_by_g() {
    let conn = opencrab_db::init_memory().unwrap();
    // discord- セッションの schedule（HB なら G=false で消える種別）。
    let sid = format!("discord-{AGENT_UUID}-1001-2002");
    let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
    let id = opencrab_db::queries::insert_agent_schedule(
        &conn,
        &sched(AGENT_UUID, &sid, "@every 30m", true, Some(&past), None),
    )
    .unwrap();

    // G=false でも schedule は残る（HB の discord- は消えるが schedule は G 非依存）。
    let entries = rebuild_entries(&test_router(), &conn, false, 1800, 300, &HashMap::new());
    assert_eq!(
        schedule_keys(&entries),
        [id].into_iter().collect(),
        "G=false でも discord- 宛 schedule は発火対象（G は heartbeat のスイッチ）"
    );
}

/// 長時間ダウン後の cron schedule も 1 エントリ・due（missed-run 1 回圧縮・§8）。
#[test]
fn schedule_missed_run_compresses_to_one() {
    let conn = opencrab_db::init_memory().unwrap();
    let sid = format!("nostr-{AGENT_UUID}");
    let long_ago = (Utc::now() - Duration::days(3)).to_rfc3339();
    opencrab_db::queries::insert_agent_schedule(
        &conn,
        &sched(AGENT_UUID, &sid, "0 7 * * *", true, Some(&long_ago), None),
    )
    .unwrap();

    let before = rebuild_entries(&test_router(), &conn, true, 1800, 300, &HashMap::new());
    let sched_entries: Vec<_> = before
        .iter()
        .filter(|e| matches!(e.key, EntryKey::Schedule { .. }))
        .collect();
    assert_eq!(
        sched_entries.len(),
        1,
        "3 日過ぎても 1 エントリ（多重発火しない）"
    );
    assert!(
        sched_entries[0]
            .next_fire_at
            .map(|t| t <= Utc::now())
            .unwrap_or(true),
        "過ぎたスロットは due（1 回だけ発火）"
    );
}

/// 解釈できない cron 式は fail-closed で skip（外部へ影響しない）。
#[test]
fn schedule_unparseable_expr_is_skipped() {
    let conn = opencrab_db::init_memory().unwrap();
    let sid = format!("nostr-{AGENT_UUID}");
    opencrab_db::queries::insert_agent_schedule(
        &conn,
        &sched(AGENT_UUID, &sid, "totally not a cron", true, None, None),
    )
    .unwrap();
    let entries = rebuild_entries(&test_router(), &conn, true, 1800, 300, &HashMap::new());
    assert!(
        schedule_keys(&entries).is_empty(),
        "解釈不能な式は発火集合に入らない（fail-closed）"
    );
}

/// 発火成功で last_fired を刻むと、次回 rebuild で cron の次スロット（未来）へ後退する
/// （二重実行しない・向きは後ろ・§8 / §4.4）。
#[test]
fn schedule_success_pushes_next_forward() {
    let conn = opencrab_db::init_memory().unwrap();
    let sid = format!("nostr-{AGENT_UUID}");
    let long_ago = (Utc::now() - Duration::days(3)).to_rfc3339();
    let id = opencrab_db::queries::insert_agent_schedule(
        &conn,
        &sched(AGENT_UUID, &sid, "0 7 * * *", true, Some(&long_ago), None),
    )
    .unwrap();

    // 発火成功を模して last_fired=now を刻む。
    opencrab_db::queries::set_agent_schedule_last_fired(&conn, id, &Utc::now().to_rfc3339())
        .unwrap();

    let after = rebuild_entries(&test_router(), &conn, true, 1800, 300, &HashMap::new());
    let e = after
        .iter()
        .find(|e| matches!(e.key, EntryKey::Schedule { .. }))
        .unwrap();
    assert!(
        e.next_fire_at.map(|t| t > Utc::now()).unwrap_or(false),
        "発火成功後は次スロット（未来）へ後退＝直後の rebuild で再発火しない（二重実行しない）"
    );
}
