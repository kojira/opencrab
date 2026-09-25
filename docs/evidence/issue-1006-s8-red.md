# Issue #1006 S8 RED evidence

Base: `499a2720927e577c6612213175d1794d9a73a498`

This checkpoint adds tests and fixtures only. It does not add the v56 migration, runtime query, migrator executable, projection implementation, or writer-audit production rule.

## Commands

```text
python3 -m unittest scripts.tests.test_issue_1006_s8_red
cargo test -p opencrab-db v56_creates_api_principals_on_fresh_and_upgraded_databases -- --nocapture
```

Raw retained output:

- `docs/evidence/issue-1006-s8-red-python.log`
- `docs/evidence/issue-1006-s8-red-db.log`

## Authentic RED

The Python suite ran 16 assertions: the two contract-fixture/fingerprint assertions passed and 14 production-seam assertions failed. The failures are assertion failures, not syntax, import, compilation, or fixture errors. They identify:

- absent offline `opencrab-gateway-migrate` workspace executable;
- absent required typed approval/verification fields and v56 required-column seam;
- absent complete present-row dispositions, mapping, credential, matched-backup restore, projection, heartbeat, and preservation seams;
- REST still resolving through the legacy path rather than `api_principals`;
- absent exactly-two gateway-legacy writer audit.

The focused DB test compiled and ran one assertion, then failed with `left: 55`, `right: 56`; the ordinary v56 migration and `api_principals` table are absent.

## Next production seams

1. Add ordinary v56 `api_principals` migration/fresh schema and query, then switch only the REST lookup while preserving its historical caller mapping.
2. Add the offline-only `opencrab-gateway-migrate` workspace executable and implement the critical-path S8-D2–D7 contract against disposable databases.
3. Extend the production boundary audit to allow exactly `project-core-state` and the later destructive-cleanup writer, rejecting every third gateway-legacy writer.

No S9 or later behavior is covered or authorized by this checkpoint.

## Critical-path scope correction

Owner direction limits #1006 to migration required for separation. Commit `af7a9e303ef2853ba60e2e74c4e5b6359369c704` added RED assertions for migration hardening and is superseded/non-gating; it must be reverted. Issue #1016 separately owns full-schema equality, generalized progressed-lifecycle reconciliation, read-once TOCTOU, partial-operation artifacts/resume/adoption, lost-response provenance reconstruction, free-space/stale-backup protocols, and generalized strict manifest evolution. The RED evidence above remains authoritative only for the retained checklist in `issue-1006-s8-contract.md`.
