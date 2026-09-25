# Issue #1006 S7 TDD evidence — superseded

The two-ledger S7 checklist previously recorded here was superseded by the owner-directed strict behavior-preserving separation redesign on 2026-09-25. It is not an execution or release gate.

Authoritative replacement:

- `docs/design-gateway-process-ownership.md` §13 S7 — generic cross-process delivery parity
- `docs/evidence/issue-1006-strict-separation-redesign.md`

The uncommitted follow-up was preserved outside the repository before reset as `issue-1006-s7-two-ledger-deferred-20260925.patch` with SHA-256 `de260afa4009a31627dbfaa4bb5aa3bb941faae405bedc23ed84ce541e4a1be9`. This document does not authorize applying it.

Before replacement S7 implementation, fully revert checkpoint `f25d51af849ea8b80984103c32a669a5bbb3fa19`; then fully revert S6-only commits in reverse order (`cffff1d`, `10bd57e`, `f707fe2`) to restore admitted caller-role snapshot parity; then selectively unwind guarantee-specific S3 fields while retaining dynamic operation metadata/routing. The S6 evidence is separately marked superseded/non-gating. No production implementation change is part of this design correction.
