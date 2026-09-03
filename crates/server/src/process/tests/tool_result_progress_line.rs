// 実況（progress line）は永続化と同じ無害化（`sanitize_tool_result_for_log`）を通す。
// #620: 旧来の nsec キー名マスク（SECRET_KEYS）は撤去したので、ここは上限/退避だけを行う。
use super::tool_result_progress_line;

/// 実況行がツール名・成否・中身を含み、失敗は failed と出ること。
///
/// #620: nsec キー名マスクは撤去した。`nostr_generate_key` は実際には nsec を返さない
/// （npub のみ / `crates/nostr/src/actions.rs`）ので、実運用の実況に nsec は元から
/// 現れない。よってここでは通常の結果で行の体裁だけを固定する。
#[test]
fn test_tool_result_progress_line_shape() {
    let wrapper = r#"{"success":true,"data":{"npub":"npub1abc"},"error":null}"#;
    let line = tool_result_progress_line(
        "nostr_generate_key",
        wrapper,
        false,
        "session-1",
        "tool-call-1",
    );
    // 実況として要る情報（ツール名・成否・中身）は落とさない。
    assert!(line.contains("nostr_generate_key"));
    assert!(line.contains("completed"));
    assert!(line.contains("npub1abc"));
    // 撤去したはずのキー名マスクが復活していない。
    assert!(
        !line.contains("[redacted]"),
        "撤去したマスクが効いている: {line}"
    );

    let failed = tool_result_progress_line("read_file", r#"{"error":"nope"}"#, true, "s", "t");
    assert!(failed.contains("failed"), "失敗は failed と出る: {failed}");
}
