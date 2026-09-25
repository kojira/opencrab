# Issue #1006 S8 implementation contract (schema 55 only)

This document closes S8-D1 through S8-D7. It is intentionally a one-release offline cutover contract, not a reusable migration framework. `opencrab-gateway-migrate` accepts only the current core lineage at `PRAGMA user_version=55`, the exact named source columns below, and the S5 destination schemas at this branch. Unknown versions, missing required tables/columns, unknown columns in a concrete legacy table, or an unlisted destination fail before any write.

## S8-D1 — existing REST principal behavior

`api_principals` exists only because `server/src/api/agents_messages.rs` calls `resolve_rest_caller_identity`, which currently performs the exact lookup `(platform='rest', user_id, agent_id)` in `trusted_users`. The cutover changes that call to `get_api_principal(user_id, agent_id)` and preserves the existing `Owner -> TrustedUser` downgrade for self-asserted REST callers. No new authentication mode is introduced.

The normal core schema migration immediately preceding S8 creates this exact table in upgraded and fresh schema:

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

`permission` deliberately remains raw text: the existing `TrustedUserPermission::from_db_str` mapping (`owner`, `user`, `co-agent`; unknown -> `user`) remains the authorization rule and preserves old malformed-row behavior. S8 preserves `id`, `user_id`, `agent_id`, `permission`, `created_by`, `created_at`, and `display_name` byte-for-byte for a `trusted_users.platform == "rest"` row. Other platforms cannot target `api_principals`; doing so is a manifest error. An absent identical row is inserted by `project-core-state`; an identical row is an idempotent no-op; either primary-key or `(user_id,agent_id)` collision with different bytes is a conflict. Runtime/admin code never reads `trusted_users` after S10.

Assertion vectors: `permission='owner'` still resolves to `TrustedUser` through REST; `co-agent` resolves to the existing co-agent caller; an unknown permission still resolves to `TrustedUser`; an otherwise identical `platform='web'` row never resolves through this table.

## S8-D2 — strict approval and verification manifests

Both files are strict UTF-8 JSON (duplicate/unknown keys rejected), `version:1`, and are canonicalized with RFC 8785 JSON Canonicalization Scheme before SHA-256. Arrays whose order is not semantic must already be sorted as stated below; an unsorted input is rejected rather than reordered silently.

Approval input:

```json
{
  "version": 1,
  "operation_id": "00000000-0000-4000-8000-000000000008",
  "core_user_version": 55,
  "source_core_sha256": "<64 lowercase hex>",
  "destinations": [
    {"kind_id":"discord","path_id":"discord-primary","schema":"s5-discord-v1"},
    {"kind_id":"nostr","path_id":"nostr-primary","schema":"s5-nostr-v1"}
  ],
  "identity_dispositions": [
    {"source_fingerprint":"<64 lowercase hex>","edges":[
      {"target":"api_principal"}
    ]}
  ],
  "channel_edges": [
    {"source_fingerprint":"<64 lowercase hex>","instance_id":"<existing UUID>","binding_id":"<existing ID>","session_id":"<existing ID>"}
  ],
  "watch_edges": [
    {"source_fingerprint":"<64 lowercase hex>","instance_id":"<existing UUID>"}
  ],
  "credential_sources": [
    {"instance_id":"<existing UUID>","source":"legacy-core:agent_discord_config:<agent_id>"}
  ]
}
```

Gateway identity edges use `{"target":"gateway","kind_id":"discord|nostr|web","instance_id":"<existing UUID>"}`. `destinations` sort by `(kind_id,path_id)`; dispositions sort by source fingerprint; each edge list sorts by its canonical JSON bytes; channel/watch/credential arrays sort by their displayed key tuple. The verification output repeats the approved data and adds source/destination backup hashes, before/after logical digests, per-class counts, exact inserted/accepted-existing destination keys, core delivery-table count/digest, heartbeat initial/lineage digests, and `manifest_sha256`. It contains no plaintext credential, master-key path, external identifier, config bytes, or raw source row; those values appear only through row fingerprints and destination-key digests.

Source-row fingerprint encoding is exact. `LP(x) = u64 big-endian byte length || x`. Encode:

```text
"opencrab/s8/source-row/v1\0"
|| LP(UTF8 table name)
|| u64be(55)
|| u32be(column count)
|| for each column in the committed order:
     LP(UTF8 column name) || (0x00 for SQL NULL; otherwise 0x01 || LP(canonical value))
```

TEXT canonical value is its exact UTF-8 bytes; INTEGER is minimal base-10 ASCII (`0`, never `+0`); BLOB is raw bytes. Committed `trusted_users` order is `id,user_id,agent_id,permission,created_by,created_at,display_name,platform`. Vector row `tu-1,42,agent-a,co-agent,owner,2026-01-01T00:00:00Z,Crab,rest` at version 55 hashes to `305663d4de781bff3c1135332815824a7fdc7144e572fd948604d0ba7b973de7`.

Every source row has exactly one disposition record and at least one edge. Duplicate edges, an unlisted target DB, a missing row, a changed fingerprint, or zero edges fails before backup/write.

## S8-D3 — supported source profile

The only accepted core is initialized schema version 55. Required tables are `agents`, `sessions`, `agent_sessions`, `gate_instances`, `gate_bindings`, `deliveries`, `trusted_users`, `channel_config`, `session_watches`, `session_heartbeat_config`, and `session_heartbeat_instructions`. Their named columns must match schema 55; absence fails even when empty.

Two historical concrete tables are optional because fresh v55 omits them while upgraded v55 retains them:

- `agent_discord_config(agent_id,bot_token,owner_discord_id,enabled,updated_at,bot_user_id)`;
- `agent_nostr_config(agent_id,secret_key,relays_json,filter_json,enabled,updated_at,owner_pubkey,self_pubkey)`.

If present, every named column must exist and no additional non-SQLite column is accepted; every row is fingerprinted. If absent, there must be no Discord/Nostr instance whose required credential/config cannot be proven from an already populated S5 destination plus its existing core `gate_instances` row. There is no support for `agent_discord_config`/`agent_nostr_config` with another shape, old user_version, or runtime import fallback.

`trusted_users` uses its actual v55 eight columns; there is no fictional `source` column. `channel_config` uses all eleven columns including `updated_at`; `session_watches` uses all six columns. Histories, agents, positive subject IDs, sessions, memberships, generic instances/bindings, inbound dedup, and the single `deliveries` table are read for proof only and remain byte/logically unchanged.

## S8-D4 — exact current-store mapping

S8 never derives a new instance, session, or binding ID. Every target edge must name an existing, non-deleted `gate_instances.instance_id` and, where applicable, an existing open `gate_bindings.binding_id/session_id`; agent identity is obtained by joining the instance positive `subject_id` to `agents.subject_id`. A missing association fails.

For Discord/Nostr `instances`, copy existing core `instance_id`, joined `agent_id`, `subject_id`, `config_b64`, `enabled`, and sorted open binding addresses. The core config must parse and reserialize identically through the current gateway `canonicalize_config_b64`; its SHA-256 decoded-byte digest must equal `gate_instances.config_digest`. Destination initial state is `desired_generation=1`, `applied_generation=NULL`, `lifecycle_state='pending'`, empty binding inventory, zero failures, and no process fields. `updated_at` is the approval manifest's fixed creation timestamp. An existing destination instance is accepted only when decrypted credential and every semantic field match; lifecycle progress fields may be non-initial only when its recorded core revision/digest/binding inventory exactly match the same source instance. Nothing is rewritten on an exact rerun.

The existing core config is authoritative; S8 does not invent missing gateway config. Every migrated identity and Nostr watch must already have the same role/watch represented by that parsed config. Otherwise migration fails and operator must correct the legacy source before taking the snapshot.

- Discord `channel_config`: each manifest edge names the existing Discord instance/binding/session. For `(instance,channel_id)`, a nonempty exact-agent row wins; otherwise its explicitly fanned-out global `agent_id=''` row supplies the effective row. Insert `endpoints(instance_id,channel_id,guild_id,readable,writable,policy_json)`, where `policy_json` is RFC-8785 canonical `{"channel_name":<text>,"source_fingerprints":[...],"whitelisted":<bool>}`. The fingerprints retain both exact/global lineage without collapsing source proof.
- Nostr: no row is written to its Discord-shaped `endpoints` table. Existing gate binding addresses and parsed `InstanceConfig` own routing. Each `session_watches` row has exactly one approved Nostr `watch_edge`; its `(id,interval_secs,filter_json)` must equal one parsed `WatchPlacement`, and the row's `(agent_id,session_id)` must equal the target instance agent and one existing binding session.
- Identity: gateway destination key is `(instance_id,role,external_id)`. Map raw permission `owner -> owner`, `co-agent -> co_agent`, and every other value -> `trusted_user`, exactly matching old fail-closed behavior. `external_id=user_id`; `relationship_id=user_id` only for `co_agent`, otherwise NULL; `relationship_revision=NULL`. The same classification must already be present in the parsed Discord/Nostr access config. `rest` is the only core edge. `web` requires an existing Web instance whose `agent_id` matches, `author_id == user_id`, and persisted bearer caller role equals the mapped role; because S5 Web permits one instance per agent and only `owner|trusted_user`, any second Web identity or `co-agent` Web row fails closed. Opaque/empty/`extgate` values require explicit gateway edges and the same config proof; no inference occurs.
- Web: S8 may only accept/update the existing S5 `instances`, `identity_projections`, and `policies` keys named by approval. It does not create another Web auth mode or reinterpret body `user_id`.

Destination-key equality is semantic field equality after canonical JSON/base64 decoding, not timestamp or randomized ciphertext equality. A same key with different semantic bytes is a conflict.

## S8-D5 — credentials and keys

Credential candidates are limited to existing current sources:

- Discord: nonempty `agent_discord_config.bot_token` for the same agent, or an already populated S5 destination envelope;
- Nostr: nonempty `agent_nostr_config.secret_key` for the same agent, or an already populated S5 destination envelope;
- Web: exactly one operator file supplied as `--credential-file <instance_id>=<absolute path>`, or an already populated S5 destination envelope.

Destination master keys are supplied as `--master-key-file <kind_id>=<absolute path>`. Credential/key files must be regular, non-symlink, mode `0600`, owned by the migrator effective UID, and read once into zeroizing memory. A master-key file contains the same padded-base64 32 bytes accepted by each existing S5 `secret_store::parse_master_key`; encryption uses that store's existing `enc:v1` XChaCha20-Poly1305 function. Plaintext never enters config, argv output, logs, approval, or verification manifests.

Exactly one nonempty candidate is selected. If both legacy and destination candidates exist, decrypt destination and require byte equality; otherwise approval must select one by the exact `credential_sources.source` string and the unselected nonempty candidate causes failure. Rerun decrypts and compares without re-encrypting. The verification manifest records only source descriptor, destination envelope SHA-256, and `credential_configured=true`.

## S8-D6 — multi-database ordering and rerun

There is no distributed-transaction claim. Before writes, the tool validates all source shapes, fingerprints, edges, configs, credentials, destination schemas/conflicts, and free backup space, then creates SQLite backup-API snapshots of core and every listed destination and verifies their logical/file digests.

Import writes one transaction per destination in sorted `(kind_id,path_id)` order. A transaction inserts only absent rows and commits only after rereading its complete expected key set. Failure rolls back that destination and stops; already committed earlier destinations remain exact partial progress covered by the matched backup set. Core remains read-only. Rerun with the same `operation_id`, source hash, approval digest, and backup-set digest accepts exact earlier rows without writing and continues missing destinations. Any non-identical row fails; a different operation cannot adopt partial rows. Operator rollback restores the entire matched set, never one DB. Only after all destinations verify does the separate `project-core-state` command open core read-write.

`project-core-state` prevalidates the unchanged source/approval/destination digests, starts one immediate core transaction, inserts absent `api_principals`, invokes existing S2 safeguard and S4 heartbeat projection seams, and writes one immutable projection marker. Any error rolls back all core changes. A lost-response rerun opens core read-only: exact marker identity/digests/fingerprint returns `already_applied`; any mismatch fails. It never reruns destination import, deletes a legacy row, or changes `deliveries`.

## S8-D7 — heartbeat edges

Every `channel_config` source row is fingerprinted. For each existing Discord binding affected by that row, `channel_edges` explicitly names `(source_fingerprint,instance_id,binding_id,session_id)`; the tool never parses a platform address to discover a session. The instance join proves agent/subject ownership, the binding proves the session, and `agent_sessions(agent_id,session_id)` must exist.

For each `(agent_id,session_id)`, collect the exact-agent row and the approved global row for the same channel. Call existing `resolve_heartbeat_projection_sources(exact,global)`: exact config wins; instruction is first nonempty exact then global; no instruction becomes NULL inheritance. If no target exists, construct:

```json
{
  "agent_id":"agent-a",
  "session_id":"session-a",
  "enabled":true,
  "interval_secs":600,
  "anchor_at":"<effective channel_config.updated_at>",
  "last_fired_at":null,
  "override_text":"<resolved nonempty text or null>",
  "updated_at":"<effective channel_config.updated_at>"
}
```

and call `project_stopped_session_heartbeat_target_in_tx`. If both target rows already exist, preserve their anchor/last-fired/timestamps and accept only when enabled/interval/override semantics equal the resolved source; record `accepted_existing` and existing fingerprints. A partial target or semantic conflict fails. Each eligible binding has exactly one target, every enabled source edge is consumed, and no source/binding may map to two differing targets.

## RED assertion inventory

One S8 integration suite must first fail on: absent migrator; absent `api_principals`; the published fingerprint vector; version 54/56 refusal; required/optional table-shape refusal; rest-only core mapping; explicit unknown/extgate fan-out; exact/global Discord endpoint precedence; Nostr watch equality; Web cardinality/role refusal; credential candidate conflict and existing-envelope equality; all-destination prevalidation; partial destination rerun; core-read-only import; atomic projection rollback; immutable exact lost-response marker; heartbeat new/existing/conflict cases; unchanged histories/IDs/bindings/deliveries; and the exactly-two offline-writer audit. These vectors require no further schema or product decision.
