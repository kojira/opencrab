use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
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
    SelectMenu {
        options: Vec<SelectOption>,
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        min_values: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_values: Option<u32>,
        action: A2uiAction,
    },
    TextInput {
        label: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        min_length: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_length: Option<u32>,
        #[serde(default = "default_true")]
        required: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        style: Option<String>,
    },
    Form {
        title: String,
        children: Vec<String>,
        action: A2uiAction,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectOption {
    pub label: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    #[serde(default)]
    pub default: bool,
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

    async fn update_on_timeout(&self, rendered: &RenderedMessage) -> Result<(), RenderError>;
}

/// ユーザーの応答を待っている A2UI インタラクションの保留状態。
///
/// **コアの型だけで構成する**。transport のチャンネル識別子・イベントループへの
/// チャンネル・UIライブラリの型を一切持たない: 描画先は [`RenderTarget`]、応答の
/// 戻し先は [`UiResponseSink`] が担い、**描画物は保持しない**。
///
/// 描画物を持たないのが要点で、transport が再描画に必要とするもの（例: Discord の
/// Form モーダルの入力欄）はすべて [`Self::a2ui_components`]（部品ツリー）と
/// [`Self::surface_id`] から再導出できる。ここに描画物を置くと transport の型か、
/// それを避けるための型消去が混入する。
pub struct PendingInteraction {
    /// 応答を受けて再開する親セッション ID。
    pub session_id: String,
    /// セッションのエージェント ID。
    pub agent_id: String,
    /// 描画先（platform + channel）。
    pub target: RenderTarget,
    pub surface_id: String,
    pub a2ui_components: Vec<A2uiComponent>,
    /// オーナー限定操作のためのオーナー識別子（transport のユーザー ID）。
    ///
    /// **空文字なら owner 判定を行わない**（誰でも操作できる）。この既定は既存挙動
    /// なので、配線側で空文字を渡すと権限ゲートが無効化される点に注意。
    pub owner_id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub timeout_secs: u64,
    pub rendered_message: RenderedMessage,
}

/// 保留中 A2UI インタラクションを interaction_id で引く登録簿。
pub type PendingInteractionRegistry = Arc<DashMap<String, PendingInteraction>>;

/// UI 応答（クリック・選択・モーダル送信・タイムアウト）を親セッションへ通知する
/// 最小ペイロード。
///
/// `SubtaskSettled`（subtask 完了の受け口）と同じ方針で、**本文は運ばない**。
/// ここにあるのは「どのセッションを再開するか」と「何が起きたか」だけで、会話へ
/// 再注入する本文は受け取り側が DB から読み直す。
#[derive(Debug, Clone)]
pub struct UiResponseEvent {
    /// 応答が返ったインタラクション ID。
    pub interaction_id: String,
    /// 再開する親セッション ID。
    pub session_id: String,
    /// 親セッションのエージェント ID。
    pub agent_id: String,
    /// 描画先（platform + channel）。session_id から返信先を導出できない
    /// gateway のためにここへ載せる。
    pub target: RenderTarget,
    /// ユーザーの操作内容（タイムアウトは `action_name = "timeout"`）。
    pub response: A2uiUserAction,
}

/// UI 応答通知の抽象（transport のイベントループへの直接依存を置換する）。
///
/// `SubtaskCompletionSink` とまったく同型の設計: 汎用層は
/// `Arc<dyn UiResponseSink>` を保持し、`on_ui_response` を呼ぶだけで transport の
/// イベント型を知らない。sink 実装が「resume ＋ その gateway の配送口」を担う。
pub trait UiResponseSink: Send + Sync {
    /// 応答を受けて当該セッションのエージェントを再開するトリガ。
    fn on_ui_response(&self, ev: UiResponseEvent);
}

/// 保留インタラクションの管理に必要な 1 組（登録簿 + 応答の受け口）。
///
/// 片方だけ配線された状態を型で作れないようにまとめてある（登録したのに応答を
/// 戻せない、あるいは受け口はあるのに登録されない、を防ぐ）。
#[derive(Clone)]
pub struct PendingUiSurface {
    pub registry: PendingInteractionRegistry,
    pub sink: Arc<dyn UiResponseSink>,
}

/// transport が提供する A2UI の描画面。
///
/// gateway 非依存層の `send_ui` はこれだけを使って UI を送る。`pending` が `None` の
/// transport では**描画のみ**行い、保留登録もタイムアウト監視もしない（応答を
/// 受け取る経路が無いため）。
#[derive(Clone)]
pub struct A2uiSurface {
    /// A2UI をこの transport の UI へ描画する実装。
    pub renderer: Arc<dyn UiRenderer>,
    /// プラットフォーム名（`RenderTarget.platform` と DB の platform 列に載る）。
    pub platform: String,
    /// オーナー限定操作の判定に使う識別子。**空文字なら判定しない**。
    pub owner_id: String,
    /// 応答を受け取れる transport のみ `Some`。
    pub pending: Option<PendingUiSurface>,
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
