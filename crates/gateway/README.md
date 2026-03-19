# opencrab-gateway

opencrab のゲートウェイ crate。外部プラットフォーム（Discord、REST API、CLI、WebSocket）との統一的なメッセージング・インターフェースを提供する。

## 概要

`opencrab-gateway` は、様々な I/O プラットフォームを抽象化する `Gateway` trait を中心に構築されている。各アダプターはこの trait を実装し、メッセージの受信・送信を統一的に扱えるようにする。

## アーキテクチャ

```
外部プラットフォーム
  ├── Discord (serenity)
  ├── REST API
  ├── CLI
  └── WebSocket
        │
        ▼
  ┌─────────────┐
  │   Gateway    │  ← 統一インターフェース
  │   trait      │
  └──────┬──────┘
         │
         ▼
    IncomingMessage / OutgoingMessage
```

## Gateway trait

すべてのゲートウェイが実装するコアインターフェース。

```rust
#[async_trait]
pub trait Gateway: Send + Sync {
    fn name(&self) -> &str;
    async fn connect(&mut self) -> Result<()>;
    async fn receive(&mut self) -> Result<IncomingMessage>;
    async fn send(&self, message: OutgoingMessage) -> Result<()>;
    async fn disconnect(&mut self) -> Result<()>;
}
```

### ライフサイクル

1. `connect()` - 接続を確立（Bot起動、WebSocket接続など）
2. `receive()` - メッセージを待ち受け（ブロッキング）
3. `send()` - 応答メッセージを送信
4. `disconnect()` - 接続を切断、リソースをクリーンアップ

## GatewayActions trait

ゲートウェイ固有の操作をエージェントにツールとして提供する仕組み。

```rust
#[async_trait]
pub trait GatewayActions: Send + Sync {
    fn definitions(&self) -> Vec<GatewayActionDef>;
    async fn execute(&self, name: &str, args: &serde_json::Value) -> GatewayActionResult;
}
```

例えば Discord ゲートウェイでは、エージェントが `discord_list_guilds` や `discord_channel_config` などのツールを通じてサーバーやチャンネルを管理できる。

## メッセージ型

### IncomingMessage（受信メッセージ）

| フィールド | 型 | 説明 |
|---|---|---|
| `id` | `String` | UUID v4 で自動生成 |
| `source` | `MessageSource` | メッセージの送信元プラットフォーム |
| `content` | `MessageContent` | テキスト、画像、またはマルチパート |
| `sender` | `Sender` | 送信者情報（ID、名前、Bot判定、アバター） |
| `channel` | `Option<Channel>` | チャンネル情報 |
| `timestamp` | `DateTime<Utc>` | 受信タイムスタンプ |
| `metadata` | `HashMap<String, Value>` | プラットフォーム固有のメタデータ |

### OutgoingMessage（送信メッセージ）

| フィールド | 型 | 説明 |
|---|---|---|
| `content` | `MessageContent` | 送信内容 |
| `target` | `MessageTarget` | 送信先（Channel / DirectMessage / Broadcast） |
| `reply_to` | `Option<String>` | 返信先メッセージID |
| `metadata` | `HashMap<String, Value>` | プラットフォーム固有のメタデータ |

### MessageSource（送信元）

- `Rest { request_id }` - REST API
- `WebSocket { connection_id }` - WebSocket
- `Discord { guild_id, channel_id }` - Discord
- `Cli { session_id }` - CLI
- `Slack { workspace_id, channel_id }` - Slack
- `Line { user_id }` - LINE

### MessageContent（コンテンツ）

- `Text(String)` - テキストメッセージ
- `Image { url, alt }` - 画像
- `Multi(Vec<ContentPart>)` - マルチパート（テキスト＋画像の組み合わせ）

## アダプター一覧

| アダプター | モジュール | feature flag | 説明 |
|---|---|---|---|
| `DiscordGateway` | `adapters::discord` | `discord` | serenity ベースの Discord Bot |
| `RestGateway` | `adapters::rest` | (デフォルト) | REST API エンドポイント |
| `CliGateway` | `adapters::cli` | (デフォルト) | コマンドライン対話 |
| `WebSocketGateway` | `adapters::websocket` | (デフォルト) | WebSocket 接続 |

## Cargo features

```toml
[features]
default = []
discord = ["serenity"]
```

- `discord` - DiscordGateway を有効化。serenity を依存関係に追加する。

## 使い方

```toml
# Cargo.toml
[dependencies]
opencrab-gateway = { path = "../gateway" }

# Discord を使う場合
opencrab-gateway = { path = "../gateway", features = ["discord"] }
```

## 関連ドキュメント

- [Discord ゲートウェイ詳細設定](../../docs/discord.md) - Discord Bot の設定・チャンネル制御・セッション管理の詳細
