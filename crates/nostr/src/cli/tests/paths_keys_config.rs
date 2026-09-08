    #[test]
    fn test_agent_dir_isolated_per_agent() {
        let a = NostaroCli::agent_nostr_dir("agent-1").unwrap();
        let b = NostaroCli::agent_nostr_dir("agent-2").unwrap();
        assert_ne!(a, b);
        assert!(a.ends_with("data/agents/agent-1/nostr"));
        assert!(NostaroCli::agent_config_path("agent-1")
            .unwrap()
            .ends_with("data/agents/agent-1/nostr/config.toml"));
    }

    /// [#299] nostaro は**エージェントの workspace ルート**を cwd にして起動する。
    ///
    /// `execute_shell` / `ws_*` は `data/agents/{id}/workspace` を cwd にしている
    /// （`shell.rs` の `cmd.current_dir(ctx.workspace.root())`）。ここが揃っていないと
    /// `nostr_run event --file <相対>` が ws_write したファイルを見つけられず、
    /// `--out <相対>` の出力も ws_read から見えない。
    #[test]
    fn test_base_command_runs_in_agent_workspace() {
        let cli = NostaroCli::new();
        let agent = "agent-cwd-test";
        let cmd = cli.base_command(agent).unwrap();
        let cwd = cmd
            .as_std()
            .get_current_dir()
            .expect("cwd がエージェント workspace に固定されていない（#299）");
        assert!(
            cwd.ends_with(format!("data/agents/{agent}/workspace")),
            "cwd は execute_shell / ws_* と同じ workspace ルート: {}",
            cwd.display()
        );
        // 相対パスの解決基準がプロセス cwd に左右されないよう絶対パスで渡す。
        assert!(cwd.is_absolute(), "cwd は絶対パス: {}", cwd.display());
        // ディレクトリは用意されている（spawn 時に「そんなディレクトリは無い」で落ちない）。
        assert!(cwd.is_dir(), "workspace ルートが無い: {}", cwd.display());
        let _ = std::fs::remove_dir_all(cli.agent_workspace_dir(agent).unwrap());
    }

    /// [#299] cwd を変えても `--config` が解決できる（＝絶対パスで渡す）。
    ///
    /// config は `data/agents/{id}/nostr/config.toml` とプロセス cwd 基準の相対パスで
    /// 組まれる。cwd を workspace へ移す以上、相対のまま渡すと
    /// `<workspace>/data/agents/...` を探して**必ず見失う**。
    #[test]
    fn test_base_command_passes_absolute_config_path() {
        let cli = NostaroCli::new();
        let agent = "agent-cfgabs-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        NostaroCli::materialize_config(agent, &["wss://relay.test".to_string()], None).unwrap();

        let cmd = cli.base_command(agent).unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args[0], "--config");
        let config_arg = std::path::PathBuf::from(&args[1]);
        assert!(
            config_arg.is_absolute(),
            "--config は絶対パスで渡す（cwd を変えるため）: {}",
            config_arg.display()
        );
        assert!(
            config_arg.ends_with(format!("data/agents/{agent}/nostr/config.toml")),
            "config は常にこのエージェントのもの: {}",
            config_arg.display()
        );
        // cwd（workspace）を基準にしても、そのパスは実在する config を指す。
        assert!(
            config_arg.is_file(),
            "cwd 変更後も解決できない config パス: {}",
            config_arg.display()
        );

        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        let _ = std::fs::remove_dir_all(cli.agent_workspace_dir(agent).unwrap());
    }

    /// [#299] cwd の元になる workspace テンプレートは `agent.workspace_path` 由来
    /// （`with_workspace_base` で配線される）。既定値を焼き込まない。
    #[test]
    fn test_workspace_base_is_configurable() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("agents/{agent_id}/ws");
        let cli = NostaroCli::new().with_workspace_base(base.to_string_lossy().to_string());
        let cmd = cli.base_command("agent-wsbase").unwrap();
        let cwd = cmd.as_std().get_current_dir().unwrap();
        assert!(
            cwd.ends_with("agents/agent-wsbase/ws"),
            "設定した workspace_base が使われていない: {}",
            cwd.display()
        );
        // 空文字は無視して既定を保つ（他の with_* と同じ扱い）。
        let cli = NostaroCli::new().with_workspace_base("  ");
        let cwd = cli
            .base_command("agent-wsbase2")
            .unwrap()
            .as_std()
            .get_current_dir()
            .unwrap()
            .to_path_buf();
        assert!(cwd.ends_with("data/agents/agent-wsbase2/workspace"));
        let _ = std::fs::remove_dir_all(
            NostaroCli::new()
                .agent_workspace_dir("agent-wsbase2")
                .unwrap(),
        );
    }

    /// workspace ルートを**同名ファイル**で塞ぎ、`create_dir_all` を失敗させる。
    /// degrade 経路（cwd 設定も `--config` 絶対化も見送り）のテスト用。
    fn blocked_workspace_base(dir: &std::path::Path, agent_id: &str) -> String {
        let blocked = dir.join(format!("agents/{agent_id}/ws"));
        std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
        // ディレクトリを作らせない：同名の通常ファイルを置く。
        std::fs::write(&blocked, b"not a directory").unwrap();
        dir.join("agents/{agent_id}/ws")
            .to_string_lossy()
            .to_string()
    }

    /// [#301 レビュー] workspace ディレクトリを用意できないときは **cwd を設定しない**。
    ///
    /// ここで `current_dir` だけ設定すると spawn が `ENOENT`/`ENOTDIR` で落ち、
    /// 「nostaro が PATH に無い」場合と同じ文面（`failed to run nostaro: No such file or
    /// directory`）になって切り分け不能になる。cwd を諦めれば #299 修正前の挙動のまま
    /// post/reply/watch/pubkey は動き続ける。
    ///
    /// 併せて **`--config` も従来どおり**（絶対化しない）ことを見る。cwd だけ移して config が
    /// 相対、という中間状態こそ #299 で実測した壊れ方（`CONFIG_MISSING`）なので、
    /// 「両方成功 or 両方見送り」を固定する。
    #[test]
    fn test_base_command_degrades_when_workspace_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let agent = "agent-degrade-base";
        let cli = NostaroCli::new().with_workspace_base(blocked_workspace_base(dir.path(), agent));

        let cmd = cli.base_command(agent).unwrap();
        assert!(
            cmd.as_std().get_current_dir().is_none(),
            "workspace を用意できないのに cwd を設定している（spawn が ENOTDIR で落ちる）: {:?}",
            cmd.as_std().get_current_dir()
        );
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args[0], "--config");
        let config_arg = std::path::PathBuf::from(&args[1]);
        assert!(
            !config_arg.is_absolute(),
            "cwd を見送ったのに config だけ絶対化している（片方だけ適用の中間状態）: {}",
            config_arg.display()
        );
        assert_eq!(
            config_arg,
            NostaroCli::agent_config_path(agent).unwrap(),
            "degrade 時の --config は従来どおりのパス"
        );
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#301 レビュー] `from`（生成鍵）経路も同じく「両方成功 or 両方見送り」。
    /// `base_command` だけ直して `generated_key_command` が取り残される退行を防ぐ。
    #[test]
    fn test_from_command_degrades_when_workspace_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let agent = "agent-degrade-from";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        NostaroCli::materialize_config(agent, &["wss://relay.test".to_string()], None).unwrap();
        let key = GeneratedKey {
            nsec: "nsec1gen".into(),
            npub: "npub1degrade".into(),
            pubkey: "hex".into(),
        };
        NostaroCli::new().save_generated_key(agent, &key).unwrap();
        let cli = NostaroCli::new().with_workspace_base(blocked_workspace_base(dir.path(), agent));

        let cmd = cli.generated_key_command(agent, "npub1degrade").unwrap();
        assert!(
            cmd.as_std().get_current_dir().is_none(),
            "from 経路が degrade していない（cwd を設定している）"
        );
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args[0], "--config");
        let config_arg = std::path::PathBuf::from(&args[1]);
        assert!(
            !config_arg.is_absolute(),
            "from 経路で config だけ絶対化している: {}",
            config_arg.display()
        );
        // #620: from 経路は**鍵行なしの本設定**を --config に使う（from-config は作らない）。
        assert_eq!(
            config_arg,
            NostaroCli::agent_config_path(agent).unwrap(),
            "degrade 時の --config は本設定 config.toml のパス"
        );
        // 生成鍵は env で注入される。
        let env_key = cmd
            .as_std()
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(SECRET_KEY_ENV))
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().to_string());
        assert_eq!(
            env_key.as_deref(),
            Some("nsec1gen"),
            "生成鍵が env に載っていない"
        );
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    #[test]
    fn test_agent_dir_rejects_traversal_id() {
        // validate_agent_id 経由なので `../` 入りは弾かれる。
        assert!(NostaroCli::agent_nostr_dir("../etc").is_err());
        assert!(NostaroCli::agent_nostr_dir("a/b").is_err());
        assert!(NostaroCli::agent_nostr_dir("").is_err());
    }

    #[test]
    fn test_validate_vanity_prefix() {
        // 空 = ランダム鍵（OK）。
        assert!(validate_vanity_prefix("").is_ok());
        assert!(validate_vanity_prefix("cat").is_ok());
        // bech32 に無い文字（`1` `b` `i` `o`）は拒否。
        assert!(validate_vanity_prefix("1ac").is_err());
        assert!(validate_vanity_prefix("bob").is_err());
        assert!(validate_vanity_prefix("cab").is_err()); // 'b' は bech32 に無い
                                                         // 長さ上限は撤廃した。bech32 charset のみで構成される長い prefix は OK
                                                         // （探索は内部 spawn + キャンセルで途中停止できる）。
        assert!(validate_vanity_prefix("cafe").is_ok()); // c,a,f,e は全て bech32 charset
        assert!(validate_vanity_prefix("crab").is_err()); // 'b' は bech32 に無い
        assert!(validate_vanity_prefix("qqqqqqqq").is_ok()); // 長くても charset OK なら通す
    }

    #[test]
    fn test_save_generated_key_writes_0600_and_sanitizes_name() {
        let key = GeneratedKey {
            nsec: "nsec1secret".to_string(),
            npub: "npub1cat/../x".to_string(), // 異物入り → 英数字のみに安全化
            pubkey: "deadbeef".to_string(),
        };
        let path = NostaroCli::new()
            .save_generated_key("agent-gen-test", &key)
            .unwrap();
        // ファイル名は英数字のみ（`/` や `.` は落ちる）。
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name, "npub1catx.nsec");
        assert!(path.ends_with("data/agents/agent-gen-test/nostr/generated-keys/npub1catx.nsec"));
        // 中身は nsec、パーミッションは 0600（unix）。
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "nsec1secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_file(&path);
    }

    /// #241: 保存に失敗しても、返るエラーに**鍵の所在（保存先パス）を載せない**。
    /// 成功経路は所在を捨てて npub だけ返すのに、失敗経路の `with_context` がパスを
    /// 載せると保護が失敗時だけ破れ、エラーが `nostr_generate_key` の結果として
    /// そのままエージェントへ渡ってしまう。所在はサーバログにだけ残す。
    #[test]
    fn test_save_generated_key_failure_does_not_leak_path() {
        let agent = "agent-241-save-fail";
        let base = NostaroCli::agent_nostr_dir(agent).unwrap();
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // `generated-keys` の位置に**ファイル**を置く → create_dir_all がそこで失敗する。
        let blocker = base.join("generated-keys");
        std::fs::write(&blocker, b"not a directory").unwrap();

        let key = GeneratedKey {
            nsec: "nsec1secret".to_string(),
            npub: "npub1abc".to_string(),
            pubkey: "deadbeef".to_string(),
        };
        let err = NostaroCli::new()
            .save_generated_key(agent, &key)
            .unwrap_err();
        let msg = format!("{err:#}");

        // 所在（保存先ディレクトリ / agent id / `generated-keys` / データパス）が一切載らない。
        assert!(!msg.contains("generated-keys"), "保存先が漏れている: {msg}");
        assert!(
            !msg.contains(agent),
            "agent id 経由で所在が漏れている: {msg}"
        );
        assert!(
            !msg.contains(base.to_string_lossy().as_ref()),
            "保存先パスが漏れている: {msg}"
        );
        // 失敗した事実は返す（エージェントは「保存に失敗した」ことは知ってよい）。
        assert!(msg.contains("失敗"), "失敗である旨は返すべき: {msg}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_list_generated_keys_returns_npubs_only() {
        let agent = "agent-list-keys-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());

        // 生成前（ディレクトリ未作成）は空一覧。
        assert!(NostaroCli::list_generated_keys(agent).unwrap().is_empty());

        // 複数鍵を保存する。
        for npub in ["npub1alpha", "npub1bravo", "npub1charlie"] {
            NostaroCli::new()
                .save_generated_key(
                    agent,
                    &GeneratedKey {
                        nsec: format!("nsec1secret-{npub}"),
                        npub: npub.to_string(),
                        pubkey: "deadbeef".to_string(),
                    },
                )
                .unwrap();
        }
        // `from` 送信用の一時 config（`.config.toml`）が混ざっていても無視される。
        let dir = NostaroCli::agent_nostr_dir(agent)
            .unwrap()
            .join("generated-keys");
        std::fs::write(
            dir.join("npub1alpha.config.toml"),
            "secret_key = \"nsec1x\"",
        )
        .unwrap();

        let npubs = NostaroCli::list_generated_keys(agent).unwrap();
        assert_eq!(
            npubs,
            vec![
                "npub1alpha".to_string(),
                "npub1bravo".to_string(),
                "npub1charlie".to_string()
            ],
            "npub 一覧（ソート済み）だけが返る"
        );
        // nsec 本文は 1 つも一覧に現れない。
        for entry in &npubs {
            assert!(!entry.contains("nsec"), "nsec が一覧に漏れている: {entry}");
        }

        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#265 レビュー堅牢化] ディレクトリ / 非 npub な `.nsec` は列挙しない。
    #[test]
    fn test_list_generated_keys_ignores_dirs_and_non_npub() {
        let agent = "agent-list-keys-robust-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        let dir = NostaroCli::agent_nostr_dir(agent)
            .unwrap()
            .join("generated-keys");
        std::fs::create_dir_all(&dir).unwrap();

        // 正規の npub 鍵。
        NostaroCli::new()
            .save_generated_key(
                agent,
                &GeneratedKey {
                    nsec: "nsec1ok".to_string(),
                    npub: "npub1good".to_string(),
                    pubkey: "deadbeef".to_string(),
                },
            )
            .unwrap();
        // `.nsec` 拡張子だが npub でない（hex fallback を模す）→ 除外。
        std::fs::write(dir.join("deadbeefhex.nsec"), "nsec1hex").unwrap();
        // `.nsec` 拡張子のディレクトリ → 通常ファイルでないので除外。
        std::fs::create_dir_all(dir.join("weird.nsec")).unwrap();

        let npubs = NostaroCli::list_generated_keys(agent).unwrap();
        assert_eq!(
            npubs,
            vec!["npub1good".to_string()],
            "npub1 で始まる通常ファイルだけを返す"
        );

        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    #[test]
    fn test_materialize_config_writes_default_relays_without_secret_key() {
        // nostaro は `relays` と `default_relays` の両方を必須とする（#262）。
        // #620: **secret_key 行は書かない**（鍵は実行時に env で注入）。config を読んでも平文の
        // 鍵が目に入らないことを確認する。
        let agent = "agent-materialize-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        let path = NostaroCli::materialize_config(
            agent,
            &[
                "wss://relay-one.example.com".to_string(),
                "wss://relay.two".to_string(),
            ],
            None,
        )
        .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        // secret_key 行が無い（平文の鍵が config に出ない）。
        assert!(
            !content.contains("secret_key"),
            "config に secret_key が書かれている: {content}"
        );
        assert!(
            !content.contains("nsec1"),
            "config に nsec が出ている: {content}"
        );
        // relays と default_relays の両方が同じリレー集合で書かれる。
        assert!(
            content.contains("relays = [\"wss://relay-one.example.com\", \"wss://relay.two\"]"),
            "relays missing: {content}"
        );
        assert!(
            content.contains(
                "default_relays = [\"wss://relay-one.example.com\", \"wss://relay.two\"]"
            ),
            "default_relays missing: {content}"
        );
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    #[test]
    fn test_mask_secrets_redacts_nsec_and_secret_key() {
        // config パース失敗時、nostaro が stderr へエコーする先頭行を模す。
        let stderr = "Error: TOML parse error at line 1, column 1\n  \
                      secret_key = \"nsec1supersecretkeymaterial\"\nmissing field `default_relays`";
        let masked = mask_secrets(stderr);
        assert!(
            !masked.contains("nsec1supersecretkeymaterial"),
            "nsec leaked: {masked}"
        );
        assert!(
            masked.contains("<redacted>"),
            "no redaction marker: {masked}"
        );
        // 非秘密の診断情報は残す。
        assert!(masked.contains("missing field `default_relays`"));
        assert!(masked.contains("TOML parse error"));
        // 行途中に現れる nsec トークンも落とす。
        let inline = mask_secrets("prefix nsec1deadbeefcafe suffix");
        assert!(
            !inline.contains("nsec1deadbeefcafe"),
            "inline leaked: {inline}"
        );
        assert!(inline.contains("nsec1<redacted>"));
        assert!(inline.contains("prefix") && inline.contains("suffix"));
    }

    #[test]
    fn test_mask_secrets_handles_gutter_and_hex_forms() {
        // nostaro 0.3.0 の実際のガター付きエラー出力形（行番号 + `|`）を模す。
        // 値は明らかにダミー（実鍵ではない）。
        let gutter = "error: TOML parse error at line 1, column 1\n  \
                      |\n1 | secret_key = \"nsec1dummydummydummydummydummy\"\n  \
                      | ^^^^^^^^^^^^\nmissing field `default_relays`";
        let masked = mask_secrets(gutter);
        assert!(
            !masked.contains("nsec1dummydummydummydummydummy"),
            "gutter nsec leaked: {masked}"
        );
        // 第1層（行伏せ）で secret_key 行の値そのものが残らない。
        assert!(masked.contains("secret_key = \"<redacted>\""));
        // 行番号ガターなど診断の前置きと、別行の診断情報は保持。
        assert!(masked.contains("1 | secret_key"));
        assert!(masked.contains("missing field `default_relays`"));

        // nsec でない 64hex 秘密（bech32 でないので第2層に掛からない）も第1層で行ごと落とす。
        let hex_secret = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let hex_line = format!("1 | secret_key = \"{hex_secret}\"");
        let masked_hex = mask_secrets(&hex_line);
        assert!(
            !masked_hex.contains(hex_secret),
            "hex secret leaked: {masked_hex}"
        );
        assert!(masked_hex.contains("secret_key = \"<redacted>\""));

        // `secret_key` を含んでも秘密値でない診断行は潰しすぎるが漏れ側には倒れない。
        // 少なくとも他の診断行は残ることを確認（過剰マスクで全滅しない）。
        let mixed =
            "note: unknown field `foo`\nsecret_key = \"nsec1dummy\"\nhelp: add default_relays";
        let masked_mixed = mask_secrets(mixed);
        assert!(masked_mixed.contains("unknown field `foo`"));
        assert!(masked_mixed.contains("help: add default_relays"));
        assert!(!masked_mixed.contains("nsec1dummy"));
    }

    /// #620: `from`（生成鍵）経路は**鍵行なしの本設定**を `--config` に使い、生成鍵は
    /// env で注入する（平文 from-config は作らない）。本設定の relays を継承する。
    #[test]
    fn test_from_command_uses_main_config_and_injects_generated_key() {
        let cli = NostaroCli::new();
        let agent = "agent-from-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        // 本設定（relays を継承させる・鍵行なし）と生成鍵を用意。
        NostaroCli::materialize_config(agent, &["wss://yabu.me".to_string()], None).unwrap();
        let key = GeneratedKey {
            nsec: "nsec1gen".into(),
            npub: "npub1genkey".into(),
            pubkey: "hex".into(),
        };
        cli.save_generated_key(agent, &key).unwrap();

        let cmd = cli.generated_key_command(agent, "npub1genkey").unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // --config は**本設定 config.toml**（from-config ではない）。
        assert!(
            args.iter()
                .any(|a| a.ends_with(&format!("data/agents/{agent}/nostr/config.toml"))),
            "--config が本設定を指していない: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.contains(".config.toml")),
            "from-config (.config.toml) が使われている（平文経路が残っている）: {args:?}"
        );
        // from-config ファイルは作られない。
        assert!(
            !NostaroCli::agent_nostr_dir(agent)
                .unwrap()
                .join("generated-keys/npub1genkey.config.toml")
                .exists(),
            "平文 from-config が作られている"
        );
        // 生成鍵は env で注入される（本鍵ではなく生成鍵）。
        let env_key = cmd
            .as_std()
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(SECRET_KEY_ENV))
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().to_string());
        assert_eq!(
            env_key.as_deref(),
            Some("nsec1gen"),
            "生成鍵が env に載っていない"
        );
        // [#299] `from` 経路も cwd はエージェント workspace。config は絶対パス。
        let cwd = cmd.as_std().get_current_dir().unwrap();
        assert!(
            cwd.ends_with(format!("data/agents/{agent}/workspace")),
            "from 経路の cwd が workspace でない: {}",
            cwd.display()
        );
        assert!(std::path::Path::new(&args[1]).is_absolute(), "{:?}", args);
        // 存在しない npub は拒否（自分が生成した鍵のみ from 指定可）。
        assert!(cli.generated_key_command(agent, "npub1missing").is_err());
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        let _ = std::fs::remove_dir_all(cli.agent_workspace_dir(agent).unwrap());
    }

    /// #620 **鍵混同防止**: base_command（本鍵）と generated_key_command（生成鍵）が
    /// **別々の鍵**を env に載せ、共有点 command_with_config には鍵が載らないこと。
    #[test]
    fn test_base_and_generated_inject_different_keys_no_confusion() {
        let agent = "agent-keymix-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        NostaroCli::materialize_config(agent, &["wss://yabu.me".to_string()], None).unwrap();
        let cli = NostaroCli::new().with_main_key_provider(std::sync::Arc::new(|_id: &str| {
            Ok(Zeroizing::new("nsec1MAINkey".to_string()))
        }));
        cli.save_generated_key(
            agent,
            &GeneratedKey {
                nsec: "nsec1GENkey".into(),
                npub: "npub1genmix".into(),
                pubkey: "hex".into(),
            },
        )
        .unwrap();

        let env_of = |cmd: &Command| -> Option<String> {
            cmd.as_std()
                .get_envs()
                .find(|(k, _)| *k == std::ffi::OsStr::new(SECRET_KEY_ENV))
                .and_then(|(_, v)| v)
                .map(|v| v.to_string_lossy().to_string())
        };

        // 本鍵経路（post/reply/pubkey/watch）は本鍵を注入。
        let base = cli.base_command(agent).unwrap();
        assert_eq!(env_of(&base).as_deref(), Some("nsec1MAINkey"));
        // 生成鍵経路（from）は生成鍵を注入（本鍵ではない）。
        let gen = cli.generated_key_command(agent, "npub1genmix").unwrap();
        assert_eq!(env_of(&gen).as_deref(), Some("nsec1GENkey"));
        // 共有点 command_with_config には鍵を差さない（provider があっても）。
        let shared = cli.command_with_config(agent, &NostaroCli::agent_config_path(agent).unwrap());
        assert_eq!(
            env_of(&shared),
            None,
            "共有点に鍵が載っている（一律注入は鍵混同事故になる）"
        );
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    #[test]
    fn test_parse_generated_key() {
        // 進捗ログが前に混ざっても最後の JSON 行を採る。
        let out = "searching...\nfound after 1234 tries\n{\"nsec\":\"nsec1abc\",\"npub\":\"npub1crab\",\"pubkey\":\"deadbeef\"}";
        let k = parse_generated_key(out).unwrap();
        assert_eq!(k.nsec, "nsec1abc");
        assert_eq!(k.npub, "npub1crab");
        assert_eq!(k.pubkey, "deadbeef");
        // nsec 空 or JSON 無しはエラー。
        assert!(parse_generated_key("no json here").is_err());
        assert!(parse_generated_key("{\"npub\":\"npub1x\"}").is_err());
    }

    #[test]
    fn test_parse_error_never_leaks_secret() {
        // `--json` 非対応版が生鍵を吐く / JSON 破損時でも、エラー文言に nsec を載せない。
        let leaky_plain = "nsec1supersecretkey npub1pub";
        let e = parse_generated_key(leaky_plain).unwrap_err().to_string();
        assert!(!e.contains("nsec1supersecret"), "plain stdout leaked: {e}");
        let leaky_json = "{\"nsec\":\"nsec1supersecretkey\", BROKEN";
        let e = parse_generated_key(leaky_json).unwrap_err().to_string();
        assert!(!e.contains("nsec1supersecret"), "json line leaked: {e}");
    }

