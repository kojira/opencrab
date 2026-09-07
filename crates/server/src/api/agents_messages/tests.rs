use super::*;
use opencrab_actions::{SettleKind, SpawnedSubtask, SubtaskLifecycle};

fn db_with_session(session_id: &str) -> opencrab_db::Db {
    let conn = opencrab_db::init_memory().unwrap();
    conn.execute(
        "INSERT INTO sessions (id, theme, status, created_at, updated_at) \
         VALUES (?1, 't', 'active', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        [session_id],
    )
    .unwrap();
    opencrab_db::Db::from_connection(conn)
}

fn status_of(db: &opencrab_db::Db, session_id: &str) -> String {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT status FROM sessions WHERE id = ?1",
        [session_id],
        |r| r.get(0),
    )
    .unwrap()
}

fn settled(session_id: &str, kind: SettleKind) -> SubtaskSettled {
    SubtaskSettled {
        session_id: session_id.to_string(),
        agent_id: "agent-a".to_string(),
        subtask_id: "st-1".to_string(),
        exit_reason: "cancelled".to_string(),
        kind,
        reply_target: None,
        caller: opencrab_actions::CallerIdentity::Agent,
    }
}

/// [P1 回帰] 最後の走行中 subtask が **cancel** されたときも session を
/// `completed` にする（cancel は `settle_completed` を通らないため、これが
/// 無いと `sessions.status` が永久に `active` のまま残る）。
#[tokio::test]
async fn cancel_reconciles_session_status() {
    let session_id = "agent-msg-agent-a-u1";
    let db = db_with_session(session_id);
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let mut state = crate::test_app_state();
    state.db = db.clone();
    let sink = RestCompletionSink {
        db: db.clone(),
        registry: registry.clone(),
        state,
        agent_name: "TestAgent".to_string(),
    };

    assert_eq!(status_of(&db, session_id), "active");
    // cancel_subtask は通知より前に registry から除去する（= もう走行中はない）。
    sink.on_subtask_cancelled(settled(session_id, SettleKind::Cancelled));
    assert_eq!(
        status_of(&db, session_id),
        "completed",
        "最後の subtask を停止したのにセッションが active のまま残る"
    );
}

/// 進捗通知（Progress）でセッションを完了扱いにしてはならない。
///
/// 進捗はまだ run が回っている最中に飛ぶ。ここで completed にすると、応答が
/// 返る前に `sessions.status` を見たクライアントが「完了した」と誤認する。
/// #175 S1 で進捗報告ツールが全経路に露出し、REST の受け口にも Progress が
/// 届くようになったので、web / Nostr と同じガードが要る。
#[tokio::test]
async fn progress_does_not_complete_the_session() {
    let session_id = "agent-msg-agent-a-u1";
    let db = db_with_session(session_id);
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let mut state = crate::test_app_state();
    state.db = db.clone();
    let sink = RestCompletionSink {
        db: db.clone(),
        registry: registry.clone(),
        state,
        agent_name: "TestAgent".to_string(),
    };

    assert_eq!(status_of(&db, session_id), "active");
    opencrab_actions::dispatch_settled(&sink, settled(session_id, SettleKind::Progress));
    assert_eq!(
        status_of(&db, session_id),
        "active",
        "進捗通知でセッションが完了扱いにされている（run はまだ回っている）"
    );

    // 決着（Completed）は**継続ターン**を起こす（#638）。status の整合は継続ターンが
    // 終わってから（走行中がゼロのとき）行われるので、**同期には完了しない**——これが
    // #638 での挙動変更点。ここでは LLM プロバイダが無いので継続は即座に失敗し、その後
    // `complete_session_if_idle` が走る。spawn された継続を待つため短く poll する。
    opencrab_actions::dispatch_settled(&sink, settled(session_id, SettleKind::Completed));
    let mut settled_status = String::new();
    for _ in 0..40 {
        settled_status = status_of(&db, session_id);
        if settled_status == "completed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        settled_status, "completed",
        "継続ターンの後にセッションが完了へ整合されていない"
    );
}

/// 他に走行中 subtask が残っているあいだは停止でも完了にしない。
#[tokio::test]
async fn cancel_keeps_session_active_while_others_run() {
    let session_id = "agent-msg-agent-a-u1";
    let db = db_with_session(session_id);
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let handle = tokio::spawn(std::future::pending::<()>());
    registry.insert(
        "st-other".to_string(),
        SpawnedSubtask {
            abort_handle: handle.abort_handle(),
            session_id: "subtask-st-other".to_string(),
            parent_session_id: session_id.to_string(),
            agent_id: "agent-a".to_string(),
            label: "other".to_string(),
            tool_name: "spawn_subtask".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: None,
            caller: opencrab_actions::CallerIdentity::Agent,
            lifecycle: SubtaskLifecycle::new(),
            steerable: false,
        },
    );
    let mut state = crate::test_app_state();
    state.db = db.clone();
    let sink = RestCompletionSink {
        db: db.clone(),
        registry: registry.clone(),
        state,
        agent_name: "TestAgent".to_string(),
    };

    sink.on_subtask_cancelled(settled(session_id, SettleKind::Cancelled));
    assert_eq!(status_of(&db, session_id), "active");
    handle.abort();
}

/// 非 REST セッション（web-* / heartbeat-*）は対象外（誤って触らない）。
#[tokio::test]
async fn cancel_ignores_non_rest_sessions() {
    let session_id = "web-agent-a-conv1";
    let db = db_with_session(session_id);
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let mut state = crate::test_app_state();
    state.db = db.clone();
    let sink = RestCompletionSink {
        db: db.clone(),
        registry,
        state,
        agent_name: "TestAgent".to_string(),
    };
    sink.on_subtask_cancelled(settled(session_id, SettleKind::Cancelled));
    assert_eq!(status_of(&db, session_id), "active");
}
