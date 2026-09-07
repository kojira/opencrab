
    /// #620: LLM 経路でも nsec キー名マスクは**しない**（撤去）。上限未満なので原文のまま。
    #[test]
    fn llm_result_no_longer_key_masks() {
        let json = r#"{"success":true,"data":{"nsec":"nsec1synthetic"},"error":null}"#;
        let out = sanitize_tool_result_for_llm("nostr_generate_key", json, "sess", "tc-1", None);
        assert_eq!(out, json, "上限未満は原文のまま（マスクしない）");
    }

    // ---- #616: 退避本文の整形（render_offload_body）と行境界打ち切り ----

    /// C3: shell 成功系（exit_code==0 かつ stderr 空）は**ヘッダ無し**で stdout を verbatim。
    /// 実改行が保たれ、`\n` の 2 文字化が起きない。
    #[test]
    fn render_shell_success_is_headerless_verbatim() {
        let stdout = "first line\nsecond line\n{\"json\":\"payload\"}\n";
        let env = serde_json::json!({
            "success": true,
            "data": {"stdout": stdout, "stderr": "", "exit_code": 0, "truncated": false},
            "error": null
        })
        .to_string();
        let (body, fmt) = render_offload_body(&env);
        assert_eq!(body.as_ref(), stdout, "ヘッダ無しで stdout そのまま");
        assert!(!body.contains("\\n"), "\\n が 2 文字化している: {body}");
        assert!(!body.contains("exit_code="), "成功系にヘッダが付いた");
        // #624: shell 生テキストは .txt。
        assert_eq!(fmt, OffloadFormat::Text);
        assert_eq!(fmt.extension(), "txt");
    }

    /// C3: 非ゼロ終了 or stderr 非空はヘッダが付く。stdout/stderr は生テキスト。
    #[test]
    fn render_shell_failure_gets_header() {
        // 非ゼロ終了。
        let env = serde_json::json!({
            "success": true,
            "data": {"stdout": "partial\noutput", "stderr": "boom\n", "exit_code": 2, "truncated": false},
            "error": null
        })
        .to_string();
        let (body, fmt) = render_offload_body(&env);
        assert!(body.starts_with("exit_code=2\n"), "{body}");
        assert!(body.contains("--- stderr ---\nboom\n"), "{body}");
        assert!(body.contains("--- stdout ---\npartial\noutput"), "{body}");
        // #624: ヘッダ付きでも shell 由来なので生テキスト＝ .txt。
        assert_eq!(fmt, OffloadFormat::Text);

        // exit_code==0 でも stderr 非空ならヘッダ。
        let env2 = serde_json::json!({
            "success": true,
            "data": {"stdout": "ok", "stderr": "warning", "exit_code": 0, "truncated": false},
            "error": null
        })
        .to_string();
        let (body2, fmt2) = render_offload_body(&env2);
        assert!(body2.starts_with("exit_code=0\n"), "{body2}");
        assert!(body2.contains("--- stderr ---\nwarning"), "{body2}");
        assert_eq!(fmt2, OffloadFormat::Text);
    }

    /// (b): stdout の無い JSON は pretty 化され、format_hint が "JSON object" を出す。
    #[test]
    fn render_structured_json_is_pretty() {
        let env = r#"{"success":true,"data":{"items":[1,2,3]},"error":null}"#;
        let (body, fmt) = render_offload_body(env);
        assert!(body.contains('\n'), "pretty 化されていない: {body}");
        assert!(body.contains("  "), "インデントが無い: {body}");
        assert_eq!(format_hint(&body), Some("looks like a JSON object"));
        // #624: pretty JSON は .json（jq が通る）。
        assert_eq!(fmt, OffloadFormat::Json);
        assert_eq!(fmt.extension(), "json");
        // 中身は等価。
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::from_str::<serde_json::Value>(env).unwrap()
        );
    }

    /// (c): parse 失敗は生バイト verbatim（借用のまま＝再割り当てしない）。
    /// #624: JSON ではないので .txt（`jq` を誘導しない）。
    #[test]
    fn render_non_json_is_borrowed_verbatim() {
        let raw = "not json at all\nline2\n";
        let (body, fmt) = render_offload_body(raw);
        assert_eq!(body.as_ref(), raw);
        assert!(matches!(body, std::borrow::Cow::Borrowed(_)));
        assert_eq!(fmt, OffloadFormat::Text);
        assert_eq!(fmt.extension(), "txt");
    }

    /// #619 レビュー: 打ち切りは**文字境界**でほぼ全量を保存し、末尾を改行で終える。行境界で
    /// 切る旧実装だと本文全体を捨てかねない（次テスト参照）ので採らない。改行を含む本文でも
    /// 「行の途中で切れる」ことは許容し（生テキストは読めて grep も効く）、ファイル完結は末尾の
    /// 改行 1 つで担保する。
    #[test]
    fn offload_truncates_at_char_boundary_and_ends_with_newline() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut body = String::with_capacity(OFFLOAD_FILE_BYTE_LIMIT + 8_000);
        while body.len() <= OFFLOAD_FILE_BYTE_LIMIT + 4_000 {
            body.push_str(&"x".repeat(1_000));
            body.push('\n');
        }
        assert!(body.len() > OFFLOAD_FILE_BYTE_LIMIT);

        let saved = offload_to_workspace(&body, "txt", "sL", "tL", Some(dir.path())).unwrap();
        let n = saved.saved_prefix_bytes.expect("切り詰められている");
        // 文字境界＝全部 ASCII なので上限ちょうど。ほぼ全量（上限分）を保存する。
        assert_eq!(n, OFFLOAD_FILE_BYTE_LIMIT);

        let on_disk = std::fs::read_to_string(dir.path().join("tmp/sL-tL.txt")).unwrap();
        assert!(on_disk.ends_with('\n'), "改行で終わっていない");
        // 上限ぶん + 完結用の改行（本文末尾がちょうど改行なら足さないが、この本文は途中で切れる）。
        assert!(
            on_disk.len() >= OFFLOAD_FILE_BYTE_LIMIT,
            "ほぼ全量が保存されていない"
        );
    }

    /// #619 レビューの回帰: 「早い位置に改行が 1 つ + 改行なしの巨大本文」で、**ほぼ全量**が
    /// 保存されること。窓内の最後の改行で切る旧実装だと end=7 になり、保存できたはずの ~10MB を
    /// 7 バイトへ激減させていた。文字境界で切る新実装はこれを起こさない。
    #[test]
    fn offload_early_single_newline_still_saves_near_full() {
        let dir = tempfile::TempDir::new().unwrap();
        // 7 バイト目に改行が 1 つ、以降は改行ゼロで上限超。
        let body = format!("header\n{}", "a".repeat(OFFLOAD_FILE_BYTE_LIMIT + 500));
        let saved = offload_to_workspace(&body, "txt", "sE", "tE", Some(dir.path())).unwrap();
        let n = saved.saved_prefix_bytes.expect("切り詰められている");
        // 旧実装なら 7。新実装は上限ちょうど（全部 ASCII）。
        assert_eq!(
            n, OFFLOAD_FILE_BYTE_LIMIT,
            "早い改行でデータが激減した（退行）"
        );
        let on_disk = std::fs::read_to_string(dir.path().join("tmp/sE-tE.txt")).unwrap();
        assert!(on_disk.ends_with('\n'), "改行で終わっていない");
        assert!(on_disk.len() > body.len() / 2, "ほぼ全量が保存されていない");
    }

    /// 改行が 1 つも無い 10MiB 超の本文でも、空ファイルにせずほぼ全量を保存し、末尾に改行を
    /// 足してファイルを完結させる（文字境界で切る＝壊れた UTF-8 にしない）。
    #[test]
    fn offload_no_newline_saves_near_full_and_appends_newline() {
        let dir = tempfile::TempDir::new().unwrap();
        let body = "a".repeat(OFFLOAD_FILE_BYTE_LIMIT + 500); // 改行ゼロ
        let saved = offload_to_workspace(&body, "txt", "sN", "tN", Some(dir.path())).unwrap();
        let n = saved.saved_prefix_bytes.expect("切り詰められている");
        // 全部 ASCII なので文字境界＝上限ちょうど。
        assert_eq!(n, OFFLOAD_FILE_BYTE_LIMIT);
        let on_disk = std::fs::read(dir.path().join("tmp/sN-tN.txt")).unwrap();
        assert!(!on_disk.is_empty(), "空ファイル");
        // 上限ぶん + 足した改行 1 バイト。
        assert_eq!(on_disk.len(), OFFLOAD_FILE_BYTE_LIMIT + 1);
        assert_eq!(on_disk.last(), Some(&b'\n'), "改行で終わっていない");
        assert!(std::str::from_utf8(&on_disk).is_ok(), "壊れた UTF-8");
    }

    /// C2 統合: shell の巨大 stdout を退避すると、ファイルは**行が保たれ**部分読み・検索が効き、
    /// notice の bytes/lines/tokens が**実ファイル**と一致する（「1 lines」にならない）。
    #[test]
    fn shell_offload_preserves_lines_and_notice_counts_match_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let stdout_text = (0..4_000)
            .map(|i| format!("row {i:05} value"))
            .collect::<Vec<_>>()
            .join("\n"); // 4000 行・末尾改行なし
        let env = serde_json::json!({
            "success": true,
            "data": {"stdout": stdout_text, "stderr": "", "exit_code": 0, "truncated": false},
            "error": null
        })
        .to_string();
        // エンベロープは上限を超える。
        assert!(exceeds_limit(&env, TOOL_RESULT_TOKEN_LIMIT), "前提: 上限超");

        let out = sanitize_tool_result_for_llm("execute_shell", &env, "sX", "tX", Some(dir.path()));

        // ファイルは stdout そのもの（実改行・ヘッダ無し）。#624: shell 生テキストは .txt。
        let saved = std::fs::read_to_string(dir.path().join("tmp/sX-tX.txt")).unwrap();
        assert_eq!(saved, stdout_text);
        assert!(!saved.contains("\\n"), "\\n が 2 文字化している");
        assert_eq!(count_lines(&saved), 4_000);

        // notice は保存本文の実数と一致する（C2: 「1 lines」にならない）。
        assert!(
            out.contains("4000 lines"),
            "行数が保存本文と一致しない: {out}"
        );
        assert!(
            out.contains(&format!("{} bytes", saved.len())),
            "保存本文サイズが notice に無い: {out}"
        );
        assert!(
            out.contains(&format!(
                "~{} tokens",
                crate::tokens::estimate_tokens_bounded(&saved)
            )),
            "トークン数が保存本文基準でない: {out}"
        );
        // shell の生テキストは "JSON object" と偽らない（C4: format_hint は保存本文基準）。
        assert!(
            !out.contains("looks like a JSON"),
            "生テキストを JSON と偽った: {out}"
        );
    }

    /// #620: nsec キー名マスクは撤去したので、退避（オフロード）でも notice には生データが
    /// 1 バイトも入らない（#294 の性質は不変）が、退避ファイル本文はマスクされずそのまま
    /// 書かれる（キー名マスクの復活が無いこと＝撤去の固定）。
    #[test]
    fn offload_does_not_key_mask_saved_body() {
        let dir = tempfile::TempDir::new().unwrap();
        let filler = "z".repeat(TOOL_RESULT_TOKEN_LIMIT * 4);
        let env = format!(
            r#"{{"success":true,"data":{{"nsec":"nsec1synthetic","note":"{filler}"}},"error":null}}"#
        );
        let out = sanitize_tool_result_for_llm("any_tool", &env, "sS", "tS", Some(dir.path()));
        // notice（inline）には生データを載せない（#294 は不変）。
        assert!(!out.contains("nsec1synthetic"), "notice に生データ: {out}");
        assert!(
            out.contains("Tool result withheld"),
            "退避 notice でない: {out}"
        );
        // 退避ファイル本文はキー名マスクされない（撤去の固定）。#624: 構造化 JSON は .json。
        let saved = std::fs::read_to_string(dir.path().join("tmp/sS-tS.json")).unwrap();
        assert!(
            !saved.contains("[redacted]"),
            "撤去したはずのキー名マスクが復活している: {saved:.120}"
        );
    }

    // ---- #624: 拡張子を中身に合わせる / 通知に読み方レシピを入れる ----

    /// #624: 退避ファイルの拡張子は**中身**に合わせる。shell 生テキストと parse 失敗の
    /// verbatim は `.txt`（JSON ではないので `jq` を誘導しない）、pretty JSON は `.json`。
    /// sanitize の全経路で実ファイルが正しい拡張子で作られることを 1 か所で固定する。
    #[test]
    fn offload_extension_matches_content() {
        let filler = "word ".repeat(TOOL_RESULT_TOKEN_LIMIT); // 確実に上限超

        // (a) shell 生テキスト（data.stdout が string・成功系）→ .txt。
        let dir_a = tempfile::TempDir::new().unwrap();
        let shell_env = serde_json::json!({
            "success": true,
            "data": {"stdout": filler.clone(), "stderr": "", "exit_code": 0, "truncated": false},
            "error": null
        })
        .to_string();
        let out_a = sanitize_tool_result_for_llm(
            "execute_shell",
            &shell_env,
            "sA",
            "tA",
            Some(dir_a.path()),
        );
        assert!(
            dir_a.path().join("tmp/sA-tA.txt").exists(),
            "shell が .txt でない"
        );
        assert!(
            !dir_a.path().join("tmp/sA-tA.json").exists(),
            ".json も作られた"
        );
        assert!(
            out_a.contains("tmp/sA-tA.txt"),
            "notice のパスが .txt でない: {out_a}"
        );

        // (b) 構造化 JSON（stdout string 無し）→ .json。
        let dir_b = tempfile::TempDir::new().unwrap();
        let json_env = format!(r#"{{"success":true,"data":{{"note":"{filler}"}},"error":null}}"#);
        let out_b =
            sanitize_tool_result_for_llm("read_file", &json_env, "sB", "tB", Some(dir_b.path()));
        assert!(
            dir_b.path().join("tmp/sB-tB.json").exists(),
            "JSON が .json でない"
        );
        assert!(
            out_b.contains("tmp/sB-tB.json"),
            "notice のパスが .json でない: {out_b}"
        );

        // (c) parse 失敗の verbatim（非 JSON）→ .txt。
        let dir_c = tempfile::TempDir::new().unwrap();
        let raw = "line one\n".repeat(TOOL_RESULT_TOKEN_LIMIT); // 非 JSON・上限超
        let out_c =
            sanitize_tool_result_for_llm("execute_shell", &raw, "sC", "tC", Some(dir_c.path()));
        assert!(
            dir_c.path().join("tmp/sC-tC.txt").exists(),
            "verbatim が .txt でない"
        );
        assert!(
            out_c.contains("tmp/sC-tC.txt"),
            "notice のパスが .txt でない: {out_c}"
        );
    }
