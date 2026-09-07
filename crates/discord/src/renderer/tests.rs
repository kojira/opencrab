use super::*;
use opencrab_core::a2ui::{A2uiAction, A2uiComponent, A2uiComponentType};

/// テスト用のダミーRendererを作成
fn test_renderer() -> DiscordRenderer {
    DiscordRenderer::new(Arc::new(Http::new("test-token")))
}

// ── ヘルパー関数 ──────────────────────────────────────

fn text(id: &str, content: &str, variant: Option<&str>) -> A2uiComponent {
    A2uiComponent {
        id: id.into(),
        component_type: A2uiComponentType::Text {
            text: content.into(),
            variant: variant.map(String::from),
        },
    }
}

fn button(id: &str, label: &str, action_name: &str) -> A2uiComponent {
    A2uiComponent {
        id: id.into(),
        component_type: A2uiComponentType::Button {
            text: label.into(),
            action: A2uiAction {
                name: action_name.into(),
                context: None,
            },
            style: None,
            emoji: None,
            disabled: false,
        },
    }
}

fn button_styled(id: &str, label: &str, action_name: &str, style: &str) -> A2uiComponent {
    A2uiComponent {
        id: id.into(),
        component_type: A2uiComponentType::Button {
            text: label.into(),
            action: A2uiAction {
                name: action_name.into(),
                context: None,
            },
            style: Some(style.into()),
            emoji: None,
            disabled: false,
        },
    }
}

fn row(id: &str, children: Vec<&str>) -> A2uiComponent {
    A2uiComponent {
        id: id.into(),
        component_type: A2uiComponentType::Row {
            children: children.into_iter().map(String::from).collect(),
        },
    }
}

fn column(id: &str, children: Vec<&str>) -> A2uiComponent {
    A2uiComponent {
        id: id.into(),
        component_type: A2uiComponentType::Column {
            children: children.into_iter().map(String::from).collect(),
        },
    }
}

// ── extract_text テスト ─────────────────────────────────

#[test]
fn extract_text_plain_body() {
    let r = test_renderer();
    let comps = vec![text("t1", "Hello world", None)];
    assert_eq!(r.extract_text(&comps), "Hello world");
}

#[test]
fn extract_text_variants() {
    let r = test_renderer();
    let comps = vec![
        column("root", vec!["h1", "h2", "h3", "cap", "body"]),
        text("h1", "Title", Some("h1")),
        text("h2", "Subtitle", Some("h2")),
        text("h3", "Section", Some("h3")),
        text("cap", "Small note", Some("caption")),
        text("body", "Normal text", None),
    ];
    let result = r.extract_text(&comps);
    assert_eq!(
        result,
        "# Title\n## Subtitle\n### Section\n-# Small note\nNormal text"
    );
}

#[test]
fn extract_text_column_ordering() {
    let r = test_renderer();
    // Column children ordering determines output order
    let comps = vec![
        column("root", vec!["second", "first"]),
        text("first", "A", None),
        text("second", "B", None),
    ];
    assert_eq!(r.extract_text(&comps), "B\nA");
}

#[test]
fn extract_text_no_column_concatenates_all() {
    let r = test_renderer();
    // Without a Column root, all Text components are concatenated in order
    let comps = vec![
        text("t1", "One", None),
        text("t2", "Two", None),
        text("t3", "Three", None),
    ];
    assert_eq!(r.extract_text(&comps), "One\nTwo\nThree");
}

#[test]
fn extract_text_skips_non_text_children() {
    let r = test_renderer();
    let comps = vec![
        column("root", vec!["t1", "row1", "t2"]),
        text("t1", "Before", None),
        row("row1", vec!["btn1"]),
        text("t2", "After", None),
        button("btn1", "Click", "act"),
    ];
    // Row children are skipped since they aren't Text
    assert_eq!(r.extract_text(&comps), "Before\nAfter");
}

// ── build_buttons テスト ────────────────────────────────

#[test]
fn build_buttons_custom_id_format() {
    let r = test_renderer();
    let comps = vec![button("btn1", "OK", "confirm")];
    let buttons = r
        .build_buttons("interaction:abc-123", &["btn1".into()], &comps)
        .unwrap();
    assert_eq!(buttons.len(), 1);

    // Serialize to check custom_id format
    let json = serde_json::to_value(&buttons[0]).unwrap();
    let custom_id = json["custom_id"].as_str().unwrap();
    assert_eq!(custom_id, "interaction:abc-123:btn1:confirm");
}

#[test]
fn build_buttons_custom_id_truncated_at_100() {
    let r = test_renderer();
    let long_action = "a".repeat(120);
    let comps = vec![A2uiComponent {
        id: "btn1".into(),
        component_type: A2uiComponentType::Button {
            text: "Click".into(),
            action: A2uiAction {
                name: long_action,
                context: None,
            },
            style: None,
            emoji: None,
            disabled: false,
        },
    }];
    let buttons = r
        .build_buttons("interaction:uuid", &["btn1".into()], &comps)
        .unwrap();
    let json = serde_json::to_value(&buttons[0]).unwrap();
    let custom_id = json["custom_id"].as_str().unwrap();
    assert_eq!(custom_id.len(), 100);
}

#[test]
fn build_buttons_label_truncated_at_80() {
    let r = test_renderer();
    let long_label = "あ".repeat(100); // 100 chars
    let comps = vec![A2uiComponent {
        id: "btn1".into(),
        component_type: A2uiComponentType::Button {
            text: long_label,
            action: A2uiAction {
                name: "act".into(),
                context: None,
            },
            style: None,
            emoji: None,
            disabled: false,
        },
    }];
    let buttons = r
        .build_buttons("interaction:u", &["btn1".into()], &comps)
        .unwrap();
    let json = serde_json::to_value(&buttons[0]).unwrap();
    let label = json["label"].as_str().unwrap();
    assert!(label.chars().count() <= 80);
    assert!(label.ends_with("..."));
}

#[test]
fn build_buttons_variant_styles() {
    let r = test_renderer();
    let comps = vec![
        button_styled("b1", "P", "a", "primary"),
        button_styled("b2", "S", "a", "secondary"),
        button_styled("b3", "G", "a", "success"),
        button_styled("b4", "D", "a", "danger"),
        button("b5", "Default", "a"), // no style → Primary
    ];
    let ids: Vec<String> = vec!["b1", "b2", "b3", "b4", "b5"]
        .into_iter()
        .map(String::from)
        .collect();
    let buttons = r.build_buttons("interaction:x", &ids, &comps).unwrap();

    let styles: Vec<u8> = buttons
        .iter()
        .map(|b| {
            let j = serde_json::to_value(b).unwrap();
            j["style"].as_u64().unwrap() as u8
        })
        .collect();
    // ButtonStyle: Primary=1, Secondary=2, Success=3, Danger=4
    assert_eq!(styles, vec![1, 2, 3, 4, 1]);
}

#[test]
fn build_buttons_component_not_found() {
    let r = test_renderer();
    let comps = vec![];
    let result = r.build_buttons("interaction:x", &["missing".into()], &comps);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, RenderError::ComponentNotFound(ref id) if id == "missing"),
        "Expected ComponentNotFound, got: {:?}",
        err
    );
}

// ── build_action_rows テスト ────────────────────────────

#[test]
fn build_action_rows_basic() {
    let r = test_renderer();
    let comps = vec![
        column("root", vec!["t1", "row1"]),
        text("t1", "Hello", None),
        row("row1", vec!["b1", "b2"]),
        button("b1", "Yes", "confirm"),
        button("b2", "No", "cancel"),
    ];
    let rows = r.build_action_rows("interaction:uuid", &comps).unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn build_action_rows_duplicate_action_names_unique_custom_ids() {
    // Regression: buttons with same action.name must have unique custom_ids
    let r = test_renderer();
    let comps = vec![
        column("root", vec!["row1"]),
        row("row1", vec!["b1", "b2"]),
        button("b1", "Option A", "choose"),
        button("b2", "Option B", "choose"),
    ];
    let rows = r.build_action_rows("interaction:uuid", &comps).unwrap();
    assert_eq!(rows.len(), 1);

    let json = serde_json::to_value(&rows[0]).unwrap();
    let components = json["components"].as_array().unwrap();
    let id1 = components[0]["custom_id"].as_str().unwrap();
    let id2 = components[1]["custom_id"].as_str().unwrap();
    assert_ne!(
        id1, id2,
        "custom_ids must be unique even with same action name"
    );
    // b1 and b2 button ids make them unique
    assert!(id1.contains(":b1:"));
    assert!(id2.contains(":b2:"));
}

#[test]
fn build_action_rows_splits_at_5_buttons() {
    let r = test_renderer();
    let btn_ids: Vec<String> = (0..6).map(|i| format!("b{}", i)).collect();
    let mut comps = vec![
        column("root", vec!["row1"]),
        row("row1", btn_ids.iter().map(|s| s.as_str()).collect()),
    ];
    for id in &btn_ids {
        comps.push(button(id, "Btn", "act"));
    }
    let rows = r.build_action_rows("interaction:uuid", &comps).unwrap();
    // 6 buttons → 2 action rows (5 + 1)
    assert_eq!(rows.len(), 2);
}

#[test]
fn build_action_rows_too_many_rows_error() {
    let r = test_renderer();
    // 6 rows × 1 button each = 6 action rows → error
    let row_ids: Vec<String> = (0..6).map(|i| format!("r{}", i)).collect();
    let children: Vec<&str> = row_ids.iter().map(|s| s.as_str()).collect();
    let mut comps = vec![column("root", children.clone())];
    for (i, rid) in row_ids.iter().enumerate() {
        let btn_id = format!("b{}", i);
        comps.push(row(rid, vec![&btn_id]));
        comps.push(button(&btn_id, "X", "act"));
    }
    let result = r.build_action_rows("interaction:uuid", &comps);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        RenderError::TooManyActionRows(6)
    ));
}

// ── Phase 2: SelectMenu テスト ─────────────────────────

fn select_menu(id: &str, options: Vec<(&str, &str)>, action_name: &str) -> A2uiComponent {
    A2uiComponent {
        id: id.into(),
        component_type: A2uiComponentType::SelectMenu {
            options: options
                .into_iter()
                .map(|(label, value)| opencrab_core::a2ui::SelectOption {
                    label: label.into(),
                    value: value.into(),
                    description: None,
                    emoji: None,
                    default: false,
                })
                .collect(),
            placeholder: Some("Choose...".into()),
            min_values: None,
            max_values: None,
            action: A2uiAction {
                name: action_name.into(),
                context: None,
            },
        },
    }
}

fn text_input(id: &str, label: &str, style: Option<&str>, required: bool) -> A2uiComponent {
    A2uiComponent {
        id: id.into(),
        component_type: A2uiComponentType::TextInput {
            label: label.into(),
            placeholder: Some("Enter text...".into()),
            min_length: None,
            max_length: None,
            required,
            style: style.map(String::from),
        },
    }
}

fn form(id: &str, title: &str, children: Vec<&str>, action_name: &str) -> A2uiComponent {
    A2uiComponent {
        id: id.into(),
        component_type: A2uiComponentType::Form {
            title: title.into(),
            children: children.into_iter().map(String::from).collect(),
            action: A2uiAction {
                name: action_name.into(),
                context: None,
            },
        },
    }
}

#[test]
fn build_select_menu_basic() {
    let r = test_renderer();
    let comps = vec![select_menu(
        "sel1",
        vec![("Option A", "a"), ("Option B", "b")],
        "choose",
    )];
    let result = r
        .build_select_menu("interaction:abc-123", "sel1", &comps)
        .unwrap();
    assert!(result.is_some());
    let menu = result.unwrap();
    let json = serde_json::to_value(&menu).unwrap();
    assert_eq!(
        json["custom_id"].as_str().unwrap(),
        "interaction:abc-123:sel1:choose"
    );
    assert_eq!(json["placeholder"].as_str().unwrap(), "Choose...");
    let options = json["options"].as_array().unwrap();
    assert_eq!(options.len(), 2);
    assert_eq!(options[0]["label"].as_str().unwrap(), "Option A");
    assert_eq!(options[0]["value"].as_str().unwrap(), "a");
}

#[test]
fn build_select_menu_custom_id_truncated() {
    let r = test_renderer();
    let long_action = "x".repeat(120);
    let comps = vec![A2uiComponent {
        id: "sel1".into(),
        component_type: A2uiComponentType::SelectMenu {
            options: vec![opencrab_core::a2ui::SelectOption {
                label: "A".into(),
                value: "a".into(),
                description: None,
                emoji: None,
                default: false,
            }],
            placeholder: None,
            min_values: None,
            max_values: None,
            action: A2uiAction {
                name: long_action,
                context: None,
            },
        },
    }];
    let result = r
        .build_select_menu("interaction:uuid", "sel1", &comps)
        .unwrap()
        .unwrap();
    let json = serde_json::to_value(&result).unwrap();
    let cid = json["custom_id"].as_str().unwrap();
    assert!(cid.len() <= 100);
}

#[test]
fn build_select_menu_with_options_details() {
    let r = test_renderer();
    let comps = vec![A2uiComponent {
        id: "sel1".into(),
        component_type: A2uiComponentType::SelectMenu {
            options: vec![
                opencrab_core::a2ui::SelectOption {
                    label: "Alpha".into(),
                    value: "alpha".into(),
                    description: Some("First option".into()),
                    emoji: Some("🅰️".into()),
                    default: true,
                },
                opencrab_core::a2ui::SelectOption {
                    label: "Beta".into(),
                    value: "beta".into(),
                    description: None,
                    emoji: None,
                    default: false,
                },
            ],
            placeholder: Some("Pick one".into()),
            min_values: Some(1),
            max_values: Some(2),
            action: A2uiAction {
                name: "pick".into(),
                context: None,
            },
        },
    }];
    let result = r
        .build_select_menu("interaction:uuid", "sel1", &comps)
        .unwrap()
        .unwrap();
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["min_values"].as_u64().unwrap(), 1);
    assert_eq!(json["max_values"].as_u64().unwrap(), 2);
    let opts = json["options"].as_array().unwrap();
    assert_eq!(opts[0]["description"].as_str().unwrap(), "First option");
    assert!(opts[0]["default"].as_bool().unwrap());
}

#[test]
fn build_action_rows_with_select_menu() {
    let r = test_renderer();
    let comps = vec![
        column("root", vec!["t1", "sel1"]),
        text("t1", "Select something", None),
        select_menu("sel1", vec![("A", "a"), ("B", "b")], "select"),
    ];
    let rows = r.build_action_rows("interaction:uuid", &comps).unwrap();
    assert_eq!(rows.len(), 1);
    // Verify it's a SelectMenu row
    let json = serde_json::to_value(&rows[0]).unwrap();
    assert!(json["components"].as_array().unwrap()[0]["options"]
        .as_array()
        .is_some());
}

/// Regression: SelectMenu が Row 内と Column 直下の両方に同じ id で列挙されても 1 ActionRow のみ。
#[test]
fn build_action_rows_select_menu_not_duplicated_when_row_and_column_child() {
    let r = test_renderer();
    let comps = vec![
        column("root", vec!["t1", "row1", "sel1"]),
        text("t1", "Pick one", None),
        row("row1", vec!["sel1"]),
        select_menu("sel1", vec![("A", "a"), ("B", "b")], "pick"),
    ];
    let rows = r.build_action_rows("interaction:uuid-1", &comps).unwrap();
    assert_eq!(rows.len(), 1);
    let json = serde_json::to_value(&rows[0]).unwrap();
    let opts = json["components"].as_array().unwrap()[0]["options"]
        .as_array()
        .unwrap();
    assert_eq!(opts.len(), 2);
    let cid = json["components"].as_array().unwrap()[0]["custom_id"]
        .as_str()
        .unwrap();
    assert_eq!(cid, "interaction:uuid-1:sel1:pick");
}

#[test]
fn build_select_menu_not_found_returns_error() {
    let r = test_renderer();
    let comps = vec![];
    let result = r.build_select_menu("interaction:x", "missing", &comps);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        RenderError::ComponentNotFound(ref id) if id == "missing"
    ));
}

// ── Phase 2: Form/Modal テスト ─────────────────────────

#[test]
fn build_modal_action_rows_basic() {
    let comps = vec![
        form("form1", "Test Form", vec!["input1", "input2"], "submit"),
        text_input("input1", "Name", Some("short"), true),
        text_input("input2", "Description", Some("paragraph"), false),
    ];
    let form_comp = &comps[0];
    let rows = DiscordRenderer::build_modal_action_rows(form_comp, &comps).unwrap();
    assert_eq!(rows.len(), 2);
}

#[test]
fn build_modal_action_rows_too_many_inputs() {
    let input_ids: Vec<String> = (0..6).map(|i| format!("input{}", i)).collect();
    let mut comps = vec![form(
        "form1",
        "Big Form",
        input_ids.iter().map(|s| s.as_str()).collect(),
        "submit",
    )];
    for id in &input_ids {
        comps.push(text_input(id, "Field", Some("short"), true));
    }
    let result = DiscordRenderer::build_modal_action_rows(&comps[0], &comps);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        RenderError::TooManyActionRows(6)
    ));
}

#[test]
fn build_modal_action_rows_missing_child() {
    let comps = vec![form("form1", "Test", vec!["missing_input"], "submit")];
    let result = DiscordRenderer::build_modal_action_rows(&comps[0], &comps);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        RenderError::ComponentNotFound(ref id) if id == "missing_input"
    ));
}

// ── Phase 2: A2UI Serialization テスト ─────────────────

#[test]
fn a2ui_select_menu_serialization_roundtrip() {
    let comp = select_menu("sel1", vec![("A", "a"), ("B", "b")], "choose");
    let json = serde_json::to_string(&comp).unwrap();
    let parsed: A2uiComponent = serde_json::from_str(&json).unwrap();
    assert!(matches!(
        parsed.component_type,
        A2uiComponentType::SelectMenu { .. }
    ));
    assert_eq!(parsed.id, "sel1");
}

#[test]
fn a2ui_form_serialization_roundtrip() {
    let comp = form("form1", "My Form", vec!["input1"], "submit");
    let json = serde_json::to_string(&comp).unwrap();
    let parsed: A2uiComponent = serde_json::from_str(&json).unwrap();
    assert!(matches!(
        parsed.component_type,
        A2uiComponentType::Form { .. }
    ));
    if let A2uiComponentType::Form {
        title, children, ..
    } = &parsed.component_type
    {
        assert_eq!(title, "My Form");
        assert_eq!(children, &["input1"]);
    }
}

#[test]
fn a2ui_text_input_serialization_roundtrip() {
    let comp = text_input("ti1", "Enter name", Some("paragraph"), false);
    let json = serde_json::to_string(&comp).unwrap();
    let parsed: A2uiComponent = serde_json::from_str(&json).unwrap();
    assert!(matches!(
        parsed.component_type,
        A2uiComponentType::TextInput { .. }
    ));
    if let A2uiComponentType::TextInput {
        label,
        required,
        style,
        ..
    } = &parsed.component_type
    {
        assert_eq!(label, "Enter name");
        assert!(!required);
        assert_eq!(style.as_deref(), Some("paragraph"));
    }
}
