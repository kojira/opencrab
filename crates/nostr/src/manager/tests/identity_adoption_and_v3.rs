    /// [#246 段階3 PR-B] `gateway_actions_for` は **稼働中の agent にだけ** capability を返し、
    /// その `text_delivery()` が自発投稿の配送口（`Some`）を提供する。稼働していない agent は
    /// `None`（config.toml が無い＝post が失敗する状態へツールを生やさない / fail-closed）。
    #[tokio::test]
    async fn gateway_actions_for_is_gated_on_is_running_and_exposes_text_delivery() {
        use opencrab_actions::AgentGatewayLifecycle;

        let mgr =
            NostrGatewayManager::new(SlowRunner::new(Duration::from_millis(1)), test_router());

        // 稼働中の agent を模す: 終わらないダミータスクの handle を登録簿へ挿す
        // （`is_running` は handle の生死で判定する）。
        let handle = tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
        mgr.gateways
            .write()
            .unwrap()
            .insert("agent-live".to_string(), handle);

        // 稼働中 → Some、かつ text_delivery() が Some（テキストを配れる gateway として見える）。
        assert!(mgr.is_running("agent-live"));
        let actions = AgentGatewayLifecycle::gateway_actions_for(&mgr, "agent-live");
        let actions = actions.expect("稼働中の agent には capability を返す");
        assert!(
            actions.text_delivery().is_some(),
            "自発投稿の配送口を提供する"
        );

        // 稼働していない agent → None（None を返し、post を呼ばない）。
        assert!(!mgr.is_running("agent-idle"));
        assert!(
            AgentGatewayLifecycle::gateway_actions_for(&mgr, "agent-idle").is_none(),
            "稼働していない agent には capability を返さない（fail-closed）"
        );

        // ダミータスクを回収する。
        mgr.gateways
            .write()
            .unwrap()
            .remove("agent-live")
            .unwrap()
            .abort();
    }

    /// pubkey を返す fake nostaro（実リレーへは繋がない / #264）。
    ///
    /// `pubkey` サブコマンドのときだけ `pubkey_out` を stdout に返す。#698: `following`
    /// サブコマンドのときは `--out=<path>` へ**空のフォローリスト**（`{"count":0,"users":[]}`）
    /// を書く（`spawn_agent_gateway` が起動時に必ず引くため。空でも正当な成功で起動は通る）。
    /// それ以外（`watch` など）は即終了する（受信ループは EOF → backoff で再試行するので handle は
    /// 生き続け、`is_running` は true を保つ）。`pubkey_out` が空なら pubkey も空を返す（起動失敗を模す）。
    fn fake_nostaro(pubkey_out: &str) -> (tempfile::TempDir, NostaroCli) {
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "#!/bin/sh\nmode=\nout=\nfor a in \"$@\"; do\n  case \"$a\" in\n    pubkey) printf '%s' '{pubkey_out}'; exit 0 ;;\n    following) mode=following ;;\n    --out=*) out=\"${{a#--out=}}\" ;;\n  esac\ndone\nif [ \"$mode\" = following ] && [ -n \"$out\" ]; then\n  printf '%s' '{{\"count\":0,\"users\":[]}}' > \"$out\"\nfi\nexit 0\n"
        );
        let script = crate::test_support::write_fake_nostaro(dir.path(), &body);
        let cli = NostaroCli::new().with_binary_path(script.to_string_lossy().to_string());
        (dir, cli)
    }

    /// [#264] 未設定エージェントが自力で採用＝接続する。`nostr_switch_identity`（採用）を
    /// 未稼働状態で呼ぶと、鍵・DEFAULT リレー・**空フィルタ**を enabled=false で書き、
    /// ゲートウェイを起動して is_running=true にし、成功後に enabled=true にする
    /// （順序ガード）。
    ///
    /// [#271] フィルタは**空**であること。旧実装は `keywords=[自分の npub]` を自動設定して
    /// おり、本文に npub 文字列を含まない e/p タグだけの返信を落としていた。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adopt_identity_bootstraps_unconfigured_agent_and_connects() {
        use opencrab_actions::GatewayIdentityProvisioning;

        let agent = "agent-bootstrap-264";
        let npub = "npub1selfbootstrap";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());

        // 自分の生成鍵を保存（read_generated_key の存在チェック＝「自分の鍵のみ」を満たす）。
        NostaroCli::new()
            .save_generated_key(
                agent,
                &crate::cli::GeneratedKey {
                    nsec: "nsec1bootstrapsecret".to_string(),
                    npub: npub.to_string(),
                    pubkey: "hexpub".to_string(),
                },
            )
            .unwrap();

        let runner = SlowRunner::new(Duration::from_millis(1));
        let (_fake, cli) = fake_nostaro("selfpubkeyhex");
        let mgr = NostrGatewayManager::new(runner.clone(), test_router()).with_cli(cli);

        assert!(!mgr.is_running(agent), "採用前は未稼働（未設定）");

        let adopted = mgr
            .identity_provisioner()
            .adopt_identity(agent, npub)
            .await
            .unwrap();
        assert_eq!(adopted, npub);
        // 返り値は npub のみ（nsec を出さない）。
        assert!(!adopted.contains("nsec"), "nsec を返さない");

        // 起動して接続済み（配送対象になれる状態）。
        assert!(
            mgr.is_running(agent),
            "採用で自力接続する（is_running=true）"
        );

        // upsert された config: 鍵＋DEFAULT relays＋空フィルタ、enabled=false。
        let upserted = runner.upserted.lock().unwrap().clone();
        assert_eq!(upserted.len(), 1, "config を 1 回 upsert する");
        let row = &upserted[0];
        assert_eq!(row.secret_key, "nsec1bootstrapsecret");
        assert!(!row.enabled, "先に enabled=false で書く（順序ガード）");
        let cfg = crate::config_from_row(row);
        assert!(
            cfg.filter.keywords.is_empty(),
            "[#271] keyword を自動設定しない（本文一致の条件を足すと p/e タグだけの返信が落ちる）: {:?}",
            cfg.filter.keywords
        );
        assert!(
            cfg.filter.authors.is_empty(),
            "author も自動設定しない: {:?}",
            cfg.filter.authors
        );
        assert!(
            !cfg.watches_beyond_self_mentions(),
            "上乗せ条件無し＝nostaro の mention-only 既定で自分宛のみを購読する"
        );
        assert_eq!(
            cfg.effective_relays(),
            crate::config::DEFAULT_RELAYS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            "未設定なら DEFAULT リレー"
        );

        // 起動成功後にだけ enabled=true。
        assert_eq!(
            *runner.enabled_calls.lock().unwrap(),
            vec![true],
            "起動成功後に enabled=true（1 回だけ）"
        );

        mgr.stop_agent_gateway(agent).await;
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#271] 運用者が明示した絞り込みは採用時に**そのまま**残す。
    ///
    /// 自動 keyword は付けない（前のテスト）が、逆に運用者が設定した keywords/authors を
    /// 勝手に外しもしない。relays も既存を継承する。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adopt_identity_bootstrap_keeps_operator_configured_filter() {
        use opencrab_actions::GatewayIdentityProvisioning;

        let agent = "agent-bootstrap-271-operator";
        let npub = "npub1operatorset";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        NostaroCli::new()
            .save_generated_key(
                agent,
                &crate::cli::GeneratedKey {
                    nsec: "nsec1operatorsecret".to_string(),
                    npub: npub.to_string(),
                    pubkey: "hexpub".to_string(),
                },
            )
            .unwrap();

        // 未稼働だが設定行はある（運用者がダッシュボードで絞り込みだけ入れた状態）。
        let existing = AgentNostrConfigRow {
            agent_id: agent.to_string(),
            secret_key: String::new(),
            relays_json: r#"["wss://relay.example"]"#.to_string(),
            filter_json: r#"{"authors":["npub1watched"],"keywords":["opencrab"],"kinds":[1,7]}"#
                .to_string(),
            enabled: false,
        };
        let runner = SlowRunner::new(Duration::from_millis(1)).with_preset_config(existing);
        let (_fake, cli) = fake_nostaro("selfpubkeyhex");
        let mgr = NostrGatewayManager::new(runner.clone(), test_router()).with_cli(cli);

        mgr.identity_provisioner()
            .adopt_identity(agent, npub)
            .await
            .unwrap();

        let upserted = runner.upserted.lock().unwrap().clone();
        let cfg = crate::config_from_row(&upserted[0]);
        assert_eq!(
            cfg.filter.keywords,
            vec!["opencrab".to_string()],
            "運用者の keyword を保つ"
        );
        assert_eq!(
            cfg.filter.authors,
            vec!["npub1watched".to_string()],
            "運用者の author を保つ"
        );
        assert_eq!(cfg.filter.kinds, vec![1, 7], "運用者の kind を保つ");
        assert!(
            !cfg.filter.keywords.contains(&npub.to_string()),
            "自分の npub を勝手に足さない"
        );
        assert_eq!(
            cfg.effective_relays(),
            vec!["wss://relay.example".to_string()]
        );

        mgr.stop_agent_gateway(agent).await;
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#264 / 配送誤爆防止] 起動に失敗したら未接続のまま（is_running=false）で、
    /// enabled=true にしない（「enabled だが未稼働」の不整合＝配送誤爆を残さない）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adopt_identity_failure_leaves_agent_disconnected_and_disabled() {
        use opencrab_actions::GatewayIdentityProvisioning;

        let agent = "agent-bootstrap-fail-264";
        let npub = "npub1failboot";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        NostaroCli::new()
            .save_generated_key(
                agent,
                &crate::cli::GeneratedKey {
                    nsec: "nsec1failsecret".to_string(),
                    npub: npub.to_string(),
                    pubkey: "x".to_string(),
                },
            )
            .unwrap();

        let runner = SlowRunner::new(Duration::from_millis(1));
        // pubkey を返さない fake → 起動が pubkey ガード（fail-closed）で失敗する。
        let (_fake, cli) = fake_nostaro("");
        let mgr = NostrGatewayManager::new(runner.clone(), test_router()).with_cli(cli);

        let res = mgr.identity_provisioner().adopt_identity(agent, npub).await;
        assert!(res.is_err(), "pubkey 取得不可なら採用は失敗する");

        assert!(
            !mgr.is_running(agent),
            "起動失敗なら is_running=false（配送対象に数えない）"
        );
        assert!(
            !runner.enabled_calls.lock().unwrap().contains(&true),
            "起動失敗時に enabled=true にしない（不整合を残さない）"
        );

        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#264 回帰] 稼働中エージェントの採用は**既存のホットスワップ経路**を通る
    /// （bootstrap の upsert / enabled 書き込みをせず、本鍵だけ差し替える＝再接続なし）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adopt_identity_uses_hotswap_when_gateway_running() {
        use opencrab_actions::GatewayIdentityProvisioning;

        let agent = "agent-hotswap-264";
        let npub_new = "npub1hotswapnew";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());

        // 稼働中エージェント: 運用者が設定した既存フィルタを持つ（ホットスワップは既存 relays を継承）。
        let existing = AgentNostrConfigRow {
            agent_id: agent.to_string(),
            secret_key: "nsec1old".to_string(),
            relays_json: r#"["wss://yabu.me"]"#.to_string(),
            filter_json: r#"{"keywords":["opencrab"]}"#.to_string(),
            enabled: true,
        };
        let runner = SlowRunner::new(Duration::from_millis(1)).with_preset_config(existing);
        // #489: fake nostaro は自 pubkey を **大文字 hex** で返す。逆引き表へは保存前に
        // `normalize_pubkey` を通した **小文字 hex** が入る（突合相手の author も正規化 hex）
        // ことを、起動時・identity 切替の両経路で固定する。
        let pubkey_upper = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789";
        let pubkey_lower = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let (_fake, cli) = fake_nostaro(pubkey_upper);
        let mgr = NostrGatewayManager::new(runner.clone(), test_router()).with_cli(cli);

        // 稼働させる（admin が admins 登録簿へ入る）。
        let configured = crate::config::NostrConfig {
            relays: vec!["wss://yabu.me".to_string()],
            filter: crate::config::NostrFilter {
                authors: vec![],
                keywords: vec!["opencrab".to_string()],
                kinds: vec![],
            },
        };
        mgr.start_agent_gateway(agent, "nsec1old", configured)
            .await
            .unwrap();
        assert!(mgr.is_running(agent));

        // 新しい生成鍵を保存して採用。
        NostaroCli::new()
            .save_generated_key(
                agent,
                &crate::cli::GeneratedKey {
                    nsec: "nsec1newhot".to_string(),
                    npub: npub_new.to_string(),
                    pubkey: "y".to_string(),
                },
            )
            .unwrap();

        let adopted = mgr
            .identity_provisioner()
            .adopt_identity(agent, npub_new)
            .await
            .unwrap();
        assert_eq!(adopted, npub_new);

        // ホットスワップ経路: bootstrap の upsert も enabled 書き込みもしない。
        assert!(
            runner.upserted.lock().unwrap().is_empty(),
            "稼働中はホットスワップ（config を upsert しない）"
        );
        assert!(
            runner.enabled_calls.lock().unwrap().is_empty(),
            "ホットスワップは enabled を触らない"
        );
        // 本鍵だけ差し替える（set_nostr_secret_key に新 nsec）。
        assert_eq!(
            *runner.secret_sets.lock().unwrap(),
            vec!["nsec1newhot".to_string()],
            "ホットスワップは本鍵だけ差し替える"
        );
        // #489: 自 pubkey は co_agent 逆引き表へ書き戻される（起動時 + identity 切替時の 2 回）。
        // どちらも fake nostaro の pubkey 出力（大文字 hex）を正規化した **小文字 hex**。
        // 切替でも stale にならない。
        assert_eq!(
            *runner.self_pubkey_sets.lock().unwrap(),
            vec![pubkey_lower.to_string(), pubkey_lower.to_string()],
            "起動時と identity 切替時に self_pubkey を正規化して書き戻す（#489）"
        );
        assert!(
            mgr.is_running(agent),
            "ホットスワップは再接続しない（稼働継続）"
        );

        mgr.stop_agent_gateway(agent).await;
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// v3 の稼働中採用はホットスワップせず、停止→revision→再起動する。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adopt_identity_v3_stops_revises_and_restarts() {
        use opencrab_actions::GatewayIdentityProvisioning;

        let agent = "agent-v3-revise-13";
        let npub_new = "npub1v3revisenew";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());

        let existing = AgentNostrConfigRow {
            agent_id: agent.to_string(),
            secret_key: "nsec1old".to_string(),
            relays_json: r#"["wss://yabu.me"]"#.to_string(),
            filter_json: r#"{"keywords":["opencrab"]}"#.to_string(),
            enabled: true,
        };
        let runner = SlowRunner::new(Duration::from_millis(1)).with_preset_config(existing);
        let pubkey_upper = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789";
        let pubkey_lower = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let (_fake, cli) = fake_nostaro(pubkey_upper);
        let revise_calls = Arc::new(AtomicUsize::new(0));
        let revise_count = revise_calls.clone();
        let mgr = NostrGatewayManager::new(runner.clone(), test_router())
            .with_cli(cli)
            .with_ingress(NostrIngress::V3)
            .with_provisioner(Arc::new(|_, _, _, _| Ok(())))
            .with_reviser(Arc::new(move |_, _, _, _| {
                revise_count.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(2)
            }));

        let configured = crate::config::NostrConfig {
            relays: vec!["wss://yabu.me".to_string()],
            filter: crate::config::NostrFilter {
                authors: vec![],
                keywords: vec!["opencrab".to_string()],
                kinds: vec![],
            },
        };
        mgr.start_agent_gateway(agent, "nsec1old", configured)
            .await
            .unwrap();
        assert!(mgr.is_running(agent));

        NostaroCli::new()
            .save_generated_key(
                agent,
                &crate::cli::GeneratedKey {
                    nsec: "nsec1newv3".to_string(),
                    npub: npub_new.to_string(),
                    pubkey: "y".to_string(),
                },
            )
            .unwrap();

        let adopted = mgr
            .identity_provisioner()
            .adopt_identity(agent, npub_new)
            .await
            .unwrap();
        assert_eq!(adopted, npub_new);
        assert_eq!(
            revise_calls.load(AtomicOrdering::SeqCst),
            1,
            "v3 切替は revision を 1 回上げる"
        );
        assert!(
            runner.upserted.lock().unwrap().is_empty(),
            "稼働中 v3 は bootstrap upsert しない"
        );
        assert!(
            runner.enabled_calls.lock().unwrap().is_empty(),
            "稼働中 v3 は enabled を触らない"
        );
        assert_eq!(
            *runner.secret_sets.lock().unwrap(),
            vec!["nsec1newv3".to_string()]
        );
        assert_eq!(
            *runner.self_pubkey_sets.lock().unwrap(),
            vec![
                pubkey_lower.to_string(),
                pubkey_lower.to_string(),
                pubkey_lower.to_string()
            ],
            "起動 + 切替 + 再起動で self_pubkey を 3 回書く"
        );
        assert!(mgr.is_running(agent), "再起動後も稼働する");

        mgr.stop_agent_gateway(agent).await;
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// v3_shadow は本番 UDS の listen 前でも legacy ループを立てる。
    /// 照合は parse/分類のメモリ内だけ。UDS hello / bind ack / live 占有はしない。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v3_shadow_starts_legacy_before_uds_listen() {
        let agent = "agent-shadow-before-listen";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());

        let existing = AgentNostrConfigRow {
            agent_id: agent.to_string(),
            secret_key: "nsec1old".to_string(),
            relays_json: r#"["wss://yabu.me"]"#.to_string(),
            filter_json: r#"{"keywords":["opencrab"]}"#.to_string(),
            enabled: true,
        };
        let runner = SlowRunner::new(Duration::from_millis(1)).with_preset_config(existing);
        let (_fake, cli) =
            fake_nostaro("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789");
        let provisioned = Arc::new(AtomicUsize::new(0));
        let count = provisioned.clone();
        let mgr = NostrGatewayManager::new(runner, test_router())
            .with_cli(cli)
            .with_ingress(NostrIngress::V3Shadow)
            .with_instance_provisioner(Arc::new(move |_, _, _, _| {
                count.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(1)
            }));

        let configured = crate::config::NostrConfig {
            relays: vec!["wss://yabu.me".to_string()],
            filter: crate::config::NostrFilter {
                authors: vec![],
                keywords: vec!["opencrab".to_string()],
                kinds: vec![],
            },
        };
        mgr.start_agent_gateway(agent, "nsec1old", configured)
            .await
            .unwrap();
        assert!(
            mgr.is_running(agent),
            "listen 前でも v3_shadow は legacy を立てる"
        );
        assert_eq!(
            provisioned.load(AtomicOrdering::SeqCst),
            1,
            "instance 行は敷く（UDS hello はしない）"
        );

        mgr.stop_agent_gateway(agent).await;
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#489] 自 pubkey が **正規化できない値**（npub でも 64 桁 hex でもない）なら、逆引き表へ
    /// **保存しない**（黙って壊れた値を入れない）。突合相手の author は `normalize_pubkey` 済みの
    /// 小文字 hex なので、生値を入れると必ず食い違って co_agent が静かに fail-closed で死ぬ
    /// ＝ #489 と同じ症状になる。それを防ぐ None 経路の回帰。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn self_pubkey_not_saved_when_unnormalizable() {
        let agent = "agent-badpub-489";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());

        let existing = AgentNostrConfigRow {
            agent_id: agent.to_string(),
            secret_key: "nsec1old".to_string(),
            relays_json: r#"["wss://yabu.me"]"#.to_string(),
            filter_json: r#"{"keywords":["opencrab"]}"#.to_string(),
            enabled: true,
        };
        let runner = SlowRunner::new(Duration::from_millis(1)).with_preset_config(existing);
        // 64 桁 hex でも npub でもない非空出力 → pubkey 取得ガードは通るが normalize_pubkey は None。
        let (_fake, cli) = fake_nostaro("not-a-valid-pubkey");
        let mgr = NostrGatewayManager::new(runner.clone(), test_router()).with_cli(cli);

        let configured = crate::config::NostrConfig {
            relays: vec!["wss://yabu.me".to_string()],
            filter: crate::config::NostrFilter {
                authors: vec![],
                keywords: vec!["opencrab".to_string()],
                kinds: vec![],
            },
        };
        mgr.start_agent_gateway(agent, "nsec1old", configured)
            .await
            .unwrap();

        assert!(
            mgr.is_running(agent),
            "自 pubkey が正規化不能でも gateway 自体は起動する（自己スキップは生値で機能する）"
        );
        assert!(
            runner.self_pubkey_sets.lock().unwrap().is_empty(),
            "#489: 正規化できない自 pubkey は逆引き表へ保存しない（黙って壊れた値を入れない）"
        );

        mgr.stop_agent_gateway(agent).await;
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }
