    /// 環境変数を触るテストを直列化するロック。
    ///
    /// 環境変数はプロセス全体で共有されるので、`cargo test` の並列実行下では
    /// あるテストの `set_var`/`remove_var` が別テストの読み取りに割り込む。
    /// 必要なのは「同一プロセス内での直列化」だけなので、`serial_test` を
    /// 依存に追加せず標準ライブラリの `Mutex` で済ませている。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `ENV_LOCK` を取得する。テストが panic してロックが poison されても、
    /// 後続テストが道連れで落ちないように中身を取り出して続行する。
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 環境変数をテスト中だけ差し替え、`Drop` で元の状態（未設定なら未設定）に
    /// 戻す RAII ガード。assert 失敗で panic しても復元されるため、値が後続
    /// テストや開発者のシェル由来の設定に漏れない。
    struct EnvVarGuard {
        key: String,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self {
                key: key.to_string(),
                previous,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }

    #[test]
    fn test_expand_env_vars() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::set("TEST_EXPAND_KEY", "hello123");
        let input = "api_key = \"${TEST_EXPAND_KEY}\"";
        let result = expand_env_vars(input);
        assert_eq!(result, "api_key = \"hello123\"");
    }

    #[test]
    fn test_expand_env_vars_missing() {
        let _lock = env_lock();
        let input = "api_key = \"${NONEXISTENT_VAR_12345}\"";
        let result = expand_env_vars(input);
        assert_eq!(result, "api_key = \"\"");
    }

    #[test]
    fn test_expand_env_vars_multiple() {
        let _lock = env_lock();
        let _a = EnvVarGuard::set("TEST_A", "aaa");
        let _b = EnvVarGuard::set("TEST_B", "bbb");
        let input = "${TEST_A} and ${TEST_B}";
        let result = expand_env_vars(input);
        assert_eq!(result, "aaa and bbb");
    }

    /// #171 失敗モード1（起動ハング）: 値が自分自身を参照しても収束し、必ず有限で終わる。
    /// 単一パス走査は置換値を再走査しないので、値中の `${SELF...}` は展開されず残る。
    /// 旧実装（毎回先頭から再走査）ではここが無限ループになっていた。
    #[test]
    fn test_expand_env_vars_self_reference_does_not_loop() {
        let _lock = env_lock();
        let _g = EnvVarGuard::set("TEST_SELF_REF_171", "x${TEST_SELF_REF_171}y");
        let result = expand_env_vars("${TEST_SELF_REF_171}");
        // 一度だけ置換され、値中の参照はリテラルのまま（再展開しない）。
        assert_eq!(result, "x${TEST_SELF_REF_171}y");
    }

    /// #171: 置換した値は再走査しない（単一パス）。旧実装は展開結果を再解釈したため、
    /// 値に別の参照が含まれると連鎖展開していた。この差分がそのまま変異検知になる
    /// （旧挙動なら "final"、単一パスなら "${TEST_NEST_INNER_171}"）。
    #[test]
    fn test_expand_env_vars_no_nested_reexpansion() {
        let _lock = env_lock();
        let _outer = EnvVarGuard::set("TEST_NEST_OUTER_171", "${TEST_NEST_INNER_171}");
        let _inner = EnvVarGuard::set("TEST_NEST_INNER_171", "final");
        let result = expand_env_vars("${TEST_NEST_OUTER_171}");
        assert_eq!(result, "${TEST_NEST_INNER_171}");
    }

    /// #171 失敗モード2（設定行の無言消失）: `${` に同じ行で対応する `}` が無い場合、
    /// 別の行の `}` まで飲み込んで間の行を消してはならない。閉じが無ければ `${` を
    /// リテラルとして残し、後続行は保存する。旧実装は b / c 行を丸ごと失っていた。
    #[test]
    fn test_expand_env_vars_unterminated_does_not_eat_following_lines() {
        let _lock = env_lock();
        let input = "a = \"${TEST_UNTERMINATED_171\"\nb = \"keep\"\nc = \"}\"\n";
        let result = expand_env_vars(input);
        assert!(
            result.contains("b = \"keep\""),
            "後続行が消えてはならない: {result:?}"
        );
        assert!(
            result.contains("c = \"}\""),
            "後続行が消えてはならない: {result:?}"
        );
        // 未終端の `${` はリテラルとして残る（展開しようとして周囲を壊さない）。
        assert!(
            result.contains("${TEST_UNTERMINATED_171"),
            "未終端の `${{` はリテラルで残る: {result:?}"
        );
    }

    /// #171 失敗モード3（原因の追えない値）: 値に `"` が含まれても、周囲を壊さず
    /// リテラルとして差し込む（TOML パースは下流で落ちるが、そのときは変数名付きの
    /// 警告が出ている）。ここでは差し込み自体が素直に行われることを固定する。
    #[test]
    fn test_expand_env_vars_value_with_quote_is_inserted_verbatim() {
        let _lock = env_lock();
        let _g = EnvVarGuard::set("TEST_QUOTE_VALUE_171", "a\"b");
        let result = expand_env_vars("k = \"${TEST_QUOTE_VALUE_171}\"");
        assert_eq!(result, "k = \"a\"b\"");
    }

    /// `owner_discord_id` は環境変数参照で与える（ローカル固有値を `.env` に寄せる）。
    #[test]
    fn owner_discord_id_expands_from_env() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::set("TEST_OWNER_DISCORD_ID", "123456789012345678");
        let raw = "[gateway.discord]\nowner_discord_id = \"${TEST_OWNER_DISCORD_ID}\"\n";
        let cfg: AppConfig = toml::from_str(&expand_env_vars(raw)).unwrap();
        assert_eq!(cfg.gateway.discord.owner_discord_id, "123456789012345678");
        assert!(crate::api::is_owner_id(
            &cfg.gateway.discord.owner_discord_id,
            "123456789012345678"
        ));
    }

    /// 環境変数が未設定なら空文字に展開される。空のオーナー ID は誰とも一致させない
    /// （= オーナー無し扱い）ので、空の caller が owner に昇格しない。
    #[test]
    fn unset_owner_discord_id_grants_owner_to_nobody() {
        let _lock = env_lock();
        let raw = "[gateway.discord]\nowner_discord_id = \"${UNSET_OWNER_DISCORD_ID_FOR_TEST}\"\n";
        let cfg: AppConfig = toml::from_str(&expand_env_vars(raw)).unwrap();
        let owner = &cfg.gateway.discord.owner_discord_id;
        assert!(owner.is_empty());
        assert!(!crate::api::is_owner_id(owner, ""));
        assert!(!crate::api::is_owner_id(owner, "123456789012345678"));
    }

    /// `load_config`（ファイル読み込み → `${}` 展開 → TOML パース）を通しても
    /// 環境変数の値が `owner_discord_id` に入る。
    #[test]
    fn load_config_expands_owner_discord_id_from_env() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::set("TEST_LOAD_OWNER_DISCORD_ID", "123456789012345678");
        // 一時ディレクトリはテストごとにユニークで、`TempDir` の Drop で削除される
        // （固定パスの残骸を作らない / 並列実行でも衝突しない）。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner.toml");
        std::fs::write(
            &path,
            "[gateway.discord]\nenabled = true\nowner_discord_id = \"${TEST_LOAD_OWNER_DISCORD_ID}\"\n",
        )
        .unwrap();
        let cfg = load_config(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.gateway.discord.owner_discord_id, "123456789012345678");
    }

    /// リポジトリに追跡されている設定ファイルは、owner を実 ID の直書きではなく
    /// `${OWNER_DISCORD_ID}` 参照で持つ。
    ///
    /// 期待値は実測と独立なセンチネル定数に固定する（環境変数から期待値を導出すると
    /// 参照が壊れていても両辺が同じ値になりトートロジーになる）。これにより
    /// 「変数名の typo」「実 ID の直書きへの逆戻り」「参照ごと消える」のいずれでも落ちる。
    ///
    /// 本番 (`crates/server/src/main.rs`) と CLI がロードするのは `config/default.toml`
    /// なので、配布テンプレートだけでなく両方を回す。
    ///
    /// **注意**: `config/default.toml` は追跡ファイル（＝各開発者の実稼働設定）なので、
    /// このテストは作業コピーの中身も検査する。ローカルで owner を実 ID に直書きすると
    /// 無関係な変更でも `cargo test` が落ちる。これは意図した挙動で、作業コピーでも
    /// `${OWNER_DISCORD_ID}` 参照を維持すること（ローカル固有値は `.env` に置く）。
    #[test]
    fn shipped_configs_take_owner_discord_id_from_env() {
        let _lock = env_lock();
        // 値はプレーンなリテラル（`${}` を含まない）にしておく。単一パス展開なので
        // 仮に含んでも再展開はされないが、期待値を曖昧にしないため素の文字列を使う。
        const SENTINEL: &str = "sentinel-owner-000";
        let _guard = EnvVarGuard::set("OWNER_DISCORD_ID", SENTINEL);

        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for name in ["config/default.toml.example", "config/default.toml"] {
            let path = repo_root.join(name);
            let cfg = load_config(path.to_str().unwrap()).unwrap_or_else(|e| {
                panic!(
                    "{name} is not valid TOML (check your working copy of this tracked file): {e:#}"
                )
            });
            assert_eq!(
                cfg.gateway.discord.owner_discord_id, SENTINEL,
                "{name}: owner_discord_id must resolve from ${{OWNER_DISCORD_ID}} \
                 (a literal ID written into this tracked file, or a typo in the variable name, \
                 breaks this). Keep the ${{OWNER_DISCORD_ID}} reference and put your own ID in .env"
            );
        }
    }

    /// Cursor CLI（`agent` / `cursor-agent`）は config のグローバル許可リストに載せない。
    ///
    /// `-p --force` で起動した Cursor CLI は自身が write と shell を持つため、許可
    /// コマンド一覧の外にあることまで実行できてしまう（許可リストが実質無効になる）。
    /// 必要なエージェントにだけ per-agent の `agent_allowed_commands` で与える運用に
    /// 固定し、config への逆戻りを回帰ガードする。
    #[test]
    fn shipped_configs_do_not_globally_allow_cursor_cli() {
        let _lock = env_lock();
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for name in ["config/default.toml.example", "config/default.toml"] {
            let path = repo_root.join(name);
            let cfg = load_config(path.to_str().unwrap())
                .unwrap_or_else(|e| panic!("{name} is not valid TOML: {e:#}"));
            let Some(shell) = cfg.tools.shell.as_ref() else {
                continue;
            };
            let allowed: Vec<String> = shell
                .effective_commands()
                .into_iter()
                .map(|c| c.name)
                .collect();
            for forbidden in ["agent", "cursor-agent", "cursor"] {
                assert!(
                    !allowed.iter().any(|c| c == forbidden),
                    "{name}: '{forbidden}' must not be globally allowed \
                     (it can spawn an agent with its own write/shell access, which bypasses \
                     this allowlist). Grant it per-agent via agent_allowed_commands instead"
                );
            }
        }
    }

    /// 配布テンプレートは共有ゲートウェイを既定で無効にしている（個人 ID や
    /// トークンを持たない状態で配られる）。
    ///
    /// `load_config` は `${}` 展開で環境変数を読むので、`set_var` する他テストと
    /// 直列化するため `env_lock()` を取る。
    #[test]
    fn shipped_config_example_keeps_shared_gateway_disabled() {
        let _lock = env_lock();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/default.toml.example");
        let cfg = load_config(path.to_str().unwrap()).expect("default.toml.example must parse");
        assert!(!cfg.gateway.discord.enabled);
    }

    /// `load_config` は owner を trim して返す（`.env` のコピペで前後に空白が
    /// 混ざっても、生比較が残る下位経路と判定がズレない）。
    #[test]
    fn load_config_trims_owner_discord_id() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::set("TEST_TRIM_OWNER_DISCORD_ID", "  123456789012345678\t");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner-trim.toml");
        std::fs::write(
            &path,
            "[gateway.discord]\nowner_discord_id = \"${TEST_TRIM_OWNER_DISCORD_ID}\"\n",
        )
        .unwrap();
        let cfg = load_config(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.gateway.discord.owner_discord_id, "123456789012345678");
    }

