// ---- #157 S5: 通知先（webhook）の管理ツール（Discord から移設） ----
//
// 実装は DB と設定ファイル由来の既定値しか触らないのに Discord gateway に
// しか無かった。定義・引数スキーマ・レスポンス JSON・エラー文言は Discord
// 実装から**1 バイトも変えずに**移している。実体は `crate::webhook_targets`。
// 権限はハンドラ内検査のみ（bridge の owner/trusted リストには無い＝単層）。
//
// `ensure_webhook` / `ensure_subtask_webhook` は **Discord に残る**。既存
// デフォルトが無いとき `discord_create_webhook`（serenity 依存）で webhook を
// 新規作成するためで、ここには定義しない。
fn get_default_subtask_webhook_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "get_default_subtask_webhook".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            }
}

fn set_default_subtask_webhook_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "set_default_subtask_webhook".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            }
}

fn list_subtask_webhooks_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "list_subtask_webhooks".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            }
}

fn get_default_webhook_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "get_default_webhook".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            }
}

fn set_default_webhook_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "set_default_webhook".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            }
}

fn list_webhooks_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "list_webhooks".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            }
}

