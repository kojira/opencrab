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
| `inbound.rs` | said → `accept_inbound`。session は `canonical_session_id`（V3.5 reuse）。`delivery_mode` を turn 完了で読む。`kind_id=nostr` は record 前に `NostrSaidAdmit`（V1 アンカー・DM/自己/allow-set）。owner 読取失敗は store_error。record 前に renderer 本文を `sanitize_tool_result_for_log`。record 後に renderer 生本文を転記。`prompt_suffix` は watch と同文面。turn は `OnlySpeaker`。watch Immediate は `WatchAccept(privilege=Some)`。Bundle は `WatchAccept(privilege=None)` + `NostrBundleCoordinator`（member 記録・on_run 抑止・全 receipt 後に turn 0/1） |
| `bundle.rs` | `nostr_bundle_state` の insert/update。最初の非重複 member で manifest 全 origin を `external_origins` と照合し old receipt を先に立てる。重複 origin path は呼ばない。manifest 不一致は store_error |
| `delivery.rs` | `DeliveryEffect` → say 1 回 |
| `delivery_mode.rs` | optional `delivery_mode`（欠落=`say`）。`tool_driven` は inbound Text を NoReply にし、自発の V3 say を渡さない。`kind_id` では分岐しない |
| `listen.rs` | UDS listen と接続状態機械。`enqueue_bind` は lock 失敗・未 live を warn（fail-quiet 禁止）。`web_binding_state` は ack=`ready` / pending=`provisioning` / それ以外=`unavailable`。`wait_bind_ack` は live 消失理由を残す |
| `close.rs` | live close。request id 抽出済みなら err 1 回、未抽出は log のみ（wire 0） |

## 非目標

`docs/design/external-gate.md` §8。別名でも作らない。
