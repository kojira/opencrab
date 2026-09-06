use super::super::*;
use super::support::*;

// ---- #157 S5: 通知先（webhook）の管理ツール ----

/// 移設した 6 ツールの名前（#157 S5）。`ensure_*` は含まない（Discord 側に残る）。
const MOVED_WEBHOOK_TOOLS: &[&str] = &[
    "get_default_subtask_webhook",
    "set_default_subtask_webhook",
    "list_subtask_webhooks",
    "get_default_webhook",
    "set_default_webhook",
    "list_webhooks",
];

/// **#157 S5 の本題**: 6 ツールが own 定義にちょうど 1 件ずつある。
#[test]
fn webhook_target_tools_are_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    for name in MOVED_WEBHOOK_TOOLS {
        assert_eq!(
            defs.iter().filter(|d| &d.name == name).count(),
            1,
            "{name} は own 定義にちょうど 1 件必要（#157 S5）"
        );
    }
}

/// **Discord 無効の構成でも 6 ツールが露出する**（#157 S5 の証明）。
///
/// `inner = None` は「transport 固有 gateway が居ない」経路（web / REST / Nostr /
/// heartbeat、および Discord feature 無効ビルド）そのもの。移設前はこの構成で
/// 6 ツールが一切出なかった＝ #157 が報告している不具合そのもの。
#[test]
fn webhook_target_tools_are_exposed_without_any_transport_gateway() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    for name in MOVED_WEBHOOK_TOOLS {
        assert!(
                names.contains(&name.to_string()),
                "transport gateway 無しの構成で {name} が露出しない（#157 の不具合そのもの）: {names:?}"
            );
    }
    // 逆に、Discord に残した `ensure_*` はここには出ない（inner が居ないため）。
    for name in ["ensure_webhook", "ensure_subtask_webhook"] {
        assert!(
            !names.contains(&name.to_string()),
            "{name} は Discord gateway 由来のはず（own に増やしてはいけない）"
        );
    }
}

/// 引数スキーマを移設前（Discord 定義）と同一に保つ。
///
/// 名前・`required`・プロパティ名の集合をリテラルで固定する。ここが変わると
/// 既存の会話ログにあるツール呼び出しが通らなくなる。
#[test]
fn webhook_target_tool_schemas_match_the_discord_originals() {
    let defs = SystemGatewayActions::own_definitions();
    let find = |n: &str| defs.iter().find(|d| d.name == n).unwrap();
    let props = |n: &str| {
        let mut keys: Vec<String> = find(n).parameters["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    };

    assert_eq!(
        find("get_default_subtask_webhook").parameters["required"],
        json!([])
    );
    assert_eq!(
        props("get_default_subtask_webhook"),
        vec!["agent_id", "scope", "tool_name"]
    );

    assert_eq!(
        find("set_default_subtask_webhook").parameters["required"],
        json!(["scope"])
    );
    assert_eq!(
        props("set_default_subtask_webhook"),
        vec![
            "agent_id",
            "enabled",
            "events",
            "kind",
            "max_chars",
            "output_mode",
            "scope",
            "tool_name",
            "url",
        ]
    );

    assert_eq!(
        find("list_subtask_webhooks").parameters["required"],
        json!([])
    );
    assert_eq!(
        props("list_subtask_webhooks"),
        vec!["agent_id", "include_disabled", "scope"]
    );

    assert_eq!(
        find("get_default_webhook").parameters["required"],
        json!([])
    );
    assert_eq!(
        props("get_default_webhook"),
        vec!["agent_id", "family", "tool_name"]
    );

    assert_eq!(
        find("set_default_webhook").parameters["required"],
        json!(["scope"])
    );
    assert_eq!(
        props("set_default_webhook"),
        vec![
            "agent_id",
            "enabled",
            "events",
            "family",
            "max_chars",
            "output_mode",
            "scope",
            "tool_name",
            "url",
        ]
    );

    assert_eq!(find("list_webhooks").parameters["required"], json!([]));
    assert_eq!(
        props("list_webhooks"),
        vec!["agent_id", "family", "include_disabled", "scope"]
    );
}

/// **6 ツールは inner へ委譲されない**（own が唯一の実装）。
///
/// 委譲パターンのまま残すと、Discord が誤って再定義したときに own の実装が黙って
/// バイパスされる（#155 の後退）。`ensure_*` は逆に inner へ渡る必要がある。
#[tokio::test]
async fn webhook_target_tools_are_not_delegated_to_inner() {
    let state = crate::test_app_state();
    let inner = Arc::new(RecordingInner::new(&[
        "get_default_subtask_webhook",
        "set_default_subtask_webhook",
        "list_subtask_webhooks",
        "get_default_webhook",
        "set_default_webhook",
        "list_webhooks",
        "ensure_webhook",
    ]));
    let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);

    for name in MOVED_WEBHOOK_TOOLS {
        let _ = actions
            .execute(name, &json!({"scope": "agent"}), &owner_ctx())
            .await;
    }
    assert!(
        inner.calls().is_empty(),
        "移設した 6 ツールが inner へ委譲された: {:?}",
        inner.calls()
    );

    // Discord に残した `ensure_webhook` は既定アームで inner へ委譲される。
    let _ = actions
        .execute("ensure_webhook", &json!({}), &owner_ctx())
        .await;
    assert_eq!(inner.calls(), vec!["ensure_webhook".to_string()]);
}
