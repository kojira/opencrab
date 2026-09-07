    use super::*;

    /// 秘密を含まない結果は**改変されない**（byte 一致）。前フィルタで parse すらしない。
    #[test]
    fn read_predicate_is_the_single_source_for_both_decisions() {
        // 「読み」の定義は 1 つ。上限（退避するか）と参照化（持ち越すか）が同じ集合を指す。
        for t in ["ws_read", "ws_list"] {
            assert!(is_read_tool(t));
            assert_eq!(inline_limit_for_tool(t), READ_TOOL_RESULT_TOKEN_LIMIT);
        }
        for t in ["execute_shell", "search_my_history", "ws_write"] {
            assert!(!is_read_tool(t));
            assert_eq!(inline_limit_for_tool(t), TOOL_RESULT_TOKEN_LIMIT);
        }
        assert_eq!(
            append_limit_for_tool("ws_read", Some(1_000)),
            1_000,
            "残り枠がツール上限より狭いときは残り枠"
        );
        assert_eq!(
            append_limit_for_tool("ws_read", Some(80_000)),
            READ_TOOL_RESULT_TOKEN_LIMIT,
            "残り枠が広いときはツール上限"
        );
        assert_eq!(append_limit_for_tool("ws_read", Some(0)), 0);
        assert_eq!(
            append_limit_for_tool("ws_read", None),
            READ_TOOL_RESULT_TOKEN_LIMIT
        );
    }

    #[test]
    fn remaining_budget_below_result_spools_stub() {
        let json = format!(r#"{{"data":"{}"}}"#, "word ".repeat(800));
        assert!(
            crate::tokens::estimate_tokens(&json) > 200,
            "前提: 本文は残り枠より大きい"
        );
        let dir = tempfile::TempDir::new().unwrap();
        let out = sanitize_tool_result_for_append(
            "ws_read",
            &json,
            "sess",
            "tc-rem",
            Some(dir.path()),
            Some(200),
        );
        assert_ne!(out, json);
        assert!(
            out.contains("Tool result withheld"),
            "残り枠不足はスタブ: {out}"
        );
        assert!(
            out.contains("start_line") && out.contains("line_count"),
            "スタブは狭めて読み直せる導線を残す: {out}"
        );
    }

    #[test]
    fn sanitize_leaves_secretless_result_byte_identical() {
        let json = r#"{"success":true,"data":{"npub":"npub1ok","note":"hello"},"error":null}"#;
        let out = sanitize_tool_result_for_log("any_tool", json, "sess", "tc-1", None);
        assert_eq!(out, json);
    }

    /// #620: `nsec` を値/キーに含む結果も**マスクされず原文のまま**流れる（キー名マスクは
    /// 撤去した）。上限未満なので byte 一致で素通りすることを固定する（オフロード判定は不変）。
    #[test]
    fn sanitize_leaves_nsec_bearing_result_unmasked_now() {
        let json = r#"{"data":{"text":"the nsec format starts with nsec1"},"error":null}"#;
        let out = sanitize_tool_result_for_log("any_tool", json, "sess", "tc-1", None);
        assert_eq!(out, json, "サイズ上限未満は原文のまま流れる");
    }

    /// 秘密を持たないツールの結果は改変されない。
    #[test]
    fn sanitize_leaves_small_results_untouched() {
        let json = r#"{"success":true,"data":{"ok":true},"error":null}"#;
        let out = sanitize_tool_result_for_log("read_file", json, "sess", "tc-1", None);
        assert_eq!(out, json);
    }

    /// 上限超過はワークスペースへ退避し、DB 本文はメタ情報だけになる。
    /// #616: 退避本文は書き込み前に整形される（stdout の無い JSON は pretty）。生データは
    /// ファイルには入るが DB 本文（notice）には 1 バイトも混ざらない。
    #[test]
    fn sanitize_offloads_large_result_to_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = format!(r#"{{"data":"{}"}}"#, "x ".repeat(TOOL_RESULT_TOKEN_LIMIT));
        let out = sanitize_tool_result_for_log("read_file", &big, "sess1", "tc9", Some(dir.path()));
        assert!(out.contains("tmp/sess1-tc9.json"), "{out}");
        assert!(
            !out.contains("x x x"),
            "生データが DB 本文に混ざっている: {out}"
        );
        let saved = std::fs::read_to_string(dir.path().join("tmp/sess1-tc9.json")).unwrap();
        // stdout の無い JSON は pretty 化される（複数行になり head/grep が効く）。中身は等価。
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&saved).unwrap(),
            serde_json::from_str::<serde_json::Value>(&big).unwrap()
        );
        assert!(saved.contains('\n'), "pretty 化されていない: {saved:.80}");
    }

    /// #568/#616: 退避ファイルが [`OFFLOAD_FILE_BYTE_LIMIT`] を超えたら**先頭だけ**保存し、
    /// 切り詰めは**文字境界**で行う（バイト境界で切ると壊れた UTF-8 になる）。末尾が改行で
    /// 終わらなければ改行を 1 つ足す（ファイルを完結した行で終える。#619 レビュー）。
    #[test]
    fn offload_truncates_over_limit_at_char_boundary() {
        let dir = tempfile::TempDir::new().unwrap();
        // 上限の 1 バイト手前に 3 バイト文字 'あ' を跨がせる。バイト境界で切ると
        // 'あ' の途中で割れて壊れた UTF-8 になるが、文字境界で切れば 'あ' の手前で止まる。
        let big = format!(
            "{}あ{}",
            "a".repeat(OFFLOAD_FILE_BYTE_LIMIT - 1),
            "b".repeat(200)
        );
        assert!(big.len() > OFFLOAD_FILE_BYTE_LIMIT);

        let saved = offload_to_workspace(&big, "txt", "sessT", "tcT", Some(dir.path())).unwrap();
        assert_eq!(saved.rel_path, "tmp/sessT-tcT.txt");
        // 'あ' の手前（文字境界）＝ LIMIT-1 バイトまで保存（足した改行は数に含めない）。
        assert_eq!(saved.saved_prefix_bytes, Some(OFFLOAD_FILE_BYTE_LIMIT - 1));

        let on_disk = std::fs::read(dir.path().join("tmp/sessT-tcT.txt")).unwrap();
        // 元本文 LIMIT-1 バイト + 完結用の改行 1 バイト。
        assert_eq!(on_disk.len(), OFFLOAD_FILE_BYTE_LIMIT);
        assert!(on_disk.len() < big.len(), "切り詰められていない");
        assert_eq!(on_disk.last(), Some(&b'\n'), "改行で終わっていない");
        // 壊れた UTF-8 になっていない（境界で切った）＝末尾の 'あ'/'b' は残らない。
        let as_str = std::str::from_utf8(&on_disk).expect("切り詰め後も妥当な UTF-8");
        assert!(
            !as_str.contains('あ') && !as_str.contains('b'),
            "上限超過分（末尾）が残っている"
        );
    }

    /// #568: 上限以下は全文保存で**1 バイトも変わらない**（no-op）。
    #[test]
    fn offload_under_limit_saves_full_unchanged() {
        let dir = tempfile::TempDir::new().unwrap();
        let content = "hello ".repeat(1000); // ~6KB、上限以下
        let saved =
            offload_to_workspace(&content, "txt", "sessU", "tcU", Some(dir.path())).unwrap();
        assert_eq!(
            saved.saved_prefix_bytes, None,
            "上限以下は切り詰めない（None）"
        );
        let on_disk = std::fs::read_to_string(dir.path().join("tmp/sessU-tcU.txt")).unwrap();
        assert_eq!(on_disk, content, "上限以下は 1 バイトも変わらない");
    }

    /// #635: UUID を含む id はハイフンをそのまま残す（`_` に潰さない）。潰すと「壊れた UUID」に
    /// 見え、モデルがパスを『直そう』として実在しないパスを渡し、退避ファイルを開けなくなる。
    /// 区切りも `-` に揃えるので、ファイル名に `_` は 1 つも現れず、通知が案内するパスと実ファイル
    /// のパスは完全一致する（通知をそのままコピーすれば開ける）。
    #[test]
    fn offload_keeps_uuid_hyphens_and_notice_path_matches_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let session_id = "web-e2e-test-bot-repro631c";
        let tool_call_id = "6f3fd055-711e-48da-8573-3bfedc778dd9";
        let big = format!(r#"{{"data":"{}"}}"#, "x ".repeat(TOOL_RESULT_TOKEN_LIMIT));
        let out = sanitize_tool_result_for_log(
            "read_file",
            &big,
            session_id,
            tool_call_id,
            Some(dir.path()),
        );

        // 検査対象は**実装が返す通知本文** out（テストが組んだ文字列ではない）。out から退避パス
        // 部分（`tmp/…json`）を取り出して調べる。通知の散文には `ws_read` / `start_line` など
        // `_` を含む語があるので、全文ではなくパス部分に絞る。
        let start = out.find("tmp/").expect("通知に退避パスが無い");
        let end =
            start + out[start..].find(".json").expect("退避パスに .json が無い") + ".json".len();
        let path_in_notice = &out[start..end];

        // (1) 実装が組んだパスに `_` が 1 つも現れない（区切りもハイフンに揃っている）。
        assert!(
            !path_in_notice.contains('_'),
            "退避パスに `_` が残っている: {path_in_notice}"
        );
        // (2) UUID が原形のまま**実装の出力に**現れる（潰れて `6f3fd055_711e_…` になっていない）。
        assert!(
            path_in_notice.contains(tool_call_id),
            "UUID が原形で残っていない: {path_in_notice}"
        );
        assert!(
            !out.contains("6f3fd055_711e"),
            "UUID をアンダースコアへ潰した形が通知に混じっている: {out}"
        );
        // 期待値は直書き。検査対象（実装の出力）と完全一致することを見る。
        let expected = format!("tmp/{session_id}-{tool_call_id}.json");
        assert_eq!(path_in_notice, expected, "通知パスが期待と違う");
        // (6) 通知が案内したパスをそのまま開ける（実ファイルが存在する）。
        assert!(
            dir.path().join(path_in_notice).exists(),
            "通知が案内したパスにファイルが無い: {path_in_notice}"
        );
    }

    /// #635: `/` や `..` を含む id でも、ワークスペース（`tmp/` 直下）の外へ出ない。`/` も `.` も
    /// 英数字でないので `-` に潰れ、パス区切りにならない＝親ディレクトリへ抜けられない。
    #[test]
    fn offload_never_escapes_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let tmp = root.join("tmp");
        for (sid, tid) in [
            ("../../etc", "6f3fd055-711e-48da-8573-3bfedc778dd9"), // `..` で親へ抜けようとする
            ("a/b/c", "tc/../../x"),                               // `/` と `..` の混在
        ] {
            let saved = offload_to_workspace("hello", "txt", sid, tid, Some(root)).unwrap();
            // rel_path は "tmp/<name>" の 2 コンポーネントのみ＝階層が増えていない。
            assert_eq!(
                std::path::Path::new(&saved.rel_path).components().count(),
                2,
                "階層が増えた（脱出の兆候）: {}",
                saved.rel_path
            );
            let full = root.join(&saved.rel_path);
            assert!(full.starts_with(&tmp), "tmp の外へ出た: {}", saved.rel_path);
            assert!(full.exists(), "実ファイルが無い: {}", saved.rel_path);
        }
    }

    /// #568/#616: notice は「全文保存」と「切り詰め保存」を区別し、どちらも元サイズ
    /// （エンベロープ由来）＋保存量（保存本文由来）の二段構え。切り詰め時は truncated を明記。
    #[test]
    fn oversized_notice_marks_truncation_vs_full() {
        // 全文保存（saved_prefix_bytes = None）: 全文を書いた旨。切り詰め表現は出ない。
        let full = OffloadResult {
            rel_path: "tmp/a.json".to_string(),
            saved_prefix_bytes: None,
        };
        // orig_bytes=42（エンベロープ）, body="original content"（16 バイト・保存本文）。
        let n_full = oversized_notice(42, "original content", Some(&full));
        assert!(
            n_full.contains("written in full to `tmp/a.json`"),
            "{n_full}"
        );
        // 規模のシグナル（42）と保存本文サイズ（16）の両方が出る。
        assert!(
            n_full.contains("was 42 bytes"),
            "規模のシグナルが無い: {n_full}"
        );
        assert!(
            n_full.contains("16 bytes, 1 lines"),
            "保存本文の数が無い: {n_full}"
        );
        // #619 レビュー: エンベロープ長は "serialized result" と名乗る（"original tool
        // output" と言うとエスケープ水増し値を「元の出力」として再報告してしまう）。
        assert!(
            n_full.contains("the serialized result was"),
            "規模の文言が serialized result でない: {n_full}"
        );
        assert!(
            !n_full.contains("original tool output"),
            "誤解を招く original tool output が残っている: {n_full}"
        );
        assert!(
            !n_full.contains("Only the first"),
            "全文保存で切り詰め表現が出ている: {n_full}"
        );
        // #624: 全文保存でも読み方のレシピ（grep -n → ws_read / head -c）が入り、パスを指す。
        assert!(
            n_full.contains("grep -n <pattern> tmp/a.json"),
            "全文保存にレシピが無い: {n_full}"
        );
        assert!(n_full.contains("ws_read"), "ws_read 導線が無い: {n_full}");
        assert!(
            n_full.contains("head -c 2000 tmp/a.json"),
            "head -c 導線が無い: {n_full}"
        );
        // #624 レビュー: 上限を守らない sed -n は誘導しない（自己ループ防止）。
        assert!(
            !n_full.contains("sed "),
            "上限を守らない sed が残っている: {n_full}"
        );

        // 切り詰め保存（saved_prefix_bytes = Some）: 元サイズ・保存量・truncated を明記。
        let trunc = OffloadResult {
            rel_path: "tmp/b.json".to_string(),
            saved_prefix_bytes: Some(123),
        };
        // orig_bytes=9999（エンベロープ）だが保存したのは body の先頭 123 バイト。
        let body = "x".repeat(9999);
        let n_trunc = oversized_notice(9999, &body, Some(&trunc));
        assert!(
            n_trunc.contains("Only the first 123 bytes"),
            "保存量が無い: {n_trunc}"
        );
        assert!(
            n_trunc.contains("was 9999 bytes"),
            "元サイズが無い: {n_trunc}"
        );
        assert!(
            n_trunc.contains("truncated"),
            "切り詰めの明記が無い: {n_trunc}"
        );
        assert!(
            n_trunc.contains("Do NOT re-run the same tool"),
            "ループ防止が無い: {n_trunc}"
        );
        // #624: 打ち切りケースにも同じ読み方レシピが入り、正しいパス（tmp/b.json）を指す。
        assert!(
            n_trunc.contains("grep -n <pattern> tmp/b.json"),
            "打ち切りにレシピが無い: {n_trunc}"
        );
        assert!(n_trunc.contains("ws_read"), "ws_read 導線が無い: {n_trunc}");
        assert!(
            n_trunc.contains("head -c 2000 tmp/b.json"),
            "head -c 導線が無い: {n_trunc}"
        );
        assert!(
            !n_trunc.contains("sed "),
            "上限を守らない sed が残っている: {n_trunc}"
        );
        // 打ち切りは「先頭だけ」であることを明示（全体像の誤読を避ける）。
        assert!(
            n_trunc.contains("only the saved prefix"),
            "先頭のみの明示が無い: {n_trunc}"
        );
    }
