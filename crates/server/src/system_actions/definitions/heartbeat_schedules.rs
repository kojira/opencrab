// ---- 時間トリガー（#455 / #612）: 間隔（@every）も定時（cron）も同じ 1 行。同じセッションに
// 複数登録でき、行ごとに message（発火時のプロンプト）を持つ。対象は常に ctx.session_id。
fn get_my_schedules_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "get_my_schedules".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）の定時実行スケジュールを、いま話しているセッションについて一覧で読み出す。各要素: id、cron_expr（cron 式または @every 形式）、timezone、message（発火時に自分へ渡される指示文）、enabled、next_fire_at（次に発火する予定時刻。照会時に anchor と最終発火時刻から算出する UTC の RFC3339 文字列。無効・式が不正などでは null）、gated / gated_reason（enabled なのに発火しない状態とその理由）、anchor_at / last_fired_at。他のエージェントや別セッションのスケジュールは読めない。「毎朝 7 時」のような定時実行も「30 分ごと」のような定期（間隔）実行も同じスケジュールで表す（@every 形式が定期実行）。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {}
                }),
            }
}

fn set_my_schedule_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "set_my_schedule".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）の定時実行スケジュールを、いま話しているセッションに対して登録する。定時実行も定期（間隔）実行もこのツールで登録する。同じセッションに複数登録でき、スケジュールごとに message を持つ。対象は常にこのセッションで、配送経路を選ぶ必要はない。cron_expr は「標準 5 フィールド cron」（例: `0 7 * * *` = 毎朝 7 時、`0 */3 * * *` = 3 時間ごとの 0 分）か「@every 形式」（例: `@every 30m`、`@every 3h`、`@every 1h30m`）で指定する。間隔実行は `@every 30m` のように書く。timezone は cron の評価に使う IANA 名で、省略時は Asia/Tokyo。message は発火時に自分へ渡される指示文（例: ニュースを巡回して要約を書く）。cron 式が不正なら登録は拒否され、その場でエラーが返る（実行時に黙って発火しないことはない）ので、エラーが出たら直して呼び直すこと。enabled は省略時 true（登録するとそのまま定期実行が始まる）。登録直後から次回発火時刻が算出され、再起動を待たず即時に反映される。登録直後に即発火はしない（最初の発火は次のスロット）。今すぐ試したいなら run_my_schedule を使う（オーナー / co_agent のみ）。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "cron_expr": {
                            "type": "string",
                            "description": "標準 5 フィールド cron（例: 0 7 * * *）または @every 形式（例: @every 3h / @every 1h30m）。"
                        },
                        "message": {
                            "type": "string",
                            "description": "発火時に自分へ渡される指示文。"
                        },
                        "enabled": {
                            "type": "boolean",
                            "description": "有効にするか。省略時 true（登録すると定期実行が始まる）。false で登録だけして止めておける。"
                        },
                        "timezone": {
                            "type": "string",
                            "description": "cron の評価に使うタイムゾーン（IANA 名・例 Asia/Tokyo）。省略時 Asia/Tokyo。@every では未使用。"
                        }
                    },
                    "required": ["cron_expr", "message"]
                }),
            }
}

// 更新・削除（#477）。set_my_schedule は (session, cron, message) キーの冪等作成なので、
// 既存スケジュールの cron/message を「変える」経路が無い（別行になる）。id 指定の
// update/delete でそれを塞ぐ。id は get_my_schedules が返したもの。**他エージェント・
// 他セッションの id を渡しても触れない**（所属チェック）。
fn update_my_schedule_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "update_my_schedule".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）の定時実行スケジュールを、id 指定で部分更新する。id は get_my_schedules が返したもの（他のエージェントや別セッションのスケジュールは触れない）。変更したい項目だけ渡す（省略した項目は現在の値を保つ）: cron_expr（cron 式または @every 形式に変える＝間隔を変える）、message（発火時の指示文を変える）、timezone、enabled（false にすると止まるが行は残る＝履歴が追える。true で再開）。cron_expr / timezone を変えたときや無効→有効に変えたときは、次回発火が「今」を起点に取り直される。cron 式が不正ならその場でエラーが返る（直して呼び直すこと）。変更項目を 1 つも指定しない呼び出しは拒否される。完全に消したいなら delete_my_schedule を使う。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "integer",
                            "description": "更新するスケジュールの id（get_my_schedules が返した値）。"
                        },
                        "cron_expr": {
                            "type": "string",
                            "description": "新しい cron 式（例: 0 7 * * *）または @every 形式（例: @every 6h）。省略すると現在の値を保つ。"
                        },
                        "message": {
                            "type": "string",
                            "description": "発火時に自分へ渡される新しい指示文。省略すると現在の値を保つ。"
                        },
                        "timezone": {
                            "type": "string",
                            "description": "cron の評価に使うタイムゾーン（IANA 名・例 Asia/Tokyo）。省略すると現在の値を保つ。"
                        },
                        "enabled": {
                            "type": "boolean",
                            "description": "有効にするか。false で止める（行は残り履歴が追える）。true で再開。省略すると現在の値を保つ。"
                        }
                    },
                    "required": ["id"]
                }),
            }
}

fn delete_my_schedule_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "delete_my_schedule".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）の定時実行スケジュールを、id 指定で削除する。id は get_my_schedules が返したもの（他のエージェントや別セッションのスケジュールは削除できない）。行ごと消えるので履歴は残らない。止めるだけで履歴を残したいなら、代わりに update_my_schedule に enabled=false を渡すこと。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "integer",
                            "description": "削除するスケジュールの id（get_my_schedules が返した値）。"
                        }
                    },
                    "required": ["id"]
                }),
            }
}


// #612 D2: 時間を待たずに自分のスケジュールを手動発火する（オーナー / co_agent 限定）。
// 定時発火と同じ経路を通り、last_fired_at は更新しない（定時発火の位相を保つ）。
fn run_my_schedule_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "run_my_schedule".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）の定時実行スケジュールを、id 指定で、次の発火時刻を待たずに今すぐ手動で発火する。id は get_my_schedules が返したもの（他のエージェントや別セッションのスケジュールは発火できない）。テストや動作確認に使う（定時発火とまったく同じ経路を通る）。実際のターンは今のターンが終わってから同じセッションで走る（すぐに投げて返る）。定時発火の位相をずらさないため last_fired_at は更新しない（次回の定期発火時刻は変わらない）。オーナーまたは co_agent のみ実行できる。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "integer",
                            "description": "発火するスケジュールの id（get_my_schedules が返した値）。"
                        }
                    },
                    "required": ["id"]
                }),
            }
}
