//! DiscordゲートウェイアクションのGatewayActions実装
//!
//! Discord API を叩く配送（一覧・リアクション・webhook・ファイル・voice）を提供する。
//! チャンネル設定の書き込み判断は core（`apply_discord_channel_config`）。

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use opencrab_gateway::{GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext};
use serde_json::json;
use serenity::http::Http;

use crate::message_loop::LoopEvent;
use opencrab_core::a2ui::PendingInteractionRegistry;

mod discord_ops;
mod subtask_engine;
mod subtask_notifier;
mod subtask_webhook;
mod text_delivery;
mod ui;
mod voice_actions;
mod webhook;

// ピアレビューは**依頼側も回収側も** gateway 非依存層（`crates/server/src/peer_review.rs`）
// にある。依頼は #157 S7、返信の回収は #156 S4 で移設した。回収の呼び出し口は
// `opencrab_actions::AgentRuntime::on_inbound_message`（受信ループから呼ぶ共通フック）。
// Discord に残るピアレビュー関連は配送口（`text_delivery::DiscordTextDelivery`）だけ。
pub use subtask_engine::spawn_activity_tool_event_sink;
pub(crate) use subtask_engine::DiscordCompletionSink;
pub use subtask_notifier::DiscordWebhookNotifier;

// 走行中 subtask の registry / エントリ型は actions の gateway 非依存版へ移設済み
// （RFC #152 S1）。#157 S2 で停止処理も移設したため、この gateway はもう registry も
// lifecycle 通知口マップも保持しない（型を import する必要すら無くなった）。
//
// 通知先（webhook）の設定型も同様に gateway 非依存層が保持する（#157 S4）。こちらは
// `DiscordGatewayActions` が env/config 由来のフォールバックとして保持し続けるため、
// re-export せず型だけを参照する（他 crate が Discord crate 経由で引かないように）。
use opencrab_actions::webhook_target::WebhookConfig;

/// Discord固有のゲートウェイアクション実装。
///
/// serenityのHTTPクライアントとDB接続を保持し、
/// Discord管理操作をGatewayActionsとして提供する。
///
/// Clone は全フィールドが Arc/ハンドルの共有クローンで、event_tx / db を**共有**する。
///
/// subtask の登録簿（`SubtaskRegistry`）と lifecycle 通知口マップ（`SubtaskNotifiers`）は
/// **もう保持しない**（#157 S2）。停止（`cancel_subtask`）が gateway 非依存層
/// （`opencrab_actions::cancel_subtask`）だけの実装になり、Discord 側から両方を参照する
/// 理由が無くなったため。所有者は server 側（`AppState` / message_loop）。
#[derive(Clone)]
pub struct DiscordGatewayActions {
    http: Arc<Http>,
    db: opencrab_db::Db,
    /// ワークスペースのベーステンプレート（例: "/data/workspace/{agent_id}"）。
    /// エージェントごとの root は `agent_workspace_root(&ctx.agent_id)` で展開する。
    workspace_base: String,
    /// spawn_subtask.webhook 省略時に使うデフォルト lifecycle webhook
    /// （`get/set_default_subtask_webhook` の解決に使う）。
    default_subtask_webhook: Option<WebhookConfig>,
    /// A2UI の保留インタラクション登録簿（コアの型 / #156 S3）。
    /// `send_ui` の実体は gateway 非依存層にあり、この gateway は
    /// `a2ui_surface()` で登録簿と受け口をそちらへ渡すだけ。
    pub pending_interaction_registry: Option<PendingInteractionRegistry>,
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<LoopEvent>>,
    /// owner-only な A2UI インタラクションの権限判定に使う owner の Discord ユーザーID。
    /// 空文字の場合は owner 判定が無効（誰でも操作可）になる点に注意。
    pub owner_discord_id: String,
    /// VC 対話（STT/TTS）。config の [voice] が有効なときのみ Some。
    pub voice: Option<Arc<crate::voice_session::VoiceSessionManager>>,
}

impl DiscordGatewayActions {
    pub fn new(
        http: Arc<Http>,
        db: opencrab_db::Db,
        workspace_base: String,
        default_subtask_webhook: Option<WebhookConfig>,
    ) -> Self {
        Self {
            http,
            db,
            workspace_base,
            default_subtask_webhook,
            pending_interaction_registry: None,
            event_tx: None,
            owner_discord_id: String::new(),
            voice: None,
        }
    }

    /// Bot トークンから組み立てる。serenity の `Http` の構築をこの中に閉じるので、
    /// 呼び出し側は SDK（serenity）の型を持たなくてよい。`Http::new` は接続せず、
    /// 実際に Discord API を叩くのは送信時なので、トークンから組むのは自然な使い方。
    ///
    /// 既に `Arc<Http>` を握っている経路（稼働中の [`crate::DiscordGateway`] から
    /// `http()` を借りるなど）は [`DiscordGatewayActions::new`] を使う。
    pub fn from_token(
        token: &str,
        db: opencrab_db::Db,
        workspace_base: String,
        default_subtask_webhook: Option<WebhookConfig>,
    ) -> Self {
        Self::new(
            Arc::new(Http::new(token)),
            db,
            workspace_base,
            default_subtask_webhook,
        )
    }

    /// エージェントのワークスペース root（ベーステンプレートの {agent_id} を展開）。
    fn agent_workspace_root(&self, agent_id: &str) -> anyhow::Result<PathBuf> {
        // 展開は core の型付きリゾルバに一本化（agent_id 検証込み — #48）。
        opencrab_core::workspace::resolve_agent_workspace(&self.workspace_base, agent_id)
    }

    /// Set the event sender only (no A2UI pending-interaction registry).
    ///
    /// subtask 完了/進捗の通知はこの sender 経由でイベントループへ届くため、
    /// run_discord_loop と組む構築では必ずどちらか（with_a2ui / with_event_tx）で
    /// event_tx を配線すること（未配線だと通知が発火しない）。
    pub fn with_event_tx(
        mut self,
        event_tx: tokio::sync::mpsc::UnboundedSender<LoopEvent>,
    ) -> Self {
        self.event_tx = Some(event_tx);
        self
    }

    /// Set the pending interaction registry and event sender for A2UI support.
    pub fn with_a2ui(
        mut self,
        registry: PendingInteractionRegistry,
        event_tx: tokio::sync::mpsc::UnboundedSender<LoopEvent>,
    ) -> Self {
        self.pending_interaction_registry = Some(registry);
        self.event_tx = Some(event_tx);
        self
    }

    /// Set the owner's Discord user id used to enforce owner-only A2UI interactions.
    pub fn with_owner_discord_id(mut self, owner_discord_id: impl Into<String>) -> Self {
        self.owner_discord_id = owner_discord_id.into();
        self
    }

    /// VC 対話マネージャを接続する（config の [voice] が有効なとき）。
    pub fn with_voice(mut self, voice: Arc<crate::voice_session::VoiceSessionManager>) -> Self {
        self.voice = Some(voice);
        self
    }
}

/// キー名が Discord ID を表すか（`channel_id` / `guild_id` / `message_id` など）。
fn is_id_key(key: &str) -> bool {
    key.ends_with("_id")
}

/// JSON 整数値を精度を保ったまま文字列へ。Discord のスノーフレークは 18–19 桁で
/// 2^53 を超えるため f64 では壊れるが、serde_json は整数リテラルを i64/u64 として
/// 保持するので `as_u64`/`as_i64` 経由なら正確。非整数（文字列・小数・真偽）は None。
fn id_number_to_string(v: &serde_json::Value) -> Option<String> {
    v.as_u64()
        .map(|u| u.to_string())
        .or_else(|| v.as_i64().map(|i| i.to_string()))
}

/// 実行前に `*_id` の整数値を文字列へ正規化する。
///
/// モデルは Discord ID を JSON 文字列ではなく JSON 数値で渡すことが多いが、各
/// ハンドラは `as_str()` だけを見ているため「channel_id パラメータが必要です」と
/// 誤って失敗していた。ここで数値 ID を文字列化して吸収する（変換が不要なら
/// 借用のまま返し、余計なコピーをしない）。トップレベルのオブジェクトのみ対象
/// （Discord アクションの ID 引数はすべてフラット）。
fn normalize_id_args(args: &serde_json::Value) -> Cow<'_, serde_json::Value> {
    let serde_json::Value::Object(map) = args else {
        return Cow::Borrowed(args);
    };
    let needs = map
        .iter()
        .any(|(k, v)| is_id_key(k) && id_number_to_string(v).is_some());
    if !needs {
        return Cow::Borrowed(args);
    }
    let mut out = map.clone();
    for (k, v) in out.iter_mut() {
        if is_id_key(k) {
            if let Some(s) = id_number_to_string(v) {
                *v = serde_json::Value::String(s);
            }
        }
    }
    Cow::Owned(serde_json::Value::Object(out))
}

#[async_trait]
impl GatewayActions for DiscordGatewayActions {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        vec![
            GatewayActionDef {
                name: "discord_list_guilds".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::Blocked, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "Botが参加しているDiscordサーバー（guild）の一覧を取得する。返り値の各サーバーの `id` フィールド（数値文字列）を、他のアクションの `guild_id` パラメータとして使用すること。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "discord_list_channels".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::Blocked, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "指定サーバーのテキストチャンネル一覧と、各チャンネルの現在のreadable/writable/whitelisted設定を取得する。チャンネルの `id` フィールドを discord_channel_config の channel_id として使用すること。guild_id は discord_list_guilds で取得した数値IDを指定。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "guild_id": {
                            "type": "string",
                            "description": "対象サーバーの数値ID（discord_list_guildsの結果から取得）。サーバー名ではなくIDを指定すること。"
                        }
                    },
                    "required": ["guild_id"]
                }),
            },
            GatewayActionDef {
                name: "discord_channel_config".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "Discordチャンネルの読み書き設定を変更する。readableをfalseにするとそのチャンネルのメッセージを無視し、writableをfalseにすると返信しない。whitelisted=trueにするとホワイトリストに登録され、そのチャンネルからのメッセージを優先処理する。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "channel_id": {
                            "type": "string",
                            "description": "対象チャンネルの数値ID（discord_list_channelsの結果から取得）。チャンネル名ではなくIDを指定すること。"
                        },
                        "guild_id": {
                            "type": "string",
                            "description": "チャンネルが属するサーバーの数値ID（discord_list_guildsまたはdiscord_list_channelsの結果から取得）。"
                        },
                        "channel_name": {
                            "type": "string",
                            "description": "チャンネル名（任意・ログ表示用のみ。省略可）。"
                        },
                        "readable": {
                            "type": "boolean",
                            "description": "このチャンネルのメッセージを読み取るか。falseにするとbotはそのチャンネルのメッセージを完全に無視する。"
                        },
                        "writable": {
                            "type": "boolean",
                            "description": "このチャンネルに返信・投稿するか。falseにするとbotはそのチャンネルへの送信を行わない。"
                        },
                        "whitelisted": {
                            "type": "boolean",
                            "description": "このチャンネルをホワイトリストに登録するか（trueにすると優先的に処理される）。デフォルトはfalse。"
                        }
                    },
                    "required": ["channel_id", "guild_id", "readable", "writable"]
                }),
            },
            GatewayActionDef {
                name: "discord_add_reaction".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::Blocked, sharing: opencrab_gateway::ToolSharing::ConversationBound },
                description: "Discordメッセージにリアクション（絵文字）を追加する。Unicode絵文字（例: ⚡）またはカスタム絵文字（name:id形式）を指定できる。テキストで返すほどでもない反応は、これでリアクションだけ付けて応答本文を NO_REPLY にしてよい。複数のリアクションは1回の応答でまとめて呼んでよく、分けて呼び直す必要はない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "channel_id": {
                            "type": "string",
                            "description": "メッセージが存在するチャンネルの数値ID。"
                        },
                        "message_id": {
                            "type": "string",
                            "description": "リアクションを付けるメッセージの数値ID。現在処理中のメッセージのIDを使う場合はコンテキストから取得すること。"
                        },
                        "emoji": {
                            "type": "string",
                            "description": "Unicode絵文字（例: ⚡、👍）またはカスタム絵文字（形式: 絵文字名:数値ID、例: parrot:123456789012345）。Unicode絵文字の場合はそのまま文字列を渡す。"
                        }
                    },
                    "required": ["channel_id", "message_id", "emoji"]
                }),
            },
            GatewayActionDef {
                name: "discord_create_webhook".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "指定したDiscordテキストチャンネルにwebhookを作成し、spawn_subtask.webhook.urlに渡せるURLを返す。Botには対象チャンネルのManage Webhooks権限が必要。返り値のurlは秘密トークンを含むため公開しないこと。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "channel_id": {
                            "type": "string",
                            "description": "webhookを作成するDiscordチャンネルの数値ID。"
                        },
                        "name": {
                            "type": "string",
                            "description": "webhook名（省略時: opencrab-subtask）。2〜80文字。"
                        }
                    },
                    "required": ["channel_id"]
                }),
            },
            GatewayActionDef {
                name: "discord_create_channel".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "指定したDiscordサーバー（guild）に新しいテキストチャンネルを作成する。Botには対象サーバーのManage Channels権限が必要。guild_idは必須で、discord_list_guildsで取得した数値IDを指定すること（このレイヤーではデフォルトサーバーを解決できないため省略不可）。返り値のurlでチャンネルを開ける。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "guild_id": {
                            "type": "string",
                            "description": "チャンネルを作成する対象サーバーの数値ID（discord_list_guildsの結果から取得）。必須。サーバー名ではなくIDを指定すること。"
                        },
                        "name": {
                            "type": "string",
                            "description": "作成するチャンネル名。2〜100文字。"
                        },
                        "parent_id": {
                            "type": "string",
                            "description": "親カテゴリの数値ID（省略可）。指定するとそのカテゴリ配下に作成される。"
                        },
                        "topic": {
                            "type": "string",
                            "description": "チャンネルトピック（省略可・0〜1024文字）。"
                        },
                        "reason": {
                            "type": "string",
                            "description": "Discord監査ログ（Audit Log）に記録する理由（省略時: opencrab discord_create_channel）。"
                        }
                    },
                    "required": ["guild_id", "name"]
                }),
            },
            // `update_memory_index_config` / `add_allowed_command` /
            // `list_allowed_commands` / `remove_allowed_command` は #157 S1 で、
            // `create_skill` は #157 S6 で gateway 非依存層（server 側
            // `SystemGatewayActions`。実体は `crates/server/src/agent_management.rs`）へ
            // 移設済み。いずれも serenity を参照せず DB だけに依存していたのに、Discord
            // 経由のターンにしか出ないのが不具合だった（#157 / #155）。
            // ここで再定義すると合成 gateway の dedup（own 優先）で own 側に食われ、
            // Discord の実装が黙って死ぬので**定義してはならない**
            // （`test_definitions_returns_expected_count` の negative assert が守る）。
            GatewayActionDef {
                name: "discord_send_file".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::Blocked, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "Discordチャンネルにファイル（画像等）をアップロードして送信する。ファイルパスはワークスペース内のパスのみ指定可能（パストラバーサル防止）。25MBサイズ制限あり。".to_string(),
                parameters: json!({
                    "type": "object",
                    "required": ["channel_id", "file_path"],
                    "properties": {
                        "channel_id": {
                            "type": "string",
                            "description": "送信先DiscordチャンネルのID（数値文字列）。現在のチャンネルIDはシステムプロンプトの[Discord context]セクションに`channel_id=XXXX`として記載されている。ユーザーIDやBotIDではなくチャンネルIDを指定すること。"
                        },
                        "file_path": {
                            "type": "string",
                            "description": "送信するファイルのパス（ワークスペース相対パスまたは絶対パス）"
                        },
                        "caption": {
                            "type": "string",
                            "description": "ファイルに添付するテキストキャプション（省略可）"
                        },
                        "filename": {
                            "type": "string",
                            "description": "Discord上で表示されるファイル名（省略時は元のファイル名）"
                        }
                    }
                }),
            },
            GatewayActionDef {
                name: "join_voice_channel".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::Blocked, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "ボイスチャンネル（VC）に参加して音声対話を開始する。参加後、VC内の発話はユーザーごとに文字起こしされてこのチャンネルの会話として届き、返信は自動で読み上げられる。owner/trusted_userの依頼時のみ使用可。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "channel_id": {
                            "type": "string",
                            "description": "参加するボイスチャンネルのID（数値文字列）"
                        },
                        "text_channel_id": {
                            "type": "string",
                            "description": "文字起こしの注入先テキストチャンネルID（省略時はこの会話のチャンネル）"
                        }
                    },
                    "required": ["channel_id"]
                }),
            },
            GatewayActionDef {
                name: "leave_voice_channel".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::Blocked, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "現在参加中のボイスチャンネルから退出する。owner/trusted_userの依頼時のみ使用可。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            // `update_heartbeat_instructions` / `read_heartbeat_instructions` は #157 S3 で
            // gateway 非依存層（server 側 `SystemGatewayActions` / 実体は
            // `crates/server/src/heartbeat_instructions.rs`）へ移設済み。DB のみに依存する
            // ツールだったのに Discord 経由のターンでしか露出していなかった（#157 / #155）。
            // ここで再定義すると合成 gateway の dedup（own 優先）で own 側に食われ、
            // Discord の実装が黙って死ぬので**定義してはならない**
            // （`test_definitions_returns_expected_count` の negative assert が守る）。
            //
            // 同じ理由で、通知先（webhook）の管理 6 種（`get/set_default_[subtask_]webhook`
            // / `list_[subtask_]webhooks`）も #157 S5 で server 側（実体は
            // `crates/server/src/webhook_targets.rs`）へ移設済み。**ここで定義してはならない。**
            //
            // 残る `ensure_subtask_webhook` / `ensure_webhook` は、既存デフォルトが無い
            // ときに `discord_create_webhook`（serenity 依存）で webhook を新規作成する
            // ため Discord 固有。解決部分だけを下位層へ割る設計は実装が 1 つしか無い空の
            // 抽象を生むので S5 では行わない。
            GatewayActionDef {
                name: "ensure_subtask_webhook".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "使えるデフォルト subtask webhook が既にあればそれを redacted で返す（owner/trusted_user/co_agent）。無ければ owner かつ channel_id 指定時のみ webhook を新規作成して既定に登録する。rawトークンは返さない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["agent", "tool", "global"],
                            "description": "登録先scope（省略時 'agent'）。"
                        },
                        "agent_id": {
                            "type": "string",
                            "description": "対象エージェントID（省略時は自分。global では '*'）。"
                        },
                        "tool_name": {
                            "type": "string",
                            "description": "scope=tool のとき省略時 'spawn_subtask'。"
                        },
                        "channel_id": {
                            "type": "string",
                            "description": "新規作成時に必須。webhookを作るチャンネルの数値ID。"
                        },
                        "name": {
                            "type": "string",
                            "description": "作成するwebhook名（省略可）。"
                        },
                        "events": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "通知イベント（省略時は全て）。"
                        }
                    },
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "ensure_webhook".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "使えるデフォルト webhook が既にあればそれを redacted で返す（既定 family='activity'、owner/trusted_user/co_agent）。無ければ owner かつ channel_id 指定時のみ webhook を新規作成して既定に登録する。rawトークンは返さない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["agent", "tool", "global"],
                            "description": "登録先scope（省略時 'agent'）。"
                        },
                        "family": {
                            "type": "string",
                            "enum": ["activity", "subtask"],
                            "description": "対象ファミリ（省略時 'activity'）。"
                        },
                        "agent_id": {
                            "type": "string",
                            "description": "対象エージェントID（省略時は自分。global では '*'）。"
                        },
                        "tool_name": {
                            "type": "string",
                            "description": "scope=tool のとき省略時 'spawn_subtask'。"
                        },
                        "channel_id": {
                            "type": "string",
                            "description": "新規作成時に必須。webhookを作るチャンネルの数値ID。"
                        },
                        "name": {
                            "type": "string",
                            "description": "作成するwebhook名（省略可）。"
                        },
                        "events": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "通知イベント（省略時は全て）。"
                        }
                    },
                    "required": []
                }),
            },
        ]
    }

    async fn execute(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // Discord のスノーフレーク ID をモデルが JSON 数値で渡してきても受け付ける
        // ため、実行前に `*_id` の整数値を文字列へ正規化する（各ハンドラは as_str
        // だけを見ており、数値だと「ID がありません」と誤って失敗していた）。
        let normalized = normalize_id_args(args);
        let args = normalized.as_ref();
        match name {
            "discord_list_guilds" => self.execute_list_guilds().await,
            "discord_list_channels" => self.execute_list_channels(args, ctx).await,
            // 書き込み判断は core（`apply_discord_channel_config`）。ここは委譲だけ。
            "discord_channel_config" => self.execute_discord_channel_config(args, ctx),
            "discord_add_reaction" => self.execute_discord_add_reaction(args).await,
            "discord_create_webhook" => self.execute_discord_create_webhook(args).await,
            "discord_create_channel" => self.execute_discord_create_channel(args).await,
            "discord_send_file" => self.execute_send_file(args, ctx).await,
            // ピアレビューは依頼（#157 S7）も返信の回収（#156 S4）も server 側
            // （`crates/server/src/peer_review.rs`）へ移設済み。Discord に残るのは
            // 配送口（`text_delivery()`）だけ。
            "join_voice_channel" => self.execute_join_voice_channel(args, ctx).await,
            "leave_voice_channel" => self.execute_leave_voice_channel(args, ctx).await,
            // 通知先（webhook）の管理は #157 S5 で server 側（`crates/server/src/
            // webhook_targets.rs`）へ移設済み。ここに残るのは webhook を**新規作成**する
            // `ensure_*` だけ（既定 family: `*_subtask_*` は subtask、汎用名は activity）。
            "ensure_subtask_webhook" => self.execute_ensure_subtask_webhook(args, ctx).await,
            "ensure_webhook" => self.execute_ensure_webhook(args, ctx).await,
            _ => GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("Unknown gateway action: {name}")),
            },
        }
    }

    /// A2UI の描画面を合成 gateway へ差し出す（#156 S3）。
    ///
    /// `send_ui` の実体は gateway 非依存層（`opencrab_actions::a2ui`）にあり、
    /// Discord が提供するのは描画（`DiscordRenderer`）と応答の受け口
    /// （`DiscordUiResponseSink`）だけ。合成 gateway
    /// （`SystemGatewayActions`）はこれが `Some` のターンでだけ `send_ui` を露出する
    /// ため、移設前と同じ「Discord 経由のターンだけで使える」露出になる。
    fn a2ui_surface(&self) -> Option<Arc<opencrab_core::a2ui::A2uiSurface>> {
        Some(Arc::new(self.build_a2ui_surface()))
    }

    /// 素テキストの配送口を合成 gateway へ差し出す（#157 S7）。
    ///
    /// `request_peer_review` の実体は gateway 非依存層
    /// （`crates/server/src/peer_review.rs`）にあり、Discord が提供するのは宛先検査・
    /// メンション記法・1 通の上限・送信そのものだけ（`DiscordTextDelivery`）。
    fn text_delivery(&self) -> Option<Arc<dyn opencrab_core::text_delivery::TextDelivery>> {
        Some(Arc::new(self.build_text_delivery()))
    }
}

#[cfg(test)]
mod tests;
