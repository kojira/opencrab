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

