// 記憶インデックスの全再構築（#175 S4）。Discord gateway 実装だけにあった
// ものを server-neutral 層へ移す。LLM クライアントを必要とする唯一の
// Discord ツールだったため、これを移すことで discord crate が LLM を
// 知らなくなる（#155 の前進）。
fn rebuild_memory_index_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "rebuild_memory_index".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Dispatchable, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "メモリインデックスをゼロから再構築する。既存のインデックスを削除し、全ログを再インデックスする。時間がかかることがある。結果として logs_indexed（処理したログ数）と nodes_created（作成したインデックスノード数）を返す。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            }
}

// ---- #157 S1: gateway 非依存の汎用管理ツール（Discord から移設） ----
//
// 以下 4 つは実装が serenity を一切参照せず DB と実行許可設定だけに依存して
// いたのに、Discord gateway にしか無かったため web / Nostr / REST / heartbeat
// 経由のターンでは使えなかった（#157 / #155）。定義・引数スキーマ・
// レスポンス JSON はすべて Discord 実装から**1 文字も変えずに**移している。
// 実体は `crate::agent_management`。
fn update_memory_index_config_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "update_memory_index_config".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Dispatchable, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            }
}

fn add_allowed_command_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "add_allowed_command".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            }
}

fn list_allowed_commands_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "list_allowed_commands".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "execute_shell で実行できる許可コマンドの一覧（実効リスト）を\
                取得する。設定ファイル由来のものと自分に追加されたものを合わせて返す（#300）。"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            }
}

fn remove_allowed_command_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "remove_allowed_command".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            }
}

// ---- #157 S6: スキル生成（Discord から移設） ----
//
// 実装は DB のみに依存していたのに Discord gateway にしか無かった。定義・
// 引数スキーマ・レスポンス JSON・エラー文言は Discord 実装から**1 バイトも
// 変えずに**移している。実体は `crate::agent_management::create_skill`。
//
// 権限は bridge の `TRUSTED_ONLY_ACTIONS`（可視性 + 実行の双方）とハンドラ内
// 検査の**二重構造**。許可集合は owner / co_agent / trusted_user で完全一致して
// おり、bridge 側は名前ベースなので移設しても効き続ける。
//
// 似た名前の core アクション `create_my_skill`（`source_type="self_created"` /
// `situation_pattern` 必須）とは**統合しない**。#157 の目的は「汎用の実体を
// transport 層から出す」ことで、重複解消は別の話。ツール名を消すと過去の
// 会話ログに残る呼び出しが通らなくなる。
fn create_skill_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "create_skill".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Dispatchable, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            }
}

