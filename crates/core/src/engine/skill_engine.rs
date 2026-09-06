use std::sync::Arc;

mod run_helpers;
mod turn_budget;

use anyhow::Result;
use tracing;

use super::types::{
    ActionExecutor, ActionResult, ChatRequest, EngineResult, LiveInboundSource, LlmCallLog,
    LlmClient, ToolDispatcher,
};
#[cfg(test)]
use opencrab_llm_types::FinishReason;
use opencrab_llm_types::{Message, MessageContent, Role, ToolCall};
use run_helpers::{
    classify_call_failure, initialize_turn, normalize_response, partition_tool_calls_for_dispatch,
    strip_continue_marker, InitialTurn,
};
use turn_budget::{apply_turn_budget, seat_tool_result};
#[cfg(test)]
use turn_budget::{message_plain_text, user_line_items};

// ---------------------------------------------------------------------------
// SkillEngine
// ---------------------------------------------------------------------------

/// LLM 呼び出しごとのログコールバック。
type LogCallback = Box<dyn Fn(&LlmCallLog) + Send + Sync>;
/// ツール結果受信フック: (tool_call_id, tool_name, result_json, is_error)。
type ToolResultHook = Arc<dyn Fn(String, String, String, bool) + Send + Sync>;
/// #898: 継続分岐（末尾 CONTINUE の text-only イテレーション）で剥がした途中発話を
/// **配送・保存する非同期フック**。配送はループ中に行い、失敗（Err）は継続を止める
/// （§13.1 j: 失敗を隠して次に進まない）。REST/extgate/intake が各レーンの配線を渡す。
type ContinuationSpeechHook = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
        + Send
        + Sync,
>;

/// #930: 走行中に畳み込んだ said を LLM へ渡す時点で、その said の origin を gateway へ
/// 通知する **非同期フック**（read state の付与＝👀）。best-effort（失敗してもターンは続ける）
/// なので Result は返さない。extgate が emit_activity(state="read", origin) の配線を渡す。
type FoldedOriginHook = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

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
    pub log_callback: Option<LogCallback>,
    /// Optional callback invoked with response text on every LLM reply.
    pub on_response_text: Option<Arc<dyn Fn(String) + Send + Sync>>,
    /// #898: 末尾 CONTINUE で継続する text-only イテレーションで剥がした途中発話を
    /// 配送・保存する非同期フック。`on_response_text` は最終応答・text+tool 併記でも
    /// 発火するため区別できず流用不可（二重配送・二重保存を招く）。このフックは **継続分岐
    /// （マーカー剥がし後・次イテレーション前）でのみ** 非空の本文で await され、Err なら
    /// 継続を止めてターンを失敗させる（§13.1 j）。
    pub on_continuation_speech: Option<ContinuationSpeechHook>,
    /// Callbacks invoked when the assistant produces tool calls: (assistant_content, tool_calls_json).
    ///
    /// **複数**持つ（#397）。購読者は独立している（subtask の進捗実況・session_logs への
    /// 永続化）ので、後から配線した方が前を消してはならない。登録順に全部呼ぶ。
    on_tool_call: Vec<Arc<dyn Fn(String, String) + Send + Sync>>,
    /// Callbacks invoked when a tool result is received: (tool_call_id, tool_name, result_json, is_error).
    /// [`Self::on_tool_call`] と同じく複数持ち、登録順に全部呼ぶ（#397）。
    on_tool_result: Vec<ToolResultHook>,
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
    /// #930: 畳み込んだ said の origin を read state として通知するフック。None なら通知しない。
    on_folded_origin: Option<FoldedOriginHook>,
    /// 各 ChatRequest に載せる出力トークン上限（#676）。使用モデルの実能力値を
    /// `model_pricing` から解決して process 側で注入する（[`Self::set_max_output_tokens`]）。
    /// `None` は「上限未指定」＝プロバイダの既定に委ねる（テスト / sub-engine 用）。
    /// 本番経路は必ず Some を入れる（未登録なら process 側がターンを fail loud で止め、
    /// engine まで来ない）。この値で頭打ちになった応答は finish_reason=Length で戻り、
    /// run ループがターンを失敗させる。
    max_output_tokens: Option<u32>,
    /// 会話車線の二水位（#826-B）。未設定なら途中圧縮しない（テスト / sub-engine）。
    conversation_high: Option<usize>,
    conversation_low: Option<usize>,
    /// PR2 (#884): 事前に組んだ typed 会話。Some のとき初期 messages を typed history から組む。
    typed_conversation: Option<crate::conversation_typed::TypedConversation>,
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
            on_continuation_speech: None,
            on_tool_call: Vec::new(),
            on_tool_result: Vec::new(),
            reasoning_effort: None,
            web_search: false,
            tool_dispatcher: None,
            tool_result_offload: None,
            live_inbound: None,
            on_folded_origin: None,
            max_output_tokens: None,
            conversation_high: None,
            conversation_low: None,
            typed_conversation: None,
        }
    }

    /// ターン内 append 境界の二水位。設定すると TokenLedger 合計だけで超過判定し、
    /// 超えたときだけ合成 user 文字列を低水位まで刈る。
    pub fn set_conversation_waters(&mut self, high: usize, low: usize) {
        self.conversation_high = Some(high);
        self.conversation_low = Some(low);
    }

    /// #884 PR2: typed 会話を差し込む（None で flat 挙動へ戻す）。
    pub fn set_typed_conversation(
        &mut self,
        tc: Option<crate::conversation_typed::TypedConversation>,
    ) {
        self.typed_conversation = tc;
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

    /// #930: read state（👀）通知フックを設定する。extgate 経路だけが渡す。
    pub fn set_on_folded_origin(&mut self, cb: FoldedOriginHook) {
        self.on_folded_origin = Some(cb);
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
    /// 上限は `min(ツール別上限, 残り会話枠)`。超えたら既存の spool-with-stub。
    /// **これを通さずに `Message::tool` へ積んではいけない。** 素通りさせると
    /// 1 件の巨大な結果（実例: 76,661 バイトのフォロー一覧）がプロンプトを占有し、
    /// 同ターンのユーザー発言が 1 件も載らなくなる。
    /// 永続化側（`on_tool_result` → `sanitize_tool_result_for_log`）と同じ退避先を使うので、
    /// 同ターンで見える本文と次ターンに再注入される本文が一致する。
    fn cap_tool_result(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        result_json: String,
        remaining: Option<usize>,
    ) -> String {
        // 上限判定は `sanitize_tool_result_for_llm` 側が持つ（トークン数で測るため
        // ここでバイト数の早期 return を二重に置くと物差しがズレる）。
        let (session_id, workspace_root) = match &self.tool_result_offload {
            Some(o) => (o.session_id.as_str(), o.workspace_root.as_deref()),
            // 退避先未設定（sub-engine / テスト）でも上限は必ず効かせる。
            None => ("session", None),
        };
        let capped = crate::tool_result_log::sanitize_tool_result_for_append(
            tool_name,
            &result_json,
            session_id,
            tool_call_id,
            workspace_root,
            remaining,
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

    /// #898: 継続分岐（末尾 CONTINUE の text-only イテレーション）専用の途中発話フックを設定する。
    /// マーカー剥がし後・次イテレーション前に、非空の本文で await される。Err は継続を止める。
    pub fn set_on_continuation_speech(&mut self, cb: ContinuationSpeechHook) {
        self.on_continuation_speech = Some(cb);
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

    /// 発話クラス（撃ちっぱなし）のツールか（§3.3・第三柱）。dispatcher が権威で、
    /// 未設定・非発話は `false`（従来ツール）。
    fn is_utterance_tool(&self, name: &str) -> bool {
        self.tool_dispatcher
            .as_ref()
            .is_some_and(|d| d.is_utterance(name))
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
        // §2.7: functions はループ内で毎イテレーション list_tools を取り直して組む（活性集合を
        // 反映）。ここでの事前取得は結果を捨てる死んだ呼び出しだったので置かない。

        let InitialTurn {
            mut messages,
            ledger: mut turn_ledger,
            governor: mut turn_gov,
        } = initialize_turn(
            system_context,
            user_message,
            image_urls,
            self.typed_conversation.as_ref(),
            (self.conversation_high, self.conversation_low),
        );

        let mut iterations = 0;
        // #930: このターンで read（👀）を通知済みの origin。1 origin 1 回に絞る。
        let mut read_emitted_origins: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut total_tool_calls = 0;
        let mut xml_fallback_parses = 0;
        // #915: 各生成で最後に成功した投稿系 utterance-op の call_id。生成開始時にリセットし、
        // 上限到達時だけ直前（打ち切られた最終生成）の値を保持して返す。
        let mut last_posting_utterance_id: Option<String> = None;
        let mut last_generation_had_continuation_speech = false;
        // #898 §13.1 a: 空 CONTINUE（本文なし・継続）の連続回数。3 連続で解析 warn を 1 行出す。
        let mut consecutive_empty_continue: usize = 0;

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
                    last_posting_utterance_id,
                    last_generation_had_continuation_speech,
                    xml_fallback_parses,
                });
            }
            last_posting_utterance_id = None;
            last_generation_had_continuation_speech = false;

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
                    // #930: origin つきで引く。畳み込んだ said を LLM へ渡す **この時点** で、
                    // その said の origin を read state として通知する（👀 を返信の前に付ける）。
                    for folded in source.poll_new_with_origin() {
                        let crate::FoldedInbound { text, origin } = folded;
                        tracing::info!(
                            iteration = iterations,
                            bytes = text.len(),
                            "injecting newly arrived user speech into the running turn"
                        );
                        messages.push(Message {
                            role: Role::User,
                            content: Some(MessageContent::Text(text.clone())),
                            name: None,
                            function_call: None,
                            tool_calls: None,
                            tool_call_id: None,
                        });
                        turn_ledger.record(format!("live:{}", messages.len()), &text);
                        // #930: この said を読んだ時点で read（👀）を付ける。1 origin 1 回。
                        if let Some(origin) = origin {
                            if read_emitted_origins.insert(origin.clone()) {
                                if let Some(cb) = &self.on_folded_origin {
                                    cb(origin).await;
                                }
                            }
                        }
                    }
                }
            }
            apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;

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

            // §2.7: describe_tools でこのターンに活性化したツールを次イテレーションの関数集合へ
            // 反映するため、毎イテレーション list_tools を取り直す（階層化しても depth>0 なら
            // 常に同じ集合を返すので従来挙動と等価）。
            let tools = self.executor.list_tools();

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
            let call_failure = classify_call_failure(&llm_result, &model, self.max_output_tokens);

            if let Some(cb) = &self.log_callback {
                cb(&LlmCallLog {
                    request: request_for_log.clone(),
                    response: llm_result.as_ref().ok().cloned(),
                    error_str: call_failure.as_ref().map(|failure| failure.body.clone()),
                    error_code: call_failure.as_ref().map(|failure| failure.code.clone()),
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
            if let Some(run_helpers::CallFailure { code, body }) = call_failure {
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
            let normalized = normalize_response(&response);
            let mut content = normalized.content;
            let tool_calls = normalized.tool_calls;

            // If the LLM returned no structured tool calls but embedded
            // <function_calls> XML in the content (e.g. DeepSeek via OpenRouter),
            // parse them out and treat them as normal tool calls.
            if normalized.xml_tool_count > 0 {
                // 発火は harness 剪定の判断材料として計測する（EngineResult 経由で
                // agent_logs にも記録される）。codex プロバイダは意図的にこの
                // フォールバックへ依存するため、発火＝異常ではない（毎イテレーション
                // 発火し得るのでログは debug に留め、run 単位の集計を agent_logs で見る）。
                xml_fallback_parses += 1;
                tracing::debug!(
                    count = normalized.xml_tool_count,
                    model = %model,
                    "Parsed XML function_calls from content (harness fallback fired)"
                );
            }

            // #890 §11 / §11.7: content の最終行が CONTINUE 単独なら「このターンを続ける意思」と
            // みなし、その行を剥がして次イテレーションへ進む（継続を起こすのは text-only 経路のみ・
            // 下の最終応答分岐で `continue`）。ツール呼び出しと併記された場合はツール経路が優先し、
            // マーカーは剥がすだけ。NO_REPLY が同居する場合は NO_REPLY 優先で終端する（継続しない・
            // 剥がしは配送層が担う）。同一行併記・途中出現は継続もしない（WARN は配送層が出す）。
            // 剥がしは on_response_text 配送前・会話保存前に行う（§11.6: マーカーを残さない）。
            let (stripped_content, continue_requested) = strip_continue_marker(content);
            content = stripped_content;

            // #898 §13.1 a: 空 CONTINUE（本文なし・継続）の連鎖を数える。3 連続で解析 warn を 1 行
            // 出す（停止はしない・上限は既存 max_iterations）。非空生成・非継続でリセットする。
            if continue_requested && content.as_deref().map(str::trim).unwrap_or("").is_empty() {
                consecutive_empty_continue += 1;
                if consecutive_empty_continue == 3 {
                    tracing::warn!(
                        target: crate::continue_marker::CONTINUE_LOG_TARGET,
                        iteration = iterations,
                        "空 CONTINUE 連続 3 回（解析用・停止しない・§13.1 a）"
                    );
                }
            } else {
                consecutive_empty_continue = 0;
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

            // If there are tool calls, execute them. A generation containing only allowed
            // utterance calls completes the turn without another LLM call; query/tool calls
            // still produce tool results and continue the loop.
            if !tool_calls.is_empty() {
                // 発話クラスだけで完結した生成は、普通の発話と同じく 1 生成で終了する（R7・
                // row360 / #880）。照会/道具、または permission denied の発話が 1 つでもあれば
                // provider の tool_call/tool_result 対を作って次の LLM 呼び出しへ進む。
                let next_llm_call_needed = tool_calls.iter().any(|tc| {
                    !self.is_utterance_tool(&tc.function.name)
                        || !self.is_action_allowed(&tc.function.name)
                });

                // #900: 純発話でも末尾 CONTINUE が併記されていれば、発話を配送してから次イテレー
                // ションへ進む（発話クラスのみ＋末尾 CONTINUE → 継続）。この場合は下の混在パスへ落とし、
                // 各発話を最小 ack で満たして次の LLM 呼び出しを起こす（本文＝マーカー剥がし済みの content）。
                if !next_llm_call_needed && !continue_requested {
                    for tool_call in &tool_calls {
                        total_tool_calls += 1;
                        let tool_name = &tool_call.function.name;
                        let args = tool_call.arguments_json();
                        let result = self
                            .executor
                            .execute_with_id(tool_name, &args, &tool_call.id)
                            .await;
                        if tool_name == "reply" && result.success {
                            last_posting_utterance_id = Some(tool_call.id.clone());
                        }
                        tracing::debug!(
                            iteration = iterations,
                            tool = %tool_name,
                            id = %tool_call.id,
                            stage = "utterance",
                            "turn: 純発話生成を配送（1 生成で完結・機械行なし）"
                        );
                    }
                    return Ok(EngineResult {
                        response: content.unwrap_or_default(),
                        iterations,
                        tool_calls_made: total_tool_calls,
                        stopped_by_limit: false,
                        last_posting_utterance_id,
                        last_generation_had_continuation_speech,
                        xml_fallback_parses,
                    });
                }

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
                turn_ledger.record(
                    format!("asst:{}", messages.len()),
                    content.as_deref().unwrap_or(""),
                );
                apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;

                // 発話クラス（reply/reaction/repost・§3.3.1 C6）の tool_call は**機械行を
                // 永続しない**。発話の本文は配送経路が speech ログとして残す（本文＋関係注記）
                // ので、ここで永続 tool_call 行から除外する。照会/道具クラスの call は従来どおり。
                let persisted: Vec<&ToolCall> = tool_calls
                    .iter()
                    .filter(|tc| !self.is_utterance_tool(&tc.function.name))
                    .collect();

                // #916 §13 #10: 本文＋照会/道具（query/dispatch）クラスの生成の本文は「宣言（holding）」。
                // 既存の中間発話配送フック（on_continuation_speech → 配送＋保存）で 1 件だけ配送・保存する。
                // 配送したら on_tool_call へは本文を渡さず二重保存を避ける（配送は保存と対）。フックが
                // 無いレーン（旧 discord は on_response_text で反復配送・core 単体テストは配送なし）は
                // 従来どおり on_tool_call が本文を保存する（挙動不変）。content は末尾 CONTINUE 剥がし済み。
                // 配送する holding 本文は NO_REPLY 終端解釈後の可視発言。判定は core 単一実装
                // terminate_at_no_reply().speech()（配送層 visible_speech_after_markers と同じ 1 実装・
                // 部分文字列の別判定を作らない・#916 レビュー）。content は末尾 CONTINUE 剥がし済み
                // （§11.6）なので visible_speech_after_markers（NO_REPLY→CONTINUE 剥がし）と同一結果。
                // 沈黙（可視本文なし・単独/行頭 NO_REPLY）は配送しない。
                let mut holding_delivered = false;
                if !persisted.is_empty() {
                    if let Some(c) = content.as_deref() {
                        let term = crate::continue_marker::terminate_at_no_reply(c);
                        if let Some(body) = term.speech().filter(|b| !b.trim().is_empty()) {
                            if let Some(ref cb) = self.on_continuation_speech {
                                cb(body.to_string()).await.map_err(|e| {
                                    anyhow::anyhow!("holding speech delivery failed: {e:#}")
                                })?;
                                holding_delivered = true;
                            }
                        }
                    }
                }

                // Notify on_tool_call callbacks.
                if !persisted.is_empty() && !self.on_tool_call.is_empty() {
                    let calls_json = serde_json::to_string(&persisted).unwrap_or_default();
                    // 配送済み holding は本文を渡さない（配送側が保存済み・二重保存回避）。未配送
                    // （フック無しレーン）は従来どおり content を渡して on_tool_call が保存する。
                    let assistant_content = if holding_delivered {
                        String::new()
                    } else {
                        content.clone().unwrap_or_default()
                    };
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
                let dispatch_partition = partition_tool_calls_for_dispatch(
                    &tool_calls,
                    self.tool_dispatcher.as_deref(),
                    |tool_name| self.is_action_allowed(tool_name),
                );
                if !dispatch_partition.forced_inline.is_empty() {
                    // dispatch 可の後ろに inline ツール → 分割不可、全体 inline
                    // に縮退。縮退原因（first より後ろの inline ツール）を明示。
                    // 相関 ID（agent_id / session_id / turn_id）は #665 の span
                    // から継承する。
                    let forced: Vec<&str> = dispatch_partition
                        .forced_inline
                        .iter()
                        .map(|(_, tool_name)| *tool_name)
                        .collect();
                    tracing::debug!(
                        iteration = iterations,
                        stage = "batch_split",
                        tools = tool_calls.len(),
                        inline_tools = %forced.join(","),
                        "turn: 混在バッチが全体 inline に縮退（dispatch 可の後ろに inline ツール）"
                    );
                }
                let dispatch_start = dispatch_partition.dispatch_start;

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
                        turn_ledger.record(format!("tool:{}", messages.len()), &result_json);
                        apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;

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

                    // 照会/道具と混在した発話クラス（§3.3.1 C3/C6）: inline 配送するが、
                    // subtask/settle/resume は起こさず、モデルへ領収書本文を返さない。次の LLM
                    // 呼び出しが不可避な混在時だけ、provider の tool_call/tool_result 対要求を
                    // データを持たない最小 ack で満たす（R7）。on_tool_result（永続機械行）は
                    // 呼ばず、本文は配送経路が speech として永続する（C6）。
                    if self.is_utterance_tool(tool_name) {
                        let result = self
                            .executor
                            .execute_with_id(tool_name, &args, &tool_call.id)
                            .await;
                        if tool_name == "reply" && result.success {
                            last_posting_utterance_id = Some(tool_call.id.clone());
                        }
                        // 最小 ack（データを持たない空オブジェクト・capping 不要）。成功/失敗を
                        // 名乗らない——失敗は say と同一経路で ❌/turn_failed に別途表面化する（C9）。
                        let ack = "{}".to_string();
                        messages.push(Message::tool(tool_call.id.clone(), ack.clone()));
                        turn_ledger.record(format!("tool:{}", messages.len()), &ack);
                        apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;
                        tracing::debug!(
                            iteration = iterations,
                            tool = %tool_name,
                            id = %tool_call.id,
                            stage = "utterance",
                            "turn: 発話クラスを配送（撃ちっぱなし・機械行なし）"
                        );
                        continue;
                    }

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
                    let result_json = seat_tool_result(
                        &mut turn_gov,
                        &mut turn_ledger,
                        &mut messages,
                        tool_name,
                        &result_json,
                        |remaining| {
                            self.cap_tool_result(
                                tool_name,
                                &tool_call.id,
                                result_json.clone(),
                                remaining,
                            )
                        },
                    )?;

                    messages.push(Message::tool(tool_call.id.clone(), result_json.clone()));
                    turn_ledger.record(format!("tool:{}", messages.len()), &result_json);
                    apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;

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
                        turn_ledger.record(format!("tool:{}", messages.len()), &result_json);
                        apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;
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

                // #898 §13 #8: 発話クラスのみ＋末尾 CONTINUE で継続するとき、併記された本文
                // （content・マーカー剥がし済み）を reply 配送のあと・次イテレーション前に、継続分岐と
                // 同じフックで配送・保存する（extgate 途中発話配送 / memory_sessions speech / REST
                // responses / intake 保存）。会話文脈は上の assistant メッセージ（tool_calls＋content）で
                // 積み済みなのでここでは配送・保存だけ。配送失敗（Err）は継続を止める（§13.1 j）。
                // 照会/道具が混じる（next_llm_call_needed）ときは本文 say を配送しない（holding は従来経路）。
                if continue_requested && !next_llm_call_needed {
                    if let Some(ref c) = content {
                        if !c.trim().is_empty() {
                            last_generation_had_continuation_speech = true;
                            if let Some(ref cb) = self.on_continuation_speech {
                                cb(c.clone()).await.map_err(|e| {
                                    anyhow::anyhow!("continuation speech delivery failed: {e:#}")
                                })?;
                            }
                        }
                    }
                }

                continue;
            }

            // #890 §11: 末尾 CONTINUE でこのターンを継続（ツール呼び出しが無い text-only 経路）。
            // 剥がし後の本文を assistant メッセージとして積み（マーカー除去済み・§11.6）、次イテレー
            // ションへ。本文が空（CONTINUE 単独）なら何も積まずに次イテレーションへ。上限は既存
            // max_iterations。
            if continue_requested {
                if let Some(ref c) = content {
                    // #898 §12.2/§13.1 j: 剥がし後の途中発話を、次イテレーション前に**ループ中で
                    // 配送・保存する**（REST responses への追加 / extgate 途中発話配送 / memory_sessions
                    // speech 保存 / intake 保存）。配送が失敗したら継続を止めてターンを失敗させる
                    // （失敗を隠して次に進まない）。on_response_text は最終・text+tool でも発火する
                    // ため区別できず流用しない（最終二重配送・text+tool 二重保存を避ける）。
                    last_generation_had_continuation_speech = true;
                    if let Some(ref cb) = self.on_continuation_speech {
                        cb(c.clone()).await.map_err(|e| {
                            anyhow::anyhow!("continuation speech delivery failed: {e:#}")
                        })?;
                    }
                    messages.push(Message {
                        role: Role::Assistant,
                        content: Some(MessageContent::Text(c.clone())),
                        name: None,
                        function_call: None,
                        tool_calls: None,
                        tool_call_id: None,
                    });
                    turn_ledger.record(format!("asst:{}", messages.len()), c);
                    apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;
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
                last_posting_utterance_id,
                last_generation_had_continuation_speech,
                xml_fallback_parses,
            });
        }
    }
}

#[cfg(test)]
#[path = "skill_engine/tests/mod.rs"]
mod tests;

/// 走行中ターンへの新着ユーザー発言の注入（#289）。
///
/// 会話履歴はターン開始時に 1 度だけ組まれるため、ツール往復が長引くとその間に届いた
/// 発言が次ターンまで入力に載らなかった。ここではエンジン側の契約
/// （いつ引くか / 何を積むか / 何もしない条件）だけを検査する。
#[cfg(test)]
mod live_inbound_tests {
    include!("skill_engine/live_inbound_tests.rs");
}
