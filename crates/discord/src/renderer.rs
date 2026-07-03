//! A2UI DiscordRenderer — A2UIコンポーネントツリーをDiscordメッセージとして描画する。

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use serenity::all::{
    ActionRowComponent, ButtonKind, ButtonStyle, ChannelId, CreateActionRow, CreateButton,
    CreateInputText, CreateMessage, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption,
    EditMessage, Http, InputTextStyle, MessageId, ReactionType,
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

        // SelectMenu は Column の子として直接並ぶ場合と Row の子として並ぶ場合があり、
        // 同じ id が両方に現れると二重に ActionRow 化して custom_id が重複する。
        let mut emitted_select_menu_ids: HashSet<String> = HashSet::new();

        for child_id in child_ids {
            if emitted_select_menu_ids.contains(child_id) {
                continue;
            }
            if let Some(comp) = components.iter().find(|c| c.id == child_id) {
                match &comp.component_type {
                    A2uiComponentType::Row { children } => {
                        // Check if Row contains a SelectMenu
                        let has_select = children.iter().any(|cid| {
                            components.iter().any(|c| {
                                c.id == *cid
                                    && matches!(
                                        &c.component_type,
                                        A2uiComponentType::SelectMenu { .. }
                                    )
                            })
                        });

                        if has_select {
                            // SelectMenu: 1 ActionRow per SelectMenu (Discord limitation)
                            for cid in children {
                                if emitted_select_menu_ids.contains(cid.as_str()) {
                                    continue;
                                }
                                if let Some(select_menu) =
                                    self.build_select_menu(surface_id, cid, components)?
                                {
                                    emitted_select_menu_ids.insert(cid.clone());
                                    action_rows.push(CreateActionRow::SelectMenu(select_menu));
                                }
                            }
                        } else {
                            let buttons = self.build_buttons(surface_id, children, components)?;
                            // Discord制限: 1つのActionRowに最大5ボタン
                            for chunk in buttons.chunks(5) {
                                action_rows.push(CreateActionRow::Buttons(chunk.to_vec()));
                            }
                        }
                    }
                    A2uiComponentType::SelectMenu { .. } => {
                        // Direct SelectMenu child of Column
                        if let Some(select_menu) =
                            self.build_select_menu(surface_id, child_id, components)?
                        {
                            emitted_select_menu_ids.insert(child_id.to_string());
                            action_rows.push(CreateActionRow::SelectMenu(select_menu));
                        }
                    }
                    _ => {}
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
                // Discord制限: custom_idは100文字まで（文字境界で切り詰めてpanicを回避）
                let custom_id = if custom_id.chars().count() > 100 {
                    custom_id.chars().take(100).collect::<String>()
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

    /// A2UI SelectMenuコンポーネントからCreateSelectMenuを構築する。
    ///
    /// custom_id形式: `interaction:{uuid_part}:{component_id}:{action_name}`
    fn build_select_menu(
        &self,
        surface_id: &str,
        component_id: &str,
        components: &[A2uiComponent],
    ) -> Result<Option<CreateSelectMenu>, RenderError> {
        let comp = components
            .iter()
            .find(|c| c.id == component_id)
            .ok_or_else(|| RenderError::ComponentNotFound(component_id.to_string()))?;

        if let A2uiComponentType::SelectMenu {
            options,
            placeholder,
            min_values,
            max_values,
            action,
        } = &comp.component_type
        {
            let uuid_part = surface_id
                .strip_prefix("interaction:")
                .unwrap_or(surface_id);

            let custom_id = format!("interaction:{}:{}:{}", uuid_part, component_id, action.name);
            let custom_id = if custom_id.chars().count() > 100 {
                custom_id.chars().take(100).collect::<String>()
            } else {
                custom_id
            };

            let menu_options: Vec<CreateSelectMenuOption> = options
                .iter()
                .map(|opt| {
                    let mut menu_opt = CreateSelectMenuOption::new(&opt.label, &opt.value);
                    if let Some(desc) = &opt.description {
                        menu_opt = menu_opt.description(desc);
                    }
                    if let Some(emoji_str) = &opt.emoji {
                        menu_opt = menu_opt.emoji(ReactionType::Unicode(emoji_str.clone()));
                    }
                    if opt.default {
                        menu_opt = menu_opt.default_selection(true);
                    }
                    menu_opt
                })
                .collect();

            let mut menu = CreateSelectMenu::new(
                &custom_id,
                CreateSelectMenuKind::String {
                    options: menu_options,
                },
            );

            if let Some(ph) = placeholder {
                menu = menu.placeholder(ph);
            }
            if let Some(min) = min_values {
                menu = menu.min_values(*min as u8);
            }
            if let Some(max) = max_values {
                menu = menu.max_values(*max as u8);
            }

            Ok(Some(menu))
        } else {
            Ok(None)
        }
    }

    /// A2UI FormコンポーネントからModal用のActionRow（InputText）リストを構築する。
    ///
    /// Form内のTextInputコンポーネントをCreateInputTextに変換し、
    /// 各InputTextを個別のActionRowに格納する（Discordの制約）。
    pub fn build_modal_action_rows(
        &self,
        form: &A2uiComponent,
        components: &[A2uiComponent],
    ) -> Result<Vec<CreateActionRow>, RenderError> {
        let (_title, children, _action) = match &form.component_type {
            A2uiComponentType::Form {
                title,
                children,
                action,
            } => (title, children, action),
            _ => {
                return Err(RenderError::InvalidTree(
                    "Expected Form component".to_string(),
                ))
            }
        };

        let mut rows = Vec::new();
        for child_id in children {
            let comp = components
                .iter()
                .find(|c| c.id == *child_id)
                .ok_or_else(|| RenderError::ComponentNotFound(child_id.clone()))?;

            if let A2uiComponentType::TextInput {
                label,
                placeholder,
                min_length,
                max_length,
                required,
                style,
            } = &comp.component_type
            {
                let input_style = match style.as_deref() {
                    Some("paragraph") => InputTextStyle::Paragraph,
                    _ => InputTextStyle::Short,
                };

                let mut input = CreateInputText::new(input_style, label, &comp.id);
                input = input.required(*required);
                if let Some(ph) = placeholder {
                    input = input.placeholder(ph);
                }
                if let Some(min) = min_length {
                    input = input.min_length(*min as u16);
                }
                if let Some(max) = max_length {
                    input = input.max_length(*max as u16);
                }
                rows.push(CreateActionRow::InputText(input));
            }
        }

        if rows.len() > 5 {
            return Err(RenderError::TooManyActionRows(rows.len()));
        }

        Ok(rows)
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

        // 全ボタン・セレクトメニューを無効化したActionRowを再構築
        let mut new_rows = Vec::new();
        for row in &original.components {
            let mut new_buttons = Vec::new();
            let mut select_menu = None;
            for comp in &row.components {
                match comp {
                    ActionRowComponent::Button(btn) => {
                        if let ButtonKind::NonLink { custom_id, style } = &btn.data {
                            let mut new_btn =
                                CreateButton::new(custom_id).style(*style).disabled(true);
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
                    ActionRowComponent::SelectMenu(menu) => {
                        if let Some(ref cid) = menu.custom_id {
                            let options: Vec<CreateSelectMenuOption> = menu
                                .options
                                .iter()
                                .map(|opt| {
                                    let mut o = CreateSelectMenuOption::new(&opt.label, &opt.value);
                                    if let Some(ref desc) = opt.description {
                                        o = o.description(desc);
                                    }
                                    o
                                })
                                .collect();
                            let mut new_menu = CreateSelectMenu::new(
                                cid,
                                CreateSelectMenuKind::String { options },
                            )
                            .disabled(true);
                            if let Some(ref ph) = menu.placeholder {
                                new_menu = new_menu.placeholder(ph);
                            }
                            select_menu = Some(new_menu);
                        }
                    }
                    _ => {}
                }
            }
            if let Some(menu) = select_menu {
                new_rows.push(CreateActionRow::SelectMenu(menu));
            } else if !new_buttons.is_empty() {
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
        let r = test_renderer();
        let comps = vec![
            form("form1", "Test Form", vec!["input1", "input2"], "submit"),
            text_input("input1", "Name", Some("short"), true),
            text_input("input2", "Description", Some("paragraph"), false),
        ];
        let form_comp = &comps[0];
        let rows = r.build_modal_action_rows(form_comp, &comps).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn build_modal_action_rows_too_many_inputs() {
        let r = test_renderer();
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
        let result = r.build_modal_action_rows(&comps[0], &comps);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            RenderError::TooManyActionRows(6)
        ));
    }

    #[test]
    fn build_modal_action_rows_missing_child() {
        let r = test_renderer();
        let comps = vec![form("form1", "Test", vec!["missing_input"], "submit")];
        let result = r.build_modal_action_rows(&comps[0], &comps);
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
}
