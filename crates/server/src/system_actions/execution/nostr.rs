impl SystemGatewayActions {
    /// bootstrap 用の鍵生成（鍵未設定でも実行可能）。実体は `NostaroCli::vanity`
    /// （config 非依存）で、生成した nsec は**サーバ内に 0600 で保存**し LLM には返さない
    /// （npub/pubkey のみ）。process.rs の防御マスク（tool_name==nostr_generate_key）と
    /// bridge の nsec redaction が多層で秘密漏洩を防ぐ。
    #[cfg(feature = "nostr")]
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
        // 登録済みの Nostr transport の払い出し口（binary_path 等の設定を継承）を使う。
        // **無ければ既定**（#191 段階2 PR4）。ここは「受け口が無ければ拒否」ではない:
        // 移設前も `unwrap_or_default()` で既定 CLI にフォールバックしており、鍵生成は
        // ゲートウェイの稼働を必要としない（bootstrap 用途 = 鍵が無い状態から呼ぶ）。
        // これをガードに変えると「鍵がまだ無いから起動もしていない」正しい経路が
        // 塞がる（#191 の PR3 で踏みかけた、順序・フォールバックをガードへ機械的に
        // 移し替える誤りと同じ形）。
        let provisioning = self
            .state
            .gateways
            .get(opencrab_actions::gateway_kinds::NOSTR)
            .and_then(|gw| gw.key_provisioning())
            .unwrap_or_else(|| Arc::new(opencrab_nostr::NostrKeyProvisioning::default()));
        match provisioning.generate_key(prefix).await {
            Ok(k) => match provisioning.store_generated_key(&ctx.agent_id, &k) {
                Ok(_) => GatewayActionResult {
                    success: true,
                    // nsec は返さない（サーバ内 0600 保存済み）。npub/pubkey のみ。
                    data: Some(json!({
                        "npub": k.public_id,
                        "pubkey": k.public_key_hex,
                        "note": "新しい鍵を生成しました。秘密鍵(nsec)はサーバ内に安全に保存済みで、セキュリティ上あなた（LLM）には渡していません。共有・言及してよいのは npub までです。",
                    })),
                    error: None,
                },
                Err(e) => err(format!("鍵は生成しましたが保存に失敗しました: {e}")),
            },
            Err(e) => err(format!("nostr_generate_key 失敗: {e}")),
        }
    }

    /// bootstrap 用の鍵一覧（鍵未設定でも実行可能）。生成鍵（`generated-keys/<npub>.nsec`）の
    /// **npub のみ**を返す。実体は `NostaroCli::list_generated_keys`（ファイル名だけを列挙し、
    /// nsec 本文は開かない）。鍵生成と同じく transport の稼働を必要としない。
    #[cfg(feature = "nostr")]
    fn nostr_list_keys(ctx: &GatewayCallContext) -> GatewayActionResult {
        match opencrab_nostr::NostaroCli::list_generated_keys(&ctx.agent_id) {
            Ok(npubs) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "npubs": npubs,
                    "note": "あなたが生成した鍵の npub 一覧です。nostr_switch_identity で本鍵に採用できます。秘密鍵(nsec)はサーバ内に安全に保存されており、ここには含まれません。",
                })),
                error: None,
            },
            Err(e) => err(format!("nostr_list_keys 失敗: {e}")),
        }
    }

    /// bootstrap 用の identity 採用（#264）。生成鍵を本鍵として採用し、未接続なら
    /// **自己ブートストラップで接続まで行う**（絞り込みは自動設定せず、nostaro の
    /// mention-only 既定に委ねて自分宛のみを購読する / #271）。実体は Nostr transport の
    /// `identity_provisioning` capability。
    ///
    /// 稼働の有無は capability の内側で判定する（稼働中はホットスワップ、未稼働は bootstrap
    /// 起動＝接続）。**秘密鍵(nsec)は扱わない**（npub 参照のみ・応答にも出さない）。
    #[cfg(feature = "nostr")]
    async fn nostr_switch_identity(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        let Some(npub) = args
            .get("npub")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return err("npub パラメータが必要です".to_string());
        };
        // Nostr transport の採用 capability を引く。登録が無ければ Nostr 非対応構成。
        let Some(provisioning) = self
            .state
            .gateways
            .get(opencrab_actions::gateway_kinds::NOSTR)
            .and_then(|gw| gw.identity_provisioning())
        else {
            return err(
                "この環境では Nostr identity の採用は利用できません（Nostr 未構成）".to_string(),
            );
        };
        match provisioning.adopt_identity(&ctx.agent_id, npub).await {
            Ok(adopted) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "npub": adopted,
                    "note": "この鍵を本鍵として採用しました。未接続だった場合は自分への言及を購読する最小フィルタで Nostr に接続済みです。以後の投稿・公開ノート受信はこの identity で行われます。秘密鍵は扱っていません。",
                })),
                error: None,
            },
            Err(e) => err(format!("nostr_switch_identity 失敗: {e}")),
        }
    }

    // `nostr_run`（薄い nostaro passthrough / #268）の impl は撤去した（オーナー裁定）。定義から
    // 外し dispatch も fail-close にしたので、この実体は不要。nostaro passthrough 機構そのもの
    // （`NostaroCli::run_passthrough` / `GatewayNostrPassthrough`）は残置（他経路の防御・将来用）。
    #[cfg(feature = "nostr")]
    async fn configure_nostr(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if !ctx.caller.is_owner_equivalent() {
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
        // #514: DM の kind（4 / 1059）はここで落とす。保存自体は apply_nostr_settings 側でも
        // ストリップするが、この tool の応答（下の "kinds"）が保存値と一致するよう、モデルへ
        // 返す前にも落として「DM を購読設定できた」と誤解させない。DM は受信破棄・送信禁止・
        // 購読除外の 3 経路で一貫して扱わない（オーナー決定）。
        let kinds: Vec<u32> = arg_or_cur_kinds()
            .into_iter()
            .filter(|k| !opencrab_nostr::DM_KINDS.contains(k))
            .collect();
        let enabled = args
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(existing.enabled);
        // Nostr のオーナー公開鍵（#319）。未指定なら現状維持。**owner 権限のあるターン
        // （実際には Discord など）からしか触れない**ので、この口が Nostr 側の
        // 「オーナー未設定 → 誰も owner になれない → 設定できない」の鶏卵を解く。
        let owner_pubkey = args.get("owner_pubkey").and_then(|v| v.as_str());

        match crate::api::nostr::apply_nostr_settings(
            &self.state,
            &agent_id,
            &relays,
            &authors,
            &keywords,
            &kinds,
            enabled,
            None,
            owner_pubkey,
        )
        .await
        {
            Ok(()) => {
                // 保存後の値を読み直して返す（入力が npub でも保存形の hex が返る＝
                // どちらの表現で渡しても同じ鍵になったことをモデルが確認できる）。
                let stored = self
                    .state
                    .db
                    .lock()
                    .ok()
                    .and_then(|conn| {
                        opencrab_db::queries::get_agent_nostr_owner_pubkey(&conn, &agent_id).ok()
                    })
                    .unwrap_or_default();
                GatewayActionResult {
                    success: true,
                    // secret_key は返さない。
                    data: Some(json!({
                        "agent_id": agent_id,
                        "relays": relays,
                        "authors": authors,
                        "keywords": keywords,
                        "kinds": kinds,
                        "enabled": enabled,
                        "owner_pubkey": stored,
                    })),
                    error: None,
                }
            }
            Err((_code, msg)) => err(msg),
        }
    }
}
