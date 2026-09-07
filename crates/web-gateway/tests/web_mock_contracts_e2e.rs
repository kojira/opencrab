//! issue #788 残 3 契約の web mock E2E（実 LLM 不要・決定的）。
//!
//! e2e_local.rs（実 LLM opt-in）のシナリオを、実プロセス（opencrab-server + web-gateway）+
//! HTTP mock LLM（OpenAI 互換）へ移植する。qc_harness_e2e.rs の `RoutedMock`（内容ルーティング /
//! Notify で長処理を保持）を、プロセス越えの HTTP mock として作り直したもの。
//!
//! 固定する 3 契約:
//!   1. NO_REPLY/withheld: `content:"NO_REPLY"` の応答は SSE に `event: message` として
//!      流れず、`event: completed_no_reply` として観測でき、DB に `no_reply` として残る。
//!   2. 非ブロッキング: 長処理（保持中の背景サブタスク）走行中に第2依頼が待たされず即応する
//!      （qc_harness_e2e の scenario_main の web 版）。
//!   3. 未許可コマンド拒否: allowlist に無いコマンドの execute_shell が拒否され、拒否理由が
//!      エラー契約どおり（"is not in the allowed list"）会話へ残る。
//!
//! mock LLM が返す JSON 形は `crates/llm/src/providers/openai_compat.rs` のパーサに従う
//! （tool_calls[].function.arguments は JSON 文字列・finish_reason は "tool_calls"/"stop"）。

include!("web_mock_contracts_e2e/support.rs");
include!("web_mock_contracts_e2e/no_reply.rs");
include!("web_mock_contracts_e2e/reply.rs");
include!("web_mock_contracts_e2e/concurrency.rs");
include!("web_mock_contracts_e2e/unauthorized.rs");
