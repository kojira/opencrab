# Issue #1006 S8 critical-path implementation contract (schema 56 only)

This contract is limited to the normal, stopped, operator-controlled migration required for behavior-preserving ownership separation. It is not a reusable migration framework. The ordinary v56 core migration creates `api_principals`; legacy source tables remain until S10. `opencrab-gateway-migrate` accepts only `PRAGMA user_version=56`, requires the named columns it reads or writes, and fails before writing when one is missing. Extra columns do not fail S8 merely because they exist.

Issue #1016 owns non-gating migration hardening: full-schema equality, generalized progressed-lifecycle reconciliation, read-once TOCTOU protection, partial-operation artifacts/adoption, lost-response provenance reconstruction, stale-backup/free-space proofs, generalized strict manifest evolution, and resumable partial progress. Commit `af7a9e303ef2853ba60e2e74c4e5b6359369c704` and its hardening RED assertions are superseded for #1006 and must be reverted before S8 is accepted.

## S8-D1 — existing REST principal behavior

`api_principals` replaces only the existing REST lookup `(platform='rest', user_id, agent_id)` from `trusted_users`. The normal v56 migration creates:

```sql
CREATE TABLE api_principals (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  permission TEXT NOT NULL,
  created_by TEXT NOT NULL,
  created_at TEXT NOT NULL,
  display_name TEXT NOT NULL DEFAULT '',
  UNIQUE(user_id, agent_id)
);
CREATE INDEX idx_api_principals_agent ON api_principals(agent_id);
```

S8 preserves all seven non-platform source values byte-for-byte. Only exact `platform='rest'` rows may target this table. An absent row is inserted by `project-core-state`; an identical row is an idempotent no-op; a conflicting primary or `(user_id,agent_id)` key fails. The existing permission parser is unchanged, including REST downgrade of owner-equivalent `owner` and `co-agent` to `TrustedUser`, unknown permission to `TrustedUser`, and exclusion of Web rows.

## S8-D2 — approval, fingerprints, and verification output

The approval is a UTF-8 JSON object with required typed fields:

- `version=1`, UUID `operation_id`, fixed UTC `created_at`, `core_user_version=56`, and `source_core_sha256`;
- the complete participating destination inventory `(kind_id,path_id,schema)`;
- one disposition per present `trusted_users` fingerprint and one or more explicit destination edges;
- complete channel, watch, and credential-source edges for present source rows.

The parser validates those required fields and their types. It does not pass unknown input fields through to verification output or provide a generalized manifest-evolution contract. Arrays are normalized into deterministic key order before hashing; callers need not pre-sort them.

The verification output is produced by the tool, not copied from arbitrary input. It contains the approval digest, matched backup inventory/digest, per-source-table count/fingerprint digest, per-destination before/after logical digest and mapped-row counts, core before/after digest, inserted-or-accepted `api_principals`, heartbeat targets, and the unchanged core `deliveries` count/digest. It contains no credential plaintext, master-key path, external identifier, config bytes, raw source row, DB path, or socket path. Credential evidence is limited to source descriptor, envelope digest, and `credential_configured=true`.

Source fingerprints remain stable and exact. `LP(x) = u64be(length) || x`; encode:

```text
"opencrab/s8/source-row/v1\0"
|| LP(UTF8 table name)
|| u64be(56)
|| u32be(column count)
|| for each committed column:
     LP(UTF8 column name) || (0x00 for NULL; otherwise 0x01 || LP(canonical value))
```

TEXT uses exact UTF-8, INTEGER minimal base-10 ASCII, and BLOB raw bytes. Committed column orders and vectors are:

- `trusted_users`: `id,user_id,agent_id,permission,created_by,created_at,display_name,platform` → vector hash `2c6850f418281b0b4ede33a1a3ab379cef850e7a85dfd7d2679ee9209a5526a7`;
- `channel_config`: `channel_id,agent_id,guild_id,channel_name,readable,writable,whitelisted,heartbeat_enabled,heartbeat_interval_secs,heartbeat_instructions,updated_at` → `e9099467f8ece3abb668571cab576378cf02d894f2eaf911005c85c5c50f6348`;
- `session_watches`: `id,session_id,agent_id,interval_secs,filter_json,created_at` → `8150c628708b328fe4c4dfdf816caa806738fda29ea2fac1304532c62ae8911d`;
- `agent_discord_config`: `agent_id,bot_token,owner_discord_id,enabled,updated_at,bot_user_id` → `0db7c5133b00d98943329f5fa1cb9fff1bc6f262f37a111875246f831da5514e`;
- `agent_nostr_config`: `agent_id,secret_key,relays_json,filter_json,enabled,updated_at,owner_pubkey,self_pubkey` → `f37159af0363c135d48435ed493b34f05ecea5ae3869294712ec82a6214cc788`.

Every present source row has exactly one disposition and at least one edge. Duplicate, missing, changed, colliding, or zero-edge dispositions fail before backup/write.

## S8-D3 — supported source and destination profile

The v56 core must contain the columns S8 uses in `agents`, `sessions`, `agent_sessions`, `gate_instances`, `gate_bindings`, `deliveries`, `trusted_users`, `channel_config`, `session_watches`, `session_heartbeat_config`, `session_heartbeat_instructions`, and `api_principals`. Historical `agent_discord_config` and `agent_nostr_config` are optional; when present, their required named columns are read. No old version, missing required column, or runtime fallback is supported. Extra unrelated columns are ignored.

Every participating destination must be one of the current S5 Discord/Nostr/Web schemas and expose the required tables/columns used by its mapper. The migration accepts actual existing destination rows only when their migration-owned semantic fields equal the approved mapped values. It does not rewrite process/lifecycle progress fields, generalize lifecycle reconciliation, or adopt another operation's partial work. A semantic conflict fails and the operator restores the complete matched backup set.

Histories, agents, positive subject IDs, sessions, memberships, generic instances/bindings, inbound deduplication, and the single core `deliveries` table are proof-only retained data and remain byte/logically unchanged.

## S8-D4 — complete current-store mapping

S8 never derives a new instance, session, binding, agent, or subject ID. Every edge names an existing non-deleted instance and, where needed, an existing open binding/session with membership. Missing association fails.

- **Instances:** copy existing core instance/agent/subject/config/enabled/binding semantics into the corresponding existing S5 owner store. Missing migration-owned rows may be inserted. Exact existing semantic rows are accepted without rewriting lifecycle/process state.
- **Discord channels:** every present `channel_config` row is fingerprinted. Explicit edges cover every affected existing binding. Nonempty exact-agent values take precedence over the explicit global fallback. Endpoint/policy rows retain source fingerprints and the existing channel/read/write/whitelist values.
- **Nostr watches:** every present `session_watches` row has exactly one approved Nostr edge whose watch values and bound agent/session equal the source and parsed current configuration.
- **Identity:** `owner -> owner`, `co-agent -> co_agent`, all other permissions -> `trusted_user`. Discord/Nostr/opaque identities require explicit existing-instance edges and matching current config. Exact `rest` is the only core edge. For an explicit Web or opaque-to-Web edge, the existing S5 Web instance must match the source `agent_id`, its persisted `author_id` must equal the Web core config `author_id`, and exactly one approved source row may map to that instance. The source `user_id` need not equal `author_id`. The existing single `identity_projections.external_id='bearer'` role must equal the source permission role (`owner` or `trusted_user`; `co-agent` is unsupported). Preserve the source external `user_id` and role in a separate non-bearer `identity_projections` row: accept an exact existing row or insert a missing row. If the source `user_id` is literally `bearer`, only accept the same existing bearer role without inserting a second caller role. Retain the existing bearer, Web policies, author, any configured credential, and authentication mode unchanged; only D5 permits installing a credential into an existing NULL envelope. A missing source row for an approved edge, missing instance or bearer role, multiple mapped source rows, or any mismatch/collision fails before writing; source rows are never dropped. The source identity projection is data disposition, not Web admission: S5 runtime continues to consult only the existing bearer role.
- **Completeness:** every present channel, watch, identity, configuration, and credential source is consumed exactly as approved; no row is guessed, silently duplicated, or dropped.

## S8-D5 — credentials

Credential sources are the present legacy Discord/Nostr secret columns, an explicitly supplied Web credential file, or an existing S5 destination envelope. Master keys and credential files must be regular non-symlink mode-`0600` files owned by the effective UID. Plaintext remains only in memory, uses the existing S5 encryption function, is never printed or embedded in approval/verification output, and is cleared after use. S8 does not require the additional read-once file-replacement/TOCTOU protocol deferred to Issue #1016.

Exactly one nonempty source is selected. Multiple candidates must decrypt to identical bytes or the explicit approval selection fails. An existing destination envelope is accepted only when it decrypts to the selected bytes; ordinary successful rerun compares rather than re-encrypts. An existing Web instance with a NULL `credential_envelope` may receive the single approved operator credential using S5 encryption, without changing its bearer role or creating another instance.

## S8-D6 — matched backup, failure restore, and rerun

Before any write, prevalidation covers required versions/columns, all present source rows and edges, current destination conflicts, credentials, and preservation digests. The tool then creates one SQLite-consistent matched backup set for core and every participating destination and records file/logical digests.

Destination transactions run in deterministic destination order while core remains read-only. If any destination or later projection step fails, S8 does not resume or adopt partial progress: all processes remain stopped, the operator restores the entire matched backup set, and restarts the migration. Partial-operation artifacts, partial adoption, lost-response provenance reconstruction, stale-backup adoption, and free-space proof are Issue #1016 scope.

After every destination verifies, `project-core-state` performs one immediate core transaction: insert/accept approved `api_principals`, invoke the existing generic safeguard and heartbeat projection seams, and write the immutable separation marker. It never deletes legacy rows or changes `deliveries`. Transaction failure changes none of those core targets.

An ordinary rerun after successful completion is a no-op when current mapped rows, approval/source/destination digests, projection marker, and retained-state digests match. A mismatch fails. #1006 does not require reconstructing byte-identical inserted-versus-accepted provenance after an unobserved lost response.

## S8-D7 — heartbeat projection

Every present `channel_config` row and every affected existing Discord binding has explicit source-to-target coverage. Projection uses the existing exact/global resolver: exact config wins; instruction is nonempty exact, then nonempty global, then `NULL` inheritance. It creates exactly one absent config/instruction pair per eligible `(agent_id,session_id)` or accepts a semantically equal complete pair without rewriting anchors, `last_fired_at`, or timestamps. A partial/conflicting pair, missing membership, duplicate target, or unconsumed enabled source fails atomically.

## Retained S8 acceptance checklist

- strict core `user_version=56` and required-column checks;
- complete mapping of present REST/Discord/Nostr/Web identities, channels, watches, heartbeat sources, settings, and credentials;
- existing destination semantic equality or conflict;
- no plaintext evidence and use of existing encryption;
- one matched pre-write backup set and complete-set restore on any failure;
- ordinary successful-completion idempotency;
- byte/logical preservation of IDs, history, membership, bindings, inbound deduplication, and deliveries;
- one atomic `project-core-state` writer and the exactly-two offline-writer audit.

## Deferred/non-gating checklist — Issue #1016

- full exact schema equality and unknown-column rejection;
- generalized progressed-lifecycle reconciliation;
- read-once secret-file TOCTOU hardening;
- partial-operation artifacts, ownership, adoption, and resumable partial progress;
- lost-response inserted/accepted provenance reconstruction;
- free-space proof and stale-backup adoption/refusal protocols beyond matched-backup verification;
- generalized strict typed-manifest evolution/unknown-field system.

## Test/evidence correction

Commit `af7a9e3` tests the deferred hardening list and is superseded/non-gating; revert it. The original `a96e58f` RED and `22e6fc5` GREEN remain valid only for retained assertions. Remove or narrow any assertion that requires a deferred property. Add no new production seam merely to satisfy deferred hardening.
