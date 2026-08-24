# crates/store

永続化。SQLite ひとつ。Discord 最小形（#793）で足した入口:

- `upsert_discord_kind` — `kind_id=discord, protocol_major=2, origin_scope=kind_address, ingress_discovery=membership`
- `discord_launch_decisions` / `discord_launch_decisions_read_only` — present && enabled。secret 値は読まず、非空だけ `start=true`
- `observe_gate_address` — v15 §1.2。policy は subject typed → default `source_row` → hard default `whitelisted=false`
