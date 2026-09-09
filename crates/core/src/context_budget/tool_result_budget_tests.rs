use super::tool_result_budget::{
    allocate_result_tokens, available_result_tokens, effective_input_limit, page_utf8_result,
    validate_final_request_tokens, ModelInputLimits,
};

#[test]
fn independent_input_limit_does_not_subtract_max_output_tokens() {
    let limits = ModelInputLimits {
        max_input_tokens: Some(100_000),
        max_output_tokens: Some(32_000),
        max_total_tokens: None,
    };
    assert_eq!(
        effective_input_limit(limits, Some(32_000)).unwrap(),
        100_000
    );
}

#[test]
fn shared_total_limit_subtracts_requested_output_not_model_maximum() {
    let limits = ModelInputLimits {
        max_input_tokens: Some(100_000),
        max_output_tokens: Some(32_000),
        max_total_tokens: Some(110_000),
    };
    assert_eq!(effective_input_limit(limits, Some(32_000)).unwrap(), 78_000);
    assert_eq!(effective_input_limit(limits, Some(8_000)).unwrap(), 100_000);
}

#[test]
fn missing_or_inconsistent_model_limits_fail_loud() {
    let missing_input = ModelInputLimits {
        max_input_tokens: None,
        max_output_tokens: Some(32_000),
        max_total_tokens: None,
    };
    assert!(effective_input_limit(missing_input, Some(8_000)).is_err());

    let output_too_large = ModelInputLimits {
        max_input_tokens: Some(100_000),
        max_output_tokens: Some(32_000),
        max_total_tokens: None,
    };
    assert!(effective_input_limit(output_too_large, Some(32_001)).is_err());

    let exhausted_shared_window = ModelInputLimits {
        max_input_tokens: Some(100_000),
        max_output_tokens: Some(32_000),
        max_total_tokens: Some(8_000),
    };
    assert!(effective_input_limit(exhausted_shared_window, Some(8_000)).is_err());
}

#[test]
fn every_existing_request_token_reduces_result_budget() {
    let limits = ModelInputLimits {
        max_input_tokens: Some(100_000),
        max_output_tokens: Some(32_000),
        max_total_tokens: None,
    };
    assert_eq!(
        available_result_tokens(limits, Some(8_000), 61_234).unwrap(),
        38_766
    );
    assert_eq!(
        available_result_tokens(limits, Some(8_000), 61_235).unwrap(),
        38_765
    );
    assert!(available_result_tokens(limits, Some(8_000), 100_001).is_err());
}

#[test]
fn multiple_results_get_fair_deterministic_redistribution() {
    // 12 tokenをまず3件へ4ずつ配る。一件目は2だけ必要なので、余り2を未収容の
    // 二件目、三件目へcompletion順で1ずつ再配分する。
    assert_eq!(allocate_result_tokens(12, &[2, 10, 10]), vec![2, 5, 5]);
    assert_eq!(allocate_result_tokens(3, &[10, 10]), vec![2, 1]);
    assert_eq!(allocate_result_tokens(0, &[10, 10]), vec![0, 0]);
}

#[test]
fn final_wire_request_is_rejected_when_meter_exceeds_effective_limit() {
    assert!(validate_final_request_tokens(99_999, 100_000).is_ok());
    assert!(validate_final_request_tokens(100_000, 100_000).is_ok());
    assert!(validate_final_request_tokens(100_001, 100_000).is_err());
}

#[test]
fn read_result_page_preserves_utf8_boundary_and_next_offset() {
    let source = "ab日本語cd";
    let first = page_utf8_result(source, 0, 5).unwrap();
    assert_eq!(first.body, "ab日");
    assert_eq!(first.start_byte, 0);
    assert_eq!(first.end_byte, 5);
    assert_eq!(first.next_byte, Some(5));
    assert!(first.has_more);

    let second = page_utf8_result(source, first.next_byte.unwrap(), 6).unwrap();
    assert_eq!(second.body, "本語");
    assert_eq!(second.start_byte, 5);
    assert_eq!(second.end_byte, 11);
    assert_eq!(second.next_byte, Some(11));
    assert!(second.has_more);

    let last = page_utf8_result(source, second.next_byte.unwrap(), 10).unwrap();
    assert_eq!(last.body, "cd");
    assert_eq!(last.next_byte, None);
    assert!(!last.has_more);
}

#[test]
fn read_result_page_rejects_non_boundary_or_zero_cap() {
    let source = "日本語";
    assert!(page_utf8_result(source, 1, 3).is_err());
    assert!(page_utf8_result(source, 0, 0).is_err());
    assert!(page_utf8_result(source, source.len() + 1, 3).is_err());
}
