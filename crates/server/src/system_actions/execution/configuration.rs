impl SystemGatewayActions {
    async fn configure_llm_provider(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if !ctx.caller.is_owner_equivalent() {
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
        if !ctx.caller.is_owner_equivalent() {
            return err("manage_allowed_commands requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());

        // `list` は DB ロックを取る前に片付ける。実効リストの解決
        // （`effective_allowed_commands`）が内部で同じ Mutex を取るため、ここで
        // 掴んだまま呼ぶと自己デッドロックする。
        if action == "list" {
            // `list_allowed_commands` と**同じ解決点**を通す。DB 行だけを返すと
            // 設定ファイル由来のコマンドが消えて「使えない」と誤認される（#300）。
            return GatewayActionResult {
                success: true,
                data: Some(
                    json!({ "commands": crate::process::effective_allowed_commands(&self.state, &agent_id) }),
                ),
                error: None,
            };
        }

        let conn = match self.state.db.lock() {
            Ok(c) => c,
            Err(_) => return err("db lock failed".to_string()),
        };
        match action {
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
    async fn configure_self(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if !ctx.caller.is_owner_equivalent() {
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
            // #412: オーナーが会話で「モデルを変えて」と言う経路がここ。ダッシュボードの
            // 口だけ塞いでも、こちらから未登録モデルを入れれば「黙って既定値」が再発する。
            // `PUT`/`PATCH /api/agents/{id}` と同じ gate を通す（owner 限定の扱いは
            // 上の fail-closed チェックのままで変えない）。
            if let Some(Some(new_model)) = patch.model.as_ref() {
                let existing = opencrab_db::queries::get_agent(&conn, &agent_id)
                    .ok()
                    .flatten();
                // #676（案Y）: 送るプロバイダの spec へ切り替えるときだけ max_output_tokens を要求。
                let sends_max = self
                    .state
                    .llm_router
                    .get()
                    .sends_max_output_tokens(new_model);
                if let Err(e) = crate::process::check_agent_model_change(
                    &conn,
                    existing.as_ref(),
                    Some(new_model),
                    sends_max,
                ) {
                    return err(e);
                }
            }
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
        if !ctx.caller.is_owner_equivalent() {
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
