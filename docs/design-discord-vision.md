# Design: Discord 画像 Vision 対応

**作成日**: 2026-03-22  
**ステータス**: 設計段階  
**対象モデル**: claude-sonnet-4-6（マルチモーダル対応済み）

---

## 1. 現状分析

### 1.1 Discord の `Message.attachments` 構造

serenity の `Message` 型には以下のフィールドが存在する:

```rust
// serenity::model::channel::Message
pub attachments: Vec<Attachment>

// serenity::model::channel::Attachment
pub struct Attachment {
    pub id: AttachmentId,
    pub url: String,           // CDN URL (例: https://cdn.discordapp.com/attachments/...)
    pub proxy_url: String,     // Discord プロキシ URL
    pub filename: String,      // ファイル名
    pub content_type: Option<String>,  // MIME type (例: "image/png")
    pub size: u32,             // バイト数
    pub width: Option<u64>,    // 画像の場合のみ
    pub height: Option<u64>,   // 画像の場合のみ
    // ...
}
```

画像の判定方法:
- `content_type` が `"image/"` プレフィックスを持つ
- または `width`/`height` が `Some` で `filename` が画像拡張子

### 1.2 現在どこで画像が落とされているか

**Drop箇所1: `crates/discord/src/gateway.rs` の `EventHandler::message()`**

```rust
// 現在のコード（attachments を完全無視）
let mut incoming = IncomingMessage::new(
    MessageSource::Discord { guild_id, channel_id },
    MessageContent::text(&msg.content),  // ← テキストのみ。attachments は無視
    sender,
);
```

serenity の `msg.attachments` が存在していても、`MessageContent::text()` で変換する際に全て捨てられる。

**Drop箇所2: `crates/discord/src/message_loop.rs`**

```rust
let text = match incoming.content.as_text() {
    Some(t) if !t.is_empty() => t.to_string(),
    _ => continue,  // ← Multi / Image コンテンツは None → スキップ（処理されない）
};
```

`MessageContent::Multi` や `MessageContent::Image` に対して `as_text()` は `None` を返すため、
画像のみのメッセージ（テキスト本文なし）は完全にスキップされる。

### 1.3 LLM へのメッセージ構築フロー（現状）

```
Discord Message
  └─ msg.content (String)
  └─ msg.attachments (Vec<Attachment>) ← 無視
        ↓
MessageContent::Text(string)          [discord/gateway.rs]
        ↓
incoming.content.as_text() → String  [discord/message_loop.rs]
        ↓
DB に text として保存                  [message_loop.rs: SessionLogRow]
        ↓
build_conversation_string()           [server/process.rs]
  → "[user]: テキスト内容\n..."
        ↓
ChatMessage { role: "user", content: String }  [core/engine.rs]
        ↓
to_llm_message() → MessageContent::Text(...)   [server/llm_adapter.rs]
        ↓
LLM API (Claude / OpenAI)
```

### 1.4 LLM 層の型定義（実装済みだが未使用）

`crates/llm/src/message.rs` にはマルチモーダル対応の型が既に定義されている:

```rust
pub enum MessageContent {
    Text(String),
    Image { content_type: String, image_url: ImageUrl },
    Multi(Vec<ContentPart>),  // ← テキスト+画像の複合
}

pub struct ImageUrl {
    pub url: String,
    pub detail: Option<String>,  // "low" | "high" | "auto"
}

pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },  // ← vision 用
}
```

**これらの型は定義されているが、実際の会話フローで使われたことは一度もない。**

### 1.5 SkillEngine の `ChatMessage` 構造

`crates/core/src/engine.rs`:

```rust
pub struct ChatMessage {
    pub role: String,
    pub content: String,          // ← テキストのみ（マルチモーダル未対応）
    pub tool_call_id: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}
```

`llm_adapter.rs` の変換:
```rust
fn to_llm_message(msg: ChatMessage) -> Message {
    // content は常に MessageContent::Text として変換される
    content: Some(MessageContent::Text(msg.content)),
    ...
}
```

---

## 2. 設計方針

### 2.1 Discord 画像添付 → LLM `ContentPart` への変換

**アプローチ: URL パッシングスルー**

Discord CDN URL はアクセス可能な公開 URL のため、LLM の `image_url` パラメータに直接渡す。
画像データのダウンロード・再エンコードは不要。

```
Discord Attachment.url (CDN URL)
  ↓
ContentPart::ImageUrl { image_url: ImageUrl { url: cdn_url, detail: Some("auto") } }
```

**変換ロジック（`discord/src/gateway.rs`）**:

```rust
// 画像添付を ContentPart::Image に変換
let image_parts: Vec<gateway::ContentPart> = msg.attachments.iter()
    .filter(|a| is_image_attachment(a))
    .map(|a| gateway::ContentPart::Image {
        url: a.url.clone(),
        alt: Some(a.filename.clone()),
    })
    .collect();

let content = if image_parts.is_empty() {
    MessageContent::text(&msg.content)
} else if msg.content.is_empty() {
    // テキストなし、画像のみ
    MessageContent::Multi(image_parts.into_iter()
        .map(|p| p)
        .collect())
} else {
    // テキスト + 画像
    let mut parts = vec![ContentPart::Text(msg.content.clone())];
    parts.extend(image_parts);
    MessageContent::Multi(parts)
};

fn is_image_attachment(a: &Attachment) -> bool {
    a.content_type.as_deref()
        .map(|ct| ct.starts_with("image/"))
        .unwrap_or(false)
    || (a.width.is_some() && a.height.is_some())
}
```

### 2.2 テキスト+画像の複合メッセージ構築

**`ChatMessage` への拡張（`core/engine.rs`）**:

マルチモーダル対応のために `content_parts` フィールドを追加する。
`content: String` は後方互換のために残す（tool メッセージ等で使用）。

```rust
pub struct ChatMessage {
    pub role: String,
    pub content: String,    // テキスト部分（後方互換）
    pub tool_call_id: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    // 新規追加: マルチモーダルコンテンツ（Some の場合は content より優先）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_parts: Vec<ChatContentPart>,
}

/// マルチモーダルコンテンツのパーツ
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChatContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { url: String, detail: Option<String> },
}
```

**`llm_adapter.rs` の変換拡張**:

```rust
fn to_llm_message(msg: ChatMessage) -> Message {
    let content = if !msg.content_parts.is_empty() {
        // マルチモーダル: content_parts を使用
        let parts: Vec<llm::ContentPart> = msg.content_parts.into_iter()
            .map(|p| match p {
                ChatContentPart::Text { text } => llm::ContentPart::Text { text },
                ChatContentPart::ImageUrl { url, detail } => {
                    llm::ContentPart::ImageUrl {
                        image_url: llm::ImageUrl { url, detail }
                    }
                }
            })
            .collect();
        Some(MessageContent::Multi(parts))
    } else if msg.content.is_empty() {
        None
    } else {
        Some(MessageContent::Text(msg.content))
    };
    // ... 残りのフィールド変換は既存と同じ
}
```

### 2.3 メッセージループの修正（`discord/message_loop.rs`）

画像のみのメッセージでもスキップしないように修正:

```rust
// 現在: テキストが空ならスキップ
let text = match incoming.content.as_text() {
    Some(t) if !t.is_empty() => t.to_string(),
    _ => continue,  // ← 問題: Multi/Image もスキップされる
};

// 修正後: テキスト OR 画像があれば処理
let (text, image_parts) = extract_content(&incoming.content);
if text.is_empty() && image_parts.is_empty() {
    continue;
}

fn extract_content(content: &gateway::MessageContent) -> (String, Vec<gateway::ContentPart>) {
    match content {
        gateway::MessageContent::Text(t) => (t.clone(), vec![]),
        gateway::MessageContent::Image { url, alt } => {
            ("".to_string(), vec![gateway::ContentPart::Image { url: url.clone(), alt: alt.clone() }])
        }
        gateway::MessageContent::Multi(parts) => {
            let text = parts.iter().filter_map(|p| match p {
                gateway::ContentPart::Text(t) => Some(t.clone()),
                _ => None,
            }).collect::<Vec<_>>().join(" ");
            let images: Vec<_> = parts.iter().filter(|p| matches!(p, gateway::ContentPart::Image { .. })).cloned().collect();
            (text, images)
        }
    }
}
```

### 2.4 非画像添付（PDF、音声等）の扱い

**方針: テキスト通知として含める（MVP段階）**

Vision 対応の MVP では画像のみをターゲットとし、非画像添付はテキストアノテーションとしてメッセージに付加する。

```rust
let non_image_notes: Vec<String> = msg.attachments.iter()
    .filter(|a| !is_image_attachment(a))
    .map(|a| {
        let ct = a.content_type.as_deref().unwrap_or("unknown");
        format!("[添付ファイル: {} ({}), {}B]", a.filename, ct, a.size)
    })
    .collect();

// 本文末尾に追記
let full_text = if non_image_notes.is_empty() {
    msg.content.clone()
} else {
    format!("{}\n{}", msg.content, non_image_notes.join("\n"))
};
```

将来的な拡張:
- PDF: テキスト抽出ツール（`extract_pdf_text` action）
- 音声: Whisper API 経由の文字起こし

### 2.5 セッション履歴への保存方法

**問題**: `SessionLogRow.content` は `String` 型。画像 URL をどう保存するか。

**方針: metadata_json に画像情報を保存**

```rust
// DB ログ行への保存
let mut log_meta = serde_json::json!({
    "source": "discord",
    "channel_id": channel_id_str,
    "user_name": incoming.sender.name,
});

if !image_parts.is_empty() {
    let image_urls: Vec<&str> = image_parts.iter()
        .filter_map(|p| match p {
            ContentPart::Image { url, .. } => Some(url.as_str()),
            _ => None,
        })
        .collect();
    log_meta["image_urls"] = serde_json::json!(image_urls);
}

let log = SessionLogRow {
    content: text.clone(),  // テキスト部分のみ
    metadata_json: Some(log_meta.to_string()),
    ...
};
```

**`build_conversation_string` の拡張**:

DB から読み込んだログの `metadata_json` に `image_urls` があれば、`ChatMessage.content_parts` に画像を含める。

```rust
pub fn build_chat_messages_for_session(
    conn: &Connection,
    session_id: &str,
) -> Vec<ChatMessage> {
    let logs = list_session_logs_by_session(conn, session_id).unwrap_or_default();
    
    logs.iter().map(|log| {
        let meta: Option<serde_json::Value> = log.metadata_json.as_ref()
            .and_then(|s| serde_json::from_str(s).ok());
        
        let image_urls: Vec<String> = meta.as_ref()
            .and_then(|m| m["image_urls"].as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        
        let role = determine_role(log);
        
        if image_urls.is_empty() {
            // テキストのみ（既存動作）
            ChatMessage { role, content: log.content.clone(), ..Default::default() }
        } else {
            // テキスト + 画像
            let mut parts = Vec::new();
            if !log.content.is_empty() {
                parts.push(ChatContentPart::Text { text: log.content.clone() });
            }
            for url in image_urls {
                parts.push(ChatContentPart::ImageUrl { url, detail: Some("auto".to_string()) });
            }
            ChatMessage { role, content: log.content.clone(), content_parts: parts, ..Default::default() }
        }
    }).collect()
}
```

**注意**: セッション履歴に画像 URL を含める場合、Discord CDN URL の有効期限に注意。
Discord CDN URL は永続的でないため、古い画像参照はエラーになる可能性がある。
MVP では許容し、将来的にプロキシキャッシュを検討する。

---

## 3. 変更範囲

### 変更ファイルと関数

| ファイル | 関数/箇所 | 変更内容 |
|---------|----------|---------|
| `crates/discord/src/gateway.rs` | `EventHandler::message()` | `msg.attachments` を読んで `MessageContent::Multi` を構築 |
| `crates/discord/src/message_loop.rs` | `run_discord_loop()` | `as_text()` スキップを廃止、Multi/Image も処理 |
| `crates/discord/src/message_loop.rs` | ログ保存箇所 | `metadata_json` に `image_urls` を保存 |
| `crates/core/src/engine.rs` | `ChatMessage` struct | `content_parts: Vec<ChatContentPart>` フィールド追加 |
| `crates/core/src/engine.rs` | 新規: `ChatContentPart` enum | `Text` / `ImageUrl` バリアント |
| `crates/server/src/llm_adapter.rs` | `to_llm_message()` | `content_parts` が存在する場合 `MessageContent::Multi` に変換 |
| `crates/server/src/process.rs` | `build_conversation_string()` | 画像 URL 対応の `build_chat_messages_for_session()` に置き換え or 拡張 |
| `crates/discord/src/lib.rs` | `AgentRunner::build_conversation_string()` | 戻り値を `Vec<ChatMessage>` に変更（または別メソッド追加） |

### 新規追加が必要なコード

1. **`is_image_attachment()` ヘルパー** (`discord/src/gateway.rs`)
   - MIME type チェック + 拡張子フォールバック

2. **`extract_content()` ヘルパー** (`discord/message_loop.rs`)
   - `MessageContent` から `(text, image_parts)` を取り出す

3. **`ChatContentPart` enum** (`core/engine.rs`)
   - Vision 対応の内部型

4. **`build_chat_messages_for_session()`** (`server/process.rs`)
   - DB ログから `Vec<ChatMessage>` を構築（画像 URL 含む）
   - 既存の `build_conversation_string()` は後方互換のため残す（`AgentRunner` トレイトの変更を最小化）

---

## 4. 実装ステップ（優先順位付き）

### Phase 1: ゲートウェイ層（最重要）

**Step 1-1**: `crates/discord/src/gateway.rs` の修正
- `msg.attachments` をイテレートして画像を `ContentPart::Image` に変換
- `MessageContent::Multi` を構築してテキストと画像を組み合わせる
- `is_image_attachment()` ヘルパー追加

**Step 1-2**: `crates/discord/src/message_loop.rs` の修正
- `as_text()` によるスキップを廃止
- `extract_content()` ヘルパーで `(text, image_parts)` を取得
- 画像 URL を `metadata_json` に保存してDBログ

### Phase 2: コアエンジン層

**Step 2-1**: `crates/core/src/engine.rs` の修正
- `ChatContentPart` enum 追加
- `ChatMessage` に `content_parts: Vec<ChatContentPart>` 追加（デフォルト空配列）

**Step 2-2**: `crates/server/src/llm_adapter.rs` の修正
- `to_llm_message()` に `content_parts` → `MessageContent::Multi` の変換ロジック追加

### Phase 3: 会話履歴層

**Step 3-1**: `crates/server/src/process.rs` の修正
- `build_chat_messages_for_session()` 新規実装
- `metadata_json["image_urls"]` から画像を復元して `ChatMessage.content_parts` に含める
- `AgentRunner::run_agent_response()` の `conversation: &str` を `messages: Vec<ChatMessage>` に変更
  （または両方のシグネチャをサポート）

### Phase 4: 統合テスト

**Step 4-1**: ユニットテスト
- `is_image_attachment()` のテスト
- `to_llm_message()` でマルチモーダル変換のテスト

**Step 4-2**: 実機テスト
- Discord で画像を送信してエージェントAが説明できるか確認
- テキスト+画像の複合メッセージのテスト
- 画像のみメッセージのテスト

---

## 5. 注意事項・懸念点

### 5.1 AgentRunner トレイトの変更

`AgentRunner::build_conversation_string()` が `String` を返す設計になっており、
`SkillEngine::run()` も `user_message: &str` を受け取る設計。

マルチモーダル対応のためには `Vec<ChatMessage>` を渡せるようにする必要があるが、
トレイト変更はDiscordクレートとサーバークレート双方に影響する。

**推奨**: 既存の `build_conversation_string()` は維持し、新規メソッド
`build_conversation_messages()` を `AgentRunner` に追加。
`SkillEngine` に新規メソッド `run_with_messages()` を追加して `run()` から分離。

### 5.2 Discord CDN URL の有効期限

Discord の添付ファイル URL はエフェメラルな場合がある（長期間後に失効する可能性）。
セッション履歴に URL を保存しても、数日後の参照は失敗するケースがある。

MVP では許容範囲として受け入れ、将来的に画像をローカルプロキシキャッシュする仕組みを検討。

### 5.3 プロバイダー互換性

Claude (Anthropic) は `image_url` 形式に対応しているが、
URL よりも base64 エンコードされた画像データを推奨する場合がある。
OpenAI は URL 直接参照をサポート。

MVP では URL パッシングスルーで実装し、
Anthropic プロバイダー向けに必要であればダウンロード＋base64 変換を Phase 2 以降で追加。

### 5.4 画像サイズ制限

大きな画像ファイルはトークン消費・コスト・レイテンシに大きく影響する。
Discord ではデフォルト最大 8MB（Nitro では 50MB）の添付が可能。

推奨: `detail: "low"` または width/height が一定値以上の場合はスキップ（警告メッセージを返す）。

---

## 6. 実装例: 最小変更セット（MVP）

最速で動作させるための最小変更（Phase 1 + Phase 2 のみ、履歴対応は Phase 3 以降）:

```
変更ファイル数: 4
追加行数: ~80行
```

1. `discord.rs`: attachments → MessageContent::Multi（+30行）
2. `message_loop.rs`: スキップ廃止 + 画像 URL 抽出（+20行）
3. `engine.rs`: ChatContentPart + ChatMessage 拡張（+15行）
4. `llm_adapter.rs`: content_parts → MessageContent::Multi 変換（+15行）

この MVP で「画像を送ったらエージェントAが認識できる（ただし履歴には画像が残らない）」が実現できる。

---

## 参考: 関連ファイルの場所

```
crates/
  gateway/
    src/
      message.rs              # IncomingMessage, MessageContent, ContentPart 型定義
  discord/
    src/
      gateway.rs              # serenity イベントハンドラ（Drop箇所1）
      message_loop.rs         # メッセージループ（Drop箇所2）
      lib.rs                  # AgentRunner トレイト定義
  core/
    src/
      engine.rs               # SkillEngine, ChatMessage, ChatRequestSimple
  llm/
    src/
      message.rs              # LLM向け MessageContent, ContentPart, ImageUrl
  server/
    src/
      llm_adapter.rs          # ChatMessage → Message 変換
      process.rs              # build_conversation_string, run_agent_response
      agent_runner_impl.rs    # AgentRunner トレイト実装
```

## レビューメモ（2026-03-22 by エージェントB）

- 設計の問題箇所特定とURLパッシングスルー方式はOK
- **型変換に注意**: `gateway::MessageContent::ContentPart::Image {url, alt}` と `llm::MessageContent::ContentPart::ImageUrl {image_url: ImageUrl}` は別の型。`gateway→llm` の型変換ロジックが漏れないように実装すること
