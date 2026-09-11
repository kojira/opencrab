use super::*;
use opencrab_core::EngineResult;
use std::sync::atomic::{AtomicUsize, Ordering};

// --- セットアップ ---

/// `n` 件のユニットを宣言し、生ログも合わせて入れる（record_memory_unit は範囲の実在を要求する）。
/// 返り値はユニットの short_id 群。
fn seed_units(state: &AppState, agent_id: &str, n: usize) -> Vec<String> {
    let conn = state.db.lock().unwrap();
    let now = Utc::now().to_rfc3339();
    let mut short_ids = Vec::new();
    for i in 0..n {
        // 1 ユニットにつき生ログ 1 件（id 範囲 [id, id]）。
        let log_id = opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.to_string(),
                session_id: format!("s-{i}"),
                log_type: "message".to_string(),
                content: format!("出来事 {i}"),
                speaker_id: None,
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
        let node = opencrab_db::queries::record_memory_unit(
            &conn,
            agent_id,
            &format!("ユニット {i}"),
            &format!("出来事 {i} の要約"),
            log_id,
            log_id,
            Some(&now),
            Some(&now),
            &now,
        )
        .unwrap();
        short_ids.push(node.short_id.unwrap());
    }
    short_ids
}

fn get_marker(state: &AppState, agent_id: &str) -> Option<String> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::get_memory_condense_cursor(&conn, agent_id).unwrap()
}

fn set_marker(state: &AppState, agent_id: &str, cursor: &str) {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::set_memory_condense_cursor(&conn, agent_id, cursor).unwrap();
}

fn cfg(enabled: bool, min_new_units: i64, min_interval_minutes: i64) -> MemoryCondenseConfig {
    MemoryCondenseConfig {
        enabled,
        min_new_units,
        min_interval_minutes,
        timeout_secs: 600,
    }
}

fn hours_ago(hours: i64) -> String {
    (Utc::now() - Duration::hours(hours)).to_rfc3339()
}

// --- FakeRunner（本番の run_agent_response を差し替える。何も構築しない）---

#[derive(Clone, Copy)]
enum FakeOutcome {
    Completed,
    StoppedByLimit,
    Error,
}

struct CapturedReq {
    gateway: String,
    caller_is_owner: bool,
    tool_allowlist: Option<Vec<String>>,
    has_gateway_actions: bool,
    persist_turn_logs: bool,
}

struct FakeRunner {
    outcome: FakeOutcome,
    calls: AtomicUsize,
    captured: std::sync::Mutex<Option<CapturedReq>>,
}

impl FakeRunner {
    fn new(outcome: FakeOutcome) -> Self {
        Self {
            outcome,
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl OrganizeTurnRunner for FakeRunner {
    async fn run_turn(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.captured.lock().unwrap() = Some(CapturedReq {
            gateway: req.gateway.clone(),
            caller_is_owner: matches!(req.caller, CallerIdentity::Owner),
            tool_allowlist: req.tool_allowlist.clone(),
            has_gateway_actions: req.gateway_actions.is_some(),
            persist_turn_logs: req.persist_turn_logs,
        });
        match self.outcome {
            FakeOutcome::Completed => Ok(engine_result(false)),
            FakeOutcome::StoppedByLimit => Ok(engine_result(true)),
            FakeOutcome::Error => Err(anyhow::anyhow!("simulated run failure")),
        }
    }
}

fn engine_result(stopped_by_limit: bool) -> EngineResult {
    EngineResult {
        response: String::new(),
        iterations: 1,
        tool_calls_made: 0,
        stopped_by_limit,
        explicit_termination: None,
        last_posting_utterance_id: None,
        last_generation_had_continuation_speech: false,
        xml_fallback_parses: 0,
    }
}

fn latest_sleep_audit(state: &AppState, agent_id: &str) -> Option<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let rows = opencrab_db::queries::list_agent_logs(&conn, Some(agent_id), None, 10).ok()?;
    rows.into_iter()
        .filter(|r| r.context == "sleep")
        .find_map(|r| {
            serde_json::from_str::<serde_json::Value>(&r.message)
                .ok()
                .filter(|v| v["kind"] == "memory_condense")
        })
}

// --- マーカー parse/format ---

#[test]
fn marker_roundtrips_and_tolerates_missing_parts() {
    let m = format_condense_marker("2026-08-08T00:00:00Z", 346, 2);
    assert_eq!(m, "2026-08-08T00:00:00Z|346|2");
    assert_eq!(
        parse_condense_marker(Some(&m)),
        (Some("2026-08-08T00:00:00Z".to_string()), 346, 2)
    );
    assert_eq!(parse_condense_marker(None), (None, 0, 0));
    assert_eq!(
        parse_condense_marker(Some("2026-08-08T00:00:00Z")),
        (Some("2026-08-08T00:00:00Z".to_string()), 0, 0)
    );
    assert_eq!(
        parse_condense_marker(Some("2026-08-08T00:00:00Z|xxx")),
        (Some("2026-08-08T00:00:00Z".to_string()), 0, 0)
    );
    // 後方互換: partial_streak を持たない 2 分割マーカーは streak 0 として読む。
    assert_eq!(
        parse_condense_marker(Some("2026-08-08T00:00:00Z|346")),
        (Some("2026-08-08T00:00:00Z".to_string()), 346, 0)
    );
    // 壊れた streak（負値・非数値）は 0 に丸める（負値は待ちを無効化するだけで暴走しない）。
    assert_eq!(
        parse_condense_marker(Some("2026-08-08T00:00:00Z|346|-5")),
        (Some("2026-08-08T00:00:00Z".to_string()), 346, 0)
    );
    assert_eq!(
        parse_condense_marker(Some("2026-08-08T00:00:00Z|346|zzz")),
        (Some("2026-08-08T00:00:00Z".to_string()), 346, 0)
    );
}

// --- partial バックオフ ---

#[test]
fn partial_backoff_doubles_and_is_capped_by_min_interval() {
    // clean 直後（streak 0）は待たない。
    assert_eq!(partial_backoff_minutes(0, 1440), 0);
    // 1 回目は base、以後 2 倍ずつ。
    assert_eq!(partial_backoff_minutes(1, 1440), 10);
    assert_eq!(partial_backoff_minutes(2, 1440), 20);
    assert_eq!(partial_backoff_minutes(3, 1440), 40);
    // 端数待ち（min_interval）を超えない。
    assert_eq!(partial_backoff_minutes(10, 1440), 1440);
    assert_eq!(partial_backoff_minutes(3, 15), 15);
    // 巨大な streak でも溢れずに上限へ張り付く。
    assert_eq!(partial_backoff_minutes(i64::MAX, 1440), 1440);
}

// --- ゲート ---

#[tokio::test]
async fn disabled_is_zero_call_and_writes_nothing() {
    let state = crate::test_app_state();
    seed_units(&state, "a1", 5);
    let fake = FakeRunner::new(FakeOutcome::Completed);
    let before = get_marker(&state, "a1");
    let ran = run_condense(
        &state.db,
        &cfg(false, 3, 1440),
        &state.index_build_inflight,
        "a1",
        &fake,
    )
    .await
    .unwrap();
    assert!(!ran, "無効化時（enabled=false）は起動しない");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0, "口を 1 度も呼ばない");
    assert_eq!(
        get_marker(&state, "a1"),
        before,
        "無効化時はマーカーを書き換えない"
    );
}

#[tokio::test]
async fn no_new_units_after_cursor_is_zero_call() {
    let state = crate::test_app_state();
    seed_units(&state, "a1", 5); // start_log_id 1..5
                                 // カーソルを末尾（5）に置く = 全部凝縮済み。残 0 → 発火しない。
    set_marker(&state, "a1", &format_condense_marker(&hours_ago(240), 5, 0));
    let fake = FakeRunner::new(FakeOutcome::Completed);
    let ran = run_condense(
        &state.db,
        &cfg(true, 3, 1440),
        &state.index_build_inflight,
        "a1",
        &fake,
    )
    .await
    .unwrap();
    assert!(!ran, "カーソルより新しいユニットが無ければ起動しない");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn full_window_fires_even_when_throttled() {
    let state = crate::test_app_state();
    seed_units(&state, "a1", 10); // 残 10 >= 窓幅 3
                                  // 直前（1 分前）に走ったばかりでも、積み残し（残 >= 窓幅）は throttle を待たず消化する。
    set_marker(&state, "a1", &format_condense_marker(&hours_ago(0), 0, 0));
    let fake = FakeRunner::new(FakeOutcome::Completed);
    let ran = run_condense(
        &state.db,
        &cfg(true, 3, 1440),
        &state.index_build_inflight,
        "a1",
        &fake,
    )
    .await
    .unwrap();
    assert!(ran, "積み残しは throttle を待たず 1 窓消化する");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    let audit = latest_sleep_audit(&state, "a1").expect("監査ログ");
    assert_eq!(audit["window_units"], 3, "1 回で窓幅ぶんだけ消化");
    assert_eq!(audit["remaining_before"], 10);
    assert_eq!(audit["remaining_after"], 7);
}

#[tokio::test]
async fn tail_below_window_waits_for_interval() {
    let state = crate::test_app_state();
    seed_units(&state, "a1", 2); // 残 2 < 窓幅 3 = 末尾の端数
                                 // 端数は min_interval を待つ。1 分前に走ったばかり（1440 分未達）→ 待つ。
    set_marker(&state, "a1", &format_condense_marker(&hours_ago(0), 0, 0));
    let fake = FakeRunner::new(FakeOutcome::Completed);
    let ran = run_condense(
        &state.db,
        &cfg(true, 3, 1440),
        &state.index_build_inflight,
        "a1",
        &fake,
    )
    .await
    .unwrap();
    assert!(!ran, "端数は min_interval 未達なら待つ");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn tail_flushes_after_interval_or_on_first_run() {
    // 初回（マーカー無し）は throttle を待たず端数を流す。
    let state = crate::test_app_state();
    seed_units(&state, "a1", 2); // 残 2 < 窓幅 3
    let fake = FakeRunner::new(FakeOutcome::Completed);
    let ran = run_condense(
        &state.db,
        &cfg(true, 3, 1440),
        &state.index_build_inflight,
        "a1",
        &fake,
    )
    .await
    .unwrap();
    assert!(ran, "初回は端数でも流す");
    let audit = latest_sleep_audit(&state, "a1").expect("監査ログ");
    assert_eq!(audit["window_units"], 2, "端数 2 件を消化");
    let (_, pos, _) = parse_condense_marker(get_marker(&state, "a1").as_deref());
    assert_eq!(pos, 2, "位置は端数の末尾 start_log_id へ");

    // 2 回目以降でも、min_interval を過ぎていれば端数を流す（`tail_below_window_waits_for_interval`
    // の裏。同条件で last_run_at だけを throttle 明けにしたら流れることを見る）。
    let state2 = crate::test_app_state();
    seed_units(&state2, "a1", 2); // 残 2 < 窓幅 3
    set_marker(
        &state2,
        "a1",
        &format_condense_marker(&hours_ago(240), 0, 0),
    );
    let fake2 = FakeRunner::new(FakeOutcome::Completed);
    let ran2 = run_condense(
        &state2.db,
        &cfg(true, 3, 1440), // 1440 分 = 24h < 240h 経過
        &state2.index_build_inflight,
        "a1",
        &fake2,
    )
    .await
    .unwrap();
    assert!(ran2, "端数でも min_interval を過ぎていれば流す");
    assert_eq!(fake2.calls.load(Ordering::SeqCst), 1);
    let audit2 = latest_sleep_audit(&state2, "a1").expect("監査ログ");
    assert_eq!(audit2["window_units"], 2, "端数 2 件を消化");
    let (_, pos2, _) = parse_condense_marker(get_marker(&state2, "a1").as_deref());
    assert_eq!(pos2, 2, "位置は端数の末尾 start_log_id へ");
}

// --- 窓の切り出し（decide_condense）---

#[test]
fn window_slices_oldest_first_and_next_window_continues() {
    let state = crate::test_app_state();
    let ids = seed_units(&state, "a1", 5); // u1..u5, start_log_id 1..5
                                           // 位置 0（初回）: 最古 3 件が窓。
    match decide_condense(&state.db, &cfg(true, 3, 1440), "a1").unwrap() {
        CondenseDecision::Run(p) => {
            let w: Vec<&str> = p
                .window_units
                .iter()
                .map(|u| u.short_id.as_deref().unwrap())
                .collect();
            assert_eq!(
                w,
                vec![ids[0].as_str(), ids[1].as_str(), ids[2].as_str()],
                "古い順の最初の 3 件"
            );
            assert_eq!(p.position_before, 0);
            assert_eq!(p.position_after, 3, "窓末尾 start_log_id");
            assert_eq!(p.remaining_before, 5);
        }
        other => panic!("expected Run, got {other:?}"),
    }
    // 位置 3（1 窓消化済み・初回でない）: 残 2 件が窓（端数だが throttle 明け）。
    set_marker(&state, "a1", &format_condense_marker(&hours_ago(240), 3, 0));
    match decide_condense(&state.db, &cfg(true, 3, 1440), "a1").unwrap() {
        CondenseDecision::Run(p) => {
            let w: Vec<&str> = p
                .window_units
                .iter()
                .map(|u| u.short_id.as_deref().unwrap())
                .collect();
            assert_eq!(w, vec![ids[3].as_str(), ids[4].as_str()], "続きの 2 件");
            assert_eq!(p.position_after, 5);
        }
        other => panic!("expected Run, got {other:?}"),
    }
}

// --- clean / partial のマーカー ---

#[tokio::test]
async fn clean_run_advances_position_to_window_tail() {
    let state = crate::test_app_state();
    seed_units(&state, "a1", 5); // start_log_id 1..5, 窓幅 3
    let fake = FakeRunner::new(FakeOutcome::Completed);
    let ran = run_condense(
        &state.db,
        &cfg(true, 3, 1440),
        &state.index_build_inflight,
        "a1",
        &fake,
    )
    .await
    .unwrap();
    assert!(ran);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    let (_, pos, _) = parse_condense_marker(get_marker(&state, "a1").as_deref());
    assert_eq!(pos, 3, "clean は位置を今回の窓の末尾 start_log_id へ進める");
    let audit = latest_sleep_audit(&state, "a1").expect("監査ログ");
    assert_eq!(audit["outcome"], "completed");
    assert_eq!(audit["position_advanced"], true);
    assert_eq!(audit["position_after"], 3);
    assert_eq!(audit["window_units"], 3);
}

#[tokio::test]
async fn partial_holds_position_but_advances_throttle() {
    let state = crate::test_app_state();
    seed_units(&state, "a1", 5);
    // 位置 2 まで消化済み・240h 前（throttle 明け）。残 3 >= 窓幅で発火。
    set_marker(&state, "a1", &format_condense_marker(&hours_ago(240), 2, 0));
    let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);
    let ran = run_condense(
        &state.db,
        &cfg(true, 3, 1440),
        &state.index_build_inflight,
        "a1",
        &fake,
    )
    .await
    .unwrap();
    assert!(ran, "起動はした（partial）");
    let (ts, pos, _) = parse_condense_marker(get_marker(&state, "a1").as_deref());
    assert_eq!(pos, 2, "partial は位置を据え置く（次 tick で同じ窓を読む）");
    let advanced = ts
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .map(|dt| Utc::now().signed_duration_since(dt) < Duration::hours(1))
        .unwrap_or(false);
    assert!(
        advanced,
        "partial でも throttle は now へ進む（端数待ちの起点をリセット）"
    );
    let audit = latest_sleep_audit(&state, "a1").expect("監査ログ");
    assert_eq!(audit["outcome"], "stopped_by_limit");
    assert_eq!(audit["position_advanced"], false);
    assert_eq!(audit["throttle_advanced"], true);
    // 位置を進めていない＝1 件も消化していないので、残数も減らない（監査の整合）。
    assert_eq!(audit["remaining_before"], 3);
    assert_eq!(audit["remaining_after"], 3, "partial は残数を減らさない");
}

/// 積み残し（残 >= 窓幅）は throttle を通らないので、partial が続くと毎 tick フルの LLM ランが
/// 走り続ける。連続 partial のバックオフでそれを間引く（本体の無限再走ループ回避）。
#[tokio::test]
async fn repeated_partial_backs_off_before_rerunning_same_window() {
    let state = crate::test_app_state();
    seed_units(&state, "a1", 10); // 残 10 >= 窓幅 3（積み残し = throttle を通らない経路）
    set_marker(&state, "a1", &format_condense_marker(&hours_ago(240), 0, 0));
    let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);

    // 1 回目: streak 0 なのでバックオフ無し → 走る。partial なので位置据え置き・streak 1。
    let ran1 = run_condense(
        &state.db,
        &cfg(true, 3, 1440),
        &state.index_build_inflight,
        "a1",
        &fake,
    )
    .await
    .unwrap();
    assert!(ran1, "1 回目は走る（バックオフ無し）");
    let (_, pos1, streak1) = parse_condense_marker(get_marker(&state, "a1").as_deref());
    assert_eq!(pos1, 0, "partial は位置を据え置く");
    assert_eq!(streak1, 1, "partial で連続回数が 1 になる");

    // 2 回目（直後）: 積み残しのままだが streak 1 のバックオフ（10 分）が明けていない → 待つ。
    let ran2 = run_condense(
        &state.db,
        &cfg(true, 3, 1440),
        &state.index_build_inflight,
        "a1",
        &fake,
    )
    .await
    .unwrap();
    assert!(!ran2, "連続 partial のあとは積み残しでもバックオフで待つ");
    assert_eq!(
        fake.calls.load(Ordering::SeqCst),
        1,
        "2 回目は LLM の口を呼ばない（毎 tick フルランの暴走を止める）"
    );
    // 待っている間もカーソルは動かさない（窓を捨てて材料を失わない）。
    let (_, pos2, streak2) = parse_condense_marker(get_marker(&state, "a1").as_deref());
    assert_eq!(pos2, 0, "バックオフ中もカーソルは強制前進しない");
    assert_eq!(streak2, 1, "スキップでは streak も増やさない");
}

/// バックオフは clean 1 回で完全解除される（積み残しの消化速度を落としたままにしない）。
#[tokio::test]
async fn clean_run_clears_partial_backoff() {
    let state = crate::test_app_state();
    seed_units(&state, "a1", 10); // 残 10 >= 窓幅 3
                                  // 連続 partial 3 回ぶんの状態から始める（バックオフ 40 分）。240h 前なのでもう明けている。
    set_marker(&state, "a1", &format_condense_marker(&hours_ago(240), 0, 3));
    let fake = FakeRunner::new(FakeOutcome::Completed);

    let ran1 = run_condense(
        &state.db,
        &cfg(true, 3, 1440),
        &state.index_build_inflight,
        "a1",
        &fake,
    )
    .await
    .unwrap();
    assert!(ran1, "バックオフが明けていれば走る");
    let (_, pos1, streak1) = parse_condense_marker(get_marker(&state, "a1").as_deref());
    assert_eq!(pos1, 3, "clean は位置を窓の末尾へ進める");
    assert_eq!(streak1, 0, "clean で連続 partial 回数が 0 に戻る");

    // 直後にもう一度: streak 0 なのでバックオフ無し（streak 3 のままなら 40 分待たされていた）。
    let ran2 = run_condense(
        &state.db,
        &cfg(true, 3, 1440),
        &state.index_build_inflight,
        "a1",
        &fake,
    )
    .await
    .unwrap();
    assert!(ran2, "clean 後は直後の tick でも積み残しを続けて消化する");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    let (_, pos2, _) = parse_condense_marker(get_marker(&state, "a1").as_deref());
    assert_eq!(pos2, 6, "次の窓も消化して位置が進む");
}

#[tokio::test]
async fn error_outcome_holds_position() {
    let state = crate::test_app_state();
    seed_units(&state, "a1", 5);
    let fake = FakeRunner::new(FakeOutcome::Error);
    let ran = run_condense(
        &state.db,
        &cfg(true, 3, 1440),
        &state.index_build_inflight,
        "a1",
        &fake,
    )
    .await
    .unwrap();
    assert!(ran);
    let (_, pos, _) = parse_condense_marker(get_marker(&state, "a1").as_deref());
    assert_eq!(
        pos, 0,
        "error（partial）は位置を据え置く（初回 position=0）"
    );
    let audit = latest_sleep_audit(&state, "a1").expect("監査ログ");
    assert_eq!(audit["outcome"], "error");
    assert_eq!(audit["position_advanced"], false);
}

// --- RunRequest の本番配線 ---

#[tokio::test]
async fn run_request_wiring_is_sleep_owner_allowlisted_and_no_send() {
    let state = crate::test_app_state();
    seed_units(&state, "a1", 5);
    let fake = FakeRunner::new(FakeOutcome::Completed);
    run_condense(
        &state.db,
        &cfg(true, 3, 1440),
        &state.index_build_inflight,
        "a1",
        &fake,
    )
    .await
    .unwrap();
    let cap = fake.captured.lock().unwrap();
    let cap = cap.as_ref().expect("captured req");
    assert_eq!(cap.gateway, "sleep");
    assert!(cap.caller_is_owner, "caller=Owner");
    assert!(
        !cap.has_gateway_actions,
        "送信経路を渡さない（会話へ出さない）"
    );
    assert!(!cap.persist_turn_logs, "生ログに書かない（#393）");
    let allow = cap.tool_allowlist.as_ref().expect("allowlist");
    let expected: Vec<String> = CONDENSE_ALLOWED_TOOLS
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(allow, &expected);
}

#[test]
fn allowlist_has_core_tools_and_excludes_outward_and_unit_writes() {
    for t in [
        "record_memory_core",
        "update_memory_core",
        "retract_memory_core",
        "search_memory_index",
        "search_my_history",
        "declare_done",
    ] {
        assert!(CONDENSE_ALLOWED_TOOLS.contains(&t), "{t} は凝縮ランに要る");
    }
    for forbidden in [
        "execute_shell",
        "nostr_run",
        "spawn_subtask",
        "ws_write",
        "update_instructions",
        // 生ログを刻む道具は凝縮ランには渡さない（宣言ランの領分）。
        "record_memory_unit",
        "retract_memory_unit",
        "plan_next_memory_window",
    ] {
        assert!(
            !CONDENSE_ALLOWED_TOOLS.contains(&forbidden),
            "{forbidden} は凝縮ランに渡してはいけない"
        );
    }
}

// --- プロンプト ---

#[test]
fn prompt_shows_units_open_axes_and_existing_cores() {
    let state = crate::test_app_state();
    let unit_ids = seed_units(&state, "a1", 3);
    // 既存 core を 1 件仕込む（更新候補として出るか）。
    {
        let conn = state.db.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        opencrab_db::queries::record_memory_core(
            &conn,
            "a1",
            "繰り返していること",
            "同じ判断を何度も繰り返している",
            &[unit_ids[0].clone()],
            &now,
        )
        .unwrap();
    }
    let plan = match decide_condense(&state.db, &cfg(true, 1, 1440), "a1").unwrap() {
        CondenseDecision::Run(p) => p,
        other => panic!("expected Run, got {other:?}"),
    };
    let sp = build_system_prompt(&plan);
    // 今回の窓のユニットが並ぶ（逐次凝縮）。
    assert!(sp.contains("今回の窓"), "窓ベースの提示");
    assert!(sp.contains(&unit_ids[0]), "窓のユニットの short_id が出る");
    // 軸は開いて見せる（自分の軸を足せる）。
    assert!(sp.contains("あなた自身の軸を足す"), "軸は開いた提示");
    assert!(sp.contains("繰り返していること"), "基本の軸が出る");
    // 基本の 7 軸それぞれに目を通し、無い軸は「今回は無い」と明示させる。
    assert!(sp.contains("基本の軸"), "基本の軸セクションがある");
    assert!(sp.contains("今回は無い"), "無い軸を明示する指示がある");
    assert!(
        sp.contains("でっち上げる"),
        "無い軸を薄い凝縮ででっち上げないよう戒めている"
    );
    // 重心は本人の興味・関心（正しさの教訓ではなく心が動いた先）。
    assert!(
        sp.contains("あなた自身の興味・関心"),
        "核の中心を本人の興味・関心に据える"
    );
    assert!(
        sp.contains("心が何に動き"),
        "各軸でも『心が何に動いたか』を探させる"
    );
    // 更新は継ぎ足しではなく蒸留（窓を重ねても core を長くしない）。
    assert!(
        sp.contains("窓を重ねても長くしないでください"),
        "核を磨いた短文に保ち、窓ごとの肥大を禁じている"
    );
    assert!(
        sp.contains("継ぎ足しではなく蒸留"),
        "更新の意味を蒸留として明示している"
    );
    // 短くする道は抽象化ではなく選択（まとめずに捨てさせる）。
    assert!(
        sp.contains("凝縮は抽象化ではなく選択です"),
        "短縮の手段を抽象化ではなく選択だと明示している"
    );
    assert!(
        sp.contains("実在の出来事を 1 つだけ選び"),
        "1 核につき実在の出来事を 1 つ残させる"
    );
    assert!(
        sp.contains("選ばなかった材料は本文から"),
        "選ばなかった材料を本文から捨てさせる（根拠はリンクへ）"
    );
    // 標語・自己引用は「具体」ではない（v3 でここが抽象化の隠れ蓑になった）。
    assert!(
        sp.contains("標語・スローガン・自分の言い回しの引用は「具体」ではありません"),
        "自作の言い回しの鍵括弧引用を具体として認めない"
    );
    assert!(
        sp.contains("誰が実際に何をしたか"),
        "具体の定義を『誰が何をしたか』に固定している"
    );
    // 見本は骨格プレースホルダで示す。実在の人名・私的な出来事を書くと、公開リポに個人情報が
    // 残るうえ、本人が自分のユニットではなく見本から具体を借りてしまう。
    for skeleton in ["〈誰〉が〈何〉をしてくれた", "〈どこ〉で〈何〉を見た"]
    {
        assert!(
            sp.contains(skeleton),
            "具体の見本は骨格プレースホルダで示す: {skeleton}"
        );
    }
    assert!(
        sp.contains("あなたのユニットにある実際の名前と出来事で埋めます"),
        "プレースホルダを本人のユニットで埋めさせる（見本から具体を借りさせない）"
    );
    // 「無理に出さない」を選べる（この窓で）。
    assert!(sp.contains("無理に何か出す必要はありません"));
    // 更新優先で core を育てる（逐次凝縮の要）。
    assert!(sp.contains("育てて"), "既存 core を update で育てる指示");
    assert!(sp.contains("更新を優先"));
    // 根拠リンク。
    assert!(sp.contains("根拠のユニット"));
    // 既存 core が更新候補として出る。
    assert!(sp.contains("同じ判断を何度も繰り返している"));
    assert!(sp.contains("record_memory_core"));
}
