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
- absent strict manifest/current-schema seam;
- absent required/optional table-shape checks;
- absent complete dispositions, mapping, credential, backup, partial-rerun, projection, heartbeat, and preservation seams;
- REST still resolving through the legacy path rather than `api_principals`;
- absent exactly-two gateway-legacy writer audit.

The focused DB test compiled and ran one assertion, then failed with `left: 55`, `right: 56`; the ordinary v56 migration and `api_principals` table are absent.

## Next production seams

1. Add ordinary v56 `api_principals` migration/fresh schema and query, then switch only the REST lookup while preserving its historical caller mapping.
2. Add the offline-only `opencrab-gateway-migrate` workspace executable and implement the strict S8-D2–D7 contract against disposable databases.
3. Extend the production boundary audit to allow exactly `project-core-state` and the later destructive-cleanup writer, rejecting every third gateway-legacy writer.

No S9 or later behavior is covered or authorized by this checkpoint.
