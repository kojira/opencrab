# fixtures

言語非依存の mock-core 手順と正規化期待値。Rust / 後続 Node が同じファイルを読む。

## ファイル

| ファイル | 範囲 |
|---|---|
| `ids.json` | 共有識別子。placement と prelude の源 |
| `hello-bind-said-dedup.json` | hello / bind / said / 同 origin 再送 |
| `say-three-results.json` | say 受理 / empty text の external_rejected / close |
| `activity.json` | activity started/ended と completed_no_reply |
| `frame-too-large.json` | LF 込み 1,048,577 byte で close |
| `frame-duplicate.json` | duplicate member で close |
| `http-post-and-routes.json` | 403 / 202 / 409 / 旧 route 404 |
| `disconnect-unacked.json` | 未 ack 503 と切断後 503 |
| `bind-conflict.json` | 同一 address の後勝ち bind で close |
| `reconnect.json` | production spawn 経路の再 hello / bind |

## スキーマ

```text
{
  name: string,
  prelude: "none" | "hello" | "hello_bind",
  steps: Step[]
}

Step =
  {op:"http_post_async", id, path, body}
| {op:"http_await", id, status, body?}
| {op:"http_post", path, body, status, body_expect?}
| {op:"http_get", path, status}
| {op:"uds_recv", id?, expect}
| {op:"uds_send", frame}
| {op:"uds_send_raw", utf8}
| {op:"uds_send_oversized", byte, count, nl}
| {op:"uds_close"}
| {op:"uds_unlisten"}
| {op:"uds_accept"}
| {op:"sse_open", id, path}
| {op:"sse_recv", id, event, data?}
```

文字列 `$name.field` は直前までの capture を参照する。期待値は既知 field の subset。未知 field と member 順は見ない。
