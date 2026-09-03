use super::error_body_with_prompt_size;

/// #706: 成功行（error_str=None）にはサイズを付けない（＝毎リクエストで再走査しない）。
#[test]
fn success_row_gets_no_size() {
    assert_eq!(
        error_body_with_prompt_size(None, "とても長いプロンプト"),
        None
    );
}

/// #706: 失敗行には prompt 列と同じ全体シリアライズの**文字数**が一様に載る。
/// マルチバイトでもバイト数ではなく文字数（Unicode スカラー数）で数える。
#[test]
fn failure_row_appends_prompt_char_count() {
    // "あいうえお" = 5 文字 / 15 バイト。文字数で数えていることを固定する。
    let prompt_json = "あいうえお";
    let out = error_body_with_prompt_size(Some("空でした"), prompt_json)
        .expect("失敗行にはサイズ付き error_body が返るはず");
    assert!(out.contains("空でした"), "元の理由が保たれていない: {out}");
    assert!(
        out.contains("prompt_chars=5"),
        "prompt 列と同じシリアライズの文字数（5）が載っていない: {out}"
    );
    assert!(
        !out.contains("prompt_chars=15"),
        "バイト数で数えてしまっている: {out}"
    );
}
