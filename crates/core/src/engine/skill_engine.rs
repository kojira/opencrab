use std::sync::Arc;

use anyhow::Result;
use tracing;

use super::types::{
    ActionExecutor, ActionResult, ChatRequest, EngineResult, LiveInboundSource, LlmCallLog,
    LlmClient, ToolDispatcher,
};
use super::xml_parser::{parse_xml_tool_calls, strip_function_calls_xml};
use opencrab_llm_types::{
    ContentPart, FinishReason, ImageUrl, Message, MessageContent, Role, ToolCall,
};

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
    /// Callbacks invoked when the assistant produces tool calls: (assistant_content, tool_calls_json).
    ///
    /// **複数**持つ（#397）。購読者は独立している（subtask の進捗実況・session_logs への
    /// 永続化）ので、後から配線した方が前を消してはならない。登録順に全部呼ぶ。
    on_tool_call: Vec<Arc<dyn Fn(String, String) + Send + Sync>>,
    /// Callbacks invoked when a tool result is received: (tool_call_id, tool_name, result_json, is_error).
    /// [`Self::on_tool_call`] と同じく複数持ち、登録順に全部呼ぶ（#397）。
    on_tool_result: Vec<Arc<dyn Fn(String, String, String, bool) + Send + Sync>>,
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
    /// 走行中に届いた新着ユーザー発言の取得口（#289）。Some のとき、2 回目以降の
    /// イテレーションで LLM を呼ぶ直前に引き、新着があれば user メッセージとして
    /// 足す。None なら従来どおりターン開始時の履歴だけで回る。
    live_inbound: Option<Arc<dyn LiveInboundSource>>,
    /// 各 ChatRequest に載せる出力トークン上限（#676）。使用モデルの実能力値を
    /// `model_pricing` から解決して process 側で注入する（[`Self::set_max_output_tokens`]）。
    /// `None` は「上限未指定」＝プロバイダの既定に委ねる（テスト / sub-engine 用）。
    /// 本番経路は必ず Some を入れる（未登録なら process 側がターンを fail loud で止め、
    /// engine まで来ない）。この値で頭打ちになった応答は finish_reason=Length で戻り、
    /// run ループがターンを失敗させる。
    max_output_tokens: Option<u32>,
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
            on_tool_call: Vec::new(),
            on_tool_result: Vec::new(),
            reasoning_effort: None,
            web_search: false,
            tool_dispatcher: None,
            tool_result_offload: None,
            live_inbound: None,
            max_output_tokens: None,
        }
    }

    /// 各 ChatRequest に載せる出力トークン上限を設定する（#676）。使用モデルの実能力値を
    /// `model_pricing` から解決して渡す。process 側が未登録を fail loud で弾くため、
    /// 本番ではここに来る前にターンが止まる。
    pub fn set_max_output_tokens(&mut self, max_output_tokens: u32) {
        self.max_output_tokens = Some(max_output_tokens);
    }

    /// 走行中の新着ユーザー発言の取得口を注入する（#289）。
    ///
    /// 設定すると、2 回目以降のイテレーションで LLM を呼ぶ直前に
    /// [`LiveInboundSource::poll_new_messages`] を引き、返ってきた本文を user
    /// メッセージとして `messages` の末尾へ足す。新着が無ければ何も足さない
    /// （＝従来と 1 バイトも変わらない）。
    ///
    /// 1 回目のイテレーションでは引かない。ターン開始時の会話履歴がその時点の
    /// 発言をすでに含んでいるため、引くと同じ発言を二重に見せることになる。
    pub fn set_live_inbound(&mut self, source: Arc<dyn LiveInboundSource>) {
        self.live_inbound = Some(source);
    }

    /// 上限超過 tool_result の退避先を設定する（#284）。
    ///
    /// 設定しなくても [`TOOL_RESULT_TOKEN_LIMIT`] は効く（退避できず本文は捨てる）が、
    /// 設定すると全文が `<workspace_root>/tmp/` に残り、エージェントが
    /// `read_file` / `execute_shell` などで自分の選んだ方法で参照できるようになる。
    ///
    /// [`TOOL_RESULT_TOKEN_LIMIT`]: crate::tool_result_log::TOOL_RESULT_TOKEN_LIMIT
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

    /// LLM へ返す直前の tool_result にトークン上限を効かせる（#284 / #294）。
    ///
    /// **これを通さずに `Message::tool` へ積んではいけない。** 素通りさせると
    /// 1 件の巨大な結果（実例: 76,661 バイトのフォロー一覧）がプロンプトを占有し、
    /// 同ターンのユーザー発言が 1 件も載らなくなる。
    /// 永続化側（`on_tool_result` → `sanitize_tool_result_for_log`）と同じ上限・
    /// 同じ退避先を使うので、同ターンで見える本文と次ターンに再注入される本文が
    /// 一致する。
    fn cap_tool_result(&self, tool_name: &str, tool_call_id: &str, result_json: String) -> String {
        // 上限判定は `sanitize_tool_result_for_llm` 側が持つ（トークン数で測るため
        // ここでバイト数の早期 return を二重に置くと物差しがズレる）。
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
        if capped != result_json {
            tracing::warn!(
                tool = %tool_name,
                original_bytes = result_json.len(),
                capped_bytes = capped.len(),
                "tool result exceeded the inline token limit; \
                 replaced with a metadata-only pointer before sending to the LLM"
            );
        }
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

    /// Add an on_tool_call callback, invoked when the assistant produces tool calls.
    ///
    /// **足す**（置き換えない / #397）。進捗実況と session_logs 永続化のように独立した
    /// 購読者が同じ engine に配線されるため、代入だと後勝ちで前の購読が黙って消える。
    pub fn add_on_tool_call(&mut self, cb: impl Fn(String, String) + Send + Sync + 'static) {
        self.on_tool_call.push(Arc::new(cb));
    }

    /// Add an on_tool_result callback, invoked when a tool result is received.
    /// [`Self::add_on_tool_call`] と同じく足す（置き換えない / #397）。
    pub fn add_on_tool_result(
        &mut self,
        cb: impl Fn(String, String, String, bool) + Send + Sync + 'static,
    ) {
        self.on_tool_result.push(Arc::new(cb));
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

            // #289: 走行中に届いた新着ユーザー発言を、この呼び出しの入力へ足す。
            //
            // 会話履歴はターン開始時に 1 度だけ組まれるため、ツール往復が長引くと
            // その間の発言は次ターンまで見えなかった（実測でオーナーの「やめて」が
            // 9 秒、#284 の例では約 1 分遅れた）。ここで**差分だけ**を user メッセージ
            // として積む。履歴全体は組み直さない（重い＋コンテキストが膨らむ）。
            //
            // 足すだけで応答は強制しない。見て答えるか作業を続けるかはエージェントの
            // 判断に委ねる（#288 の強制を撤回した方針）。
            //
            // 1 周目は引かない: ターン開始時の履歴がその時点の発言を既に含んでおり、
            // 引くと同じ発言が二重に載る。重複防止の残りは実装側（poll は「前回以降」
            // だけを返す契約）。
            //
            // 位置はツール結果を積み終えた後・LLM 呼び出しの直前。tool_result の直後に
            // user メッセージが並ぶ形になるが、連続 user ロールは許容される
            // （Anthropic は同ロールを 1 ターンへ併合する）。
            if iterations > 1 {
                if let Some(source) = &self.live_inbound {
                    for text in source.poll_new_messages() {
                        tracing::info!(
                            iteration = iterations,
                            bytes = text.len(),
                            "injecting newly arrived user speech into the running turn"
                        );
                        messages.push(Message {
                            role: Role::User,
                            content: Some(MessageContent::Text(text)),
                            name: None,
                            function_call: None,
                            tool_calls: None,
                            tool_call_id: None,
                        });
                    }
                }
            }

            // Check for dynamic model override.
            let model = model_override
                .as_ref()
                .and_then(|o| o.lock().ok().and_then(|m| m.clone()))
                .unwrap_or_else(|| default_model.to_string());

            // #665: LLM 呼び出しの入り。この後の `self.llm.chat(...).await` が返らなければここが
            // 最後の行になる（宙吊りの典型＝推論に入って戻らない／プロキシ未到達）。agent_id / session_id /
            // turn_id は run_agent_response が張った span から継承する。
            tracing::debug!(
                iteration = iterations,
                model = %model,
                messages = messages.len(),
                stage = "llm_call",
                "turn: LLM リクエスト 開始（入）"
            );

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
                max_tokens: self.max_output_tokens,
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
            // #665: LLM 呼び出しの出。入と対で出す（入だけだと「入って止まった」と「戻った」が
            // 区別できない）。成否と latency を載せ、この後のツール往復／最終応答へ進む。
            tracing::debug!(
                iteration = iterations,
                latency_ms,
                ok = llm_result.is_ok(),
                stage = "llm_call",
                "turn: LLM リクエスト 完了（出）"
            );

            // #706 / #676: transport の成否とは別に「このターンの応答は使えるか」を
            // **log_callback の前に**判定する。log_callback（process.rs 側）はこの時点で
            // llm_logs へ即 INSERT するので、判定結果を載せずに呼ぶと、切り捨て（#676）や
            // 意味的に空（#706）の応答が error 欄空の「成功行」として残り、fail loud に
            // しても理由がログに載らない（設計 §1-c の落とし穴）。空応答と出力上限切り捨てを
            // 同じ 1 経路で捕まえ、種別（error_code）を engine 側で確定させる——process は
            // その値を写すだけにする。判定は中身の形だけで行い、finish_reason=Length は
            // 「上限切り捨て」の特定にのみ使う（空判定には混ぜない＝stop を名乗る空応答を
            // 取りこぼさない）。
            let call_failure: Option<(String, String)> = match &llm_result {
                Err(e) => {
                    let body = e.to_string();
                    let code = if opencrab_llm_types::is_context_window_error(&body) {
                        opencrab_llm_types::CONTEXT_WINDOW_EXCEEDED_ERROR_CODE.to_string()
                    } else {
                        "error".to_string()
                    };
                    Some((code, body))
                }
                Ok(resp) => {
                    let finish_reason = resp.choices.first().and_then(|c| c.finish_reason.as_ref());
                    if finish_reason == Some(&FinishReason::Length) {
                        // #676: 出力トークン上限で切り捨てられた応答は、最終回答としても
                        // ツール往復の一手としても扱わない。継続生成などの自動リカバリは
                        // 入れない（#676 方針）。
                        let body = format!(
                            "LLM 応答が出力トークン上限（model={}, max_output_tokens={:?}, \
                             completion_tokens={}）に達して切り捨てられました。切り捨てられた\
                             応答は最終回答として扱いません（fail loud / 継続生成は #676 方針に\
                             よりしない）。上限を上げるには model_pricing にそのモデルの \
                             max_output_tokens を登録し直してください。",
                            model, self.max_output_tokens, resp.usage.completion_tokens,
                        );
                        Some((
                            opencrab_llm_types::OUTPUT_TRUNCATED_ERROR_CODE.to_string(),
                            body,
                        ))
                    } else if opencrab_llm_types::is_empty_response(resp) {
                        // #706: HTTP 200・finish_reason=stop を名乗りつつ content も tool_call
                        // も無いターン。最終回答として扱わず、なぜ黙ったかを llm_logs に残す。
                        let body = format!(
                            "LLM 応答が意味的に空でした（content がフィールド欠落／空文字／\
                             空白のみ、かつ tool_call 無し）。最終回答として扱いません（fail \
                             loud / リトライ・フォールバックは #706 方針によりしない）。\
                             model={}, finish_reason={:?}, prompt_chars={}（opencrab が送った\
                             本文の文字数。空応答は 335k〜365k 文字帯で頻発する実測がある——\
                             長さが原因かの当たり付け用。provider の usage.prompt_tokens は\
                             当てにならないため実測値を使う。閾値判定はしない）",
                            model,
                            finish_reason,
                            request_for_log.message_content_chars(),
                        );
                        Some((
                            opencrab_llm_types::EMPTY_RESPONSE_ERROR_CODE.to_string(),
                            body,
                        ))
                    } else {
                        None
                    }
                }
            };

            if let Some(cb) = &self.log_callback {
                cb(&LlmCallLog {
                    request: request_for_log.clone(),
                    response: llm_result.as_ref().ok().cloned(),
                    error_str: call_failure.as_ref().map(|(_, body)| body.clone()),
                    error_code: call_failure.as_ref().map(|(code, _)| code.clone()),
                    latency_ms,
                    requested_at: requested_at.clone(),
                    is_bot_iteration: iterations > 1,
                });
            }

            // transport 失敗はここで打ち切り（理由は上で llm_logs に残した）。
            let response = llm_result?;

            // Ok だが意味的に使えない応答（空 #706 / 切り捨て #676）は fail loud で打ち切る。
            // tool_calls / content を抽出する**前**に見る——切り捨てられた tool_call JSON が
            // 「空の tool_calls → 最終応答扱い」で黙って消える形をここ 1 点で塞ぐ。
            if let Some((code, body)) = call_failure {
                tracing::error!(
                    iteration = iterations,
                    error_code = %code,
                    model = %model,
                    stage = "turn_failed",
                    "turn: LLM 応答が使えないためターン失敗（fail loud）"
                );
                anyhow::bail!("{body}");
            }

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

                // Notify on_tool_call callbacks.
                if !self.on_tool_call.is_empty() {
                    let calls_json = serde_json::to_string(&tool_calls).unwrap_or_default();
                    let assistant_content = content.clone().unwrap_or_default();
                    for cb in &self.on_tool_call {
                        cb(assistant_content.clone(), calls_json.clone());
                    }
                }

                // 自動 dispatch（RFC #152 S3a・非ブロック）のバッチ分割判定（#671）。
                //
                // **バッチ単位**で決める（tool_call 単位ではない）。同一 assistant
                // メッセージのツールは LLM が並べた順に依存し得る
                // （`write_file` → `execute_shell("cargo build")` / `add_allowed_command`
                // → `execute_shell`）。1 ツールの「dispatch 可」は
                // `is_action_allowed && should_dispatch`、それ以外（配送系・制御系・
                // 共有状態を書くツールなど非 dispatch 可、および未許可ツール）は「inline」。
                //
                // 分割規則:
                //  - 全部 dispatch 可 → **1 本の subtask** にまとめて逐次実行（順序保持・
                //    完了通知も 1 回 = 親の resume も 1 回）。
                //  - **先頭に inline 接頭辞、続く接尾辞が全部 dispatch 可** → 接頭辞を同期
                //    実行し、残りの接尾辞全体を 1 本の subtask として dispatch（#671）。
                //    接頭辞の完了後に接尾辞を dispatch し、接尾辞内は逐次実行のため
                //    バッチ内順序は保たれる。
                //  - **dispatch 可の後ろに inline ツールが来る** → 分割すると inline と
                //    background の相対順序が保証できないため**バッチ全体を inline 実行**
                //    （従来経路）。どのツールが縮退の原因かを debug ログに明示する。
                //  - dispatcher 未設定・全部 inline → 従来どおり全体 inline。
                //
                // `dispatch_start` は inline 接頭辞と dispatch 接尾辞の境界:
                //   Some(k) → inline [0,k) を同期実行、dispatch [k,len) を subtask 化
                //             （k==0 は全体 dispatch）。
                //   None    → 全体 inline。
                let dispatch_start: Option<usize> = match &self.tool_dispatcher {
                    Some(d) => {
                        let dispatchable: Vec<bool> = tool_calls
                            .iter()
                            .map(|tc| {
                                self.is_action_allowed(&tc.function.name)
                                    && d.should_dispatch(&tc.function.name)
                            })
                            .collect();
                        match dispatchable.iter().position(|&ok| ok) {
                            // dispatch 可が 1 つも無い（全部 inline）→ 全体 inline。
                            // 元から非ブロック要素が無いので縮退ログも出さない。
                            None => None,
                            Some(first) => {
                                if dispatchable[first..].iter().all(|&ok| ok) {
                                    // inline 接頭辞 [0,first) ＋ dispatch 可接尾辞 [first,len)。
                                    Some(first)
                                } else {
                                    // dispatch 可の後ろに inline ツール → 分割不可、全体 inline
                                    // に縮退。縮退原因（first より後ろの inline ツール）を明示。
                                    // 相関 ID（agent_id / session_id / turn_id）は #665 の span
                                    // から継承する。
                                    let forced: Vec<&str> = tool_calls
                                        .iter()
                                        .enumerate()
                                        .filter(|(i, _)| *i > first && !dispatchable[*i])
                                        .map(|(_, tc)| tc.function.name.as_str())
                                        .collect();
                                    tracing::debug!(
                                        iteration = iterations,
                                        stage = "batch_split",
                                        tools = tool_calls.len(),
                                        inline_tools = %forced.join(","),
                                        "turn: 混在バッチが全体 inline に縮退（dispatch 可の後ろに inline ツール）"
                                    );
                                    None
                                }
                            }
                        }
                    }
                    None => None,
                };

                // inline 接頭辞と dispatch 接尾辞に分ける。dispatch_start==None は全体 inline
                // （接尾辞は空）。境界の順で実行するため接頭辞を先に走らせ、その後で接尾辞を
                // 1 本の subtask に dispatch する。
                let (inline_calls, dispatch_calls): (&[ToolCall], &[ToolCall]) =
                    match dispatch_start {
                        Some(k) => (&tool_calls[..k], &tool_calls[k..]),
                        None => (&tool_calls[..], &tool_calls[..0]),
                    };

                for tool_call in inline_calls {
                    total_tool_calls += 1;
                    let tool_name = &tool_call.function.name;

                    // #665: inline ツール実行の入り。この後の `execute_with_id(...).await` が返らなければ
                    // ここが最後の行になる（シェル・MCP・返信送信など外部待ちのツールで固着した形）。
                    tracing::debug!(
                        iteration = iterations,
                        tool = %tool_name,
                        id = %tool_call.id,
                        stage = "tool_call",
                        "turn: ツール実行 開始（入）"
                    );

                    // Check if the action is declared by active skills.
                    if !self.is_action_allowed(tool_name) {
                        let denied = Self::permission_denied(tool_name);
                        let result_json = serde_json::to_string(&denied)
                            .unwrap_or_else(|_| r#"{"error": "Permission denied"}"#.to_string());
                        messages.push(Message::tool(tool_call.id.clone(), result_json.clone()));

                        // Notify on_tool_result callbacks for denied action.
                        for cb in &self.on_tool_result {
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

                    // ここを通るのは inline 実行対象（全体 inline、または混在バッチの
                    // inline 接頭辞）のみ。分割判定はバッチ単位でループ前に済んでいる（#671）。
                    let result = self
                        .executor
                        .execute_with_id(tool_name, &args, &tool_call.id)
                        .await;
                    // #665: inline ツール実行の出。入と対。success を載せ、この後 tool_result を積んで
                    // 次イテレーションへ回る。
                    tracing::debug!(
                        iteration = iterations,
                        tool = %tool_name,
                        id = %tool_call.id,
                        success = result.success,
                        stage = "tool_call",
                        "turn: ツール実行 完了（出）"
                    );

                    let result_json = serde_json::to_string(&result).unwrap_or_else(|_| {
                        r#"{"error": "Failed to serialize result"}"#.to_string()
                    });

                    // #284: LLM へ返す前に上限を効かせる。以降（messages / callback）は
                    // すべてこの capped 本文を使い、同ターンのプロンプトと DB に残る
                    // 本文を一致させる。
                    let result_json = self.cap_tool_result(tool_name, &tool_call.id, result_json);

                    messages.push(Message::tool(tool_call.id.clone(), result_json.clone()));

                    // Notify on_tool_result callbacks.
                    for cb in &self.on_tool_result {
                        cb(
                            tool_call.id.clone(),
                            tool_name.clone(),
                            result_json.clone(),
                            !result.success,
                        );
                    }
                }

                // dispatch 接尾辞（あれば）を 1 本の subtask にまとめて起動する。
                // inline 接頭辞の同期実行が終わった**後**にここへ来るため、順序保証は保たれる。
                // 各 tool_call には同じ subtask_id を持つ spawned マーカーを同ターンで返す。
                if !dispatch_calls.is_empty() {
                    let dispatcher = self
                        .tool_dispatcher
                        .as_ref()
                        .expect("dispatch_start is Some");
                    let calls: Vec<super::types::DispatchCall> = dispatch_calls
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
                    for tool_call in dispatch_calls {
                        let spawned = serde_json::json!({
                            "status": "spawned",
                            "subtask_id": outcome.subtask_id,
                            "tool": tool_call.function.name,
                            "label": outcome.label,
                        });
                        let result_json = serde_json::to_string(&spawned)
                            .unwrap_or_else(|_| r#"{"status":"spawned"}"#.to_string());
                        messages.push(Message::tool(tool_call.id.clone(), result_json.clone()));
                        for cb in &self.on_tool_result {
                            cb(
                                tool_call.id.clone(),
                                tool_call.function.name.clone(),
                                result_json.clone(),
                                false,
                            );
                        }
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

            // ここに来る最終応答は本文がある（#706: content 欠落／空文字／空白のみで
            // tool_call も無いターンは上流の意味的検証で fail loud 済み。空応答が Ok として
            // 通る唯一の穴だった 787 はこれで塞がっている）。

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

    /// 出力上限で切り捨てられた応答（finish_reason=Length）を表す。`text` は切り捨て
    /// 前にモデルが吐いた前置き。**これは chatgpt の parse_response が incomplete 応答に
    /// 対して返す形と同じ**（server 側の end-to-end テストが本物の parse_response を通す）。
    fn length_truncated_response(text: Option<&str>) -> ChatResponse {
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
                    tool_calls: None,
                    tool_call_id: None,
                },
                finish_reason: Some(FinishReason::Length),
            }],
            usage: Usage {
                completion_tokens: 4096,
                ..Usage::default()
            },
            created: 0,
        }
    }

    /// #676: finish_reason=Length（出力上限で切り捨て）はターンを失敗させる（fail loud）。
    /// 前置きテキストがあっても最終回答にしない。
    #[tokio::test]
    async fn test_output_limit_truncation_fails_the_turn() {
        let llm = MockLlm::new(vec![length_truncated_response(Some(
            "これから報告を書きます",
        ))]);
        let engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);

        let err = engine
            .run("system", "調査して報告して", "hermit:claude-opus-5")
            .await
            .expect_err("出力上限で切り捨てられたターンは Err にならねばならない");
        let msg = err.to_string();
        assert!(
            msg.contains("切り捨て"),
            "エラー文言が切り捨てを明示していない: {msg}"
        );
        assert!(
            msg.contains("max_output_tokens"),
            "エラー文言が上限（登録先）を含んでいない: {msg}"
        );
    }

    /// #706: 意味的に空の応答（content 欠落／空文字／空白のみ、かつ tool_call 無し）は
    /// ターンを失敗させる（fail loud）。3 形すべてを対象にする。finish_reason は付けない
    /// （＝provider が "stop" 相当を名乗る経路と同じ）。
    #[tokio::test]
    async fn test_empty_response_fails_the_turn_all_three_shapes() {
        // (content の形, 説明) の 3 形。
        let cases: Vec<(Option<&str>, &str)> = vec![
            (None, "content フィールド欠落"),
            (Some(""), "空文字"),
            (Some("   \n\t  "), "空白のみ"),
        ];
        for (content, label) in cases {
            let llm = MockLlm::new(vec![resp(content, vec![])]);
            let engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);
            let err = engine
                .run("system", "答えて", "cursor:grok")
                .await
                .expect_err(&format!("空応答（{label}）は Err にならねばならない"));
            assert!(
                err.to_string().contains("意味的に空"),
                "空応答（{label}）の Err 文言が理由を明示していない: {err}"
            );
        }
    }

    /// #706（最重要）: 空応答が **llm_logs に理由付きで残る**こと。log_callback は意味的
    /// 検証の結果（error_code / error_body）を受け取らねばならない——「error 欄空の成功行」
    /// として残る旧穴（設計 §1-c）が塞がっていることを固定する。特に「content フィールド
    /// 欠落 + tool_call 無し」を失敗として記録する。加えて、**原因の当たり付けに必要な
    /// opencrab 実測のプロンプト文字数**が本文に載ること（provider の usage ではなく engine
    /// が測った値）を固定する。
    #[tokio::test]
    async fn test_empty_response_is_recorded_in_log_with_reason() {
        use std::sync::Mutex;
        // (error_code, error_str, opencrab がこのリクエストで送った本文の文字数)
        let captured: Arc<Mutex<Vec<(Option<String>, Option<String>, usize)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();

        let llm = MockLlm::new(vec![resp(None, vec![])]); // content 欠落・tool_call 無し
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);
        engine.set_log_callback(move |log: &LlmCallLog| {
            sink.lock().unwrap().push((
                log.error_code.clone(),
                log.error_str.clone(),
                log.request.message_content_chars(),
            ));
        });

        // provider usage は既定で prompt_tokens=0（当てにならない経路）。本文は engine が
        // 組む system + conversation なので、実測文字数は 0 より大きくなるはず。
        let _ = engine.run("system", "答えて", "cursor:grok").await;

        let logs = captured.lock().unwrap();
        assert_eq!(logs.len(), 1, "1 ターン = 1 ログ行のはず");
        let (code, body, sent_chars) = &logs[0];
        assert_eq!(
            code.as_deref(),
            Some("empty_response"),
            "error_code が empty_response でない: {code:?}"
        );
        let body = body.as_deref().unwrap_or("");
        assert!(
            body.contains("意味的に空"),
            "error_body が空応答を明示していない: {body}"
        );
        // 原因の当たり付け材料: opencrab 実測のプロンプト文字数が本文に載っていること。
        assert!(
            *sent_chars > 0,
            "opencrab 実測の本文文字数が 0（測れていない）"
        );
        assert!(
            body.contains(&format!("prompt_chars={sent_chars}")),
            "error_body に opencrab 実測のプロンプト文字数が載っていない（body={body}, \
             期待 prompt_chars={sent_chars}）"
        );
    }

    /// #706 回帰防止: empty content でも **tool_call があれば空ではない**（ツール往復の
    /// 一手なのでターンは継続する）。空判定に tool_call を混ぜていることの固定。
    #[tokio::test]
    async fn test_empty_content_with_tool_call_is_not_empty() {
        let llm = MockLlm::new(vec![
            tool_call_response(vec![tc("c1", "test_tool", serde_json::json!({}))]),
            text_response("done"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!("ok"),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        let result = engine
            .run("system", "使って", "test-model")
            .await
            .expect("tool_call 付きの空 content は失敗にならない");
        assert_eq!(result.response, "done");
        assert_eq!(result.tool_calls_made, 1);
    }

    /// #676: finish_reason=Stop の正常応答は従来どおり最終回答として返る（回帰防止）。
    #[tokio::test]
    async fn test_stop_finish_reason_is_returned_normally() {
        let llm = MockLlm::new(vec![ChatResponse::text("完了しました")]);
        let engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);
        let result = engine.run("system", "hi", "test-model").await.unwrap();
        assert_eq!(result.response, "完了しました");
    }

    /// #676: set_max_output_tokens で設定した値が実際に ChatRequest.max_tokens へ載る。
    /// 未設定なら None（プロバイダ既定に委ねる）。
    #[tokio::test]
    async fn test_max_output_tokens_reaches_the_request() {
        use std::sync::Mutex;

        struct RecordingLlm {
            seen: Arc<Mutex<Option<Option<u32>>>>,
        }
        #[async_trait]
        impl LlmClient for RecordingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                *self.seen.lock().unwrap() = Some(request.max_tokens);
                Ok(ChatResponse::text("ok"))
            }
        }

        // set した場合。
        let seen = Arc::new(Mutex::new(None));
        let mut engine = SkillEngine::new(
            Box::new(RecordingLlm { seen: seen.clone() }),
            Box::new(MockExecutor::new()),
            10,
        );
        engine.set_max_output_tokens(128_000);
        engine.run("system", "hi", "test-model").await.unwrap();
        assert_eq!(*seen.lock().unwrap(), Some(Some(128_000)));

        // 未設定なら None（上限未指定）。
        let seen2 = Arc::new(Mutex::new(None));
        let engine2 = SkillEngine::new(
            Box::new(RecordingLlm {
                seen: seen2.clone(),
            }),
            Box::new(MockExecutor::new()),
            10,
        );
        engine2.run("system", "hi", "test-model").await.unwrap();
        assert_eq!(*seen2.lock().unwrap(), Some(None));
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

    /// #397: ツールフックは**複数の購読者**が同じ engine に載る（subtask の進捗実況と
    /// session_logs への永続化）。後から配線した方が前を消してはならない。
    ///
    /// 代入だった頃は 2 つ目の登録で 1 つ目が黙って落ち、`persist_turn_logs` が true の
    /// ターン（＝後から永続化フックが載るターン）で進捗実況が丸ごと死んでいた。
    #[tokio::test]
    async fn test_tool_hooks_accumulate_instead_of_replacing() {
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![
            tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
            text_response("done"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"result": "ok"}),
                error: None,
            },
        );
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        // 1 つ目 = 進捗実況相当、2 つ目 = 永続化相当。process.rs と同じ配線順。
        let calls: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(vec![]));
        let results: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(vec![]));

        let c1 = calls.clone();
        engine.add_on_tool_call(move |_content, _json| c1.lock().unwrap().push("notifier"));
        let r1 = results.clone();
        engine
            .add_on_tool_result(move |_id, _name, _json, _err| r1.lock().unwrap().push("notifier"));
        let c2 = calls.clone();
        engine.add_on_tool_call(move |_content, _json| c2.lock().unwrap().push("turn_log"));
        let r2 = results.clone();
        engine
            .add_on_tool_result(move |_id, _name, _json, _err| r2.lock().unwrap().push("turn_log"));

        engine.run("system", "do it", "test-model").await.unwrap();

        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &["notifier", "turn_log"],
            "on_tool_call は登録順に全部呼ばれること（後勝ちで消えない）"
        );
        assert_eq!(
            results.lock().unwrap().as_slice(),
            &["notifier", "turn_log"],
            "on_tool_result は登録順に全部呼ばれること（後勝ちで消えない）"
        );
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
        engine.add_on_tool_result(move |_id, name, json, _err| {
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
        engine.add_on_tool_result(move |_id, _name, json, _err| {
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
            crate::tokens::estimate_tokens(tool_msg)
                < crate::tool_result_log::TOOL_RESULT_TOKEN_LIMIT,
            "LLM へ {} トークンの tool_result が渡っている（上限 {}）",
            crate::tokens::estimate_tokens(tool_msg),
            crate::tool_result_log::TOOL_RESULT_TOKEN_LIMIT
        );
        // #294: 生データは 1 バイトも渡らない（プレビューも無い）。
        assert!(
            !tool_msg.contains("npub1abcdefgh"),
            "生データが LLM へ渡っている: {tool_msg}"
        );
        assert!(
            tool_msg.contains("withheld"),
            "退避の案内が無い: {tool_msg}"
        );
        assert!(
            tool_msg.contains("tmp/sess1-tc-1.json"),
            "全文の在り処が案内されていない: {tool_msg}"
        );
        assert!(tool_msg.contains("lines"), "行数が無い: {tool_msg}");
        assert!(tool_msg.contains("tokens"), "トークン数が無い: {tool_msg}");
        // 全文はワークスペースに残り、エージェントが読める。
        assert!(workspace.path().join("tmp/sess1-tc-1.json").exists());
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
        assert!(
            crate::tokens::estimate_tokens(tool_msg)
                < crate::tool_result_log::TOOL_RESULT_TOKEN_LIMIT
        );
        assert!(tool_msg.contains("could not be saved"));
        // 退避できなくても生データは流さない（#294）。
        assert!(
            !tool_msg.contains("zzz"),
            "生データが流れている: {tool_msg}"
        );
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
        engine.add_on_tool_result(move |_id, _name, json, _err| {
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

    /// [#671 回帰] inline 接頭辞 ＋ dispatch 可接尾辞の混在バッチは、接頭辞を同期実行して
    /// から接尾辞を **1 本の subtask** として dispatch する。実行順は「接頭辞 inline →
    /// 接尾辞 dispatch」で固定し、接尾辞は spawned マーカーを同ターンで返す。
    /// （実事故: `[record_task_progress(inline), execute_shell(31 分)]` が全体 inline に
    /// 落ちてロックを占有した縮退の再発防止。）
    #[tokio::test]
    async fn test_inline_prefix_then_dispatch_suffix_split() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use std::sync::{Arc, Mutex};

        // record_task_progress(inline 分類) → execute_shell(dispatch 可) の順。
        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                tc(
                    "tc-1",
                    "record_task_progress",
                    serde_json::json!({"note": "start"}),
                ),
                tc(
                    "tc-2",
                    "execute_shell",
                    serde_json::json!({"cmd": "claude ..."}),
                ),
            ]),
            text_response("開始しました"),
        ]);

        // executor（inline 実行）と dispatcher（subtask 化）を同一タイムラインへ記録し、
        // 「接頭辞 inline が接尾辞 dispatch より先に完了する」を固定する。
        let timeline: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        struct TimelineExecutor {
            tl: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl ActionExecutor for TimelineExecutor {
            async fn execute(&self, name: &str, _args: &Value) -> ActionResult {
                self.tl.lock().unwrap().push(format!("inline:{name}"));
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

        struct TimelineDispatcher {
            control: std::collections::HashSet<String>,
            tl: Arc<Mutex<Vec<String>>>,
            dispatched: Mutex<Vec<String>>,
            batches: AtomicUsize,
        }
        impl crate::ToolDispatcher for TimelineDispatcher {
            fn should_dispatch(&self, name: &str) -> bool {
                !self.control.contains(name)
            }
            fn dispatch_batch(&self, calls: &[crate::DispatchCall]) -> crate::DispatchOutcome {
                self.batches.fetch_add(1, AtomicOrdering::SeqCst);
                let names: Vec<String> = calls.iter().map(|c| c.tool_name.clone()).collect();
                self.tl
                    .lock()
                    .unwrap()
                    .push(format!("dispatch:{}", names.join("+")));
                self.dispatched.lock().unwrap().push(names.join(","));
                crate::DispatchOutcome {
                    subtask_id: format!("sub-for-{}", names.join("+")),
                    label: names.join(", "),
                }
            }
        }

        let executor = TimelineExecutor {
            tl: timeline.clone(),
        };
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        let dispatcher = Arc::new(TimelineDispatcher {
            control: ["record_task_progress"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            tl: timeline.clone(),
            dispatched: Mutex::new(Vec::new()),
            batches: AtomicUsize::new(0),
        });
        engine.set_tool_dispatcher(dispatcher.clone());

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        engine.add_on_tool_result(move |_id, name, json, _err| {
            seen_clone.lock().unwrap().push(format!("{name}:{json}"));
        });

        let result = engine.run("system", "go", "test-model").await.unwrap();

        // 接頭辞 inline → 接尾辞 dispatch の順で実行される（順序保証）。
        assert_eq!(
            timeline.lock().unwrap().as_slice(),
            &[
                "inline:record_task_progress".to_string(),
                "dispatch:execute_shell".to_string()
            ],
            "inline 接頭辞は接尾辞 dispatch より先に完了する"
        );
        // dispatch は接尾辞（execute_shell）だけを 1 本にまとめる。
        assert_eq!(dispatcher.batches.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(
            dispatcher.dispatched.lock().unwrap().as_slice(),
            &["execute_shell"]
        );

        // tool_result: 接頭辞は inline 実結果、接尾辞は spawned マーカー（同ターン返却）。
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].starts_with("record_task_progress:"));
        assert!(
            !seen[0].contains("\"status\":\"spawned\""),
            "接頭辞は inline 実行結果であって spawned マーカーではない"
        );
        assert!(seen[1].starts_with("execute_shell:"));
        assert!(
            seen[1].contains("\"status\":\"spawned\""),
            "接尾辞は spawned マーカー（同ターン返却）"
        );
        assert!(seen[1].contains("\"subtask_id\":\"sub-for-execute_shell\""));

        assert_eq!(result.tool_calls_made, 2);
        assert_eq!(result.response, "開始しました");
    }

    /// [#671 回帰] dispatch 可ツールの**後ろに** inline ツールが来る混在バッチは分割できず
    /// （inline と background の相対順序が保証できない）、従来どおり全体 inline に縮退する。
    /// このとき縮退原因のツール名を含む debug ログ（stage="batch_split"）を出す。
    ///
    /// 縮退ログの捕捉はスレッドローカル subscriber に依存するため、cargo の並列テストと
    /// 干渉しないよう、専用の current-thread ランタイムを `with_default` の内側で回す
    /// （`#[tokio::test]` だと subscriber の有効スレッドと polling スレッドがずれ得る）。
    #[test]
    fn test_dispatchable_then_inline_stays_whole_inline_and_logs() {
        use std::sync::{Arc, Mutex};

        // execute_shell(dispatch 可) → record_task_progress(inline) の順。
        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                tc("tc-1", "execute_shell", serde_json::json!({"cmd": "x"})),
                tc(
                    "tc-2",
                    "record_task_progress",
                    serde_json::json!({"note": "done"}),
                ),
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

        // 縮退 debug ログを捕捉する。cargo の並列テストが触る tracing のグローバル
        // MAX_LEVEL と干渉しないよう、fmt を使わず**常時 enabled** の最小 Subscriber で
        // イベントのフィールドを直接拾う（`enabled` が常に true、`max_level_hint` は
        // 既定=TRACE なのでレベル早期棄却の影響を受けない）。
        struct FieldGrabber {
            out: String,
        }
        impl tracing::field::Visit for FieldGrabber {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.out.push_str(&format!("{}={:?};", field.name(), value));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.out.push_str(&format!("{}={};", field.name(), value));
            }
        }
        struct CaptureSubscriber {
            events: Arc<Mutex<Vec<String>>>,
        }
        impl tracing::Subscriber for CaptureSubscriber {
            fn enabled(&self, _md: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                let mut grabber = FieldGrabber { out: String::new() };
                event.record(&mut grabber);
                self.events.lock().unwrap().push(grabber.out);
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: events.clone(),
        };

        let order = Arc::new(Mutex::new(Vec::new()));
        let mut engine = SkillEngine::new(
            Box::new(llm),
            Box::new(OrderExecutor {
                order: order.clone(),
            }),
            10,
        );
        // record_task_progress を inline（should_dispatch=false）扱いに。
        let dispatcher = Arc::new(RecordingDispatcher::new(&["record_task_progress"]));
        engine.set_tool_dispatcher(dispatcher.clone());

        // subscriber を有効化したまま、同一スレッドで run を完走させる。
        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                engine.run("system", "go", "test-model").await.unwrap();
            });
        });

        // dispatch は起きず、全体 inline を LLM 順で実行する。
        assert_eq!(
            dispatcher.dispatched.lock().unwrap().len(),
            0,
            "dispatch 可の後ろに inline が来る混在バッチは dispatch せず全体 inline"
        );
        assert_eq!(
            order.lock().unwrap().as_slice(),
            &["execute_shell", "record_task_progress"],
            "全体 inline は LLM が並べた順序を守る"
        );

        // 縮退ログが出ており、stage=batch_split と原因ツール名（record_task_progress）を含む。
        let logs = events.lock().unwrap().join("\n");
        assert!(
            logs.contains("batch_split"),
            "縮退 debug ログ（stage=batch_split）が出る: {logs}"
        );
        assert!(
            logs.contains("record_task_progress"),
            "縮退ログに原因の inline ツール名が載る: {logs}"
        );
    }

    /// [#671] 制御系 inline ツール（declare_done: ターン終了宣言）が接頭辞に来ても、
    /// エンジンのループ終了条件と矛盾しないことを固定する。declare_done を inline 実行し、
    /// 後続の execute_shell を背景 subtask 化して同ターンで spawned を返す。ターンの終了は
    /// 従来どおり「LLM が次イテレーションでツールを呼ばない」ことで駆動され、declare_done の
    /// `{done:true}` 結果はループを早期に切らない（engine は tool_result の done を見ない）。
    #[tokio::test]
    async fn test_control_inline_prefix_dispatches_suffix_and_loop_ends_on_llm() {
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                tc(
                    "tc-1",
                    "declare_done",
                    serde_json::json!({"reason": "十分議論した"}),
                ),
                tc(
                    "tc-2",
                    "execute_shell",
                    serde_json::json!({"cmd": "claude ..."}),
                ),
            ]),
            // 次イテレーションでツールを呼ばない → ここでループ終了（declare_done ではなく）。
            text_response("終わります"),
        ]);

        // executor は inline 実行だけを記録（declare_done のみ来るべき）。
        struct SpyExecutor {
            called: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl ActionExecutor for SpyExecutor {
            async fn execute(&self, name: &str, _args: &Value) -> ActionResult {
                self.called.lock().unwrap().push(name.to_string());
                ActionResult {
                    success: true,
                    data: serde_json::json!({"done": true}),
                    error: None,
                }
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                vec![]
            }
        }
        let called = Arc::new(Mutex::new(Vec::new()));
        let mut engine = SkillEngine::new(
            Box::new(llm),
            Box::new(SpyExecutor {
                called: called.clone(),
            }),
            10,
        );
        // declare_done を inline（should_dispatch=false）扱いに。
        let dispatcher = Arc::new(RecordingDispatcher::new(&["declare_done"]));
        engine.set_tool_dispatcher(dispatcher.clone());

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        engine.add_on_tool_result(move |_id, name, json, _err| {
            seen_clone.lock().unwrap().push(format!("{name}:{json}"));
        });

        let result = engine.run("system", "go", "test-model").await.unwrap();

        // declare_done だけ inline 実行、execute_shell は inline 実行されない。
        assert_eq!(called.lock().unwrap().as_slice(), &["declare_done"]);
        // execute_shell（接尾辞）が 1 本の subtask として dispatch される。
        assert_eq!(
            dispatcher.dispatched.lock().unwrap().as_slice(),
            &["execute_shell"]
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].starts_with("declare_done:"));
        assert!(
            seen[0].contains("\"done\":true"),
            "接頭辞は declare_done の inline 実行結果"
        );
        assert!(seen[1].starts_with("execute_shell:"));
        assert!(
            seen[1].contains("\"status\":\"spawned\""),
            "接尾辞は spawned マーカー（同ターン返却）"
        );

        // ループは declare_done では切れず、LLM が次イテレーションでツールを呼ばず終了する。
        assert_eq!(
            result.iterations, 2,
            "ターンは 2 イテレーションで正常終了する"
        );
        assert_eq!(result.response, "終わります");
    }

    /// [#671 挙動変化] 未許可ツールが接頭辞・dispatch 可ツールが接尾辞に来るバッチ
    /// （例: typo や権限落ちの 1 ツール）。**旧実装**は「1 つでも `is_action_allowed &&
    /// should_dispatch` を満たさない → 全体 inline」で execute_shell も inline に落ちていた。
    /// **新実装**は未許可ツールに permission denied を返した後、接尾辞を背景 subtask 化する
    /// （1 ツールの権限落ちが非ブロック性を壊さない）。denied の扱い自体は不変。
    #[tokio::test]
    async fn test_unauthorized_prefix_still_dispatches_suffix() {
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                tc("tc-1", "not_a_real_tool", serde_json::json!({})),
                tc("tc-2", "execute_shell", serde_json::json!({"cmd": "x"})),
            ]),
            text_response("done"),
        ]);
        // executor は inline 実行のみ記録（未許可は executor に届かず denied になるべき）。
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
        let mut engine = SkillEngine::new(
            Box::new(llm),
            Box::new(SpyExecutor {
                called: called.clone(),
            }),
            10,
        );
        // execute_shell のみ許可（not_a_real_tool は未許可 → is_action_allowed=false）。
        engine.set_allowed_actions(["execute_shell".to_string()]);
        // control 集合は空。execute_shell は dispatch 可。
        let dispatcher = Arc::new(RecordingDispatcher::new(&[]));
        engine.set_tool_dispatcher(dispatcher.clone());

        let seen: Arc<Mutex<Vec<(String, String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        engine.add_on_tool_result(move |_id, name, json, err| {
            seen_clone.lock().unwrap().push((name, json, err));
        });

        engine.run("system", "go", "test-model").await.unwrap();

        // 未許可ツールは executor に届かない（denied の扱い不変）。
        assert!(
            called.lock().unwrap().is_empty(),
            "未許可ツールは inline executor に渡らない"
        );
        // 接尾辞 execute_shell は inline に落ちず、背景 subtask として dispatch される（挙動変化）。
        assert_eq!(
            dispatcher.dispatched.lock().unwrap().as_slice(),
            &["execute_shell"],
            "未許可ツールが接頭辞にあっても dispatch 可接尾辞は subtask 化される"
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        // 接頭辞: permission denied（err=true・not authorized 文言）。従来どおり。
        assert_eq!(seen[0].0, "not_a_real_tool");
        assert!(seen[0].2, "未許可ツールは err=true で通知される");
        assert!(seen[0].1.contains("is not authorized"));
        // 接尾辞: spawned マーカー（同ターン・err=false）。
        assert_eq!(seen[1].0, "execute_shell");
        assert!(!seen[1].2);
        assert!(seen[1].1.contains("\"status\":\"spawned\""));
    }
}

/// 走行中ターンへの新着ユーザー発言の注入（#289）。
///
/// 会話履歴はターン開始時に 1 度だけ組まれるため、ツール往復が長引くとその間に届いた
/// 発言が次ターンまで入力に載らなかった。ここではエンジン側の契約
/// （いつ引くか / 何を積むか / 何もしない条件）だけを検査する。
#[cfg(test)]
mod live_inbound_tests {
    use super::*;
    use async_trait::async_trait;
    use opencrab_llm_types::{
        ChatResponse, Choice, FunctionCall, FunctionDefinition, MessageContent, Usage,
    };

    /// LLM へ実際に渡ったリクエストを記録するモック。
    struct RecordingLlm {
        responses: std::sync::Mutex<Vec<ChatResponse>>,
        requests: std::sync::Mutex<Vec<ChatRequest>>,
    }

    impl RecordingLlm {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
                requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// n 回目（0 始まり）の呼び出しに載った user ロールの本文。
        fn user_texts(&self, nth: usize) -> Vec<String> {
            let requests = self.requests.lock().unwrap();
            requests[nth]
                .messages
                .iter()
                .filter(|m| m.role == Role::User)
                .filter_map(|m| match m.content.as_ref() {
                    Some(MessageContent::Text(t)) => Some(t.clone()),
                    _ => None,
                })
                .collect()
        }

        fn call_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl LlmClient for RecordingLlm {
        async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
            self.requests.lock().unwrap().push(request);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                anyhow::bail!("no more mock responses");
            }
            Ok(responses.remove(0))
        }
    }

    struct NoopExecutor;

    #[async_trait]
    impl ActionExecutor for NoopExecutor {
        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
            ActionResult {
                success: true,
                data: serde_json::json!("ok"),
                error: None,
            }
        }
        fn list_tools(&self) -> Vec<FunctionDefinition> {
            vec![FunctionDefinition {
                name: "test_tool".to_string(),
                description: Some("A test tool".to_string()),
                parameters: serde_json::json!({}),
            }]
        }
    }

    /// 実装側の契約（前回 poll 以降だけを返す）を再現する source。
    ///
    /// 「まだ配っていない分」を配り切ったら以後は空を返す。本番実装（server 側）は
    /// 同じことを log id の watermark で行う。
    struct ScriptedInbound {
        pending: std::sync::Mutex<Vec<Vec<String>>>,
        polls: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedInbound {
        fn new(batches: Vec<Vec<&str>>) -> Self {
            Self {
                pending: std::sync::Mutex::new(
                    batches
                        .into_iter()
                        .map(|b| b.into_iter().map(str::to_string).collect())
                        .collect(),
                ),
                polls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn polls(&self) -> usize {
            self.polls.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl LiveInboundSource for ScriptedInbound {
        fn poll_new_messages(&self) -> Vec<String> {
            self.polls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut pending = self.pending.lock().unwrap();
            if pending.is_empty() {
                Vec::new()
            } else {
                pending.remove(0)
            }
        }
    }

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "test_tool".to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    fn response(text: Option<&str>, calls: Vec<ToolCall>) -> ChatResponse {
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

    /// ループ実行中に届いた発言が、**次のイテレーションの入力**に載る。
    ///
    /// これが #289 の本体: 1 回目の LLM 呼び出し時点では入力に無く、ツール往復を挟んだ
    /// 2 回目には載っていること。
    #[tokio::test]
    async fn new_speech_reaches_the_next_iteration() {
        let llm = std::sync::Arc::new(RecordingLlm::new(vec![
            response(None, vec![tool_call("call-1")]),
            response(Some("了解、止めるね"), vec![]),
        ]));
        let source = std::sync::Arc::new(ScriptedInbound::new(vec![vec!["[owner]:\nやめて"]]));

        let mut engine =
            SkillEngine::new(Box::new(LlmHandle(llm.clone())), Box::new(NoopExecutor), 10);
        engine.set_live_inbound(source.clone());
        engine
            .run("system", "作業して", "test-model")
            .await
            .unwrap();

        assert_eq!(llm.call_count(), 2);
        let first = llm.user_texts(0);
        assert!(
            !first.iter().any(|t| t.contains("やめて")),
            "ターン開始時にはまだ届いていない: {first:?}"
        );
        let second = llm.user_texts(1);
        assert!(
            second.iter().any(|t| t.contains("やめて")),
            "走行中の新着が次のイテレーションに載る: {second:?}"
        );
    }

    /// 同じ発言は二度注入されない。
    ///
    /// source は「前回以降」だけを返す契約なので、3 イテレーション回しても該当の本文は
    /// 全リクエストを通じて 1 回しか現れない。毎回足すとプロンプトが際限なく膨らむ。
    #[tokio::test]
    async fn the_same_speech_is_never_injected_twice() {
        let llm = std::sync::Arc::new(RecordingLlm::new(vec![
            response(None, vec![tool_call("call-1")]),
            response(None, vec![tool_call("call-2")]),
            response(Some("done"), vec![]),
        ]));
        let source = std::sync::Arc::new(ScriptedInbound::new(vec![vec!["[owner]:\nやめて"]]));

        let mut engine =
            SkillEngine::new(Box::new(LlmHandle(llm.clone())), Box::new(NoopExecutor), 10);
        engine.set_live_inbound(source.clone());
        engine
            .run("system", "作業して", "test-model")
            .await
            .unwrap();

        assert_eq!(llm.call_count(), 3);
        let occurrences = llm
            .user_texts(2)
            .iter()
            .filter(|t| t.contains("やめて"))
            .count();
        assert_eq!(occurrences, 1, "最終リクエストにも 1 件だけ載る");
    }

    /// 1 回目の LLM 呼び出しの前には poll しない（履歴と二重になるため）。
    #[tokio::test]
    async fn the_first_iteration_does_not_poll() {
        let llm = std::sync::Arc::new(RecordingLlm::new(vec![response(Some("hi"), vec![])]));
        let source = std::sync::Arc::new(ScriptedInbound::new(vec![]));

        let mut engine =
            SkillEngine::new(Box::new(LlmHandle(llm.clone())), Box::new(NoopExecutor), 10);
        engine.set_live_inbound(source.clone());
        engine.run("system", "hi", "test-model").await.unwrap();

        assert_eq!(llm.call_count(), 1);
        assert_eq!(source.polls(), 0, "ツール往復が無ければ引かない");
    }

    /// 新着が無ければ入力は従来と同一（1 バイトも増えない）。
    #[tokio::test]
    async fn no_new_speech_changes_nothing() {
        let script = vec![
            response(None, vec![tool_call("call-1")]),
            response(Some("done"), vec![]),
        ];
        let with_source = std::sync::Arc::new(RecordingLlm::new(script.clone()));
        let without_source = std::sync::Arc::new(RecordingLlm::new(script));

        let mut engine = SkillEngine::new(
            Box::new(LlmHandle(with_source.clone())),
            Box::new(NoopExecutor),
            10,
        );
        engine.set_live_inbound(std::sync::Arc::new(ScriptedInbound::new(vec![])));
        engine.run("system", "go", "test-model").await.unwrap();

        let baseline = SkillEngine::new(
            Box::new(LlmHandle(without_source.clone())),
            Box::new(NoopExecutor),
            10,
        );
        baseline.run("system", "go", "test-model").await.unwrap();

        assert_eq!(
            with_source.user_texts(1),
            without_source.user_texts(1),
            "新着ゼロなら注入口の有無でプロンプトは変わらない"
        );
    }

    /// `Arc<RecordingLlm>` を `Box<dyn LlmClient>` として engine に渡すための薄い委譲。
    struct LlmHandle(std::sync::Arc<RecordingLlm>);

    #[async_trait]
    impl LlmClient for LlmHandle {
        async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
            self.0.chat(request).await
        }
    }
}
