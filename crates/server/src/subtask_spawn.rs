//! `spawn_subtask` の gateway 非依存実装（#175 S4）。
//!
//! 旧実装は Discord ゲートウェイ（`crates/discord` の `execute_spawn_subtask`）にあり、
//! LLM クライアント・既定モデル・ワークスペース・ツール設定を Discord 側に持たせて
//! **sub-engine を自前で組み立てていた**。その構築コードは `process::run_agent_response`
//! の engine 構築とツール登録・許可コマンドのマージ・executor 構築・ログ記録・
//! workspace 解決・モデル解決までほぼ同一で、コメントまで同文だった。
//!
//! ここでは sub-engine を組み立てず、**`process::run_agent_response` を depth+1 で
//! 再入呼び出し**する。差分として明示的に渡すのは次の 4 つだけ:
//!
//! 1. 許可リスト（`SubEngineGatewayActions`）— `run_agent_response` が depth>=1 で自動的に
//!    合成 gateway の最外周へ被せる。
//! 2. `current_purpose = "subtask"` 相当 — 同じく depth から導出される。
//! 3. 通知（`SubtaskRunNotifier` / #175 S3）— `RunRequest::with_run_notifier`。
//! 4. タイムアウト — ここで `tokio::time::timeout` として掛ける。
//!
//! 守る不変条件（壊すと重大。順に対応するテストがある）:
//! - **順序契約**: DB 永続化 → 登録簿から除去 → 完了通知。`settle_completed` を必ず経由する。
//! - **停止の到達性**: spawn した subtask は `cancel_subtask` が引くのと同一の登録簿に入れる。
//! - **開始ゲート**: 登録簿へ insert し終えるまでタスク本体を走らせない。
//! - **ネスト禁止**: sub-engine の許可リストに `spawn_subtask` を含めない。

use std::sync::Arc;

use chrono::Utc;
use opencrab_actions::subtask::{
    settle_completed, SettleContext, SpawnedSubtask, SubtaskCompletionSink, SubtaskRegistry,
};
use opencrab_actions::subtask_notify::SubtaskRunInfo;
use opencrab_gateway::{GatewayActionResult, GatewayCallContext};
use serde_json::json;
use uuid::Uuid;

use crate::AppState;

/// `timeout_secs` 省略時の既定（旧 Discord 実装と同一）。
const DEFAULT_TIMEOUT_SECS: u64 = 1800;

fn err(msg: String) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg),
    }
}

/// サブエンジン用の system prompt（旧 Discord 実装の文面をそのまま保つ）。
fn sub_system_prompt(
    conn: &rusqlite::Connection,
    agent_id: &str,
    subtask_id: &str,
    depth: u32,
) -> String {
    let (personality, instructions) = opencrab_db::queries::get_agent(conn, agent_id)
        .ok()
        .flatten()
        .map(|a| (a.personality.unwrap_or_default(), a.instructions))
        .unwrap_or_default();
    let personality_section = if personality.is_empty() {
        String::new()
    } else {
        format!("{personality}\n\n")
    };
    let instructions_section = if instructions.is_empty() {
        String::new()
    } else {
        format!("\n\n## Instructions\n{instructions}")
    };
    format!(
        "{personality_section}\
         あなたはサブエンジンとして起動されています。\n\
         - subtask_id: {subtask_id}\n\
         - depth: {depth}\n\
         - Discordへの直接送信は禁止されています\n\
         - 進捗報告は report_progress を使ってください（subtask_id 引数は省略可。省略時はこのサブタスクとして報告されます）\n\
         - タスク完了時はテキストで結果を返してください（Discord送信はメインエンジンが行います）\n\n\
         You are a sub-engine executing a delegated task.\
         {instructions_section}"
    )
}

/// #332: subtask がタイムアウトで打ち切られたときに `subtask_completed` の本文
/// （`result`）として残す文言を組む。**タイムアウト限定**（他の `exit_reason` の本文は
/// 一切変えない）。
///
/// 旧文言は `"Subtask timed out."` の一文だけで、「終わった」という通知に見えて何を
/// すべきかが書かれておらず、エージェントが `NO_REPLY` を選んでいた（issue #332 実測）。
/// ここでは **「未完了・対応が必要」と読める形**にし、途中経過の在り処（sub セッションの
/// ログ）と、経過・上限の実数を添える。
///
/// **返信は強制しない**。これは情報として渡すだけで、実際に反応するかどうかは
/// エージェントが決める（オーナー方針「人間の反応を無視するかはエージェントが決めればいい。
/// 強制はしなくていい」/ 評価を会話へ割り込ませない #291・#292 と同じ線引き）。文言は
/// 具体的な機能（例: 再開）を前提にせず、いま取れる手だけを例示にとどめる。
fn timeout_result_text(sub_session_id: &str, elapsed_secs: u64, timeout_secs: u64) -> String {
    format!(
        "サブタスクが制限時間（{timeout_secs}秒）内に終わらず、約{elapsed_secs}秒で打ち切られました。\
         未完了なので対応が必要です。どこまで進んだかは `{sub_session_id}` セッションのログに\
         残っているので、確認して次の手（別の方法を試す・改めて依頼し直す・見送る等）を\
         判断してください。"
    )
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

    // lifecycle 通知は抽象境界（`SubtaskLifecycleNotifier`）越しに扱う（#175 S3）。
    // 宛先の解決・配送ワーカーの起動・整形はすべて実装側に閉じており、ここは
    // 「起きた事実」を渡すだけ。解決に失敗したら spawn しない（fail-closed）。
    // raw url はどこにも出さない。
    let notify = match state
        .subtask_lifecycle_notifier()
        .begin_run(&SubtaskRunInfo {
            agent_id: &agent_id,
            subtask_id: &subtask_id,
            sub_session_id: &sub_session_id,
            parent_session_id: &parent_session_id,
            label: &label,
            tool_args: args,
        }) {
        Ok(session) => session,
        Err(e) => {
            return GatewayActionResult {
                success: false,
                error: Some(format!("{}: {}", e.code, e.message)),
                data: Some(json!({
                    "webhook_source": e.source,
                    "webhook_status": "error",
                    "webhook_error": e.message,
                })),
            };
        }
    };
    let notifier = notify.notifier;
    let webhook_source = notify.target.source;
    let webhook_status = notify.target.status;
    let webhook_redacted_url = notify.target.redacted_url;

    // 開始を通知する。
    notifier.on_started(&task);

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
                "webhook_source": webhook_source,
                "webhook_status": webhook_status,
                "webhook_redacted_url": webhook_redacted_url,
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
    let run_notifier = notifier.clone();
    let run_lifecycle = lifecycle.clone();
    let run_sink: Arc<dyn SubtaskCompletionSink> =
        sink.unwrap_or_else(|| Arc::new(opencrab_actions::NoopCompletionSink));
    let run_notifiers = state.subtask_notifiers.clone();
    // sub-run（クロージャ）へ渡す実行 caller。登録簿 insert 用に `parent_caller` は
    // クロージャ外へ残す（同一値のクローン）。
    let run_caller = parent_caller.clone();

    let join_handle = tokio::spawn(async move {
        // insert 完了を待つ（送信側が drop された場合も先へ進む）。
        let _ = start_rx.await;

        let mut req = opencrab_actions::RunRequest::new(
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
        .with_depth(depth)
        .with_run_notifier(run_notifier.clone());
        // 親と同一の登録簿・完了受け口を渡す。sub-engine の `report_progress` は
        // ここから自分自身のエントリを引き、進捗を親セッションへ再注入する。
        req = req.with_dispatch(Some(run_registry.clone()), run_sink.clone());
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

        // --- gateway 固有の後始末（DB 永続化 / sink 発火の前に済ませる。これらは
        //     DB 永続化とも sink 発火とも順序依存が無い＝webhook は非同期配送・別マップ）。---

        // 終了（正常 / 異常 / タイムアウト）を通知する。表示状態への写像と購読フィルタは
        // 通知実装側の責務。
        run_notifier.on_finished(
            &exit_reason,
            started_instant.elapsed().as_millis() as u64,
            &result_text,
        );

        // 保留中の progress デバウンスを無効化する。エントリが消えると、まだ sleep 中の
        // デバウンスタスクは「最新ではない」扱いになり発火しない。これが無いと、終了
        // イベントの後に遅延 progress（0〜3秒窓）が届いて完了返信の直後に余計な推論・
        // 重複返信が走ることがある。
        run_state.progress_debounce.clear(&run_parent_session_id);

        // 通知口を登録簿と対で除去する。
        run_notifiers.remove(&run_subtask_id);

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
            // 明示 spawn の返信先は親セッションから復元する（sink 側の責務 / #167）。
            reply_target: None,
            // 親ターンの呼び出し元を保持する（#298）。sub-engine の実行 caller
            // （上の `parent_caller`）と同一の値で、決着後に**親会話を resume** する
            // sink も元の権限で再開する。ここで落とすと、オーナー発のターンが subtask
            // 決着の瞬間に Agent へ降格する。
            caller: parent_caller.clone(),
            lifecycle,
        },
    );
    // 通知口は登録簿と対の随伴マップへ分離する（RFC §1.5）。cancel / report_progress は
    // ここから引く。
    state.subtask_notifiers.insert(subtask_id.clone(), notifier);

    // insert が完了したのでタスク本体の実行を許可する。
    let _ = start_tx.send(());

    GatewayActionResult {
        success: true,
        data: Some(json!({
            "status": "spawned",
            "subtask_id": subtask_id,
            "session_id": sub_session_id,
            "spawned_at": spawned_at,
            "webhook_source": webhook_source,
            "webhook_redacted_url": webhook_redacted_url,
            "webhook_status": webhook_status,
            "webhook_error": serde_json::Value::Null,
        })),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_actions::subtask::{SettleKind, SubtaskSettled};
    use opencrab_gateway::{GatewayActions as _, GatewayCaller};

    /// #332: タイムアウトの本文が「未完了・対応が必要」と読め、途中経過の在り処
    /// （`subtask-{id}` セッション）と経過・上限の実数を含むこと。**返信は強制しない**
    /// ので命令形の強制文言（「返信せよ」等）は入れない。旧文言 `"Subtask timed out."`
    /// のような「終わった」だけの通知に戻ったら落ちる。
    #[test]
    fn timeout_result_text_prompts_action_without_forcing_reply() {
        let text = timeout_result_text("subtask-abc123", 300, 300);

        // 「対応が必要」と読める（issue の核心: 何をすべきか分かる）。
        assert!(text.contains("対応が必要"), "対応を促す文言が無い: {text}");
        // 「未完了」と明示する（#444 と同じく timeout を completed と言わない）。
        assert!(text.contains("未完了"), "未完了と明示していない: {text}");
        // 完了を肯定する言い回しにはしない（「完了しました」等）。
        assert!(
            !text.contains("完了しました"),
            "timeout なのに完了を断言している: {text}"
        );
        // 途中経過の在り処（sub セッション名 = subtask-{id}）を指す。
        assert!(
            text.contains("subtask-abc123"),
            "ログの在り処（sub セッション）が無い: {text}"
        );
        // 経過秒・上限秒の実数が入る（何がどれだけ経ったか）。
        assert!(text.contains("300"), "経過/上限の実数が無い: {text}");
        // 返信を強制しない: 命令的な返信要求を入れない（促すが強制はしない）。
        assert!(
            !text.contains("返信して") && !text.contains("必ず返信"),
            "返信を強制する文言が入っている: {text}"
        );
        // 旧文言そのものには戻らない。
        assert_ne!(text, "Subtask timed out.");
    }

    /// 経過秒と上限秒は引数がそのまま反映される（固定文字列ではない）。
    #[test]
    fn timeout_result_text_reflects_elapsed_and_limit() {
        let text = timeout_result_text("subtask-xyz", 42, 120);
        assert!(text.contains("120"), "上限秒が反映されない: {text}");
        assert!(text.contains("42"), "経過秒が反映されない: {text}");
        assert!(text.contains("subtask-xyz"));
    }
    use std::sync::Mutex;

    use crate::system_actions::SystemGatewayActions;

    // ---- テスト用の LLM プロバイダ（"mock:test" として登録する） ----

    /// 1 往復で終わる stub。`hang=true` なら永久に返さない（cancel の対象用）。
    struct StubProvider {
        reply: String,
        hang: bool,
    }

    #[async_trait::async_trait]
    impl opencrab_llm::traits::LlmProvider for StubProvider {
        fn name(&self) -> &str {
            "mock"
        }
        async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
            Ok(vec![])
        }
        async fn chat_completion(
            &self,
            request: opencrab_llm::message::ChatRequest,
        ) -> anyhow::Result<opencrab_llm::message::ChatResponse> {
            if self.hang {
                std::future::pending::<()>().await;
            }
            Ok(opencrab_llm::message::ChatResponse {
                id: "resp-1".to_string(),
                model: request.model,
                choices: vec![opencrab_llm::message::Choice {
                    index: 0,
                    message: opencrab_llm::message::Message::assistant(&self.reply),
                    finish_reason: Some(opencrab_llm::message::FinishReason::Stop),
                }],
                usage: Default::default(),
                created: 0,
            })
        }
    }

    /// sub-run が LLM へ提示したツール名（`ChatRequest.functions`）を毎コール記録する
    /// stub。sub-engine の**実行 caller** を、その caller で見えるはずのツールの有無で
    /// 観測するために使う（#333）。`Stop` で 1 反復で終わる。
    struct CapturingStub {
        seen: Arc<Mutex<Vec<Vec<String>>>>,
    }

    #[async_trait::async_trait]
    impl opencrab_llm::traits::LlmProvider for CapturingStub {
        fn name(&self) -> &str {
            "mock"
        }
        async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
            Ok(vec![])
        }
        async fn chat_completion(
            &self,
            request: opencrab_llm::message::ChatRequest,
        ) -> anyhow::Result<opencrab_llm::message::ChatResponse> {
            let names = request
                .functions
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|f| f.name)
                .collect::<Vec<_>>();
            self.seen.lock().unwrap().push(names);
            Ok(opencrab_llm::message::ChatResponse {
                id: "resp-1".to_string(),
                model: request.model,
                choices: vec![opencrab_llm::message::Choice {
                    index: 0,
                    message: opencrab_llm::message::Message::assistant("done"),
                    finish_reason: Some(opencrab_llm::message::FinishReason::Stop),
                }],
                usage: Default::default(),
                created: 0,
            })
        }
    }

    /// 完了を**テストの合図まで遅延**させる stub。`gate` が `true` を受け取るまで
    /// `chat_completion` が返らないので、その間サブタスクは settle できず、共有登録簿に
    /// 載ったままになる。#450: 「spawn 直後は登録簿に載っている」という assert を、子が
    /// 即完了して registry から remove する競合から切り離すために使う。
    ///
    /// `sleep` で待つ形（＝競合を隠すだけで遅いマシンで再発する）ではなく、**完了の順序を
    /// 固定する**。親が登録を確認し終えるまで子は決着できない。合図後は latch が開いた
    /// ままになるので、`chat_completion` が複数回呼ばれてもブロックしない。
    struct GatedStub {
        reply: String,
        gate: tokio::sync::watch::Receiver<bool>,
    }

    #[async_trait::async_trait]
    impl opencrab_llm::traits::LlmProvider for GatedStub {
        fn name(&self) -> &str {
            "mock"
        }
        async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
            Ok(vec![])
        }
        async fn chat_completion(
            &self,
            request: opencrab_llm::message::ChatRequest,
        ) -> anyhow::Result<opencrab_llm::message::ChatResponse> {
            // 合図（true）が来るまで返さない = サブタスクを走行中のまま保つ。送信側が
            // drop されたら（テスト終了時など）先へ進む。
            let mut gate = self.gate.clone();
            loop {
                if *gate.borrow_and_update() {
                    break;
                }
                if gate.changed().await.is_err() {
                    break;
                }
            }
            Ok(opencrab_llm::message::ChatResponse {
                id: "resp-1".to_string(),
                model: request.model,
                choices: vec![opencrab_llm::message::Choice {
                    index: 0,
                    message: opencrab_llm::message::Message::assistant(&self.reply),
                    finish_reason: Some(opencrab_llm::message::FinishReason::Stop),
                }],
                usage: Default::default(),
                created: 0,
            })
        }
    }

    /// テストで使う親エージェント `agent-x` を `agents` 行として登録する（#632）。
    ///
    /// サブタスクは親と同じ `agent_id` で sub-run を回すため、`run_agent_response` の
    /// 存在チョークポイント（#632）を通すには行が必要。以前は行を作らず既定に落ちて
    /// 動いていた（＝ #632 の症状そのもの）ので、テスト側で実在させる。
    fn insert_agent_x(state: &AppState) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: "agent-x".to_string(),
                name: "Agent X".to_string(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "Tester".to_string(),
                personality: None,
                instructions: String::new(),
                heartbeat_instructions: String::new(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .unwrap();
    }

    /// `mock:test` を解決できる `AppState`（Discord を一切通さない = web / REST 相当）。
    fn state_with_stub_llm(reply: &str, hang: bool) -> AppState {
        let state = crate::test_app_state();
        insert_agent_x(&state);
        let mut router = opencrab_llm::router::LlmRouter::new();
        router.add_provider(Arc::new(StubProvider {
            reply: reply.to_string(),
            hang,
        }));
        state.llm_router.swap(router);
        state
    }

    /// `state_with_stub_llm` の gated 版。sub-run の完了を `gate` が `true` を受け取る
    /// まで遅延させる（#450）。
    fn state_with_gated_llm(reply: &str, gate: tokio::sync::watch::Receiver<bool>) -> AppState {
        let state = crate::test_app_state();
        insert_agent_x(&state);
        let mut router = opencrab_llm::router::LlmRouter::new();
        router.add_provider(Arc::new(GatedStub {
            reply: reply.to_string(),
            gate,
        }));
        state.llm_router.swap(router);
        state
    }

    /// 決着を記録する sink。**順序契約の検証**のため、通知を受けた時点で
    /// `subtask_completed` が既に DB へ着地しているかも同時に記録する。
    struct OrderCheckingSink {
        db: opencrab_db::Db,
        /// (kind, exit_reason, 通知時点で完了ログが DB にあったか)
        seen: Mutex<Vec<(SettleKind, String, bool)>>,
    }

    impl OrderCheckingSink {
        fn new(db: opencrab_db::Db) -> Self {
            Self {
                db,
                seen: Mutex::new(Vec::new()),
            }
        }
        fn seen(&self) -> Vec<(SettleKind, String, bool)> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl SubtaskCompletionSink for OrderCheckingSink {
        fn on_subtask_settled(&self, ev: SubtaskSettled) {
            let persisted = has_log_of_type(&self.db, &ev.session_id, "subtask_completed");
            self.seen
                .lock()
                .unwrap()
                .push((ev.kind, ev.exit_reason, persisted));
        }
        fn on_subtask_cancelled(&self, ev: SubtaskSettled) {
            self.seen
                .lock()
                .unwrap()
                .push((ev.kind, ev.exit_reason, false));
        }
    }

    /// 親セッションログに指定 type の system ログがあるか。
    fn has_log_of_type(db: &opencrab_db::Db, session_id: &str, log_type: &str) -> bool {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, session_id)
            .unwrap_or_default()
            .iter()
            .any(|row| {
                serde_json::from_str::<serde_json::Value>(&row.content)
                    .ok()
                    .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
                    .as_deref()
                    == Some(log_type)
            })
    }

    /// `subtask_completed` ログの本文（result）を返す。
    fn completed_result(db: &opencrab_db::Db, session_id: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, session_id)
            .unwrap_or_default()
            .iter()
            .find_map(|row| {
                let v: serde_json::Value = serde_json::from_str(&row.content).ok()?;
                if v.get("type")?.as_str()? != "subtask_completed" {
                    return None;
                }
                Some(v.get("result")?.as_str()?.to_string())
            })
    }

    fn registry() -> SubtaskRegistry {
        Arc::new(dashmap::DashMap::new())
    }

    fn parent_ctx(session_id: &str) -> GatewayCallContext {
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id(session_id)
    }

    /// spawn 直後に返る subtask_id。
    fn spawned_id(res: &GatewayActionResult) -> String {
        res.data.as_ref().unwrap()["subtask_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..400 {
            if cond() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        cond()
    }

    /// **#175 S4 の主目的**: Discord を通さない経路（web / REST 相当 = inner gateway なし）
    /// から `spawn_subtask` が動き、完了が親セッションログへ着地する。
    ///
    /// 旧実装は Discord ゲートウェイにしか無く、REST は LLM クライアントとして `None` を
    /// 渡していたため「no LLM client available」で必ず失敗していた。
    ///
    /// #450: 「spawn 直後は登録簿に載っている」assert は、子が即完了して registry から
    /// remove した後に親が assert する競合を塞げていなかった（`:233-` の開始ゲートは
    /// 「親 insert より先に子が remove しない」順序しか保証しない）。ここでは
    /// **完了を `gate` の合図まで遅延**させ、親が登録を確認し終えるまで子が決着できない
    /// ようにして順序を固定する（`sleep` で隠さない）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_subtask_runs_and_settles_without_discord() {
        // 子の完了ゲート。親が登録を確認するまで `true` を送らない。
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        let state = state_with_gated_llm("sub-engine done", gate_rx);
        let reg = registry();
        let sink = Arc::new(OrderCheckingSink::new(state.db.clone()));
        let actions = SystemGatewayActions::new(
            state.clone(),
            None, // inner gateway 無し（= web / REST / Nostr 経路）
            Some(reg.clone()),
            Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
        );

        let res = actions
            .execute(
                "spawn_subtask",
                &json!({ "task": "調べ物をする", "label": "job" }),
                &parent_ctx("web-parent-1"),
            )
            .await;
        assert!(res.success, "spawn_subtask: {:?}", res.error);
        let subtask_id = spawned_id(&res);

        // spawn は即座に返り、走行中エントリが共有登録簿に載っている。子はまだ `gate` の
        // 合図待ちで settle できないため、この確認は競合なく成立する（#450）。ここで登録が
        // 無ければ本物の退行（spawn したのに登録されない）＝赤になる。
        assert_eq!(res.data.as_ref().unwrap()["status"], "spawned");
        assert!(
            reg.contains_key(&subtask_id),
            "spawn 直後は登録簿に載っていなければならない"
        );
        // 親セッションログに subtask_spawned が残る。
        assert!(has_log_of_type(
            &state.db,
            "web-parent-1",
            "subtask_spawned"
        ));

        // 登録を確認したので、子の完了を許可する。
        gate_tx
            .send(true)
            .expect("gate receiver は sub-run が保持している");

        // 決着を待つ。
        assert!(
            wait_until(|| !sink.seen().is_empty()).await,
            "完了通知が届かない"
        );
        let seen = sink.seen();
        assert_eq!(seen.len(), 1, "決着通知はちょうど 1 本: {seen:?}");
        assert_eq!(seen[0].0, SettleKind::Completed);
        assert_eq!(seen[0].1, "completed", "sub-engine は正常終了する");
        // **順序契約**: sink が呼ばれた時点で完了本文は既に DB へ永続化されている。
        assert!(seen[0].2, "順序契約違反: DB 永続化より先に sink が発火した");

        assert_eq!(
            completed_result(&state.db, "web-parent-1").as_deref(),
            Some("sub-engine done"),
            "完了本文が親セッションログへ着地する"
        );
        // 決着後は登録簿からも随伴マップからも消える。
        assert!(!reg.contains_key(&subtask_id));
        assert!(!state.subtask_notifiers.contains_key(&subtask_id));
    }

    /// #431: **明示 `spawn_subtask` も**親ターンの subtask 起動カウンタを進める。
    ///
    /// auto-dispatch 経路（`SubtaskToolDispatcher`）だけを数えていると、この経路で
    /// 掘削を始めたターンに「発言終わり」🏁 が付き、『調べますね🏁』の数分後に完了
    /// resume の続きが届く逆情報になる。両経路が**同じカウンタ**へ載ることを固定する。
    ///
    /// 起動に**失敗**したターンは数えない（resume が来ない＝そのターンが最後の発話
    /// なので 🏁 は付くのが正しい）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_subtask_counts_the_start_for_the_parent_turn() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let state = state_with_stub_llm("never", true);
        let reg = registry();
        let starts = Arc::new(AtomicUsize::new(0));
        let actions = SystemGatewayActions::new(state.clone(), None, Some(reg.clone()), None)
            .with_subtask_starts(Some(starts.clone()));

        let res = actions
            .execute(
                "spawn_subtask",
                &json!({ "task": "調べ物をする", "label": "job" }),
                &parent_ctx("web-parent-count"),
            )
            .await;
        assert!(res.success, "spawn_subtask: {:?}", res.error);
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "起動が成立したら親ターンのカウンタが進む"
        );

        // 起動に失敗するターン（`task` 引数なし）は数えない。
        let failed = actions
            .execute("spawn_subtask", &json!({}), &parent_ctx("web-parent-count"))
            .await;
        assert!(!failed.success, "task 引数なしは失敗する");
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "起動に失敗したターンは数えない（resume が来ないので 🏁 は付いてよい）"
        );
    }

    /// **停止の到達性**: spawn した subtask は、同じ `SystemGatewayActions` の
    /// `cancel_subtask` が引く**同一の登録簿**に入る。別の登録簿へ入れると not found。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawned_subtask_is_cancellable_through_the_shared_registry() {
        let state = state_with_stub_llm("never", true);
        let reg = registry();
        let sink = Arc::new(OrderCheckingSink::new(state.db.clone()));
        let actions = SystemGatewayActions::new(
            state.clone(),
            None,
            Some(reg.clone()),
            Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
        );

        let res = actions
            .execute(
                "spawn_subtask",
                &json!({ "task": "終わらない仕事", "label": "endless" }),
                &parent_ctx("web-parent-1"),
            )
            .await;
        assert!(res.success, "spawn_subtask: {:?}", res.error);
        let subtask_id = spawned_id(&res);

        // 親セッションから停止できる。
        let cancelled = actions
            .execute(
                "cancel_subtask",
                &json!({ "subtask_id": subtask_id }),
                &parent_ctx("web-parent-1"),
            )
            .await;
        assert!(
            cancelled.success,
            "spawn した subtask は同一登録簿から停止できなければならない: {:?}",
            cancelled.error
        );
        assert!(!reg.contains_key(&subtask_id), "停止後は登録簿から消える");
        assert!(has_log_of_type(
            &state.db,
            "web-parent-1",
            "subtask_spawned"
        ));

        // 停止は `on_subtask_cancelled`（resume しない別メソッド）で通知される。
        let seen = sink.seen();
        assert_eq!(seen.len(), 1, "停止通知は 1 本: {seen:?}");
        assert_eq!(seen[0].0, SettleKind::Cancelled);

        // 二重決着しない: 完了ログは着地しない（止めたのに返信が届くのを防ぐ）。
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !has_log_of_type(&state.db, "web-parent-1", "subtask_completed"),
            "停止した subtask の完了ログが着地してはならない"
        );
    }

    /// **開始ゲート**: 走行が終わる時点では必ず登録簿へ登録済み。登録より先に決着すると
    /// 「running のまま」のエントリがリークする。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subtask_is_registered_before_the_run_can_finish() {
        /// 終了時点の登録簿の状態を記録する通知口。
        struct RegistryWatcher {
            registry: SubtaskRegistry,
            registered_at_finish: Mutex<Option<bool>>,
        }
        impl opencrab_actions::subtask_notify::SubtaskRunNotifier for RegistryWatcher {
            fn on_finished(&self, _exit: &str, _ms: u64, _text: &str) {
                *self.registered_at_finish.lock().unwrap() = Some(!self.registry.is_empty());
            }
        }
        struct WatcherFactory(Arc<RegistryWatcher>);
        impl opencrab_actions::subtask_notify::SubtaskLifecycleNotifier for WatcherFactory {
            fn begin_run(
                &self,
                _run: &SubtaskRunInfo<'_>,
            ) -> Result<
                opencrab_actions::subtask_notify::SubtaskNotifySession,
                opencrab_actions::subtask_notify::NotifyTargetError,
            > {
                Ok(opencrab_actions::subtask_notify::SubtaskNotifySession {
                    notifier: self.0.clone(),
                    target: opencrab_actions::subtask_notify::NotifyTarget::none(),
                })
            }
        }

        let state = state_with_stub_llm("fast", false);
        let reg = registry();
        let watcher = Arc::new(RegistryWatcher {
            registry: reg.clone(),
            registered_at_finish: Mutex::new(None),
        });
        *state.subtask_lifecycle_notifier.lock().unwrap() =
            Some(Arc::new(WatcherFactory(watcher.clone())));

        let sink = Arc::new(OrderCheckingSink::new(state.db.clone()));
        let actions = SystemGatewayActions::new(
            state.clone(),
            None,
            Some(reg.clone()),
            Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
        );
        let res = actions
            .execute(
                "spawn_subtask",
                &json!({ "task": "すぐ終わる" }),
                &parent_ctx("web-parent-1"),
            )
            .await;
        assert!(res.success, "{:?}", res.error);

        assert!(
            wait_until(|| watcher.registered_at_finish.lock().unwrap().is_some()).await,
            "終了通知が届かない"
        );
        assert_eq!(
            *watcher.registered_at_finish.lock().unwrap(),
            Some(true),
            "開始ゲートが無いと、登録より先に決着してエントリがリークする"
        );
        // 決着後は空（リークしていない）。
        assert!(
            wait_until(|| reg.is_empty()).await,
            "登録簿にエントリが残った"
        );
    }

    /// #298/#333: spawn した subtask は**親ターンの呼び出し元**を登録簿に持つ。
    ///
    /// 決着時に `settle_completed` がこれを読んで sink へ渡し、resume が元の権限で走る。
    /// #333 以降は sub-engine の実行 caller も同じ親 caller（`parent_caller`）なので、
    /// 登録簿の caller は「実行時に見えていた権限」と一致する（resume でズレない）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawned_subtask_records_the_parent_caller() {
        let state = state_with_stub_llm("never", true);
        let reg = registry();
        let actions = SystemGatewayActions::new(state.clone(), None, Some(reg.clone()), None);

        let ctx = GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
            .with_session_id("discord-agent-x-1-2");
        let res = actions
            .execute("spawn_subtask", &json!({ "task": "長い仕事" }), &ctx)
            .await;
        assert!(res.success, "spawn_subtask: {:?}", res.error);

        let subtask_id = spawned_id(&res);
        let entry = reg.get(&subtask_id).expect("登録簿に載る");
        assert_eq!(
            entry.caller,
            opencrab_actions::CallerIdentity::Owner,
            "親ターンの呼び出し元が登録簿に保持されていない（resume で降格する）"
        );
        entry.abort_handle.abort();
    }

    /// 昇格経路にはしない: 親が `Agent` なら登録簿の caller も `Agent`。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawned_subtask_does_not_escalate_agent_callers() {
        let state = state_with_stub_llm("never", true);
        let reg = registry();
        let actions = SystemGatewayActions::new(state.clone(), None, Some(reg.clone()), None);

        let res = actions
            .execute(
                "spawn_subtask",
                &json!({ "task": "長い仕事" }),
                &parent_ctx("web-parent-1"),
            )
            .await;
        assert!(res.success, "{:?}", res.error);
        let subtask_id = spawned_id(&res);
        let entry = reg.get(&subtask_id).expect("登録簿に載る");
        assert_eq!(entry.caller, opencrab_actions::CallerIdentity::Agent);
        entry.abort_handle.abort();
    }

    /// shell を有効化した `AppState` に capturing stub を挿す。
    fn state_with_shell_and_capture(seen: Arc<Mutex<Vec<Vec<String>>>>) -> AppState {
        let state = crate::test_app_state();
        insert_agent_x(&state);
        {
            let mut cfg = state.tools_config.write().unwrap();
            cfg.enabled = true;
            cfg.shell = Some(opencrab_actions::tools::ShellToolConfig {
                enabled: true,
                allowed_commands: vec!["ls".to_string()],
                timeout_secs: 30,
                max_timeout_secs: 300,
                working_dir: None,
                inherit_env: false,
                allowed_env_vars: Vec::new(),
                max_output_bytes: 1024,
                commands: Vec::new(),
            });
        }
        let mut router = opencrab_llm::router::LlmRouter::new();
        router.add_provider(Arc::new(CapturingStub { seen }));
        state.llm_router.swap(router);
        state
    }

    /// #333 の本丸: sub-engine の**実行 caller** が親ターンの caller を継承すること、
    /// および `spawn_subtask` 経由の迂回が閉じること。
    ///
    /// sub-run が LLM へ提示するツール一覧を観測する。`execute_shell` / `ws_read` は
    /// #330 で owner_only なので、提示されていれば sub-run の実行 caller は Owner、
    /// 提示されていなければ Agent。
    /// - **親 Owner → サブ Owner**: `execute_shell` / `ws_read` が見える（実装作業が死なない）。
    /// - **親 Agent（外部由来ターン相当）→ サブ Agent**: どちらも消える
    ///   （`spawn_subtask` を挟んでローカル操作へ昇格する迂回路の封鎖）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sub_engine_inherits_parent_caller_and_closes_spawn_bypass() {
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let state = state_with_shell_and_capture(seen.clone());
        let reg = registry();
        let actions = SystemGatewayActions::new(state.clone(), None, Some(reg.clone()), None);

        // --- 親 Owner ---
        let owner_ctx =
            GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("sub-owner-1");
        let res = actions
            .execute("spawn_subtask", &json!({ "task": "t" }), &owner_ctx)
            .await;
        assert!(res.success, "spawn(owner): {:?}", res.error);
        assert!(
            wait_until(|| !seen.lock().unwrap().is_empty()).await,
            "親 Owner の sub-run が LLM を呼ばない"
        );
        let owner_tools = seen.lock().unwrap().last().unwrap().clone();
        assert!(
            owner_tools.iter().any(|t| t == "execute_shell"),
            "親 Owner のサブ run に execute_shell が出ない（継承されていない / #333）: {owner_tools:?}"
        );
        assert!(
            owner_tools.iter().any(|t| t == "ws_read"),
            "親 Owner のサブ run に ws_read が出ない（#333）: {owner_tools:?}"
        );

        // 観測を混ぜないよう、次の spawn の前に走行中サブを止めて登録簿を空にする。
        let owner_calls = seen.lock().unwrap().len();
        for id in reg.iter().map(|e| e.key().clone()).collect::<Vec<_>>() {
            if let Some(e) = reg.get(&id) {
                e.abort_handle.abort();
            }
        }
        reg.clear();

        // --- 親 Agent（外部由来ターン相当）---
        let agent_ctx =
            GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("sub-agent-1");
        let res2 = actions
            .execute("spawn_subtask", &json!({ "task": "t" }), &agent_ctx)
            .await;
        assert!(res2.success, "spawn(agent): {:?}", res2.error);
        assert!(
            wait_until(|| seen.lock().unwrap().len() > owner_calls).await,
            "親 Agent の sub-run が LLM を呼ばない"
        );
        let agent_tools = seen.lock().unwrap().last().unwrap().clone();
        assert!(
            !agent_tools.iter().any(|t| t == "execute_shell"),
            "外部 Agent 親のサブ run に execute_shell が出た = spawn_subtask 迂回が開いている（#333）: {agent_tools:?}"
        );
        assert!(
            !agent_tools.iter().any(|t| t == "ws_read"),
            "外部 Agent 親のサブ run に ws_read が出た（#333）: {agent_tools:?}"
        );

        for id in reg.iter().map(|e| e.key().clone()).collect::<Vec<_>>() {
            if let Some(e) = reg.get(&id) {
                e.abort_handle.abort();
            }
        }
    }

    /// セッション必須ガード（fail-closed）: session_id が無い文脈では起動できない。
    #[tokio::test]
    async fn spawn_subtask_requires_session_context() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, Some(registry()), None);
        let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-x");
        let res = actions
            .execute("spawn_subtask", &json!({ "task": "t" }), &ctx)
            .await;
        assert!(!res.success);
        assert!(res.error.unwrap().contains("セッション"));
    }

    /// `task` は必須。
    #[tokio::test]
    async fn spawn_subtask_requires_task() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, Some(registry()), None);
        let res = actions
            .execute("spawn_subtask", &json!({}), &parent_ctx("web-parent-1"))
            .await;
        assert!(!res.success);
        assert!(res.error.unwrap().contains("'task' argument is required"));
    }

    /// **停止の到達性（fail-closed）**: 登録簿が未配線なら起動しない。走らせてしまうと
    /// `cancel_subtask` から到達できない「見えない走行」になる。
    #[tokio::test]
    async fn spawn_subtask_refuses_without_a_registry() {
        let state = state_with_stub_llm("x", false);
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);
        let res = actions
            .execute(
                "spawn_subtask",
                &json!({ "task": "t" }),
                &parent_ctx("web-parent-1"),
            )
            .await;
        assert!(!res.success, "登録簿が無ければ起動してはならない");
        assert!(res.error.unwrap().contains("登録簿"));
        assert!(!has_log_of_type(
            &state.db,
            "web-parent-1",
            "subtask_spawned"
        ));
    }

    /// 通知先の解決に失敗したら spawn しない（fail-closed）。親ログも汚さない。
    #[tokio::test]
    async fn spawn_subtask_is_not_started_when_notify_target_fails() {
        struct FailingFactory;
        impl opencrab_actions::subtask_notify::SubtaskLifecycleNotifier for FailingFactory {
            fn begin_run(
                &self,
                _run: &SubtaskRunInfo<'_>,
            ) -> Result<
                opencrab_actions::subtask_notify::SubtaskNotifySession,
                opencrab_actions::subtask_notify::NotifyTargetError,
            > {
                Err(opencrab_actions::subtask_notify::NotifyTargetError {
                    code: "invalid_webhook_url".to_string(),
                    message: "url must start with https://".to_string(),
                    source: "explicit",
                })
            }
        }

        let state = state_with_stub_llm("x", false);
        *state.subtask_lifecycle_notifier.lock().unwrap() = Some(Arc::new(FailingFactory));
        let reg = registry();
        let actions = SystemGatewayActions::new(state.clone(), None, Some(reg.clone()), None);

        let res = actions
            .execute(
                "spawn_subtask",
                &json!({ "task": "t" }),
                &parent_ctx("web-parent-1"),
            )
            .await;
        assert!(!res.success);
        assert!(res.error.unwrap().contains("invalid_webhook_url"));
        assert!(reg.is_empty(), "起動していないので登録簿は空");
        assert!(!has_log_of_type(
            &state.db,
            "web-parent-1",
            "subtask_spawned"
        ));
    }
}
