# web-gateway

Web 会話の独立 binary。HTTP/SSE を V3 protocol=2（UDS）へ変換するだけ。判断しない。Bearer を持たない。core crate の wire DTO に依存しない。

## 入口

| method / path | 役割 |
|---|---|
| `POST /api/web-conversations/{session_id}/messages` | body exact `{client_message_id,text,attachments}` → V3 `said`。`202` は said ack まで |
| `GET /api/web-conversations/{session_id}/events` | say / activity / `completed_no_reply` の SSE |
| `GET\|POST /rooms/{room}/messages` | 404。alias しない |
| `GET /chat` | 404。redirect しない |

`session_id` は binding address（legacy logical session と byte-equal）。未 ack は `503`。同一 binding の別 UUID は `409 conversation_busy`。`ok.seq=null` は `403 {state:"not_admitted"}`。

## 配置

operator が書いた placement JSON を argv で渡す。

```text
web-gateway /path/to/placement.json
```

`http_bind` は loopback のみ。`core_socket` は絶対 path。instance ごとに UDS を 1 本。config bytes は byte-exact `{"author_id":<json-string>}`（空白なし）。digest は SHA-256 lowerhex。

## 検収

`cargo test -p opencrab-web-gateway --test conformance`

## 関連

- 本設計: `DESIGN-WEBGATE.md`
- 線の契約: `DESIGN-EXTGATE-V3.md`（変更しない）
