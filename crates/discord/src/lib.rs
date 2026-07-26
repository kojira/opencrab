//! Discord integration crate for OpenCrab.
//!
//! Provides Discord gateway actions, message processing loop, and per-agent bot management.
//! All Discord-specific logic lives here, keeping the server crate Discord-free.

pub mod form_modal;
pub mod gateway_actions;
pub mod manager;
pub mod message_loop;
pub mod owner_warning;
pub mod renderer;
pub mod voice_session;

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;

pub use gateway_actions::DiscordGatewayActions;
pub use gateway_actions::{spawn_activity_tool_event_sink, WebhookConfig};
pub use manager::DiscordGatewayManager;
pub use message_loop::run_discord_loop;
pub use owner_warning::{
    gateway_will_start, warn_if_agent_gateway_owner_unset, warn_if_shared_gateway_owner_unset,
};
pub use renderer::DiscordRenderer;

/// A2UI pending interaction registry type.
pub type PendingInteractionRegistry = Arc<DashMap<String, PendingInteraction>>;

/// A pending A2UI interaction waiting for user response.
pub struct PendingInteraction {
    pub session_id: String,
    pub agent_id: String,
    pub channel_id: u64,
    pub channel_id_str: String,
    /// ギルドID（DMの場合は空文字列）。送信時には不明なことがあるため空がデフォルト。
    pub guild_id: String,
    pub is_dm: bool,
    pub surface_id: String,
    pub a2ui_components: Vec<opencrab_core::a2ui::A2uiComponent>,
    pub owner_discord_id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub timeout_secs: u64,
    pub rendered_message: opencrab_core::a2ui::RenderedMessage,
    pub event_tx: tokio::sync::mpsc::UnboundedSender<message_loop::LoopEvent>,
    /// Formコンポーネント情報（モーダル用）。Button押下時にModalを表示するために保持。
    pub form_data: Option<FormData>,
}

/// Form/Modal表示に必要なデータ
pub struct FormData {
    /// Modal custom_id（形式: `interaction:{uuid}:modal:{form_action_name}`）
    pub modal_custom_id: String,
    /// Modalタイトル
    pub title: String,
    /// Modal用ActionRows（CreateInputTextの配列）
    pub action_rows: Vec<serenity::all::CreateActionRow>,
    /// Submit時のアクション
    pub action: opencrab_core::a2ui::A2uiAction,
}

/// Discord 応答の記録コンテキスト（#42: ターン記録の集約）。
///
/// metadata の `triggered_by` 等の差分を型で表現する。記録の実体（SessionLogRow の
/// 組み立てと書き込みポリシー）は server 側の transcript モジュールが所有する。
#[derive(Debug, Clone)]
pub enum DiscordReplyContext<'a> {
    /// 新着メッセージへの直接応答。
    Direct { tool_calls_made: usize },
    /// サブタスク完了を受けた再呼び出しの応答。
    SubtaskCompleted,
    /// A2UI インタラクション応答を受けた再呼び出しの応答。
    InteractionResponse { interaction_id: &'a str },
}

/// A2UI インタラクションの記録内容（#42）。
pub struct InteractionRecord<'a> {
    pub interaction_id: &'a str,
    pub surface_id: &'a str,
    pub action_name: &'a str,
    pub component_id: &'a str,
    pub responder_id: &'a str,
    /// session_logs の content に書く整形済みテキスト。
    pub content: &'a str,
}

/// Trait abstracting the server-side agent processing pipeline.
///
/// Defined here (in the discord crate) to break the circular dependency:
/// discord needs to invoke agent processing, but server depends on discord.
/// Server implements this trait for its `AppState`.
///
/// メソッドは意図レベル（記録・判定・セッション管理）で切る（#43）。discord 側が
/// 生の SQL（`opencrab_db::queries::*`）を直接叩くことは、`db()` を使う
/// ゲートウェイアクション**構築**を除き禁止。
#[async_trait]
pub trait AgentRunner: Send + Sync + Clone + 'static {
    /// Access the shared database connection.
    ///
    /// **構築専用**（DiscordGatewayActions 等のコンポーネント配線のみに使う）。
    /// メッセージ処理ロジックからの直接クエリには使わないこと（#43 — ストレージ
    /// への結合を構築時の1点に限定する）。
    fn db(&self) -> &opencrab_db::Db;

    /// Access the shared tools configuration.
    fn tools_config(&self) -> &Arc<std::sync::RwLock<opencrab_actions::tools::ToolsConfig>>;

    /// Whether any LLM providers are configured.
    fn has_llm_providers(&self) -> bool;

    /// Build the agent's system prompt and name from DB.
    ///
    /// Returns `(system_prompt, agent_name)`.
    fn build_agent_context(&self, agent_id: &str) -> (String, String);

    /// Build the conversation history string for a session (with compaction).
    fn build_conversation_string(
        &self,
        session_id: &str,
        agent_id: &str,
        context_budget_tokens: usize,
    ) -> Result<String, anyhow::Error>;

    /// Run the full agent response pipeline (SkillEngine + LLM).
    /// 実行要求は `RunRequest`（#33）で受ける。
    async fn run_agent_response(
        &self,
        req: opencrab_actions::RunRequest,
    ) -> anyhow::Result<opencrab_core::EngineResult>;

    /// エージェントのLLMクライアントを生成する。
    fn create_llm_client(&self) -> Arc<dyn opencrab_core::LlmClient>;

    /// デフォルトモデル名を返す（"provider:model" 形式）。
    fn default_model(&self) -> String;

    /// 会話コンテキストのトークン予算を返す（context_window * compaction_ratio）。
    /// `agent_id` の per-agent モデルに応じた pricing を参照する。
    fn context_budget_tokens(&self, agent_id: &str) -> usize;

    /// ワークスペースベースパスを返す（例: "/data/workspace/{agent_id}"）。
    fn workspace_base(&self) -> &str;

    // ---- 転記（#42: ターン記録の集約。実装は server の transcript モジュール） ----

    /// Discord ユーザー発言をセッションログに記録する（best-effort）。
    #[allow(clippy::too_many_arguments)]
    fn record_user_message(
        &self,
        session_id: &str,
        sender_id: &str,
        sender_name: &str,
        avatar_url: Option<&str>,
        channel_id: &str,
        text: &str,
        image_urls: &[String],
    );

    /// NO_REPLY（沈黙の明示）を記録する（best-effort）。
    fn record_agent_no_reply(&self, agent_id: &str, session_id: &str);

    /// エージェントの Discord 応答を記録する（best-effort）。
    fn record_agent_reply(
        &self,
        agent_id: &str,
        session_id: &str,
        channel_id: &str,
        text: &str,
        context: DiscordReplyContext<'_>,
    );

    /// A2UI インタラクション応答をセッションログに記録する（best-effort）。
    fn record_interaction_response(
        &self,
        agent_id: &str,
        session_id: &str,
        record: InteractionRecord<'_>,
    );

    // ---- 判定（#43: trust / channel ポリシー） ----

    /// チャンネルが書き込み可か。DB 不可時は fail-closed（false）。
    fn is_channel_writable(&self, channel_id: &str) -> bool;

    /// チャンネルがこのエージェントのホワイトリストにあるか。fail-closed。
    fn is_channel_whitelisted_for_agent(&self, channel_id: &str, agent_id: &str) -> bool;

    /// DM を受け付けるか（いずれかのエージェントが信頼していれば true の事前ゲート）。
    /// owner は常に許可。DB 不可時は fail-closed。
    fn dm_allowed_any(&self, sender_id: &str, agent_ids: &[String], owner_discord_id: &str)
        -> bool;

    /// DM を受け付けるか（エージェント個別ゲート）。owner は常に許可。fail-closed。
    fn dm_allowed(&self, sender_id: &str, agent_id: &str, owner_discord_id: &str) -> bool;

    /// 送信者の CallerIdentity を解決する（owner > trusted_users の permission > Agent）。
    fn resolve_caller(
        &self,
        sender_id: &str,
        agent_ids: &[String],
        owner_discord_id: &str,
    ) -> opencrab_actions::CallerIdentity;

    // ---- セッション/インタラクション管理（#43） ----

    /// セッションが無ければ作成し、あれば metadata 未設定時のみ補完する（best-effort）。
    fn ensure_session(
        &self,
        session_id: &str,
        agent_ids: &[String],
        theme: &str,
        metadata_json: &str,
    );

    /// セッションの theme を返す（無ければ None）。
    fn session_theme(&self, session_id: &str) -> Option<String>;

    /// pending interaction のステータスを更新する（best-effort）。
    fn mark_interaction_status(
        &self,
        interaction_id: &str,
        status: &str,
        response_json: Option<&str>,
        responder_id: Option<&str>,
    );

    /// 古い pending interaction を掃除する（起動時）。
    fn cleanup_stale_interactions(&self);

    /// 有効な per-agent Discord 設定の一覧。
    fn list_enabled_discord_configs(&self) -> Vec<opencrab_db::queries::AgentDiscordConfigRow>;

    /// このエージェント専用の per-agent Discord ゲートウェイが**実際に稼働中**か。
    ///
    /// 共有（TOML）ゲートウェイ側の二重処理防止（#40）に使う。判定は DB の enabled
    /// フラグではなくゲートウェイの生死（manager の liveness）で行う: enabled=1 でも
    /// 起動失敗（無効トークン等）していれば false を返し、共有側がフォールバックとして
    /// 処理を続ける（エージェントがどのゲートウェイからも応答しない状態を作らない）。
    /// 優先順位ルールは docs/config-precedence.md 参照。
    fn served_by_dedicated_gateway(&self, agent_id: &str) -> bool;
}
