# process conformance

DESIGN-SAMPLES-NODE.md §1。SUT は `argv[1]=placement.json` の executable。既定は Rust `web-gateway`。差し替えは `OPENCRAB_CONFORMANCE_SUT` だけ。harness は一時 UDS の mock core と実 TCP の HTTP/SSE だけで駆動する。言語名分岐・skip・片側 golden は置かない。

## 起動 ABI

placement は `{http_bind,core_socket,instances:[{instance_id,revision,author_id}]}`。ready は `http_bind` への TCP listen 成立だけ。stdout/stderr は見ない。

## fixture

`fixtures/` の JSON をディレクトリ走査で全件読み、全 SUT に同じ手順・同じ正規化期待値で適用する。SUT 名分岐、実装別 skip、片側 golden は置かない。

共通範囲: gateway 側 framing（1,048,576 成功 / LF 未着超過 / invalid UTF-8 / 非 object / duplicate / LF 込み超過）/ hello / bind / said / dedup / say 3 結果（受理 / external_rejected / indeterminate）/ live queue overflow(32) / activity / disconnect / HTTP / SSE。再接続は production spawn 経路。bind-conflict は WEBGATE §8.1。

## 合格数

`cargo test -p opencrab-web-gateway --test conformance` の fixture 件数だけが共通 conformance の合格数。`rust-unit` と lib unit は算入しない。
