
    /// #624: 上限超過の通知（全文保存）に**具体的な読み方レシピ**が入る。`grep -n` で行番号 →
    /// `ws_read`、または `ws_read` を持たない caller 向けに `head -c`。読む手段が無い caller 向け
    /// の再実行導線も残る。#624 レビュー: 上限を守らない `sed -n` は誘導しない（自己ループ防止）。
    #[test]
    fn oversized_notice_carries_read_recipe() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = "row value\n".repeat(TOOL_RESULT_TOKEN_LIMIT); // 非 JSON・上限超 → .txt 全文保存
        let out = sanitize_tool_result_for_llm("execute_shell", &big, "sR", "tR", Some(dir.path()));

        // レシピの具体操作がパス入りで出る。
        assert!(
            out.contains("grep -n <pattern> tmp/sR-tR.txt"),
            "grep 導線が無い: {out}"
        );
        assert!(out.contains("ws_read"), "ws_read 導線が無い: {out}");
        assert!(out.contains("start_line"), "start_line が無い: {out}");
        // #624 レビュー: バイト頭打ちの head -c だけ（sed -n は落とした）。
        assert!(
            out.contains("head -c 2000 tmp/sR-tR.txt"),
            "head -c 導線が無い: {out}"
        );
        assert!(
            !out.contains("sed "),
            "上限を守らない sed が残っている: {out}"
        );
        // 読む手段が無い caller 向けの再実行導線は残す。
        assert!(
            out.contains("If you cannot read that file"),
            "非リーダー向け導線が無い: {out}"
        );
        assert!(
            out.contains("Do NOT re-run the same tool"),
            "ループ防止が無い: {out}"
        );
    }

    /// #856 発見3・作業1.2: **`head -c 2000` の bound**。回収レシピの非 `ws_read` 導線は
    /// `head -c 2000 <path>` で、その出力（≤ 2,000 バイト）を `execute_shell` 結果として
    /// もう一度 sanitize に通しても**再 offload されず inline に残る**（＝読み戻しがループ
    /// しない）ことを固定する。構造保証: トークン数 ≤ バイト数 ≤ 2,000 + 封筒数十バイト <
    /// [`TOOL_RESULT_TOKEN_LIMIT`]（2,500）。逆 revert（バイト cap を 2,000 → 上限超へ）で FAIL。
    #[test]
    fn head_c_2000_output_stays_inline_no_reoffload() {
        // head -c 2000 相当: ちょうど 2,000 バイトの stdout（最悪＝バイト cap ぴったり）。
        let stdout = "x".repeat(2_000);
        let env = serde_json::json!({
            "success": true,
            "data": { "stdout": stdout, "stderr": "", "exit_code": 0 }
        })
        .to_string();
        // 封筒込みでも上限バイト未満であることを明示（tokens ≤ bytes < 2,500）。
        assert!(
            env.len() < TOOL_RESULT_TOKEN_LIMIT,
            "head -c 2000 の封筒が上限バイトを超える（前提崩れ）: {} bytes",
            env.len()
        );
        let dir = tempfile::TempDir::new().unwrap();
        let out =
            sanitize_tool_result_for_llm("execute_shell", &env, "sHC", "tHC", Some(dir.path()));
        // inline に残る＝ sanitize は本文をそのまま返し、退避もしない。
        assert_eq!(
            out, env,
            "head -c 2000 の出力が再 offload された（inline に残らない）"
        );
        assert!(
            !out.contains("Tool result withheld"),
            "head -c 2000 の出力に offload notice が出た＝再 offload: {out:.200}"
        );
        assert!(
            !dir.path().join("tmp").exists(),
            "head -c 2000 の小出力で退避ファイルが作られた（不要な offload）"
        );
    }

    /// #856 発見3・作業2: **grep スパイラルが回復可能（無限ループでない）**の核レベル固定。
    /// レシピ最初の `grep -n <pattern>` は `execute_shell` 経由で 2,500 上限にかかり、広い
    /// パターンだと grep 出力自体が**再 offload**される（#856 発見3）。その再 offload が
    /// **必ず新しい読める退避ハンドル（tmp パス＋ `ws_read` レシピ）を伴う**＝次の `ws_read`
    /// で読めることを固定する。ハンドルの無い墓標を作らない限り再帰は必ず終端する（実際の
    /// 読み戻しが inline に収まることは actions 側の loop-closed テストで実証）。
    #[test]
    fn reoffloaded_grep_output_always_carries_readable_handle() {
        let dir = tempfile::TempDir::new().unwrap();
        // 広い grep がマッチを大量に返した想定の大出力（>2,500 tok・行のある生テキスト）。
        let grep_out = "src/foo.rs:42:    let x = compute();\n".repeat(TOOL_RESULT_TOKEN_LIMIT);
        let env = serde_json::json!({
            "success": true,
            "data": { "stdout": grep_out, "stderr": "", "exit_code": 0 }
        })
        .to_string();
        let notice =
            sanitize_tool_result_for_llm("execute_shell", &env, "sGREP", "tGREP", Some(dir.path()));
        // 再 offload された（notice へ化けた）。
        assert!(
            notice.contains("Tool result withheld"),
            "広い grep の大出力が offload されない（前提崩れ）: {notice:.200}"
        );
        // だが墓標ではない: 新しい退避ファイル（実在）＋ ws_read 回収レシピが必ず付く。
        assert!(
            dir.path().join("tmp/sGREP-tGREP.txt").exists(),
            "再 offload で読める退避ファイルが作られていない: {notice}"
        );
        assert!(
            notice.contains("tmp/sGREP-tGREP.txt") && notice.contains("ws_read"),
            "再 offload の notice に読める handle（tmp パス＋ws_read）が無い＝墓標: {notice}"
        );
    }

    /// #856 発見3・作業2(a): レシピは **`ws_read` 直読みを先頭に置き、`grep -n` を後置**する。
    /// これで退避ファイルを読むだけの一般ケースが grep スパイラルを踏まない。逆 revert
    /// （grep 先行へ戻す）でこのピンは FAIL する。
    #[test]
    fn recipe_leads_with_ws_read_before_grep() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = "row value\n".repeat(TOOL_RESULT_TOKEN_LIMIT);
        let out =
            sanitize_tool_result_for_llm("execute_shell", &big, "sRO", "tRO", Some(dir.path()));
        let ws_at = out.find("ws_read").expect("ws_read 導線が無い");
        let grep_at = out.find("grep -n <pattern>").expect("grep 導線が無い");
        assert!(
            ws_at < grep_at,
            "ws_read 直読みが grep より先に来ていない（grep 先行のまま＝スパイラルを踏む）: {out}"
        );
    }
