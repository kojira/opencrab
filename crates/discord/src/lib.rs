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

pub use gateway_actions::DiscordGatewayActions;
// 通知先（webhook）の設定型は gateway 非依存層（`opencrab_actions::webhook_target`）が
// 保持する（#157 S4）。Discord crate は re-export せず、他 crate が Discord crate 経由で
// 通知先の型を引かないようにする（#170 と同じ方針）。
pub use gateway_actions::{spawn_activity_tool_event_sink, DiscordWebhookNotifier};
pub use manager::DiscordGatewayManager;
pub use message_loop::run_discord_loop;
pub use owner_warning::{
    gateway_will_start, warn_if_agent_gateway_owner_unset, warn_if_shared_gateway_owner_unset,
};
pub use renderer::DiscordRenderer;

// A2UI の保留状態（`PendingInteraction` / `PendingInteractionRegistry`）と Form の
// 描画物（旧 `FormData`）は **gateway 非依存層**（`opencrab_core::a2ui`）へ移設済み
// （#156 S3）。旧実装は保留状態に Discord のイベントループへ送るチャンネル
// （`UnboundedSender<LoopEvent>`）・チャンネル/ギルド識別子・DM 判定・serenity の
// モーダル描画物を直に埋めていたため、汎用層に置けなかった。
//
// 置き換え:
// - 応答の戻し先 → `opencrab_core::a2ui::UiResponseSink`（Discord 実装は
//   `gateway_actions::ui::DiscordUiResponseSink`）
// - チャンネル識別子 → `opencrab_core::a2ui::RenderTarget`
// - モーダル描画物 → **保持しない**。ボタン押下時に保留状態の部品ツリーから組み直す
//   （`form_modal::resolve_form_modal_for_button`）。コアが serenity の型を知らずに済む。
//
// #170 と同じ方針で **re-export しない**（他 crate が Discord crate 経由でコアの型を
// 引かないようにする）。

// 転記の語彙 — 応答の起動要因を表す旧 `DiscordReplyContext` と A2UI 応答の記録内容を
// 表す旧 `InteractionRecord` — は **gateway 非依存層**（`opencrab_actions::transcript`）
// へ移設済み（#158 S3）。どちらも中身は文字列と数値だけで transport 依存の型を含んで
// いなかったのに、server の転記モジュールが discord crate の型を引くため、記録の関数が
// `#[cfg(feature = "discord")]` 配下に落ちていた（discord を切ると Nostr と同じ形の
// 記録まで消える）。
//
// 置き換え:
// - `DiscordReplyContext` → `opencrab_actions::AgentReplyContext`
// - `InteractionRecord` → `opencrab_actions::InteractionRecord`
// - 記録メソッド → `opencrab_actions::AgentRuntime::record_inbound_message` /
//   `record_outbound_reply` / `record_interaction_response`
//
// #170 と同じ方針で **re-export しない**。

/// Trait abstracting the server-side agent processing pipeline.
///
/// Defined here (in the discord crate) to break the circular dependency:
/// discord needs to invoke agent processing, but server depends on discord.
/// Server implements this trait for its `AppState`.
///
/// メソッドは意図レベル（記録・判定・セッション管理）で切る（#43）。discord 側が
/// 生の SQL（`opencrab_db::queries::*`）を直接叩くことは、`db()` を使う
/// ゲートウェイアクション**構築**を除き禁止。
///
/// ゲートウェイの語彙を含まないメソッド（応答生成・会話履歴・トークン予算・
/// セッション/インタラクション管理）は [`opencrab_actions::AgentRuntime`] が持つ（#156 S1）。
/// ここには Discord の語彙（チャンネル・DM・per-agent ゲートウェイ）を含むものだけを残す。
pub trait AgentRunner: opencrab_actions::AgentRuntime {
    /// Access the shared database connection.
    ///
    /// **構築専用**（DiscordGatewayActions 等のコンポーネント配線のみに使う）。
    /// メッセージ処理ロジックからの直接クエリには使わないこと（#43 — ストレージ
    /// への結合を構築時の1点に限定する）。
    fn db(&self) -> &opencrab_db::Db;

    /// ワークスペースベースパスを返す（例: "/data/workspace/{agent_id}"）。
    fn workspace_base(&self) -> &str;

    // 転記（#42: ターン記録の集約）は [`opencrab_actions::AgentRuntime`] が持つ
    // （`record_inbound_message` / `record_outbound_reply` /
    // `record_interaction_response` — #158 S3）。

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

    // ---- per-agent ゲートウェイ（#40） ----

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
