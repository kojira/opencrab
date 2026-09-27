# Issue #1006 S0 audit hardening — TDD evidence

Base under test: `d1ba0c3be4aed72508408b46dbc4e9228e14e679`.
Scope: S0 static boundary guards only; no S1/runtime behavior and no stash use.

## Assertion-level RED

Command (tests were added before changing the detector):

```text
python3 -m unittest scripts.tests.test_gateway_boundary_audit -v
```

Captured result: **26 tests; 19 failures and 1 error**. Each missing seam failed by its own assertion:

- dependency direction/metadata: `test_reverse_normal_concrete_gateway_dependency_is_rejected`, `test_reverse_build_concrete_gateway_dependency_is_rejected`, `test_cargo_metadata_allows_reviewed_server_dev_qc_edge`, `test_cargo_metadata_rejects_server_gateway_normal_edge`, and `test_cargo_metadata_rejects_server_gateway_build_edge`;
- identifier inventory: `test_schema_identifier_mutation_is_rejected`, `test_platform_dto_field_mutation_is_rejected`, `test_concrete_route_mutation_is_rejected`, `test_gateway_kind_branch_mutation_is_rejected`, `test_operation_name_branch_mutation_is_rejected`, `test_session_watch_singular_query_symbol_is_rejected`, and `test_session_watches_plural_camel_symbol_is_rejected`;
- core SQLite/store provenance: `test_gateway_core_path_acceptance_is_rejected`, `test_direct_aliased_sqlite_open_is_inventoried`, `test_helper_mediated_core_store_open_is_rejected`, and `test_gateway_owned_store_open_is_explicit_inventory_not_silent`;
- public reachability: `test_public_reachability_rejects_renamed_admin_factory`, `test_public_reachability_rejects_direct_six_operation_registration`, and `test_public_reachability_rejects_public_merge`;
- fail-closed inventory: `test_unclassified_and_stale_burn_down_entries_fail_closed` could not obtain the newly required singular finding.

Representative exact failures were `AssertionError: 'platform-production-gateway-dependency' not found in set()`, `AssertionError: 'shared-platform-dto' not found in set()`, `AssertionError: 'gateway-db-open' not found in set()`, and `AssertionError: 'public-gate-admin-reachable' not found in set()`.

Two negative controls initially used the new rule names and therefore did not expose the old detector's false positives. They were tightened to require zero findings, then replayed directly against the base detector from `git show d1ba0c3:scripts/gateway_boundary_audit.py`:

```text
old arbitrary-core-import rules: ['gateway-core-sqlite-open']
AssertionError: arbitrary core import was mislabeled as gateway-core-sqlite-open

old protected-UDS-only rules: ['public-gate-admin']
AssertionError: protected UDS-only admin router was mislabeled public by lexical spelling
```

The complete captured RED logs were `/tmp/issue-1006-s0-fix-red.log`, `/tmp/issue-1006-s0-fix-red-old-regressions.log`, and `/tmp/issue-1006-s0-fix-red-old-uds.log` during this run; the concise, non-secret evidence is retained here.

## Minimal GREEN

The detector now:

1. classifies both dependency directions from TOML and real `cargo metadata`, including normal/build rejection, reviewed server dev-only QC evidence, and rejection of dev gateway edges outside that scope;
2. separates schema/query identifiers, DTO `platform` fields, concrete routes, and gateway/operation-name branches while permitting generic value use of `platform` and generic `kind` branches;
3. separately inventories core/legacy path acceptance and DB/store open sites, recognizes aliased `rusqlite::Connection`, catches helper-mediated core paths, and records exactly two current gateway-owned store opens as valid provenance;
4. traces the router value passed to the public TCP listener through Router-returning factories, so renamed/direct/merged gate-admin routes are reachable findings while UDS-only construction is not;
5. makes generated baselines `pending-line-review` until a reviewer marks the exact line inventory reviewed.

Final GREEN commands and results:

```text
python3 -m unittest scripts.tests.test_gateway_boundary_audit -v
Ran 29 tests ... OK

python3 scripts/gateway_boundary_audit.py
gateway boundary audit OK: 454 classified findings
cargo metadata allowed dev-only QC edges: ['opencrab-server -> opencrab-discord-gateway', 'opencrab-server -> opencrab-nostr-gateway']

cargo test -p opencrab-server --test webgate_static_audit
9 passed; 0 failed

cargo fmt --all -- --check
passed

cargo clippy --workspace --all-targets --all-features -- -D warnings
passed

bash scripts/check-deps.sh
R4 OK; R5 OK; R6 OK; R7 OK

bash scripts/check-file-size.sh
OK: every Rust source file is at most 800 lines

cargo tree -p opencrab-server --edges no-dev | (! grep -E 'opencrab-(discord|nostr|web)-gateway')
passed (no concrete gateway in the production tree)
```

One intermediate metadata validation assertion incorrectly included the generic `opencrab-gateway` crate because it filtered only by the `-gateway` suffix. The command failed, the assertion was corrected to the three concrete daemon packages, and the corrected check proved Discord/Nostr are dev-only and Web is absent.

## Reviewed baseline

The line-specific baseline has **454** entries: **450** production violations, **2** explicitly valid gateway-owned store opens, and **2** reviewed server dev-only QC edges.

| Rule | Count |
|---|---:|
| `gateway-core-path` | 16 |
| `gateway-db-open` | 4 |
| `gateway-production-dependency` | 4 |
| `public-gate-admin-reachable` | 3 |
| `reviewed-gateway-dev-dependency` | 2 |
| `shared-concrete-route` | 4 |
| `shared-concrete-schema` | 212 |
| `shared-concrete-vocabulary` | 192 |
| `shared-gateway-name-branch` | 2 |
| `shared-platform-dto` | 15 |

The only valid store-provenance entries are `crates/nostr-gateway/src/daemon.rs:115` and `crates/nostr-gateway/src/store.rs:66`. The reviewed dev-only edges are `crates/server/Cargo.toml:79` and `crates/server/Cargo.toml:82`; real metadata and the no-dev tree prove they are absent from production edges. Every entry has an exact path, line, snippet, classification, V01–V16 mapping, owner stage, and expiry. Coverage-anchor duplication, stale entries, new unclassified findings, count drift, and an unreviewed regenerated baseline all fail closed.

## Follow-up false-negative fix pass

Base under test: `d3f1562a76ceb57a9a413d7eff5b256958a1d116`.

Seven named assertions were added before changing the detector and run with:

```text
python3 -m unittest -v \
  ...test_public_reachability_traces_direct_served_factory_expression \
  ...test_public_reachability_traces_mutation_and_alias_into_serve \
  ...test_grouped_arbitrary_sqlite_alias_open_is_inventoried \
  ...test_platform_dto_qualified_lowercase_type_is_rejected \
  ...test_gateway_kind_non_equality_branch_is_rejected \
  ...test_borrowed_qualified_gateway_name_match_is_rejected \
  ...test_qualified_operation_name_match_is_rejected
```

Exact RED: **7 tests, 7 failures**. Every assertion reported its expected rule absent from `set()`:

- both public reachability assertions lacked `public-gate-admin-reachable`;
- grouped `Conn::open` lacked `gateway-db-open`;
- `pub platform: serde_json::Value` lacked `shared-platform-dto`;
- `!=`, borrowed qualified `match`, and qualified `.as_str()` match forms lacked `shared-gateway-name-branch`.

The captured RED log was `/tmp/issue-1006-s0-fix2-red.log` during this run. Minimal GREEN adds balanced `axum::serve` argument extraction and backwards, position-aware expression/assignment/alias tracing, grouped/direct/type `rusqlite::Connection` alias provenance, type-independent public `platform` field recognition, and guarded comparison/match forms for qualified or borrowed gateway/operation variables. A type-alias regression was added for the supported straightforward alias form, and the non-equality assertion covers `!=`, `<`, `<=`, `>`, and `>=` independently. A negative assertion proves mutation after the served alias does not contaminate reachability. The protected-UDS-only negative remains green.

The repository finding set remains exactly **454** entries, so the reviewed baseline was not regenerated or rewritten in this pass.

Follow-up GREEN validation:

```text
python3 -m unittest scripts.tests.test_gateway_boundary_audit -v
Ran 38 tests ... OK

python3 scripts/gateway_boundary_audit.py
gateway boundary audit OK: 454 classified findings

cargo test -p opencrab-server --test webgate_static_audit
9 passed; 0 failed

bash scripts/check-deps.sh && bash scripts/check-file-size.sh
R4/R5/R6/R7 OK; Rust source size OK

cargo metadata --format-version 1 --no-deps
server concrete edges: Discord dev-only, Nostr dev-only, Web absent

cargo tree -p opencrab-server --edges no-dev
no Discord/Nostr/Web concrete gateway edge

cargo fmt --all -- --check
passed

cargo clippy --workspace --all-targets --all-features -- -D warnings
passed
```

## Final review-blocker pass: normative provenance and function-item aliases

Base under test: `b5f36f465e043f55cdd0d9f1c8f0ed02784df08a`.

Eight independent assertions were added before detector changes and run together. Exact RED was **8 tests: 6 failures, 2 passes**. The six proven gaps were:

- unqualified and qualified public router function-item aliases both failed with `AssertionError: 'public-gate-admin-reachable' not found in set()`;
- relabeling an unreviewed production DB open as `valid-gateway-owned-store`, or independently changing its violation, owner stage, or expiry, produced only the unrelated V01–V16 coverage warning and no field-mismatch error.

The two visibility-qualified alias assertions were already GREEN on the base: both `pub(crate) type StoreConn = rusqlite::Connection` and a chained `pub(in crate::store)` / `pub(super)` alias emitted `gateway-db-open`. The existing unanchored `\btype` scan recognizes visibility-prefixed declarations despite its narrower-looking optional `pub` group. Per review direction, those regression fixtures remain, but no unsupported production parser change was made without RED. The exact mixed RED/preexisting-GREEN output was retained during the run at `/tmp/issue-1006-s0-fix3-red.log`.

Minimal GREEN now resolves a bare or qualified function-item path discovered through the existing position-aware served-expression trace into the reviewed Router factory graph. It also compares every current baseline entry's `classification`, `violation`, `owner_stage`, and `expires_when` exactly with `_metadata_for(finding)`. Consequently only the two code-reviewed `VALID_GATEWAY_DB_OPENS` tuples can carry `valid-gateway-owned-store`; baseline regeneration cannot redefine that provenance policy.

Initial focused GREEN and full mutation validation:

```text
8 focused tests ... OK
python3 -m unittest scripts.tests.test_gateway_boundary_audit -v
Ran 46 tests ... OK

python3 scripts/gateway_boundary_audit.py
gateway boundary audit OK: 454 classified findings
```

The repository finding set remained exactly **454**, so the reviewed baseline was not regenerated or modified.

Final validation for this pass:

```text
cargo test -p opencrab-server --test webgate_static_audit
9 passed; 0 failed

bash scripts/check-deps.sh && bash scripts/check-file-size.sh
R4/R5/R6/R7 OK; every Rust source file is at most 800 lines

cargo metadata --format-version 1 --no-deps
server concrete edges: Discord dev-only, Nostr dev-only, Web absent

cargo tree -p opencrab-server --edges no-dev
no Discord/Nostr/Web concrete gateway edge

cargo fmt --all -- --check
passed

cargo clippy --workspace --all-targets --all-features -- -D warnings
passed

git diff --check
passed
```

## Exact gateway-store identity pass

Base under test: `ac37156f8b86b4f6e3d517766bb4653a85794a72`.

A focused mutation assertion duplicated each reviewed gateway DB-open snippet on the immediately following line in the same approved file. Before the detector change, both subtests failed independently:

```text
test_valid_gateway_db_open_requires_exact_finding_identity
store.rs: expected 'production-violation', got 'valid-gateway-owned-store'
daemon.rs: expected 'production-violation', got 'valid-gateway-owned-store'
FAILED (failures=2)
```

The raw RED output was retained during the run at `/tmp/issue-1006-s0-fix4-red.log`. Minimal GREEN replaces path/snippet provenance with exact `(rule, path, line, snippet)` identities for `store.rs:66` and `daemon.rs:115`. The original sites remain valid; a duplicate or moved occurrence is a production violation and also causes baseline stale/unclassified failure. Focused GREEN passed. The current identities still match the reviewed 454-entry inventory, so the baseline was not regenerated or modified.

Final GREEN validation passed: 47 Python mutation tests; the 454-finding repository audit; 9 `webgate_static_audit` tests; R4–R7 dependency and source-size checks; Cargo metadata and no-dev tree assertions; `cargo fmt --check`; full-workspace/all-target/all-feature Clippy with warnings denied; and `git diff --check`.

## Split-listener and cross-module-alias pass

Base under test: `8e9d438a52fac37f00734c1b4a43e51f733ff927`.

Three assertions were added before detector changes. Exact RED was **3 tests, 3 failures**:

- `test_public_reachability_traces_split_listener_binding_helper` found no `public-gate-admin-reachable` when `bind_public()` owned `TcpListener::bind` and `main` called `axum::serve` with `create_router_with_gate()`;
- direct and chained cross-module SQLite alias tests both expected the daemon `Handle::open(path)` finding but received an empty list.

The raw RED output was retained during the run at `/tmp/issue-1006-s0-fix5-red.log`. Minimal GREEN roots the shared-production route graph on `axum::serve` itself, while the protected `serve_uds` negative remains outside the graph. Concrete-gateway SQLite aliases are now derived to a fixed point from all production source files in that one crate and supplied to each file audit. A separate negative assertion proves aliases do not leak into another gateway crate. Focused GREEN passed all new assertions plus protected-UDS and exact-site-provenance controls. The repository audit remains exactly 454 findings, so the reviewed baseline was not regenerated or modified.

Final GREEN validation passed: 51 Python mutation tests; the 454-finding repository audit; 9 `webgate_static_audit` tests; R4–R7 dependency and source-size checks; Cargo metadata and no-dev tree assertions; `cargo fmt --check`; full-workspace/all-target/all-feature Clippy with warnings denied; and `git diff --check`.

## Renamed import/re-export alias pass

Base under test: `d17872b08922fbc9515b6f2c7ef28002bd8cbb1e`.

Two assertions were added before detector changes. Exact RED was **2 tests, 2 failures**: a direct `use crate::store_types::Raw as Handle` and a multi-hop grouped `pub use ...::{Raw as Exported}` followed by `use ...::{Exported as Handle}` both expected the daemon `Handle::open(path)` finding but received an empty list. Raw RED output was retained during the run at `/tmp/issue-1006-s0-fix6-red.log`.

Minimal GREEN extracts straightforward ordinary/grouped `use` and `pub use` imported-name-to-local-name edges, then resolves those edges together with type aliases in the existing per-concrete-gateway-crate fixed point. Focused GREEN passed both new assertions and the ordinary import, chained type alias, grouped rusqlite alias, cross-crate non-leakage, protected-UDS, and exact-site-provenance controls. The repository audit remains exactly 454 findings, so neither the reviewed baseline nor provenance authority changed.

Final GREEN validation passed: 53 Python mutation tests; the 454-finding repository audit; 9 `webgate_static_audit` tests; R4–R7 dependency and source-size checks; Cargo metadata and no-dev tree assertions; `cargo fmt --check`; full-workspace/all-target/all-feature Clippy with warnings denied; and `git diff --check`.
