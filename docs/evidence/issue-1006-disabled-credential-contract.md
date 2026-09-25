# D-1006-CRED-01 — preserve disabled instances without inventing credentials

Status: design amendment and forward RED only. Applies to the stopped S8 migration of existing Discord/Nostr instances; it changes neither Web admission nor live credential policy.

## Historical and S5 state

A disabled instance starts no child and can lack a bot token/secret. The existing S5 Discord/Nostr `instances.credential_envelope TEXT NOT NULL` stores **unconfigured** as the empty string `''`; the admin reports such a row as not configured. A nonempty encrypted envelope remains an actual configured credential. S8 currently requires a nonempty source and descriptor for every Discord/Nostr instance, including disabled rows, forcing an operator to invent a secret or lose the disabled instance. This is not required to separate storage.

| Clause | Production seam | Forward RED and smallest GREEN |
| --- | --- | --- |
| D-C1 — disabled with no source | `gateway-migrate/src/destination_plans.rs`: stopped instance/credential planning | Import a disabled Discord and a disabled Nostr instance with no source row/descriptor into their S5 stores; assert the same instance IDs and `enabled=false`, exact `credential_envelope=''`, zero configured-credential entries/count in generated report. The current `credential source missing` failure is RED. Permit missing secret/descriptor **only** for a disabled instance without an existing nonempty envelope; emit no synthetic credential. |
| D-C2 — rerun and existing secret | `gateway-migrate/src/destination.rs`: insert/semantic compare/report and ordinary successful rerun | The same stopped import reruns without adding a credential or changing its empty envelope. If a disabled destination already has a nonempty envelope, retain it and use the existing approved source/equality checks; never overwrite it with empty. Existing S5 schema/store remains valid; no new schema, secret, or crypto format. |
| D-C3 — enable remains gated | `destination_plans.rs`, S5 admin/daemon | An enabled instance with no authoritative credential still fails before backup. A disabled unconfigured instance stays without a child; a later enable must first install a real credential through the gateway-owned admin. No implicit enable, token invention, cross-instance reuse, or reassignment. |

The S8 matched core-plus-all-participating-gateway backup and retained ID/history/binding/delivery rules are unchanged. No real QC/production database, runtime legacy import, third writer, new authentication mode, S10 deletion, or #1016 durability work is authorized by this amendment. Existing Web no-credential behavior is governed separately by D-1006-WEB-01.
