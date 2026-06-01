//! DiscordゲートウェイアクションのGatewayActions実装
//!
//! Discord管理操作（サーバー一覧、チャンネル一覧、チャンネル設定）を
//! ゲートウェイ固有アクションとして提供する。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dashmap::DashMap;
use opencrab_gateway::{GatewayActionDef, GatewayActionResult, GatewayActions};
use serde_json::json;
use serenity::http::Http;
use tokio::task::AbortHandle;

use crate::PendingInteractionRegistry;
use crate::message_loop::LoopEvent;

mod agent_management;
mod discord_ops;
mod subtask_engine;
mod ui;
mod webhook;

use webhook::{DeliveryBatch, WebhookConfig};

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

/// Callback invoked when a subtask completes.
/// Args: (subtask_id: String, result: String, exit_reason: String)
pub type SubtaskCompletionFn = Arc<dyn Fn(String, String, String) + Send + Sync>;

/// Registry of completion callbacks keyed by parent_session_id.
pub type CompletionRegistry = Arc<DashMap<String, SubtaskCompletionFn>>;

/// Registry of active subtasks keyed by subtask_id.
pub type SubtaskRegistry = Arc<DashMap<String, SpawnedSubtask>>;

struct ArcLlmClient(Arc<dyn opencrab_core::LlmClient>);

#[async_trait::async_trait]
impl opencrab_core::LlmClient for ArcLlmClient {
    async fn chat(
        &self,
        request: opencrab_core::ChatRequestSimple,
    ) -> anyhow::Result<opencrab_core::ChatResponseSimple> {
        self.0.chat(request).await
    }
}

/// Discord固有のゲートウェイアクション実装。
///
/// serenityのHTTPクライアントとDB接続を保持し、
/// Discord管理操作をGatewayActionsとして提供する。
pub struct DiscordGatewayActions {
    http: Arc<Http>,
    db: Arc<Mutex<rusqlite::Connection>>,
    agent_id: String,
    tools_config: Arc<std::sync::RwLock<opencrab_actions::tools::ToolsConfig>>,
    llm_client: Option<Arc<dyn opencrab_core::LlmClient>>,
    default_model: String,
    workspace_root: PathBuf,
    subtask_registry: SubtaskRegistry,
    completion_registry: CompletionRegistry,
    /// Subtask lifecycle webhook 配送用の HTTP クライアント（worker で共有）。
    webhook_client: reqwest::Client,
    pub pending_interaction_registry: Option<PendingInteractionRegistry>,
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<LoopEvent>>,
}

impl DiscordGatewayActions {
    pub fn new(
        http: Arc<Http>,
        db: Arc<Mutex<rusqlite::Connection>>,
        agent_id: String,
        tools_config: Arc<std::sync::RwLock<opencrab_actions::tools::ToolsConfig>>,
        llm_client: Option<Arc<dyn opencrab_core::LlmClient>>,
        default_model: String,
        workspace_root: PathBuf,
        subtask_registry: SubtaskRegistry,
        completion_registry: CompletionRegistry,
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
            completion_registry,
            webhook_client: reqwest::Client::new(),
            pending_interaction_registry: None,
            event_tx: None,
        }
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
                            "description": "subtask lifecycle を Discord webhook へ通知する設定（省略可）。",
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
        ]
    }

    async fn execute(&self, name: &str, args: &serde_json::Value) -> GatewayActionResult {
        match name {
            "discord_list_guilds" => self.execute_list_guilds().await,
            "discord_list_channels" => self.execute_list_channels(args).await,
            "discord_channel_config" => self.execute_discord_channel_config(args),
            "discord_add_reaction" => self.execute_discord_add_reaction(args).await,
            "discord_create_webhook" => self.execute_discord_create_webhook(args).await,
            "update_memory_index_config" => self.execute_update_memory_index_config(args),
            "add_allowed_command" => self.execute_add_allowed_command(args),
            "list_allowed_commands" => self.execute_list_allowed_commands(),
            "remove_allowed_command" => self.execute_remove_allowed_command(args),
            "rebuild_memory_index" => self.execute_rebuild_memory_index().await,
            "create_skill" => self.execute_create_skill(args),
            "discord_send_file" => self.execute_send_file(args).await,
            "spawn_subtask" => self.execute_spawn_subtask(args).await,
            "cancel_subtask" => self.execute_cancel_subtask(args),
            "report_progress" => self.execute_report_progress(args).await,
            "send_ui" => self.execute_send_ui(args).await,
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
    use serde_json::json;

    /// テスト用: serenity Httpは不要だがDiscordGatewayActionsの構築に必要。
    /// channel_config系テストではHTTP呼び出しは発生しないのでダミーでOK。
    fn make_test_actions() -> (DiscordGatewayActions, Arc<Mutex<rusqlite::Connection>>) {
        let db = Arc::new(Mutex::new(opencrab_db::init_memory().unwrap()));
        // serenityのHttpはダミートークンで作成（API呼び出しはしない）
        let http = Arc::new(Http::new("dummy-token"));
        let tools_config = Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        ));
        let subtask_registry: SubtaskRegistry = Arc::new(DashMap::new());
        let completion_registry: CompletionRegistry = Arc::new(DashMap::new());
        let actions = DiscordGatewayActions::new(
            http,
            db.clone(),
            "test-agent".to_string(),
            tools_config,
            None,
            String::new(),
            std::path::PathBuf::from("/tmp"),
            subtask_registry,
            completion_registry,
        );
        (actions, db)
    }

    // ---- definitions ----

    #[test]
    fn test_definitions_returns_expected_count() {
        let (actions, _db) = make_test_actions();
        let defs = actions.definitions();
        assert_eq!(defs.len(), 16);

        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"discord_list_guilds"));
        assert!(names.contains(&"discord_list_channels"));
        assert!(names.contains(&"discord_channel_config"));
        assert!(names.contains(&"discord_add_reaction"));
        assert!(names.contains(&"discord_create_webhook"));
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
        let cfg = opencrab_db::queries::get_channel_config(&conn, "ch-1")
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
            )
            .await;
        assert!(result.success);

        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config(&conn, "ch-1")
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
            .execute("discord_channel_config", &json!({"channel_id": "ch-1"}))
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
            )
            .await;
        assert!(result.success);

        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config(&conn, "ch-1")
            .unwrap()
            .unwrap();
        assert_eq!(cfg.channel_name, "");
    }

    // ---- unknown action ----

    #[tokio::test]
    async fn test_unknown_gateway_action() {
        let (actions, _db) = make_test_actions();
        let result = actions.execute("nonexistent", &json!({})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown gateway action"));
    }

    // ---- list_channels パラメータバリデーション ----

    #[tokio::test]
    async fn test_list_channels_missing_guild_id() {
        let (actions, _db) = make_test_actions();
        let result = actions.execute("discord_list_channels", &json!({})).await;
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
                    "__caller": "owner",
                    "name": "天気確認",
                    "description": "curl wttr.inで天気を確認する"
                }),
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
                    "__caller": "owner",
                    "name": "天気確認",
                    "description": "first version"
                }),
            )
            .await;
        let result2 = actions
            .execute(
                "create_skill",
                &json!({
                    "__caller": "owner",
                    "name": "天気確認",
                    "description": "updated version"
                }),
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
            .execute("discord_send_file", &json!({"channel_id": "123"}))
            .await;
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("file_path"));
    }
}
