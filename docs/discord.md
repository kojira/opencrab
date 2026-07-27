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
`${...}` で参照する。`cp .env.example .env` してから値を埋める。

`.env` には自分の Discord ユーザー ID を書く（下記はプレースホルダなのでそのまま
使えない。実際の 18 桁程度の数値 ID に置き換える）:

```dotenv
OWNER_DISCORD_ID=<your-discord-user-id>
```

> **注意**: `.env.example` には `DISCORD_TOKEN=` の行もある。既に `DISCORD_TOKEN` を
> 設定済みの環境で上のブロックだけを追記する分には問題ないが、空の値を含む行を
> まとめて貼り付けると設定済みの値を空で上書きしてしまう。値を持つ行は個別に編集すること。

### DiscordGatewayConfig の各フィールド

| フィールド | 型 | デフォルト | 説明 |
|---|---|---|---|
| `enabled` | `bool` | `false` | Discord ゲートウェイの有効/無効 |
| `token` | `String` | (必須) | Discord Bot トークン。環境変数 `${DISCORD_TOKEN}` を推奨 |
| `guild_ids` | `Vec<u64>` | `[]` | **現在未使用**。将来の Guild フィルタリング用予約フィールド |
| `agent_ids` | `Vec<String>` | `[]` | この Bot が応答するエージェント ID のリスト |
| `owner_discord_id` | `String` | `""` | オーナーの Discord User ID。環境変数 `${OWNER_DISCORD_ID}` 経由で渡す。**空にしないこと**（下記「オーナー未設定時の挙動」を参照） |

### オーナー未設定時の挙動

`owner_discord_id` が空（`.env` の `OWNER_DISCORD_ID` を入れ忘れた、または
`${OWNER_DISCORD_ID}` が未定義で空文字に展開された）場合、判定はすべて
**拒否側（fail-closed）へ倒れる**（#174）。空白のみの値も未設定と同じ扱い。

| 影響 | 内容 |
|---|---|
| オーナー専用機能 | 誰も owner と判定されないため全て使えなくなる |
| DM の受け付け | 信頼ユーザーが 1 件も登録されていないエージェントは、**誰からの DM も受け付けない**（下記「1. DM 制御」参照） |
| オーナー専用 UI | フォーム/モーダル/ボタンを**誰も操作できない** |

オーナーを設定し忘れると、権限が緩むのではなく「エージェントが DM に応答しない」
形で現れる。心当たりがあるときはまず起動時の警告ログを確認すること。

> **#174 以前からの挙動変更**: かつては「信頼ユーザーの登録が 0 件でオーナーも
> 未設定なら全許可」というフォールバックがあり、DM もオーナー専用 UI も誰にでも
> 開いていた。オーナー未設定のまま運用していた環境では、この変更で DM への応答が
> 止まる。オーナーを設定するか、信頼ユーザーを登録すれば復旧する。

**本番では必ずオーナーを設定すること。** owner が空のままゲートウェイが起動すると
`tracing::warn!` で警告が出る。発火は「サーバー起動時」だけではない（per-agent 経路は
ダッシュボードからの保存時にも起動するため、そのたびに出る）。経路ごとの条件は次の通り。

| 経路 | 警告のタイミング | 条件 |
|---|---|---|
| 共有（TOML）ゲートウェイ | サーバー起動時 | `[gateway.discord]` が `enabled = true` かつトークンが空でない（＝ゲートウェイが実際に起動する）状態で `owner_discord_id` が空 |
| per-agent ゲートウェイ | 各エージェントのゲートウェイ起動時（ダッシュボードからの設定保存時／サーバー再起動時の DB からの復元時） | そのエージェントの `owner_discord_id` が空（`agent_id` 付きで警告） |

トークンを持たない状態（`enabled = true` だが `DISCORD_TOKEN` 未設定）では共有
ゲートウェイは起動しないため、共有側の警告は出ない。「トークンが空でない」の判定は
前後の空白を無視する（`DISCORD_TOKEN=" "` はトークン無しと同じ扱い）。起動判定と
警告判定は同じ述語（`opencrab_discord::gateway_will_start`）を参照するので、
「起動するのに警告が出ない」取りこぼしは起きない。

オーナー判定そのものは下位クレートの 1 実装（`opencrab_core::owner::is_owner_id`）に
集約してあり、server / discord の両方がそれを使う（#174）。「未設定なら制限しない」
という緩め方をどこかに書き足すと、この文書の表と実挙動がずれる。

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

`owner_discord_id` と信頼ユーザー登録（`trusted_users` テーブル）の組み合わせで
DM の受け付けが変わる:

| `owner_discord_id` | 信頼ユーザー登録 | 動作 |
|---|---|---|
| User ID を指定 | 0 件 | オーナーからの DM のみ応答 |
| User ID を指定 | 1 件以上 | オーナー + 信頼ユーザーからの DM に応答 |
| 空文字 `""`（未設定） | 0 件 | **どの DM にも応答しない**（誰もオーナーではないため） |
| 空文字 `""`（未設定） | 1 件以上 | 信頼ユーザーからの DM のみ応答 |

許可されるのは「オーナー」か「Discord 経路の信頼ユーザー」だけで、それ以外は
拒否する（#174）。「登録が 0 件なら制限を外す」という段は無い。オーナー未設定は
制限が緩む方向ではなく、**DM に一切応答しない**方向に効くので、本番では必ず設定
すること。

信頼ユーザーの判定は経路（platform）ごとに切られている（#214）。web / REST に
登録があっても Discord の DM は開かない。

なお、オーナー ID の比較は前後の空白を無視する（`.env` やダッシュボードからの
コピペで空白が混ざっても一致する）。空白のみの値は未設定（空文字）と同じ扱い。

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
| `owner_discord_id` | `String` | DM 応答先オーナー ID（保存時に前後の空白を除去） |
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
