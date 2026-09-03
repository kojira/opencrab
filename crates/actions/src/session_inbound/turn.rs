use super::normalize::NormalizedInbound;
use crate::agent_runtime::AgentRuntime;
use crate::transcript::TranscriptSource;
use crate::RunRequest;
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
pub async fn start_session_turn<R, Wrap, Build>(
    runtime: &R,
    source: TranscriptSource,
    inbound: &NormalizedInbound<'_>,
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
    run_session_turn(
        runtime,
        inbound.session_id,
        inbound.agent_id,
        system_prompt,
        runtime_context_text,
        wrap_conversation,
        build_run,
    )
    .await
}

/// resume / 継続ターン（直列ロック内）。会話構築 + `run_agent_response`。
///
/// inbound フックは呼ばない（受信は既に記録済み。subtask / interaction の現行どおり）。
pub async fn run_session_turn<R, Wrap, Build>(
    runtime: &R,
    session_id: &str,
    agent_id: &str,
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
    // #826: fail-loud 予算。既定へは落とさず、一意名（超過は `context_budget_exhausted`）で
    // ログしてこのターンを run しない（`None`）。`system_prompt` / `runtime_context_text` は
    // `wrap_conversation` が前置する実 request と一致させること（呼び出し側の契約）。
    let budget = match runtime.context_budget_tokens(
        agent_id,
        session_id,
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
    let raw = match runtime.build_conversation_string(
        session_id,
        agent_id,
        budget,
        system_prompt,
        runtime_context_text,
    ) {
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
    Some(runtime.run_agent_response(build_run(conversation)).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web_inbound<'a>(
        session_id: &'a str,
        agent_id: &'a str,
        sender_id: &'a str,
        text: &'a str,
    ) -> NormalizedInbound<'a> {
        NormalizedInbound {
            session_id,
            agent_id,
            sender_id,
            sender_name: "",
            avatar_url: None,
            channel_id: None,
            pubkey: None,
            text,
            image_urls: &[],
            external_id: "",
        }
    }

    /// ensure → record の順。本文・識別子は渡した inbound のまま。
    #[test]
    fn prepare_session_inbound_write_ensures_then_records() {
        let calls = std::sync::Mutex::new(Vec::new());
        let inbound = web_inbound("web-a-c1", "a", "alice", "hi");
        prepare_session_inbound_write(
            &inbound,
            |sid, aid| {
                calls.lock().unwrap().push(format!("ensure:{sid}:{aid}"));
                Ok(())
            },
            |aid, sid, uid, content| {
                calls
                    .lock()
                    .unwrap()
                    .push(format!("record:{aid}:{sid}:{uid}:{content}"));
                Ok(())
            },
        )
        .expect("ensure+record は成功する");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "ensure:web-a-c1:a".to_string(),
                "record:a:web-a-c1:alice:hi".to_string(),
            ]
        );
    }

    #[test]
    fn prepare_session_inbound_write_ensure_failure_skips_record() {
        let calls = std::sync::Mutex::new(Vec::new());
        match prepare_session_inbound_write(
            &web_inbound("s", "a", "u", "hi"),
            |sid, aid| {
                calls.lock().unwrap().push(format!("ensure:{sid}:{aid}"));
                Err(anyhow::anyhow!("disk full"))
            },
            |_, _, _, _| {
                calls.lock().unwrap().push("record".into());
                Ok(())
            },
        ) {
            Err(PrepareSessionInboundError::Ensure(e)) => {
                assert!(e.to_string().contains("disk full"), "{e:#}");
            }
            other => panic!("expected Ensure, got {other:?}"),
        }
        assert_eq!(*calls.lock().unwrap(), vec!["ensure:s:a".to_string()]);
    }

    #[test]
    fn prepare_session_inbound_write_record_failure_is_distinct() {
        let calls = std::sync::Mutex::new(Vec::new());
        match prepare_session_inbound_write(
            &web_inbound("s", "a", "u", "hi"),
            |sid, aid| {
                calls.lock().unwrap().push(format!("ensure:{sid}:{aid}"));
                Ok(())
            },
            |aid, sid, uid, content| {
                calls
                    .lock()
                    .unwrap()
                    .push(format!("record:{aid}:{sid}:{uid}:{content}"));
                Err(anyhow::anyhow!("locked"))
            },
        ) {
            Err(PrepareSessionInboundError::Record(e)) => {
                assert!(e.to_string().contains("locked"), "{e:#}");
            }
            other => panic!("expected Record, got {other:?}"),
        }
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["ensure:s:a".to_string(), "record:a:s:u:hi".to_string()]
        );
    }
}
