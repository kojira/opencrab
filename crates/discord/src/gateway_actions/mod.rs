//! DiscordゲートウェイアクションのGatewayActions実装
//!
//! Discord管理操作（サーバー一覧、チャンネル一覧、チャンネル設定）を
//! ゲートウェイ固有アクションとして提供する。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use opencrab_gateway::{GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext};
use serde_json::json;
use serenity::http::Http;
use tokio::task::AbortHandle;

use crate::message_loop::LoopEvent;
use crate::PendingInteractionRegistry;

mod agent_management;
mod discord_ops;
mod heartbeat_instructions;
mod peer_review;
mod subtask_engine;
mod subtask_webhook;
mod ui;
mod webhook;

pub(crate) use peer_review::record_peer_review_reply;
pub use subtask_engine::spawn_activity_tool_event_sink;
use webhook::DeliveryBatch;
pub use webhook::WebhookConfig;

/// A running subtask tracked by the registry.
#[derive(Clone)]
pub struct SpawnedSubtask {
    pub abort_handle: AbortHandle,
    pub session_id: String,
    pub parent_session_id: String,
    pub spawned_at: String,
    pub agent_id: String,
    /// Subtask lifecycle webhook config (spawn 時指定)。None なら通知無効。
    pub webhook: Option<WebhookConfig>,
    /// 同一 run の lifecycle delivery を直列化する sender。
    pub webhook_tx: Option<tokio::sync::mpsc::UnboundedSender<DeliveryBatch>>,
    /// duration 算出用の起動時刻。
    pub started_instant: std::time::Instant,
}

/// Registry of active subtasks keyed by subtask_id.
pub type SubtaskRegistry = Arc<DashMap<String, SpawnedSubtask>>;

struct ArcLlmClient(Arc<dyn opencrab_core::LlmClient>);

#[async_trait::async_trait]
impl opencrab_core::LlmClient for ArcLlmClient {
    async fn chat(
        &self,
        request: opencrab_core::ChatRequest,
    ) -> anyhow::Result<opencrab_core::ChatResponse> {
        self.0.chat(request).await
    }
}

/// Discord固有のゲートウェイアクション実装。
///
/// serenityのHTTPクライアントとDB接続を保持し、
/// Discord管理操作をGatewayActionsとして提供する。
pub struct DiscordGatewayActions {
    http: Arc<Http>,
    db: opencrab_db::Db,
    agent_id: String,
    tools_config: Arc<std::sync::RwLock<opencrab_actions::tools::ToolsConfig>>,
    llm_client: Option<Arc<dyn opencrab_core::LlmClient>>,
    default_model: String,
    workspace_root: PathBuf,
    subtask_registry: SubtaskRegistry,
    /// Subtask lifecycle webhook 配送用の HTTP クライアント（worker で共有）。
    webhook_client: reqwest::Client,
    /// spawn_subtask.webhook 省略時に使うデフォルト lifecycle webhook。
    default_subtask_webhook: Option<WebhookConfig>,
    pub pending_interaction_registry: Option<PendingInteractionRegistry>,
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<LoopEvent>>,
    /// owner-only な A2UI インタラクションの権限判定に使う owner の Discord ユーザーID。
    /// 空文字の場合は owner 判定が無効（誰でも操作可）になる点に注意。
    pub owner_discord_id: String,
    /// report_progress のデバウンス世代カウンタ（parent_session_id → 最新世代）。
    /// 短時間に複数回 report_progress が呼ばれても、最後の1回のみメインエンジン再呼び出しを
    /// 発火させるために使う。
    progress_debounce: Arc<dashmap::DashMap<String, u64>>,
}

impl DiscordGatewayActions {
    pub fn new(
        http: Arc<Http>,
        db: opencrab_db::Db,
        agent_id: String,
        tools_config: Arc<std::sync::RwLock<opencrab_actions::tools::ToolsConfig>>,
        llm_client: Option<Arc<dyn opencrab_core::LlmClient>>,
        default_model: String,
        workspace_root: PathBuf,
        subtask_registry: SubtaskRegistry,
        default_subtask_webhook: Option<WebhookConfig>,
    ) -> Self {
        Self {
            http,
            db,
            agent_id,
            tools_config,
            llm_client,
            default_model,
            workspace_root,
            subtask_registry,
            webhook_client: reqwest::Client::new(),
            default_subtask_webhook,
            pending_interaction_registry: None,
            event_tx: None,
            owner_discord_id: String::new(),
            progress_debounce: Arc::new(dashmap::DashMap::new()),
        }
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
            GatewayActionDef {
                name: "update_memory_index_config".to_string(),
                description: "メモリインデックスの設定（batch_size、threshold）を更新する。少なくとも1つのパラメータを指定する必要がある。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "batch_size": {
                            "type": "integer",
                            "description": "一度に処理するメモリのバッチサイズ"
                        },
                        "threshold": {
                            "type": "integer",
                            "description": "インデックス再構築の閾値"
                        }
                    },
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "add_allowed_command".to_string(),
                description: "シェルツールの許可コマンドリストに新しいコマンドを追加する。オーナーのみ実行可能。コマンド名（例: curl, wget, git）を指定する。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "追加するコマンド名（英数字・ハイフン・アンダースコアのみ。例: curl, wget, git）"
                        }
                    },
                    "required": ["command"]
                }),
            },
            GatewayActionDef {
                name: "list_allowed_commands".to_string(),
                description: "現在DBに保存されている許可コマンドの一覧を取得する。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "remove_allowed_command".to_string(),
                description: "シェルツールの許可コマンドリストからコマンドを削除する。オーナーのみ実行可能。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "削除するコマンド名"
                        }
                    },
                    "required": ["command"]
                }),
            },
            GatewayActionDef {
                name: "rebuild_memory_index".to_string(),
                description: "メモリインデックスをゼロから再構築する。既存のインデックスを削除し、全ログを再インデックスする。時間がかかることがある。結果として logs_indexed（処理したログ数）と nodes_created（作成したインデックスノード数）を返す。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "create_skill".to_string(),
                description: "ユーザーから「〇〇するスキルを作って」と言われたとき新しいスキルを作成する。guidanceにコマンド例・使い方を書くことで、LLMがexecute_shellで動的に実行できるようになる。同名スキルが存在する場合は更新される。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "スキル名"
                        },
                        "description": {
                            "type": "string",
                            "description": "スキルの説明"
                        },
                        "guidance": {
                            "type": "string",
                            "description": "スキルのガイダンス（省略時は空文字列）"
                        }
                    },
                    "required": ["name", "description"]
                }),
            },
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
                name: "spawn_subtask".to_string(),
                description: "バックグラウンドでサブタスクを起動します。LLMエンジンがサブエンジンとして非同期実行し、完了後にメインエンジンを自動的に再呼び出しします。複雑な長時間処理（画像生成・コード実装・調査など）に使用してください。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "サブエンジンに実行させるタスクの説明"
                        },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "タイムアウト秒数（省略時1800秒）"
                        },
                        "max_iterations": {
                            "type": "integer",
                            "description": "LLMループの最大イテレーション数（省略時は無制限）"
                        },
                        "label": {
                            "type": "string",
                            "description": "サブタスクのラベル（webhook通知の表示用。省略時はtask先頭を使用）"
                        },
                        "webhook": {
                            "type": "object",
                            "description": "subtask lifecycle を Discord webhook へ通知する設定（省略時は gateway.discord.default_subtask_webhook を使用）。",
                            "properties": {
                                "url": {
                                    "type": "string",
                                    "description": "Discord webhook URL"
                                },
                                "events": {
                                    "type": "array",
                                    "description": "通知するイベント（省略時は全て）。started/progress/completed/failed/timed_out/aborted",
                                    "items": { "type": "string" }
                                }
                            },
                            "required": ["url"]
                        }
                    },
                    "required": ["task"]
                }),
            },
            GatewayActionDef {
                name: "cancel_subtask".to_string(),
                description: "実行中のサブタスクをキャンセルします。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "subtask_id": {
                            "type": "string",
                            "description": "キャンセルするサブタスクのID（subtask_spawnedイベントから取得）"
                        }
                    },
                    "required": ["subtask_id"]
                }),
            },
            GatewayActionDef {
                name: "report_progress".to_string(),
                description: "サブエンジンからメインエンジンへ進捗を報告します。depth >= 1のサブエンジンのみ使用可能。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "進捗メッセージ"
                        },
                        "subtask_id": {
                            "type": "string",
                            "description": "このサブタスクのID（オプション）"
                        }
                    },
                    "required": ["message"]
                }),
            },
            GatewayActionDef {
                name: "update_heartbeat_instructions".to_string(),
                description: "ハートビート（自律発言）時の振る舞い指示を更新する。オーナーが「これからハートビートでは○○して」と明示的に依頼した文脈でのみ呼ぶこと。出力形式（SPEAK/LEARN/IDLE）はランタイムが固定するため、ここでは頻度・トーン・話題・沈黙条件などの方針のみを書く。オーナー限定。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["agent", "channel"],
                            "description": "agent=エージェント全体のグローバル指示、channel=特定チャンネルの上書き。"
                        },
                        "channel_id": {
                            "type": "string",
                            "description": "scope=channelのとき必須。対象チャンネルの数値ID。"
                        },
                        "guild_id": {
                            "type": "string",
                            "description": "scope=channelで新規にチャンネル設定を作成する場合に必要なサーバーの数値ID。"
                        },
                        "instructions": {
                            "type": "string",
                            "description": "新しいハートビート指示の全文（最大4000字）。"
                        },
                        "reason": {
                            "type": "string",
                            "description": "変更理由（監査ログに記録される。省略可）。"
                        }
                    },
                    "required": ["scope", "instructions"]
                }),
            },
            GatewayActionDef {
                name: "read_heartbeat_instructions".to_string(),
                description: "現在のハートビート指示を読み出す。scope=agentでエージェント全体、scope=channelでチャンネル上書きのみ、scope=effectiveで実際にtickで使われる合成結果（解決ルール適用後）を返す。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["agent", "channel", "effective"],
                            "description": "agent / channel / effective。channel・effectiveのときはchannel_id必須。"
                        },
                        "channel_id": {
                            "type": "string",
                            "description": "scope=channel または effective のとき必須。対象チャンネルの数値ID。"
                        }
                    },
                    "required": ["scope"]
                }),
            },
            GatewayActionDef {
                name: "get_default_subtask_webhook".to_string(),
                description: "spawn_subtask が webhook 未指定時に実際に使うデフォルト subtask webhook を解決して返す。トークンは秘匿され redacted_url のみ返る。owner/trusted_user/co_agent のみ。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "対象エージェントID（省略時は自分）。"
                        },
                        "tool_name": {
                            "type": "string",
                            "description": "tool scope を解決する際のツール名（省略可）。"
                        },
                        "scope": {
                            "type": "string",
                            "description": "参考情報（解決は固定順序: tool>agent>global>env）。"
                        }
                    },
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "set_default_subtask_webhook".to_string(),
                description: "scope（agent/tool/global）ごとのデフォルト subtask webhook を設定する。urlを空/省略にするとそのscopeを無効化（enabled=false）する。owner限定。応答にrawトークンは含まれない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["agent", "tool", "global"],
                            "description": "agent=エージェント既定、tool=spawn_subtaskツール既定、global=全体既定。"
                        },
                        "agent_id": {
                            "type": "string",
                            "description": "対象エージェントID（省略時は自分。global では '*' に強制）。"
                        },
                        "tool_name": {
                            "type": "string",
                            "description": "scope=tool のとき省略時 'spawn_subtask'。"
                        },
                        "url": {
                            "type": "string",
                            "description": "Discord webhook URL。空/省略でそのscopeを無効化する。"
                        },
                        "enabled": {
                            "type": "boolean",
                            "description": "有効/無効（url指定時のデフォルトtrue）。"
                        },
                        "events": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "通知イベント（省略時は全て）。"
                        },
                        "output_mode": {
                            "type": "string",
                            "description": "出力モード（省略時 'summary'）。"
                        },
                        "max_chars": {
                            "type": "integer",
                            "description": "最大文字数（省略時 1500）。"
                        },
                        "kind": {
                            "type": "string",
                            "description": "種別（省略時 'subtask'）。"
                        }
                    },
                    "required": ["scope"]
                }),
            },
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
                name: "list_subtask_webhooks".to_string(),
                description: "登録されている subtask webhook 設定を一覧する。トークンは秘匿され redacted_url のみ返る。owner/trusted_user/co_agent のみ。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "対象エージェントID（省略時は自分。globalも併せて返る）。"
                        },
                        "scope": {
                            "type": "string",
                            "description": "scopeで絞り込み（省略可）。"
                        },
                        "include_disabled": {
                            "type": "boolean",
                            "description": "無効化済みも含めるか（省略時 false）。"
                        }
                    },
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "get_default_webhook".to_string(),
                description: "実際に使われるデフォルト webhook を解決して返す（既定 family='activity'＝一般ツール/コマンド活動）。トークンは秘匿され redacted_url のみ返る。owner/trusted_user/co_agent のみ。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "family": {
                            "type": "string",
                            "enum": ["activity", "subtask"],
                            "description": "解決するファミリ（省略時 'activity'）。"
                        },
                        "agent_id": {
                            "type": "string",
                            "description": "対象エージェントID（省略時は自分）。"
                        },
                        "tool_name": {
                            "type": "string",
                            "description": "tool scope を解決する際のツール名（省略可）。"
                        }
                    },
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "set_default_webhook".to_string(),
                description: "scope（agent/tool/global）ごとのデフォルト webhook を設定する（既定 family='activity'）。urlを空/省略にするとそのscopeを無効化（enabled=false）する。owner は全 scope、agent は自分の agent-scope のみ設定/無効化できる。応答にrawトークンは含まれない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["agent", "tool", "global"],
                            "description": "agent=エージェント既定、tool=ツール既定、global=全体既定。"
                        },
                        "family": {
                            "type": "string",
                            "enum": ["activity", "subtask"],
                            "description": "設定するファミリ（省略時 'activity'）。"
                        },
                        "agent_id": {
                            "type": "string",
                            "description": "対象エージェントID（省略時は自分。global では '*' に強制）。"
                        },
                        "tool_name": {
                            "type": "string",
                            "description": "scope=tool のとき省略時 'spawn_subtask'。activity の特定ツール宛先はツール名を指定する。"
                        },
                        "url": {
                            "type": "string",
                            "description": "Discord webhook URL。空/省略でそのscopeを無効化する。"
                        },
                        "enabled": {
                            "type": "boolean",
                            "description": "有効/無効（url指定時のデフォルトtrue）。"
                        },
                        "events": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "通知イベント（省略時は全て）。"
                        },
                        "output_mode": {
                            "type": "string",
                            "description": "出力モード（省略時 'summary'）。"
                        },
                        "max_chars": {
                            "type": "integer",
                            "description": "最大文字数（省略時 1500）。"
                        }
                    },
                    "required": ["scope"]
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
            GatewayActionDef {
                name: "list_webhooks".to_string(),
                description: "登録されている webhook 設定を一覧する。`family`/`scope` で絞り込み可（省略時は全件）。トークンは秘匿され redacted_url のみ返る。owner/trusted_user/co_agent のみ。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "対象エージェントID（省略時は自分。globalも併せて返る）。"
                        },
                        "family": {
                            "type": "string",
                            "description": "family（kind）で絞り込み（省略可）。例: 'activity' / 'subtask'。"
                        },
                        "scope": {
                            "type": "string",
                            "description": "scopeで絞り込み（省略可）。"
                        },
                        "include_disabled": {
                            "type": "boolean",
                            "description": "無効化済みも含めるか（省略時 false）。"
                        }
                    },
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "send_ui".to_string(),
                description: "A2UIコンポーネントで構成されたUIを送信し、ユーザーの応答を待機する。\n\n使用例（ボタン）:\n{\"channel_id\": \"123456789\", \"components\": [{\"id\": \"txt1\", \"component\": \"Text\", \"text\": \"選んでください\"}, {\"id\": \"row1\", \"component\": \"Row\", \"children\": [\"btn1\", \"btn2\"]}, {\"id\": \"btn1\", \"component\": \"Button\", \"text\": \"選択A\", \"style\": \"primary\", \"action\": {\"name\": \"choose\", \"context\": {\"value\": \"A\"}}}, {\"id\": \"btn2\", \"component\": \"Button\", \"text\": \"選択B\", \"style\": \"secondary\", \"action\": {\"name\": \"choose\", \"context\": {\"value\": \"B\"}}}]}\n\n使用例（セレクトメニュー）:\n{\"channel_id\": \"123456789\", \"components\": [{\"id\": \"txt1\", \"component\": \"Text\", \"text\": \"モデルを選択\"}, {\"id\": \"col1\", \"component\": \"Column\", \"children\": [\"txt1\", \"sel1\"]}, {\"id\": \"sel1\", \"component\": \"SelectMenu\", \"placeholder\": \"モデルを選んでください\", \"options\": [{\"label\": \"GPT-4\", \"value\": \"gpt-4\"}, {\"label\": \"Claude\", \"value\": \"claude\"}], \"action\": {\"name\": \"select_model\"}}]}\n\n使用例（フォーム/モーダル）:\n{\"channel_id\": \"123456789\", \"components\": [{\"id\": \"col1\", \"component\": \"Column\", \"children\": [\"txt1\", \"row1\"]}, {\"id\": \"txt1\", \"component\": \"Text\", \"text\": \"設定を変更\"}, {\"id\": \"row1\", \"component\": \"Row\", \"children\": [\"trigger_btn\"]}, {\"id\": \"trigger_btn\", \"component\": \"Button\", \"text\": \"設定を開く\", \"style\": \"primary\", \"action\": {\"name\": \"open_form\"}}, {\"id\": \"form1\", \"component\": \"Form\", \"title\": \"設定変更\", \"children\": [\"input_name\", \"input_desc\"], \"action\": {\"name\": \"submit_form\"}}, {\"id\": \"input_name\", \"component\": \"TextInput\", \"label\": \"名前\", \"placeholder\": \"名前を入力\", \"style\": \"short\", \"required\": true}, {\"id\": \"input_desc\", \"component\": \"TextInput\", \"label\": \"説明\", \"placeholder\": \"説明を入力\", \"style\": \"paragraph\", \"required\": false}]}\n\n注意: Rowのchildrenで参照するButton/SelectMenuはトップレベルのcomponents配列に定義する。各Buttonには一意のidとaction（name + context）を設定する。SelectMenuの選択結果はaction.contextにselected_valuesとして返される。Formはモーダル表示用でトリガーボタンが必要。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "channel_id": {
                            "type": "string",
                            "description": "送信先チャンネルID"
                        },
                        "components": {
                            "type": "array",
                            "description": "A2UI v0.9 コンポーネント配列",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": { "type": "string" },
                                    "component": { "type": "string", "enum": ["Text", "Button", "Row", "Column", "SelectMenu", "TextInput", "Form"] },
                                    "text": { "type": "string" },
                                    "variant": { "type": "string" },
                                    "label": { "type": "string", "description": "TextInputのラベル" },
                                    "title": { "type": "string", "description": "Formのタイトル" },
                                    "action": {
                                        "type": "object",
                                        "properties": {
                                            "name": { "type": "string" },
                                            "context": { "type": "object" }
                                        }
                                    },
                                    "style": { "type": "string" },
                                    "emoji": { "type": "string" },
                                    "children": { "type": "array", "items": { "type": "string" } },
                                    "options": {
                                        "type": "array",
                                        "description": "SelectMenuの選択肢",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "label": { "type": "string" },
                                                "value": { "type": "string" },
                                                "description": { "type": "string" },
                                                "emoji": { "type": "string" },
                                                "default": { "type": "boolean" }
                                            },
                                            "required": ["label", "value"]
                                        }
                                    },
                                    "placeholder": { "type": "string" },
                                    "min_values": { "type": "integer" },
                                    "max_values": { "type": "integer" },
                                    "min_length": { "type": "integer" },
                                    "max_length": { "type": "integer" },
                                    "required": { "type": "boolean" }
                                },
                                "required": ["id", "component"]
                            }
                        },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "タイムアウト秒数（デフォルト: 300）"
                        },
                        "owner_only": {
                            "type": "boolean",
                            "description": "オーナーのみ操作可能か（デフォルト: true）"
                        }
                    },
                    "required": ["channel_id", "components"]
                }),
            },
            GatewayActionDef {
                name: "request_peer_review".to_string(),
                description: "自分の成果物（diff・実行結果・トレース等）を、同じチャンネルにいる別のBot（別モデル）にピアレビューしてもらうため、レビュー依頼をDiscordチャンネルへ投稿する。contentは要約せずRAWのまま part X/N で分割送信される。レビュアーは [Peer Review] で始まる返信（score 0.0-1.0 / gaps / summary）を返す想定。activeタスクがあればタスク台帳に [peer review requested] を自動記録する。".to_string(),
                parameters: json!({
                    "type": "object",
                    "required": ["content", "channel_id"],
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "レビュー対象のRAWコンテンツ（diff・出力・トレース等）。要約せずそのまま渡すこと。上限12000文字（超える場合はワークスペースに保存してdiscord_send_fileで添付する）。"
                        },
                        "channel_id": {
                            "type": "string",
                            "description": "投稿先DiscordチャンネルのID（数値文字列）。現在のチャンネルIDは会話の[Discord context]にchannel_id=XXXXとして記載されている。"
                        },
                        "instructions": {
                            "type": "string",
                            "description": "レビュアーに重点的に見てほしい観点（省略可）。"
                        },
                        "reviewer": {
                            "type": "string",
                            "description": "指名したいレビュアー（省略可）。システムプロンプトの Peer Reviewers 一覧にある表示名または Discord user id。指定するとヘッダにメンションが付く。"
                        }
                    }
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
        match name {
            "discord_list_guilds" => self.execute_list_guilds().await,
            "discord_list_channels" => self.execute_list_channels(args).await,
            "discord_channel_config" => self.execute_discord_channel_config(args),
            "discord_add_reaction" => self.execute_discord_add_reaction(args).await,
            "discord_create_webhook" => self.execute_discord_create_webhook(args).await,
            "discord_create_channel" => self.execute_discord_create_channel(args).await,
            "update_memory_index_config" => self.execute_update_memory_index_config(args),
            "add_allowed_command" => self.execute_add_allowed_command(args, ctx),
            "list_allowed_commands" => self.execute_list_allowed_commands(),
            "remove_allowed_command" => self.execute_remove_allowed_command(args, ctx),
            "rebuild_memory_index" => self.execute_rebuild_memory_index().await,
            "create_skill" => self.execute_create_skill(args, ctx),
            "discord_send_file" => self.execute_send_file(args).await,
            "request_peer_review" => self.execute_request_peer_review(args, ctx).await,
            "spawn_subtask" => self.execute_spawn_subtask(args, ctx).await,
            "cancel_subtask" => self.execute_cancel_subtask(args),
            "report_progress" => self.execute_report_progress(args, ctx).await,
            "send_ui" => self.execute_send_ui(args, ctx).await,
            "update_heartbeat_instructions" => {
                self.execute_update_heartbeat_instructions(args, ctx)
            }
            "read_heartbeat_instructions" => self.execute_read_heartbeat_instructions(args, ctx),
            "get_default_subtask_webhook" => self.execute_get_default_subtask_webhook(args, ctx),
            "set_default_subtask_webhook" => self.execute_set_default_subtask_webhook(args, ctx),
            "ensure_subtask_webhook" => self.execute_ensure_subtask_webhook(args, ctx).await,
            "list_subtask_webhooks" => self.execute_list_subtask_webhooks(args, ctx),
            // 汎用名（既定 family='activity'）。
            "get_default_webhook" => self.execute_get_default_webhook(args, ctx),
            "set_default_webhook" => self.execute_set_default_webhook(args, ctx),
            "ensure_webhook" => self.execute_ensure_webhook(args, ctx).await,
            "list_webhooks" => self.execute_list_webhooks(args, ctx),
            _ => GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("Unknown gateway action: {name}")),
            },
        }
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
        let tools_config = Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        ));
        let subtask_registry: SubtaskRegistry = Arc::new(DashMap::new());
        let actions = DiscordGatewayActions::new(
            http,
            db.clone(),
            "test-agent".to_string(),
            tools_config,
            None,
            String::new(),
            std::path::PathBuf::from("/tmp"),
            subtask_registry,
            None,
        );
        (actions, db)
    }

    /// テスト用の呼び出しコンテキスト。旧テストは `__caller` を JSON に混ぜていたが、
    /// #36 で型付き GatewayCallContext に移行した。session_id は Discord 形式の
    /// ダミーを既定で持たせる（セッション必須アクションの検証テストを通すため）。
    fn tctx(caller: GatewayCaller) -> GatewayCallContext {
        GatewayCallContext::new(caller, "test-agent").with_session_id("discord-test-agent-111-222")
    }

    // ---- #36: セッション必須アクションの fail-closed ----

    /// セッション文脈の無い実行（session_id: None）では、session に依存する
    /// アクションが "" で黙って進まず明示エラーになること。
    #[tokio::test]
    async fn test_session_required_actions_fail_closed_without_session() {
        let (actions, _db) = make_test_actions();
        let no_session = GatewayCallContext::new(GatewayCaller::Owner, "test-agent");
        // 各アクションの他の必須引数は満たしておき、session 検査だけで落ちることを見る。
        let args = json!({
            "content": "diff",
            "channel_id": "123",
            "message": "m",
            "components": [],
            "task": "t",
        });
        for name in [
            "request_peer_review",
            "report_progress",
            "send_ui",
            "spawn_subtask",
        ] {
            let result = actions.execute(name, &args, &no_session).await;
            assert!(!result.success, "{name} should fail without session");
            assert!(
                result.error.unwrap().contains("セッション"),
                "{name} should mention missing session context"
            );
        }
    }

    // ---- definitions ----

    #[test]
    fn test_definitions_returns_expected_count() {
        let (actions, _db) = make_test_actions();
        let defs = actions.definitions();
        assert_eq!(defs.len(), 28);

        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"request_peer_review"));
        assert!(names.contains(&"discord_list_guilds"));
        assert!(names.contains(&"discord_list_channels"));
        assert!(names.contains(&"discord_channel_config"));
        assert!(names.contains(&"discord_add_reaction"));
        assert!(names.contains(&"discord_create_webhook"));
        assert!(names.contains(&"discord_create_channel"));
        assert!(names.contains(&"update_memory_index_config"));
        assert!(names.contains(&"add_allowed_command"));
        assert!(names.contains(&"list_allowed_commands"));
        assert!(names.contains(&"remove_allowed_command"));
        assert!(names.contains(&"rebuild_memory_index"));
        assert!(names.contains(&"create_skill"));
        assert!(names.contains(&"discord_send_file"));
        assert!(names.contains(&"spawn_subtask"));
        assert!(names.contains(&"cancel_subtask"));
        assert!(names.contains(&"report_progress"));
        assert!(names.contains(&"send_ui"));
        assert!(names.contains(&"update_heartbeat_instructions"));
        assert!(names.contains(&"read_heartbeat_instructions"));
        assert!(names.contains(&"get_default_subtask_webhook"));
        assert!(names.contains(&"set_default_subtask_webhook"));
        assert!(names.contains(&"ensure_subtask_webhook"));
        assert!(names.contains(&"list_subtask_webhooks"));
        assert!(names.contains(&"get_default_webhook"));
        assert!(names.contains(&"set_default_webhook"));
        assert!(names.contains(&"ensure_webhook"));
        assert!(names.contains(&"list_webhooks"));
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

    // ---- request_peer_review (検証エラー系: HTTP呼び出し前に返る) ----

    #[tokio::test]
    async fn test_peer_review_missing_content() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "request_peer_review",
                &json!({"channel_id": "123"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("content"));
    }

    #[tokio::test]
    async fn test_peer_review_missing_or_invalid_channel() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "request_peer_review",
                &json!({"content": "diff"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("channel_id"));

        let result = actions
            .execute(
                "request_peer_review",
                &json!({"content": "diff", "channel_id": "not-a-number"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("無効なchannel_id"));
    }

    #[tokio::test]
    async fn test_peer_review_content_too_long() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "request_peer_review",
                &json!({"content": "x".repeat(12_001), "channel_id": "123"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("12000"));
    }

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

    // ---- create_skill ----

    #[tokio::test]
    async fn test_create_skill_basic() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "create_skill",
                &json!({

                    "name": "天気確認",
                    "description": "curl wttr.inで天気を確認する"
                }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(result.success, "create_skill should succeed");
        let data = result.data.unwrap();
        assert!(data["id"].is_string(), "should return id");
    }

    #[tokio::test]
    async fn test_create_skill_dedup() {
        let (actions, _db) = make_test_actions();
        // Create skill twice
        actions
            .execute(
                "create_skill",
                &json!({

                    "name": "天気確認",
                    "description": "first version"
                }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        let result2 = actions
            .execute(
                "create_skill",
                &json!({

                    "name": "天気確認",
                    "description": "updated version"
                }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(result2.success, "second create should succeed (dedup)");
    }

    #[tokio::test]
    async fn test_create_skill_rejected_for_non_owner() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "create_skill",
                &json!({
                    "name": "test",
                    "description": "test"
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("trusted user"));
    }

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

    #[tokio::test]
    async fn test_update_heartbeat_instructions_rejected_for_non_owner() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "update_heartbeat_instructions",
                &json!({

                    "scope": "agent",
                    "instructions": "話題があるときだけ話す",
                }),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("オーナー"));
        // No audit recorded.
        let conn = db.lock().unwrap();
        let rows = opencrab_db::queries::list_heartbeat_instructions_audit(&conn, "test-agent", 10)
            .unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn test_update_heartbeat_instructions_owner_success_and_audit() {
        let (actions, db) = make_test_actions();
        // Agent row must exist for scope=agent patch.
        {
            let conn = db.lock().unwrap();
            let agent = opencrab_db::queries::AgentRow {
                agent_id: "test-agent".to_string(),
                name: "N".to_string(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "P".to_string(),
                personality: None,
                instructions: String::new(),
                heartbeat_instructions: "OLD".to_string(),
                model: None,
                metadata_json: None,
            };
            opencrab_db::queries::upsert_agent(&conn, &agent).unwrap();
        }
        let result = actions
            .execute(
                "update_heartbeat_instructions",
                &json!({

                    "scope": "agent",
                    "instructions": "NEW指示",
                    "reason": "オーナー依頼",
                }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(
            result.success,
            "owner update should succeed: {:?}",
            result.error
        );

        let conn = db.lock().unwrap();
        let got = opencrab_db::queries::get_agent(&conn, "test-agent")
            .unwrap()
            .unwrap();
        assert_eq!(got.heartbeat_instructions, "NEW指示");
        let rows = opencrab_db::queries::list_heartbeat_instructions_audit(&conn, "test-agent", 10)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].old_value.as_deref(), Some("OLD"));
        assert_eq!(rows[0].new_value.as_deref(), Some("NEW指示"));
        assert_eq!(rows[0].reason.as_deref(), Some("オーナー依頼"));
    }

    #[tokio::test]
    async fn test_read_heartbeat_instructions_effective() {
        let (actions, db) = make_test_actions();
        {
            let conn = db.lock().unwrap();
            opencrab_db::queries::upsert_channel_config(
                &conn,
                &opencrab_db::queries::ChannelConfigRow {
                    channel_id: "ch1".to_string(),
                    agent_id: "test-agent".to_string(),
                    guild_id: "g1".to_string(),
                    channel_name: String::new(),
                    readable: true,
                    writable: true,
                    whitelisted: false,
                    heartbeat_enabled: true,
                    heartbeat_interval_secs: None,
                    heartbeat_instructions: "業務連絡のみ".to_string(),
                },
            )
            .unwrap();
        }
        let result = actions
            .execute(
                "read_heartbeat_instructions",
                &json!({"scope": "effective", "channel_id": "ch1", }),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(result.success);
        let data = result.data.unwrap();
        assert_eq!(data["source"], "channel");
        assert_eq!(data["instructions"], "業務連絡のみ");
    }

    #[tokio::test]
    async fn test_read_heartbeat_instructions_rejected_for_plain_agent() {
        let (actions, _db) = make_test_actions();
        // 素の agent 権限は拒否される。
        let result = actions
            .execute(
                "read_heartbeat_instructions",
                &json!({"scope": "agent"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);

        // co_agent は許可される。
        let allowed = actions
            .execute(
                "read_heartbeat_instructions",
                &json!({"scope": "agent", }),
                &tctx(GatewayCaller::CoAgent {
                    agent_id: "co-agent-1".to_string(),
                }),
            )
            .await;
        assert!(allowed.success);
    }

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

    // ---- subtask webhook gateway actions ----

    const WH_VALID_URL: &str = "https://discord.com/api/webhooks/123456789/abcSECRETtok";
    const WH_SECRET: &str = "abcSECRETtok";

    fn json_has_no_raw_token(v: &serde_json::Value) -> bool {
        !v.to_string().contains(WH_SECRET)
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_requires_owner() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("requires owner"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_self_manage_allowed() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "family": "activity",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(
            result.success,
            "agent self-manage should succeed: {:?}",
            result.error
        );
        let data = result.data.unwrap();
        assert_eq!(data["enabled"], true);

        let conn = db.lock().unwrap();
        let row = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "activity",
        )
        .unwrap()
        .unwrap();
        assert!(row.enabled);
        assert_eq!(row.url, WH_VALID_URL);
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_can_disable_own() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": "",
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(
            result.success,
            "agent disable should succeed: {:?}",
            result.error
        );
        assert_eq!(result.data.unwrap()["enabled"], false);
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_cannot_set_tool_scope() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "tool",
                    "tool_name": "execute_shell",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("forbidden_scope"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_cannot_set_global() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "global",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("forbidden_scope"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_cannot_set_other_agent() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "agent_id": "someone-else",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("forbidden_scope"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_trusted_user_cannot_set() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("requires owner"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_owner_success_redacted() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(
            result.success,
            "owner set should succeed: {:?}",
            result.error
        );
        let data = result.data.unwrap();
        assert!(json_has_no_raw_token(&data), "raw token leaked in response");
        assert!(data["redacted_url"]
            .as_str()
            .unwrap()
            .contains("[redacted]"));

        // stored in DB
        let conn = db.lock().unwrap();
        let row = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "subtask",
        )
        .unwrap()
        .unwrap();
        assert!(row.enabled);
        assert_eq!(row.url, WH_VALID_URL);
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_invalid_url() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": "http://evil.com/x",
                }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("invalid_webhook_url"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_empty_url_disables() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({  "scope": "agent" }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        let conn = db.lock().unwrap();
        let row = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "subtask",
        )
        .unwrap()
        .unwrap();
        assert!(!row.enabled);
    }

    #[tokio::test]
    async fn test_get_default_subtask_webhook_permission_and_redaction() {
        let (actions, db) = make_test_actions();
        // seed an agent default
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

        // bare agent denied
        let denied = actions
            .execute(
                "get_default_subtask_webhook",
                &json!({}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!denied.success);

        // trusted_user allowed, redacted only
        let allowed = actions
            .execute(
                "get_default_subtask_webhook",
                &json!({}),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(allowed.success);
        let data = allowed.data.unwrap();
        assert!(json_has_no_raw_token(&data));
        assert_eq!(data["status"], "ok");
        assert_eq!(data["source"], "agent_default");
    }

    #[tokio::test]
    async fn test_get_default_subtask_webhook_include_secret_rejected() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "get_default_subtask_webhook",
                &json!({  "include_secret": true }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("include_secret"));
    }

    #[tokio::test]
    async fn test_list_subtask_webhooks_permission_and_redaction() {
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

        // bare agent denied
        let denied = actions
            .execute(
                "list_subtask_webhooks",
                &json!({}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!denied.success);

        let allowed = actions
            .execute(
                "list_subtask_webhooks",
                &json!({}),
                &tctx(GatewayCaller::CoAgent {
                    agent_id: "co-agent-1".to_string(),
                }),
            )
            .await;
        assert!(allowed.success);
        let data = allowed.data.unwrap();
        assert!(json_has_no_raw_token(&data), "raw token leaked in list");
        let hooks = data["webhooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1);
        assert!(hooks[0]["redacted_url"]
            .as_str()
            .unwrap()
            .contains("[redacted]"));
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

    // ---- generic webhook action names / default family ----

    /// 汎用 set_default_webhook は既定で family='activity' の行を upsert する。
    #[tokio::test]
    async fn test_generic_set_default_webhook_defaults_to_activity_family() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_webhook",
                &json!({  "scope": "agent", "url": WH_VALID_URL }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(
            result.success,
            "owner set should succeed: {:?}",
            result.error
        );
        let data = result.data.unwrap();
        assert_eq!(data["family"], "activity");
        assert!(json_has_no_raw_token(&data), "raw token leaked in response");

        let conn = db.lock().unwrap();
        // activity 行が作られ、subtask 行は作られない。
        let activity = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "activity",
        )
        .unwrap();
        assert!(activity.is_some(), "activity row should exist");
        assert_eq!(activity.unwrap().url, WH_VALID_URL);
        let subtask = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "subtask",
        )
        .unwrap();
        assert!(
            subtask.is_none(),
            "subtask row must not be created by generic name"
        );
    }

    /// 後方互換 set_default_subtask_webhook は既定で family='subtask' を返しつつ、
    /// agent の通常 tool/command activity へも効くよう activity 行も mirror する。
    #[tokio::test]
    async fn test_subtask_named_set_defaults_to_subtask_and_activity_families() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({  "scope": "agent", "url": WH_VALID_URL }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.data.unwrap()["family"], "subtask");
        let conn = db.lock().unwrap();
        assert!(opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "subtask",
        )
        .unwrap()
        .is_some());
        let activity = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "activity",
        )
        .unwrap();
        assert!(
            activity.is_some(),
            "compat subtask default should also enable activity streaming"
        );
        let resolved = crate::gateway_actions::webhook::resolve_activity_webhook(
            &conn,
            "test-agent",
            "execute_shell",
        );
        assert!(
            matches!(
                resolved,
                crate::gateway_actions::webhook::WebhookResolution::Use { .. }
            ),
            "activity default should resolve after set_default_subtask_webhook"
        );
    }

    /// agent 自身は汎用名でも自分の agent-scope のみ設定でき、他 scope は拒否される。
    #[tokio::test]
    async fn test_generic_set_default_webhook_agent_scope_permission() {
        let (actions, _db) = make_test_actions();
        // 自分の agent-scope は許可。
        let ok = actions
            .execute(
                "set_default_webhook",
                &json!({  "scope": "agent", "url": WH_VALID_URL }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(
            ok.success,
            "agent self-manage should succeed: {:?}",
            ok.error
        );
        // global は拒否。
        let denied = actions
            .execute(
                "set_default_webhook",
                &json!({  "scope": "global", "url": WH_VALID_URL }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!denied.success);
        assert!(denied.error.unwrap().contains("forbidden_scope"));
    }

    /// 汎用 get_default_webhook は activity 行のみを解決する（subtask 行は使わない）。
    #[tokio::test]
    async fn test_generic_get_default_webhook_resolves_activity_only() {
        let (actions, db) = make_test_actions();
        {
            let conn = db.lock().unwrap();
            // subtask 行のみを seed。activity 行は無い。
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
        // activity family の解決では subtask 行に fall through しない → none。
        let activity = actions
            .execute(
                "get_default_webhook",
                &json!({}),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(activity.success);
        let data = activity.data.unwrap();
        assert_eq!(data["status"], "none");
        assert_eq!(data["family"], "activity");
        // subtask family（family 明示）なら解決できる。
        let subtask = actions
            .execute(
                "get_default_webhook",
                &json!({  "family": "subtask" }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert_eq!(subtask.data.unwrap()["status"], "ok");
    }

    /// 汎用 list_webhooks は family で kind を絞り込める。
    #[tokio::test]
    async fn test_generic_list_webhooks_family_filter() {
        let (actions, db) = make_test_actions();
        {
            let conn = db.lock().unwrap();
            for kind in ["subtask", "activity"] {
                let row = opencrab_db::queries::AgentWebhookConfigRow {
                    scope: "agent".to_string(),
                    agent_id: "test-agent".to_string(),
                    tool_name: String::new(),
                    kind: kind.to_string(),
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
        }
        // 絞り込み無し → 両方。
        let all = actions
            .execute("list_webhooks", &json!({}), &tctx(GatewayCaller::Owner))
            .await;
        assert_eq!(all.data.unwrap()["webhooks"].as_array().unwrap().len(), 2);
        // family=activity → 1 件。
        let filtered = actions
            .execute(
                "list_webhooks",
                &json!({  "family": "activity" }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        let hooks = filtered.data.unwrap();
        let arr = hooks["webhooks"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["kind"], "activity");
        assert!(json_has_no_raw_token(&hooks));
    }
}
