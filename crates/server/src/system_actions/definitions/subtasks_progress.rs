// `nostr_run`（薄い nostaro passthrough / #268）は**そもそも使えないように**定義から
// 撤去した（オーナー裁定）。返信は core の say 一本（gateway が対象ノートへの nostaro
// reply として投稿する / #840）、独立投稿は nostr_post を使う。dispatch では名前指定
// で来ても fail-close で拒否する（`self.nostr_run` は呼ばない・impl も削除済み）。
// 実行中の subtask を停止するツール（#161）。Discord gateway 実装だけに
// あった cancel_subtask を server-neutral 層へ露出し、web/Nostr/REST でも
// 自動 dispatch された subtask を停止できるようにする。認可（親セッション/
// owner 限定）は共有 registry を引く実体（cancel_subtask）が担う。
// サブタスクの起動（#175 S4）。Discord gateway 実装だけにあった
// spawn_subtask を server-neutral 層へ移し、web / REST / Nostr / heartbeat
// でもサブタスクを起動できるようにする。実体は
// `crate::subtask_spawn::spawn_subtask`（sub-engine は自前で組まず
// `run_agent_response` を depth+1 で再入呼び出しする）。
fn spawn_subtask_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "spawn_subtask".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "バックグラウンドでサブタスクを起動します。LLMエンジンがサブエンジンとして非同期実行し、完了後にメインエンジンを自動的に再呼び出しします。複雑な長時間処理（画像生成・コード実装・調査など）に使用してください。".to_string(),
                parameters: json!({
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
                        "label": {
                            "type": "string",
                            "description": "サブタスクのラベル（通知の表示用。省略時はtask先頭を使用）"
                        },
                        "webhook": {
                            "type": "object",
                            "description": "subtask lifecycle の通知先（省略時はエージェント既定 / グローバル既定を使用）。",
                            "properties": {
                                "url": {
                                    "type": "string",
                                    "description": "通知先の webhook URL"
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
            }
}

fn cancel_subtask_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "cancel_subtask".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "実行中のサブタスクをキャンセルします。キャンセルできるのは自分のセッションが親のサブタスクのみ（owner は制限なし）。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "subtask_id": {
                            "type": "string",
                            "description": "キャンセルするサブタスクのID（subtask_spawnedイベントから取得）"
                        }
                    },
                    "required": ["subtask_id"]
                }),
            }
}

fn steer_subtask_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "steer_subtask".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "走行中のサブタスクを止めずに追加の指示（steer）を送ります。指示はサブタスクの次の反復の合間に読まれ、以後の判断へ反映されます。送れるのは自分のセッションが親のサブタスクのみ（owner は制限なし）。明示的な spawn_subtask のサブにのみ有効で、auto-dispatch のサブや既に完了/停止したサブへ送った場合はその旨が返ります。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "subtask_id": {
                            "type": "string",
                            "description": "追加指示を送るサブタスクのID（subtask_spawnedイベントから取得）"
                        },
                        "message": {
                            "type": "string",
                            "description": "サブタスクへ送る追加指示（方向転換・条件追加・見落としの伝達など）"
                        }
                    },
                    "required": ["subtask_id", "message"]
                }),
            }
}

// サブタスクの進捗報告（#175 S1）。Discord gateway 実装だけにあった
// report_progress を server-neutral 層へ露出し、web / Nostr / REST /
// heartbeat でもサブエンジンが進捗を報告できるようにする。引数スキーマは
// Discord 側の定義と同一（sub-engine の system prompt が「subtask_id は
// 省略可」と案内している契約を保つ）。
fn report_progress_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "report_progress".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::Allowed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "サブエンジンからメインエンジンへ進捗を報告します。depth >= 1のサブエンジンのみ使用可能。".to_string(),
                parameters: json!({
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
            }
}

