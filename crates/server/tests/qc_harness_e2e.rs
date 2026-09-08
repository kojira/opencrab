//! QC ハーネス E2E（Phase 2）。
//!
//! リレー・鍵・nostaro 子プロセス無しで、**実配線**を通す決定的オフライン E2E:
//!   実 `serve_uds`（extgate core）＋ 実 `AppState`（mock LLM）
//!     ⇕ 実 UDS ⇕
//!   実 `spawn_instance`（nostr-gateway・fake_watch＋dry_run 両有効）
//!
//! 観測channel = dry-run の tracing ログ（target = `opencrab_nostrgate::dry_run`）。
//! グローバル subscriber を 1 回だけ張り、各テストは注入した固有本文で絞る。
//! 単一スレッド（`--test-threads=1`）前提。
//!
//! 注意（現行本線 DI-16 / row292）: say は常に standalone post として publish される
//! （特定イベントへの e-tag 返信は DI `reply` 操作が担い、say 経路には返信先が無い）。
//! よって観測は「standalone post の本文」で行い、返信先イベント id では相関しない。

include!("qc_harness_e2e/support.rs");
include!("qc_harness_e2e/delivery_and_concurrency.rs");
include!("qc_harness_e2e/dispatch_resume.rs");
include!("qc_harness_e2e/utterance_contracts.rs");
include!("qc_harness_e2e/continuation_898.rs");
include!("qc_harness_e2e/no_reply_899.rs");
include!("qc_harness_e2e/audit_support.rs");
include!("qc_harness_e2e/audit_898.rs");
include!("qc_harness_e2e/audit_899.rs");
include!("qc_harness_e2e/audit_900_and_s13.rs");
include!("qc_harness_e2e/heartbeat_925.rs");
