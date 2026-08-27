# v3

V3 protocol=2 の独立実装。core crate の wire DTO は依存しない。

`client` / `wire` / `json` は `opencrab-gate-client` を再 export する。process conformance は `spawn` した binary だけを見る。

| ファイル | 役割 |
|---|---|
| `http.rs` | POST / SSE。切断中は POST `503 disconnect`、events は SSE `gate_error`。未 bind は `503 instance_not_ready` |
| `config.rs` | placement JSON |
