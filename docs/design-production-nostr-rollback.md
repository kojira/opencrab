# D-RB-001 — Production Nostr pre-separation recovery

## Status

Proposed emergency recovery design. This design does not authorize implementation, main merge, or deployment until the owner approves it in Discord.

## Incident and objective

Production currently runs commit `07e928c1fe637fee0fdd1ac0bc6458c2adff91c4`, which contains the partially merged gateway-separation changes from PRs #988–#990. The underlying Nostr Owner values still exist in both the core legacy source and the gateway store, but the partially deployed ownership/API surface has caused visible incorrect behavior.

Restore the last approved pre-separation Nostr ownership behavior while preserving the current schema-52 database and every current conversation, memory, tool, model, binding, and history row.

## Constraints

- Do not restore the schema-49 historical DB into production.
- Do not lose or overwrite any current production row.
- Do not run the unmodified pre-separation binary against schema 52; it intentionally rejects newer schemas.
- Do not invent a mixed historical runtime from unmatched binaries, DB, and config.
- Do not continue Issue #1006 or implement later gateway-separation stages.
- Do not delete the current `nostr-gateway.db`; retain it as rollback evidence only.
- Main merge and production deployment remain separate explicit approvals.

## Recovery architecture

Create a targeted revert candidate from the current production source, using commit `1c3b7821a46ad1dfdacfd5640cd8943cc211dc41` only as the behavioral reference.

1. **Authority**
   - Core `agent_nostr_config` and existing core identity/policy rows are again the live Nostr authority.
   - The verified non-empty core Owner values are used directly.
   - `nostr-gateway.db` is not read or written as live authority after cutover.

2. **Lifecycle**
   - Restore the pre-separation server-owned Nostr placement/child lifecycle.
   - Remove the standalone Nostr daemon from the production process topology.
   - Nostr gateway children consume server-generated placement files as before separation.

3. **Database compatibility**
   - Keep schema 52 unchanged.
   - Use the current schema-52 migration layer and later additive data structures.
   - Reintroduce only the pre-separation Nostr authority/lifecycle behavior; do not downgrade `PRAGMA user_version` and do not remove tables or columns.

4. **Later fixes**
   - Preserve all changes after `1c3b782` that are independent of Nostr ownership, including conversation history, completion behavior, model switching, GPT-6 budgets, and Discord autocomplete.
   - Resolve conflicts in favor of current behavior unless the code is specifically part of Nostr ownership/lifecycle.

5. **Deployer topology**
   - Update the transactional deployer to consume an explicit package topology manifest.
   - For this recovery candidate, health expects core-owned Nostr children and no standalone Nostr daemon/admin UDS.
   - The deployer must not write candidate state into the real DB during preflight.

## Pre-production validation

Using SQLite backup clones only:

1. Run schema-52 quick/integrity checks.
2. Start the complete recovery topology against cloned core/config data.
3. Prove both enabled Nostr agents load exactly one expected Owner from core.
4. Prove Nostr inbound classification, Nostr outbound delivery, Discord gateways, Web/dashboard, and model administration start successfully.
5. Compare pre/post logical digests for agents, sessions, memory, tool logs, LLM logs, Nostr config, trusted users/co-agents, instances, bindings, and model settings; no unexpected deletion or rewrite is allowed.
6. Prove the production DB paths were never opened by the clone test.

## Production procedure

1. Build and review one exact recovery commit and package.
2. Merge only after explicit owner merge approval.
3. Run remote deployer/package hash verification and `--preflight-only`.
4. Immediately before stop, require idle queues/tools and recheck runtime identity.
5. Stop the current topology.
6. Create a fresh full snapshot of core DB, Nostr DB, configs, runtime state, marker, and release pointers; verify SQLite integrity and hashes.
7. Start the recovery topology without modifying schema or importing data.
8. Verify Owner classification for both enabled Nostr agents, Nostr/Discord/Web health, commit identity, queues, and startup errors.
9. On failure, restore the fresh snapshot and exact current `07e928c` runtime.

## Acceptance criteria

- Production runs the exact approved recovery commit.
- Nostr authority and lifecycle match the pre-separation behavior.
- Both enabled Nostr agents classify the expected identity as Owner.
- Nostr inbound and outbound work normally.
- Discord and Web remain healthy.
- Core remains schema 52 and all pre-cutover logical data digests/counts are preserved except normal post-start append-only activity.
- The standalone Nostr daemon is absent; only the expected server-owned Nostr children exist.
- The fresh rollback snapshot and previous runtime remain available.

## Non-goals

- Completing Issue #1006.
- Migrating gateway-owned state.
- Cleaning up or deleting `nostr-gateway.db`.
- General deployer refactoring beyond explicit topology selection and clone-safe preflight.
- Any unrelated robustness or feature work.
