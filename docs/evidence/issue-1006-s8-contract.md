# Issue #1006 S8 implementation contract (schema 56 only)

This document closes S8-D1 through S8-D7. It is intentionally a one-release offline cutover contract, not a reusable migration framework. The ordinary v56 core migration creates `api_principals` without reading or projecting gateway legacy state; legacy source tables remain present until S10. `opencrab-gateway-migrate` accepts only that current core lineage at `PRAGMA user_version=56`, the exact named source columns below, and the S5 destination schemas at this branch. Unknown versions, missing required tables/columns, unknown columns in a concrete legacy table, or an unlisted destination fail before any write.

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

Assertion vectors: `permission='owner'` still resolves to `TrustedUser` through REST; `co-agent` is parsed as today and then likewise downgraded to `TrustedUser` because it is owner-equivalent; an unknown permission still resolves to `TrustedUser`; an otherwise identical `platform='web'` row never resolves through this table.

## S8-D2 — strict approval and verification manifests

Both files are strict UTF-8 JSON (duplicate/unknown keys rejected), `version:1`, and are canonicalized with RFC 8785 JSON Canonicalization Scheme before SHA-256. Arrays whose order is not semantic must already be sorted as stated below; an unsorted input is rejected rather than reordered silently.

Approval input:

```json
{
  "version": 1,
  "operation_id": "00000000-0000-4000-8000-000000000008",
  "created_at": "2026-01-01T00:00:00Z",
  "core_user_version": 56,
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

`created_at` is a fixed operator-approved UTC timestamp in exactly `YYYY-MM-DDTHH:MM:SSZ` form (no fraction or offset); it is the only timestamp S8 may copy into new destination rows. Gateway identity edges use `{"target":"gateway","kind_id":"discord|nostr|web","instance_id":"<existing UUID>"}`. `destinations` sort by `(kind_id,path_id)`; dispositions sort by source fingerprint; each edge list sorts by its canonical JSON bytes; channel/watch/credential arrays sort by their displayed key tuple.

The strict verification output has exactly this shape. `approval` is the complete `ApprovalInput` object above, with the same field values and array order, not a path or a partial copy:

```json
{
  "version": 1,
  "approval": {
    "version": 1,
    "operation_id": "00000000-0000-4000-8000-000000000008",
    "created_at": "2026-01-01T00:00:00Z",
    "core_user_version": 56,
    "source_core_sha256": "<64 lowercase hex>",
    "destinations": [
      {"kind_id":"discord","path_id":"discord-primary","schema":"s5-discord-v1"},
      {"kind_id":"nostr","path_id":"nostr-primary","schema":"s5-nostr-v1"}
    ],
    "identity_dispositions": [
      {"source_fingerprint":"<64 lowercase hex>","edges":[{"target":"api_principal"}]}
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
  },
  "approval_sha256": "<64 lowercase hex>",
  "backup_set_sha256": "<64 lowercase hex>",
  "backups": [
    {"kind_id":"core","path_id":"core","schema":"core-v56","file_sha256":"<64 lowercase hex>","logical_sha256":"<64 lowercase hex>"},
    {"kind_id":"discord","path_id":"discord-primary","schema":"s5-discord-v1","file_sha256":"<64 lowercase hex>","logical_sha256":"<64 lowercase hex>"}
  ],
  "source_rows": [
    {"table":"channel_config","row_count":1,"fingerprint_set_sha256":"<64 lowercase hex>"},
    {"table":"trusted_users","row_count":1,"fingerprint_set_sha256":"<64 lowercase hex>"}
  ],
  "destinations": [
    {
      "kind_id":"discord",
      "path_id":"discord-primary",
      "schema":"s5-discord-v1",
      "before_logical_sha256":"<64 lowercase hex>",
      "after_logical_sha256":"<64 lowercase hex>",
      "counts":{"instances":1,"endpoints":1,"identity_projections":1,"policies":0,"credentials":1},
      "inserted_keys":[{"table":"instances","key":["<instance UUID>"],"row_sha256":"<64 lowercase hex>"}],
      "accepted_existing_keys":[],
      "credentials":[{"instance_id":"<instance UUID>","source":"legacy-core:agent_discord_config:<agent_id>","envelope_sha256":"<64 lowercase hex>","credential_configured":true}]
    }
  ],
  "core_projection": {
    "before_logical_sha256":"<64 lowercase hex>",
    "after_logical_sha256":"<64 lowercase hex>",
    "inserted_api_principal_ids":["<id>"],
    "accepted_existing_api_principal_ids":[],
    "deliveries":{"row_count":0,"logical_sha256":"<64 lowercase hex>"},
    "heartbeat":{
      "inserted_keys":[{"agent_id":"<agent ID>","session_id":"<session ID>"}],
      "accepted_existing_keys":[],
      "initial_sha256":"<64 lowercase hex>",
      "lineage_sha256":"<64 lowercase hex>"
    }
  },
  "manifest_sha256": "<64 lowercase hex>"
}
```

All shown fields are required; unknown or duplicate fields fail. `version`, `core_user_version`, every `row_count`, and every fixed `counts` value are nonnegative JSON integers; `credential_configured` is JSON boolean; all other leaves are strings or the displayed arrays/objects. The `counts` object always contains exactly `instances,endpoints,identity_projections,policies,credentials`, using zero for a class absent from that destination schema. `backups` sort by `(kind_id,path_id)` with core first by ordinary bytewise string order; `source_rows` sort by `table`; verification `destinations` sort by `(kind_id,path_id)`; `inserted_keys` and `accepted_existing_keys` sort by `(table,key elements)`; `credentials` sort by `instance_id`; API-principal ID arrays sort bytewise; heartbeat keys sort by `(agent_id,session_id)`. A key is an array of the destination table's TEXT primary-key components in declared primary-key order. `row_sha256` is SHA-256 of RFC-8785 canonical JSON `{"table":<table>,"key":[...],"row":<semantic mapped row object>}`; randomized ciphertext and nonsemantic timestamps are excluded as required by D4/D5. `fingerprint_set_sha256` is SHA-256 of the concatenated raw 32-byte source fingerprints in lowercase-hex bytewise order. `backup_set_sha256` is SHA-256 of RFC-8785 canonical JSON of the complete sorted `backups` array. `approval_sha256` hashes canonical `approval`. `manifest_sha256` hashes the RFC-8785 canonical complete verification object with only the `manifest_sha256` member omitted. Unsorted arrays are rejected before hashing, and the file itself is emitted as RFC-8785 canonical UTF-8 JSON.

The output contains no plaintext credential, master-key path, external identifier, config bytes, or raw source row; those values appear only through row fingerprints, semantic row hashes, and credential-envelope hashes.

Source-row fingerprint encoding is exact. `LP(x) = u64 big-endian byte length || x`. Encode:

```text
"opencrab/s8/source-row/v1\0"
|| LP(UTF8 table name)
|| u64be(56)
|| u32be(column count)
|| for each column in the committed order:
     LP(UTF8 column name) || (0x00 for SQL NULL; otherwise 0x01 || LP(canonical value))
```

TEXT canonical value is its exact UTF-8 bytes; INTEGER is minimal base-10 ASCII (`0`, never `+0`); BLOB is raw bytes. The committed schema-56 column orders and published vectors are:

- `trusted_users`: `id,user_id,agent_id,permission,created_by,created_at,display_name,platform`. Values `tu-1,42,agent-a,co-agent,owner,2026-01-01T00:00:00Z,Crab,rest` hash to `2c6850f418281b0b4ede33a1a3ab379cef850e7a85dfd7d2679ee9209a5526a7`.
- `channel_config`: `channel_id,agent_id,guild_id,channel_name,readable,writable,whitelisted,heartbeat_enabled,heartbeat_interval_secs,heartbeat_instructions,updated_at`. Values `chan-1,agent-a,guild-1,General,1,0,1,1,NULL,Ping,2026-01-01T00:00:00Z` hash to `e9099467f8ece3abb668571cab576378cf02d894f2eaf911005c85c5c50f6348`.
- `session_watches`: `id,session_id,agent_id,interval_secs,filter_json,created_at`. Values `7,session-a,agent-a,600,{"authors":["abc"]},2026-01-01T00:00:00Z` hash to `8150c628708b328fe4c4dfdf816caa806738fda29ea2fac1304532c62ae8911d`.
- `agent_discord_config`: `agent_id,bot_token,owner_discord_id,enabled,updated_at,bot_user_id`. Values `agent-a,test-token,42,1,2026-01-01T00:00:00Z,99` hash to `0db7c5133b00d98943329f5fa1cb9fff1bc6f262f37a111875246f831da5514e`.
- `agent_nostr_config`: `agent_id,secret_key,relays_json,filter_json,enabled,updated_at,owner_pubkey,self_pubkey`. Values `agent-a,test-secret,["wss://relay.example"],{"kinds":[1]},1,2026-01-01T00:00:00Z,owner-pub,self-pub` hash to `f37159af0363c135d48435ed493b34f05ecea5ae3869294712ec82a6214cc788`.

Comma separation above is explanatory only; the fingerprint always uses the typed LP encoding. `NULL` is the SQL NULL marker, JSON-looking TEXT is hashed byte-for-byte without JSON recanonicalization, and dummy token/secret strings are test vectors only.

Every source row has exactly one disposition record and at least one edge. Duplicate edges, an unlisted target DB, a missing row, a changed fingerprint, or zero edges fails before backup/write.

## S8-D3 — supported source profile

The only accepted core is initialized schema version 56. Required tables are `agents`, `sessions`, `agent_sessions`, `gate_instances`, `gate_bindings`, `deliveries`, `trusted_users`, `channel_config`, `session_watches`, `session_heartbeat_config`, `session_heartbeat_instructions`, and `api_principals`. Their named columns must match schema 56; absence fails even when empty.

Two historical concrete tables are optional because fresh v56 omits them while upgraded v56 retains them:

- `agent_discord_config(agent_id,bot_token,owner_discord_id,enabled,updated_at,bot_user_id)`;
- `agent_nostr_config(agent_id,secret_key,relays_json,filter_json,enabled,updated_at,owner_pubkey,self_pubkey)`.

If present, every named column must exist and no additional non-SQLite column is accepted; every row is fingerprinted. If absent, there must be no Discord/Nostr instance whose required credential/config cannot be proven from an already populated S5 destination plus its existing core `gate_instances` row. There is no support for `agent_discord_config`/`agent_nostr_config` with another shape, old user_version, or runtime import fallback.

`trusted_users` uses its actual v56 eight columns; there is no fictional `source` column. `channel_config` uses all eleven columns including `updated_at`; `session_watches` uses all six columns. Histories, agents, positive subject IDs, sessions, memberships, generic instances/bindings, inbound dedup, and the single `deliveries` table are read for proof only and remain byte/logically unchanged.

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

The S5 Discord, Nostr, and Web stores have no suitable immutable migration-operation metadata seam: their `instances`, endpoint/policy, and identity tables are runtime-owned semantic state and cannot be overloaded. S8 therefore uses one external immutable partial-operation artifact per destination. Artifacts live beside the approval manifest under a mode-`0700`, effective-UID-owned, non-symlink directory named `<approval filename>.partials`. The filename is `<locator_sha256>.json`, where `locator_sha256` hashes canonical JSON `{"operation_id":<UUID>,"kind_id":<kind>,"path_id":<path>}`. Each file is a regular non-symlink effective-UID-owned mode-`0600` file created with exclusive create, fsynced before opening the destination write transaction, and never rewritten or deleted by S8.

The artifact has exactly this strict JSON shape:

```json
{
  "version":1,
  "record_type":"opencrab-s8-partial-destination",
  "operation_id":"00000000-0000-4000-8000-000000000008",
  "approval_sha256":"<64 lowercase hex>",
  "backup_set_sha256":"<64 lowercase hex>",
  "source_core_sha256":"<64 lowercase hex>",
  "destination":{
    "kind_id":"discord",
    "path_id":"discord-primary",
    "schema":"s5-discord-v1",
    "before_file_sha256":"<64 lowercase hex>",
    "before_logical_sha256":"<64 lowercase hex>"
  },
  "expected_keys":[
    {
      "table":"instances",
      "key":["<instance UUID>"],
      "before_row_sha256":null,
      "expected_row_sha256":"<64 lowercase hex>"
    }
  ],
  "record_sha256":"<64 lowercase hex>"
}
```

All fields are required and unknown/duplicate fields fail. `expected_keys` sorts by `(table,key elements)` and contains every row the destination transaction will verify, including exact rows already present in the matched before-backup. `before_row_sha256` is either JSON null when that key was absent in the bound backup or the semantic row hash defined in D2 when present; `expected_row_sha256` is always that hash for the approved mapped row. `record_sha256` hashes RFC-8785 canonical JSON of the whole record with only `record_sha256` omitted. The tool validates the hash, ownership, mode, regular-file type, filename locator, approval/source/backup identities, destination before-backup hashes, and complete expected-key equality before accepting any current destination row.

Import writes one transaction per destination in sorted `(kind_id,path_id)` order. After global prevalidation and matched backups, it exclusively creates and fsyncs that destination's artifact, then opens the destination transaction. The transaction inserts only an expected absent key whose artifact has `before_row_sha256:null`; it accepts an already present key only when the current semantic row hash equals `expected_row_sha256` and either (a) the bound before-backup proves the same `before_row_sha256`, or (b) `before_row_sha256` is null and this exact same-operation artifact owns the interrupted insertion. It commits only after rereading its complete expected key set. Failure rolls back that destination and stops; already committed earlier destinations remain exact partial progress covered by the matched backup set. Core remains read-only.

A lost response after artifact fsync but before destination commit reruns with the same artifact and inserts still-absent approved keys. A lost response after commit accepts the exact rows without writing and continues. Every rerun must supply the same `operation_id`, canonical approval/`approval_sha256`, original matched backup set/`backup_set_sha256`, source hash, destination identity, and byte-identical artifact; missing, changed, newly generated, or non-identical material fails. Before any acceptance, S8 scans all valid artifacts in the partials directory. Another `operation_id` whose artifact overlaps any `(kind_id,path_id,table,key)` conflicts even when the current row bytes are exact, so a new backup cannot adopt another interrupted operation's rows. This one-release cutover does not supersede an operation: rollback restores the entire matched database set, never one DB, and any retry continues with the original operation identity and artifacts. Only after all destinations verify does the separate `project-core-state` command open core read-write; partial artifacts remain immutable audit inputs through S10 and matched rollback.

`project-core-state` prevalidates the unchanged source/approval/destination/partial-artifact digests, starts one immediate core transaction, inserts absent `api_principals`, invokes existing S2 safeguard and S4 heartbeat projection seams, and writes one immutable projection marker. Any error rolls back all core changes. A lost-response rerun opens core read-only: exact marker identity/digests/fingerprint returns `already_applied`; any mismatch fails. It never reruns destination import, deletes a legacy row, or changes `deliveries`.

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
