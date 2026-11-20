# opencrab-gate-client

Rust gateway 共用の gate socket client / wire / json（protocol 3）。core crate の wire DTO には依存しない。利用者は gateway リポジトリ（[opencrab-gateways](https://github.com/kojira/opencrab-gateways)）の各 gateway で、この crate を core の rev 固定の git 依存で参照する（Issue #1074）。プロトコルの正本は `docs/design/external-gate.md`（D-1099 の節）。

| ファイル | 役割 |
|---|---|
| `client.rs` / `client/` | 1 instance = 1 UDS 接続。`connect` は 1 回だけ接続する。`spawn*` は本番経路で、切れたら backoff して接続し直し `hello` を送り直す（`client/transport.rs`）。`said` と `command`（`/model` など）の送信、`say` / `invoke` の受信と応答、先行生成（`client/provisional.rs`） |
| `wire.rs` / `wire/` | frame の読み書き。`hello`（道具の宣言と環境変数）、`partial`、`provisional`、活動通知、`ok` / `err` 応答、`model_profile` |
| `json.rs` | duplicate member 拒否 |
| `outbound.rs` | core が `"x-opencrab-workspace-file": true` の欄に入れて渡すファイル descriptor の読み取りと検証 |
