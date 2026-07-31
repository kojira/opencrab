use std::sync::Arc;

use anyhow::Result;
use tracing;

use super::types::{
    ActionExecutor, ActionResult, ChatRequest, EngineResult, LlmCallLog, LlmClient, ToolDispatcher,
};
use super::xml_parser::{parse_xml_tool_calls, strip_function_calls_xml};
use opencrab_llm_types::{ContentPart, ImageUrl, Message, MessageContent, Role, ToolCall};

// ---------------------------------------------------------------------------
// SkillEngine
// ---------------------------------------------------------------------------

/// The LLM-driven action loop engine.
///
/// The SkillEngine orchestrates the cycle of:
/// 1. Building context from the agent's state
/// 2. Getting available tools from the action executor
/// 3. Calling the LLM with function calling enabled
/// 4. Executing any requested tool calls
/// 5. Feeding results back and repeating
///
/// This continues until the LLM produces a final text response
/// or the maximum iteration count is reached.
pub struct SkillEngine {
    /// The LLM client for chat completion.
    llm: Box<dyn LlmClient>,
    /// The action executor for tool calls.
    executor: Box<dyn ActionExecutor>,
    /// Maximum number of LLM call iterations before stopping.
    pub max_iterations: usize,
    /// Set of actions declared by active skills. If Some, only declared actions are allowed.
    pub allowed_actions: Option<std::collections::HashSet<String>>,
    /// Optional callback invoked after each LLM call for logging.
    pub log_callback: Option<Box<dyn Fn(&LlmCallLog) + Send + Sync>>,
    /// Optional callback invoked with response text on every LLM reply.
    pub on_response_text: Option<Arc<dyn Fn(String) + Send + Sync>>,
    /// Optional callback invoked when the assistant produces tool calls: (assistant_content, tool_calls_json).
    on_tool_call: Option<Arc<dyn Fn(String, String) + Send + Sync>>,
    /// Optional callback invoked when a tool result is received: (tool_call_id, tool_name, result_json, is_error).
    on_tool_result: Option<Arc<dyn Fn(String, String, String, bool) + Send + Sync>>,
    /// Per-run reasoning (thinking) effort. Attached to every ChatRequest so
    /// providers can override their construction-time default per agent.
    reasoning_effort: Option<String>,
    /// 本文中の URL をプロバイダのネイティブ機能で読ませるか（エージェント単位の
    /// オプトイン）。true なら各 ChatRequest の metadata に `web_search: true` を
    /// 載せ、対応プロバイダ（chatgpt=web_search / google=url_context）がツールを
    /// 有効化する。非対応プロバイダは単に無視する。
    web_search: bool,
    /// 自動 dispatch フック（RFC #152 S3a）。Some のとき、`should_dispatch` が真の
    /// ツールは inline 実行せず background subtask 化し、**同ターンで**
    /// `{status:"spawned", ...}` を tool_result として返す。engine 外（executor 経由の
    /// 合成 runtime）から注入する。None なら従来どおり全ツールを inline 実行する。
    tool_dispatcher: Option<Arc<dyn ToolDispatcher>>,
    /// 上限超過の tool_result を退避する先（#284）。未設定でも上限自体は効く
    /// （退避できないぶん、案内付きで切り詰めるだけ）。
    tool_result_offload: Option<ToolResultOffload>,
}

/// LLM へ返す tool_result の退避先設定（#284）。
struct ToolResultOffload {
    /// 退避ファイル名に使うセッション ID。
    session_id: String,
    /// エージェントのワークスペース root。`<root>/tmp/` へ全文を書き出す。
    workspace_root: Option<std::path::PathBuf>,
}

impl SkillEngine {
    /// Create a new SkillEngine.
    pub fn new(
        llm: Box<dyn LlmClient>,
        executor: Box<dyn ActionExecutor>,
        max_iterations: usize,
    ) -> Self {
        Self {
            llm,
            executor,
            max_iterations,
            allowed_actions: None,
            log_callback: None,
            on_response_text: None,
            on_tool_call: None,
            on_tool_result: None,
            reasoning_effort: None,
            web_search: false,
            tool_dispatcher: None,
            tool_result_offload: None,
        }
    }

    /// 上限超過 tool_result の退避先を設定する（#284）。
    ///
    /// 設定しなくても [`TOOL_RESULT_SIZE_LIMIT`] は効く（退避せず切り詰める）が、
    /// 設定すると全文が `<workspace_root>/tmp/` に残り、エージェントが
    /// `read_file` / `execute_shell` で続きを読めるようになる。
    ///
    /// [`TOOL_RESULT_SIZE_LIMIT`]: crate::tool_result_log::TOOL_RESULT_SIZE_LIMIT
    pub fn set_tool_result_offload(
        &mut self,
        session_id: impl Into<String>,
        workspace_root: Option<std::path::PathBuf>,
    ) {
        self.tool_result_offload = Some(ToolResultOffload {
            session_id: session_id.into(),
            workspace_root,
        });
    }

    /// LLM へ返す直前の tool_result にサイズ上限を効かせる（#284）。
    ///
    /// **これを通さずに `Message::tool` へ積んではいけない。** 素通りさせると
    /// 1 件の巨大な結果（実例: 76,661 バイトのフォロー一覧）がプロンプトを占有し、
    /// 同ターンのユーザー発言が 1 件も載らなくなる。
    /// 永続化側（`on_tool_result` → `sanitize_tool_result_for_log`）と同じ上限・
    /// 同じ退避先を使うので、同ターンで見える本文と次ターンに再注入される本文が
    /// 一致する。
    fn cap_tool_result(&self, tool_name: &str, tool_call_id: &str, result_json: String) -> String {
        if result_json.len() < crate::tool_result_log::TOOL_RESULT_SIZE_LIMIT {
            return result_json;
        }
        let (session_id, workspace_root) = match &self.tool_result_offload {
            Some(o) => (o.session_id.as_str(), o.workspace_root.as_deref()),
            // 退避先未設定（sub-engine / テスト）でも上限は必ず効かせる。
            None => ("session", None),
        };
        let capped = crate::tool_result_log::sanitize_tool_result_for_llm(
            tool_name,
            &result_json,
            session_id,
            tool_call_id,
            workspace_root,
        );
        tracing::warn!(
            tool = %tool_name,
            original_bytes = result_json.len(),
            capped_bytes = capped.len(),
            "tool result exceeded the inline size limit; truncated before sending to the LLM"
        );
        capped
    }

    /// 自動 dispatch フックを注入する（RFC #152 S3a）。以後、`should_dispatch` が真の
    /// ツール呼び出しは inline 実行されず background subtask 化され、同ターンで
    /// spawned マーカーが tool_result として返る。
    pub fn set_tool_dispatcher(&mut self, dispatcher: Arc<dyn ToolDispatcher>) {
        self.tool_dispatcher = Some(dispatcher);
    }

    /// Set the per-run reasoning (thinking) effort attached to each request.
    /// 空文字は「未設定」として扱う。
    pub fn set_reasoning_effort(&mut self, effort: impl Into<String>) {
        let s = effort.into();
        self.reasoning_effort = if s.trim().is_empty() { None } else { Some(s) };
    }

    /// 本文URL読取り（プロバイダネイティブの web_search / url_context）を有効化する。
    pub fn set_web_search(&mut self, enabled: bool) {
        self.web_search = enabled;
    }

    /// Set the LLM log callback, invoked after each LLM call.
    pub fn set_log_callback(&mut self, cb: impl Fn(&LlmCallLog) + Send + Sync + 'static) {
        self.log_callback = Some(Box::new(cb));
    }

    /// Set the on_response_text callback, invoked with response text on every LLM reply.
    pub fn set_on_response_text(&mut self, cb: impl Fn(String) + Send + Sync + 'static) {
        self.on_response_text = Some(Arc::new(cb));
    }

    /// Set the on_tool_call callback, invoked when the assistant produces tool calls.
    pub fn set_on_tool_call(&mut self, cb: impl Fn(String, String) + Send + Sync + 'static) {
        self.on_tool_call = Some(Arc::new(cb));
    }

    /// Set the on_tool_result callback, invoked when a tool result is received.
    pub fn set_on_tool_result(
        &mut self,
        cb: impl Fn(String, String, String, bool) + Send + Sync + 'static,
    ) {
        self.on_tool_result = Some(Arc::new(cb));
    }

    /// Set the allowed actions from active skill declarations.
    pub fn set_allowed_actions(&mut self, actions: impl IntoIterator<Item = String>) {
        self.allowed_actions = Some(actions.into_iter().collect());
    }

    /// Check if an action is allowed by the active skill declarations.
    fn is_action_allowed(&self, action_name: &str) -> bool {
        match &self.allowed_actions {
            None => true,
            Some(allowed) => allowed.contains(action_name),
        }
    }

    /// Build an ActionResult for a permission denied error.
    fn permission_denied(action_name: &str) -> ActionResult {
        ActionResult {
            success: false,
            data: serde_json::json!(null),
            error: Some(format!(
                "Action '{}' is not authorized. Add '{}' to the skill's actions frontmatter to enable this capability.",
                action_name, action_name
            )),
        }
    }

    /// Run the action loop with the given system context and user message.
    ///
    /// Returns the final text response from the LLM after all tool calls
    /// have been resolved.
    pub async fn run(
        &self,
        system_context: &str,
        user_message: &str,
        model: &str,
    ) -> Result<EngineResult> {
        self.run_with_model_override(system_context, user_message, model, None, &[])
            .await
    }

    /// Run the action loop with optional dynamic model override.
    ///
    /// If `model_override` is provided, the engine checks it before each LLM call
    /// and uses the overridden model if set (e.g., by `select_llm` action).
    pub async fn run_with_model_override(
        &self,
        system_context: &str,
        user_message: &str,
        default_model: &str,
        model_override: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
        image_urls: &[String],
    ) -> Result<EngineResult> {
        // プロンプトキャッシュはプロバイダの能力としてプロバイダ側が適用する（#44）。
        // 以前はここで Anthropic 固有の cache_control を全リクエストに無条件付与して
        // いたが、読むのは anthropic だけ・system 分は黙って落ちる偽ユニバーサル
        // 抽象だった。エンジンはプロバイダ非依存のリクエストだけを組む。
        let tools = self.executor.list_tools();

        // ユーザーメッセージ本文（画像があればマルチパート）。
        let user_content = if image_urls.is_empty() {
            MessageContent::Text(user_message.to_string())
        } else {
            let mut parts = vec![ContentPart::Text {
                text: user_message.to_string(),
            }];
            for url in image_urls {
                parts.push(ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: url.clone(),
                        detail: Some("auto".to_string()),
                    },
                });
            }
            MessageContent::Multi(parts)
        };

        let mut messages = vec![
            Message {
                role: Role::System,
                content: Some(MessageContent::Text(system_context.to_string())),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: Some(user_content),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ];

        let mut iterations = 0;
        let mut total_tool_calls = 0;
        let mut xml_fallback_parses = 0;

        loop {
            iterations += 1;

            if iterations > self.max_iterations {
                tracing::warn!(
                    iterations = iterations,
                    max = self.max_iterations,
                    "SkillEngine reached max iterations, stopping"
                );
                return Ok(EngineResult {
                    response: "I've reached the maximum number of steps for this task. Here's what I've done so far.".to_string(),
                    iterations,
                    tool_calls_made: total_tool_calls,
                    stopped_by_limit: true,
                    xml_fallback_parses,
                });
            }

            // Check for dynamic model override.
            let model = model_override
                .as_ref()
                .and_then(|o| o.lock().ok().and_then(|m| m.clone()))
                .unwrap_or_else(|| default_model.to_string());

            tracing::debug!(iteration = iterations, model = %model, "SkillEngine LLM call");

            let request = ChatRequest {
                model: model.clone(),
                messages: messages.clone(),
                functions: if tools.is_empty() {
                    None
                } else {
                    Some(tools.clone())
                },
                function_call: None,
                temperature: Some(0.7),
                max_tokens: Some(4096),
                stop: None,
                stream: None,
                metadata: {
                    let mut m: std::collections::HashMap<String, serde_json::Value> =
                        Default::default();
                    if self.web_search {
                        m.insert("web_search".to_string(), serde_json::json!(true));
                    }
                    m
                },
                agent_id: None,
                reasoning_effort: self.reasoning_effort.clone(),
            };

            let request_for_log = request.clone();
            let requested_at =
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            let call_start = std::time::Instant::now();
            let llm_result = self.llm.chat(request).await;
            let latency_ms = call_start.elapsed().as_millis() as i64;

            if let Some(cb) = &self.log_callback {
                match &llm_result {
                    Ok(resp) => cb(&LlmCallLog {
                        request: request_for_log.clone(),
                        response: Some(resp.clone()),
                        error_str: None,
                        latency_ms,
                        requested_at: requested_at.clone(),
                        is_bot_iteration: iterations > 1,
                    }),
                    Err(e) => cb(&LlmCallLog {
                        request: request_for_log.clone(),
                        response: None,
                        error_str: Some(e.to_string()),
                        latency_ms,
                        requested_at: requested_at.clone(),
                        is_bot_iteration: iterations > 1,
                    }),
                }
            }

            let response = llm_result?;

            // 応答本文とツールコールをローカルに抽出（正準モデルは choices[0] を持つ）。
            let mut content: Option<String> = response.first_text().map(|s| s.to_string());
            let mut tool_calls: Vec<ToolCall> = response
                .first_message()
                .and_then(|m| m.tool_calls.clone())
                .unwrap_or_default();

            // If the LLM returned no structured tool calls but embedded
            // <function_calls> XML in the content (e.g. DeepSeek via OpenRouter),
            // parse them out and treat them as normal tool calls.
            if tool_calls.is_empty() {
                if let Some(ref c) = content {
                    if c.contains("<function_calls>") {
                        let parsed = parse_xml_tool_calls(c);
                        if !parsed.is_empty() {
                            // 発火は harness 剪定の判断材料として計測する（EngineResult 経由で
                            // agent_logs にも記録される）。codex プロバイダは意図的にこの
                            // フォールバックへ依存するため、発火＝異常ではない（毎イテレーション
                            // 発火し得るのでログは debug に留め、run 単位の集計を agent_logs で見る）。
                            xml_fallback_parses += 1;
                            tracing::debug!(
                                count = parsed.len(),
                                model = %model,
                                "Parsed XML function_calls from content (harness fallback fired)"
                            );
                            tool_calls = parsed;
                            // Strip the XML block from content so it doesn't leak to the user.
                            let cleaned = strip_function_calls_xml(c);
                            content = if cleaned.is_empty() {
                                None
                            } else {
                                Some(cleaned)
                            };
                        }
                    }
                }
            }

            // Fire on_response_text for every LLM reply that has non-empty text.
            if let Some(ref text) = content {
                if !text.trim().is_empty() {
                    if let Some(ref cb) = self.on_response_text {
                        tracing::warn!(
                            iteration = iterations,
                            text_len = text.len(),
                            text_preview = %text.chars().take(100).collect::<String>(),
                            "LLM response text received, firing on_response_text callback"
                        );
                        cb(text.clone());
                        tracing::warn!(iteration = iterations, "on_response_text callback fired");
                    }
                }
            }

            // If there are tool calls, execute them and continue the loop.
            if !tool_calls.is_empty() {
                // Add the assistant message with tool calls (arguments already
                // canonical Strings, so no Value->String conversion needed).
                messages.push(Message {
                    role: Role::Assistant,
                    content: content.clone().map(MessageContent::Text),
                    name: None,
                    function_call: None,
                    tool_calls: Some(tool_calls.clone()),
                    tool_call_id: None,
                });

                // Notify on_tool_call callback.
                if let Some(ref cb) = self.on_tool_call {
                    let calls_json = serde_json::to_string(&tool_calls).unwrap_or_default();
                    cb(content.clone().unwrap_or_default(), calls_json);
                }

                // 自動 dispatch（RFC #152 S3a・非ブロック）のバッチ判定。
                //
                // **バッチ単位**で決める（tool_call 単位ではない）。同一 assistant
                // メッセージのツールは LLM が並べた順に依存し得る
                // （`write_file` → `execute_shell("cargo build")` / `add_allowed_command`
                // → `execute_shell`）ため、
                //  - 全部 dispatch 可 → **1 本の subtask** にまとめて逐次実行（順序保持・
                //    完了通知も 1 回 = 親の resume も 1 回）。
                //  - 1 つでも dispatch 不可（配送系・制御系・共有状態を書くツール）や
                //    未許可ツールが混ざる → **バッチ全体を inline 実行**（従来経路）。
                //    混在バッチを分割すると inline と background の相対順序が保証できない。
                let dispatch_whole_batch = match &self.tool_dispatcher {
                    Some(d) => tool_calls.iter().all(|tc| {
                        self.is_action_allowed(&tc.function.name)
                            && d.should_dispatch(&tc.function.name)
                    }),
                    None => false,
                };

                if dispatch_whole_batch {
                    let dispatcher = self.tool_dispatcher.as_ref().expect("checked above");
                    let calls: Vec<super::types::DispatchCall> = tool_calls
                        .iter()
                        .map(|tc| super::types::DispatchCall {
                            tool_name: tc.function.name.clone(),
                            args: tc.arguments_json(),
                            tool_call_id: tc.id.clone(),
                        })
                        .collect();
                    total_tool_calls += calls.len();
                    let outcome = dispatcher.dispatch_batch(&calls);
                    tracing::debug!(
                        tools = calls.len(),
                        subtask_id = %outcome.subtask_id,
                        "tool batch auto-dispatched as a single background subtask"
                    );
                    for tool_call in &tool_calls {
                        let spawned = serde_json::json!({
                            "status": "spawned",
                            "subtask_id": outcome.subtask_id,
                            "tool": tool_call.function.name,
                            "label": outcome.label,
                        });
                        let result_json = serde_json::to_string(&spawned)
                            .unwrap_or_else(|_| r#"{"status":"spawned"}"#.to_string());
                        messages.push(Message::tool(tool_call.id.clone(), result_json.clone()));
                        if let Some(ref cb) = self.on_tool_result {
                            cb(
                                tool_call.id.clone(),
                                tool_call.function.name.clone(),
                                result_json,
                                false,
                            );
                        }
                    }
                    continue;
                }

                for tool_call in &tool_calls {
                    total_tool_calls += 1;
                    let tool_name = &tool_call.function.name;

                    tracing::debug!(
                        tool = %tool_name,
                        id = %tool_call.id,
                        "Executing tool call"
                    );

                    // Check if the action is declared by active skills.
                    if !self.is_action_allowed(tool_name) {
                        let denied = Self::permission_denied(tool_name);
                        let result_json = serde_json::to_string(&denied)
                            .unwrap_or_else(|_| r#"{"error": "Permission denied"}"#.to_string());
                        messages.push(Message::tool(tool_call.id.clone(), result_json.clone()));

                        // Notify on_tool_result callback for denied action.
                        if let Some(ref cb) = self.on_tool_result {
                            cb(
                                tool_call.id.clone(),
                                tool_name.clone(),
                                result_json.clone(),
                                true,
                            );
                        }
                        continue;
                    }

                    // Canonical tool-call arguments are a JSON string; parse to a
                    // Value for the executor boundary (empty object on malformed).
                    let args = tool_call.arguments_json();

                    // ここに来るのは「バッチ全体を inline 実行する」経路のみ
                    // （dispatch 判定はバッチ単位でループ前に済んでいる）。
                    let result = self
                        .executor
                        .execute_with_id(tool_name, &args, &tool_call.id)
                        .await;

                    let result_json = serde_json::to_string(&result).unwrap_or_else(|_| {
                        r#"{"error": "Failed to serialize result"}"#.to_string()
                    });

                    // #284: LLM へ返す前に上限を効かせる。以降（messages / callback）は
                    // すべてこの capped 本文を使い、同ターンのプロンプトと DB に残る
                    // 本文を一致させる。
                    let result_json = self.cap_tool_result(tool_name, &tool_call.id, result_json);

                    messages.push(Message::tool(tool_call.id.clone(), result_json.clone()));

                    // Notify on_tool_result callback.
                    if let Some(ref cb) = self.on_tool_result {
                        cb(
                            tool_call.id.clone(),
                            tool_name.clone(),
                            result_json.clone(),
                            !result.success,
                        );
                    }
                }

                continue;
            }

            // No tool calls: this is the final response.
            let final_text = content.unwrap_or_default();

            tracing::warn!(
                iteration = iterations,
                text_len = final_text.len(),
                text_preview = %final_text.chars().take(100).collect::<String>(),
                "SkillEngine final response ready"
            );

            if final_text.is_empty() {
                tracing::debug!("LLM returned empty content (no tool calls), using empty response");
            }

            return Ok(EngineResult {
                response: final_text,
                iterations,
                tool_calls_made: total_tool_calls,
                stopped_by_limit: false,
                xml_fallback_parses,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use opencrab_llm_types::{
        ChatResponse, Choice, FunctionCall, FunctionDefinition, MessageContent, Usage,
    };
    use serde_json::Value;

    /// Build a canonical tool call with JSON arguments (as a value, serialized).
    fn tc(id: &str, name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: serde_json::to_string(&args).unwrap(),
            },
        }
    }

    /// Build a single-choice ChatResponse with optional text and tool calls.
    fn resp(text: Option<&str>, calls: Vec<ToolCall>) -> ChatResponse {
        ChatResponse {
            id: String::new(),
            model: String::new(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: text.map(|s| MessageContent::Text(s.to_string())),
                    name: None,
                    function_call: None,
                    tool_calls: if calls.is_empty() { None } else { Some(calls) },
                    tool_call_id: None,
                },
                finish_reason: None,
            }],
            usage: Usage::default(),
            created: 0,
        }
    }

    fn text_response(text: &str) -> ChatResponse {
        resp(Some(text), vec![])
    }

    fn tool_call_response(calls: Vec<ToolCall>) -> ChatResponse {
        resp(None, calls)
    }

    struct MockLlm {
        responses: std::sync::Mutex<Vec<ChatResponse>>,
    }

    impl MockLlm {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn chat(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                anyhow::bail!("no more mock responses");
            }
            Ok(responses.remove(0))
        }
    }

    struct MockExecutor {
        results: std::collections::HashMap<String, ActionResult>,
    }

    impl MockExecutor {
        fn new() -> Self {
            Self {
                results: std::collections::HashMap::new(),
            }
        }
        fn add_result(mut self, name: &str, result: ActionResult) -> Self {
            self.results.insert(name.to_string(), result);
            self
        }
    }

    #[async_trait]
    impl ActionExecutor for MockExecutor {
        async fn execute(&self, name: &str, _args: &Value) -> ActionResult {
            self.results.get(name).cloned().unwrap_or(ActionResult {
                success: false,
                data: serde_json::json!(null),
                error: Some(format!("Unknown action: {name}")),
            })
        }
        fn list_tools(&self) -> Vec<FunctionDefinition> {
            vec![FunctionDefinition {
                name: "test_tool".to_string(),
                description: Some("A test tool".to_string()),
                parameters: serde_json::json!({}),
            }]
        }
    }

    #[tokio::test]
    async fn test_direct_response() {
        let llm = MockLlm::new(vec![text_response("Hello, world!")]);
        let executor = MockExecutor::new();
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine.run("system", "hi", "test-model").await.unwrap();
        assert_eq!(result.response, "Hello, world!");
        assert_eq!(result.iterations, 1);
        assert_eq!(result.tool_calls_made, 0);
        assert!(!result.stopped_by_limit);
    }

    #[tokio::test]
    async fn test_single_tool_call() {
        let llm = MockLlm::new(vec![
            tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
            text_response("Done with tool call"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"result": "ok"}),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine
            .run("system", "do something", "test-model")
            .await
            .unwrap();
        assert_eq!(result.iterations, 2);
        assert_eq!(result.tool_calls_made, 1);
        assert!(!result.stopped_by_limit);
    }

    #[tokio::test]
    async fn test_max_iterations() {
        let llm = MockLlm::new(vec![
            tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
            tool_call_response(vec![tc("tc-2", "test_tool", serde_json::json!({}))]),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!(null),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 1);

        let result = engine
            .run("system", "loop forever", "test-model")
            .await
            .unwrap();
        assert!(result.stopped_by_limit);
    }

    #[tokio::test]
    async fn test_multiple_tool_calls() {
        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                tc("tc-1", "test_tool", serde_json::json!({})),
                tc("tc-2", "test_tool", serde_json::json!({})),
            ]),
            text_response("Both tools done"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!(null),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine
            .run("system", "do two things", "test-model")
            .await
            .unwrap();
        assert_eq!(result.tool_calls_made, 2);
        assert_eq!(result.iterations, 2);
        assert!(!result.stopped_by_limit);
    }

    #[tokio::test]
    async fn test_tool_result_feedback() {
        let llm = MockLlm::new(vec![
            tool_call_response(vec![tc(
                "tc-1",
                "test_tool",
                serde_json::json!({"query": "test"}),
            )]),
            text_response("Received tool feedback"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"answer": 42}),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine
            .run("system", "query something", "test-model")
            .await
            .unwrap();
        assert_eq!(result.response, "Received tool feedback");
        assert_eq!(result.iterations, 2);
        assert_eq!(result.tool_calls_made, 1);
        assert!(!result.stopped_by_limit);
    }

    #[tokio::test]
    async fn test_model_override() {
        use std::sync::{Arc, Mutex};

        // MockLlm that captures the model from each request.
        struct ModelCapturingLlm {
            responses: Mutex<Vec<ChatResponse>>,
            captured_models: Arc<Mutex<Vec<String>>>,
        }

        #[async_trait]
        impl LlmClient for ModelCapturingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                self.captured_models
                    .lock()
                    .unwrap()
                    .push(request.model.clone());
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    anyhow::bail!("no more mock responses");
                }
                Ok(responses.remove(0))
            }
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let llm = ModelCapturingLlm {
            responses: Mutex::new(vec![
                // First call uses default model; after tool call, model override kicks in.
                tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
                text_response("Done after model switch"),
            ]),
            captured_models: captured.clone(),
        };

        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"ok": true}),
                error: None,
            },
        );

        let model_override = Arc::new(Mutex::new(None));
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        // Simulate: after the first tool call, model_override gets set.
        let override_clone = model_override.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            *override_clone.lock().unwrap() = Some("openai:gpt-4o-mini".to_string());
        });

        let result = engine
            .run_with_model_override("system", "hi", "default-model", Some(model_override), &[])
            .await
            .unwrap();

        assert_eq!(result.response, "Done after model switch");

        let models = captured.lock().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0], "default-model"); // First call uses default.
                                                // Second call should use the overridden model (race condition safe - set before tool call finishes).
                                                // Due to timing, it might be either; the important thing is the mechanism works.
    }

    #[tokio::test]
    async fn test_on_response_text_fires_on_every_iteration() {
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![
            resp(
                Some("調べてみます"),
                vec![tc("tc-1", "test_tool", serde_json::json!({}))],
            ),
            resp(Some("天気は20度です"), vec![]),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!(null),
                error: None,
            },
        );

        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        let fired_texts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let fired_clone = fired_texts.clone();
        engine.set_on_response_text(move |text: String| {
            fired_clone.lock().unwrap().push(text);
        });

        let result = engine
            .run("system", "天気は？", "test-model")
            .await
            .unwrap();
        let texts = fired_texts.lock().unwrap();
        assert_eq!(texts.len(), 2, "should fire for both iterations");
        assert_eq!(texts[0], "調べてみます");
        assert_eq!(texts[1], "天気は20度です");
        assert_eq!(result.response, "天気は20度です");
    }

    #[tokio::test]
    async fn test_tool_history_in_next_llm_call() {
        use std::sync::{Arc, Mutex};

        // MockLlm that captures the messages from each request
        struct MessageCapturingLlm {
            responses: Mutex<Vec<ChatResponse>>,
            captured_messages: Arc<Mutex<Vec<Vec<Message>>>>,
        }

        #[async_trait]
        impl LlmClient for MessageCapturingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                self.captured_messages
                    .lock()
                    .unwrap()
                    .push(request.messages.clone());
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    anyhow::bail!("no more mock responses");
                }
                Ok(responses.remove(0))
            }
        }

        let captured = Arc::new(Mutex::new(Vec::<Vec<Message>>::new()));
        let llm = MessageCapturingLlm {
            responses: Mutex::new(vec![
                // First response: tool call
                tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
                // Second response: final text
                text_response("All done"),
            ]),
            captured_messages: captured.clone(),
        };

        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"result": "ok"}),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine.run("system", "do it", "test-model").await.unwrap();
        assert_eq!(result.response, "All done");
        assert_eq!(result.iterations, 2);

        let all_messages = captured.lock().unwrap();
        assert_eq!(all_messages.len(), 2, "LLM should have been called twice");

        // Check messages sent on the second LLM call (iteration 2)
        let second_call_msgs = &all_messages[1];

        // Should contain an assistant message with non-empty tool_calls
        let has_assistant_with_tool_calls = second_call_msgs.iter().any(|m| {
            m.role == Role::Assistant && m.tool_calls.as_ref().map_or(false, |t| !t.is_empty())
        });
        assert!(
            has_assistant_with_tool_calls,
            "Second LLM call must include an assistant message with tool_calls"
        );

        // Should contain a tool message with tool_call_id set
        let has_tool_result = second_call_msgs
            .iter()
            .any(|m| m.role == Role::Tool && m.tool_call_id.is_some());
        assert!(
            has_tool_result,
            "Second LLM call must include a tool result message with tool_call_id"
        );
    }

    #[tokio::test]
    async fn test_on_response_text_fires_for_direct_response() {
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![text_response("直接答えます")]);
        let executor = MockExecutor::new();

        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        let fired_texts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let fired_clone = fired_texts.clone();
        engine.set_on_response_text(move |text: String| {
            fired_clone.lock().unwrap().push(text);
        });

        let result = engine.run("system", "direct", "test-model").await.unwrap();
        let texts = fired_texts.lock().unwrap();
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0], "直接答えます");
        assert_eq!(result.response, "直接答えます");
    }

    // ---- RFC #152 S3a: 自動 dispatch（非ブロック / 全ツール subtask 化） ----

    /// 記録用の最小 `ToolDispatcher`。`should_dispatch` は control 集合以外を真にし、
    /// `dispatch_batch` は inline 実行せずマーカーだけ返す（実処理は起こさない）。
    struct RecordingDispatcher {
        control: std::collections::HashSet<String>,
        /// dispatch されたツール名（バッチごとに 1 エントリ = カンマ連結）。
        dispatched: std::sync::Mutex<Vec<String>>,
        /// `dispatch_batch` の呼び出し回数（= 生成された subtask の本数）。
        batches: std::sync::atomic::AtomicUsize,
    }

    impl RecordingDispatcher {
        fn new(control: &[&str]) -> Self {
            Self {
                control: control.iter().map(|s| s.to_string()).collect(),
                dispatched: std::sync::Mutex::new(Vec::new()),
                batches: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl crate::ToolDispatcher for RecordingDispatcher {
        fn should_dispatch(&self, tool_name: &str) -> bool {
            !self.control.contains(tool_name)
        }
        fn dispatch_batch(&self, calls: &[crate::DispatchCall]) -> crate::DispatchOutcome {
            self.batches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let names: Vec<String> = calls.iter().map(|c| c.tool_name.clone()).collect();
            self.dispatched.lock().unwrap().push(names.join(","));
            crate::DispatchOutcome {
                subtask_id: format!("sub-for-{}", names.join("+")),
                label: names.join(", "),
            }
        }
    }

    /// dispatch 対象ツールは inline 実行（executor）されず、**同ターンで** spawned
    /// マーカーが tool_result として返り、次イテレーションでエージェントが継続すること。
    #[tokio::test]
    async fn test_auto_dispatch_returns_spawned_marker_same_turn() {
        use std::sync::{Arc, Mutex};

        // 1回目: ツール呼び出し（dispatch 対象）。2回目: 最終テキスト。
        let llm = MockLlm::new(vec![
            tool_call_response(vec![tc(
                "tc-1",
                "nostr_generate_key",
                serde_json::json!({}),
            )]),
            text_response("鍵の生成を開始しました"),
        ]);
        // executor が呼ばれたら記録する（dispatch 対象は呼ばれてはならない）。
        struct SpyExecutor {
            called: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl ActionExecutor for SpyExecutor {
            async fn execute(&self, name: &str, _args: &Value) -> ActionResult {
                self.called.lock().unwrap().push(name.to_string());
                ActionResult {
                    success: true,
                    data: serde_json::json!(null),
                    error: None,
                }
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                vec![]
            }
        }
        let called = Arc::new(Mutex::new(Vec::new()));
        let executor = SpyExecutor {
            called: called.clone(),
        };

        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        let dispatcher = Arc::new(RecordingDispatcher::new(&[
            "spawn_subtask",
            "report_progress",
            "cancel_subtask",
        ]));
        engine.set_tool_dispatcher(dispatcher.clone());

        // 2回目の LLM 呼び出しが見る messages を記録し、spawned マーカーの再注入を検証する。
        let seen_tool_results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen_tool_results.clone();
        engine.set_on_tool_result(move |_id, name, json, _err| {
            seen_clone.lock().unwrap().push(format!("{name}:{json}"));
        });

        let result = engine
            .run("system", "鍵を作って", "test-model")
            .await
            .unwrap();

        // dispatch されたので executor は呼ばれない。
        assert!(
            called.lock().unwrap().is_empty(),
            "dispatch 対象ツールは inline executor で実行されてはならない"
        );
        // dispatcher.dispatch が1回呼ばれた。
        assert_eq!(
            dispatcher.dispatched.lock().unwrap().as_slice(),
            &["nostr_generate_key"]
        );
        // tool_result は spawned マーカー（同ターン返却）。
        let seen = seen_tool_results.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("\"status\":\"spawned\""));
        assert!(seen[0].contains("\"subtask_id\":\"sub-for-nostr_generate_key\""));
        // エージェントは自分のターンで継続して最終応答を出す。
        assert_eq!(result.response, "鍵の生成を開始しました");
        assert_eq!(result.iterations, 2);
    }

    /// #284: **巨大なツール結果を生のまま LLM へ返さない。**
    ///
    /// 実事故では 76,661 バイトのフォロー一覧がそのままプロンプトへ積まれ、同ターンの
    /// 会話（ユーザー発言を含む）が押し出された。DB 永続化側には上限があったのに
    /// `messages.push(Message::tool(...))` だけが素通りしていた非対称が原因。
    /// ここでは「LLM が次の呼び出しで実際に見る tool メッセージ」を捕まえて上限内で
    /// あることと、全文の在り処が案内されることを固定する。
    #[tokio::test]
    async fn huge_tool_result_is_capped_before_reaching_the_llm() {
        use std::sync::{Arc, Mutex};

        /// 2 回目の呼び出しで受け取った messages を記録する LLM。
        struct CapturingLlm {
            responses: Mutex<Vec<ChatResponse>>,
            seen_tool_messages: Arc<Mutex<Vec<String>>>,
        }

        #[async_trait]
        impl LlmClient for CapturingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                for m in &request.messages {
                    if m.role == Role::Tool {
                        if let Some(MessageContent::Text(t)) = &m.content {
                            self.seen_tool_messages.lock().unwrap().push(t.clone());
                        }
                    }
                }
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    anyhow::bail!("no more mock responses");
                }
                Ok(responses.remove(0))
            }
        }

        let workspace = tempfile::TempDir::new().unwrap();
        let seen_tool_messages: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(vec![
                tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
                text_response("ok"),
            ]),
            seen_tool_messages: seen_tool_messages.clone(),
        };
        // 事故と同規模の結果を返すツール。
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({ "list": "npub1abcdefgh ".repeat(7_000) }),
                error: None,
            },
        );

        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_result_offload("sess1", Some(workspace.path().to_path_buf()));
        // DB へ渡る本文（callback）も同じ capped 本文であること。
        let logged: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let logged_clone = logged.clone();
        engine.set_on_tool_result(move |_id, _name, json, _err| {
            logged_clone.lock().unwrap().push(json);
        });

        let result = engine
            .run("system", "一覧を見せて", "test-model")
            .await
            .unwrap();
        assert_eq!(result.response, "ok");

        let seen = seen_tool_messages.lock().unwrap();
        let tool_msg = seen
            .first()
            .expect("LLM が tool メッセージを受け取っていない");
        assert!(
            tool_msg.len() <= crate::tool_result_log::TOOL_RESULT_SIZE_LIMIT,
            "LLM へ {} バイトの tool_result が渡っている（上限 {}）",
            tool_msg.len(),
            crate::tool_result_log::TOOL_RESULT_SIZE_LIMIT
        );
        assert!(tool_msg.contains("truncated"), "切り詰めの案内が無い");
        assert!(
            tool_msg.contains("tmp/sess1_tc_1.json"),
            "全文の在り処が案内されていない: {tool_msg}"
        );
        // 全文はワークスペースに残り、エージェントが読める。
        assert!(workspace.path().join("tmp/sess1_tc_1.json").exists());
        // 同ターンで見えた本文と、DB へ渡る本文が一致する（次ターンで内容が変わらない）。
        assert_eq!(
            logged.lock().unwrap().as_slice(),
            std::slice::from_ref(tool_msg)
        );
    }

    /// 退避先が未設定でも上限は効く（sub-engine / 直呼びでも素通りさせない）。
    #[tokio::test]
    async fn tool_result_is_capped_even_without_an_offload_target() {
        use std::sync::{Arc, Mutex};

        struct CapturingLlm {
            responses: Mutex<Vec<ChatResponse>>,
            seen: Arc<Mutex<Vec<String>>>,
        }

        #[async_trait]
        impl LlmClient for CapturingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                for m in &request.messages {
                    if m.role == Role::Tool {
                        if let Some(MessageContent::Text(t)) = &m.content {
                            self.seen.lock().unwrap().push(t.clone());
                        }
                    }
                }
                let mut responses = self.responses.lock().unwrap();
                Ok(responses.remove(0))
            }
        }

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(vec![
                tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
                text_response("ok"),
            ]),
            seen: seen.clone(),
        };
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({ "blob": "z".repeat(100_000) }),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.run("system", "やって", "test-model").await.unwrap();

        let seen = seen.lock().unwrap();
        let tool_msg = seen.first().unwrap();
        assert!(tool_msg.len() <= crate::tool_result_log::TOOL_RESULT_SIZE_LIMIT);
        assert!(tool_msg.contains("could not be saved"));
    }

    /// control 系ツール（report_progress 等）は dispatch されず inline 実行される。
    #[tokio::test]
    async fn test_control_tools_not_dispatched() {
        use std::sync::Arc;

        let llm = MockLlm::new(vec![
            tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
            text_response("done"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"ok": true}),
                error: None,
            },
        );
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        // test_tool を control 扱いにして dispatch させない。
        let dispatcher = Arc::new(RecordingDispatcher::new(&["test_tool"]));
        engine.set_tool_dispatcher(dispatcher.clone());

        let result = engine.run("system", "go", "test-model").await.unwrap();
        // dispatch されず inline 実行された（dispatched は空）。
        assert!(dispatcher.dispatched.lock().unwrap().is_empty());
        assert_eq!(result.tool_calls_made, 1);
        assert_eq!(result.response, "done");
    }

    /// [P0 回帰] 同一ターンに複数ツールが来たとき、tool_call ごとに個別 dispatch せず
    /// **1 本の subtask** にまとめること（順序保持 ＋ 完了通知＝親 resume の 1 回化）。
    #[tokio::test]
    async fn test_multi_tool_batch_dispatched_as_single_subtask() {
        use std::sync::atomic::Ordering as AtomicOrdering;
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                tc("tc-1", "write_file", serde_json::json!({"path": "x"})),
                tc("tc-2", "execute_shell", serde_json::json!({"cmd": "build"})),
            ]),
            text_response("開始しました"),
        ]);
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);
        let dispatcher = Arc::new(RecordingDispatcher::new(&["spawn_subtask"]));
        engine.set_tool_dispatcher(dispatcher.clone());

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        engine.set_on_tool_result(move |_id, _name, json, _err| {
            seen_clone.lock().unwrap().push(json);
        });

        let result = engine.run("system", "go", "test-model").await.unwrap();

        // subtask は 1 本だけ（= settle も sink 発火も 1 回）。
        assert_eq!(
            dispatcher.batches.load(AtomicOrdering::SeqCst),
            1,
            "同一バッチの複数ツールは 1 本の subtask にまとめる"
        );
        // dispatch 順序は LLM が並べた順のまま渡る。
        assert_eq!(
            dispatcher.dispatched.lock().unwrap().as_slice(),
            &["write_file,execute_shell"]
        );
        // tool_call ごとに spawned マーカーは返る（同じ subtask_id）。
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen
            .iter()
            .all(|s| s.contains("\"subtask_id\":\"sub-for-write_file+execute_shell\"")));
        assert_eq!(result.tool_calls_made, 2);
    }

    /// [P0 回帰] dispatch 不可のツールが 1 つでも混ざるバッチは**全体を inline 実行**し、
    /// LLM が並べた順序を保つ（分割すると inline と background の相対順序が崩れる）。
    #[tokio::test]
    async fn test_mixed_batch_falls_back_to_inline_in_order() {
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                tc("tc-1", "write_file", serde_json::json!({"path": "x"})),
                tc("tc-2", "discord_send", serde_json::json!({"text": "hi"})),
            ]),
            text_response("done"),
        ]);
        struct OrderExecutor {
            order: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl ActionExecutor for OrderExecutor {
            async fn execute(&self, name: &str, _args: &Value) -> ActionResult {
                self.order.lock().unwrap().push(name.to_string());
                ActionResult {
                    success: true,
                    data: serde_json::json!(null),
                    error: None,
                }
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                vec![]
            }
        }
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut engine = SkillEngine::new(
            Box::new(llm),
            Box::new(OrderExecutor {
                order: order.clone(),
            }),
            10,
        );
        // discord_send は dispatch 不可（配送系）。
        let dispatcher = Arc::new(RecordingDispatcher::new(&["discord_send"]));
        engine.set_tool_dispatcher(dispatcher.clone());

        engine.run("system", "go", "test-model").await.unwrap();

        assert_eq!(
            dispatcher.dispatched.lock().unwrap().len(),
            0,
            "混在バッチは dispatch せず inline に落とす"
        );
        assert_eq!(
            order.lock().unwrap().as_slice(),
            &["write_file", "discord_send"],
            "inline 実行は LLM が並べた順序を守る"
        );
    }
}
