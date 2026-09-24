use super::*;
use serde_json::json;

fn no_reserved(_: &str) -> bool {
    false
}

fn decl(name: &str) -> Value {
    json!({
        "name": name,
        "description": "d",
        "input_schema": {"type": "object"},
        "output_schema": null,
        "callback_schema": null,
        "authorization": {"allowed_callers": ["owner"]},
        "dispatch": "background",
        "sub_engine": "allowed",
        "sharing": "conversation_bound",
        "effect": "state_change"
    })
}

#[test]
fn empty_array_is_valid_zero_tools() {
    let decls = validate_operations(&json!([]), &no_reserved).unwrap();
    assert!(decls.is_empty());
    // 空配列の digest も安定。
    assert_eq!(declaration_digest(&decls).len(), 64);
}

#[test]
fn sorted_names_ok_unsorted_rejected() {
    let ok = validate_operations(&json!([decl("a"), decl("b")]), &no_reserved).unwrap();
    assert_eq!(ok.len(), 2);
    let err = validate_operations(&json!([decl("b"), decl("a")]), &no_reserved).unwrap_err();
    assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
}

#[test]
fn duplicate_name_rejected() {
    let err = validate_operations(&json!([decl("a"), decl("a")]), &no_reserved).unwrap_err();
    assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
}

#[test]
fn reserved_name_is_bad_request() {
    let reserved = |n: &str| n == "reply";
    let err = validate_operations(&json!([decl("reply")]), &reserved).unwrap_err();
    assert_eq!(err.code, ErrorCode::BadRequest);
}

#[test]
fn bad_name_grammar_rejected() {
    let err = validate_operations(&json!([decl("1bad")]), &no_reserved).unwrap_err();
    assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
    let err = validate_operations(&json!([decl("has space")]), &no_reserved).unwrap_err();
    assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
}

#[test]
fn empty_description_rejected() {
    let mut d = decl("a");
    d["description"] = json!("");
    let err = validate_operations(&json!([d]), &no_reserved).unwrap_err();
    assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
}

#[test]
fn schema_must_be_object() {
    let mut d = decl("a");
    d["input_schema"] = json!("not-object");
    let err = validate_operations(&json!([d]), &no_reserved).unwrap_err();
    assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
}

#[test]
fn schema_disallowed_keyword_rejected() {
    let mut d = decl("a");
    d["input_schema"] = json!({"type": "object", "additionalProperties": false});
    let err = validate_operations(&json!([d]), &no_reserved).unwrap_err();
    assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
}

#[test]
fn nested_properties_recurse_and_allow() {
    let mut d = decl("a");
    d["input_schema"] = json!({
        "type": "object",
        "required": ["event", "text"],
        "properties": {
            "event": {"type": "string", "description": "e番号"},
            "text": {"type": "string"}
        }
    });
    let ok = validate_operations(&json!([d]), &no_reserved).unwrap();
    assert_eq!(ok.len(), 1);
}

#[test]
fn class_unknown_enum_rejected() {
    let mut d = decl("a");
    d["sub_engine"] = json!("nope");
    let err = validate_operations(&json!([d]), &no_reserved).unwrap_err();
    assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
}

#[test]
fn missing_field_rejected() {
    let mut d = decl("a");
    d.as_object_mut().unwrap().remove("output_schema");
    let err = validate_operations(&json!([d]), &no_reserved).unwrap_err();
    assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
}

#[test]
fn digest_stable_across_member_order() {
    // schema object の member 順が違っても digest は同値（DI-05 golden 性質）。
    let mut a = decl("x");
    a["input_schema"] = json!({"type": "object", "description": "z"});
    let mut b = decl("x");
    b["input_schema"] = json!({"description": "z", "type": "object"});
    let da = validate_operations(&json!([a]), &no_reserved).unwrap();
    let db = validate_operations(&json!([b]), &no_reserved).unwrap();
    assert_eq!(declaration_digest(&da), declaration_digest(&db));
}

#[test]
fn digest_changes_with_declaration() {
    let one = validate_operations(&json!([decl("a")]), &no_reserved).unwrap();
    let two = validate_operations(&json!([decl("a"), decl("b")]), &no_reserved).unwrap();
    assert_ne!(declaration_digest(&one), declaration_digest(&two));
}

#[test]
fn callback_capable_flag_follows_schema() {
    let mut d = decl("a");
    d["callback_schema"] = json!({"type": "object"});
    let decls = validate_operations(&json!([d]), &no_reserved).unwrap();
    assert!(decls[0].is_callback_capable());
    let plain = validate_operations(&json!([decl("a")]), &no_reserved).unwrap();
    assert!(!plain[0].is_callback_capable());
}

fn s3_decl(name: &str, dispatch: &str, effect: &str) -> Value {
    json!({
        "name": name,
        "description": "synthetic operation",
        "input_schema": {"type": "object"},
        "output_schema": null,
        "callback_schema": null,
        "authorization": {"allowed_callers": ["owner", "trusted"]},
        "dispatch": dispatch,
        "sub_engine": "allowed",
        "sharing": "agent_bound",
        "effect": effect
    })
}

#[test]
fn s3_arbitrary_operation_name_is_classified_only_by_complete_metadata() {
    let declarations = validate_operations(
        &json!([s3_decl("quasar.synthetic-v7", "background", "state_change")]),
        &no_reserved,
    )
    .expect("an arbitrary name with complete generic metadata must validate");
    assert_eq!(declarations[0].name, "quasar.synthetic-v7");
}

#[test]
fn s3_missing_or_unknown_required_metadata_is_rejected() {
    for field in [
        "authorization",
        "dispatch",
        "sub_engine",
        "sharing",
        "effect",
    ] {
        let mut declaration = s3_decl("synthetic", "inline", "read_only");
        declaration.as_object_mut().unwrap().remove(field);
        assert!(
            validate_operations(&json!([declaration]), &no_reserved).is_err(),
            "missing {field} unexpectedly validated"
        );
    }
    for (field, value) in [
        ("dispatch", "later"),
        ("sub_engine", "sometimes"),
        ("sharing", "global"),
        ("effect", "network"),
    ] {
        let mut declaration = s3_decl("synthetic", "inline", "read_only");
        declaration[field] = json!(value);
        assert!(
            validate_operations(&json!([declaration]), &no_reserved).is_err(),
            "unknown {field}={value} unexpectedly validated"
        );
    }
}

#[test]
fn s3_dispatch_utterance_if_and_only_if_effect_utterance() {
    for (dispatch, effect, valid) in [
        ("utterance", "utterance", true),
        ("utterance", "state_change", false),
        ("inline", "utterance", false),
        ("background", "utterance", false),
        ("inline", "read_only", true),
        ("background", "state_change", true),
    ] {
        assert_eq!(
            validate_operations(
                &json!([s3_decl("synthetic", dispatch, effect)]),
                &no_reserved,
            )
            .is_ok(),
            valid,
            "dispatch={dispatch}, effect={effect}"
        );
    }
}

#[test]
fn s3_runtime_compatibility_rejects_missing_utterance_and_weak_guarantee() {
    let background = validate_operations(
        &json!([s3_decl("synthetic", "background", "state_change")]),
        &no_reserved,
    )
    .unwrap();
    assert!(validate_runtime_compatibility(
        &background,
        FinalDelivery::OperationDriven,
        DeliveryGuarantee::AtMostOnceIndeterminate,
    )
    .is_err());

    let mut exact = s3_decl("synthetic", "utterance", "utterance");
    exact["required_delivery_guarantee"] = json!("exactly_once");
    let exact = validate_operations(&json!([exact]), &no_reserved).unwrap();
    assert!(validate_runtime_compatibility(
        &exact,
        FinalDelivery::OperationDriven,
        DeliveryGuarantee::AtMostOnceIndeterminate,
    )
    .is_err());
    assert!(validate_runtime_compatibility(
        &exact,
        FinalDelivery::OperationDriven,
        DeliveryGuarantee::ExactlyOnce,
    )
    .is_ok());
}

#[test]
fn s3_declaration_requirement_allows_only_exactly_once() {
    let mut weak = s3_decl("synthetic", "utterance", "utterance");
    weak["required_delivery_guarantee"] = json!("at_most_once_indeterminate");
    assert_eq!(
        validate_operations(&json!([weak]), &no_reserved)
            .unwrap_err()
            .code,
        ErrorCode::OperationDeclarationInvalid
    );

    let mut exact = s3_decl("synthetic", "utterance", "utterance");
    exact["required_delivery_guarantee"] = json!("exactly_once");
    assert!(validate_operations(&json!([exact]), &no_reserved).is_ok());
}

#[test]
fn s3_runtime_capabilities_are_digest_covered() {
    let declarations = validate_operations(
        &json!([s3_decl("synthetic", "inline", "read_only")]),
        &no_reserved,
    )
    .unwrap();
    assert_ne!(
        runtime_declaration_digest(
            &declarations,
            FinalDelivery::Automatic,
            DeliveryGuarantee::AtMostOnceIndeterminate,
        ),
        runtime_declaration_digest(
            &declarations,
            FinalDelivery::Automatic,
            DeliveryGuarantee::ExactlyOnce,
        )
    );
}

#[test]
fn s3_all_policy_metadata_changes_the_declaration_digest() {
    let base = s3_decl("synthetic", "inline", "read_only");
    let base_digest =
        declaration_digest(&validate_operations(&json!([base.clone()]), &no_reserved).unwrap());
    let mutations = [
        ("authorization", json!({"allowed_callers": ["owner"]})),
        ("dispatch", json!("background")),
        ("sub_engine", json!("blocked")),
        ("sharing", json!("conversation_bound")),
        ("effect", json!("state_change")),
    ];
    for (field, value) in mutations {
        let mut changed = base.clone();
        changed[field] = value;
        let digest =
            declaration_digest(&validate_operations(&json!([changed]), &no_reserved).unwrap());
        assert_ne!(digest, base_digest, "{field} was not digest-covered");
    }
}
