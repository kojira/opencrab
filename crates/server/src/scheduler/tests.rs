use super::*;

mod alias_resolution;

const AGENT_UUID: &str = "11111111-1111-4111-8111-111111111111";

/// テスト用の登録簿: **本番と同じ源**（`register_production_descriptors`）で descriptor を
/// 積む（#628）。本番へ transport を足せば scheduler テストの登録簿も自動で追随する
/// （各所での register の散らしを避ける・ブロッカー対応）。rebuild_entries は登録簿へ
/// 問い合わせて発火先を解決するので、ここは scheduler がその登録簿を正しく引くことを見る。
fn test_router() -> opencrab_actions::TimedFireRouter {
    opencrab_actions::TimedFireRouter::new()
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
use std::collections::HashMap;

fn schedule_key(id: i64) -> EntryKey {
    EntryKey::Schedule { schedule_id: id }
}

fn seed_generic_alias_binding(
    conn: &mut rusqlite::Connection,
    agent_id: &str,
    session_id: &str,
) -> (String, String) {
    opencrab_db::queries::upsert_agent(
        conn,
        &opencrab_db::queries::AgentRow {
            agent_id: agent_id.into(),
            name: "agent".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "persona".into(),
            personality: None,
            instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    let subject_id: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = ?1",
            [agent_id],
            |row| row.get(0),
        )
        .unwrap();
    let instance_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string();
    let binding_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_string();
    conn.execute(
        "INSERT INTO gate_instances
         (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
         VALUES (?1, 'generic', ?2, 1, 1, 'e30=', ?3, 1, 1)",
        rusqlite::params![instance_id, subject_id, "0".repeat(64)],
    )
    .unwrap();
    let tx = conn.transaction().unwrap();
    opencrab_db::queries::insert_session_in_tx(&tx, session_id, "alias", "2026-01-01T00:00:00Z")
        .unwrap();
    opencrab_db::queries::insert_agent_session_in_tx(&tx, agent_id, session_id).unwrap();
    opencrab_db::queries::create_gate_binding_in_tx(
        &tx,
        &binding_id,
        &instance_id,
        session_id,
        "alias",
        1,
    )
    .unwrap();
    tx.commit().unwrap();
    (instance_id, binding_id)
}

// ---- ビジーループ回避（A1・#6） ----

fn entry_at(id: i64, next: Option<DateTime<Utc>>) -> Entry {
    Entry {
        key: schedule_key(id),
        agent_id: AGENT_UUID.to_string(),
        session_id: "session".to_string(),
        next_fire_at: next,
        target: FireTarget {
            binding_id: "11111111-1111-4111-8111-111111111111".into(),
            session_id: "session".into(),
        },
        message: "m".into(),
    }
}

/// in-flight エントリは sleep 候補から除外され、0 秒スピンしない（A1）。
#[test]
fn sleep_excludes_in_flight_and_never_spins_to_zero() {
    let now = Utc::now();
    let mut in_flight = HashSet::new();
    in_flight.insert(schedule_key(1));

    let running_future = entry_at(1, Some(now + Duration::seconds(10)));
    let idle_future = entry_at(2, Some(now + Duration::seconds(120)));
    assert_eq!(
        next_sleep_secs(&[running_future, idle_future], &in_flight, now),
        120,
        "in-flight の未来エントリを除外し、非 in-flight の 120s まで眠る"
    );

    let running_stuck = entry_at(1, Some(now - Duration::seconds(30)));
    assert_eq!(
        next_sleep_secs(&[running_stuck], &in_flight, now),
        MAX_SLEEP_SECS,
        "走行中 due だけなら 0 秒スピンせず MAX_SLEEP（完了 wake で拾う）"
    );

    let far = entry_at(3, Some(now + Duration::hours(2)));
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

    let mut running = entry_at(1, Some(now - Duration::seconds(5)));
    running.session_id = session.clone();
    // 同一セッションの別スケジュール（id=2）は in_flight に居ないので sleep 候補に残る。
    let mut sibling = entry_at(2, Some(now + Duration::seconds(90)));
    sibling.session_id = session;
    assert_eq!(
        next_sleep_secs(&[running, sibling], &in_flight, now),
        90,
        "id=1 が走行中でも id=2（同一セッション）は別キーなので候補に残る"
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
    let key = schedule_key(1);

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

#[test]
fn sleep_targets_exact_remaining_not_fixed_grid() {
    let now = Utc::now();
    let e = entry_at(4, Some(now + Duration::seconds(250)));
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
        .map(|e| match e.key {
            EntryKey::Schedule { schedule_id } => schedule_id,
        })
        .collect()
}

/// enabled な cron / `@every` の両方が rebuild に載る。disabled は載らない（enabled=false で停止）。
#[test]
fn schedules_enabled_both_kinds_load_disabled_excluded() {
    let mut conn = opencrab_db::init_memory().unwrap();
    let sid = format!("nostr-{AGENT_UUID}");
    seed_generic_alias_binding(&mut conn, AGENT_UUID, &sid);
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

    let entries = rebuild_entries(&test_router(), &conn, &HashMap::new());
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

/// 長時間ダウン後の cron schedule も 1 エントリ・due（missed-run 1 回圧縮・§8）。
#[test]
fn schedule_missed_run_compresses_to_one() {
    let mut conn = opencrab_db::init_memory().unwrap();
    let sid = format!("nostr-{AGENT_UUID}");
    seed_generic_alias_binding(&mut conn, AGENT_UUID, &sid);
    let long_ago = (Utc::now() - Duration::days(3)).to_rfc3339();
    opencrab_db::queries::insert_agent_schedule(
        &conn,
        &sched(AGENT_UUID, &sid, "0 7 * * *", true, Some(&long_ago), None),
    )
    .unwrap();

    let before = rebuild_entries(&test_router(), &conn, &HashMap::new());
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
    let entries = rebuild_entries(&test_router(), &conn, &HashMap::new());
    assert!(
        schedule_keys(&entries).is_empty(),
        "解釈不能な式は発火集合に入らない（fail-closed）"
    );
}

/// 発火成功で last_fired を刻むと、次回 rebuild で cron の次スロット（未来）へ後退する
/// （二重実行しない・向きは後ろ・§8 / §4.4）。
#[test]
fn schedule_success_pushes_next_forward() {
    let mut conn = opencrab_db::init_memory().unwrap();
    let sid = format!("nostr-{AGENT_UUID}");
    seed_generic_alias_binding(&mut conn, AGENT_UUID, &sid);
    let long_ago = (Utc::now() - Duration::days(3)).to_rfc3339();
    let id = opencrab_db::queries::insert_agent_schedule(
        &conn,
        &sched(AGENT_UUID, &sid, "0 7 * * *", true, Some(&long_ago), None),
    )
    .unwrap();

    // 発火成功を模して last_fired=now を刻む。
    opencrab_db::queries::set_agent_schedule_last_fired(&conn, id, &Utc::now().to_rfc3339())
        .unwrap();

    let after = rebuild_entries(&test_router(), &conn, &HashMap::new());
    let e = after
        .iter()
        .find(|e| matches!(e.key, EntryKey::Schedule { .. }))
        .unwrap();
    assert!(
        e.next_fire_at.map(|t| t > Utc::now()).unwrap_or(false),
        "発火成功後は次スロット（未来）へ後退＝直後の rebuild で再発火しない（二重実行しない）"
    );
}
