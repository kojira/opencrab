# nostaro ↔ OpenCrab インターフェース契約

OpenCrab の Nostr sub-gateway（`crates/nostr`）は、Nostr プロトコルの実処理を自作 CLI
[**nostaro**](https://github.com/kojira/nostaro) に subprocess で委譲する。本書はその
呼び出し契約と、そのために nostaro 側へ必要な**汎用的な**改造を定義する。

OpenCrab 側は本契約の I/O をモックしてユニットテスト済み（`crates/nostr`）。実機で
繋ぐには nostaro 側に下記の改造が必要（`--config` / `pubkey` / `vanity --json` /
`watch` フィルタ+`--json`。いずれも一般の Nostr ツールとして有用な機能）。

## 前提: 鍵の隔離

OpenCrab はエージェント毎に一意な config を使い、秘密鍵の共有事故を防ぐ。全 nostaro
呼び出しに `--config <path>` を付ける:

```
data/agents/<agent_id>/nostr/config.toml
```

config.toml は既存スキーマ（`secret_key` / `relays` / `blossom_server` / ...）のまま。
OpenCrab が起動時に DB の per-agent 設定からこのファイルを materialize する。

### 改造 1: `--config <path>` グローバルフラグ（汎用）

現状 config は `~/.nostaro/config.toml` にハードコード。任意パスを指定できる
`--config <path>`（または `NOSTARO_CONFIG` env）を全サブコマンドに追加する。
指定時はそのファイルを config として読む。複数アカウントの運用一般に有用。

### 改造 1b: `nostaro pubkey`（必須・汎用）

`nostaro --config <p> pubkey` で、その config の**公開鍵（hex）を stdout に出力**する。
OpenCrab は起動時にこれを取得し、**自分の投稿を受信ループでスキップ**する（自己返信
無限ループ＋LLM 支出の防止）。取得できないと OpenCrab はゲートウェイを**起動しない**
（fail-closed）ので、実機接続にはこのコマンドが必須。

### 改造 1c: `nostaro vanity --json`（鍵生成・汎用）

`nostaro vanity --json [--prefix=<bech32>]` で**新規鍵を生成**し、1 行の JSON を stdout に
出力する。OpenCrab はこれをエージェントの Nostr アイデンティティ生成に使う（ダッシュボード /
`POST /api/agents/{id}/nostr/generate`）。

```
nostaro vanity --json [--prefix=<bech32prefix>]
```

- `--prefix` は npub（`npub1...`）の `1` 以降に前置される bech32 文字列。省略/空なら通常の
  ランダム鍵を即返す。
- **config 非依存**: 既存 config/秘密鍵を読まず、純粋に生成するだけ（OpenCrab は生成時に
  `--config` を付けない）。
- 出力（stdout, 1 行 JSON）:

```json
{ "nsec": "nsec1...", "npub": "npub1...", "pubkey": "<hex pubkey>" }
```

- `nsec` は必須。`npub`/`pubkey` は任意（あれば表示・自己ループ防止に使える）。
- 進捗を出す場合は stderr へ（OpenCrab は stdout の最後の JSON 行を採用する）。

OpenCrab 側の防御: prefix は bech32 charset かつ最大 3 文字に制限してから渡す（探索コストは
`32^len` で増えるため、同期リクエストがハングしないよう保守的に制限）。nostaro 側は通常の
生成でよい。

### 引数の渡し方（OpenCrab 側の防御・nostaro は通常のパーサでよい）

OpenCrab は positional を取るサブコマンド（post/reply/dm/zap/upload）で **`--`
（オプション終端）を挟み**、watch のフラグ値は **`--flag=value` の = 形式**で渡す。
target/text/recipient は受信イベント/モデル由来（`-` 始まりの値がフラグに化ける
引数インジェクションを防ぐため）。nostaro 側は `--` と `--flag=value` を通常どおり
解釈すればよい（特別対応不要）。

## 送信（既存サブコマンドをそのまま利用）

OpenCrab のツール（`nostr_*`）は既存 CLI を呼ぶだけ。改造不要:

| OpenCrab ツール | nostaro 呼び出し |
|---|---|
| `nostr_post`   | `nostaro --config <p> post "<text>"` |
| `nostr_reply`  | `nostaro --config <p> reply <target> "<text>"` |
| `nostr_dm`     | `nostaro --config <p> dm send <recipient> "<text>"` |
| `nostr_zap`    | `nostaro --config <p> zap <recipient> <amount> -m "<message>"` |
| `nostr_upload` | `nostaro --config <p> upload <path>` |

成功時 exit 0・stdout に結果（投稿なら note id / upload なら URL）を出す前提。

## 受信（`watch` の汎用改造が必要）

現状の `watch` は Discord webhook 専用でフィルタが弱い。OpenCrab は次の形で spawn し、
**stdout の JSONL** を読む:

```
nostaro --config <p> watch --json \
  --relay wss://yabu.me --relay wss://r.kojira.io \
  [--author <npub|hex>]... [--keyword <kw>]... [--kind <n>]...
```

### 改造 2: `watch` にフィルタフラグ追加（汎用）

- `--author <npub|hex>`（複数可）: 指定 author のみ。
- `--keyword <kw>`（複数可）: content にいずれかを含むもののみ。
- `--kind <n>`（複数可）: 指定 kind のみ（未指定は kind:1）。
- `--relay <url>`（複数可）: **このフラグで指定したリレーのみに接続**する
  （config の `relays`/`default_relays` は使わない）。OpenCrab は許可リレーを
  明示フラグで渡すため、config 由来の別リレーに繋がせない。

### 改造 3: `watch --json`（汎用）

`--json` 指定時、Discord webhook を送らず、**マッチしたイベント1件を1行の JSON**として
stdout に出力する（JSONL）。スキーマ（`crates/nostr/src/event.rs` の `NostrEvent`）:

```json
{
  "id": "<hex event id>",
  "pubkey": "<hex pubkey>",
  "npub": "npub1...",          // 任意
  "note_id": "note1...",        // 任意（返信対象指定に使う）
  "author_name": "kojira",      // 任意（プロフィール由来）
  "created_at": 1700000000,
  "kind": 1,
  "content": "本文",
  "tags": [["p", "..."], ["e", "..."]]
}
```

- 未知フィールドがあっても OpenCrab 側は無視する（前方互換）。
- JSON でない行（ログ等）は OpenCrab がスキップするので、進捗ログを stderr に出すのは自由。
- プロセスは stdin クローズ / SIGTERM で終了する（OpenCrab が停止時に kill）。

## OpenCrab 側の動作（参考）

- `crates/nostr`: config/event/CLI ラッパー/送信アクション（本 PR）。
- Phase 1b: per-agent マネージャ（watch を spawn し JSONL を読む → `RunRequest` →
  `run_agent_response` → 返信）、`/api/agents/{id}/nostr`、ダッシュボード UI。
