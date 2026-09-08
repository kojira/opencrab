// ---- #157 S3: ハートビート指示ツール（Discord から移設） ----
//
// 実装は DB のみに依存していたのに Discord gateway にしか無かった。定義・
// 引数スキーマ・レスポンス JSON は Discord 実装から**1 文字も変えずに**移して
// いる。実体は `crate::heartbeat_instructions`。
// 権限は bridge の `OWNER_ONLY_ACTIONS`（update）/ `TRUSTED_ONLY_ACTIONS`
// （read）が可視性と実行の双方でゲートし、ハンドラ内検査も残す（多層防御）。
// **チャンネル単位の設定は非対称**（`scope="channel"` が触るのは Discord の
// チャンネル設定テーブルなので、非 Discord 経路では通常「行が無い」応答に
// なる）。詳細は `crate::heartbeat_instructions` の doc。
fn update_heartbeat_instructions_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "update_heartbeat_instructions".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Dispatchable, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            }
}

fn read_heartbeat_instructions_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "read_heartbeat_instructions".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            }
}

// ---- #247 段階 2: エージェント自身のハートビート設定 ----
//
// **指示文（`update_heartbeat_instructions`）とは別物**。あちらは
// 「動いたとき何をするか」でオーナー限定のまま。こちらは「いつ動くか」で、
// エージェント自身が触れる（下限つき）。
//
// 引数に `agent_id` は**無い**。対象は常に `ctx.agent_id`（呼び出し文脈）。
// 実体と「自分のだけ」の保証は `crate::agent_heartbeat` の doc を参照。
fn get_my_heartbeat_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "get_my_heartbeat".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）のハートビート設定を、いま話しているセッションについて読み出す。返り値: enabled（有効か）、interval_secs（実効間隔・秒）、next_fire_at（このセッションのハートビートがゲートされていない場合に次に発火する予定時刻。照会した時点で anchor_at と最終発火時刻から算出する値で、UTC の RFC3339 文字列。無効・発火経路なし・間隔が不正などでは null。gated=true のときはこの時刻が来ても実際には発火しない）、anchor_at / last_fired_at（起点と最終発火時刻。同じく UTC RFC3339 か null）、min/max/default_interval_secs（設定できる下限・上限・既定）。設定したことが無ければ無効。有効なのに発火しないときは gated=true と、その理由 gated_reason（例: グローバルのハートビートが無効化されている / 間隔が不正）を返すので、なぜ発火しないのかを自分で把握できる。他のエージェントの設定は読めない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {}
                }),
            }
}

fn set_my_heartbeat_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "set_my_heartbeat".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）のハートビート（自律実行）の有効/無効と間隔を、いま話しているセッションに対して設定する。対象は常にこのセッション（Nostr の自発投稿、またはこの Discord チャンネル）で、どこに設定するか選ぶ必要はない。他のエージェントや別のチャンネルの設定は変えられない。間隔には運用者が決めた下限があり、それより短い値は拒否される（丸められない）ので、拒否されたらエラーに載っている下限以上で指定し直すこと。有効にした直後から次回発火時刻が算出され、再起動を待たず即時に反映される。発火タイミングは非対称: 一度も発火していないセッションを初めて有効化したときは間隔をまるごと待つ（今すぐは発火しない）が、既に発火したことがあるセッションの再有効化や間隔の短縮では、前回発火（や起点）＋新しい間隔が既に過ぎていれば直ちに発火する（設定変更で発火の記録は消えない）。今すぐ試したいなら run_my_heartbeat を使う。ハートビートで何をするかの指示文はこのツールでは変えられない（オーナー限定の別ツール）。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "enabled": {
                            "type": "boolean",
                            "description": "自律実行を有効にするか。省略すると現在の値を保つ。"
                        },
                        "interval_secs": {
                            "type": "integer",
                            "description": "ハートビートの間隔（秒）。下限は get_my_heartbeat の min_interval_secs、上限は max_interval_secs。null を渡すと運用者の既定に戻る。省略すると現在の値を保つ。"
                        }
                    }
                }),
            }
}

// #599: 時間を待たずにハートビートを手動発火する（テスト用・オーナー / co_agent 限定）。
// 時間発火とまったく同じ経路を通り、last_fired_at は更新しない（時間発火の位相を保つ）。
fn run_my_heartbeat_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "run_my_heartbeat".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）のハートビートを、次の発火時刻を待たずに今すぐ手動で発火する。テストや動作確認に使う（時間発火とまったく同じ経路——宣言→サブタスク→継続→投稿——を通るので、待たずに一連の流れを検証できる）。対象は省略すると「いま話しているセッション」、session_id を渡せばそのセッション。発火先は Discord チャンネルまたは Nostr の自発投稿で、発火経路の無いセッション種別は拒否される。実際のターンは今のターンが終わってから走る（すぐに投げて返る）。time-fire の位相をずらさないため last_fired_at は更新しない（次回の定期発火時刻は変わらない）。オーナーまたは co_agent のみ実行できる。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "発火する対象セッションの session_id（discord-… / nostr-…）。省略すると、いま話しているセッションを発火する。"
                        }
                    }
                }),
            }
}

// ---- 定時実行（#455）: ハートビート（固定短間隔の tick）とは別に、cron / @every で
// 「時刻・周期ベース」の自律実行を自分で登録できる。対象は常に ctx.session_id。
// 語彙はハートビートに揃える（next_fire_at / gated / gated_reason）。
fn get_my_schedules_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "get_my_schedules".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）の定時実行スケジュールを、いま話しているセッションについて一覧で読み出す。各要素: id、cron_expr（cron 式または @every 形式）、timezone、message（発火時に自分へ渡される指示文）、enabled、next_fire_at（次に発火する予定時刻。照会時に anchor と最終発火時刻から算出する UTC の RFC3339 文字列。無効・式が不正などでは null）、gated / gated_reason（enabled なのに発火しない状態とその理由）、anchor_at / last_fired_at。他のエージェントや別セッションのスケジュールは読めない。定時実行はハートビート（固定短間隔）とは別物で、「毎朝 7 時」「3 時間ごと」のような時刻・周期ベースの自律実行。".to_string(),
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
                description: "自分（呼び出し元エージェント）の定時実行スケジュールを、いま話しているセッションに対して登録する。ハートビート（固定短間隔の tick）とは別の、時刻・周期ベースの自律実行。対象は常にこのセッション（Nostr の自発投稿、またはこの Discord チャンネル）で、どこに登録するか選ぶ必要はない。cron_expr は「標準 5 フィールド cron」（例: `0 7 * * *` = 毎朝 7 時、`0 */3 * * *` = 3 時間ごとの 0 分）か「@every 形式」（例: `@every 3h`、`@every 1h30m`、`@every 45m`）で指定する。timezone は cron の評価に使う IANA 名で、省略時は Asia/Tokyo。message は発火時に自分へ渡される指示文（例: ニュースを巡回して要約を書く）。cron 式が不正なら登録は拒否され、その場でエラーが返る（実行時に黙って発火しないことはない）ので、エラーが出たら直して呼び直すこと。enabled は省略時 true（登録するとそのまま定期実行が始まる）。登録直後から次回発火時刻が算出され、再起動を待たず即時に反映される。運用者がハートビートを無効化していても、定時実行は止まらない（別概念）。".to_string(),
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

