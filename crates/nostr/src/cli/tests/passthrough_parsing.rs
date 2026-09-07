    // ------------------------------------------------------------------
    // nostr_run 薄い passthrough（#268）
    // ------------------------------------------------------------------

    /// 各 argv を 1 行ずつ echo する fake nostaro（引数の中身を検証するため）。
    /// シェルは介さず `"$@"` をそのまま出すので、`;` や空白入りの値も 1 引数として現れる。
    #[cfg(unix)]
    fn fake_echo_nostaro() -> (tempfile::TempDir, NostaroCli) {
        let dir = tempfile::tempdir().unwrap();
        let script = crate::test_support::write_fake_nostaro(
            dir.path(),
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n",
        );
        let cli = NostaroCli::new().with_binary_path(script.to_string_lossy().to_string());
        (dir, cli)
    }

    /// stdout / stderr に nsec を吐く fake nostaro（マスク検証用）。`leak_to_stderr` で
    /// 出力先と終了コードを切り替える（stderr なら exit 1 = 失敗経路）。
    #[cfg(unix)]
    fn fake_leaky_nostaro(leak_to_stderr: bool) -> (tempfile::TempDir, NostaroCli) {
        let dir = tempfile::tempdir().unwrap();
        let body = if leak_to_stderr {
            "#!/bin/sh\nprintf 'secret_key = \"nsec1leakedsecretmaterial\"\\n' 1>&2\nexit 1\n"
        } else {
            "#!/bin/sh\nprintf 'secret_key = \"nsec1leakedsecretmaterial\"\\n'\nexit 0\n"
        };
        let script = crate::test_support::write_fake_nostaro(dir.path(), body);
        let cli = NostaroCli::new().with_binary_path(script.to_string_lossy().to_string());
        (dir, cli)
    }

    /// config.toml を materialize して「鍵採用済み」状態を作る。
    fn materialize_for(agent: &str) {
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        NostaroCli::materialize_config(agent, &["wss://relay.test".to_string()], None).unwrap();
    }

    /// `init` / `watch` / `relay` / `dm` は materialize の有無に関わらず**拒否**
    /// （鍵管理・受信・リレー設定・DM 送信は passthrough の外）。deny
    /// チェックは config 存在チェックより手前なので nostaro を spawn しない。`relay` は
    /// config.toml だけ書き換わって DB と desync し次の gateway start / switch_identity で
    /// 揮発するため塞ぐ（configure_nostr / ダッシュボードの DB 経路に閉じる）。`dm`（#514）は
    /// `nostr_dm` ツール撤去だけでは `nostr_run dm send` から通ってしまう送信のもう一方の
    /// 経路を塞ぐ。`event` は #699（オーナー裁定）で許可に転じた——deny に残っていない
    /// ことも下の許可テストで固定する。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_denies_init_watch_relay_and_dm() {
        let agent = "agent-pt-deny";
        materialize_for(agent);
        let (_d, cli) = fake_echo_nostaro();

        for sub in ["init", "watch", "relay", "dm"] {
            let r = cli.run_passthrough(agent, sub, &[]).await;
            assert!(r.is_err(), "{sub} は拒否されるべき");
            let msg = r.unwrap_err().to_string();
            assert!(msg.contains(sub), "拒否理由に {sub} が含まれること: {msg}");
        }
        // relay の拒否理由には opencrab 側で管理する旨を明示する（誘導）。
        let msg = cli
            .run_passthrough(agent, "relay", &["add".to_string(), "wss://x".to_string()])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("configure_nostr") || msg.contains("ダッシュボード"),
            "relay の拒否理由に opencrab 側の管理経路を含めること: {msg}"
        );
        // #514: dm の拒否理由には代替（Discord）への誘導を含める。`dm send` の形でも塞ぐ。
        let msg = cli
            .run_passthrough(agent, "dm", &["send".to_string(), "npub1x".to_string()])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("Discord"),
            "dm の拒否理由に代替（Discord）への誘導を含めること: {msg}"
        );
        // #699（オーナー裁定）: event は許可——任意 kind の publish（例: kind:40 の
        // パブリックチャット作成）が nostaro へ素通しになることを固定する。
        let out = cli
            .run_passthrough(
                agent,
                "event",
                &[
                    "-k".to_string(),
                    "40".to_string(),
                    "-c".to_string(),
                    "テスト用チャンネル".to_string(),
                ],
            )
            .await
            .expect("event は許可される（#699）");
        assert!(
            out.contains("event") && out.contains("40"),
            "event と kind が nostaro へ verbatim に渡ること: {out}"
        );
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// config 未 materialize（鍵未採用）なら nostaro を spawn せず明示エラー。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_errors_when_config_missing() {
        let agent = "agent-pt-noconfig";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        let (_d, cli) = fake_echo_nostaro();

        let r = cli
            .run_passthrough(agent, "post", &["hi".to_string()])
            .await;
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("config.toml") || msg.contains("採用"),
            "未 materialize は明示エラー: {msg}"
        );
    }

    /// 素通しは常に `--config <このエージェントの config>` を前置し、subcommand と args を
    /// **1 argv ずつ**そのまま渡す。`;`・空白入りの値もシェル解釈されず 1 引数として届く
    /// （＝シェルインジェクション不可）。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_uses_agent_config_and_passes_args_verbatim() {
        let agent = "agent-pt-args";
        materialize_for(agent);
        let (_d, cli) = fake_echo_nostaro();

        let injection = "hello; rm -rf / && echo pwned".to_string();
        // subcommand は実 deny を通らない読み取り系（timeline）にする。`event` は #699 で許可済み
        // なので、素通しの汎用挙動（config 固定・argv verbatim）の検証には使えない。
        let out = cli
            .run_passthrough(
                agent,
                "timeline",
                &["--limit".to_string(), "5".to_string(), injection.clone()],
            )
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // 先頭は必ず --config <このエージェントの config.toml>。
        assert_eq!(lines[0], "--config");
        assert!(
            lines[1].contains(&format!("data/agents/{agent}/nostr/config.toml")),
            "config は常に ctx.agent_id のもの: {}",
            lines[1]
        );
        assert_eq!(lines[2], "timeline");
        assert_eq!(lines[3], "--limit");
        assert_eq!(lines[4], "5");
        // インジェクション文字列は 1 argv として丸ごと届く（分割・実行されない）。
        assert_eq!(lines[5], injection);
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// args で `--config` を上書きさせない（config 固定＝鍵混同防止を回避されない）。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_rejects_config_override() {
        let agent = "agent-pt-cfgoverride";
        materialize_for(agent);
        let (_d, cli) = fake_echo_nostaro();

        for bad in [
            vec!["--config".to_string(), "/etc/other".to_string()],
            vec!["--config=/etc/other".to_string()],
        ] {
            let r = cli.run_passthrough(agent, "get", &bad).await;
            assert!(r.is_err(), "--config 上書きは拒否: {bad:?}");
            assert!(r.unwrap_err().to_string().contains("--config"));
        }
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// 成功時の stdout も nsec マスクを通す（config を表示しうる系のサブコマンドで万一
    /// 秘密が混じっても伏せる / 多層防御 #263）。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_masks_nsec_in_stdout() {
        let agent = "agent-pt-stdoutmask";
        materialize_for(agent);
        let (_d, cli) = fake_leaky_nostaro(false);

        let out = cli.run_passthrough(agent, "get", &[]).await.unwrap();
        assert!(
            !out.contains("nsec1leakedsecretmaterial"),
            "stdout に nsec が漏れている: {out}"
        );
        assert!(out.contains("<redacted>"), "マスク痕跡が無い: {out}");
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#299] passthrough で起動した nostaro は**エージェント workspace の中**で動き、
    /// そこにある相対パスのファイルを読める。`--config` は cwd を移しても解決できる。
    ///
    /// `nostr_run <sub> --out <相対>` 等が `ws_write` / `execute_shell` の作ったファイルと
    /// 噛み合うことを、fake nostaro の `pwd` / 相対 `cat` / config 存在チェックで固定する。
    /// subcommand は読み取り系（get）を使う（`event` は #699 で許可済み）。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_runs_in_agent_workspace() {
        let agent = "agent-pt-cwd";
        materialize_for(agent);
        let cli_probe = NostaroCli::new();
        let ws = cli_probe.agent_workspace_dir(agent).unwrap();
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        // エージェントが ws_write / execute_shell で置いたファイルを模す。
        std::fs::write(ws.join("payload.json"), "MARKER_FROM_WORKSPACE").unwrap();

        // cwd と「相対パスの中身」と「--config が読めるか」を出力する fake nostaro。
        let dir = tempfile::tempdir().unwrap();
        let script = crate::test_support::write_fake_nostaro(
            dir.path(),
            "#!/bin/sh\npwd\ncat payload.json 2>/dev/null || printf 'NO_FILE'\nprintf '\\n'\n\
             if [ -f \"$2\" ]; then printf 'CONFIG_OK\\n'; else printf 'CONFIG_MISSING\\n'; fi\n",
        );
        let cli = NostaroCli::new().with_binary_path(script.to_string_lossy().to_string());

        let out = cli
            .run_passthrough(
                agent,
                "get",
                &["--out".to_string(), "payload.json".to_string()],
            )
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            std::path::Path::new(lines[0]).ends_with(format!("data/agents/{agent}/workspace")),
            "nostaro の cwd が workspace でない: {out}"
        );
        assert_eq!(
            lines[1], "MARKER_FROM_WORKSPACE",
            "workspace 相対のファイルが読めていない: {out}"
        );
        assert_eq!(
            lines[2], "CONFIG_OK",
            "cwd を移したあと --config が解決できていない: {out}"
        );

        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// 失敗時の stderr（nostaro が config 先頭行をエコーする経路）も nsec を伏せる。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_masks_nsec_in_error_output() {
        let agent = "agent-pt-errmask";
        materialize_for(agent);
        let (_d, cli) = fake_leaky_nostaro(true);

        let r = cli.run_passthrough(agent, "get", &[]).await;
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(
            !msg.contains("nsec1leakedsecretmaterial"),
            "エラー出力に nsec が漏れている: {msg}"
        );
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#698] `following --out-format json` の実出力形（`{count, users:[{hex,npub}]}`）を
    /// 照合キー集合へ落とせる。hex は小文字化のみ（follow_key）で一致し、count は無視する。
    #[test]
    fn parse_following_json_extracts_hex_keys() {
        let raw = r#"{
          "count": 2,
          "users": [
            {"hex":"AA00000000000000000000000000000000000000000000000000000000000001","npub":"npub1x"},
            {"hex":"bb00000000000000000000000000000000000000000000000000000000000002","npub":"npub1y"}
          ]
        }"#;
        let set = parse_following_json(raw).unwrap();
        assert_eq!(set.len(), 2);
        // hex は 64 桁なので normalize され小文字化される。
        assert!(set.contains("aa00000000000000000000000000000000000000000000000000000000000001"));
        assert!(set.contains("bb00000000000000000000000000000000000000000000000000000000000002"));
    }

    /// [#698] 0 フォロー（空の users）は**正当な成功**＝空集合（エラーにしない）。
    #[test]
    fn parse_following_json_empty_is_ok_empty_set() {
        let set = parse_following_json(r#"{"count":0,"users":[]}"#).unwrap();
        assert!(set.is_empty());
    }

    /// [#698] JSON 全体が壊れていれば Err（呼び出し側が fail-loud にできる。全通しへ倒す
    /// 空集合を黙って返さない）。
    #[test]
    fn parse_following_json_broken_is_err() {
        assert!(parse_following_json("not json").is_err());
        // users 配列が無い形も Err。
        assert!(parse_following_json(r#"{"count":0}"#).is_err());
    }
