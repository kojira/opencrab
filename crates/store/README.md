# crates/store

永続化。SQLite ひとつ。Discord 最小形（#793）で足した入口:

- `subject_create` / `subject_replace` / `subject_patch` / `subject_delete` — agent subject 書き（P2）。DELETE の tombstone は discord instance のみ。subject 0 行でも ancillary は消す。
- `soul_preset_create` / `soul_preset_delete` / `soul_preset_apply` / `soul_preset_list` — B 表 `soul_presets`。Apply は `SubjectPatch`。
- `skill_create` / `skill_update` / `skill_set_active` / `skill_archive` / `skill_seed_standard` / `skill_list` — 台帳 `skills`（旧表不書）。state は archived/active/retired の閉表。
- `memory_index_clear` / `memory_index_policy_update` / `memory_index_build` / `memory_index_rebuild` / `memory_index_merge` / `daily_log_index_rebuild` / `daily_log_index_run` — B 表 `memory_index_*`。キーは旧 agent_id UUID。表が無いと Store エラー。LLM が要る構築は未索引データがあると fail loud。
- `forget` — curated DELETE（`memories` を subject+id で消す）。
- `agent_direct_message` — place 保証 + said 注入。

- `upsert_discord_kind` — `kind_id=discord, protocol_major=2, origin_scope=kind_address, ingress_discovery=membership`
- `discord_launch_decisions` / `discord_launch_decisions_read_only` — present && enabled。secret 値は読まず、dedicated かつ非空だけ `start=true`。`shared:*` は token 非空でも `start=false`（v15 §8 未実装・#793）
- `observe_gate_address` — v15 §1.2。policy は subject typed → default `source_row` → hard default `whitelisted=false`
