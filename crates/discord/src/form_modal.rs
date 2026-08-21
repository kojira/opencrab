//! Form トリガーボタン → モーダル応答の解決（gateway の interaction_create で使用）。
//!
//! 保留状態（`opencrab_core::a2ui::PendingInteraction`）は**描画物を持たない**（#156 S3）。
//! モーダルの入力欄・タイトル・`custom_id` はすべて保留状態の部品ツリーと `surface_id`
//! から**ここで組み直す**。そのおかげでコアは serenity の型を知らず、型消去も要らない。

use std::sync::Arc;

use crate::gateway::{A2uiFormModalResolver, A2uiFormModalSpec};
use opencrab_core::a2ui::{A2uiComponentType, PendingInteractionRegistry};
use tracing::warn;

use crate::renderer::DiscordRenderer;

/// `PendingInteractionRegistry` を参照してモーダル仕様を解決するクロージャを返す。
pub fn form_modal_resolver(registry: PendingInteractionRegistry) -> A2uiFormModalResolver {
    Arc::new(move |custom_id: &str, user_id: &str| {
        resolve_form_modal_for_button(&registry, custom_id, user_id)
    })
}

/// Form のモーダル `custom_id`（形式: `interaction:{uuid}:modal:{action_name}`）。
///
/// ボタンの `custom_id`（`interaction:{uuid}:{component_id}:{action_name}` /
/// `DiscordRenderer::build_buttons`）と同じ `uuid` 部分を使う。
fn modal_custom_id(surface_id: &str, action_name: &str) -> String {
    let uuid_part = surface_id
        .strip_prefix("interaction:")
        .unwrap_or(surface_id);
    format!("interaction:{}:modal:{}", uuid_part, action_name)
}

/// ボタンの `custom_id` が Form 送信アクションと一致し UI 上に Form がある場合、モーダル表示用データを返す。
///
/// 操作できるのはオーナーだけ。**オーナー未設定なら誰も開けない**（#174）。
/// 以前は「`owner_id` が空なら判定しない」＝誰でも開ける、という fail-open
/// だったため、オーナー欄を空のまま運用すると権限ゲートが黙って無効化されていた。
/// 判定は `opencrab_core::owner::is_owner_id` に集約。
///
/// このゲートは `send_ui` の `owner_only` 引数を見ない（あれは DB の列にしか
/// 効かない）。**すべての A2UI 描画面**に効く点に注意。
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

    if !opencrab_core::owner::is_owner_id(&pending.owner_id, user_id) {
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

    // 最初の Form コンポーネントだけを対象にする（送信時の挙動と同じ）。
    let form = pending
        .a2ui_components
        .iter()
        .find(|c| matches!(&c.component_type, A2uiComponentType::Form { .. }))?;
    let A2uiComponentType::Form {
        title,
        action: form_action,
        ..
    } = &form.component_type
    else {
        return None;
    };
    if form_action.name != action_name {
        return None;
    }

    // 入力欄は部品ツリーから組み直す（保留状態は描画物を持たない）。
    let components = match DiscordRenderer::build_modal_action_rows(form, &pending.a2ui_components)
    {
        Ok(rows) => rows,
        Err(e) => {
            // モーダルは開かず、この押下は**通常のボタン応答**として親セッションを
            // 再開する（フォーム入力値は付かない）。原因追跡の手掛かりを残す。
            warn!(
                interaction_id = %interaction_id,
                action = %action_name,
                error = %e,
                "A2UI Form のモーダル構築に失敗: モーダルを開かず、通常のボタン応答として\
                 セッションを再開する（フォーム入力値なし）"
            );
            return None;
        }
    };

    Some(A2uiFormModalSpec {
        modal_custom_id: modal_custom_id(&pending.surface_id, action_name),
        title: title.clone(),
        components,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;
    use opencrab_core::a2ui::{
        A2uiAction, A2uiComponent, PendingInteraction, RenderTarget, RenderedMessage,
    };

    /// 既定のオーナー。owner 判定は fail-closed（#174）なので、モーダル解決そのものを
    /// 見たいテストではオーナーを設定した状態を既定にする（未設定だと誰も開けず、
    /// 「Form が無いから None」なのか「オーナー未設定だから None」なのか区別できない）。
    const OWNER: &str = "owner99";

    fn test_registry() -> PendingInteractionRegistry {
        Arc::new(DashMap::new())
    }

    fn pending_with(components: Vec<A2uiComponent>) -> PendingInteraction {
        PendingInteraction {
            session_id: "s1".into(),
            agent_id: "a1".into(),
            target: RenderTarget {
                channel_id: "1".into(),
                platform: "discord".into(),
            },
            surface_id: "interaction:uuid-abc".into(),
            a2ui_components: components,
            owner_id: OWNER.into(),
            caller: opencrab_actions::CallerIdentity::Owner,
            created_at: chrono::Utc::now(),
            timeout_secs: 300,
            rendered_message: RenderedMessage {
                platform: "discord".into(),
                message_id: Some("1".into()),
                channel_id: "1".into(),
            },
        }
    }

    fn action(name: &str) -> A2uiAction {
        A2uiAction {
            name: name.into(),
            context: None,
        }
    }

    fn button(id: &str, action_name: &str) -> A2uiComponent {
        A2uiComponent {
            id: id.into(),
            component_type: A2uiComponentType::Button {
                text: "Go".into(),
                action: action(action_name),
                style: None,
                emoji: None,
                disabled: false,
            },
        }
    }

    fn form(id: &str, title: &str, children: Vec<&str>, action_name: &str) -> A2uiComponent {
        A2uiComponent {
            id: id.into(),
            component_type: A2uiComponentType::Form {
                title: title.into(),
                children: children.into_iter().map(String::from).collect(),
                action: action(action_name),
            },
        }
    }

    fn text_input(id: &str, label: &str) -> A2uiComponent {
        A2uiComponent {
            id: id.into(),
            component_type: A2uiComponentType::TextInput {
                label: label.into(),
                placeholder: None,
                min_length: None,
                max_length: None,
                required: true,
                style: None,
            },
        }
    }

    #[test]
    fn resolve_returns_none_without_a_form_component() {
        let reg = test_registry();
        reg.insert(
            "uuid-abc".into(),
            pending_with(vec![button("btn1", "submit")]),
        );
        let r = resolve_form_modal_for_button(&reg, "interaction:uuid-abc:btn1:submit", OWNER);
        assert!(r.is_none());
    }

    /// **描画物ではなく部品ツリーからモーダルを組み直す**（#156 S3）。
    #[test]
    fn resolve_rebuilds_modal_from_the_component_tree() {
        let reg = test_registry();
        reg.insert(
            "uuid-abc".into(),
            pending_with(vec![
                form("form1", "My form", vec!["field1"], "submit"),
                button("btn1", "submit"),
                text_input("field1", "Name"),
            ]),
        );

        let spec = resolve_form_modal_for_button(&reg, "interaction:uuid-abc:btn1:submit", OWNER)
            .expect("modal spec");
        assert_eq!(spec.modal_custom_id, "interaction:uuid-abc:modal:submit");
        assert_eq!(spec.title, "My form");
        assert_eq!(spec.components.len(), 1);
    }

    #[test]
    fn resolve_respects_owner_only() {
        let pending = pending_with(vec![form("f1", "T", vec![], "go"), button("b1", "go")]);
        let reg = test_registry();
        reg.insert("uuid-abc".into(), pending);

        assert!(
            resolve_form_modal_for_button(&reg, "interaction:uuid-abc:b1:go", "other_user",)
                .is_none()
        );
        // owner なら開く。
        assert!(resolve_form_modal_for_button(&reg, "interaction:uuid-abc:b1:go", OWNER).is_some());
    }

    /// #174: オーナー未設定なら**誰も**開けない（従来は誰でも開けた）。
    ///
    /// 空白のみのオーナー ID も未設定と同じ扱いで、空白を送った相手にも開かない。
    #[test]
    fn resolve_denies_everyone_when_owner_is_unset() {
        for owner in ["", "   ", " \n"] {
            let mut pending = pending_with(vec![form("f1", "T", vec![], "go"), button("b1", "go")]);
            pending.owner_id = owner.into();
            let reg = test_registry();
            reg.insert("uuid-abc".into(), pending);

            for user in ["other_user", "", " ", OWNER] {
                assert!(
                    resolve_form_modal_for_button(&reg, "interaction:uuid-abc:b1:go", user)
                        .is_none(),
                    "owner={owner:?} user={user:?} でモーダルが開いた"
                );
            }
        }
    }

    /// 入力欄の組み直しに失敗する部品ツリー（子が実在しない）ではモーダルを開かない。
    /// このときは通常のボタン応答として扱われる（警告をログに残す）。
    #[test]
    fn resolve_returns_none_when_modal_rows_cannot_be_rebuilt() {
        let reg = test_registry();
        reg.insert(
            "uuid-abc".into(),
            pending_with(vec![
                form("f1", "T", vec!["missing_input"], "go"),
                button("b1", "go"),
            ]),
        );
        assert!(resolve_form_modal_for_button(&reg, "interaction:uuid-abc:b1:go", OWNER).is_none());
    }

    #[test]
    fn resolve_requires_matching_action_names() {
        let reg = test_registry();
        reg.insert(
            "uuid-abc".into(),
            pending_with(vec![
                form("f1", "T", vec![], "submit_form"),
                button("b1", "open_form"),
            ]),
        );
        // ボタンの action が Form の action と違う → モーダルではなく通常のボタン応答。
        assert!(
            resolve_form_modal_for_button(&reg, "interaction:uuid-abc:b1:open_form", OWNER)
                .is_none()
        );
    }

    #[test]
    fn modal_custom_id_uses_the_uuid_part_of_the_surface_id() {
        assert_eq!(
            modal_custom_id("interaction:abc", "submit"),
            "interaction:abc:modal:submit"
        );
        // prefix が無い surface_id はそのまま使う（送信側の切り出しと同じ扱い）。
        assert_eq!(
            modal_custom_id("abc", "submit"),
            "interaction:abc:modal:submit"
        );
    }
}
