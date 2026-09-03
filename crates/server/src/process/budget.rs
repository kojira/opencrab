use super::*;

// 会話組み立て（[`opencrab_core::conversation`]）と文脈予算・モデル pricing ゲート
// （[`opencrab_core::context_budget`]）の実体は core へ移した（#518 手順 3〜4）。
// `build_ledger_section` / `build_impression_section` と同型（`conn` を取り会話用
// セクションを組む純粋ロジックで gateway/server の型に依存しない）ため下位層に置ける。
// 既存の呼び出し元（`process::build_conversation_string` 等）のパスを保つため再エクスポート
// する（doc に理由を明記した `subtask_registries` と同じ手）。
pub use opencrab_core::context_budget::{
    check_agent_model_change, compute_context_budget, ensure_functions_within_cap,
    ensure_model_context_window_registered, ensure_model_max_output_tokens_registered,
    ensure_startup_budget_inputs, measure_functions_tokens, model_context_window_missing_message,
    normalize_model_spec, resolve_agent_request_envelope, resolve_model_max_output_tokens,
    resolve_water_levels, split_llm_model_spec, ContextBudgetEnvelope, ContextBudgetError,
    ContextBudgetPolicy, MemoryIndexDecision, RequestEnvelopeArgs, DEFAULT_MEMORY_INDEX_TOKEN_CAP,
};
pub use opencrab_core::conversation::{
    build_conversation_string, build_conversation_string_with_memory_index,
    build_conversation_string_with_waters,
};

/// 入口共通: コア dispatcher の tool schema を 1 回測る。gateway / MCP は
/// [`ensure_request_functions_budget`] が実 `list_tools` で再検査する。
pub fn core_functions_tokens() -> Result<usize, ContextBudgetError> {
    let defs: Vec<opencrab_core::FunctionDefinition> = opencrab_actions::ActionDispatcher::new()
        .get_definitions(&[])
        .into_iter()
        .map(|d| opencrab_core::FunctionDefinition {
            name: d.name,
            description: if d.description.is_empty() {
                None
            } else {
                Some(d.description)
            },
            parameters: d.parameters,
        })
        .collect();
    measure_functions_tokens(&defs)
}

/// Memory Index を載せるか。判定は envelope 側だけが持つ。
pub fn include_memory_index(env: &ContextBudgetEnvelope) -> bool {
    matches!(env.memory_index_decision, MemoryIndexDecision::Inject)
}

/// #884 PR2 hard cap: typed 側は PR2 では圧縮しないため、typed の wire トークンがモデルの
/// 入力上限（`input_high`）を超えると provider が hard-fail する。超過なら typed を諦めて
/// flat 経路（圧縮あり）へ落とす（§7 fallback）。
pub(crate) fn typed_exceeds_input_budget(wire_tokens: usize, input_high: usize) -> bool {
    wire_tokens > input_high
}

/// ターン終了直後の正時: 派生スナップショットを行追加する（#826-B）。
fn persist_turn_end_snapshot(
    state: &AppState,
    session_id: &str,
    agent_id: &str,
    conversation_high: usize,
    conversation_low: usize,
) -> anyhow::Result<()> {
    let conn = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("db lock poisoned: {e}"))?;
    let assembled =
        opencrab_core::context_budget::assemble_from_snapshot(&conn, session_id, agent_id)?;
    let mut gov =
        opencrab_core::context_budget::TurnGovernor::new(conversation_high, conversation_low);
    gov.finish_turn(
        &conn,
        session_id,
        &assembled.items,
        assembled.through_log_id,
        &assembled.text,
    )?;
    Ok(())
}

/// 利用者の待ち時間に乗せない。失敗は応答をひっくり返さない（正時の失敗は次開始の超過検査で見える）。
pub(super) fn spawn_background_turn_end_snapshot(
    state: &AppState,
    session_id: &str,
    agent_id: &str,
    conversation_high: usize,
    conversation_low: usize,
) {
    let state = state.clone();
    let session_id = session_id.to_string();
    let agent_id = agent_id.to_string();
    tokio::spawn(async move {
        if let Err(e) = persist_turn_end_snapshot(
            &state,
            &session_id,
            &agent_id,
            conversation_high,
            conversation_low,
        ) {
            tracing::error!(
                target: "context_budget_check",
                session_id = %session_id,
                error = %e,
                "turn-end snapshot persist failed"
            );
        }
    });
}

/// 各 request 前: 実 `list_tools` で functions cap と `fixed >= input_high` を検査する。
///
/// functions 超過も `apply_line_items` 経由にする（一意名 + 全費目 Display + 観測行）。
pub fn ensure_request_functions_budget(
    args: RequestEnvelopeArgs<'_>,
    tools: &[opencrab_core::FunctionDefinition],
) -> Result<ContextBudgetEnvelope, ContextBudgetError> {
    let functions_tokens = measure_functions_tokens(tools)?;
    resolve_agent_request_envelope(RequestEnvelopeArgs {
        functions_tokens,
        ..args
    })
}
// `format_single_log` は `format_live_inbound`（本番経路）が使うので常時取り込む。
pub(crate) use opencrab_core::conversation::format_single_log;
// 以下はテストだけが参照する（本番コードは使わない）。cfg(test) で本番ビルドの
// unused 警告を避ける。子モジュールのテストが `super::` で辿れる。
#[cfg(test)]
use opencrab_core::conversation::{past_summary_omitted_notice, RECENT_MIN_USER_SPEECHES};

#[cfg(test)]
#[path = "tests/typed_hard_cap.rs"]
mod typed_hard_cap_tests;

/// #284: コンテキストが逼迫しても**直近のユーザー発言は必ずプロンプトに載る**。
///
/// 事故当時、直近 10 件（`RECENT_MIN_LOGS`）が tool_result / evaluation / エージェント
/// 自身の発言で埋まり、ユーザーの生発言が 1 件も入らなかった。エージェントは指示を
/// 一度も見ないまま応答していた。ここで固定するのは「ログ種別に関係なく、直近の
/// ユーザー発言 N 件が優先で残る」こと。
///
/// **行の形は本番と同じでなければならない**（#286）。ユーザー発言は**必ず
/// `record_inbound_message` 経由で**入れること（`agent_id`＝受信側 / `speaker_id`＝送信者、
/// #377）。手書きの行だと本番と形がずれ、述語のバグを見逃す。
///
/// 経緯: 以前ゲートウェイ受信は `agent_id` 列にも送信者 ID を入れており
/// （`agent_id == speaker_id`）、「`speaker_id != log.agent_id`」という列比較の述語が
/// Discord / Nostr では常に false になった（当時の該当 4,490 件すべてが `==`）。#377 で
/// 受信行が `agent_id`＝受信側 に直り列は縮退しなくなったが、正しい述語は今も
/// `speaker_id != <agent_id 引数>`（`opencrab_core::conversation::is_user_speech` 参照）。
#[cfg(test)]
#[path = "tests/recent_user_speech_guarantee.rs"]
mod recent_user_speech_guarantee_tests;

#[cfg(test)]
#[path = "tests/past_summary_notice_contract.rs"]
mod past_summary_notice_contract_tests;
