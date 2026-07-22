# nostaro ↔ OpenCrab インターフェース契約

OpenCrab の Nostr sub-gateway（`crates/nostr`）は、Nostr プロトコルの実処理を自作 CLI
[**nostaro**](https://github.com/kojira/nostaro) に subprocess で委譲する。本書はその
呼び出し契約と、そのために nostaro 側へ必要な**汎用的な**改造を定義する。

OpenCrab 側は本契約の I/O をモックしてユニットテスト済み（`crates/nostr`）。実機で
繋ぐには nostaro 側に下記 3 点の改造が必要（いずれも一般の Nostr ツールとして有用な機能）。

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
