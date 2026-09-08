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
    ///
    /// **部品ツリーだけの純関数**（`self.http` を触らない）なので関連関数にしてある。
    /// これにより、保留状態（`opencrab_core::a2ui::PendingInteraction`）に描画物を
    /// 持たせず、ボタン押下時に部品ツリーから組み直せる（#156 S3 —
    /// `form_modal::resolve_form_modal_for_button`）。
    pub fn build_modal_action_rows(
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
mod tests;
