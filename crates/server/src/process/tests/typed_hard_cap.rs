use super::typed_exceeds_input_budget;

/// #884 PR2 hard cap: wire トークンが input_high を超えるときだけ flat へ落とす。
#[test]
fn typed_falls_back_only_above_input_budget() {
    // 上限以下・ちょうど上限は typed を維持（false）。
    assert!(!typed_exceeds_input_budget(0, 1000));
    assert!(!typed_exceeds_input_budget(999, 1000));
    assert!(!typed_exceeds_input_budget(1000, 1000));
    // 上限超過は flat へフォールバック（true）。
    assert!(typed_exceeds_input_budget(1001, 1000));
    assert!(typed_exceeds_input_budget(50_000, 1000));
}
