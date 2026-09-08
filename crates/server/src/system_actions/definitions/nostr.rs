#[cfg(feature = "nostr")]
fn configure_nostr_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "configure_nostr".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description:
                    "自分の Nostr 連携設定（購読リレー・フィルタ authors/keywords/kinds・\
                有効/無効・Nostr でのオーナーの公開鍵）を変更する（owner 限定）。\
                秘密鍵は変更も取得もできない（鍵生成は別手段）。\
                省略したフィールドは現状維持。enabled=true にするには author か keyword が必要。\
                設定は保存と同時にマネージャへ反映（enabled なら起動 / 無効なら停止）。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "relays": {
                            "type": "array", "items": {"type": "string"},
                            "description": "購読リレー URL 一覧（例: wss://yabu.me）。"
                        },
                        "authors": {
                            "type": "array", "items": {"type": "string"},
                            "description": "購読する author（npub/hex）。"
                        },
                        "keywords": {
                            "type": "array", "items": {"type": "string"},
                            "description": "購読キーワード。"
                        },
                        "kinds": {
                            "type": "array", "items": {"type": "integer"},
                            "description": "購読する kind 番号。DM の kind（4 / 1059）は指定しても\
                            無視される（#514: DM は扱わない。private な話は Discord で）。"
                        },
                        "enabled": {
                            "type": "boolean",
                            "description": "有効化して起動 / 無効化して停止。"
                        },
                        "owner_pubkey": {
                            "type": "string",
                            "description": "Nostr でのオーナーの公開鍵（npub1... または 64 桁 hex）。\
                            この鍵から届いたメッセージだけが owner 権限のターンになる。\
                            未設定のうちは Nostr からは誰も owner にならないので、\
                            最初の 1 回は Discord など owner 権限のある経路から設定する。\
                            \"\" を渡すと未設定に戻る。"
                        }
                    }
                }),
            }
}

// bootstrap ツール（鍵不要）。送信系（nostr_post 等・鍵前提）とは分離し、
// transport 非依存で全ターンに露出する。これにより「鍵を作るツールが鍵の
// ある時しか出ない」循環依存（#141）を解消する。owner 限定にはしない
// （nsec は返さず・送信もしないので Agent 呼び出しでも安全）。
#[cfg(feature = "nostr")]
fn nostr_generate_key_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "nostr_generate_key".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Dispatchable, sub_engine: opencrab_gateway::SubEngineAccess::Allowed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "新しい Nostr 鍵（keypair）を生成する。任意で vanity prefix（npub の \
                              npub1 以降・bech32 文字のみ。長さ上限は無いが、長いほど探索に時間が \
                              かかる＝3文字程度で即時、それ以上は徐々に長くなる）を指定できる。返るのは公開情報の \
                              npub / pubkey のみ。**秘密鍵(nsec)はサーバ内に安全に保存され、あなた（LLM）\
                              には渡されない**（セキュリティのため）。これは新規 keypair を作るユーティリティ\
                              であり、あなた自身の送信用アイデンティティは変更しない。"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "prefix": {"type": "string", "description": "任意。npub の npub1 以降に前置したい bech32 文字列（長さ上限なし。長いほど探索に時間がかかる, 例: cat）。"}
                    }
                }),
            }
}

// bootstrap ツール（鍵不要）。`nostr_generate_key` と対で、生成した鍵の npub
// 一覧を返す（採用候補の確認）。transport 非依存で全ターンに露出する。
// owner 限定にはしないが、bridge の `TRUSTED_ONLY_ACTIONS` により未信頼の
// 会話ターン（caller=Agent）には出さない（`nostr_switch_identity` と同じ扱い）。
#[cfg(feature = "nostr")]
fn nostr_list_keys_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "nostr_list_keys".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分が nostr_generate_key で生成した鍵の一覧（npub のみ）を返す。\
                              nostr_switch_identity で本鍵に採用する候補を確認するのに使う。\
                              返るのは公開情報の npub だけで、**秘密鍵(nsec)は一切返らない**。"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {}
                }),
            }
}

// bootstrap ツール（鍵不要）。生成鍵を本鍵として採用し、**未設定でも自力で
// 接続する**（採用時は絞り込みを自動設定せず、nostaro の mention-only 既定に
// 委ねて自分宛のみを購読する / #271）。
// transport 非依存で全ターンに露出する。bridge の `TRUSTED_ONLY_ACTIONS` に
// より未信頼の会話ターン（caller=Agent）には出さない（乗っ取り防止 / #264）。
#[cfg(feature = "nostr")]
fn nostr_switch_identity_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "nostr_switch_identity".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分が nostr_generate_key で生成した鍵を、この Nostr ゲートウェイの\
                              **本鍵（送信・受信のアイデンティティ）として採用**する。以後の投稿は\
                              その鍵で行われる。まだ Nostr に接続していなければ、この操作で自動的に\
                              接続まで行う（自分への言及を購読する最小フィルタを設定して起動する）。\
                              npub には nostr_generate_key で作った鍵の npub を渡す。重要な操作なので\
                              owner（信頼ユーザー）からの依頼時のみ実行される。秘密鍵は扱わない\
                              （npub 参照のみ）。"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "npub": {"type": "string", "description": "本鍵に採用する、生成済み鍵の npub。"}
                    },
                    "required": ["npub"]
                }),
            }
}

// ---- #252 段階 C: エージェント自身の Nostr 受信 → Discord 転記先設定 ----
//
// 段階 A（#253）が敷いた `agent_nostr_relay_config` を、エージェント自身が
// own ツールで読み書きする。引数に `agent_id` は**無い**。対象は常に
// `ctx.agent_id`（呼び出し文脈）で、他エージェントを指す経路は作らない。
// 実体と「自分のだけ」の保証・秘匿値の扱いは `crate::agent_nostr_relay` の doc。
#[cfg(feature = "nostr")]
fn get_my_nostr_relay_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "get_my_nostr_relay".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）の Nostr 受信 → Discord 転記の設定を読み出す。転記が有効か・転記先が設定済みか（転記先 URL は伏字で返す）を返す。他のエージェントの設定は読めない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                }),
            }
}

#[cfg(feature = "nostr")]
fn set_my_nostr_relay_definition() -> GatewayActionDef {
    GatewayActionDef {
                name: "set_my_nostr_relay".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）が Nostr で受け取った自分宛の受信（メンション/リプライ/DM）を Discord へ転記する設定を更新する。対象は常に自分で、他のエージェントの設定は変えられない。enabled で転記の有効/無効を、webhook_url で転記先の Discord webhook URL を設定する。URL が Discord webhook として不正なら拒否される（丸められない）ので、拒否されたらエラーの理由を見て正しい URL で指定し直すこと。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "enabled": {
                            "type": "boolean",
                            "description": "転記を有効にするか。省略すると現在の値を保つ。"
                        },
                        "webhook_url": {
                            "type": "string",
                            "description": "転記先の Discord webhook URL。空文字または null を渡すと転記先を消去する。省略すると現在の値を保つ。"
                        }
                    }
                }),
            }
}

