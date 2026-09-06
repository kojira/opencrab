use std::sync::Arc;

mod run;
mod run_helpers;
mod turn_budget;

use anyhow::Result;
use tracing;

#[cfg(test)]
use super::types::ChatRequest;
use super::types::{
    self, ActionExecutor, ActionResult, LiveInboundSource, LlmCallLog, LlmClient, ToolDispatcher,
};
#[cfg(test)]
use opencrab_llm_types::FinishReason;
#[cfg(test)]
use opencrab_llm_types::{Message, Role, ToolCall};
#[cfg(test)]
use turn_budget::{apply_turn_budget, message_plain_text, seat_tool_result, user_line_items};

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
