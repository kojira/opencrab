use super::normalize::NormalizedInbound;
use crate::agent_runtime::AgentRuntime;
use crate::transcript::TranscriptSource;
use crate::{RunRequest, TurnModelProfile};
use opencrab_core::EngineResult;

/// 確保 + inbound 記録の失敗。順序は ensure → record（[`prepare_session_inbound`] と同一）。
#[derive(Debug)]
pub enum PrepareSessionInboundError {
    Ensure(anyhow::Error),
    Record(anyhow::Error),
}

/// セッション確保 + inbound 記録（セッションロックより前・#284）。
///
/// `true` = 記録できた。ゲートは `false` を無視せずエスカレーションする。
#[must_use]
pub fn prepare_session_inbound<R: AgentRuntime>(
    runtime: &R,
    source: TranscriptSource,
    inbound: &NormalizedInbound<'_>,
    theme: &str,
    metadata_json: &str,
    mode: &str,
) -> bool {
    let agent_id = inbound.agent_id.to_string();
    runtime.ensure_session(
        inbound.session_id,
        std::slice::from_ref(&agent_id),
        theme,
        metadata_json,
        mode,
    );
    runtime.record_inbound_message(source, &inbound.as_record())
}

/// [`prepare_session_inbound`] と同じ口（ensure → record）。web はこちら。
///
/// 行の形は呼び出し側（`session_logs` 現行形。`TranscriptSource` は使わない）。
pub fn prepare_session_inbound_write(
    inbound: &NormalizedInbound<'_>,
    ensure: impl FnOnce(&str, &str) -> anyhow::Result<()>,
    record: impl FnOnce(&str, &str, &str, &str) -> anyhow::Result<()>,
) -> Result<(), PrepareSessionInboundError> {
    ensure(inbound.session_id, inbound.agent_id).map_err(PrepareSessionInboundError::Ensure)?;
    record(
        inbound.agent_id,
        inbound.session_id,
        inbound.sender_id,
        inbound.text,
    )
    .map_err(PrepareSessionInboundError::Record)?;
    Ok(())
}

/// inbound ターン起動（直列ロック内）。受信フック + 会話構築 + `run_agent_response`。
///
/// 会話構築に失敗したら `None`（現行どおり run しない）。`Some` は run の
/// `Result` そのもの（成功も失敗もゲートの配送へ渡す）。
/// `profile` は発端 said の宣言（D-1081）。ターンのモデルはここで 1 回だけ決まる。
/// D-1085: a `Realtime` said starts a lightweight turn: the conversation is the last
/// [`LIGHTWEIGHT_TURN_SPEECH_ROWS`] speech rows and the run is [`RunRequest::as_lightweight`].
/// The caller passes the matching system prompt (`build_realtime_context`).
#[allow(clippy::too_many_arguments)]
pub async fn start_session_turn<R, Wrap, Build>(
    runtime: &R,
    source: TranscriptSource,
    inbound: &NormalizedInbound<'_>,
    profile: TurnModelProfile,
    system_prompt: &str,
    runtime_context_text: &str,
    wrap_conversation: Wrap,
    build_run: Build,
) -> Option<anyhow::Result<EngineResult>>
where
    R: AgentRuntime,
    Wrap: FnOnce(&str) -> String,
    Build: FnOnce(String) -> RunRequest,
{
    runtime.on_inbound_message(source, inbound.agent_id, &inbound.as_record());
    run_turn(
        runtime,
        inbound.session_id,
        inbound.agent_id,
        profile,
        profile == TurnModelProfile::Realtime,
        system_prompt,
        runtime_context_text,
        wrap_conversation,
        build_run,
    )
    .await
}

/// D-1085: how many recent speech rows a lightweight turn sees.
pub const LIGHTWEIGHT_TURN_SPEECH_ROWS: usize = 20;

/// resume / 継続ターン（直列ロック内）。会話構築 + `run_agent_response`。
///
/// inbound フックは呼ばない（受信は既に記録済み。subtask / interaction の現行どおり）。
#[allow(clippy::too_many_arguments)]
pub async fn run_session_turn<R, Wrap, Build>(
    runtime: &R,
    session_id: &str,
    agent_id: &str,
    profile: TurnModelProfile,
    system_prompt: &str,
    runtime_context_text: &str,
    wrap_conversation: Wrap,
    build_run: Build,
) -> Option<anyhow::Result<EngineResult>>
where
    R: AgentRuntime,
    Wrap: FnOnce(&str) -> String,
    Build: FnOnce(String) -> RunRequest,
{
    run_turn(
        runtime,
        session_id,
        agent_id,
        profile,
        false,
        system_prompt,
        runtime_context_text,
        wrap_conversation,
        build_run,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_turn<R, Wrap, Build>(
    runtime: &R,
    session_id: &str,
    agent_id: &str,
    profile: TurnModelProfile,
    lightweight: bool,
    system_prompt: &str,
    runtime_context_text: &str,
    wrap_conversation: Wrap,
    build_run: Build,
) -> Option<anyhow::Result<EngineResult>>
where
    R: AgentRuntime,
    Wrap: FnOnce(&str) -> String,
    Build: FnOnce(String) -> RunRequest,
{
    // D-1080: the session's model is read once here (inside the serialized section) and pinned
    // for the whole turn. A set/reset that lands mid-turn applies from the next turn.
    let model = match runtime.session_turn_model(agent_id, session_id, profile) {
        Ok(model) => model,
        Err(e) => {
            tracing::error!(
                session_id = %session_id,
                agent_id = %agent_id,
                "session turn model lookup failed: {e}"
            );
            return Some(Err(e));
        }
    };
    // #826: fail-loud 予算。既定へは落とさず、一意名（超過は `context_budget_exhausted`）で
    // ログしてこのターンを run しない（`None`）。`system_prompt` / `runtime_context_text` は
    // `wrap_conversation` が前置する実 request と一致させること（呼び出し側の契約）。
    let budget = match runtime.context_budget_tokens(
        agent_id,
        session_id,
        &model,
        system_prompt,
        runtime_context_text,
    ) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                session_id = %session_id,
                agent_id = %agent_id,
                error_name = e.name(),
                "{name}: {e}",
                name = e.name()
            );
            return None;
        }
    };
    let raw = if lightweight {
        // D-1085: the budget check above stays as the fail-loud guard; nothing is compacted.
        runtime.build_recent_conversation_string(session_id, agent_id, LIGHTWEIGHT_TURN_SPEECH_ROWS)
    } else {
        runtime.build_conversation_string(
            session_id,
            agent_id,
            &model,
            budget,
            system_prompt,
            runtime_context_text,
        )
    };
    let raw = match raw {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                session_id = %session_id,
                agent_id = %agent_id,
                "build_conversation_string failed: {e}"
            );
            return None;
        }
    };
    let conversation = wrap_conversation(&raw);
    let req = build_run(conversation).with_pinned_model(model);
    let req = if lightweight {
        req.as_lightweight()
    } else {
        req
    };
    Some(runtime.run_agent_response(req).await)
}
