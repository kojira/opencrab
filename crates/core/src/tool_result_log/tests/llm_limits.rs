
    /// 退避先が無くても生データは残さない（#294）。切り詰めた本文も session_logs へ
    /// 入れない — 次ターンで会話へ再注入され、結局 LLM が「先頭だけ」を読む。
    #[test]
    fn sanitize_keeps_no_raw_data_when_offload_is_impossible() {
        let big = format!(r#"{{"data":"{}"}}"#, "あ".repeat(TOOL_RESULT_TOKEN_LIMIT));
        let out = sanitize_tool_result_for_log("read_file", &big, "sess", "tc-1", None);
        assert!(!out.contains("あああ"), "生データが流れている: {out}");
        assert!(out.contains("could not be saved"), "{out}");
        assert!(out.contains("discarded"), "{out}");
    }

    /// tool_call_id にパス区切りが混ざってもワークスペースの外へ書かない（#284）。
    #[test]
    fn offload_sanitizes_path_components() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = format!(r#"{{"data":"{}"}}"#, "x ".repeat(TOOL_RESULT_TOKEN_LIMIT));
        let out = sanitize_tool_result_for_log(
            "read_file",
            &big,
            "sess",
            "../../etc/passwd",
            Some(dir.path()),
        );
        assert!(!out.contains(".."));
        assert_eq!(dir.path().join("tmp").read_dir().unwrap().count(), 1);
    }

    /// #294 中核: 上限超過時、LLM へ渡る本文に**生データが 1 バイトも含まれない**。
    #[test]
    fn llm_result_contains_no_raw_data() {
        let dir = tempfile::TempDir::new().unwrap();
        // 実事故（#284）と同じ形の、979 人のフォロー一覧を模した結果。
        let entries: Vec<String> = (0..979)
            .map(|i| format!(r#"{{"npub":"npub1follower{i:04}","name":"user{i:04}"}}"#))
            .collect();
        let big = format!(r#"{{"success":true,"data":[{}]}}"#, entries.join(","));
        assert!(big.len() > 40_000, "前提が崩れている: {}", big.len());

        let out = sanitize_tool_result_for_llm(
            "nostr_get_following",
            &big,
            "sessA",
            "tc1",
            Some(dir.path()),
        );

        // 元データの特徴的な文字列は 1 つも出てこない（先頭の 1 件すら渡さない）。
        assert!(
            !out.contains("npub1follower0000"),
            "生データが流れている: {out}"
        );
        assert!(
            !out.contains("npub1follower"),
            "生データが流れている: {out}"
        );
        assert!(!out.contains("user0000"), "生データが流れている: {out}");
        // 案内はメタ情報＋読み方レシピ＋狭めて取り直す導線だけで、なお小さい（76KB → 1KB 台）。
        // #624: 読み方レシピ（grep -n → ws_read / head -c）を足したぶん増えたが、生データを
        // 載せないので依然として桁違いに小さい。上限（2,500 トークン ≒ 数 KB）を食い破らない。
        assert!(out.len() < 1_400, "案内が肥大している: {} bytes", out.len());

        // 全文は退避され、そこを指している（#616: stdout の無い JSON は pretty 化）。
        assert!(out.contains("tmp/sessA-tc1.json"), "{out}");
        let saved = std::fs::read_to_string(dir.path().join("tmp/sessA-tc1.json")).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&saved).unwrap(),
            serde_json::from_str::<serde_json::Value>(&big).unwrap()
        );
        // 通知の bytes は「保存本文（pretty）」の実サイズと一致する（C2）。元サイズ
        // （エンベロープ）も規模のシグナルとして併記される。
        assert!(
            out.contains(&format!("Its saved form ({} bytes", saved.len())),
            "保存本文サイズが notice と一致しない: {out}"
        );
        assert!(
            out.contains(&format!("was {} bytes", big.len())),
            "元サイズ（エンベロープ）が notice に無い: {out}"
        );
    }

    /// 案内にはパス・バイトサイズ・行数・推定トークン数が載る（#294 のオーナー要求）。
    #[test]
    fn llm_notice_reports_path_bytes_lines_and_tokens() {
        let dir = tempfile::TempDir::new().unwrap();
        // 3 行（末尾改行なし）。
        let big = format!(
            "{}\n{}\n{}",
            "a ".repeat(2_000),
            "b ".repeat(2_000),
            "c ".repeat(2_000)
        );
        let out =
            sanitize_tool_result_for_llm("execute_shell", &big, "sessB", "tc2", Some(dir.path()));

        // #624: 生テキスト（parse 失敗の verbatim）は .txt。
        assert!(out.contains("tmp/sessB-tc2.txt"), "パスが無い: {out}");
        assert!(
            out.contains(&format!("{} bytes", big.len())),
            "バイトサイズが無い: {out}"
        );
        assert!(out.contains("3 lines"), "行数が無い: {out}");
        // 案内のトークン数は概算（`~` 付きの目安）。判定と同じ有界推定を使う（#576）。
        assert!(
            out.contains(&format!(
                "~{} tokens",
                crate::tokens::estimate_tokens_bounded(&big)
            )),
            "推定トークン数が無い: {out}"
        );
        // 参照方法は選択肢として示すだけで強制しない。
        assert!(out.contains("up to you how to use it"), "{out}");
        // ループ防止の趣旨は残す（#284）。
        assert!(out.contains("Do NOT re-run the same tool"), "{out}");
    }

    /// 形式の手がかりは判別できたときだけ載せる（無理なら省く）。
    #[test]
    fn format_hint_is_best_effort() {
        assert_eq!(format_hint(r#"{"a":1}"#), Some("looks like a JSON object"));
        assert_eq!(format_hint("  [1,2,3]\n"), Some("looks like a JSON array"));
        assert_eq!(format_hint("plain text output"), None);
        assert_eq!(format_hint(""), None);
    }

    /// 行数の数え方: 末尾改行は空行を増やさない。空文字列は 0 行。
    #[test]
    fn line_counting_matches_head_and_editors() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("\n"), 1);
        assert_eq!(count_lines("a"), 1);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb"), 2);
        assert_eq!(count_lines("a\nb\n"), 2);
        assert_eq!(count_lines("a\n\nb\n"), 3);
    }

    /// 上限未満の結果は LLM 経路でも素通り（回帰防止）。
    #[test]
    fn llm_result_under_limit_is_untouched() {
        let json = r#"{"success":true,"data":{"ok":true},"error":null}"#;
        let out = sanitize_tool_result_for_llm("read_file", json, "sess", "tc-1", None);
        assert_eq!(out, json);
    }

    /// 判定は**トークン基準**なので、日本語が「バイト量が多い」だけで不当に早く退避される
    /// ことはない（#294 の趣旨。#576 で全体トークナイズはやめたが単位はトークンのまま）。
    ///
    /// 日本語 1 文字 3 バイトの本文は、バイトで測ると実効トークン量よりずっと大きく見える。
    /// トークン数が上限未満なら、バイト数が上限相当を超えていても素通りする。
    #[test]
    fn japanese_text_is_measured_in_tokens_not_bytes() {
        // トークン上限に迫る量の日本語（バイトでは上限バイト換算 ~10KB を意識した長さ）だが、
        // トークン数では上限未満。ここで退避されないことを担保する。
        let json = format!(r#"{{"data":"{}"}}"#, "こんにちは世界".repeat(220));
        // バイトでは「上限トークン数」という数値（2,500）をゆうに超える一方…
        assert!(
            json.len() > TOOL_RESULT_TOKEN_LIMIT,
            "前提: バイトは 2,500 超"
        );
        // …トークンでは上限未満。だから退避されない。
        assert!(crate::tokens::estimate_tokens(&json) < TOOL_RESULT_TOKEN_LIMIT);
        let out = sanitize_tool_result_for_llm("read_file", &json, "sess", "tc-1", None);
        assert_eq!(out, json);
    }

    /// 退避判定は上限（2,500 トークン）の直下・直上・マルチバイト境界で、**正確な**
    /// トークン数と同じ側に落ちる（#576 の有界判定が境界をズラさない）。これらの本文は複数窓を
    /// 跨ぐが、CJK・空白区切りのトークンは窓境界（[`crate::tokens::BOUNDED_TOKENIZE_WINDOW`]）を
    /// 跨がないのでチャンク境界の上振れは出ない（上振れは base64/単一文字の長大ランのみ）。
    #[test]
    fn exceeds_limit_agrees_with_exact_token_count_across_boundary() {
        let samples: Vec<String> = vec![
            "あ".repeat(2_400),
            "あ".repeat(2_450),
            "あ".repeat(2_550),
            "あ".repeat(2_600),
            "word ".repeat(1_800),
            "word ".repeat(2_600),
            // マルチバイト＋ASCII 混在。複数窓を跨ぐが CJK/空白でトークンは境界を跨がない。
            format!("{}{}", "あ".repeat(1_250), "word ".repeat(1_250)),
        ];
        for s in &samples {
            let exact = crate::tokens::estimate_tokens(s);
            assert_eq!(
                exceeds_limit(s, TOOL_RESULT_TOKEN_LIMIT),
                exact >= TOOL_RESULT_TOKEN_LIMIT,
                "len={}, exact={exact}",
                s.len(),
            );
        }
    }

    /// 長い単一文字ラン（区切りの無い 1 pre-token）でも判定は返り、退避される。
    /// 全体を一括トークナイズしていたら 486MB 級で固まる経路を、有界判定が塞ぐ（#576）。
    /// 時間アサートは不安定なので、ここでは**判定が返って退避されること**だけを見る。
    #[test]
    fn huge_single_run_is_offloaded_without_hanging() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = "あ".repeat(100_000); // 300KB・単一 pre-token・確実に上限超
        let out =
            sanitize_tool_result_for_llm("execute_shell", &big, "sessR", "tcR", Some(dir.path()));
        assert!(out.contains("withheld"), "退避されていない: {out}");
        assert!(!out.contains("ああああ"), "生データが流れている");
        // 退避ファイルに全文が入っている（#624: 非 JSON は .txt）。
        let saved = std::fs::read_to_string(dir.path().join("tmp/sessR-tcR.txt")).unwrap();
        assert_eq!(saved.len(), big.len());
    }

    /// 退避できないときも生データを流さず、消えたことを LLM に伝える。
    #[test]
    fn llm_result_without_workspace_explains_the_data_is_gone() {
        let big = format!(r#"{{"data":"{}"}}"#, "あ".repeat(20_000));
        let out = sanitize_tool_result_for_llm("execute_shell", &big, "sess", "tc-1", None);
        assert!(!out.contains("あああ"), "生データが流れている: {out}");
        assert!(out.contains("could not be saved"), "{out}");
        assert!(out.contains("there is no file to read"), "{out}");
        assert!(out.contains("narrower arguments"), "{out}");
    }

    /// #286: 案内文が長くなっても（session_id / tool_call_id が長い）上限を超えない。
    ///
    /// 案内文が上限を食い破ると、永続化側の「上限未満なら素通り」を通過して
    /// LLM が見た本文と DB に残る本文が食い違う。
    #[test]
    fn llm_notice_with_long_ids_still_fits_the_limit() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = "q ".repeat(50_000);
        let long_session = "s".repeat(2_000);
        let long_call_id = "c".repeat(2_000);
        let out = sanitize_tool_result_for_llm(
            "read_file",
            &big,
            &long_session,
            &long_call_id,
            Some(dir.path()),
        );
        assert!(
            !exceeds_limit(&out, TOOL_RESULT_TOKEN_LIMIT),
            "案内文が枠を食い破っている: {} bytes",
            out.len()
        );
        // ID を切り詰めるのでファイル名長エラーにならず、ちゃんと退避できている。
        assert!(out.contains("tmp/ssss"), "{out}");
        assert_eq!(dir.path().join("tmp").read_dir().unwrap().count(), 1);
        // 永続化側を通しても no-op（＝ DB と LLM の本文が一致する）。
        let logged =
            sanitize_tool_result_for_log("read_file", &out, &long_session, &long_call_id, None);
        assert_eq!(logged, out);
    }

    /// LLM 経路と DB 経路は同じ本文を返す（#294 の invariant）。
    #[test]
    fn llm_and_log_bodies_agree() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = format!(r#"{{"data":"{}"}}"#, "w ".repeat(TOOL_RESULT_TOKEN_LIMIT));
        let llm = sanitize_tool_result_for_llm("read_file", &big, "sessC", "tc3", Some(dir.path()));
        let log = sanitize_tool_result_for_log("read_file", &big, "sessC", "tc3", Some(dir.path()));
        assert_eq!(llm, log);
    }
