/// #332: タイムアウトの本文が「未完了・対応が必要」と読め、途中経過の在り処
/// （`subtask-{id}` セッション）と経過・上限の実数を含むこと。**返信は強制しない**
/// ので命令形の強制文言（「返信せよ」等）は入れない。旧文言 `"Subtask timed out."`
/// のような「終わった」だけの通知に戻ったら落ちる。
#[test]
fn timeout_result_text_prompts_action_without_forcing_reply() {
    let text = timeout_result_text("subtask-abc123", 300, 300);

    assert!(text.contains("対応が必要"), "対応を促す文言が無い: {text}");
    assert!(text.contains("未完了"), "未完了と明示していない: {text}");
    assert!(
        !text.contains("完了しました"),
        "timeout なのに完了を断言している: {text}"
    );
    assert!(
        text.contains("subtask-abc123"),
        "ログの在り処（sub セッション）が無い: {text}"
    );
    assert!(text.contains("300"), "経過/上限の実数が無い: {text}");
    assert!(
        !text.contains("返信して") && !text.contains("必ず返信"),
        "返信を強制する文言が入っている: {text}"
    );
    assert_ne!(text, "Subtask timed out.");
}

/// 経過秒と上限秒は引数がそのまま反映される（固定文字列ではない）。
#[test]
fn timeout_result_text_reflects_elapsed_and_limit() {
    let text = timeout_result_text("subtask-xyz", 42, 120);
    assert!(text.contains("120"), "上限秒が反映されない: {text}");
    assert!(text.contains("42"), "経過秒が反映されない: {text}");
    assert!(text.contains("subtask-xyz"));
}
