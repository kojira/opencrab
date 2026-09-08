# Nostr and Discord V3-only cutover (#970)

Status: approved by the operator in the incident thread (remove the old implementation; no fallback).

## Incident

A production configuration without `[gate]` selected the legacy in-process Nostr runtime. That runtime accepted events and ran the LLM, but ordinary final speech was only recorded and was not published. Startup logs nevertheless reported a per-agent gateway as running.

## Contract

The deletion phase precedes QC and production remediation. Newer incident details do not narrow this original objective.

1. An enabled Nostr agent uses only the external `nostr-gateway` V3 path.
2. Missing or non-`v3` ingress configuration is a startup error when Nostr is configured.
3. The server provisions V3 instances/bindings and supervises one `nostr-gateway` child per enabled agent.
4. The decrypted Nostr secret is passed only in the child's `NOSTARO_SECRET_KEY` environment. It is absent from placement files, argv, and logs.
5. Child exit is detected and restarted with bounded exponential backoff. Server shutdown terminates children.
6. The legacy watch/respond loops and rollout modes are removed rather than retained as fallback.
7. The selected `self_pubkey` remains the signing identity. Unknown/missing keys and gateway binaries fail loudly.

## Production acceptance

A real relay event addressed to the agent reaches the LLM and produces exactly one published reply event signed by `npub1n0staxr79rlk9472m0gxj6684p7n83778lhypy4d47smn45rmyvqzkzvt6`. Receiving an event, generating text, recording a DB row, or logging gateway startup alone does not satisfy acceptance.

Discord follows the same V3-only contract: remove legacy/shared ingress, shadow mode, per-message fallback, and implicit legacy defaults before QC.
