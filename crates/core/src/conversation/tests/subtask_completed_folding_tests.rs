
/// #713: `subtask_completed` の入れ子 `result`（ツール実行の本文）を会話へ持ち越さない。
///
/// opencrab は「ツールを常に切り離す」ため、実運用ではツール結果の主経路がここ（`tool_result`
/// ではなく完了本文の入れ子 `result`）。全テストは公開入口 `format_single_log` を通して実際の
/// `system` アーム分岐を叩く（変異検知の best altitude）。
#[cfg(test)]
mod subtask_completed_folding_tests {
    use super::format_single_log;
    use opencrab_db::queries::SessionLogRow;

    /// `settle_completed` が書く完了本文と同一形の system ログを作る。`result` は**文字列**
    /// （`result_text` を JSON 文字列として載せる）。`speaker_id=None`（scaffolding と区別）。
    fn subtask_completed_log(exit_reason: &str, result_str: &str) -> SessionLogRow {
        let content = serde_json::json!({
            "type": "subtask_completed",
            "subtask_id": "st-1",
            "session_id": "subtask-st-1",
            "exit_reason": exit_reason,
            "result": result_str,
        })
        .to_string();
        SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "parent".to_string(),
            log_type: "system".to_string(),
            content,
            speaker_id: None,
            turn_number: None,
            metadata_json: None,
            created_at: None,
        }
    }

    /// 1. 単一ツール成功（execute_shell）→ **stdout 本文が resume 会話へ残る**（くらぶ暴走の根因修正）。
    ///    切り離した subtask は取り直せないので、畳むとモデルが stdout をどのターンでも読めない。監査の
    ///    相関（起動応答との突き合わせ）は封筒（`[s{n} 完了]` ヘッダ）が残し、生 UUID は出さない。
    ///
    ///    ここは代表的な小 stdout で固定する。offload しきい値超過の大出力は会話へ来る前に退避 notice
    ///    （resolve ハンドル付き）へ差し替わり（#551・下の `large_shell_output_folds_with_resolve_handle`）
    ///    この経路には JSON で来ないので、ここへ来るのは会話に収まる小出力＝丸ごと残す。
    #[test]
    fn single_tool_success_keeps_stdout_in_conversation() {
        let out = "晴れ 28度 くもり所により雨";
        let result = serde_json::json!({
            "success": true, "data": {"exit_code": 0, "stdout": out, "stderr": ""}
        })
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &result));
        assert!(
            o.contains(out),
            "stdout 本文が resume 会話に無い（畳まれた＝くらぶ暴走の根因が残っている）: {o}"
        );
        assert!(o.contains("終了コード 0"), "終了コードが無い: {o}");
        // 非冪等ツールに「もう一度実行して見る」と連想させない（副作用ループの引き金・欠陥3）。
        assert!(
            !o.contains("もう一度"),
            "非冪等 subtask に再取得を約束: {o}"
        );
        // row295b: 生 UUID（subtask_id/session）は会話に出さない。在り処はヘッダの s 番号
        // （refs 無しの単体表示では "subtask"）が示す。
        assert!(o.contains("[subtask 完了]"), "完了ヘッダが無い: {o}");
        assert!(!o.contains("st-1"), "生 subtask_id が残存: {o}");
        assert!(!o.contains("session="), "生 session UUID が残存: {o}");
    }

    /// 1b. 成功時の stderr は本文を inline せず規模だけ添える（判断の主材料は stdout・嵩防止）。
    ///     stdout が空でも stderr の規模は出す（stdout だけ数えて事実と違う表示にしない・#716 指摘1）。
    ///     失敗（非ゼロ終了）時は上流で stderr 本文ごと残る（別テスト）。
    #[test]
    fn single_tool_success_stderr_shows_size_not_body() {
        let err = "warning: unused variable `x`\n".repeat(2_000);
        let result = serde_json::json!({
            "success": true,
            "data": {"exit_code": 0, "stdout": "", "stderr": err}
        })
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &result));
        assert!(
            !o.contains("warning: unused variable"),
            "成功時の stderr 本文が会話へ載っている（嵩む）: {o:.120}"
        );
        assert!(o.contains("終了コード 0"), "終了コードが無い: {o}");
        assert!(
            o.contains(&format!("stderr {} 文字", err.chars().count())),
            "stderr の規模が数えられていない: {o}"
        );
    }

    /// 1c. **大出力は畳んでも取得ハンドル付き**（欠陥3・回収不能な墓標を作らない）。
    ///
    /// offload しきい値（`inline_limit_for_tool`）超過の shell 出力は、会話へ届く前に
    /// `sanitize_tool_result_for_log` が workspace へ退避し、本文を**非 JSON の退避 notice**——
    /// `grep -n`/`ws_read`/`head -c` の回収レシピ（＝実体のある resolve ハンドル）付き——へ差し替える。
    /// その notice は `fold_subtask_completed` の非 JSON 分岐が素通しするので、resume 会話には
    /// 「全文は記録に残る」だけの墓標ではなく**回収レシピと退避先パス**が現れ、退避ファイルには
    /// 全文が入っている（＝モデルが実際に全文を回収できる）。「同じ引数で再実行するな」も明示され、
    /// 非冪等ツールの再取得ループを誘発しない。
    #[test]
    fn large_shell_output_folds_with_resolve_handle() {
        // offload しきい値（2,500 tok）を確実に超える大 stdout。回収検証用の marker を仕込む。
        let out = "SHELL-BIG-MARKER 大きなコマンド出力の行\n".repeat(3_000);
        let raw = serde_json::json!({
            "success": true, "data": {"exit_code": 0, "stdout": out, "stderr": ""}
        })
        .to_string();

        // 上流（settle_completed 相当）: workspace を与えて退避させ、完了本文を得る。
        let ws = tempfile::tempdir().unwrap();
        let sanitized = crate::tool_result_log::sanitize_tool_result_for_log(
            "subtask_completed",
            &raw,
            "sess-big",
            "call-big",
            Some(ws.path()),
        );
        // 退避されている（=非 JSON notice に化けた）ことを確認。
        assert!(
            serde_json::from_str::<serde_json::Value>(&sanitized).is_err(),
            "大出力が退避されず JSON のまま（しきい値未達＝テスト前提が崩れた）: {sanitized:.200}"
        );

        let o = format_single_log(&subtask_completed_log("completed", &sanitized));

        // (a) resolve ハンドル（回収レシピ）が会話に現れる——墓標ではない。
        assert!(
            o.contains("ws_read") || o.contains("head -c"),
            "退避参照に resolve ハンドル（回収レシピ）が無い（墓標）: {o:.400}"
        );
        // (b) 非冪等ツールの再取得ループを誘発しない（「同じ引数で再実行するな」）。
        assert!(
            o.contains("Do NOT re-run the same tool with the same arguments"),
            "再実行を戒める文言が無い（副作用ループの引き金）: {o:.400}"
        );
        // (c) その resolve で全文が取れる: 退避ファイルに全文（marker）が入っている。
        let tmp = ws.path().join("tmp");
        let saved: Vec<_> = std::fs::read_dir(&tmp)
            .expect("退避先 tmp/ が作られていない")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(
            saved.len(),
            1,
            "退避ファイルがちょうど 1 つでない: {saved:?}"
        );
        let body = std::fs::read_to_string(saved[0].path()).unwrap();
        assert!(
            body.contains("SHELL-BIG-MARKER"),
            "退避ファイルに全文が入っていない（resolve しても回収できない）"
        );
    }

    /// 2. 単一ツール失敗（execute_shell は success:true のまま exit_code!=0）→ 本文を丸ごと残す。
    ///    stderr / stdout を選り分けず両方残す（罠2）。
    #[test]
    fn single_tool_failure_keeps_the_whole_body() {
        let result = serde_json::json!({
            "success": true,
            "data": {
                "exit_code": 1,
                "stdout": "panicked at src/lib.rs:88",
                "stderr": "error[E0308]: mismatched types"
            }
        })
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &result));
        assert!(o.contains("E0308"), "stderr の失敗理由が消えた: {o}");
        assert!(
            o.contains("src/lib.rs:88"),
            "stdout の失敗詳細が消えた: {o}"
        );
    }

    /// 3. `exit_reason=="completed"` でも内側 `exit_code!=0` なら本文保持（外側 completed に騙されない・罠1）。
    #[test]
    fn completed_outer_does_not_mask_inner_nonzero_exit() {
        let stdout = "assertion failed: used <= budget ".repeat(2_000);
        let result = serde_json::json!({
            "success": true,
            "data": {"exit_code": 101, "stdout": stdout, "stderr": "test failed"}
        })
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &result));
        assert!(
            o.contains("assertion failed: used <= budget"),
            "外側 completed で内側の失敗詳細が消えた: {o:.120}"
        );
    }

    /// 4. `exit_reason ∈ {timeout,error,stopped_by_limit}` → 畳めるはずの本文でも丸ごと残す（罠1）。
    #[test]
    fn non_completed_exit_reasons_keep_the_body() {
        let out = "x".repeat(50_000);
        // completed なら畳まれる成功ツール結果。exit_reason だけで保持へ倒れることを見る。
        let result = serde_json::json!({
            "success": true, "data": {"exit_code": 0, "stdout": out, "stderr": ""}
        })
        .to_string();

        for er in ["timeout", "error", "stopped_by_limit"] {
            let o = format_single_log(&subtask_completed_log(er, &result));
            assert!(
                o.contains(&"x".repeat(100)),
                "exit_reason={er} で本文が畳まれた（completed 以外は保持のはず）: {o:.120}"
            );
        }
    }

    /// 5. 生産者A（サブエージェント最終応答・非 JSON の散文）→ そのまま残る（要約で消さない・決定B）。
    #[test]
    fn producer_a_prose_is_left_intact() {
        let prose = "サブエージェントが出した最終応答の本文。".repeat(50);
        let o = format_single_log(&subtask_completed_log("completed", &prose));
        assert!(
            o.contains("サブエージェントが出した最終応答の本文。"),
            "散文の答えが消えた（決定B に反する）: {o:.120}"
        );
        assert!(
            !o.contains("本文は会話に残していない"),
            "散文を参照へ潰した: {o:.120}"
        );
    }

    /// 5b. 生産者A がたまたま JSON オブジェクトを返しても（`success` 封筒が無い）畳まない（決定B・fail-safe）。
    #[test]
    fn producer_a_bare_json_object_is_not_folded() {
        let answer = serde_json::json!({
            "answer": "これはサブエージェントの答え".repeat(100), "confidence": "high"
        })
        .to_string();
        let o = format_single_log(&subtask_completed_log("completed", &answer));
        assert!(
            o.contains("これはサブエージェントの答え"),
            "ツール封筒でない JSON を畳んで答えを消した: {o:.120}"
        );
    }

    /// 6. 退避 notice（非 JSON・読み方レシピ入り）→ そのまま素通し（レシピが壊れない・罠4）。
    #[test]
    fn offload_notice_passes_through_untouched() {
        let notice =
            "結果が大きいため退避しました: workspace/offload/abc.txt（全 1234 行）。読み方: ws_read で行範囲を指定して読む。";
        let o = format_single_log(&subtask_completed_log("completed", notice));
        assert!(
            o.contains("workspace/offload/abc.txt") && o.contains("ws_read で行範囲"),
            "退避 notice の読み方レシピが壊れた: {o}"
        );
    }

    /// 7. batch 配列で 1 要素でも失敗 → 配列全体を保持（罠2）。
    #[test]
    fn batch_with_one_failure_keeps_the_whole_array() {
        let arr = serde_json::json!([
            {"tool":"execute_shell","tool_call_id":"c1",
             "result":{"success":true,"data":{"exit_code":0,"stdout":"ok"}}},
            {"tool":"ws_read","tool_call_id":"c2",
             "result":{"success":false,"data":null,"error":"path not found: docs/missing.md"}}
        ])
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &arr));
        assert!(
            o.contains("path not found: docs/missing.md"),
            "batch の失敗要素が消えた: {o:.160}"
        );
    }

    /// 8. batch 全成功 → **各要素の本文を残す**（#880: 要素ごとに exit_code/payload 判定）。
    ///    shell 要素は stdout を素の形で、exit_code 無しの要素（ws_write 等）は payload をそのまま。
    ///    畳むと切り離した batch subtask の結果を resume がどのターンでも読めず再 dispatch の燃料に
    ///    なる（症状B）。会話へ届く前に結合本文の上限超過は退避 notice へ差し替わる（#551/#878）ので
    ///    JSON で来るのは会話に収まる小 batch＝丸ごと残して有界。
    #[test]
    fn batch_all_success_keeps_element_bodies() {
        let arr = serde_json::json!([
            {"tool":"execute_shell","tool_call_id":"c1",
             "result":{"success":true,"data":{"exit_code":0,"stdout":"BATCH-SHELL-OUT 晴れ","stderr":""}}},
            {"tool":"ws_write","tool_call_id":"c2",
             "result":{"success":true,"data":{"path":"BATCH-WROTE/notes.md","written":true}}}
        ])
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &arr));
        // shell 要素: stdout が素の形で残る。
        assert!(
            o.contains("BATCH-SHELL-OUT 晴れ"),
            "batch の shell stdout が畳まれた（再 dispatch 燃料が残る）: {o:.200}"
        );
        // exit_code 無しの要素: payload（path）が残る。
        assert!(
            o.contains("BATCH-WROTE/notes.md"),
            "batch の exit_code 無し要素の本文が畳まれた: {o:.200}"
        );
        assert!(o.contains("2 件"), "件数が無い: {o}");
        assert!(o.contains("[subtask 完了]"), "完了ヘッダが無い: {o}");
        assert!(!o.contains("session="), "生 session UUID が残存: {o}");
        assert!(!o.contains("st-1"), "生 subtask_id が残存: {o}");
    }

    /// 8b. **exit_code 無しの単一 dispatch ツール結果も本文を resume 会話へ残す**（#880 の核）。
    ///
    /// #877 は exit_code を持つ結果（execute_shell）だけ畳みを撤回したが、`ws_write` /
    /// `learn_from_experience` / `summarize_and_save` 等の exit_code 無し dispatch ツールは
    /// 「結果 N 文字」へ畳まれ、切り離した subtask の結果を resume がどのターンでも読めなかった
    /// （症状B の連投燃料）。ここでは代表的な小 payload（ws_write の path）が resume 会話に残ること、
    /// および畳みの墓標文言（「本文は会話に残していない」）が付かないことを固定する。
    #[test]
    fn single_tool_no_exit_code_keeps_its_payload() {
        let result = serde_json::json!({
            "success": true,
            "data": {"path": "WROTE-MARKER/design.md", "written": true}
        })
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &result));
        assert!(
            o.contains("WROTE-MARKER/design.md"),
            "exit_code 無し dispatch ツールの本文が resume 会話に無い（畳まれた＝症状B の燃料）: {o}"
        );
        assert!(
            !o.contains("本文は会話に残していない"),
            "本文を残すのに墓標文言が付いている: {o}"
        );
        assert!(o.contains("[subtask 完了]"), "完了ヘッダが無い: {o}");
        assert!(!o.contains("session="), "生 session UUID が残存: {o}");
        assert!(!o.contains("st-1"), "生 subtask_id が残存: {o}");
    }

    /// 9. 参照が本文以上に長くなる極小結果 → 本文を残す（長さ不変条件・#709 と共有）。
    #[test]
    fn tiny_results_are_never_expanded() {
        // 参照より短い極小結果は本文を残す（長さ不変条件・#709 と共有）。参照文言が短くなった
        // （row295b で生 UUID/接頭辞を落とした）ぶん閾値は下がるが、参照が本文以上なら本文を残す
        // 不変条件は不変——空 data で確認する。
        let result = serde_json::json!({"success": true, "data": {}}).to_string();
        let o = format_single_log(&subtask_completed_log("completed", &result));
        assert!(
            !o.contains("本文は会話に残していない"),
            "極小結果を参照へ潰した（長さ不変条件が効いていない）: {o}"
        );
    }

    /// 10. 参照に再取得誘導（「もう一度…読む」）を**含まない**（非冪等・罠3）。
    #[test]
    fn reference_never_promises_refetch() {
        let out = "x".repeat(50_000);
        let result = serde_json::json!({
            "success": true, "data": {"exit_code": 0, "stdout": out}
        })
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &result));
        assert!(
            !o.contains("もう一度"),
            "回収できない subtask に再取得を約束している: {o}"
        );
    }

    /// 11a. **構造テスト: top-level の未知 log_type が catch-all で全文を運ぶことの固定**。
    ///
    /// `format_single_log` の catch-all は log_type を問わず `content` を**丸ごと**会話へ運ぶ。
    /// 本文を畳む（参照化する）経路を持つのは現在 2 つだけ——`tool_result`（result_reference）と
    /// `system` + `type=="subtask_completed"`（format_subtask_completed → fold_subtask_completed）。
    /// ここでは top-level の未知 log_type が catch-all で全文を運ぶことを固定する。
    ///
    /// **このテストが守る向き・守らない向き（#716 レビュー 2 巡目・正確に）**:
    /// - **守る**: 許可集合 `FOLDS_BODY_TOP_LEVEL` を更新せずに、census 内の type を**畳む枝を足した**とき
    ///   赤くなる（残骸を減らす安全な方向の変更を「意図的に決めた」証跡として要求する）。
    /// - **守らない**: 新しい log_type が**畳まれずに追加され、30 万文字を運び始める**ケース——**これは
    ///   #713 の再発そのもの**（`subtask_completed` は誰も畳まず 324,176 文字を黙って積み上げた）だが、
    ///   このテストは通ってしまう。catch-all は元から全文を運ぶのが仕様なので、新カリアが catch-all を
    ///   通っても assert は緑のまま。**「新しい運び手はこのテストが検知する」と誤解しないこと。**
    /// - **危険な向き（畳まれない新カリアの追加）を機械的に捕まえるには**、生産者側（`log_type` /
    ///   `type` を書く箇所）を型で持って分類を強制する必要があり、**それは別スコープ**（別 issue）。
    ///   守られていないのに守られていると信じるのが最悪なので、ここに非対称性を明記する。
    #[test]
    fn unknown_log_types_carry_full_body_through_catch_all() {
        // 本文を畳む経路を持つ log_type（top-level）はこれだけ。system は subtype で分岐するので
        // 11b で別に census する。ここを増やすときは畳み経路を意識的に足したことの証跡になる。
        const FOLDS_BODY_TOP_LEVEL: &[&str] = &["tool_result"];

        let body = "運ばれてはいけない生本文".repeat(30);
        let raw_log = |log_type: &str| SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "s".to_string(),
            log_type: log_type.to_string(),
            content: body.clone(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };

        // 将来 type を含む代表 census。畳み集合に無い type は catch-all で全文を運ぶ。
        for lt in [
            "evaluation_note",
            "reflection",
            "brand_new_type_2027",
            "some_future_log_kind",
        ] {
            assert!(
                !FOLDS_BODY_TOP_LEVEL.contains(&lt),
                "census が畳み集合と衝突している（テストの前提が壊れた）: {lt}"
            );
            let out = format_single_log(&raw_log(lt));
            assert!(
                out.contains(&body),
                "catch-all が log_type={lt} の本文を落としている。畳み経路を足したなら \
                 FOLDS_BODY_TOP_LEVEL とこのテストを更新し「畳むか・運ぶか」を明示的に決めること: {out:.80}"
            );
        }
    }

    /// 11b. **構造テスト: `system` の未知サブタイプが全文を運ぶことの固定（#716 レビュー指摘1）**。
    ///
    /// #713 自身が「`system` の 1 サブタイプ（`type=="subtask_completed"`）」として現れたとおり、
    /// **次の運び手も最も自然には `system` + 新しい `type`**（切り離しツールの別カリア等）として来る。
    /// その形は 11a の top-level census を素通りし、`system` アームの `if kind == "subtask_completed"`
    /// も素通りして `to_string_pretty` の丸ごと pretty-print に落ちて**本文を会話へ運ぶ**。11a だけでは
    /// **#713 が生きている次元（system サブタイプ）そのものを見ていなかった**ので、その次元を固定する。
    ///
    /// **このテストが守る向き・守らない向き（#716 レビュー 2 巡目・正確に）**:
    /// - **守る**: 許可集合 `FOLDS_BODY_SYSTEM_SUBTYPES` を更新せずに、census 内のサブタイプを**畳む枝を
    ///   足した**とき赤くなる（system アームに fold 枝を足す＝本文を減らす方向の変更を「意図的に決めた」
    ///   証跡として要求する）。
    /// - **守らない**: 新しい system サブタイプが**畳まれずに追加され、30 万文字を運び始める**ケース——
    ///   **これは #713 の再発そのもの**だが、このテストは通ってしまう。system アームは未知 type を元から
    ///   pretty-print で全文運ぶのが現状の仕様なので、新カリアがそこを通っても assert は緑のまま。
    ///   統括指示の目的「新しい type を足した人が本文の持ち越しに気づかず素通しできてしまう状態を無くす」は
    ///   **この census では達成できていない**（＝ fold を足す向きだけを縛り、運び手を足す向きは縛れない）。
    /// - **危険な向き（畳まれない新カリアの追加）を機械的に捕まえるには**、生産者側（`log_type='system'` を
    ///   書く箇所）を型で持って分類を強制する必要があり、**それは別スコープ**（別 issue）。守られていない
    ///   のに守られていると信じるのが最悪なので、この非対称性をここに明記する。
    #[test]
    fn unknown_system_subtypes_carry_full_body() {
        // 本文を畳む（参照化する）system サブタイプはこれだけ。ここを増やす＝新しい切り離し系
        // カリアを畳むと決めたことの証跡。増やさずに畳む枝を足すと下の census が赤くなる。
        const FOLDS_BODY_SYSTEM_SUBTYPES: &[&str] = &["subtask_completed"];

        let body = "運ばれてはいけない生本文".repeat(30);
        // system ログは content に `type` を持つ JSON。畳まれなければ pretty-print が本文（note）を運ぶ。
        let system_log = |subtype: &str| SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "s".to_string(),
            log_type: "system".to_string(),
            content: serde_json::json!({ "type": subtype, "note": body }).to_string(),
            speaker_id: None,
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };

        // 将来のカリアを含む代表 census。許可集合に無いサブタイプは全文を運ぶ。
        for subtype in [
            "reflection_note",
            "handoff",
            "new_tool_carrier_2027",
            "some_future_system_kind",
        ] {
            assert!(
                !FOLDS_BODY_SYSTEM_SUBTYPES.contains(&subtype),
                "census が畳み集合と衝突している（テストの前提が壊れた）: {subtype}"
            );
            let out = format_single_log(&system_log(subtype));
            assert!(
                out.contains(&body),
                "system サブタイプ type={subtype} の本文が会話から落ちた。切り離し系の新カリアを \
                 畳むなら FOLDS_BODY_SYSTEM_SUBTYPES とこのテストを更新し、そうでなければ本文を \
                 運ぶ（握り潰さない）こと: {out:.80}"
            );
        }
    }
}
