//! DiscordゲートウェイアクションのGatewayActions実装
//!
//! Discord管理操作（サーバー一覧、チャンネル一覧、チャンネル設定）を
//! ゲートウェイ固有アクションとして提供する。

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
mod peer_review;
mod subtask_engine;
mod subtask_notifier;
mod subtask_webhook;
mod text_delivery;
mod ui;
mod voice_actions;
mod webhook;

pub(crate) use peer_review::record_peer_review_reply;
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
                description: "Botが参加しているDiscordサーバー（guild）の一覧を取得する。返り値の各サーバーの `id` フィールド（数値文字列）を、他のアクションの `guild_id` パラメータとして使用すること。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "discord_list_channels".to_string(),
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
                description: "Discordメッセージにリアクション（絵文字）を追加する。Unicode絵文字（例: ⚡）またはカスタム絵文字（name:id形式）を指定できる。".to_string(),
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
            "discord_channel_config" => self.execute_discord_channel_config(args, ctx),
            "discord_add_reaction" => self.execute_discord_add_reaction(args).await,
            "discord_create_webhook" => self.execute_discord_create_webhook(args).await,
            "discord_create_channel" => self.execute_discord_create_channel(args).await,
            "discord_send_file" => self.execute_send_file(args, ctx).await,
            // ピアレビュー依頼は #157 S7 で server 側（`crates/server/src/peer_review.rs`）
            // へ移設済み。Discord に残るのは配送口（`text_delivery()`）と、返信の回収
            // （`peer_review::record_peer_review_reply` / 受信ループから呼ぶ）だけ。
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
mod tests {
    use super::*;
    use opencrab_gateway::GatewayCaller;
    use serde_json::json;

    /// テスト用: serenity Httpは不要だがDiscordGatewayActionsの構築に必要。
    /// channel_config系テストではHTTP呼び出しは発生しないのでダミーでOK。
    fn make_test_actions() -> (DiscordGatewayActions, opencrab_db::Db) {
        let db = opencrab_db::Db::memory().unwrap();
        // serenityのHttpはダミートークンで作成（API呼び出しはしない）
        let http = Arc::new(Http::new("dummy-token"));
        let actions = DiscordGatewayActions::new(http, db.clone(), "/tmp".to_string(), None);
        (actions, db)
    }

    /// テスト用の呼び出しコンテキスト。旧テストは `__caller` を JSON に混ぜていたが、
    /// #36 で型付き GatewayCallContext に移行した。session_id は Discord 形式の
    /// ダミーを既定で持たせる（セッション必須アクションの検証テストを通すため）。
    fn tctx(caller: GatewayCaller) -> GatewayCallContext {
        GatewayCallContext::new(caller, "test-agent").with_session_id("discord-test-agent-111-222")
    }

    // `cancel_subtask` の 8 テスト（認可 4 / セッション無し 1 / 停止ログの説明文 3）と
    // その registry ヘルパは #157 S2 で server 側（`crates/server/src/system_actions.rs`）
    // へ移植済み。停止処理は gateway 非依存層の唯一の実装になったので、この gateway は
    // registry も lifecycle 通知口マップも持たない。

    // ---- #63: SubEngineGatewayActions 許可リスト ----

    #[tokio::test]
    async fn test_sub_engine_gateway_allowlist() {
        use opencrab_actions::SubEngineGatewayActions;

        let (actions, _db) = make_test_actions();
        // 後方互換の経路（root_gateway 未注入）では transport gateway 単体を wrap する。
        let sub_gw = SubEngineGatewayActions::new(std::sync::Arc::new(actions.clone()));

        // 許可リストの 2 名（report_progress / nostr_generate_key）はいずれも server 側
        // の定義になったため（#175 S4）、Discord 単体を wrap すると露出は空になる。
        // = sub-engine から Discord のツールへは一切到達できない。合成 gateway 経由で
        // 許可ツールに到達できることは `crates/actions/src/bridge.rs` の S2 テストが固定する。
        let names: Vec<String> = sub_gw.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            names.is_empty(),
            "Discord 単体では許可ツールが無い: {names:?}"
        );

        let sub_ctx = GatewayCallContext::new(GatewayCaller::Agent, "test-agent")
            .with_session_id("subtask-s1")
            .with_depth(1);

        // 実在するが許可外 → rejected: マーカー（`spawn_subtask` / `cancel_subtask` /
        // `create_skill` / `send_ui` を含まないのは、Discord がもう定義していないため。
        // ネスト禁止の実効ゲートは許可リスト側）。
        for name in ["discord_channel_config", "discord_send_file"] {
            let result = sub_gw.execute(name, &json!({}), &sub_ctx).await;
            assert!(!result.success, "{name} should be blocked");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap()
                    .starts_with(opencrab_actions::REJECTION_CODE_PREFIX),
                "{name} should be a policy rejection"
            );
        }

        // 未知の名前 → 通常の失敗（Unknown gateway action）
        let result = sub_gw.execute("no_such_tool", &json!({}), &sub_ctx).await;
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("Unknown gateway action"));
        assert!(!err.starts_with(opencrab_actions::REJECTION_CODE_PREFIX));

        // 移設済みツールも Discord 単体経由では届かない（未知の名前として失敗する）。
        for moved in [
            "report_progress",
            "spawn_subtask",
            "cancel_subtask",
            "read_heartbeat_instructions",
            "update_heartbeat_instructions",
            "create_skill",
            // #156 S3: A2UI 送信も server 側（`SystemGatewayActions`）の own ツール。
            // 合成 gateway 経由で sub-engine から到達できないことは
            // `send_ui_is_blocked_in_sub_engine`（`crates/server/src/system_actions.rs`）
            // が固定する。
            "send_ui",
            // #157 S7: ピアレビュー依頼も同様（`request_peer_review_is_blocked_in_sub_engine`）。
            "request_peer_review",
        ] {
            let result = sub_gw
                .execute(moved, &json!({"message": "x"}), &sub_ctx)
                .await;
            assert!(!result.success, "{moved} は Discord 単体では実行できない");
        }
    }

    // ---- #36: セッション必須アクションの fail-closed ----
    //
    // この gateway にセッション必須アクションはもう残っていない:
    // `report_progress` / `spawn_subtask` は #175 S4、`send_ui` は #156 S3、
    // `request_peer_review` は #157 S7 で gateway 非依存層へ移設済み。同趣旨のガードは
    // それぞれ `crates/server/src/system_actions.rs` / `crates/actions/src/a2ui.rs` の
    // `send_ui_without_session_fails_closed` / `crates/server/src/peer_review.rs` の
    // `error_messages_are_byte_stable` にある。

    // ---- #45: bridge ポリシー表と gateway 定義のドリフト検出 ----

    /// bridge のポリシー表（owner-only / trusted-only / discord depth ゲート）が
    /// 指す gateway 側の名前が実在すること。表が死に名を指したまま実アクションが
    /// ゲート漏れする事故を検出する。
    #[test]
    fn test_bridge_policy_names_are_live_gateway_actions() {
        let (actions, _db) = make_test_actions();
        let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();

        // owner-only な `update_heartbeat_instructions` と trusted-only な
        // `read_heartbeat_instructions` は #157 S3 で server 側へ移設済み。実在性の検証は
        // `crates/server/src/system_actions.rs` の
        // `heartbeat_instruction_tools_are_exposed_in_own_definitions` が担う。
        // trusted-only（gateway 側。execute_skill は防御的エントリで実装なし）
        for n in opencrab_actions::TRUSTED_ONLY_ACTIONS {
            if *n == "execute_skill" {
                assert!(
                    !names.contains(&n.to_string()),
                    "execute_skill は未実装のはず"
                );
            } else if *n == "read_heartbeat_instructions" {
                // 移設済み（#157 S3）。Discord が再定義すると合成 gateway の dedup で
                // own 側に食われるので、無いことを固定する。
                assert!(
                    !names.contains(&n.to_string()),
                    "read_heartbeat_instructions は server 側の実装だけであるべき"
                );
            } else if *n == "create_skill" {
                // 移設済み（#157 S6）。同じ理由で Discord には無い。実在性の検証は
                // `crates/server/src/system_actions.rs` の
                // `create_skill_is_exposed_in_own_definitions` が担う。
                assert!(
                    !names.contains(&n.to_string()),
                    "create_skill は server 側の実装だけであるべき"
                );
            } else if n.starts_with("nostr_") {
                // nostr_zap / nostr_dm は Nostr ゲートウェイ側のアクション（この
                // Discord gateway の definitions には出ない）。ここでは検証対象外。
                continue;
            } else {
                assert!(names.contains(&n.to_string()), "{n} が definitions に無い");
            }
        }
        // DISCORD_ACTIONS は**全要素が実在**しなければならない（死名は depth ゲートも
        // dispatch 除外も空振りさせる）。以前は 20 名のうち 13 名が死名だった。
        for n in opencrab_actions::DISCORD_ACTIONS {
            if *n == "send_ui" {
                // #156 S3 で gateway 非依存層へ移設済み。深さ拒否は**名前ベース**なので
                // 実装がどこにあっても効くため一覧には残す（`TRUSTED_ONLY_ACTIONS` の
                // `create_skill` と同じ扱い）。Discord が再定義すると合成 gateway の
                // dedup で own 側に食われるので、無いことを固定する。実在性の検証は
                // `send_ui_is_exposed_in_own_definitions`
                // （`crates/server/src/system_actions.rs`）が担う。
                assert!(
                    !names.contains(&n.to_string()),
                    "send_ui は gateway 非依存層の実装だけであるべき"
                );
                continue;
            }
            if *n == "request_peer_review" {
                // #157 S7 で gateway 非依存層へ移設済み。send_ui と同じ扱い（深さ拒否は
                // 名前ベースなので一覧には残す）。実在性の検証は
                // `request_peer_review_is_exposed_in_own_definitions`
                // （`crates/server/src/system_actions.rs`）が担う。
                assert!(
                    !names.contains(&n.to_string()),
                    "request_peer_review は gateway 非依存層の実装だけであるべき"
                );
                continue;
            }
            assert!(
                names.contains(&n.to_string()),
                "DISCORD_ACTIONS の {n} が definitions() に無い（死名）"
            );
        }
        assert_eq!(
            opencrab_actions::DISCORD_ACTIONS.to_vec(),
            vec![
                "discord_send_file",
                "discord_add_reaction",
                "discord_list_channels",
                "discord_list_guilds",
                "send_ui",
                "request_peer_review",
                "join_voice_channel",
                "leave_voice_channel",
            ]
        );
    }

    /// **fail-closed な dispatch 分類ガード（#152）**。
    ///
    /// `definitions()` の全名が「非ブロック dispatch の除外集合（inline）」か
    /// 「意図的な dispatch 可リスト」のどちらか**ちょうど一方**に属することを要求する。
    ///
    /// 定数 → 実装の片方向だけを見るテストでは、「新しい配送系ツールを実装したが定数へ
    /// 入れ忘れた」を検知できない（`send_ui` が dispatch されていた実際の事故がこれ）。
    /// ここは実装（`definitions()`）を起点に走査するので、新ツールを追加すると分類を
    /// 明示するまでテストが落ちる。判定基準は
    /// `opencrab_actions::default_non_dispatch_tools` の doc（5 項目）。
    #[test]
    fn discord_tools_are_classified_for_dispatch() {
        let (actions, _db) = make_test_actions();
        let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
        let non_dispatch = opencrab_actions::default_non_dispatch_tools();

        for name in &names {
            let inline = non_dispatch.contains(name);
            let dispatchable =
                opencrab_actions::DISCORD_DISPATCHABLE_ACTIONS.contains(&name.as_str());
            assert!(
                inline ^ dispatchable,
                "{name} の dispatch 分類が未定義（inline={inline}, dispatchable={dispatchable}）。\
                 新しいツールを追加したら opencrab_actions::DISCORD_INLINE_ACTIONS か \
                 DISCORD_DISPATCHABLE_ACTIONS のどちらかへ入れること（判定基準は \
                 default_non_dispatch_tools の doc）"
            );
        }

        // 逆方向: 定数側に死名が無いこと。
        for name in opencrab_actions::DISCORD_INLINE_ACTIONS {
            assert!(
                names.contains(&name.to_string()),
                "DISCORD_INLINE_ACTIONS の {name} が definitions() に無い（死名）"
            );
        }
        // #157 S6 で `create_skill` が server 側へ移り、この集合は**空**になった（Discord に
        // 残るツールは全部 inline）。空でもこのループは死名検出として意味を持つ。
        for name in opencrab_actions::DISCORD_DISPATCHABLE_ACTIONS {
            assert!(
                names.contains(&name.to_string()),
                "DISCORD_DISPATCHABLE_ACTIONS の {name} が definitions() に無い（死名）"
            );
        }
        // 分類は definitions() を覆い尽くす。
        assert_eq!(
            opencrab_actions::DISCORD_INLINE_ACTIONS.len()
                + opencrab_actions::DISCORD_DISPATCHABLE_ACTIONS.len(),
            names.len(),
            "分類集合の合計が definitions() の数と一致しない"
        );
    }

    /// 配送系・同ターン結果依存・純粋な読み取りが dispatch されていない（#152 の実害）。
    ///
    /// 特に `send_ui` は「UI を送信しユーザーの応答を待機する」配送系で、background 化
    /// すると (a) UI 投稿と本文返信の順序が入れ替わり、(b) エージェントはインタラクション
    /// ID を扱えず、(c) クリック resume と subtask 決着 resume で返信が 2 通になる。
    #[test]
    fn delivery_and_read_tools_are_inline() {
        let non_dispatch = opencrab_actions::default_non_dispatch_tools();
        for name in [
            // 配送系（`send_ui` は #156 S3、`request_peer_review` は #157 S7 で server 側へ
            // 移設。同趣旨の inline 固定は `crates/server/src/system_actions.rs` にある）
            "discord_send_file",
            "discord_add_reaction",
            // 同ターンで戻り値（URL / ID）を使う
            "ensure_webhook",
            "ensure_subtask_webhook",
            "discord_create_webhook",
            "discord_create_channel",
            // 純粋な読み取り: `list_allowed_commands` は #157 S1、
            // `read_heartbeat_instructions` は #157 S3、通知先の管理 6 種
            // （`get/set_default_[subtask_]webhook` / `list_[subtask_]webhooks`）は
            // #157 S5 で server 側へ移設。同趣旨の inline 固定は
            // `crates/server/src/system_actions.rs` にある。
        ] {
            assert!(
                non_dispatch.contains(name),
                "{name} が dispatch されてしまう（inline に残すべき）"
            );
        }
    }

    // ---- definitions ----

    #[test]
    fn test_definitions_returns_expected_count() {
        let (actions, _db) = make_test_actions();
        let defs = actions.definitions();
        assert_eq!(defs.len(), 11);

        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"discord_list_guilds"));
        assert!(names.contains(&"discord_list_channels"));
        assert!(names.contains(&"discord_channel_config"));
        assert!(names.contains(&"discord_add_reaction"));
        assert!(names.contains(&"discord_create_webhook"));
        assert!(names.contains(&"discord_create_channel"));
        assert!(names.contains(&"discord_send_file"));
        assert!(names.contains(&"join_voice_channel"));
        assert!(names.contains(&"leave_voice_channel"));
        // webhook 新規作成つきの 2 本だけが残る（#157 S5）。
        assert!(names.contains(&"ensure_subtask_webhook"));
        assert!(names.contains(&"ensure_webhook"));

        // #175 S4 / #157 S1・S2・S3・S5・S6・S7 / #155: サブタスク生成・進捗報告・**停止**・記憶
        // インデックス再構築と、汎用管理ツール（記憶インデックス設定・許可コマンド 3 種）・
        // ハートビート指示 2 種・通知先（webhook）の管理 6 種・スキル生成は gateway 非依存層
        // （server 側 `SystemGatewayActions`）へ移設済み。
        // Discord がこれらを再び定義すると `SystemGatewayActions` の dedup（own 優先）で
        // own 側に食われ、Discord 実装の後処理が黙って落ちる（#155 の後退）。
        for moved in [
            "spawn_subtask",
            "report_progress",
            "cancel_subtask",
            "rebuild_memory_index",
            "update_memory_index_config",
            "add_allowed_command",
            "list_allowed_commands",
            "remove_allowed_command",
            "update_heartbeat_instructions",
            "read_heartbeat_instructions",
            // #157 S5 で移設した通知先の管理 6 種。
            "get_default_subtask_webhook",
            "set_default_subtask_webhook",
            "list_subtask_webhooks",
            "get_default_webhook",
            "set_default_webhook",
            "list_webhooks",
            // #157 S6 で移設したスキル生成。
            "create_skill",
            // #157 S7 で移設したピアレビュー依頼（Discord に残るのは配送口と返信回収）。
            "request_peer_review",
        ] {
            assert!(
                !names.contains(&moved),
                "{moved} は server 側の実装だけであるべき"
            );
        }
    }

    #[test]
    fn test_definitions_have_valid_parameters() {
        let (actions, _db) = make_test_actions();
        for def in actions.definitions() {
            assert!(
                def.parameters.is_object(),
                "parameters should be object for {}",
                def.name
            );
            assert!(def.parameters["type"] == "object");
        }
    }

    // ---- request_peer_review ----
    //
    // 引数検査（content 必須 / 長さ上限 / 宛先の解決と検査）のテストは #157 S7 で
    // server 側（`crates/server/src/peer_review.rs`）へ移設済み。Discord に残る配送口の
    // テストは `super::text_delivery`。

    // ---- channel_config ----

    #[tokio::test]
    async fn test_channel_config_upsert() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "channel_name": "general",
                    "readable": true,
                    "writable": false,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(result.success);
        assert!(result.error.is_none());

        let data = result.data.unwrap();
        assert_eq!(data["channel_id"], "ch-1");
        assert_eq!(data["readable"], true);
        assert_eq!(data["writable"], false);

        // DB確認
        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config_for_agent(&conn, "ch-1", "test-agent")
            .unwrap()
            .unwrap();
        assert!(cfg.readable);
        assert!(!cfg.writable);
        assert_eq!(cfg.channel_name, "general");
        assert_eq!(cfg.guild_id, "guild-1");
    }

    #[tokio::test]
    async fn test_channel_config_update_existing() {
        let (actions, db) = make_test_actions();

        // 初回設定
        actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "channel_name": "general",
                    "readable": true,
                    "writable": true,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;

        // 更新
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "channel_name": "general",
                    "readable": false,
                    "writable": false,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(result.success);

        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config_for_agent(&conn, "ch-1", "test-agent")
            .unwrap()
            .unwrap();
        assert!(!cfg.readable);
        assert!(!cfg.writable);
    }

    #[tokio::test]
    async fn test_channel_config_missing_params() {
        let (actions, _db) = make_test_actions();

        // channel_idのみ → guild_idが欠けてエラー
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({"channel_id": "ch-1"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("guild_id"));
    }

    #[tokio::test]
    async fn test_channel_config_missing_readable() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "writable": true,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("readable"));
    }

    #[tokio::test]
    async fn test_channel_config_optional_name() {
        let (actions, db) = make_test_actions();
        // channel_nameなしでも動く
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "readable": true,
                    "writable": true,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(result.success);

        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config_for_agent(&conn, "ch-1", "test-agent")
            .unwrap()
            .unwrap();
        assert_eq!(cfg.channel_name, "");
    }

    /// 回帰: モデルが Discord スノーフレーク ID を JSON 数値で渡しても通ること。
    /// 以前は各ハンドラが as_str だけを見ており「channel_id パラメータが必要です」と
    /// 誤って失敗していた（2^53 超の 19 桁 ID も精度を保って文字列化される）。
    #[tokio::test]
    async fn test_channel_config_numeric_ids() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": 1479115942293409942u64,
                    "guild_id": 1465697209541726362u64,
                    "readable": true,
                    "writable": true,
                    "whitelisted": false,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(
            result.success,
            "numeric ids should be accepted: {:?}",
            result.error
        );

        // DB には文字列化した ID が精度そのままで入る。
        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config_for_agent(
            &conn,
            "1479115942293409942",
            "test-agent",
        )
        .unwrap()
        .unwrap();
        assert!(cfg.readable);
        assert!(cfg.writable);
        assert_eq!(cfg.guild_id, "1465697209541726362");
    }

    #[test]
    fn test_normalize_id_args_stringifies_only_id_numbers() {
        // *_id の整数は文字列化、それ以外（真偽・非 id 数値・既に文字列）は不変。
        let input = json!({
            "channel_id": 1479115942293409942u64,
            "guild_id": "already-str",
            "readable": true,
            "count": 5,
        });
        let out = normalize_id_args(&input);
        assert_eq!(out["channel_id"], json!("1479115942293409942"));
        assert_eq!(out["guild_id"], json!("already-str"));
        assert_eq!(out["readable"], json!(true));
        assert_eq!(out["count"], json!(5));

        // 変換不要ならコピーしない（借用のまま）。
        let noop = json!({"readable": true, "count": 1});
        assert!(matches!(normalize_id_args(&noop), Cow::Borrowed(_)));
    }

    // ---- unknown action ----

    #[tokio::test]
    async fn test_unknown_gateway_action() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute("nonexistent", &json!({}), &tctx(GatewayCaller::Agent))
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown gateway action"));
    }

    // ---- list_channels パラメータバリデーション ----

    #[tokio::test]
    async fn test_list_channels_missing_guild_id() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_list_channels",
                &json!({}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("guild_id"));
    }

    #[tokio::test]
    async fn test_list_channels_invalid_guild_id() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_list_channels",
                &json!({"guild_id": "not-a-number"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("数値ID"));
    }

    // `create_skill` の 3 テスト（基本 / 同名 dedup / 非 trusted 拒否）は #157 S6 で
    // server 側（`crates/server/src/system_actions.rs`）へ移植済み（1 件も落としていない）。
    // 実体は `crates/server/src/agent_management.rs`。

    // ---- discord_send_file ----

    #[tokio::test]
    async fn test_send_file_workspace_violation() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_send_file",
                &json!({
                    "channel_id": "12345678901234567",
                    "file_path": "/etc/passwd",
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(
            result.error.as_ref().unwrap().contains("ワークスペース外")
                || result.error.as_ref().unwrap().contains("見つかりません")
        );
    }

    #[tokio::test]
    async fn test_send_file_missing_params() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_send_file",
                &json!({"channel_id": "123"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("file_path"));
    }

    // ---- discord_create_channel ----

    #[test]
    fn test_create_channel_schema() {
        let (actions, _db) = make_test_actions();
        let defs = actions.definitions();
        let def = defs
            .iter()
            .find(|d| d.name == "discord_create_channel")
            .expect("discord_create_channel definition should exist");

        // required fields
        let required = def.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "guild_id"));
        assert!(required.iter().any(|v| v == "name"));

        // properties present
        let props = &def.parameters["properties"];
        for key in ["guild_id", "name", "parent_id", "topic", "reason"] {
            assert!(props.get(key).is_some(), "missing property {key}");
        }
    }

    #[tokio::test]
    async fn test_create_channel_missing_guild_id() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_create_channel",
                &json!({"name": "general"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("guild_id"));
    }

    #[tokio::test]
    async fn test_create_channel_invalid_guild_id() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_create_channel",
                &json!({"guild_id": "not-a-number", "name": "general"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("数値ID"));
    }

    #[tokio::test]
    async fn test_create_channel_missing_name() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_create_channel",
                &json!({"guild_id": "123456789"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("name"));
    }

    #[tokio::test]
    async fn test_create_channel_name_too_short() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_create_channel",
                &json!({"guild_id": "123456789", "name": "a"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("2〜100文字"));
    }

    // ---- heartbeat instructions ----
    //
    // 4 テスト（owner 以外の拒否 + 監査なし / owner 成功 + 監査 / effective 解決 /
    // 素の agent 拒否 + co_agent 許可）は #157 S3 で server 側
    // （`crates/server/src/system_actions.rs`）へ移植済み。

    #[tokio::test]
    async fn test_create_channel_invalid_parent_id() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_create_channel",
                &json!({
                    "guild_id": "123456789",
                    "name": "general",
                    "parent_id": "not-a-number",
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("parent_id"));
    }

    // ---- webhook 新規作成つきアクション（ensure_*） ----
    //
    // 可視化・管理の 6 本（`get/set_default_[subtask_]webhook` / `list_[subtask_]webhooks`）
    // とその 18 テストは #157 S5 で server 側（`crates/server/src/webhook_targets.rs`）へ
    // 移設済み。DB と設定ファイル由来の既定値しか触らないので gateway 非依存層で持てる。
    // ここに残るのは `discord_create_webhook`（serenity 依存）を呼ぶ `ensure_*` だけ。

    const WH_VALID_URL: &str = "https://discord.com/api/webhooks/123456789/abcSECRETtok";
    const WH_SECRET: &str = "abcSECRETtok";

    /// 応答 JSON に raw トークンが 1 度も現れないこと（秘匿処理の不変条件）。
    fn json_has_no_raw_token(v: &serde_json::Value) -> bool {
        !v.to_string().contains(WH_SECRET)
    }

    #[tokio::test]
    async fn test_ensure_subtask_webhook_returns_existing_without_create() {
        let (actions, db) = make_test_actions();
        {
            let conn = db.lock().unwrap();
            let row = opencrab_db::queries::AgentWebhookConfigRow {
                scope: "agent".to_string(),
                agent_id: "test-agent".to_string(),
                tool_name: String::new(),
                kind: "subtask".to_string(),
                url: WH_VALID_URL.to_string(),
                events_json: None,
                enabled: true,
                name: None,
                created_by: Some("owner".to_string()),
                output_mode: "summary".to_string(),
                max_chars: 1500,
                updated_at: String::new(),
            };
            opencrab_db::queries::upsert_agent_webhook_config(&conn, &row).unwrap();
        }
        // trusted_user can read existing without creating
        let result = actions
            .execute(
                "ensure_subtask_webhook",
                &json!({  "scope": "agent" }),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        let data = result.data.unwrap();
        assert_eq!(data["created"], false);
        assert!(json_has_no_raw_token(&data));
    }

    #[tokio::test]
    async fn test_ensure_subtask_webhook_create_requires_owner_and_channel() {
        let (actions, _db) = make_test_actions();
        // non-owner, nothing exists -> owner-only error
        let non_owner = actions
            .execute(
                "ensure_subtask_webhook",
                &json!({  "scope": "agent" }),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(!non_owner.success);
        assert!(non_owner.error.unwrap().contains("owner"));

        // owner but no channel_id -> channel_id required
        let no_channel = actions
            .execute(
                "ensure_subtask_webhook",
                &json!({  "scope": "agent" }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(!no_channel.success);
        assert!(no_channel.error.unwrap().contains("channel_id"));
    }

    /// **設定ファイル由来のフォールバックが Discord 経路で今までどおり効く**（#157 S5）。
    ///
    /// この値は #157 S5 で `AppState` へ持ち上げたが、Discord gateway_actions は
    /// 従来どおりコンストラクタで同じ値を受け取る。DB に行が無くても既定が解決され、
    /// `ensure_*` は webhook を**作らずに**それを返す（持ち上げによる挙動変化なし）。
    #[tokio::test]
    async fn config_fallback_still_resolves_on_the_discord_path() {
        let db = opencrab_db::Db::memory().unwrap();
        let http = Arc::new(Http::new("dummy-token"));
        let actions = DiscordGatewayActions::new(
            http,
            db,
            "/tmp".to_string(),
            opencrab_actions::webhook_target::WebhookConfig::from_parts(
                WH_VALID_URL.to_string(),
                Some(vec!["started".to_string()]),
            ),
        );

        let result = actions
            .execute(
                "ensure_subtask_webhook",
                &json!({ "scope": "agent" }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        let data = result.data.unwrap();
        assert_eq!(data["created"], false, "既定があるので作成してはいけない");
        assert_eq!(data["source"], "env_config");
        assert_eq!(data["scope"], "env_config");
        assert!(json_has_no_raw_token(&data));
    }
}
