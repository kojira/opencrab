use super::skill_mentioned;

#[test]
fn matches_name_case_insensitively() {
    let resp = "この件は Deploy Runbook に従って対応しました。".to_lowercase();
    assert!(skill_mentioned(&resp, "Deploy Runbook"));
    assert!(skill_mentioned(&resp, "deploy runbook"));
}

#[test]
fn ignores_short_names() {
    // 3文字以下は誤マッチ防止で対象外
    let resp = "abc の話".to_lowercase();
    assert!(!skill_mentioned(&resp, "abc"));
}

#[test]
fn no_match_when_absent() {
    let resp = "普通の返答です".to_lowercase();
    assert!(!skill_mentioned(&resp, "translation-helper"));
}
