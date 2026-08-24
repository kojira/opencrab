# crates/store

永続化。SQLite ひとつ。Discord 最小形（#793）で足した入口:

- `subject_create` / `subject_replace` / `subject_patch` / `subject_delete` — agent subject 書き（P2）。DELETE の tombstone は discord instance のみ。subject 0 行でも ancillary は消す。
- `soul_preset_create` / `soul_preset_delete` / `soul_preset_apply` / `soul_preset_list` — B 表 `soul_presets`。Apply は `SubjectPatch`。
- `agent_direct_message` — place 保証 + said 注入。

- `upsert_discord_kind` — `kind_id=discord, protocol_major=2, origin_scope=kind_address, ingress_discovery=membership`
- `discord_launch_decisions` / `discord_launch_decisions_read_only` — present && enabled。secret 値は読まず、dedicated かつ非空だけ `start=true`。`shared:*` は token 非空でも `start=false`（v15 §8 未実装・#793）
- `observe_gate_address` — v15 §1.2。policy は subject typed → default `source_row` → hard default `whitelisted=false`
