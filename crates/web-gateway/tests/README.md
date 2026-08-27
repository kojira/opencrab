# tests

| 対象 | コマンド | 算入 |
|---|---|---|
| process conformance | `cargo test -p opencrab-web-gateway --test conformance` | 共通合格数。fixture は `conformance/fixtures/` |
| rust-unit | `cargo test -p opencrab-web-gateway --test rust-unit` | 算入しない。frame parser / in-process HTTP / 採取 / harness 不変条件 |
| core_process_e2e | `cargo test -p opencrab-web-gateway --test core_process_e2e` | 実 core 結合 |
| web_conversation_create_e2e | `cargo test -p opencrab-web-gateway --test web_conversation_create_e2e` | 実 core 結合 |

conformance harness は `InstanceClient` / `router()` / `parse_frame_bytes` を import しない。SUT は `CARGO_BIN_EXE_web-gateway`。
