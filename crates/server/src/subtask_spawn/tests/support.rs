use std::sync::{Arc, Mutex};

use opencrab_actions::subtask::{
    SettleKind, SubtaskCompletionSink, SubtaskRegistry, SubtaskSettled,
};
use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};

use crate::AppState;

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
    // #676: この stub は max_tokens を無視するので「送らない」を宣言し、出力上限の
    // モデル登録（fail loud）の対象外にする。subtask 生成の検証に集中させる（上限の
    // 解決/ゲートは context_budget / skill_engine の専用テストで担保）。
    fn sends_max_output_tokens(&self) -> bool {
        false
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
    // #676: stub は max_tokens を無視するので「送らない」を宣言（上記 StubProvider と同じ理由）。
    fn sends_max_output_tokens(&self) -> bool {
        false
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
    // #676: stub は max_tokens を無視するので「送らない」を宣言（上記 StubProvider と同じ理由）。
    fn sends_max_output_tokens(&self) -> bool {
        false
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
    opencrab_db::queries::upsert_model_pricing(
        &conn,
        &opencrab_db::queries::ModelPricingRow {
            provider: "mock".to_string(),
            model: "test".to_string(),
            input_price_per_1m: 0.0,
            output_price_per_1m: 0.0,
            context_window: Some(200_000),
            max_output_tokens: Some(4_096),
        },
    )
    .expect("test model_pricing for envelope");
}

/// `mock:test` を解決できる `AppState`（Discord を一切通さない = web / REST 相当）。
pub(super) fn state_with_stub_llm(reply: &str, hang: bool) -> AppState {
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
pub(super) fn state_with_gated_llm(
    reply: &str,
    gate: tokio::sync::watch::Receiver<bool>,
) -> AppState {
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
pub(super) struct OrderCheckingSink {
    db: opencrab_db::Db,
    /// (kind, exit_reason, 通知時点で完了ログが DB にあったか)
    seen: Mutex<Vec<(SettleKind, String, bool)>>,
}

impl OrderCheckingSink {
    pub(super) fn new(db: opencrab_db::Db) -> Self {
        Self {
            db,
            seen: Mutex::new(Vec::new()),
        }
    }
    pub(super) fn seen(&self) -> Vec<(SettleKind, String, bool)> {
        self.seen.lock().unwrap().clone()
    }
}

impl SubtaskCompletionSink for OrderCheckingSink {
    fn session_prefix(&self) -> &'static str {
        ""
    }
    fn forwards_progress(&self) -> bool {
        true
    }
    fn deliver_continuation(&self, ev: SubtaskSettled) {
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
pub(super) fn has_log_of_type(db: &opencrab_db::Db, session_id: &str, log_type: &str) -> bool {
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
pub(super) fn completed_result(db: &opencrab_db::Db, session_id: &str) -> Option<String> {
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

pub(super) fn registry() -> SubtaskRegistry {
    Arc::new(dashmap::DashMap::new())
}

pub(super) fn parent_ctx(session_id: &str) -> GatewayCallContext {
    GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id(session_id)
}

/// spawn 直後に返る subtask_id。
pub(super) fn spawned_id(res: &GatewayActionResult) -> String {
    res.data.as_ref().unwrap()["subtask_id"]
        .as_str()
        .unwrap()
        .to_string()
}

pub(super) async fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..400 {
        if cond() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    cond()
}

/// shell を有効化した `AppState` に capturing stub を挿す。
pub(super) fn state_with_shell_and_capture(seen: Arc<Mutex<Vec<Vec<String>>>>) -> AppState {
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
