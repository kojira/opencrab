
/// #709: **ツール結果の本文を次のターンへ持ち越さない**ことの回帰ガード。
#[cfg(test)]
mod result_reference_tests {
    use super::result_reference;

    /// 読みは元のファイル名がそのまま参照になる。
    #[test]
    fn read_results_leave_only_a_reference() {
        let body = "秘密の設計メモ本文".repeat(50);
        let result = serde_json::json!({
            "success": true,
            "data": {
                "path": "docs/design.md",
                "content": body,
                "start_line": 1,
                "estimated_tokens": 18_000,
                "has_more": true,
            }
        })
        .to_string();

        let r = result_reference("ws_read", &result);
        assert!(
            !r.contains("秘密の設計メモ本文"),
            "本文が会話へ載っている: {r}"
        );
        assert!(r.contains("docs/design.md"), "ファイル名が無い: {r}");
        assert!(r.contains("18000"), "規模が無い: {r}");
        assert!(r.contains("続きあり"), "続きの有無が無い: {r}");
    }

    /// くらぶ暴走の根因修正: **コマンド実行の stdout 本文を会話へ残す**（#709 の畳みを execute_shell
    /// 系だけ撤回する）。
    ///
    /// #709 は shell 出力も参照へ畳んでいたが、`execute_shell` は常に背景 subtask 化され（#152/#671）
    /// 同ターン往復が無いため、畳むと stdout をどのターンでも読めず、モデルが出力を取り直そうと待機
    /// 宣言を連投した（本番実機で確認）。会話へ来る前に offload しきい値超過は退避 notice（resolve
    /// ハンドル付き）へ差し替わる（#551）ので、`result_reference` へ JSON で来る shell 出力は会話に
    /// 収まる小出力＝丸ごと残して安全。ここでは代表的な小 stdout が verbatim で残ることを固定する。
    #[test]
    fn shell_results_keep_their_stdout_body() {
        let out = "晴れ 28度 くもり所により雨";
        let result = serde_json::json!({
            "success": true,
            "data": {"exit_code": 0, "stdout": out, "stderr": ""}
        })
        .to_string();

        let r = result_reference("execute_shell", &result);
        assert!(
            r.contains(out),
            "コマンド出力が会話から消えた（畳まれた＝くらぶ暴走の根因が残っている）: {r}"
        );
        assert!(r.contains("終了コード 0"), "終了コードが無い: {r}");
    }

    /// #709 レビュー指摘: **コマンドの非ゼロ終了は stderr を本文で残す**。
    ///
    /// `execute_shell` は非ゼロ終了でもツール層では `success: true` を返すので、`success` 判定
    /// では捕まらない。ここを塞がないとコンパイルエラーやテスト失敗の理由がターンをまたぐと
    /// 消える——エージェントが最も頻繁に読むものであり、握り潰しになる。
    #[test]
    fn failed_commands_keep_their_stderr() {
        let result = serde_json::json!({
            "success": true,
            "data": {
                "exit_code": 1,
                "stdout": "",
                "stderr": "error[E0308]: mismatched types\n  --> src/main.rs:42:9"
            }
        })
        .to_string();

        let r = result_reference("execute_shell", &result);
        assert!(
            r.contains("E0308") && r.contains("src/main.rs:42"),
            "失敗の理由（stderr）が消えている: {r}"
        );
        assert!(
            r.contains("exit_code") || r.contains("1"),
            "失敗だと分からない: {r}"
        );
    }

    /// #709 レビュー 2 巡目: **失敗詳細が stdout に出るケース**（cargo test / pytest / jest）でも
    /// 本文が残る。stderr だけを残す形では `cargo build` しか塞げていなかった。
    #[test]
    fn failed_commands_keep_stdout_details_too() {
        let result = serde_json::json!({
            "success": true,
            "data": {
                "exit_code": 101,
                "stdout": "thread 'tests::budget' panicked at src/lib.rs:88:\nassertion failed: used <= budget",
                "stderr": "error: test failed, to rerun pass `-p opencrab-core --lib`"
            }
        })
        .to_string();

        let r = result_reference("execute_shell", &result);
        assert!(
            r.contains("assertion failed") && r.contains("tests::budget"),
            "stdout に出た失敗詳細が消えている: {r}"
        );
    }

    /// #709 レビュー指摘: 非冪等なツールに「もう一度呼ぶ」と言わない。
    ///
    /// `generate_inner_voice` を呼び直すと**別の思考**が生成され、過去のそれは回収できない。
    /// 回収できないものを回収できることにする誘導は、失敗を成功に見せるのと同じ質の嘘になる。
    #[test]
    fn non_idempotent_tools_do_not_promise_recovery() {
        let result = serde_json::json!({
            "success": true, "data": {"voice": "思考の断片".repeat(200)}
        })
        .to_string();

        let r = result_reference("generate_inner_voice", &result);
        assert!(
            !r.contains("思考の断片思考の断片"),
            "本文が載っている: {r:.80}"
        );
        assert!(
            !r.contains("もう一度"),
            "回収できないのに再取得を約束している: {r}"
        );
    }

    /// 失敗した結果は本文を残す（参照へ潰すと「成功した」ことに化ける）。
    #[test]
    fn failed_results_keep_their_error() {
        let failed = serde_json::json!({
            "success": false, "data": null, "error": "path not found: docs/missing.md"
        })
        .to_string();

        let r = result_reference("ws_read", &failed);
        assert!(r.contains("path not found"), "失敗の理由が消えている: {r}");
        assert!(!r.contains("読んだ"), "読めていないのに読んだことに: {r}");
    }

    /// 一覧は件数と正しい再取得ツールを出す。
    #[test]
    fn list_reference_points_at_the_right_tool() {
        let listed = serde_json::json!({
            "success": true, "data": {"path": "src", "entries": ["a.rs","b.rs","c.rs"]}
        })
        .to_string();

        let r = result_reference("ws_list", &listed);
        assert!(r.contains("3 件"), "件数が無い: {r}");
        assert!(r.contains("ws_list"), "誤ったツールへ誘導: {r}");
    }

    /// その他のツール（記憶・検索など）も規模だけ残す。
    #[test]
    fn other_tools_leave_size_only() {
        let big = serde_json::json!({
            "success": true, "data": {"hits": vec!["長い検索結果".repeat(500)]}
        })
        .to_string();

        let r = result_reference("search_my_history", &big);
        assert!(
            !r.contains("長い検索結果長い検索結果"),
            "本文が載っている: {r:.80}"
        );
        assert!(r.contains("文字"), "規模が分からない: {r}");
    }

    /// #709 レビュー指摘1: 小さな mutation 結果を参照化しても、書いた対象（path）を会話から
    /// 消さない。`ws_write` の `{"path":"...","written":true}` が「結果 N 文字」に化けると
    /// **どのファイルを書いたのかが会話から消える**——削減効果ゼロなのに作業記憶を削っていた。
    #[test]
    fn mutation_results_keep_their_path() {
        let result = serde_json::json!({
            "success": true,
            "data": {"path": "crates/core/src/lib.rs", "written": true}
        })
        .to_string();

        let r = result_reference("ws_write", &result);
        assert!(
            r.contains("crates/core/src/lib.rs"),
            "書いたファイルが会話から消えた: {r}"
        );
        assert!(!r.contains("\"written\""), "本文がそのまま載っている: {r}");
    }

    /// #709 レビュー指摘1: 参照が本文より長くなるなら潰さず本文を残す（会話を軽くする仕組みが
    /// 会話を重くしない）。極小の結果は参照化の固定オーバーヘッドの方が長くなる。
    #[test]
    fn tiny_results_are_never_expanded() {
        // 参照文（path + tool_name + 定型句）の方が本文より長くなる極小ケース。
        let result = serde_json::json!({
            "success": true, "data": {"path": "x"}
        })
        .to_string();

        let r = result_reference("configure_self", &result);
        assert!(
            r.chars().count() <= result.chars().count(),
            "参照が本文より長い（会話を重くしている）: ref={} body={} / {r}",
            r.chars().count(),
            result.chars().count()
        );
    }

    /// #709 レビュー指摘2: 失敗は必ず本文ごと残る——catch-all の「結果 N 文字」へ潰れて黙って
    /// 消えることはない。この系の不変条件（失敗は `success:false` **または** `exit_code!=0`）を
    /// `signals_failure` に集約したので、どちらの経路を落としてもこのテストが落ちる。
    #[test]
    fn failures_are_never_summarized_as_success() {
        // (a) ツール層の失敗: success:false。
        let tool_fail = serde_json::json!({
            "success": false, "data": {"foo": "bar"}, "error": "boom"
        })
        .to_string();
        assert_eq!(
            result_reference("some_tool", &tool_fail),
            tool_fail,
            "success:false が要約されて消えた"
        );

        // (b) コマンドの非ゼロ終了: execute_shell は success:true のまま返す。
        let cmd_fail = serde_json::json!({
            "success": true, "data": {"exit_code": 2, "stdout": "", "stderr": "boom"}
        })
        .to_string();
        assert_eq!(
            result_reference("execute_shell", &cmd_fail),
            cmd_fail,
            "非ゼロ終了が要約されて消えた"
        );
    }
}
