# Issue #1006 S8 — report redaction RED

Contract: `docs/evidence/issue-1006-s8-contract.md` §S8-D2 and §S8-D5. The added assertion runs the actual `command::run_import` and `command::run_project` against disposable core and S5 Discord databases, then checks the tool-produced report and verification artifacts. It retains the non-secret credential source category, while refusing synthetic raw agent/external IDs or credential plaintext anywhere in either artifact.

Command: `cargo test -q -p opencrab-gateway-migrate --test s8 s8_import_and_project_are_offline_idempotent_and_preserve_core_rows -- --exact --nocapture`

Result before any production edit: assertion-level RED, exit 101. The existing migration and projection complete; the new assertion independently inspects both artifacts and reports that the import report **and** verification contain synthetic raw agent/external identifiers. Full focused transcript: `issue-1006-s8-report-red.log`.

The other requested fixtures cannot honestly be added in this checkpoint:

- S5 Discord and Nostr `instances.credential_envelope` columns are `TEXT NOT NULL`. An existing valid row with `NULL` cannot be constructed without changing the production schema or fabricating an invalid fixture. Their absence-of-credential case is a *new* instance, already handled by the existing insertion path.
- S5 Web permits `NULL`, but the approved migration contract does not specify whether a legacy Web `trusted_users.user_id` denotes the persisted `instances.author_id`, the one `identity_projections.external_id='bearer'`, or another identity. The existing `instance_semantic` also cannot match Web's distinct stored columns. No Web credential assertion can isolate its seam until the source-to-Web authority mapping is decided.

No migrator production source, S5 schema, or S9+ code was changed here. This checkpoint does not claim S8 GREEN.
