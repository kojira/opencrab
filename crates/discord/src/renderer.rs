//! A2UI DiscordRenderer — A2UIコンポーネントツリーをDiscordメッセージとして描画する。

use std::sync::Arc;

use async_trait::async_trait;
use serenity::all::{
    ActionRowComponent, ButtonKind, ButtonStyle, ChannelId, CreateActionRow, CreateButton,
    CreateMessage, EditMessage, Http, MessageId, ReactionType,
};
use tracing::debug;

use opencrab_core::a2ui::{
    A2uiComponent, A2uiComponentType, RenderError, RenderTarget, RenderedMessage, UiRenderer,
    UserActionResponse,
};

/// Discord向けA2UIレンダラー。
///
/// A2UIコンポーネントツリーをDiscordのメッセージ + ボタンに変換して送信する。
pub struct DiscordRenderer {
    pub http: Arc<Http>,
}

impl DiscordRenderer {
    pub fn new(http: Arc<Http>) -> Self {
        Self { http }
    }

    /// A2UIコンポーネントツリーからテキストコンテンツを抽出する。
    ///
    /// ルートColumnがあればその子要素順、なければ全Textコンポーネントを順に連結する。
    /// variant に応じてDiscord Markdownフォーマットを適用:
    /// - h1 → `# text`, h2 → `## text`, h3 → `### text`
    /// - caption → `-# text`, body/None → plain
    fn extract_text(&self, components: &[A2uiComponent]) -> String {
        let root = components
            .iter()
            .find(|c| matches!(&c.component_type, A2uiComponentType::Column { .. }));

        let child_ids: Vec<&str> = if let Some(root) = root {
            if let A2uiComponentType::Column { children } = &root.component_type {
                children.iter().map(|s| s.as_str()).collect()
            } else {
                vec![]
            }
        } else {
            components.iter().map(|c| c.id.as_str()).collect()
        };

        let mut texts = Vec::new();
        for id in child_ids {
            if let Some(comp) = components.iter().find(|c| c.id == id) {
                if let A2uiComponentType::Text { text, variant } = &comp.component_type {
                    let formatted = match variant.as_deref() {
                        Some("h1") => format!("# {}", text),
                        Some("h2") => format!("## {}", text),
                        Some("h3") => format!("### {}", text),
                        Some("caption") => format!("-# {}", text),
                        _ => text.clone(),
                    };
                    texts.push(formatted);
                }
            }
        }
        texts.join("\n")
    }

    /// A2UIコンポーネントツリーからActionRowを構築する。
    ///
    /// ルートColumnの子からRowコンポーネントを抽出し、各RowのボタンをActionRowに変換する。
    /// 1つのRowに6個以上のボタンがある場合は5個ずつに分割する。
    /// 合計5つを超えるActionRowはエラー。
    fn build_action_rows(
        &self,
        surface_id: &str,
        components: &[A2uiComponent],
    ) -> Result<Vec<CreateActionRow>, RenderError> {
        let mut action_rows = Vec::new();

        let root = components
            .iter()
            .find(|c| matches!(&c.component_type, A2uiComponentType::Column { .. }));

        let child_ids: Vec<&str> = if let Some(root) = root {
            if let A2uiComponentType::Column { children } = &root.component_type {
                children.iter().map(|s| s.as_str()).collect()
            } else {
                vec![]
            }
        } else {
            components.iter().map(|c| c.id.as_str()).collect()
        };

        for child_id in child_ids {
            if let Some(comp) = components.iter().find(|c| c.id == child_id) {
                if let A2uiComponentType::Row { children } = &comp.component_type {
                    let buttons = self.build_buttons(surface_id, children, components)?;
                    // Discord制限: 1つのActionRowに最大5ボタン
                    for chunk in buttons.chunks(5) {
                        action_rows.push(CreateActionRow::Buttons(chunk.to_vec()));
                    }
                }
            }
        }

        if action_rows.len() > 5 {
            return Err(RenderError::TooManyActionRows(action_rows.len()));
        }

        Ok(action_rows)
    }

    /// ボタンコンポーネントからCreateButtonを構築する。
    ///
    /// custom_id形式: `interaction:{uuid_part}:{button_id}:{action_name}`
    /// - ラベルは80文字で切り詰め
    /// - custom_idは100文字で切り詰め
    fn build_buttons(
        &self,
        surface_id: &str,
        button_ids: &[String],
        components: &[A2uiComponent],
    ) -> Result<Vec<CreateButton>, RenderError> {
        let mut buttons = Vec::new();

        // surface_idからUUID部分を抽出 (形式: "interaction:{uuid}")
        let uuid_part = surface_id
            .strip_prefix("interaction:")
            .unwrap_or(surface_id);

        for btn_id in button_ids {
            let comp = components
                .iter()
                .find(|c| c.id == *btn_id)
                .ok_or_else(|| RenderError::ComponentNotFound(btn_id.clone()))?;

            if let A2uiComponentType::Button {
                text,
                action,
                style,
                emoji,
                disabled,
            } = &comp.component_type
            {
                // custom_id: interaction:{uuid}:{button_id}:{action_name}
                let custom_id = format!("interaction:{}:{}:{}", uuid_part, btn_id, action.name);
                // Discord制限: custom_idは100文字まで
                let custom_id = if custom_id.len() > 100 {
                    custom_id[..100].to_string()
                } else {
                    custom_id
                };

                // ラベルは80文字まで（超過時は末尾を"..."に）
                let label = if text.chars().count() > 80 {
                    text.chars().take(77).collect::<String>() + "..."
                } else {
                    text.clone()
                };

                let button_style = match style.as_deref() {
                    Some("primary") => ButtonStyle::Primary,
                    Some("secondary") => ButtonStyle::Secondary,
                    Some("success") => ButtonStyle::Success,
                    Some("danger") => ButtonStyle::Danger,
                    _ => ButtonStyle::Primary,
                };

                let mut btn = CreateButton::new(&custom_id)
                    .label(&label)
                    .style(button_style)
                    .disabled(*disabled);

                if let Some(emoji_str) = emoji {
                    btn = btn.emoji(ReactionType::Unicode(emoji_str.clone()));
                }

                buttons.push(btn);
            }
        }
        Ok(buttons)
    }

    /// メッセージのボタンを全て無効化して編集する内部ヘルパー。
    ///
    /// 元メッセージを取得し、全ボタンを disabled=true で再構築して編集する。
    /// `append_text` が指定されていればコンテンツに追記する。
    async fn disable_buttons(
        &self,
        rendered: &RenderedMessage,
        append_text: Option<&str>,
    ) -> Result<(), RenderError> {
        let channel_id: u64 = rendered
            .channel_id
            .parse()
            .map_err(|e| RenderError::PlatformError(format!("Invalid channel_id: {}", e)))?;
        let message_id: u64 = rendered
            .message_id
            .as_ref()
            .ok_or_else(|| RenderError::PlatformError("No message_id".into()))?
            .parse()
            .map_err(|e| RenderError::PlatformError(format!("Invalid message_id: {}", e)))?;

        let channel = ChannelId::new(channel_id);
        let msg_id = MessageId::new(message_id);

        // 元メッセージを取得してコンポーネント情報を読む
        let original = channel
            .message(&self.http, msg_id)
            .await
            .map_err(|e| RenderError::PlatformError(format!("Failed to fetch message: {}", e)))?;

        let mut edit = EditMessage::new();

        // 全ボタンを無効化したActionRowを再構築
        let mut new_rows = Vec::new();
        for row in &original.components {
            let mut new_buttons = Vec::new();
            for comp in &row.components {
                if let ActionRowComponent::Button(btn) = comp {
                    if let ButtonKind::NonLink { custom_id, style } = &btn.data {
                        let mut new_btn = CreateButton::new(custom_id)
                            .style(*style)
                            .disabled(true);
                        if let Some(label) = &btn.label {
                            new_btn = new_btn.label(label);
                        }
                        if let Some(emoji) = &btn.emoji {
                            let emoji: ReactionType = emoji.clone();
                            new_btn = new_btn.emoji(emoji);
                        }
                        new_buttons.push(new_btn);
                    }
                }
            }
            if !new_buttons.is_empty() {
                new_rows.push(CreateActionRow::Buttons(new_buttons));
            }
        }

        edit = edit.components(new_rows);

        if let Some(extra) = append_text {
            let new_content = format!("{}\n\n{}", original.content, extra);
            edit = edit.content(new_content);
        }

        channel
            .edit_message(&self.http, msg_id, edit)
            .await
            .map_err(|e| RenderError::PlatformError(format!("Failed to edit message: {}", e)))?;

        Ok(())
    }
}

#[async_trait]
impl UiRenderer for DiscordRenderer {
    async fn render(
        &self,
        surface_id: &str,
        components: &[A2uiComponent],
        channel: &RenderTarget,
    ) -> Result<RenderedMessage, RenderError> {
        let content = self.extract_text(components);
        let action_rows = self.build_action_rows(surface_id, components)?;

        let channel_id: u64 = channel
            .channel_id
            .parse()
            .map_err(|e| RenderError::PlatformError(format!("Invalid channel_id: {}", e)))?;

        let mut msg = CreateMessage::new();
        if !content.is_empty() {
            msg = msg.content(content);
        }
        if !action_rows.is_empty() {
            msg = msg.components(action_rows);
        }

        debug!(
            channel_id = %channel_id,
            "A2UI render: sending message to Discord"
        );

        let sent = ChannelId::new(channel_id)
            .send_message(&self.http, msg)
            .await
            .map_err(|e| RenderError::PlatformError(format!("Failed to send message: {}", e)))?;

        Ok(RenderedMessage {
            platform: "discord".into(),
            message_id: Some(sent.id.to_string()),
            channel_id: channel.channel_id.clone(),
        })
    }

    async fn update_on_response(
        &self,
        rendered: &RenderedMessage,
        _response: &UserActionResponse,
    ) -> Result<(), RenderError> {
        self.disable_buttons(rendered, None).await
    }

    async fn update_on_timeout(&self, rendered: &RenderedMessage) -> Result<(), RenderError> {
        self.disable_buttons(rendered, Some("⏰ タイムアウトしました"))
            .await
    }
}

#[cfg(test)]
mod tests {
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
        assert_ne!(id1, id2, "custom_ids must be unique even with same action name");
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
        let mut children: Vec<&str> = row_ids.iter().map(|s| s.as_str()).collect();
        let mut comps = vec![column(
            "root",
            children.clone(),
        )];
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
}
