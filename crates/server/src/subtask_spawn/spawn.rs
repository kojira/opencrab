use std::sync::Arc;

use chrono::Utc;
use opencrab_actions::subtask::{
    settle_completed, SettleContext, SpawnedSubtask, SubtaskCompletionSink, SubtaskRegistry,
};
use opencrab_gateway::{GatewayActionResult, GatewayCallContext};
use serde_json::json;
use uuid::Uuid;

use crate::AppState;

use super::{sub_prompt::sub_system_prompt, timeout_text::timeout_result_text};

/// `timeout_secs` 省略時の既定（旧 Discord 実装と同一）。
const DEFAULT_TIMEOUT_SECS: u64 = 1800;

fn err(msg: String) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg),
    }
}

/// `spawn_subtask` の本体。
///
/// `registry` は `run_agent_response` が解決した**この run の共有登録簿**（
/// `cancel_subtask` / `report_progress` が引くのと同一 Arc）。`sink` はサブタスクの
/// 決着を親会話へ再注入する口（未配線なら DB 永続化のみ）。`root_gateway` は
/// `BridgedExecutor` が注入した合成 gateway のハンドルで、sub-engine の inner になる。
pub async fn spawn_subtask(
    state: &AppState,
    registry: Option<&SubtaskRegistry>,
    sink: Option<Arc<dyn SubtaskCompletionSink>>,
    root_gateway: Option<Arc<dyn opencrab_gateway::GatewayActions>>,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    let Some(task) = args.get("task").and_then(|v| v.as_str()) else {
        return err("spawn_subtask: 'task' argument is required".to_string());
    };
    let task = task.to_string();
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TIMEOUT_SECS);

    // セッション必須（fail-closed）: 完了通知・親ログの宛先が session_id に依存するため、
    // 不明なまま "" で進まず明示エラーにする（#36）。
    let parent_session_id = match ctx.session_id.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return err(
                "spawn_subtask はセッション文脈でのみ実行できます（session_id 不明）".to_string(),
            );
        }
    };

    // 停止の到達性（不変条件）: 登録簿が無いまま走らせると、生まれた subtask を
    // `cancel_subtask` から止められない「見えない走行」になる。fail-closed で断る。
    let Some(registry) = registry else {
        return err(
            "spawn_subtask: 走行中サブタスクの登録簿が未配線のため起動できません".to_string(),
        );
    };
    let registry = registry.clone();

    let agent_id = ctx.agent_id.clone();
    let depth = ctx.depth + 1;
    // 親ターンの呼び出し元。sub-engine の実行 caller にも、決着後に親会話を resume
    // する登録簿の caller にも、この同一の値を使う（#333）。両者が食い違うと、実行時に
    // 見えていたツールと resume 後に見えるツールがズレる。
    let parent_caller = opencrab_actions::CallerIdentity::from(&ctx.caller);
    let subtask_id = Uuid::new_v4().to_string();
    let sub_session_id = format!("subtask-{subtask_id}");
    let spawned_at = Utc::now().to_rfc3339();
    let started_instant = std::time::Instant::now();
    let label = args
        .get("label")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| task.chars().take(50).collect::<String>());

    // sub-session の行と、親セッションログへの subtask_spawned を記録する。
    let system_prompt = {
        let Ok(conn) = state.db.lock() else {
            return err("spawn_subtask: db lock failed".to_string());
        };
        let meta = json!({
            "parent_session_id": parent_session_id,
            "depth": depth,
            "subtask_id": subtask_id,
        });
        let session = opencrab_db::queries::SessionRow {
            id: sub_session_id.clone(),
            mode: "subtask".to_string(),
            theme: format!("Subtask: {}", task.chars().take(50).collect::<String>()),
            phase: "active".to_string(),
            turn_number: 0,
            status: "active".to_string(),
            participant_ids_json: json!([&agent_id]).to_string(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: Some(meta.to_string()),
        };
        opencrab_db::queries::insert_session(&conn, &session).ok();

        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: agent_id.clone(),
            session_id: parent_session_id.clone(),
            log_type: "system".to_string(),
            content: json!({
                "type": "subtask_spawned",
                "subtask_id": subtask_id,
                "session_id": sub_session_id,
                "spawned_at": spawned_at,
            })
            .to_string(),
            speaker_id: None,
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log_best_effort(&conn, &log);

        sub_system_prompt(&conn, &agent_id, &subtask_id, depth)
    };

    // --- 走行本体 -----------------------------------------------------------
    //
    // sub-engine を自前で組み立てず、通常の応答生成経路（`run_agent_response`）へ
    // depth+1 で再入する。許可リスト（`SubEngineGatewayActions`）・purpose="subtask"・
    // 反復上限なし・llm_logs への記録は、すべて向こう側が depth から導出する。

    // 開始ゲート: 親が登録簿へ insert し終えるまでタスク本体を走らせない。これが無いと、
    // 即座に失敗するサブタスクが親の insert より先に remove を実行し、その後 insert が
    // 着地して「running のまま」のエントリがリークする。
    let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();

    // 停止/決着の排他ラッチ。sub-engine が完走してから settle が DB へ着地するまでの窓で
    // cancel が入っても、完了ログと sink 発火は行われない（= 止めたのに返信が届くのを防ぐ）。
    let lifecycle = opencrab_actions::SubtaskLifecycle::new();

    let run_state = state.clone();
    let run_task = task.clone();
    let run_agent_id = agent_id.clone();
    let run_sub_session_id = sub_session_id.clone();
    let run_parent_session_id = parent_session_id.clone();
    let run_subtask_id = subtask_id.clone();
    let run_registry = registry.clone();
    let run_lifecycle = lifecycle.clone();
    // D-1087: 終端イベント用の写し（`label` は登録簿、`task` は sub-run がそれぞれ所有する）。
    let run_label = label.clone();
    let run_task_text = task.clone();
    let run_sink: Arc<dyn SubtaskCompletionSink> =
        sink.unwrap_or_else(|| Arc::new(opencrab_actions::NoopCompletionSink));
    // sub-run（クロージャ）へ渡す実行 caller。登録簿 insert 用に `parent_caller` は
    // クロージャ外へ残す（同一値のクローン）。
    let run_caller = parent_caller.clone();
    // D-1087: 開始イベントは走行前に本体スレッドから出す（closure は `run_sink` を所有する）。
    let start_sink = run_sink.clone();

    let join_handle = tokio::spawn(async move {
        // insert 完了を待つ（送信側が drop された場合も先へ進む）。
        let _ = start_rx.await;

        let req = opencrab_actions::RunRequest::new(
            run_agent_id.clone(),
            run_agent_id.clone(),
            run_sub_session_id.clone(),
            system_prompt,
            run_task,
            // RuntimeInfo の gateway 名。旧実装と同じく "subtask"。
            "subtask",
            // 親ターンの呼び出し元を継承する（#333）。旧実装は素の `Agent` に固定して
            // いたが、それは「守るものが無い」制約だった: 同じ作業をメインターン
            // （caller=親のまま）で直接やれば通るのに、サブへ委譲した瞬間だけ owner 限定
            // ツールが消える、という委譲都合の非対称を生むだけで、外部由来ターンは
            // `spawn_subtask` を挟めば逆に制限を迂回できた（サブ = 素の Agent でも
            // execute_shell 等が無ゲートで通っていた / #330 で塞ぐ前）。継承すれば軸が
            // 「誰の指示か」だけに揃う: 親 Owner → サブ Owner（当然使える）、親 Agent →
            // サブ Agent（**昇格しない**。外部由来のサブは制限されたまま = 迂回封鎖）。
            // `from` は親より強い identity を生まないので昇格経路にはならない。
            run_caller,
        )
        .with_depth(depth);
        // 親と同一の登録簿・完了受け口を渡す。sub-engine の `report_progress` は
        // ここから自分自身のエントリを引き、進捗を親セッションへ再注入する。
        let mut req = req.with_dispatch(Some(run_registry.clone()), run_sink.clone());
        // D-1087: 副実行のツール実行も同じ活動の観測者に載せる（旧実装で run_notifier を
        // 子へ引き継いでいたのと同じ範囲）。購読が無ければ `None` で、executor は素通りする。
        if let Some(sink) = run_sink.tool_event_sink() {
            req = req.with_tool_event_sink(sink);
        }
        if let Some(root) = root_gateway {
            req = req.with_gateway_actions(root);
        }

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            crate::process::run_agent_response(&run_state, req),
        )
        .await;

        let (exit_reason, result_text) = match result {
            Ok(Ok(engine_result)) => {
                let exit_reason = if engine_result.stopped_by_limit {
                    "stopped_by_limit"
                } else {
                    "completed"
                };
                (exit_reason.to_string(), engine_result.response)
            }
            Ok(Err(e)) => ("error".to_string(), format!("Error: {e}")),
            // #332: タイムアウトだけは「完了」ではなく「未完了・対応が必要」と読める本文に
            // する（他の exit_reason は不変）。返信は強制しない＝情報として渡すだけ。
            Err(_) => (
                "timeout".to_string(),
                timeout_result_text(
                    &run_sub_session_id,
                    started_instant.elapsed().as_secs(),
                    timeout_secs,
                ),
            ),
        };

        // D-1087: 終端（正常 / 異常 / タイムアウト）を活動の観測者へ伝える。表示への写像と
        // 購読フィルタは実装側の責務。`settle_completed` の前に出すのは、決着の記録と
        // 通知の順序を旧実装と同じ「終端通知 → DB・sink」に保つため。
        run_sink.on_subtask_lifecycle(&opencrab_actions::SubtaskLifecycleEvent {
            subtask_id: &run_subtask_id,
            parent_session_id: &run_parent_session_id,
            agent_id: &run_agent_id,
            status: opencrab_actions::SubtaskLifecycleStatus::from_exit_reason(&exit_reason),
            label: &run_label,
            task: &run_task_text,
            detail: None,
            duration_ms: Some(started_instant.elapsed().as_millis() as u64),
            result_text: Some(&result_text),
        });

        // 保留中の progress デバウンスを無効化する。エントリが消えると、まだ sleep 中の
        // デバウンスタスクは「最新ではない」扱いになり発火しない。これが無いと、終了
        // イベントの後に遅延 progress（0〜3秒窓）が届いて完了返信の直後に余計な推論・
        // 重複返信が走ることがある。
        run_state.progress_debounce.clear(&run_parent_session_id);

        // --- 中核（gateway 非依存）: DB へ subtask_completed を永続化 → 登録簿から除去
        //     → sink 発火。順序契約は settle_completed が 1 箇所で保証する。---
        settle_completed(
            &run_registry,
            &run_state.db,
            run_sink.as_ref(),
            SettleContext {
                parent_session_id: run_parent_session_id,
                agent_id: run_agent_id,
                subtask_id: run_subtask_id,
                sub_session_id: run_sub_session_id,
                exit_reason,
                lifecycle: run_lifecycle,
            },
            &result_text,
        );
    });

    // D-1087: 開始を活動の観測者へ伝える。走行を許す前に 1 回だけ発火するので、
    // この走行の他のイベント（進捗・終端）より必ず先に届く。購読が無いときの
    // `NoopCompletionSink` は既定実装で何もしない。
    start_sink.on_subtask_lifecycle(&opencrab_actions::SubtaskLifecycleEvent {
        subtask_id: &subtask_id,
        parent_session_id: &parent_session_id,
        agent_id: &agent_id,
        status: opencrab_actions::SubtaskLifecycleStatus::Started,
        label: &label,
        task: &task,
        detail: None,
        duration_ms: None,
        result_text: None,
    });

    let abort_handle = join_handle.abort_handle();
    registry.insert(
        subtask_id.clone(),
        SpawnedSubtask {
            abort_handle,
            session_id: sub_session_id.clone(),
            parent_session_id: parent_session_id.clone(),
            agent_id: agent_id.clone(),
            label,
            tool_name: "spawn_subtask".to_string(),
            started_at: started_instant,
            // 親ターンの返信先（発端イベントの origin）を引き継ぐ。決着→resume ターンの say が
            // この origin へ e-tag reply できるようにする（gateway は session_id から返信先を
            // 復元できないため）。ctx.reply_target は RunRequest.reply_target と同じ不透明 token。
            reply_target: ctx.reply_target.clone(),
            // 親ターンの呼び出し元を保持する（#298）。sub-engine の実行 caller
            // （上の `parent_caller`）と同一の値で、決着後に**親会話を resume** する
            // sink も元の権限で再開する。ここで落とすと、オーナー発のターンが subtask
            // 決着の瞬間に Agent へ降格する。
            caller: parent_caller.clone(),
            lifecycle,
            // 明示 `spawn_subtask` は自前の LLM ループ（`run_agent_response` 再入）を持ち、
            // 反復の合間に steer ログを読める。steer 可（#647）。
            steerable: true,
        },
    );
    // insert が完了したのでタスク本体の実行を許可する。
    let _ = start_tx.send(());

    GatewayActionResult {
        success: true,
        data: Some(json!({
            "status": "spawned",
            "subtask_id": subtask_id,
            "session_id": sub_session_id,
            "spawned_at": spawned_at,
        })),
        error: None,
    }
}
