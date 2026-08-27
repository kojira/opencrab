# v3

V3 protocol=2 の独立実装。core crate の wire DTO は依存しない。

| ファイル | 役割 |
|---|---|
| `client.rs` | 1 instance = 1 UDS。`connect` は 1 回（rust-unit）。`spawn` は production 経路（指数 backoff 再接続 + hello replay）。process conformance は `spawn` した binary だけを見る。hello / bind / said / say / close を info |
| `http.rs` | POST / SSE。切断中は POST `503 disconnect`、events は SSE `gate_error`。未 bind は `503 instance_not_ready` |
| `wire.rs` | frame の読写。core DTO を使わない |
| `config.rs` | placement JSON |
| `json.rs` | duplicate member 拒否 |
