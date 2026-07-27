# A2UI対応 UIコンポーネント設計書

## 概要

エージェントがA2UI（Agent to UI）プロトコルに基づいたJSON形式でUIを記述し、
プラットフォーム固有のレンダラー層がネイティブコンポーネントに変換して表示する。
これにより、Discord・Slack・Web等の複数プラットフォームに対応可能な設計を実現する。

### ユースケース

1. **GO確認ボタン**: サブタスク起動前に「GO？」ボタンを出してオーナーが押すまでブロック
2. **ツール実行確認**: 危険な操作の前に OK / NG ダイアログを出す
3. **モデル選択**: セレクトメニューでモデル一覧から選択させる
4. **フォーム入力**: モーダルで複数フィールドの入力を受け取る（設定変更など）

### Phase 1 スコープ

- **Button**（GO / Cancel 確認）+ **Text** のみ
- Discord レンダラーのみ実装
- セレクトメニュー、モーダルは Phase 2 以降

---

## 1. アーキテクチャ

### 1.1 A2UIとは

A2UIはGoogle製のオープンソース宣言的UIプロトコル（https://a2ui.org/）。
AIエージェントがJSON形式でUIを記述し、クライアント側がネイティブコンポーネントとしてレンダリングする。

**設計思想:**
- **セキュリティ重視**: 宣言的データであり、実行コードではない
- **LLMフレンドリー**: フラットなJSON構造で、ストリーミング生成に最適化
- **フレームワーク非依存**: React, Flutter, SwiftUI等で同じJSONを描画可能

### 1.2 なぜA2UIを採用するか

opencrabはフレームワークであり、特定のプラットフォームに依存しない設計が求められる。
Discord固有のAPIに直接依存するのではなく、A2UIの抽象層を挟むことで:

- **プラットフォーム追加が容易**: 新しいレンダラーを実装するだけ
- **エージェントのコードが変わらない**: UI記述は統一の `send_ui` アクション
- **テスト容易性**: レンダラーなしでA2UI JSONをバリデーション可能

### 1.3 全体アーキテクチャ

```
┌──────────────────────────────────┐
│          エージェント LLM          │
│   send_ui アクションを呼び出し      │
└──────────────┬───────────────────┘
               │ A2UI JSON
               ▼
┌──────────────────────────────────┐
│        UiRenderer trait          │
│   A2UI JSON → プラットフォーム固有  │
│   コンポーネントへの変換            │
├──────────┬───────────┬───────────┤
│ Discord  │  Slack    │   Web     │
│ Renderer │ Renderer  │ Renderer  │
│ (Phase1) │ (将来)    │ (将来)    │
└──────┬───┴───────────┴───────────┘
       │ Discord API
       ▼
┌──────────────────────────────────┐
│         Discord / etc.           │
│       ユーザーがUIを操作           │
└──────────────────────────────────┘
```

### 1.4 A2UIプロトコルバージョン

opencrabは **A2UI v0.9（Draft）** をベースとする。
v0.9はv0.8よりフラットな構造を採用しており、LLM生成・手動構築ともに扱いやすい。

ただし、opencrabの用途（確認ボタン・選択メニュー等の単純UI）では
A2UIのフル機能（ストリーミング、データバインディング、カタログネゴシエーション等）は不要。
**A2UIの構文・構造のサブセットを採用し、メッセージ型の互換性を維持する。**

---

## 2. opencrab A2UIサブセット

### 2.1 サポートするメッセージ型

A2UI v0.9の4メッセージ型のうち、以下の2つのみをサポート:

| メッセージ型 | サポート | 用途 |
|-------------|---------|------|
| `createSurface` | ❌ 不要 | opencrabではsurfaceの明示的作成は不要。`send_ui`呼び出し時に内部で自動作成 |
| `updateComponents` | ✅ | UIコンポーネントの定義・更新 |
| `updateDataModel` | ❌ 不要 | Phase 1では静的UI（リテラル値のみ）。データバインディングは将来拡張 |
| `deleteSurface` | ❌ 不要 | タイムアウト/応答時にレンダラーが内部的にUIを無効化 |

### 2.2 サポートするコンポーネント

Phase 1:

| A2UIコンポーネント | 説明 | Discord対応 |
|-------------------|------|-------------|
| `Text` | テキスト表示 | メッセージ本文 (`content`) |
| `Button` | ボタン | Discord Button (`CreateButton`) |
| `Row` | 水平レイアウト | Discord ActionRow |

Phase 2 以降:

| A2UIコンポーネント | 説明 | Discord対応 |
|-------------------|------|-------------|
| `DropdownInput` | ドロップダウン選択 | Discord Select Menu |
| `TextInput` | テキスト入力 | Discord Modal TextInput |
| `Column` | 垂直レイアウト | 複数ActionRow |
| `Card` | カードコンテナ | Embed |

### 2.3 コンポーネント定義（A2UI v0.9形式）

#### Text

```json
{
  "id": "prompt_text",
  "component": "Text",
  "text": "サブタスク「画像生成」を起動しますか？",
  "variant": "body"
}
```

| プロパティ | 型 | 必須 | 説明 |
|-----------|------|------|------|
| `text` | string | ✅ | 表示テキスト |
| `variant` | string | ❌ | `"h1"`, `"h2"`, `"h3"`, `"body"`, `"caption"` |

#### Button

```json
{
  "id": "btn_confirm",
  "component": "Button",
  "text": "GO",
  "action": { "name": "confirm" },
  "style": "success",
  "emoji": "✅"
}
```

| プロパティ | 型 | 必須 | 説明 |
|-----------|------|------|------|
| `text` | string | ✅ | ボタンラベル |
| `action` | object | ✅ | `{ "name": string, "context": object? }` |
| `style` | string | ❌ | `"primary"`, `"secondary"`, `"success"`, `"danger"` |
| `emoji` | string | ❌ | 絵文字（Discordレンダラーで使用） |
| `disabled` | boolean | ❌ | 無効化フラグ |

#### Row（水平レイアウト）

```json
{
  "id": "button_row",
  "component": "Row",
  "children": ["btn_confirm", "btn_cancel"]
}
```

| プロパティ | 型 | 必須 | 説明 |
|-----------|------|------|------|
| `children` | string[] | ✅ | 子コンポーネントIDの配列 |

### 2.4 A2UI JSON の完全な例（確認ダイアログ）

エージェントが `send_ui` を呼び出す際に内部で生成されるA2UI JSON:

```json
{
  "version": "v0.9",
  "updateComponents": {
    "surfaceId": "interaction:abc-123",
    "components": [
      {
        "id": "root",
        "component": "Column",
        "children": ["prompt_text", "button_row"]
      },
      {
        "id": "prompt_text",
        "component": "Text",
        "text": "サブタスク「画像生成」を起動しますか？",
        "variant": "body"
      },
      {
        "id": "button_row",
        "component": "Row",
        "children": ["btn_go", "btn_cancel"]
      },
      {
        "id": "btn_go",
        "component": "Button",
        "text": "GO",
        "action": { "name": "confirm", "context": { "value": true } },
        "style": "success",
        "emoji": "✅"
      },
      {
        "id": "btn_cancel",
        "component": "Button",
        "text": "キャンセル",
        "action": { "name": "confirm", "context": { "value": false } },
        "style": "danger",
        "emoji": "❌"
      }
    ]
  }
}
```

---

## 3. レンダラー層（trait設計）

### 3.1 `UiRenderer` trait

```rust
use async_trait::async_trait;

/// A2UI JSONをプラットフォーム固有のUIに変換・送信するtrait。
#[async_trait]
pub trait UiRenderer: Send + Sync {
    /// A2UIコンポーネントツリーをレンダリングし、プラットフォームに送信する。
    /// 
    /// 戻り値: 送信したメッセージの識別子（Discord: message_id, Web: element_id 等）
    async fn render(
        &self,
        surface_id: &str,
        components: &[A2uiComponent],
        channel: &RenderTarget,
    ) -> Result<RenderedMessage, RenderError>;

    /// ユーザー応答を受けてUIを更新する（ボタン無効化等）。
    async fn update_on_response(
        &self,
        rendered: &RenderedMessage,
        response: &UserActionResponse,
    ) -> Result<(), RenderError>;

    /// タイムアウト時にUIを無効化する。
    async fn update_on_timeout(
        &self,
        rendered: &RenderedMessage,
    ) -> Result<(), RenderError>;
}

/// A2UIコンポーネントのRust表現
pub struct A2uiComponent {
    pub id: String,
    pub component_type: A2uiComponentType,
}

pub enum A2uiComponentType {
    Text {
        text: String,
        variant: Option<String>,
    },
    Button {
        text: String,
        action: A2uiAction,
        style: Option<String>,
        emoji: Option<String>,
        disabled: bool,
    },
    Row {
        children: Vec<String>,  // child component IDs
    },
    Column {
        children: Vec<String>,
    },
    // Phase 2+
    // DropdownInput { ... },
    // TextInput { ... },
}

pub struct A2uiAction {
    pub name: String,
    pub context: Option<serde_json::Value>,
}

/// レンダリング結果（プラットフォーム固有の識別子を保持）
pub struct RenderedMessage {
    pub platform: String,           // "discord", "slack", "web"
    pub message_id: Option<String>, // Discord: message ID
    pub channel_id: String,
}

/// レンダリング対象
pub struct RenderTarget {
    pub channel_id: String,
    pub platform: String,
}

/// ユーザーの操作結果
pub struct UserActionResponse {
    pub action_name: String,
    pub context: Option<serde_json::Value>,
    pub user_id: String,
}
```

### 3.2 DiscordRenderer

A2UIコンポーネントツリーをDiscord Components（serenity 0.12）に変換する。

```rust
pub struct DiscordRenderer {
    http: Arc<serenity::http::Http>,
}

#[async_trait]
impl UiRenderer for DiscordRenderer {
    async fn render(
        &self,
        surface_id: &str,
        components: &[A2uiComponent],
        channel: &RenderTarget,
    ) -> Result<RenderedMessage, RenderError> {
        // 1. A2UIツリーからrootを見つける
        // 2. Textコンポーネント → message content
        // 3. Row[Button, ...] → CreateActionRow(Buttons)
        // 4. serenityでメッセージ送信
        
        let content = self.extract_text(components);
        let action_rows = self.build_action_rows(surface_id, components)?;
        
        let msg = CreateMessage::new()
            .content(content)
            .components(action_rows);
        
        let sent = channel_id.send_message(&self.http, msg).await?;
        
        Ok(RenderedMessage {
            platform: "discord".into(),
            message_id: Some(sent.id.to_string()),
            channel_id: channel.channel_id.clone(),
        })
    }
    
    // ...
}
```

### 3.3 A2UI → Discord マッピング

| A2UI コンポーネント | Discord 変換先 | 備考 |
|-------------------|---------------|------|
| `Text` | `CreateMessage::content()` | 複数Textは改行で結合 |
| `Text (variant: h1)` | `# テキスト`（markdown） | Discord markdownヘッダー |
| `Text (variant: h2)` | `## テキスト` | |
| `Button (style: primary)` | `ButtonStyle::Primary` (青) | |
| `Button (style: secondary)` | `ButtonStyle::Secondary` (灰) | |
| `Button (style: success)` | `ButtonStyle::Success` (緑) | GO用 |
| `Button (style: danger)` | `ButtonStyle::Danger` (赤) | Cancel用 |
| `Row` | `CreateActionRow::Buttons(...)` | |
| `Column` | 複数 `CreateActionRow` | 各子を個別ActionRowに |

### 3.4 Discord固有の制約をレンダラー層で吸収

| Discord制約 | A2UI側 | レンダラーでの対処 |
|------------|--------|-------------------|
| 1行 ActionRow に最大5ボタン | Row の children 数に制限なし | 5個超は自動的に次のActionRowに分割 |
| メッセージあたり最大5 ActionRow | Column の children 数に制限なし | 5行超はエラーを返す |
| 3秒以内にInteraction ACK必須 | A2UIはACKの概念なし | EventHandler内で即座にACK。レンダラー層の責務 |
| custom_id 最大100文字 | A2UI action.name に制限なし | `interaction:{uuid}:{action_name}` 形式に変換。100文字超はハッシュ化 |
| ボタンラベル最大80文字 | A2UI text に制限なし | 80文字で切り詰め |

---

## 4. エージェントが使うアクション設計

### 4.1 `send_ui` — 統一UIアクション

プラットフォーム非依存の統一アクション。
Discord固有の `discord_send_confirmation` 等は用意しない。

```json
{
  "name": "send_ui",
  "description": "A2UIコンポーネントで構成されたUIを送信し、ユーザーの応答を待機する",
  "parameters": {
    "type": "object",
    "properties": {
      "channel_id": {
        "type": "string",
        "description": "送信先チャンネルID"
      },
      "components": {
        "type": "array",
        "description": "A2UI v0.9 コンポーネント配列",
        "items": {
          "type": "object",
          "properties": {
            "id": { "type": "string" },
            "component": { "type": "string", "enum": ["Text", "Button", "Row", "Column"] },
            "text": { "type": "string" },
            "variant": { "type": "string" },
            "action": {
              "type": "object",
              "properties": {
                "name": { "type": "string" },
                "context": { "type": "object" }
              }
            },
            "style": { "type": "string" },
            "emoji": { "type": "string" },
            "children": { "type": "array", "items": { "type": "string" } }
          },
          "required": ["id", "component"]
        }
      },
      "timeout_secs": {
        "type": "integer",
        "description": "タイムアウト秒数（デフォルト: 300）"
      },
      "owner_only": {
        "type": "boolean",
        "description": "オーナーのみ操作可能か（デフォルト: true）"
      }
    },
    "required": ["channel_id", "components"]
  }
}
```

### 4.2 エージェントからの呼び出し例

#### 確認ボタン（Phase 1 ユースケース）

```json
{
  "name": "send_ui",
  "arguments": {
    "channel_id": "123456789",
    "components": [
      { "id": "root", "component": "Column", "children": ["msg", "actions"] },
      { "id": "msg", "component": "Text", "text": "サブタスク「画像生成」を起動しますか？" },
      { "id": "actions", "component": "Row", "children": ["go", "cancel"] },
      { "id": "go", "component": "Button", "text": "GO", "style": "success", "emoji": "✅", "action": { "name": "confirm", "context": { "value": true } } },
      { "id": "cancel", "component": "Button", "text": "キャンセル", "style": "danger", "emoji": "❌", "action": { "name": "confirm", "context": { "value": false } } }
    ],
    "timeout_secs": 300
  }
}
```

#### 便利ヘルパー（内部使用）

`send_ui` は汎用的だが、よくあるパターンには内部ヘルパー関数を用意する。
ただし外部インターフェース（エージェントが呼ぶアクション）は `send_ui` に統一する。

```rust
/// 確認ダイアログ用のA2UIコンポーネントを生成するヘルパー
pub fn build_confirmation_components(
    prompt: &str,
    confirm_label: &str,  // デフォルト: "GO"
    cancel_label: &str,   // デフォルト: "キャンセル"
) -> Vec<A2uiComponent> {
    vec![
        A2uiComponent { id: "root".into(), component_type: A2uiComponentType::Column { children: vec!["msg".into(), "actions".into()] } },
        A2uiComponent { id: "msg".into(), component_type: A2uiComponentType::Text { text: prompt.into(), variant: None } },
        A2uiComponent { id: "actions".into(), component_type: A2uiComponentType::Row { children: vec!["btn_confirm".into(), "btn_cancel".into()] } },
        A2uiComponent { id: "btn_confirm".into(), component_type: A2uiComponentType::Button {
            text: confirm_label.into(),
            action: A2uiAction { name: "confirm".into(), context: Some(json!({"value": true})) },
            style: Some("success".into()),
            emoji: Some("✅".into()),
            disabled: false,
        }},
        A2uiComponent { id: "btn_cancel".into(), component_type: A2uiComponentType::Button {
            text: cancel_label.into(),
            action: A2uiAction { name: "confirm".into(), context: Some(json!({"value": false})) },
            style: Some("danger".into()),
            emoji: Some("❌".into()),
            disabled: false,
        }},
    ]
}
```

### 4.3 戻り値

```json
{
  "success": true,
  "data": {
    "interaction_id": "uuid-xxxx",
    "surface_id": "interaction:uuid-xxxx",
    "status": "pending",
    "message": "UIを送信しました。ユーザーの応答を待機中..."
  }
}
```

> **重要**: このアクション自体は即座に返る（非同期待機）。ユーザーが応答すると、
> エージェントの会話に結果が注入される（§5参照）。

---

## 5. Interaction応答の処理フロー

### 5.1 全体フロー

```
                            ┌──────────────────┐
                            │  エージェントLLM  │
                            └────┬────────▲────┘
                                 │        │
                           send_ui     結果注入
                        (ツール呼び出し)   (LoopEvent)
                                 │        │
                            ┌────▼────────┴────┐
                            │   UiRenderer      │
                            │   + Registry      │
                            └────┬────────▲────┘
                                 │        │
                         A2UI→Discord  interaction_create
                          変換・送信    (EventHandler)
                                 │        │
                            ┌────▼────────┴────┐
                            │   Discord API     │
                            └────┬────────▲────┘
                                 │        │
                           表示      クリック
                                 │        │
                            ┌────▼────────┴────┐
                            │    ユーザー       │
                            └──────────────────┘
```

### 5.2 詳細ステップ（確認ボタンの場合）

1. **エージェントがツール呼び出し**: `send_ui` アクションを実行
2. **アクション実装**:
   - `interaction_id` (UUID) を生成
   - `surface_id` を `interaction:{uuid}` 形式で生成
   - A2UIコンポーネント配列をパース・バリデーション
   - DBの `pending_interactions` テーブルにレコードを挿入
   - `UiRenderer::render()` を呼び出し（DiscordRendererがA2UI→Discord変換・送信）
   - `PendingInteractionRegistry`（インメモリ）に登録
   - 即座に `GatewayActionResult` を返す（`status: "pending"`）
3. **ユーザーがボタンをクリック**
4. **EventHandler::interaction_create が発火**:
   - `custom_id` から `interaction_id` と `action_name` を抽出
   - owner_onlyチェック（§8参照）
   - Discord APIに即座にACK応答（`UPDATE_MESSAGE` でボタンを無効化）
   - A2UIの `userAction` 形式に変換
   - `LoopEvent::InteractionResponse` をイベントキューに送信
5. **メッセージループが処理**:
   - DBの `pending_interactions` を `responded` に更新
   - セッションログに `interaction_response` イベントを記録
   - エージェントを再呼び出し（subtask_completed と同じパターン）

### 5.3 userAction（A2UI準拠）

ユーザーの操作は、A2UIの `userAction` 形式に変換して内部処理する:

```json
{
  "userAction": {
    "surfaceId": "interaction:abc-123",
    "componentId": "btn_go",
    "action": {
      "name": "confirm",
      "context": { "value": true }
    }
  }
}
```

### 5.4 エージェントへの結果注入

subtask_completedと同じパターンで、`LoopEvent` として処理する。

```rust
enum LoopEvent {
    IncomingMessage(IncomingMessage),
    SubtaskCompleted { ... },
    // ★新規
    InteractionResponse {
        interaction_id: String,
        session_id: String,
        agent_id: String,
        channel_id: u64,
        channel_id_str: String,
        response: A2uiUserAction,
        is_dm: bool,
    },
}

/// A2UI userAction のRust表現
pub struct A2uiUserAction {
    pub surface_id: String,
    pub component_id: String,
    pub action_name: String,
    pub context: Option<serde_json::Value>,
    pub responder_id: String,
}
```

エージェントLLMには、以下のようなシステムメッセージとして注入する:

```
[interaction_response] ユーザーがUIに応答しました。
surface_id: interaction:abc-123
component_id: btn_go
action: confirm
context: {"value": true}
responder: user#123456
```

---

## 6. 待機の仕組み

### 6.1 方針: 非同期イベント駆動（ブロックしない）

**ブロッキング待機はしない。** 既存の subtask_completed パターンに合わせ、
イベント駆動で処理を再開する。

### 6.2 フロー

```
┌─────────────┐    ┌──────────────┐    ┌─────────────────┐
│ エージェント  │    │ アクション    │    │ PendingRegistry │
│ LLMループ    │    │ 実装         │    │ (DashMap)       │
└──────┬──────┘    └──────┬───────┘    └────────┬────────┘
       │                   │                     │
       │ send_ui           │                     │
       │──────────────────>│                     │
       │                   │ register(id, tx)    │
       │                   │────────────────────>│
       │                   │                     │
       │  {status:pending} │                     │
       │<──────────────────│                     │
       │                   │                     │
       │ (LLMループ終了)    │                     │
       │                   │                     │
    ... 時間経過 ...        │                     │
       │                   │                     │
       │                   │    ユーザーがクリック │
       │                   │    EventHandlerから  │
       │                   │    resolve(id, data) │
       │                   │<────────────────────│
       │                   │                     │
       │ LoopEvent::       │                     │
       │ InteractionResponse│                     │
       │<──────────────────│                     │
       │                   │                     │
       │ (LLMループ再開)    │                     │
```

### 6.3 PendingInteractionRegistry

```rust
use dashmap::DashMap;

/// Interactionの応答を中継するレジストリ。
///
/// interaction_id → PendingInteraction
pub type PendingInteractionRegistry = Arc<DashMap<String, PendingInteraction>>;

pub struct PendingInteraction {
    pub session_id: String,
    pub agent_id: String,
    pub channel_id: u64,
    pub channel_id_str: String,
    pub is_dm: bool,
    pub surface_id: String,
    pub a2ui_components: Vec<A2uiComponent>,  // タイムアウト時のUI更新に使用
    pub owner_discord_id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub timeout_secs: u64,
    pub rendered_message: RenderedMessage,     // レンダラーの送信結果
    /// イベントループへの通知チャンネル
    pub event_tx: mpsc::UnboundedSender<LoopEvent>,
}
```

### 6.4 タイムアウトの自動解決

`PendingInteractionRegistry` に登録された各Interactionに対して、
タイムアウト監視用の `tokio::spawn` タスクを起動する。

```rust
let registry = pending_registry.clone();
let renderer = renderer.clone();
let interaction_id = interaction_id.clone();
let timeout = Duration::from_secs(timeout_secs);

tokio::spawn(async move {
    tokio::time::sleep(timeout).await;
    if let Some((_, pending)) = registry.remove(&interaction_id) {
        // レンダラーでUIを無効化
        let _ = renderer.update_on_timeout(&pending.rendered_message).await;
        // タイムアウトイベントを送信
        let _ = pending.event_tx.send(LoopEvent::InteractionResponse {
            interaction_id: interaction_id.clone(),
            response: A2uiUserAction {
                surface_id: pending.surface_id.clone(),
                component_id: "_timeout".into(),
                action_name: "timeout".into(),
                context: None,
                responder_id: "system".into(),
            },
            // ... 他フィールド
        });
    }
});
```

---

## 7. タイムアウト処理

### 7.1 タイムアウト発生時の処理

1. **UIの更新**: `UiRenderer::update_on_timeout()` を呼び出し
   - DiscordRenderer: 元メッセージを編集し、ボタンを無効化 + テキスト追記
2. **DB更新**: `pending_interactions.status` を `"timeout"` に更新
3. **エージェント通知**: `A2uiUserAction { action_name: "timeout" }` をLoopEventに送信

### 7.2 タイムアウト値

| ユースケース | デフォルト | 最小 | 最大 |
|-------------|-----------|------|------|
| 確認ボタン | 300秒 (5分) | 10秒 | 3600秒 (1時間) |
| セレクトメニュー | 300秒 | 10秒 | 3600秒 |
| モーダル | 600秒 (10分) | 30秒 | 3600秒 |

### 7.3 タイムアウト後の表示（Discord）

```
┌──────────────────────────────────────────┐
│ サブタスク「画像生成」を起動しますか？       │
│                                          │
│  [GO] (無効)  [キャンセル] (無効)           │
│                                          │
│ ⏰ タイムアウトしました（5分経過）           │
└──────────────────────────────────────────┘
```

---

## 8. owner_onlyチェック

### 8.1 方針

デフォルトでは **オーナーのみ** がUIを操作できる。
他のユーザーがクリックした場合はエフェメラル（本人にしか見えない）メッセージで
「この操作はオーナーのみ実行可能です」と表示する。

### 8.2 実装（DiscordRenderer内）

```rust
async fn handle_component_interaction(
    &self,
    ctx: Context,
    component: ComponentInteraction,
) {
    let interaction_id = extract_interaction_id(&component.data.custom_id);

    let pending = match self.pending_registry.get(&interaction_id) {
        Some(p) => p,
        None => {
            component.create_response(&ctx.http, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("この操作は期限切れです")
                    .ephemeral(true)
            )).await.ok();
            return;
        }
    };

    // owner_only チェック
    let clicker_id = component.user.id.to_string();
    if clicker_id != pending.owner_discord_id {
        let is_trusted = {
            let conn = self.db.lock().unwrap();
            opencrab_db::queries::is_trusted_discord_user(
                &conn, &clicker_id, &pending.agent_id,
            )
        };

        if !is_trusted {
            component.create_response(&ctx.http, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ この操作はオーナーまたは信頼済みユーザーのみ実行可能です")
                    .ephemeral(true)
            )).await.ok();
            return;
        }
    }

    // 正常処理: ACK → userAction変換 → LoopEvent送信
}
```

### 8.3 権限レベル

| レベル | 操作可否 | 判定方法 |
|--------|----------|----------|
| オーナー | ✅ | `agent_discord_config.owner_discord_id` と一致 |
| 信頼済みユーザー | ✅ | `trusted_users` テーブルに登録あり |
| 一般ユーザー | ❌ | エフェメラルメッセージで拒否 |

### 8.4 将来拡張: `allow_users` パラメータ

`send_ui` のパラメータで操作可能ユーザーを指定するオプション（将来拡張）:

```json
{
  "allow_users": ["owner", "user:123456789"],
  "allow_roles": ["admin"]
}
```

---

## 9. DBテーブル設計

### 9.1 `pending_interactions` テーブル

```sql
CREATE TABLE IF NOT EXISTS pending_interactions (
    id TEXT PRIMARY KEY,                    -- UUID
    agent_id TEXT NOT NULL,                 -- エージェントID
    session_id TEXT NOT NULL,               -- セッションID（応答をどのセッションに返すか）
    channel_id TEXT NOT NULL,               -- チャンネルID（数値文字列）
    message_id TEXT,                        -- プラットフォームのメッセージID（UI更新に使用）
    platform TEXT NOT NULL DEFAULT 'discord',-- レンダリング先プラットフォーム
    surface_id TEXT NOT NULL,               -- A2UI surface ID
    a2ui_components_json TEXT NOT NULL,     -- 送信したA2UIコンポーネント（JSON）
    status TEXT NOT NULL DEFAULT 'pending', -- 'pending' | 'responded' | 'timeout' | 'cancelled'
    response_json TEXT,                     -- userAction応答内容（JSON）
    responder_id TEXT,                      -- 応答したユーザーのID
    owner_only INTEGER NOT NULL DEFAULT 1,  -- オーナーのみ操作可能か
    timeout_secs INTEGER NOT NULL DEFAULT 300,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    responded_at TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_pending_interactions_agent
    ON pending_interactions(agent_id, status);
CREATE INDEX IF NOT EXISTS idx_pending_interactions_session
    ON pending_interactions(session_id, status);
CREATE INDEX IF NOT EXISTS idx_pending_interactions_surface
    ON pending_interactions(surface_id);
```

### 9.2 旧設計からの変更点

| カラム | 旧設計 | 新設計 | 理由 |
|--------|--------|--------|------|
| `component_type` | `'confirmation' \| 'select_menu' \| 'modal'` | **削除** | A2UIコンポーネントツリーが型情報を持つ |
| `prompt` | 表示テキスト | **削除** | `a2ui_components_json` に含まれる |
| `options_json` | セレクトの選択肢等 | **削除** | 同上 |
| — | — | `platform` (新規) | マルチプラットフォーム対応 |
| — | — | `surface_id` (新規) | A2UI surface の追跡 |
| — | — | `a2ui_components_json` (新規) | 送信した完全なA2UIツリーを保存 |
| — | — | `owner_only` (新規) | owner_onlyフラグをDB化 |

### 9.3 response_json の形式（A2UI userAction準拠）

```json
{
  "surface_id": "interaction:abc-123",
  "component_id": "btn_go",
  "action_name": "confirm",
  "context": { "value": true }
}
```

タイムアウト時:
```json
{
  "surface_id": "interaction:abc-123",
  "component_id": "_timeout",
  "action_name": "timeout",
  "context": null
}
```

### 9.4 マイグレーション

```rust
conn.execute_batch(
    "CREATE TABLE IF NOT EXISTS pending_interactions (
        id TEXT PRIMARY KEY,
        agent_id TEXT NOT NULL,
        session_id TEXT NOT NULL,
        channel_id TEXT NOT NULL,
        message_id TEXT,
        platform TEXT NOT NULL DEFAULT 'discord',
        surface_id TEXT NOT NULL,
        a2ui_components_json TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'pending',
        response_json TEXT,
        responder_id TEXT,
        owner_only INTEGER NOT NULL DEFAULT 1,
        timeout_secs INTEGER NOT NULL DEFAULT 300,
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        responded_at TEXT,
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_pending_interactions_agent
        ON pending_interactions(agent_id, status);
    CREATE INDEX IF NOT EXISTS idx_pending_interactions_session
        ON pending_interactions(session_id, status);
    CREATE INDEX IF NOT EXISTS idx_pending_interactions_surface
        ON pending_interactions(surface_id);",
)?;
```

---

## 10. 既存コードへの影響範囲

### 10.1 変更が必要なファイル

| ファイル | 変更内容 | 影響度 |
|----------|----------|--------|
| `crates/gateway/src/adapters/discord.rs` | `interaction_create` ハンドラ追加、`PendingInteractionRegistry` を保持 | 中 |
| `crates/discord/src/gateway_actions/mod.rs` | `send_ui` アクション定義追加 | 中 |
| `crates/discord/src/gateway_actions/ui.rs` | ★新規: `send_ui` アクション実装 | 新規 |
| `crates/discord/src/renderer.rs` | ★新規: `DiscordRenderer` 実装（`UiRenderer` trait） | 新規 |
| `crates/discord/src/message_loop.rs` | `LoopEvent::InteractionResponse` バリアント追加 | 中 |
| `crates/discord/src/manager.rs` | `PendingInteractionRegistry` + `DiscordRenderer` の生成・注入 | 低 |
| `crates/discord/src/lib.rs` | 新しい型のエクスポート | 低 |
| `crates/core/src/a2ui.rs` | ★新規: `A2uiComponent`, `UiRenderer` trait, 共通型定義 | 新規 |
| `crates/db/src/schema.rs` | `pending_interactions` テーブル追加 | 低 |
| `crates/db/src/queries.rs` | Interaction CRUD関数追加 | 低 |

### 10.2 変更不要なファイル

| ファイル | 理由 |
|----------|------|
| `crates/llm/` | LLM層は無関係 |
| `crates/actions/` | GatewayActions経由で呼ばれるため直接変更不要 |
| `crates/server/` | GatewayActionsの定義追加で自動的に使える |
| `crates/gateway/src/traits.rs` | Gateway trait自体は変更不要 |

### 10.3 旧設計との差分まとめ

| 項目 | 旧設計 | A2UI対応設計 |
|------|--------|-------------|
| アクション名 | `discord_send_confirmation` / `discord_send_select_menu` / `discord_send_modal_trigger` | **`send_ui`** 1つに統一 |
| UI記述 | 各アクション固有のパラメータ | **A2UI v0.9 コンポーネントJSON** |
| プラットフォーム | Discord固定 | **UiRenderer traitで抽象化** |
| DB `component_type` | `confirmation / select_menu / modal` | **`a2ui_components_json`** にA2UIツリーをそのまま保存 |
| Interaction応答 | `InteractionResponseType` enum | **`A2uiUserAction`**（A2UI userAction準拠） |
| core crateへの影響 | なし | **`a2ui.rs` を追加**（trait + 共通型） |

### 10.4 新しい依存クレートの追加

追加不要。既存の `dashmap`, `tokio`, `serenity`, `chrono`, `uuid`, `serde_json` で実装可能。

---

## 実装優先度

| フェーズ | 内容 | 見積もり |
|----------|------|----------|
| Phase 1 | `send_ui` アクション + DiscordRenderer（Button + Text のみ） | 中 |
| Phase 2 | DropdownInput（セレクトメニュー）対応 | 小 |
| Phase 3 | TextInput + Modal対応 | 中 |
| Phase 4 | データバインディング、`allow_users` 拡張 | 中 |
| Phase 5 | 追加レンダラー（Slack, Web等） | 大 |

Phase 1 だけで主要ユースケース（GO確認、ツール実行確認）をカバーできるため、
最小MVPとしてPhase 1から着手するのが推奨。

---

## Appendix A: A2UIコンポーネント ↔ Discord Component マッピング表

### Phase 1（Button + Text）

| A2UI | A2UIプロパティ | Discord | Discord API |
|------|---------------|---------|-------------|
| `Text` | `text`, `variant` | メッセージ content | `CreateMessage::content()` |
| `Text (variant: h1)` | `variant: "h1"` | `# text` (markdown) | — |
| `Text (variant: h2)` | `variant: "h2"` | `## text` | — |
| `Text (variant: h3)` | `variant: "h3"` | `### text` | — |
| `Text (variant: body)` | `variant: "body"` | プレーンテキスト | — |
| `Text (variant: caption)` | `variant: "caption"` | `-# text` (small text) | — |
| `Button` | `text`, `action`, `style`, `emoji` | Discord Button | `CreateButton::new(custom_id)` |
| `Button (style: primary)` | `style: "primary"` | 青ボタン | `ButtonStyle::Primary` |
| `Button (style: secondary)` | `style: "secondary"` | 灰ボタン | `ButtonStyle::Secondary` |
| `Button (style: success)` | `style: "success"` | 緑ボタン | `ButtonStyle::Success` |
| `Button (style: danger)` | `style: "danger"` | 赤ボタン | `ButtonStyle::Danger` |
| `Row` | `children` | ActionRow | `CreateActionRow::Buttons(...)` |
| `Column` | `children` | 複数 ActionRow | 各子を個別ActionRowに展開 |

### Phase 2+（計画）

| A2UI | Discord | 備考 |
|------|---------|------|
| `DropdownInput` | Select Menu | `CreateSelectMenu` |
| `TextInput` | Modal TextInput | `CreateModal` 経由 |
| `Card` | Embed | `CreateEmbed` |
| `Image` | Embed image / Attachment | — |

## Appendix B: custom_id の命名規則

```
interaction:{uuid}:{action_name}
```

例:
- `interaction:abc123:confirm` — 確認ボタン（action.name = "confirm"）
- `interaction:abc123:select` — セレクトメニュー（action.name = "select"）
- `interaction:abc123:modal_submit` — モーダル送信

`{uuid}` 部分で `pending_interactions` テーブルのレコードを検索し、
`{action_name}` 部分でA2UIの `userAction.action.name` にマッピングする。

## Appendix C: A2UI v0.9 メッセージ型リファレンス

opencrabが参照するA2UI v0.9メッセージ構造（抜粋）:

### createSurface（参考: opencrabでは内部自動生成）

```json
{
  "version": "v0.9",
  "createSurface": {
    "surfaceId": "interaction:abc-123",
    "catalogId": "https://opencrab.dev/a2ui/v1/basic"
  }
}
```

### updateComponents（opencrabが実際に使用）

```json
{
  "version": "v0.9",
  "updateComponents": {
    "surfaceId": "interaction:abc-123",
    "components": [
      { "id": "root", "component": "Column", "children": ["msg", "actions"] },
      { "id": "msg", "component": "Text", "text": "確認メッセージ" },
      { "id": "actions", "component": "Row", "children": ["btn1", "btn2"] },
      { "id": "btn1", "component": "Button", "text": "GO", "style": "success", "action": { "name": "confirm" } },
      { "id": "btn2", "component": "Button", "text": "Cancel", "style": "danger", "action": { "name": "cancel" } }
    ]
  }
}
```

### userAction（ユーザー応答 → エージェントへ注入）

```json
{
  "userAction": {
    "surfaceId": "interaction:abc-123",
    "componentId": "btn1",
    "action": {
      "name": "confirm",
      "context": {}
    }
  }
}
```
