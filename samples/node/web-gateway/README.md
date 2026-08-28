# samples/node/web-gateway

HTTP/SSE ⇄ V3 protocol=2（UDS）の変換だけ。判断しない。Bearer を持たない。fail-loud（例外 / unhandled rejection / invariant 違反は nonzero exit）。fallback しない。

## 起動

```text
./web-gateway.js /path/to/placement.json
```

placement schema は Rust と同一: `{http_bind,core_socket,instances:[{instance_id,revision,author_id}]}`。`http_bind` は loopback。`core_socket` は nonempty 絶対 path。`instances` は nonempty、`instance_id` は重複しない canonical lowercase UUID、`revision` は positive u64、`author_id` は nonempty。

ready は `http_bind` への listen 成立だけ。config / listen failure は listen 前に nonzero exit。UDS 切断後も HTTP listen は落とさない。

## 固定値（WEBGATE §8）

- live queue 容量 32。overflow は `external_rejected`
- SSE error event 名 `gate_error`
- said 待ち 10s。超過は切断
- reconnect backoff 初期 200ms、2 倍、上限 8s、接続成功で初期値へ reset

## 入口

| method / path | 役割 |
|---|---|
| `POST /api/web-conversations/{session_id}/messages` | `{client_message_id,text,attachments}` → V3 `said`。202 は said ack まで。turn 実行中の別 UUID も送る。`409 conversation_busy` はキュー満杯の Busy だけ |
| `GET /api/web-conversations/{session_id}/events` | say / activity / `completed_no_reply` / `gate_error` |
| `GET\|POST /rooms/{room}/messages` | 404 |
| `GET /chat` | 404 |

`package.json` の `dependencies` は空。npm を走らせない。

## 検収

`OPENCRAB_CONFORMANCE_SUT` に本 script の path を渡して `cargo test -p opencrab-web-gateway --test conformance`。fixture は `conformance/fixtures/`。harness に Node 分岐は無い。
