//! エージェント（owner 限定）が OpenCrab 自体の設定を変更するためのサーバ内ツール源。
//!
//! `AppState`（db / llm_router / llm_config）を必要とするため、素の dispatcher
//! アクションでは配線できない。`GatewayActions` として実装し、`BridgedExecutor` の
//! 単一 `gateway_actions` スロットに載せる。既存の gateway（Discord/Nostr 等）を
//! `inner` として保持し、自分が扱わないツールは委譲する（composite）ことで、
//! transport 非依存に「設定ツール」を全ターンへ供給する。
//!
//! owner ゲートは bridge の `OWNER_ONLY_ACTIONS`（可視性 + 実行の双方）が担うが、
//! 多層防御として本ハンドラでも caller を確認する（fail-closed）。

use std::sync::Arc;

use async_trait::async_trait;
use opencrab_gateway::{
    GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext, GatewayCaller,
};
use opencrab_mcp::is_valid_server_name;
use serde_json::{json, Value};

use crate::AppState;

/// `configure_llm_provider` などのサーバ内設定ツールを提供する `GatewayActions`。
pub struct SystemGatewayActions {
    state: AppState,
    /// transport 固有の gateway（Discord/Nostr 等）。自分が扱わないツールを委譲する。
    inner: Option<Arc<dyn GatewayActions>>,
}

impl SystemGatewayActions {
    pub fn new(state: AppState, inner: Option<Arc<dyn GatewayActions>>) -> Self {
        Self { state, inner }
    }

    /// 本ツール源が直接提供するツール定義。
    fn own_definitions() -> Vec<GatewayActionDef> {
        vec![
            GatewayActionDef {
                name: "configure_llm_provider".to_string(),
                description:
                    "LLM プロバイダの設定を即時適用する（owner 限定）。DB オーバーライドに\
                保存してルーターをホットスワップするため再起動は不要。codex/cursor は適用後に\
                起動確認（health_check）を行い、失敗した場合は自動的に直前の設定へロールバック\
                して結果で通知する。acp と API キー型は自動ロールバックの対象外（acp の起動確認は\
                ネットワーク依存で誤判定しうるため。ダッシュボードの接続テストで明示的に確認する）。\
                各フィールドは三値: 省略=変更しない / null=オーバーライド解除（TOML に戻す）/ 値=上書き。\
                api_key はこのツールでは変更できない（ダッシュボードから設定する）。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "provider": {
                            "type": "string",
                            "description": "対象プロバイダ名（例: acp, codex, cursor, openai）。"
                        },
                        "enabled": {
                            "type": ["boolean", "null"],
                            "description": "有効/無効。null で解除。"
                        },
                        "default_model": {
                            "type": ["string", "null"],
                            "description": "既定モデル。null で解除。"
                        },
                        "binary_path": {
                            "type": ["string", "null"],
                            "description": "起動バイナリ（subprocess）。空文字/null で解除。"
                        },
                        "args": {
                            "type": ["array", "null"],
                            "items": { "type": "string" },
                            "description": "起動引数（subprocess）。null で解除。"
                        },
                        "working_dir": {
                            "type": ["string", "null"],
                            "description": "作業ディレクトリ。空文字/null で解除。"
                        },
                        "timeout_secs": {
                            "type": ["integer", "null"],
                            "description": "タイムアウト秒。null で解除。"
                        },
                        "reasoning_effort": {
                            "type": ["string", "null"],
                            "description": "推論強度（low/medium/high 等）。空文字/null で解除。"
                        },
                        "base_url": {
                            "type": ["string", "null"],
                            "description": "API ベース URL。null で解除。"
                        }
                    },
                    "required": ["provider"]
                }),
            },
            GatewayActionDef {
                name: "manage_allowed_commands".to_string(),
                description:
                    "自分（このエージェント）が execute_shell で実行できる許可コマンドを\
                管理する（owner 限定）。許可コマンドの追加はシェル実行範囲を広げるため owner のみ。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "add", "remove"],
                            "description": "list=一覧 / add=追加 / remove=削除。"
                        },
                        "command": {
                            "type": "string",
                            "description": "add/remove 対象のコマンド（例: git, cargo）。list では不要。"
                        }
                    },
                    "required": ["action"]
                }),
            },
            GatewayActionDef {
                name: "configure_nostr".to_string(),
                description:
                    "自分の Nostr 連携設定（購読リレー・フィルタ authors/keywords/kinds・\
                有効/無効）を変更する（owner 限定）。秘密鍵は変更も取得もできない（鍵生成は別手段）。\
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
                            "description": "購読する kind 番号。"
                        },
                        "enabled": {
                            "type": "boolean",
                            "description": "有効化して起動 / 無効化して停止。"
                        }
                    }
                }),
            },
            GatewayActionDef {
                name: "configure_self".to_string(),
                description:
                    "自分（このエージェント）の人格・モデル・推論強度・web 検索などの設定を変更する\
                （owner 限定）。model/reasoning_effort/web_search の変更は次ターン以降に反映される。\
                指示文の変更は update_instructions / update_heartbeat_instructions を使う。\
                省略したフィールドは変更しない。null で解除（既定に戻す。persona_name は解除不可）。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "persona_name": {"type": "string", "description": "ペルソナ名。"},
                        "personality": {"type": ["string", "null"], "description": "性格・思考スタイル。"},
                        "job_title": {"type": ["string", "null"], "description": "肩書き。"},
                        "organization": {"type": ["string", "null"], "description": "所属。"},
                        "model": {"type": ["string", "null"], "description": "既定モデル（provider:model）。次ターン以降に反映。"},
                        "reasoning_effort": {"type": ["string", "null"], "description": "推論強度（low/medium/high 等）。"},
                        "web_search": {"type": ["boolean", "null"], "description": "本文URL読取り/web 検索の有効化。"}
                    }
                }),
            },
            GatewayActionDef {
                name: "configure_mcp_server".to_string(),
                description:
                    "自分の MCP サーバ設定を管理する（owner 限定）。追加/更新・削除・有効切替が\
                でき、変更後は接続をバックグラウンドで貼り直す。env の値は結果に出さない（キー名のみ）。\
                add で env を省略すると既存の env を保持する。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "add", "remove", "set_enabled"],
                            "description": "list=一覧 / add=追加更新 / remove=削除 / set_enabled=有効切替。"
                        },
                        "name": {"type": "string", "description": "サーバ論理名（英数字・_・-、__ 不可）。list 以外で必須。"},
                        "command": {"type": "string", "description": "起動コマンド（add で必須、例: npx）。"},
                        "args": {"type": "array", "items": {"type": "string"}, "description": "起動引数。"},
                        "env": {"type": "object", "description": "追加環境変数（キー→値）。省略で既存保持。値は結果に出さない。"},
                        "trusted_only": {"type": "boolean", "description": "true で owner/trusted のターンのみ露出。"},
                        "enabled": {"type": "boolean", "description": "有効/無効。"}
                    },
                    "required": ["action"]
                }),
            },
            // bootstrap ツール（鍵不要）。送信系（nostr_post 等・鍵前提）とは分離し、
            // transport 非依存で全ターンに露出する。これにより「鍵を作るツールが鍵の
            // ある時しか出ない」循環依存（#141）を解消する。owner 限定にはしない
            // （nsec は返さず・送信もしないので Agent 呼び出しでも安全）。
            GatewayActionDef {
                name: "nostr_generate_key".to_string(),
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
            },
        ]
    }

    /// bootstrap 用の鍵生成（鍵未設定でも実行可能）。実体は `NostaroCli::vanity`
    /// （config 非依存）で、生成した nsec は**サーバ内に 0600 で保存**し LLM には返さない
    /// （npub/pubkey のみ）。process.rs の防御マスク（tool_name==nostr_generate_key）と
    /// bridge の nsec redaction が多層で秘密漏洩を防ぐ。
    async fn nostr_generate_key(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        let prefix = args
            .get("prefix")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .unwrap_or("");
        // 稼働中のマネージャの CLI（binary_path 等の設定を継承）を使う。無ければ既定。
        let cli = self
            .state
            .nostr_manager
            .as_ref()
            .map(|m| m.cli().clone())
            .unwrap_or_default();
        match cli.vanity(prefix).await {
            Ok(k) => match opencrab_nostr::NostaroCli::save_generated_key(&ctx.agent_id, &k) {
                Ok(_) => GatewayActionResult {
                    success: true,
                    // nsec は返さない（サーバ内 0600 保存済み）。npub/pubkey のみ。
                    data: Some(json!({
                        "npub": k.npub,
                        "pubkey": k.pubkey,
                        "note": "新しい鍵を生成しました。秘密鍵(nsec)はサーバ内に安全に保存済みで、セキュリティ上あなた（LLM）には渡していません。共有・言及してよいのは npub までです。",
                    })),
                    error: None,
                },
                Err(e) => err(format!("鍵は生成しましたが保存に失敗しました: {e}")),
            },
            Err(e) => err(format!("nostr_generate_key 失敗: {e}")),
        }
    }

    async fn configure_llm_provider(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("configure_llm_provider requires owner".to_string());
        }
        let Some(provider) = args.get("provider").and_then(|v| v.as_str()) else {
            return err("provider is required".to_string());
        };
        let provider = provider.to_string();

        // LLM 由来の args から許可フィールドだけを抜き出して三値ボディを組む。
        // api_key は意図的に受け付けない（秘密情報を LLM 経路・ログに載せない）。
        let mut body = serde_json::Map::new();
        for key in [
            "enabled",
            "default_model",
            "binary_path",
            "args",
            "working_dir",
            "timeout_secs",
            "reasoning_effort",
            "base_url",
        ] {
            if let Some(v) = args.get(key) {
                body.insert(key.to_string(), v.clone());
            }
        }

        match crate::api::providers::apply_provider_override_with_rollback(
            &self.state,
            &provider,
            &body,
        )
        .await
        {
            Ok(outcome) => {
                let data = json!({
                    "provider": provider,
                    "applied": outcome.applied,
                    "test_ok": outcome.test_ok,
                    "rolled_back": outcome.rolled_back,
                });
                if outcome.rolled_back {
                    // 適用したが起動確認に失敗 → 元に戻した。エージェントに明示的に伝える。
                    GatewayActionResult {
                        success: false,
                        data: Some(data),
                        error: Some(format!(
                            "'{provider}' の設定を適用しましたが起動確認に失敗したため、\
                             直前の設定へ自動ロールバックしました。binary_path/args/working_dir を確認してください。"
                        )),
                    }
                } else {
                    GatewayActionResult {
                        success: true,
                        data: Some(data),
                        error: None,
                    }
                }
            }
            Err((_code, msg)) => err(msg),
        }
    }

    async fn manage_allowed_commands(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("manage_allowed_commands requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());

        let conn = match self.state.db.lock() {
            Ok(c) => c,
            Err(_) => return err("db lock failed".to_string()),
        };
        match action {
            "list" => match opencrab_db::queries::list_agent_allowed_commands(&conn, &agent_id) {
                Ok(cmds) => GatewayActionResult {
                    success: true,
                    data: Some(json!({ "commands": cmds })),
                    error: None,
                },
                Err(e) => err(e.to_string()),
            },
            "add" => {
                let Some(cmd) = command.filter(|s| !s.is_empty()) else {
                    return err("command is required for add".to_string());
                };
                match opencrab_db::queries::add_agent_allowed_command(
                    &conn, &agent_id, &cmd, "owner",
                ) {
                    Ok(added) => GatewayActionResult {
                        success: true,
                        data: Some(json!({ "command": cmd, "added": added })),
                        error: None,
                    },
                    Err(e) => err(e.to_string()),
                }
            }
            "remove" => {
                let Some(cmd) = command.filter(|s| !s.is_empty()) else {
                    return err("command is required for remove".to_string());
                };
                match opencrab_db::queries::remove_agent_allowed_command(&conn, &agent_id, &cmd) {
                    Ok(removed) => GatewayActionResult {
                        success: true,
                        data: Some(json!({ "command": cmd, "removed": removed })),
                        error: None,
                    },
                    Err(e) => err(e.to_string()),
                }
            }
            other => err(format!("unknown action: {other} (list/add/remove)")),
        }
    }

    async fn configure_nostr(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("configure_nostr requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();
        // 既存設定を partial 更新のベースにする（省略フィールドは現状維持）。
        let existing = {
            let conn = match self.state.db.lock() {
                Ok(c) => c,
                Err(_) => return err("db lock failed".to_string()),
            };
            opencrab_db::queries::get_agent_nostr_config(&conn, &agent_id).unwrap_or(None)
        };
        let Some(existing) = existing else {
            return err(
                "Nostr 設定が未作成です。先に鍵を生成してください（operator がダッシュボードで生成）"
                    .to_string(),
            );
        };
        let ef: Value = serde_json::from_str(&existing.filter_json).unwrap_or_else(|_| json!({}));
        // args の配列（文字列）を取り出す。無ければ None（＝現状維持）。
        let arg_strs = |k: &str| -> Option<Vec<String>> {
            args.get(k).and_then(|x| x.as_array()).map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                    .collect()
            })
        };
        let cur_strs = |v: &Value, k: &str| -> Vec<String> {
            v.get(k)
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let arg_or_cur_kinds = || -> Vec<u32> {
            let extract = |v: &Value| -> Vec<u32> {
                v.as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|n| n.as_u64().map(|v| v as u32))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            match args.get("kinds") {
                Some(v) => extract(v),
                None => ef.get("kinds").map(extract).unwrap_or_default(),
            }
        };

        let relays = arg_strs("relays")
            .unwrap_or_else(|| serde_json::from_str(&existing.relays_json).unwrap_or_default());
        let authors = arg_strs("authors").unwrap_or_else(|| cur_strs(&ef, "authors"));
        let keywords = arg_strs("keywords").unwrap_or_else(|| cur_strs(&ef, "keywords"));
        let kinds = arg_or_cur_kinds();
        let enabled = args
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(existing.enabled);

        match crate::api::nostr::apply_nostr_settings(
            &self.state,
            &agent_id,
            &relays,
            &authors,
            &keywords,
            &kinds,
            enabled,
            None,
        )
        .await
        {
            Ok(()) => GatewayActionResult {
                success: true,
                // secret_key は返さない。
                data: Some(json!({
                    "agent_id": agent_id,
                    "relays": relays,
                    "authors": authors,
                    "keywords": keywords,
                    "kinds": kinds,
                    "enabled": enabled,
                })),
                error: None,
            },
            Err((_code, msg)) => err(msg),
        }
    }

    async fn configure_self(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("configure_self requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();

        // 三値: キー欠落=変更しない / null=解除（Some(None）) / 値=設定（Some(Some(v))）。
        let tri_string = |k: &str| -> Option<Option<String>> {
            match args.get(k) {
                None => None,
                Some(Value::Null) => Some(None),
                Some(Value::String(s)) => Some(Some(s.clone())),
                _ => None,
            }
        };
        let tri_bool = |k: &str| -> Option<Option<bool>> {
            match args.get(k) {
                None => None,
                Some(Value::Null) => Some(None),
                Some(Value::Bool(b)) => Some(Some(*b)),
                _ => None,
            }
        };

        let patch = opencrab_db::queries::AgentPatch {
            // persona_name は Option<String>（解除不可）。文字列指定時のみ設定。
            persona_name: args
                .get("persona_name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            personality: tri_string("personality"),
            job_title: tri_string("job_title"),
            organization: tri_string("organization"),
            model: tri_string("model"),
            reasoning_effort: tri_string("reasoning_effort"),
            web_search: tri_bool("web_search"),
            ..Default::default()
        };

        let result = {
            let conn = match self.state.db.lock() {
                Ok(c) => c,
                Err(_) => return err("db lock failed".to_string()),
            };
            opencrab_db::queries::apply_agent_patch(&conn, &agent_id, &patch)
        };
        match result {
            Ok(true) => GatewayActionResult {
                success: true,
                data: Some(json!({ "agent_id": agent_id, "updated": true })),
                error: None,
            },
            Ok(false) => err(format!("agent not found: {agent_id}")),
            Err(e) => err(e.to_string()),
        }
    }

    async fn configure_mcp_server(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("configure_mcp_server requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());

        match action {
            "list" => {
                let servers = {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    match opencrab_db::queries::list_agent_mcp_servers(&conn, &agent_id) {
                        Ok(s) => s,
                        Err(e) => return err(e.to_string()),
                    }
                };
                // env の値は出さず、キー名のみ返す（秘密を LLM 経路に載せない）。
                let list: Vec<Value> = servers
                    .iter()
                    .map(|s| {
                        let env_keys: Vec<String> =
                            serde_json::from_str::<serde_json::Map<String, Value>>(&s.env_json)
                                .map(|m| m.keys().cloned().collect())
                                .unwrap_or_default();
                        let args_arr: Vec<String> =
                            serde_json::from_str(&s.args_json).unwrap_or_default();
                        json!({
                            "name": s.name,
                            "command": s.command,
                            "args": args_arr,
                            "env_keys": env_keys,
                            "trusted_only": s.trusted_only,
                            "enabled": s.enabled,
                        })
                    })
                    .collect();
                GatewayActionResult {
                    success: true,
                    data: Some(json!({ "servers": list })),
                    error: None,
                }
            }
            "add" => {
                let Some(name) = name.filter(|s| !s.is_empty()) else {
                    return err("name is required".to_string());
                };
                if !is_valid_server_name(&name) {
                    return err(
                        "サーバ名は英数字・_・-（1〜64文字、__ を含まない）にしてください"
                            .to_string(),
                    );
                }
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                if command.is_empty() {
                    return err("command が必要です".to_string());
                }
                let args_vec: Vec<String> = args
                    .get("args")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                // 既存を（env 保持・デフォルト継承のため）読む。
                let existing = {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    opencrab_db::queries::get_agent_mcp_server(&conn, &agent_id, &name)
                        .unwrap_or(None)
                };
                // env は空/未指定なら既存を保持（値を伏せているため無変更更新で消さない）。
                let env_json = match args.get("env") {
                    Some(Value::Object(m)) if !m.is_empty() => {
                        serde_json::to_string(m).unwrap_or_else(|_| "{}".to_string())
                    }
                    _ => existing
                        .as_ref()
                        .map(|e| e.env_json.clone())
                        .unwrap_or_else(|| "{}".to_string()),
                };
                let trusted_only = args
                    .get("trusted_only")
                    .and_then(|v| v.as_bool())
                    .unwrap_or_else(|| existing.as_ref().map(|e| e.trusted_only).unwrap_or(false));
                let enabled = args
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or_else(|| existing.as_ref().map(|e| e.enabled).unwrap_or(true));
                let row = opencrab_db::queries::AgentMcpServerRow {
                    agent_id: agent_id.clone(),
                    name: name.clone(),
                    command,
                    args_json: serde_json::to_string(&args_vec)
                        .unwrap_or_else(|_| "[]".to_string()),
                    env_json,
                    trusted_only,
                    enabled,
                };
                {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    if let Err(e) = opencrab_db::queries::upsert_agent_mcp_server(&conn, &row) {
                        return err(e.to_string());
                    }
                }
                crate::api::mcp::spawn_reload(&self.state, agent_id);
                GatewayActionResult {
                    success: true,
                    // env の値は返さない。
                    data: Some(json!({ "name": name, "upserted": true, "enabled": enabled })),
                    error: None,
                }
            }
            "remove" => {
                let Some(name) = name.filter(|s| !s.is_empty()) else {
                    return err("name is required".to_string());
                };
                let removed = {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    match opencrab_db::queries::delete_agent_mcp_server(&conn, &agent_id, &name) {
                        Ok(r) => r,
                        Err(e) => return err(e.to_string()),
                    }
                };
                crate::api::mcp::spawn_reload(&self.state, agent_id);
                GatewayActionResult {
                    success: true,
                    data: Some(json!({ "name": name, "removed": removed })),
                    error: None,
                }
            }
            "set_enabled" => {
                let Some(name) = name.filter(|s| !s.is_empty()) else {
                    return err("name is required".to_string());
                };
                let Some(enabled) = args.get("enabled").and_then(|v| v.as_bool()) else {
                    return err("enabled (bool) is required for set_enabled".to_string());
                };
                {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    if let Err(e) = opencrab_db::queries::set_agent_mcp_server_enabled(
                        &conn, &agent_id, &name, enabled,
                    ) {
                        return err(e.to_string());
                    }
                }
                crate::api::mcp::spawn_reload(&self.state, agent_id);
                GatewayActionResult {
                    success: true,
                    data: Some(json!({ "name": name, "enabled": enabled })),
                    error: None,
                }
            }
            other => err(format!(
                "unknown action: {other} (list/add/remove/set_enabled)"
            )),
        }
    }
}

fn err(msg: String) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg),
    }
}

#[async_trait]
impl GatewayActions for SystemGatewayActions {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        let mut defs = Self::own_definitions();
        if let Some(inner) = &self.inner {
            // own と同名のツールは重複させない（own を優先）。nostr watch ループ稼働時は
            // inner=NostrGatewayActions も nostr_generate_key を定義するため、ここで
            // dedup しないとツール一覧に同名が2つ並ぶ（provider が拒否しうる）。
            let own_names: std::collections::HashSet<String> =
                defs.iter().map(|d| d.name.clone()).collect();
            for d in inner.definitions() {
                if !own_names.contains(&d.name) {
                    defs.push(d);
                }
            }
        }
        defs
    }

    async fn execute(
        &self,
        name: &str,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        match name {
            "configure_llm_provider" => self.configure_llm_provider(args, ctx).await,
            "manage_allowed_commands" => self.manage_allowed_commands(args, ctx).await,
            "configure_nostr" => self.configure_nostr(args, ctx).await,
            "configure_self" => self.configure_self(args, ctx).await,
            "configure_mcp_server" => self.configure_mcp_server(args, ctx).await,
            // bootstrap 鍵生成（鍵未設定でも露出）。inner より先に own が処理する。
            "nostr_generate_key" => self.nostr_generate_key(args, ctx).await,
            // 自分が扱わないツールは inner gateway へ委譲する。
            _ => match &self.inner {
                Some(inner) => inner.execute(name, args, ctx).await,
                None => GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("Unknown action: {name}")),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_definition_shape() {
        let defs = SystemGatewayActions::own_definitions();
        let d = defs
            .iter()
            .find(|d| d.name == "configure_llm_provider")
            .expect("configure_llm_provider must be defined");
        // provider は必須。
        let required = d.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "provider"));
        // 秘密情報 api_key は LLM ツールでは露出しない（ダッシュボード専用）。
        let props = d.parameters["properties"].as_object().unwrap();
        assert!(
            !props.contains_key("api_key"),
            "api_key must not be settable via the agent tool"
        );
        // 起動系フィールドは受け付ける。
        for key in ["binary_path", "args", "working_dir", "timeout_secs"] {
            assert!(props.contains_key(key), "missing property: {key}");
        }
    }
}
