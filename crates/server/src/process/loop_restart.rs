use super::*;

/// ループ再起動 v1（#52）の判定と準備。
///
/// 再実行すべきなら「再構築した会話文字列」を返す（[restart] decision の記録と
/// restart_count のインクリメントは済ませてある）。それ以外は None。
///
/// - 対象: depth 0、`loop_restart_enabled`、run が Ok かつ stopped_by_limit、
///   セッションに active タスクが残っている場合のみ。
/// - 上限は二重: per-task（restart_count、永続）+ per-call（restarts_this_call）。
///   per-call 上限が無いと、再実行中にエージェントがタスクを close/open して
///   差し替えた場合に新タスク（restart_count=0）で再々実行が始まり非有界になる。
/// - 記録順序: decision → 会話再構築 → increment → 再実行。decision を先に書くのは
///   再構築される会話（台帳セクション）に載せて run-2 へ見せるため。increment は
///   再構築の**後**（失敗時に per-task 予算を消費しない）かつ再実行の**前**
///   （再実行中にクラッシュしても、次回の上限判定が効いて無限再起動しない。
///   decision〜increment 間のクラッシュは「予算未消費のまま decision が残る」だけで
///   無害 — 次の再起動判定はフルの run を経てしか到達しない）。
/// - per-task 予算枯渇時の abandoned 遷移は「この呼び出しで実際に再実行した」
///   （= run-2 も上限で停止した）場合のみ。過去ターンで予算を使い切ったタスクを、
///   後日の上限停止で突然殺さない。abandoned は session_logs にも記録する: 台帳
///   セクションは active タスクしか描画しないため、blocker エントリだけでは次ターン
///   以降のエージェント/ユーザーから不可視になる。
pub(super) fn prepare_loop_restart(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
    depth: u32,
    restarts_this_call: i64,
    trigger_message_id: Option<&str>,
    result: &anyhow::Result<opencrab_core::EngineResult>,
) -> Option<String> {
    /// v1 の再実行上限（per-task / per-call 共通）。
    const LOOP_RESTART_MAX: i64 = 1;

    if depth != 0 || !state.loop_restart_enabled {
        return None;
    }
    if !matches!(result, Ok(er) if er.stopped_by_limit) {
        return None;
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(session_id = %session_id, "loop restart skipped (db lock): {e}");
            return None;
        }
    };
    let task = opencrab_db::queries::get_active_task_for_session(&conn, agent_id, session_id)
        .ok()
        .flatten()?;

    if task.restart_count >= LOOP_RESTART_MAX {
        // per-task 予算枯渇。この呼び出しで実際に再実行していた場合のみ abandoned に
        // 落とす（= 再実行後もまた上限で停止した）。そうでなければ何もしない
        // （機能導入前と同じ「上限で止まって終わり」— 過去ターンで予算を使い切った
        // タスクを後日の上限停止で突然殺さない）。
        if restarts_this_call > 0 {
            tracing::warn!(
                session_id = %session_id,
                task_id = task.id,
                restart_count = task.restart_count,
                "loop restart budget exhausted; abandoning task"
            );
            let _ = opencrab_db::queries::insert_task_progress(
                &conn,
                task.id,
                "blocker",
                &format!(
                    "[restart] 自動再実行後も反復上限で停止した（restart 上限 {LOOP_RESTART_MAX} 回に到達）。\
                     タスクを abandoned にする。再開には goal/contract の再交渉か人手の介入が必要。"
                ),
            );
            let _ = opencrab_db::queries::update_task_status(&conn, agent_id, task.id, "abandoned");
            // 台帳セクションは active タスクしか描画しない → session_logs 側にも
            // 残して、次ターンの会話から見えるようにする（詳細は get_task で辿れる）。
            opencrab_db::queries::insert_session_log_best_effort(
                &conn,
                &opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    log_type: "task_event".to_string(),
                    content: format!(
                        "Task #{} was abandoned automatically: the run hit the iteration \
                         limit again right after an automatic restart. Goal: {}. Renegotiate \
                         the goal/contract with the user or ask for help before reopening \
                         (full history: get_task).",
                        task.id,
                        task.goal.chars().take(200).collect::<String>(),
                    ),
                    speaker_id: Some("system".to_string()),
                    turn_number: None,
                    metadata_json: Some(
                        serde_json::json!({
                            "task_id": task.id,
                            "event": "abandoned_by_loop_restart",
                        })
                        .to_string(),
                    ),
                    created_at: None,
                },
            );
        }
        return None;
    }
    if restarts_this_call >= LOOP_RESTART_MAX {
        // per-call 安全弁: 再実行中にタスクが差し替わっていても（新タスクは
        // restart_count=0）、この呼び出し内ではこれ以上再実行しない。
        // 新タスクの per-task 予算は消費しない。
        tracing::warn!(
            session_id = %session_id,
            task_id = task.id,
            "loop restart per-call cap reached; not restarting again in this call"
        );
        return None;
    }

    // decision を先に記録する: 直後に再構築する会話へ台帳セクション経由で載り、
    // run-2 が「これは再実行である」ことと埋めるべき gaps の在処を知る。
    // 停止時の EngineResult.response は定型文（"I've reached the maximum..."）で
    // 情報が無いため記録しない — run-1 の実質的な結論はツール実行時の speech ログに
    // 残っており、再構築した会話に含まれる。
    let _ = opencrab_db::queries::insert_task_progress(
        &conn,
        task.id,
        "decision",
        &format!(
            "[restart] 反復上限で停止したため、クリーンな context で自動再実行する（{} 回目 / 上限 {LOOP_RESTART_MAX}）。\
             直近の [evaluation] エントリの gaps を優先的に埋め、この再実行で完了できなければ blocker を記録すること。",
            task.restart_count + 1
        ),
    );

    // 会話を再構築: run-1 のトレース・evaluation（gaps 全文）・上の decision が入る。
    // 呼び出し元が付けていた [Context] 前置（日時 / テーマ / Discord message_id）も
    // ここで再現する（無いと run-2 が現在日時を失い、message_id 依存のゲートウェイ
    // 操作ができなくなる）。
    let (system_prompt, _) =
        build_agent_context(&conn, agent_id, &opencrab_actions::CallerIdentity::Owner);
    let theme = opencrab_db::queries::get_session(&conn, session_id)
        .ok()
        .flatten()
        .map(|s| s.theme)
        .unwrap_or_default();
    let runtime_text = match trigger_message_id {
        Some(message_id) if !message_id.is_empty() => {
            prepend_runtime_context_discord("", &theme, message_id)
        }
        _ => prepend_runtime_context("", &theme),
    };
    let functions_tokens = match core_functions_tokens() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error_name = e.name(),
                "loop restart aborted ({name}): {e}",
                name = e.name()
            );
            return None;
        }
    };
    let env = match resolve_agent_request_envelope(RequestEnvelopeArgs {
        conn: &conn,
        agent_id,
        session_id,
        default_model: &state.default_model,
        policy: &state.context_budget_policy(),
        system_prompt: &system_prompt,
        runtime_context_text: &runtime_text,
        functions_tokens,
        entrypoint: "process_loop_restart",
    }) {
        Ok(env) => env,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error_name = e.name(),
                "loop restart aborted ({name}): {e}",
                name = e.name()
            );
            let _ = opencrab_db::queries::insert_task_progress(
                &conn,
                task.id,
                "progress",
                &format!(
                    "[restart] {} のため自動再実行を中止した（予算は未消費）。",
                    e.name()
                ),
            );
            return None;
        }
    };
    let rebuilt = match build_conversation_string_with_waters(
        &conn,
        session_id,
        agent_id,
        env.conversation_high,
        env.conversation_low,
        include_memory_index(&env),
    ) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                "loop restart aborted (conversation rebuild failed): {e}"
            );
            let _ = opencrab_db::queries::insert_task_progress(
                &conn,
                task.id,
                "progress",
                "[restart] 会話の再構築に失敗したため自動再実行を中止した（予算は未消費）。",
            );
            return None;
        }
    };
    let rebuilt = match trigger_message_id {
        Some(message_id) if !message_id.is_empty() => {
            prepend_runtime_context_discord(&rebuilt, &theme, message_id)
        }
        _ => prepend_runtime_context(&rebuilt, &theme),
    };

    // 再実行の直前にカウントを永続化（再実行中にクラッシュしても上限判定が効く）。
    match opencrab_db::queries::increment_task_restart_count(&conn, agent_id, task.id) {
        Ok(true) => {}
        _ => return None,
    }

    tracing::info!(
        session_id = %session_id,
        task_id = task.id,
        restart_count = task.restart_count + 1,
        "restarting engine run after iteration limit (loop restart v1)"
    );
    Some(rebuilt)
}
