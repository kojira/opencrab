# Discord ゲートウェイ 詳細設定ガイド

opencrab の Discord 連携に関する設定・運用ガイド。

## 概要

opencrab は [serenity](https://github.com/serenity-rs/serenity) を使用して Discord Bot として動作する。メッセージの受信・応答、チャンネル制御、エージェントごとの Bot 管理などの機能を提供する。

### 関連 crate

| crate | 役割 |
|---|---|
| `opencrab-gateway` | Gateway trait の定義、`DiscordGateway` の実装（低レイヤー） |
| `opencrab-discord` | メインループ、マネージャー、エージェントツール（高レイヤー） |

## 設定

### 基本設定（config/default.toml）

```toml
[gateway.discord]
enabled = true
token = "${DISCORD_TOKEN}"
guild_ids = []
agent_ids = ["crab"]
owner_discord_id = "${OWNER_DISCORD_ID}"
```

環境ごとに変わる値（トークン、オーナー ID）は TOML に直接書かず、`.env` から
`${...}` で参照する（`.env.example` を参照）。

```dotenv
DISCORD_TOKEN=
OWNER_DISCORD_ID=123456789012345678
```

### DiscordGatewayConfig の各フィールド

| フィールド | 型 | デフォルト | 説明 |
|---|---|---|---|
| `enabled` | `bool` | `false` | Discord ゲートウェイの有効/無効 |
| `token` | `String` | (必須) | Discord Bot トークン。環境変数 `${DISCORD_TOKEN}` を推奨 |
| `guild_ids` | `Vec<u64>` | `[]` | **現在未使用**。将来の Guild フィルタリング用予約フィールド |
| `agent_ids` | `Vec<String>` | `[]` | この Bot が応答するエージェント ID のリスト |
| `owner_discord_id` | `String` | `""` | DM に応答するオーナーの Discord User ID。環境変数 `${OWNER_DISCORD_ID}` を推奨。空（未設定）ならオーナー無しとして扱われる |

### Discord Bot トークンの取得

1. [Discord Developer Portal](https://discord.com/developers/applications) でアプリケーションを作成
2. Bot セクションでトークンを生成
3. 環境変数 `DISCORD_TOKEN` に設定

### 必要な Bot Intents

Bot には以下の Gateway Intents が必要（コード内で自動設定）:

- `GUILD_MESSAGES` - サーバーメッセージの受信
- `DIRECT_MESSAGES` - DM の受信
- `MESSAGE_CONTENT` - メッセージ内容の取得（Privileged Intent）
- `GUILDS` - サーバー情報の取得

> **注意**: `MESSAGE_CONTENT` は Privileged Intent のため、Discord Developer Portal の Bot 設定で明示的に有効化する必要がある。

## チャンネル制御

opencrab のチャンネル制御は DB ベースで管理されている。

### 制御の階層

```
1. DM制御（owner_discord_id による）
   ↓
2. Bot自身のメッセージスキップ（無限ループ防止）
   ↓
3. チャンネル readable/writable 制御（DBベース）
```

### 1. DM 制御

`owner_discord_id` の設定によって DM の受け付けが変わる:

| 設定 | 動作 |
|---|---|
| User ID を指定 | そのユーザーからの DM のみ応答 |
| 空文字 `""` | すべてのユーザーからの DM を受け付け |

### 2. Bot メッセージの自動スキップ

Bot 自身が送信したメッセージは自動的に無視される。これにより Bot がBotの応答に反応する無限ループを防止する。

### 3. チャンネルの readable/writable 制御

DB テーブル `discord_channel_config` で各チャンネルの読み書き権限を管理する。

| フィールド | 型 | 説明 |
|---|---|---|
| `readable` | `bool` | `false` にするとそのチャンネルのメッセージを無視 |
| `writable` | `bool` | `false` にするとそのチャンネルへの返信を抑止 |

**デフォルト動作**: DB に未登録のチャンネルは `readable = true`, `writable = true` として扱われる（全チャンネルで読み書き可能）。

## エージェントによる自己制御（GatewayActions）

エージェントは以下のツールを通じて Discord の設定を動的に変更できる。

### discord_list_guilds

Bot が参加しているサーバーの一覧を取得する。

### discord_list_channels

指定サーバーのチャンネル一覧を取得する。各チャンネルの `readable` / `writable` 状態も含まれる。

### discord_channel_config

チャンネルの `readable` / `writable` 設定を変更する。

**使用例**: エージェントが自ら「このチャンネルは監視不要」と判断して `readable = false` に設定するなど、自律的なチャンネル管理が可能。

## セッション管理

Discord メッセージはチャンネルごとに自動的にセッションに紐付けられる。

### セッション ID の形式

| 種別 | 形式 | 例 |
|---|---|---|
| サーバーチャンネル | `discord-{guild_id}-{channel_id}` | `discord-123456-789012` |
| DM | `discord--{channel_id}` | `discord--345678` |

同じチャンネルでの会話は同一セッションとして継続的に管理される。

## Per-Agent Bot（DiscordGatewayManager）

エージェントごとに異なる Discord Bot トークンを割り当てることで、複数のエージェントがそれぞれ独立した Bot として動作できる。

### 仕組み

`DiscordGatewayManager` が全エージェントの Bot インスタンスを一元管理する。

```
DiscordGatewayManager
  ├── Agent "crab"  → Bot Token A → Discord Bot A
  ├── Agent "sage"  → Bot Token B → Discord Bot B
  └── Agent "clerk" → Bot Token C → Discord Bot C
```

### DB テーブル: agent_discord_config

| フィールド | 型 | 説明 |
|---|---|---|
| `agent_id` | `String` | エージェント ID |
| `bot_token` | `String` | エージェント固有の Bot トークン |
| `owner_discord_id` | `String` | DM 応答先オーナー ID |
| `enabled` | `bool` | 有効/無効 |

### 起動時の自動復元

サーバー起動時に `restore_from_db()` が呼ばれ、DB に登録済みのエージェント Bot 設定を自動的に復元・起動する。

## メッセージ処理の流れ

```
Discord サーバー
    │
    ▼ (serenity)
DiscordGateway.recv()
    │
    ├── Bot自身のメッセージ → スキップ
    ├── DM で owner_discord_id 不一致 → スキップ
    │
    ▼
IncomingMessage 生成
    │  metadata: guild_name, guild_icon_url, channel_name
    │
    ▼
run_discord_loop() (opencrab-discord)
    │
    ├── チャンネル readable チェック → false ならスキップ
    │
    ▼
エージェント処理
    │
    ▼
OutgoingMessage 生成
    │
    ├── チャンネル writable チェック → false なら送信しない
    │
    ▼
DiscordGateway.send_to_channel()
    │
    ├── 2000文字超 → split_message() で自動分割
    │
    ▼
Discord サーバー
```

## メタデータ

Discord から受信した `IncomingMessage` には以下のメタデータが付加される:

| キー | 説明 |
|---|---|
| `guild_name` | サーバー名 |
| `guild_icon_url` | サーバーアイコン URL |
| `channel_name` | チャンネル名 |

## 将来の拡張

### guild_ids によるフィルタリング

`config/default.toml` の `guild_ids` フィールドは現在のコードでは使用されていない。将来的に特定の Guild のみに応答するフィルタリング機能として実装される予定の予約フィールド。
