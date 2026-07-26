//! Form トリガーボタン → モーダル応答の解決（gateway の interaction_create で使用）。

use std::sync::Arc;

use opencrab_core::a2ui::{A2uiComponentType, PendingInteractionRegistry};
use opencrab_gateway::{A2uiFormModalResolver, A2uiFormModalSpec};
use serenity::all::CreateActionRow;

/// `PendingInteractionRegistry` を参照してモーダル仕様を解決するクロージャを返す。
pub fn form_modal_resolver(registry: PendingInteractionRegistry) -> A2uiFormModalResolver {
    Arc::new(move |custom_id: &str, user_id: &str| {
        resolve_form_modal_for_button(&registry, custom_id, user_id)
    })
}

/// ボタンの `custom_id` が Form 送信アクションと一致し UI 上に Form がある場合、モーダル表示用データを返す。
pub fn resolve_form_modal_for_button(
    registry: &PendingInteractionRegistry,
    custom_id: &str,
    user_id: &str,
) -> Option<A2uiFormModalSpec> {
    let parts: Vec<&str> = custom_id.splitn(4, ':').collect();
    if parts.len() < 4 || parts[0] != "interaction" {
        return None;
    }
    let interaction_id = parts[1];
    let button_component_id = parts[2];
    let action_name = parts[3];

    let pending = registry.get(interaction_id)?;

    if !pending.owner_id.is_empty() && user_id != pending.owner_id {
        return None;
    }

    let fd = pending.form_data.as_ref()?;
    if fd.action.name != action_name {
        return None;
    }

    let btn = pending
        .a2ui_components
        .iter()
        .find(|c| c.id == button_component_id)?;
    let A2uiComponentType::Button { action, .. } = &btn.component_type else {
        return None;
    };
    if action.name != action_name {
        return None;
    }

    let form = pending
        .a2ui_components
        .iter()
        .find(|c| matches!(&c.component_type, A2uiComponentType::Form { .. }))?;
    let A2uiComponentType::Form {
        action: form_action,
        ..
    } = &form.component_type
    else {
        return None;
    };
    if form_action.name != action_name {
        return None;
    }

    // 保留状態は gateway 非依存（コアの型）なので、serenity の描画物は
    // `RenderedForm` の型消去された payload から取り出す（入れたのは
    // `DiscordRenderer::build_form`）。
    let action_rows = fd.payload::<Vec<CreateActionRow>>()?;

    Some(A2uiFormModalSpec {
        modal_custom_id: fd.modal_custom_id.clone(),
        title: fd.title.clone(),
        components: action_rows.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;
    use opencrab_core::a2ui::{
        A2uiAction, A2uiComponent, A2uiComponentType, PendingInteraction, RenderTarget,
        RenderedForm, RenderedMessage,
    };
    use serenity::all::{CreateInputText, InputTextStyle};

    fn test_registry() -> PendingInteractionRegistry {
        Arc::new(DashMap::new())
    }

    fn dummy_pending(form_data: Option<RenderedForm>) -> PendingInteraction {
        PendingInteraction {
            session_id: "s1".into(),
            agent_id: "a1".into(),
            target: RenderTarget {
                channel_id: "1".into(),
                platform: "discord".into(),
            },
            surface_id: "interaction:uuid-abc".into(),
            a2ui_components: vec![],
            owner_id: String::new(),
            created_at: chrono::Utc::now(),
            timeout_secs: 300,
            rendered_message: RenderedMessage {
                platform: "discord".into(),
                message_id: Some("1".into()),
                channel_id: "1".into(),
            },
            form_data,
        }
    }

    fn form_render(modal_custom_id: &str, title: &str, action_name: &str) -> RenderedForm {
        RenderedForm::new(
            modal_custom_id,
            title,
            A2uiAction {
                name: action_name.into(),
                context: None,
            },
            vec![CreateActionRow::InputText(
                CreateInputText::new(InputTextStyle::Short, "Name", "field1").required(true),
            )],
        )
    }

    #[test]
    fn resolve_returns_none_without_form_data() {
        let reg = test_registry();
        reg.insert("uuid-abc".into(), dummy_pending(None));
        let r = resolve_form_modal_for_button(&reg, "interaction:uuid-abc:btn1:submit", "user1");
        assert!(r.is_none());
    }

    #[test]
    fn resolve_returns_modal_when_button_matches_form_action() {
        let form_data = form_render("interaction:uuid-abc:modal:submit", "My form", "submit");
        let mut pending = dummy_pending(Some(form_data));
        pending.a2ui_components = vec![
            A2uiComponent {
                id: "form1".into(),
                component_type: A2uiComponentType::Form {
                    title: "My form".into(),
                    children: vec!["field1".into()],
                    action: A2uiAction {
                        name: "submit".into(),
                        context: None,
                    },
                },
            },
            A2uiComponent {
                id: "btn1".into(),
                component_type: A2uiComponentType::Button {
                    text: "Open".into(),
                    action: A2uiAction {
                        name: "submit".into(),
                        context: None,
                    },
                    style: None,
                    emoji: None,
                    disabled: false,
                },
            },
        ];
        let reg = test_registry();
        reg.insert("uuid-abc".into(), pending);

        let spec = resolve_form_modal_for_button(&reg, "interaction:uuid-abc:btn1:submit", "user1")
            .expect("modal spec");
        assert_eq!(spec.modal_custom_id, "interaction:uuid-abc:modal:submit");
        assert_eq!(spec.title, "My form");
        assert_eq!(spec.components.len(), 1);
    }

    #[test]
    fn resolve_respects_owner_only() {
        let form_data = form_render("interaction:uuid-abc:modal:go", "T", "go");
        let mut pending = dummy_pending(Some(form_data));
        pending.owner_id = "owner99".into();
        pending.a2ui_components = vec![
            A2uiComponent {
                id: "f1".into(),
                component_type: A2uiComponentType::Form {
                    title: "T".into(),
                    children: vec![],
                    action: A2uiAction {
                        name: "go".into(),
                        context: None,
                    },
                },
            },
            A2uiComponent {
                id: "b1".into(),
                component_type: A2uiComponentType::Button {
                    text: "Go".into(),
                    action: A2uiAction {
                        name: "go".into(),
                        context: None,
                    },
                    style: None,
                    emoji: None,
                    disabled: false,
                },
            },
        ];
        let reg = test_registry();
        reg.insert("uuid-abc".into(), pending);

        assert!(
            resolve_form_modal_for_button(&reg, "interaction:uuid-abc:b1:go", "other_user",)
                .is_none()
        );
    }

    /// 描画物が Discord の型でない（＝別 transport が入れた）保留状態からは
    /// モーダル仕様を作らない（downcast 失敗で None）。
    #[test]
    fn resolve_returns_none_for_foreign_form_payload() {
        let foreign = RenderedForm::new(
            "interaction:uuid-abc:modal:go",
            "T",
            A2uiAction {
                name: "go".into(),
                context: None,
            },
            vec!["not-a-serenity-row".to_string()],
        );
        let mut pending = dummy_pending(Some(foreign));
        pending.a2ui_components = vec![
            A2uiComponent {
                id: "f1".into(),
                component_type: A2uiComponentType::Form {
                    title: "T".into(),
                    children: vec![],
                    action: A2uiAction {
                        name: "go".into(),
                        context: None,
                    },
                },
            },
            A2uiComponent {
                id: "b1".into(),
                component_type: A2uiComponentType::Button {
                    text: "Go".into(),
                    action: A2uiAction {
                        name: "go".into(),
                        context: None,
                    },
                    style: None,
                    emoji: None,
                    disabled: false,
                },
            },
        ];
        let reg = test_registry();
        reg.insert("uuid-abc".into(), pending);
        assert!(
            resolve_form_modal_for_button(&reg, "interaction:uuid-abc:b1:go", "user1").is_none()
        );
    }
}
