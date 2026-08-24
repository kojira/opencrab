# crates/discord-gate

`opencrab-discord-gate` は Discord の非 voice I/O と protocol 2 変換だけを持つ。core / store には依存しない。

## 契約

- 1 bot token = 1 instance = 1 process。token は `OPENCRAB_GATE_TOKEN`。argv / log に出さない。
- hello は protocol 2 / `kind_id=discord` / `origin_scope=kind_address` / `ingress_discovery=membership`。
- 接続: hello → Discord REST current-user → Gateway READY → `ready`。boot 失敗は hello のあと `failed(code)`。
- ingress は Message Create → `said`。discovery は v15 §2.2。intents は `GUILDS | GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT`。
- effect は `say` / `react` / `ui`。`ui create` の renderer はこの slice に無いので fail loud。
- `read` は送らない。bind / unbind は受け取っても membership では core が送らない。

## 起動

launcher が `present && enabled && secret 非空` の **dedicated** discord instance を列挙し、`opencrab-gate-runner <db> <uuid> <sock> opencrab-discord-gate` を exec する。secret 空は 1 行ログして未起動。`shared:*` は token 非空でも起動拒否し、§8 未実装を #793 で追跡する。token は argv / log に出さない。
