# Discord ファイル添付送信サポート — 設計ドキュメント

> 作成日: 2026-03-22  
> ステータス: 設計段階 (Draft)

---

## 1. 背景と目的

opencrabのかいろ（エージェント）は現在、`send_speech` アクションでテキストメッセージのみをDiscordに送信できる。  
画像生成スクリプト（nano-banana-pro等）で生成したファイルをDiscordに直接送信する手段がない。

本ドキュメントでは **Discordへのファイル（画像）添付送信** 機能を追加するための設計を定義する。

---

## 2. 現状分析

### 2.1 `send_speech` の実装

**`crates/actions/src/common.rs`** — `SendSpeechAction`

```rust
async fn execute(&self, args: &serde_json::Value, _ctx: &ActionContext) -> ActionResult {
    let content = args["content"].as_str()...;
    ActionResult::success(json!({ "sent": true, "content": content }))
        .with_side_effect(SideEffect::MessageSent {
            channel: "default".to_string(),
            content,
        })
}
```

- `SideEffect::MessageSent` を発行するだけで、実際の送信はエンジン側（`run_agent_response`）が処理
- ファイルパスや添付ファイルを渡す仕組みが **存在しない**

### 2.2 Discord送信パイプライン

```
LLM → ActionResult(SideEffect::MessageSent) 
  → Engine → gateway.send_to_channel(channel_id, text)
  → crates/gateway/src/adapters/discord.rs: ChannelId::say(http, text)
```

**`crates/gateway/src/adapters/discord.rs`** — `send_to_channel`:

```rust
pub async fn send_to_channel(&self, channel_id: u64, text: &str) -> Result<()> {
    ChannelId::new(channel_id).say(&self.http, text).await?;
    Ok(())
}
```

- `say()` はテキストのみ。ファイル添付には対応していない
- serenity v0.12 では `ChannelId::send_message()` + `CreateMessage::add_file()` が必要

### 2.3 serenity v0.12 のファイル添付API

現在のワークスペース依存: `serenity = { version = "0.12", features = ["client", "gateway", "model", "cache", "rustls_backend"] }`

serenity v0.12 のファイル添付方法:

```rust
use serenity::all::{ChannelId, CreateAttachment, CreateMessage};

// ファイルパスから添付
let attachment = CreateAttachment::path("/path/to/image.png").await?;
let msg = CreateMessage::new()
    .content("キャプション")
    .add_file(attachment);
ChannelId::new(channel_id).send_message(&http, msg).await?;

// バイト列から添付
let attachment = CreateAttachment::bytes(bytes, "image.png");
```

### 2.4 GatewayActions パターン

**`crates/gateway/src/traits.rs`** — `GatewayActions` トレイト:

```rust
#[async_trait]
pub trait GatewayActions: Send + Sync {
    fn definitions(&self) -> Vec<GatewayActionDef>;
    async fn execute(&self, name: &str, args: &serde_json::Value) -> GatewayActionResult;
}
```

**`crates/discord/src/gateway_actions.rs`** — `DiscordGatewayActions` が実装:  
- 12個のアクションが登録済み（`discord_list_guilds`, `discord_add_reaction` 等）
- `Arc<Http>` を保持しているため、HTTP API直接呼び出しが可能

### 2.5 ワークスペースとセキュリティ境界

**`crates/core/src/workspace.rs`** — エージェントが操作できるファイルは workspace root 配下に限定されている。  
ファイル送信でも同じ制約を適用すべき。

---

## 3. 設計方針

### 3.1 新規 Gateway Action: `discord_send_file`

既存の `GatewayActions` パターン（`DiscordGatewayActions`）に新しいアクションを追加する方針を採用。

**理由:**
- 既存の `send_speech` → `SideEffect` → Engine の経路を変更しない（破壊的変更なし）
- `Arc<Http>` を直接持つ `DiscordGatewayActions` に追加するのが最も自然
- エージェント（LLM）が明示的に「ファイルを送る」という意思決定をするフローに合う
- Discord以外のゲートウェイには影響しない

### 3.2 アクション名

```
discord_send_file
```

- 既存の `discord_*` naming convention に従う
- "send_with_attachment" より明示的で短い

### 3.3 パラメータ設計

```json
{
  "type": "object",
  "required": ["channel_id", "file_path"],
  "properties": {
    "channel_id": {
      "type": "string",
      "description": "送信先チャンネルの数値ID"
    },
    "file_path": {
      "type": "string",
      "description": "送信するファイルのパス（ワークスペース相対パスまたは絶対パス）"
    },
    "caption": {
      "type": "string",
      "description": "ファイルに添付するテキストキャプション（省略可）"
    },
    "filename": {
      "type": "string",
      "description": "Discord上で表示されるファイル名（省略時は元のファイル名）"
    }
  }
}
```

### 3.4 セキュリティ設計（ワークスペース制限）

エージェントが任意のシステムファイルを送信できないよう、**ワークスペース内のファイルのみ**に制限する。

```
許可: /workspace/generated/image.png
許可: generated/image.png  (ワークスペース相対)
禁止: /etc/passwd
禁止: /Volumes/2TB/secrets/keys.json
禁止: ../../../etc/shadow  (パストラバーサル)
```

実装:
1. `file_path` をワークスペースrootに対してcanonical化
2. canonical pathがworkspace root以下であることを確認
3. ファイルが実際に存在することを確認
4. ファイルサイズ上限チェック（Discord制限: 25MB、Free: 8MB）

---

## 4. 変更範囲

### 4.1 追加するファイル・関数

#### `crates/discord/src/gateway_actions.rs`

**追加関数:**
```rust
async fn execute_send_file(&self, args: &serde_json::Value) -> GatewayActionResult
```

**実装概要:**
```rust
async fn execute_send_file(&self, args: &serde_json::Value) -> GatewayActionResult {
    // 1. パラメータ取得
    let channel_id_str = args.get("channel_id").and_then(|v| v.as_str())?;
    let file_path_str = args.get("file_path").and_then(|v| v.as_str())?;
    let caption = args.get("caption").and_then(|v| v.as_str()).unwrap_or("");
    let filename_override = args.get("filename").and_then(|v| v.as_str());
    
    // 2. channel_id パース
    let channel_id: u64 = channel_id_str.parse()?;
    
    // 3. ワークスペースパス検証（セキュリティ）
    let workspace_root = self.workspace_root.canonicalize()?;
    let abs_path = if Path::new(file_path_str).is_absolute() {
        PathBuf::from(file_path_str)
    } else {
        workspace_root.join(file_path_str)
    };
    let canonical = abs_path.canonicalize()?;
    if !canonical.starts_with(&workspace_root) {
        return GatewayActionResult::error("ワークスペース外のファイルは送信できません");
    }
    
    // 4. ファイルサイズチェック
    let metadata = fs::metadata(&canonical)?;
    if metadata.len() > 25 * 1024 * 1024 {
        return GatewayActionResult::error("ファイルサイズが25MB制限を超えています");
    }
    
    // 5. serenity CreateAttachment でファイル添付
    let display_name = filename_override
        .unwrap_or_else(|| canonical.file_name().unwrap().to_str().unwrap());
    let attachment = CreateAttachment::path(&canonical).await?;
    let mut msg = CreateMessage::new().add_file(attachment);
    if !caption.is_empty() {
        msg = msg.content(caption);
    }
    
    // 6. 送信
    ChannelId::new(channel_id).send_message(&self.http, msg).await?;
    
    GatewayActionResult {
        success: true,
        data: Some(json!({ "channel_id": channel_id_str, "file": display_name })),
        error: None,
    }
}
```

**`definitions()` への追加:**
```rust
GatewayActionDef {
    name: "discord_send_file".to_string(),
    description: "DiscordチャンネルにファイルをアップロードしてDiscordに送信する。...".to_string(),
    parameters: json!({ ... }),
},
```

**`execute()` のマッチ追加:**
```rust
"discord_send_file" => self.execute_send_file(args).await,
```

#### `DiscordGatewayActions` 構造体への `workspace_root` フィールド追加

```rust
pub struct DiscordGatewayActions {
    http: Arc<Http>,
    db: Arc<Mutex<rusqlite::Connection>>,
    agent_id: String,
    tools_config: Arc<std::sync::RwLock<opencrab_actions::tools::ToolsConfig>>,
    llm_client: Option<Arc<dyn opencrab_core::LlmClient>>,
    default_model: String,
    workspace_root: std::path::PathBuf,  // ← 新規追加
}
```

`DiscordGatewayActions::new()` のシグネチャ変更:
```rust
pub fn new(
    http: Arc<Http>,
    db: Arc<Mutex<rusqlite::Connection>>,
    agent_id: String,
    tools_config: Arc<...>,
    llm_client: Option<Arc<dyn LlmClient>>,
    default_model: String,
    workspace_root: std::path::PathBuf,  // ← 新規追加
) -> Self
```

### 4.2 既存コードへの影響範囲

| ファイル | 変更内容 | 影響度 |
|---|---|---|
| `crates/discord/src/gateway_actions.rs` | `execute_send_file` 追加、`workspace_root` フィールド追加、`definitions()` / `execute()` 更新 | **主要変更** |
| `crates/discord/src/manager.rs` | `DiscordGatewayActions::new()` 呼び出し箇所に `workspace_root` 引数を追加 | 軽微 |
| `crates/gateway/src/adapters/discord.rs` | **変更なし** — テキスト送信は既存のまま | なし |
| `crates/actions/src/common.rs` | **変更なし** — `send_speech` は既存のまま | なし |
| `crates/actions/src/dispatcher.rs` | **変更なし** | なし |
| `Cargo.toml` (workspace) | serenity features に `"http"` が必要な場合のみ追加 | 要確認 |

### 4.3 serenity featureの確認

`discord_send_file` で使う `CreateAttachment::path()` は serenity の `model` feature に含まれる。  
現在 `Cargo.toml` の serenity features: `["client", "gateway", "model", "cache", "rustls_backend"]`

→ `model` は既に有効なので **追加 feature 不要**。ただし `tokio::fs` を使うため `tokio` の `fs` feature が必要（現在の設定を要確認）。

---

## 5. nano-banana-pro との連携フロー

### かいろが「画像を生成してDiscordに送って」と言われた時

```
ユーザー: "未来の東京の画像を生成してDiscordのgeneral（#1234567890）に送って"
    ↓
かいろ (LLM思考):
  1. execute_shell で nano-banana-pro を実行して画像生成
  2. discord_send_file で生成した画像をDiscordに送信
    ↓
[Tool Call 1] execute_shell
  args: {
    "command": "nb-pro generate '未来の東京' --output /workspace/generated/tokyo_future.png"
  }
  → 画像ファイルが /workspace/generated/tokyo_future.png に生成される
    ↓
[Tool Call 2] discord_send_file
  args: {
    "channel_id": "1234567890",
    "file_path": "generated/tokyo_future.png",  # ワークスペース相対パス
    "caption": "未来の東京 ⚡"
  }
  → Discord APIにファイルアップロード + 送信
    ↓
[Tool Call 3] send_speech
  args: {
    "content": "画像を送信したよ！"
  }
  → Discordに確認メッセージ送信
```

### 具体的なコード例（nano-banana-pro CLIとの連携）

```bash
# execute_shell で実行するコマンド例
nb-pro generate "prompt text" --output /workspace/generated/image.png

# または既存のnano-banana-pro skillの場合
/path/to/nb-pro --prompt "..." --out /workspace/generated/result.png
```

```json
// discord_send_file の呼び出し例
{
  "channel_id": "1234567890",
  "file_path": "generated/result.png",
  "caption": "生成完了！ ⚡",
  "filename": "result.png"
}
```

---

## 6. 実装ステップ（優先順位付き）

### P0: 最小実装（コアMVP）

**Step 1: `workspace_root` フィールド追加**
- `DiscordGatewayActions` 構造体に `workspace_root: PathBuf` を追加
- `new()` シグネチャ更新
- `manager.rs` の呼び出し箇所を更新（`state.workspace_root()` を渡す）

**Step 2: `execute_send_file` 実装**
- ワークスペースパス検証（canonicalize + prefix check）
- serenity `CreateAttachment::path()` + `CreateMessage::add_file()` で送信
- エラーハンドリング（ファイル不存在、サイズ超過、権限エラー）

**Step 3: `definitions()` と `execute()` への登録**
- `definitions()` に `discord_send_file` の定義を追加
- `execute()` のマッチアームに追加
- テスト更新（`definitions().len()` アサーション）

### P1: 品質向上

**Step 4: ファイルサイズ制限**
- Discord制限（25MB）のチェック
- より細かいエラーメッセージ（"8MB以下推奨"など）

**Step 5: ユニットテスト**
- パス検証ロジックのテスト（ワークスペース外パスのブロック確認）
- パストラバーサル防止テスト（`../../etc/passwd` 等）
- モックHTTPでの送信テスト（可能な範囲で）

### P2: 拡張機能

**Step 6: 複数ファイル対応**
- `file_paths: Vec<String>` パラメータ対応
- `CreateMessage::add_files()` で一括送信

**Step 7: バイト列から直接送信**
- `file_bytes` + `filename` パラメータ対応（ファイルを作らずに送信）
- Base64エンコード経由でのデータ渡し

---

## 7. 実装時の注意点

### serenity v0.12 の `CreateAttachment::path()` は async

```rust
// ⚠️ path() は async fn
let attachment = CreateAttachment::path(&canonical).await
    .map_err(|e| format!("ファイル読み込み失敗: {e}"))?;
```

### `execute_send_file` は `async fn` が必要

`execute_add_reaction` と同様に `async fn` で定義する。

### テストのアサーション更新

```rust
// 現在のテスト
assert_eq!(defs.len(), 12);

// 追加後
assert_eq!(defs.len(), 13);
```

### `workspace_root` の取得元

`AgentRunner` trait が `workspace_root()` を提供しているか確認が必要。  
提供していない場合は `state.workspace_root()` メソッドを追加するか、  
`DiscordGatewayActions::new()` の呼び出し側（`manager.rs`）で別途 workspace パスを取得して渡す。

---

## 8. 今後の拡張可能性

- **voice channel への音声ファイル送信**: 同様のパターンで `discord_send_audio` として追加可能
- **embed 付き送信**: `CreateMessage::embed()` でリッチなメッセージ形式に対応
- **スレッドへのファイル送信**: `thread_id` パラメータを追加するだけで対応可能
- **他ゲートウェイへの対応**: REST / WebSocket ゲートウェイにも同様の `send_file` を追加可能

---

## 9. 関連ファイル一覧

```
crates/
├── discord/
│   ├── src/
│   │   ├── gateway_actions.rs    ← 主要変更ファイル
│   │   ├── manager.rs            ← new() 呼び出し更新
│   │   └── message_loop.rs       ← 変更なし
│   └── Cargo.toml                ← 変更なし
├── gateway/
│   └── src/
│       ├── adapters/discord.rs   ← 変更なし
│       └── traits.rs             ← 変更なし
└── actions/
    └── src/
        ├── common.rs             ← 変更なし
        └── dispatcher.rs         ← 変更なし
```
