#[cfg(test)]
pub(crate) fn run_shadow_comparison(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    conversation_high: usize,
    conversation_low: usize,
    include_memory_index: bool,
) -> DeriveDiagnostics {
    let flat_tokens = match crate::conversation::build_conversation_string_with_waters(
        conn,
        session_id,
        agent_id,
        conversation_high,
        conversation_low,
        include_memory_index,
    ) {
        Ok(flat) => crate::tokens::estimate_tokens(&flat),
        Err(error) => {
            tracing::warn!(session_id, %error, "typed shadow could not build flat conversation");
            0
        }
    };
    let snapshot = match opencrab_db::queries::latest_conversation_snapshot(conn, session_id) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(session_id, %error, "typed shadow could not read snapshot");
            return DeriveDiagnostics {
                item_count: 0,
                unpaired_call_count: 0,
                opaque_event_count: 0,
            };
        }
    };
    let logs = match snapshot {
        Some(snapshot) => {
            opencrab_db::queries::list_session_logs_after(conn, session_id, snapshot.through_log_id)
        }
        None => opencrab_db::queries::list_session_logs_by_session(conn, session_id),
    };
    let logs = match logs {
        Ok(logs) => crate::conversation::retain_conversation_logs(logs),
        Err(error) => {
            tracing::warn!(session_id, %error, "typed shadow could not read conversation logs");
            return DeriveDiagnostics {
                item_count: 0,
                unpaired_call_count: 0,
                opaque_event_count: 0,
            };
        }
    };
    let all = match opencrab_db::queries::list_session_logs_by_session(conn, session_id) {
        Ok(logs) => crate::conversation::retain_conversation_logs(logs),
        Err(error) => {
            tracing::warn!(session_id, %error, "typed shadow could not read full conversation logs");
            return DeriveDiagnostics {
                item_count: 0,
                unpaired_call_count: 0,
                opaque_event_count: 0,
            };
        }
    };
    let refs = crate::conversation::ConversationRefs::build(&all, agent_id);
    let completed = all
        .iter()
        .filter(|log| log.log_type == "tool_result" || log.log_type == "tool_cancelled")
        .filter_map(|log| result_metadata(log).0)
        .collect();
    let derived = derive_items(&logs, &refs, &completed, agent_id);
    let typed_json = serde_json::to_string(&derived.items).unwrap_or_else(|error| {
        tracing::warn!(session_id, %error, "typed shadow could not serialize items");
        String::new()
    });
    // 未確認: provider 別 item overhead は PR2 以降で実測する。ここは JSON 近似値だけを比較する。
    let typed_tokens = crate::tokens::estimate_tokens(&typed_json);
    tracing::debug!(
        session_id,
        typed_items = derived.diagnostics.item_count,
        unpaired = derived.diagnostics.unpaired_call_count,
        opaque = derived.diagnostics.opaque_event_count,
        typed_tokens,
        flat_tokens,
        "typed shadow comparison"
    );
    derived.diagnostics
}

