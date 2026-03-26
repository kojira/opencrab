use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A2UIコンポーネントのRust表現
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2uiComponent {
    pub id: String,
    #[serde(flatten)]
    pub component_type: A2uiComponentType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "component")]
pub enum A2uiComponentType {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        variant: Option<String>,
    },
    Button {
        text: String,
        action: A2uiAction,
        #[serde(skip_serializing_if = "Option::is_none")]
        style: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        emoji: Option<String>,
        #[serde(default)]
        disabled: bool,
    },
    Row {
        children: Vec<String>,
    },
    Column {
        children: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2uiAction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
}

/// レンダリング結果
#[derive(Debug, Clone)]
pub struct RenderedMessage {
    pub platform: String,
    pub message_id: Option<String>,
    pub channel_id: String,
}

/// レンダリング対象
#[derive(Debug, Clone)]
pub struct RenderTarget {
    pub channel_id: String,
    pub platform: String,
}

/// ユーザーの操作結果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserActionResponse {
    pub action_name: String,
    pub context: Option<serde_json::Value>,
    pub user_id: String,
}

/// A2UI userAction のRust表現
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2uiUserAction {
    pub surface_id: String,
    pub component_id: String,
    pub action_name: String,
    pub context: Option<serde_json::Value>,
    pub responder_id: String,
}

/// レンダリングエラー
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("Too many action rows: {0} (max 5)")]
    TooManyActionRows(usize),
    #[error("Component not found: {0}")]
    ComponentNotFound(String),
    #[error("Invalid component tree: {0}")]
    InvalidTree(String),
    #[error("Platform error: {0}")]
    PlatformError(String),
}

/// A2UI JSONをプラットフォーム固有のUIに変換・送信するtrait
#[async_trait]
pub trait UiRenderer: Send + Sync {
    async fn render(
        &self,
        surface_id: &str,
        components: &[A2uiComponent],
        channel: &RenderTarget,
    ) -> Result<RenderedMessage, RenderError>;

    async fn update_on_response(
        &self,
        rendered: &RenderedMessage,
        response: &UserActionResponse,
    ) -> Result<(), RenderError>;

    async fn update_on_timeout(
        &self,
        rendered: &RenderedMessage,
    ) -> Result<(), RenderError>;
}

/// 確認ダイアログ用のA2UIコンポーネントを生成するヘルパー
pub fn build_confirmation_components(
    prompt: &str,
    confirm_label: &str,
    cancel_label: &str,
) -> Vec<A2uiComponent> {
    vec![
        A2uiComponent {
            id: "root".into(),
            component_type: A2uiComponentType::Column {
                children: vec!["msg".into(), "actions".into()],
            },
        },
        A2uiComponent {
            id: "msg".into(),
            component_type: A2uiComponentType::Text {
                text: prompt.into(),
                variant: None,
            },
        },
        A2uiComponent {
            id: "actions".into(),
            component_type: A2uiComponentType::Row {
                children: vec!["btn_confirm".into(), "btn_cancel".into()],
            },
        },
        A2uiComponent {
            id: "btn_confirm".into(),
            component_type: A2uiComponentType::Button {
                text: confirm_label.into(),
                action: A2uiAction {
                    name: "confirm".into(),
                    context: Some(serde_json::json!({"value": true})),
                },
                style: Some("success".into()),
                emoji: Some("\u{2705}".into()),
                disabled: false,
            },
        },
        A2uiComponent {
            id: "btn_cancel".into(),
            component_type: A2uiComponentType::Button {
                text: cancel_label.into(),
                action: A2uiAction {
                    name: "confirm".into(),
                    context: Some(serde_json::json!({"value": false})),
                },
                style: Some("danger".into()),
                emoji: Some("\u{274c}".into()),
                disabled: false,
            },
        },
    ]
}
