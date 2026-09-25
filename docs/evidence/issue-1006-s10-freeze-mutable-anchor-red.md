# S10 read-only freeze: mutable heartbeat anchor RED

At `15985aa`, a disposable fully projected S8 core plus Discord store was advanced through the existing `opencrab_db::queries::set_session_last_fired` query before a new matched post-QC freeze. The fixture asserted that `(agent-a, session-1).last_fired_at` advanced, created the freeze set afterward, then ran the actual `opencrab-gateway-migrate verify-freeze` CLI.

`cargo test -p opencrab-gateway-migrate --test s8 s10_verify_freeze_accepts_post_qc_heartbeat_last_fired_advance -- --exact` failed at the acceptance assertion with `opencrab-gateway-migrate: projection marker mismatch` (exit 101). No production source was edited for this RED. The existing frozen state and marker are valid; `freeze.rs` reuses `projection::verify_already_applied`, which recomputes the initial target fingerprint from the now-advanced `last_fired_at`. The unadvanced S10 test remains a separate control.

Required GREEN is limited to freeze verification comparing the persisted immutable projection marker and stable source/edge/subject/heartbeat lineage, while retaining snapshot/live hashes and identity checks. S8's ordinary stopped successful-rerun check is unchanged.
