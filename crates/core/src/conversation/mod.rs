//! 会話文字列の組み立て（トークン予算ベースのコンパクション対応）。
//!
//! セッションログから LLM へ渡す会話文字列を構築する。`build_ledger_section`
//! （[`crate::task_ledger`]）/ `build_impression_section`（[`crate::impression_section`]）
//! と同型で、`conn` を取り会話用のセクションを組む純粋ロジック。server / gateway の型に
//! 依存しないため core に置く（#518 手順 3〜4）。呼び出し元は `server::process`
//! （既存パスを保つ再エクスポート）。

mod assembly;
mod format;
mod legacy_budget_fit;
mod past_summary;
mod refs;
mod retain;
mod sanitize;
mod tool_result_fold;

pub use assembly::{
    build_conversation_string, build_conversation_string_with_memory_index,
    build_conversation_string_with_waters, NO_MESSAGES_MARKER, RECENT_MIN_USER_SPEECHES,
    RESPONSE_ONLY_DIRECTIVE,
};
pub use format::{format_single_log, format_single_log_with_echo};
pub use past_summary::past_summary_omitted_notice;
pub use refs::ConversationRefs;
pub use retain::retain_conversation_logs;

#[allow(unused_imports)]
pub(crate) use legacy_budget_fit::fit_logs_to_budget;
pub(crate) use refs::spawn_ack_subtask_id;
#[allow(unused_imports)]
pub(crate) use sanitize::{
    frozen_snapshot_with_marker, leaked_identifier_in_delta, leaked_identifier_in_render,
    restore_frozen_snapshot, scrub_identifiers_for_display, strip_frozen_snapshot,
    strip_inbound_meta_for_display, FROZEN_SNAPSHOT_V2_MARKER,
};
pub(crate) use tool_result_fold::{result_reference, signals_failure};

#[cfg(test)]
use crate::tokens::estimate_tokens;
#[cfg(test)]
use past_summary::{
    build_past_context_summary_section, PAST_SUMMARY_BUDGET_DEN, PAST_SUMMARY_BUDGET_NUM,
};
#[cfg(test)]
use retain::build_recent_window;

#[cfg(test)]
include!("tests/format_log_tests.rs");
#[cfg(test)]
include!("tests/detector_false_positive_847_tests.rs");
#[cfg(test)]
include!("tests/memory_index_section_injection_tests.rs");
#[cfg(test)]
include!("tests/past_summary_budget_tests.rs");
#[cfg(test)]
include!("tests/budget_driven_recent_window_tests.rs");
#[cfg(test)]
include!("tests/budget_fit_recovery_guard_tests.rs");
#[cfg(test)]
include!("tests/orphan_user_speech_tests.rs");
#[cfg(test)]
include!("tests/evaluation_not_in_conversation_tests.rs");
#[cfg(test)]
include!("tests/heartbeat_prompt_dedup_tests.rs");
#[cfg(test)]
include!("tests/impression_section_injection_tests.rs");
#[cfg(test)]
include!("tests/response_only_directive_tests.rs");
#[cfg(test)]
include!("tests/result_reference_tests.rs");
#[cfg(test)]
include!("tests/subtask_completed_folding_tests.rs");
#[cfg(test)]
include!("tests/render_refs_tests.rs");
