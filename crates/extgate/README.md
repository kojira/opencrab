# opencrab-extgate

External gate V3 最小形。契約・設計・検収は `docs/design/external-gate.md` に自己完結で置く。

## 境界

- core が UDS を listen し、敷設・認可・会話・delivery を所有する。`gate.listen_socket` が空・欠落なら listen しない（recover は実行する）。
- gateway は UDS に connect し、外部 I/O と wire 変換だけを持つ。
- 接続は process-local registry。startup は空。DB から復元しない。

## モジュール

| ファイル | 役割 |
|---|---|
| `error.rs` | §5.4 の 25 code と HTTP / wire 写像 |
| `ids.rs` | UUID・digest・`session_id_for_binding`・時刻 |
| `json.rs` | 全階層 duplicate member 拒否 |
| `bearer.rs` | operator token の読取・定数時間比較 |
| `registry.rs` | live 接続の process-local 表。`GateProbe` は `#[cfg]` 隔離 |
| `admin.rs` | 6 operation |
| `protocol.rs` | frame と message の読写 |
| `inbound.rs` | said → `accept_inbound` |
| `delivery.rs` | `DeliveryEffect` → say 1 回 |
| `listen.rs` | UDS listen と接続状態機械 |
| `close.rs` | live close。request id 抽出済みなら err 1 回、未抽出は log のみ（wire 0） |

## 非目標

`docs/design/external-gate.md` §8。別名でも作らない。
