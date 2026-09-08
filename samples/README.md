# samples

正典どおりの独立第二実装。production Rust workspace の build / 配布 / 起動必須対象ではない。merge CI の共通 process conformance 行列には載せる。

| 列 | パス | 状態 |
|---|---|---|
| Node.js | `samples/node/web-gateway` | 必須被検体（WEBGATE §5.1） |
| Go | 後続 | Node 適合の完了条件にしない |

起動 ABI は言語共通: `argv[1]=placement.json`。ready は `http_bind` への TCP listen。SUT 切替は `OPENCRAB_CONFORMANCE_SUT`（未設定時は Rust `web-gateway` binary）。言語名分岐・skip・片側 golden は置かない。
