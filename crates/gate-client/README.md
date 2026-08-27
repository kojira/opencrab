# opencrab-gate-client

in-tree Rust gateway 共用の V3 protocol=2 client / wire / json。core crate の wire DTO は依存しない。独立実装の原則は `samples/` にだけ適用する（POLICY-BUILTIN-GATES 裁定 3）。

| ファイル | 役割 |
|---|---|
| `client.rs` | 1 instance = 1 UDS。`connect` は 1 回（rust-unit）。`spawn` は production 経路（指数 backoff 再接続 + hello replay）。`SayPolicy::RejectExternal` は say を投稿せず `external_rejected`。`post_said_with_author` は event ごとの author。hello / bind / said / say / close を info |
| `wire.rs` | frame の読写。core DTO を使わない |
| `json.rs` | duplicate member 拒否 |

抽出元は `crates/web-gateway/src/v3/{client,wire,json}.rs`。機械的移動のみで、挙動は変えない。
