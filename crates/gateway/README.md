# opencrab-gateway

opencrab のゲートウェイ crate。**ポート専用**で、具象 transport（SDK に依存する実装）を含まない。提供するのは **gateway 共通のメッセージ型 / アクション抽象**（ポート）のみ。Discord の具象実装（`DiscordGateway`）は `opencrab-discord` へ移設した（SDK を共有層から切り離すため）。

## 現状（#215 以降）

**「すべての transport を束ねる `Gateway` trait」は存在しない。** かつて `connect` /
`receive` / `send` / `disconnect` を持つ `Gateway` trait と REST / CLI / WebSocket の
アダプタがあったが、**実装 4 つに対して利用者ゼロ**（`dyn Gateway` の使用箇所なし・
WebSocket は全メソッド `todo!()`）のまま腐っていたので #215 で削除した。

構造的にも受け皿にならなかった:

- `receive(&mut self)` は `Arc` 共有された状態から呼べない。実運用の transport は
  すべて push 型（イベントループが `mpsc` に流す）で、pull 型の署名と噛み合わない。
- どのメソッドにも `agent_id` が無く、per-agent ゲートウェイ（#40）を表現できない。

上位（server）は現在も個々のゲートウェイを具象型で名指しして保持している。これを
「上位が個々のゲートウェイを知らない」形へ寄せる方針は
[docs/DESIGN.md](../../docs/DESIGN.md) §2.4 と
[docs/design-plugin-architecture.md](../../docs/design-plugin-architecture.md) を参照
（#191）。**新しい transport 抽象を足すなら、この crate の型を再利用するのではなく
そこの設計から始めること。**

実際の I/O 経路:

| 経路 | 実体 |
|---|---|
| Discord | `DiscordGateway`（`opencrab-discord`）＋ `opencrab-discord` のイベントループ |
| REST | `opencrab-server` の axum ハンドラ（この crate を経由しない） |
| Nostr | `opencrab-nostr` |
| Web | `opencrab-web-gateway` |

## DiscordGateway

serenity/songbird ベースの Discord Bot 実装は **`opencrab-discord`（`gateway` モジュール）へ移設した**（SDK を共有層から切り離す #1-A）。`DiscordGateway` と関連型（`ComponentInteractionData` / `InteractionKind` / `A2uiFormModalSpec` / `A2uiFormModalResolver`）は `opencrab_discord::` から使う。

## GatewayActions trait

ゲートウェイ固有の操作をエージェントにツールとして提供する仕組み。こちらは
**実利用中**（`opencrab-server` の `SystemGatewayActions` などが実装・合成する）。

```rust
#[async_trait]
pub trait GatewayActions: Send + Sync {
    fn definitions(&self) -> Vec<GatewayActionDef>;
    async fn execute(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult;

    /// A2UI（`send_ui`）の描画面。提供しない transport は `None`（既定）。
    fn a2ui_surface(&self) -> Option<Arc<opencrab_core::a2ui::A2uiSurface>> { None }

    /// 素テキストの配送口（`request_peer_review` 等が使う）。既定は `None`。
    fn text_delivery(&self) -> Option<Arc<dyn opencrab_core::text_delivery::TextDelivery>> { None }
}
```

`args` は **LLM 由来のツール引数のみ**。呼び出し元の権限・セッション・返信先などの
実行コンテキストは JSON に混ぜず `GatewayCallContext` で型付きに渡す（#36）。

### GatewayCallContext

| フィールド | 説明 |
|---|---|
| `caller` | `GatewayCaller`（`Owner` / `Agent` / `CoAgent` / `TrustedUser`）。権限判定は enum match で行う |
| `session_id` | 呼び出し元エンジンのセッション ID。無ければ `None`（セッション必須のアクションは fail-closed） |
| `depth` | sub-engine のネスト深さ（メイン = 0） |
| `agent_id` | 実行中のエージェント |
| `root_gateway` | 自身を包む合成 gateway へのハンドル（#152 S2） |
| `reply_target` | inbound メッセージの返信先（gateway 不透明トークン / #158 S1） |

## メッセージ型

### IncomingMessage（受信メッセージ）

Discord の受信処理と音声セッションで実運用中。

| フィールド | 型 | 説明 |
|---|---|---|
| `id` | `String` | UUID v4 で自動生成 |
| `source` | `MessageSource` | メッセージの送信元プラットフォーム |
| `content` | `MessageContent` | テキスト、画像、またはマルチパート |
| `sender` | `Sender` | 送信者情報（ID、名前、Bot判定、アバター） |
| `channel` | `Option<Channel>` | チャンネル情報 |
| `timestamp` | `DateTime<Utc>` | 受信タイムスタンプ |
| `metadata` | `HashMap<String, Value>` | プラットフォーム固有のメタデータ |

対になる `OutgoingMessage` / `MessageTarget` は利用者がいなかったため #215 で削除した。
送信は各 transport の具象メソッド（Discord なら `send_to_channel`）で行う。

### MessageSource（送信元）

- `Rest { request_id }` - REST API
- `WebSocket { connection_id }` - WebSocket
- `Discord { guild_id, channel_id }` - Discord
- `Cli { session_id }` - CLI
- `Slack { workspace_id, channel_id }` - Slack
- `Line { user_id }` - LINE

`Discord` 以外のバリアントは現状この crate 経由では生成されない（列挙として残している）。

### MessageContent（コンテンツ）

- `Text(String)` - テキストメッセージ
- `Image { url, alt }` - 画像
- `Multi(Vec<ContentPart>)` - マルチパート（テキスト＋画像の組み合わせ）

## プロトコル定数

`PEER_REVIEW_REQUEST_MARKER` / `PEER_REVIEW_REPLY_MARKER` — ピアレビューのヘッダ組み立て
（discord 側）と system prompt 規約（server 側）の両方が参照する。文字列がズレると
Silent Reply の例外判定が発火せずレビューが silent に死ぬので、必ず定数を使うこと。

## 使い方

```toml
# Cargo.toml
[dependencies]
opencrab-gateway = { workspace = true }
```

feature は 1 つも持たない。この crate はメッセージ型と `GatewayActions` 抽象（ポート）だけを提供し、SDK（serenity / songbird）には依存しない。Discord の具象実装が要るときは `opencrab-discord` を使う。

## 関連ドキュメント

- [Discord ゲートウェイ詳細設定](../../docs/discord.md) - Discord Bot の設定・チャンネル制御・セッション管理の詳細
- [docs/DESIGN.md](../../docs/DESIGN.md) §2.4 - transport 層の現状と方針
