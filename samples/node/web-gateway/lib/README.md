# lib

Node web-gateway の変換部品。Rust crate を import しない。

| ファイル | 役割 |
|---|---|
| `fail.js` | invariant 違反は即 nonzero exit |
| `json.js` | UTF-8 fatal、全階層 duplicate 拒否、既知 integer（`protocol`/`revision`/`seq`）の十進保持。未知 field は構文と nested duplicate だけ |
| `wire.js` | frame 上限 1 MiB+LF、hello/said/ok/err、config digest（`{"author_id":…}` の SHA-256 lowerhex） |
| `placement.js` | 配置 JSON の load と起動前検証 |
| `client.js` | 1 instance = 1 UDS。pending を write より先に登録。generation で旧接続を隔離。live queue 32。said 10s。backoff 200ms×2〜8s reset |
| `http.js` | POST / SSE。切断中は 503 `disconnect` / SSE `gate_error`。未 bind は 503 `instance_not_ready` |
