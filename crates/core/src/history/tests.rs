use super::*;
use serde_json::json;

/// The row each former writer built, written out literally (D-1111: stored data must not change).
fn old_row(
    log_type: &str,
    content: &str,
    speaker_id: Option<&str>,
    metadata_json: Option<&str>,
) -> SessionLogRow {
    SessionLogRow {
        id: None,
        agent_id: "agent-a".to_string(),
        session_id: "session-s".to_string(),
        log_type: log_type.to_string(),
        content: content.to_string(),
        speaker_id: speaker_id.map(str::to_string),
        turn_number: None,
        metadata_json: metadata_json.map(str::to_string),
        created_at: None,
    }
}

fn assert_same(event: Event<'_>, expected: SessionLogRow) {
    let row = event.row("agent-a", "session-s");
    assert_eq!(row.agent_id, expected.agent_id);
    assert_eq!(row.session_id, expected.session_id);
    assert_eq!(row.log_type, expected.log_type);
    assert_eq!(row.content, expected.content);
    assert_eq!(row.speaker_id, expected.speaker_id);
    assert_eq!(row.turn_number, expected.turn_number);
    assert_eq!(row.metadata_json, expected.metadata_json);
    assert_eq!(row.created_at, expected.created_at);
}

#[test]
fn each_event_kind_builds_the_row_its_former_writer_built() {
    // extgate say / prelude / interrupt delivered part: speech by the agent with a source.
    let say_meta = json!({"source": "external:external_response:reply"});
    assert_same(
        Event::AgentSpeech {
            text: "こんにちは",
            metadata: Some(say_meta.clone()),
        },
        old_row(
            "speech",
            "こんにちは",
            Some("agent-a"),
            Some(&say_meta.to_string()),
        ),
    );
    // engine text + tool_call speech, intake speech: no metadata.
    assert_same(
        Event::AgentSpeech {
            text: "調べるね",
            metadata: None,
        },
        old_row("speech", "調べるね", Some("agent-a"), None),
    );
    // gate said / REST POST: the speaker is the sender.
    let said_meta = json!({"external_origin": "o1", "source": "x"});
    assert_same(
        Event::InboundSpeech {
            speaker_id: "u1",
            text: "やあ",
            metadata: Some(said_meta.clone()),
        },
        old_row("speech", "やあ", Some("u1"), Some(&said_meta.to_string())),
    );
    assert_same(
        Event::InboundSpeech {
            speaker_id: "u1",
            text: "やあ",
            metadata: None,
        },
        old_row("speech", "やあ", Some("u1"), None),
    );
    // tool_call / tool_result rows of the turn callbacks.
    let call_meta = json!({"tool_calls_json": "[]"});
    assert_same(
        Event::ToolCall {
            content: "",
            metadata: call_meta.clone(),
        },
        old_row(
            "tool_call",
            "",
            Some("agent-a"),
            Some(&call_meta.to_string()),
        ),
    );
    let result_meta = json!({"tool_call_id": "c1", "tool_name": "t", "is_error": false});
    assert_same(
        Event::ToolResult {
            content: "{}",
            metadata: result_meta.clone(),
        },
        old_row(
            "tool_result",
            "{}",
            Some("agent-a"),
            Some(&result_meta.to_string()),
        ),
    );
    // system events (turn end, subtask spawned/progress/completed, delivery cut).
    let end = json!({"type": "turn_terminated", "marker": "NO_REPLY"}).to_string();
    assert_same(
        Event::System {
            content: end.clone(),
            speaker_id: None,
            metadata: None,
        },
        old_row("system", &end, None, None),
    );
    // intake audit note, REST continuation error.
    assert_same(
        Event::System {
            content: "conversation".to_string(),
            speaker_id: Some("intake"),
            metadata: None,
        },
        old_row("system", "conversation", Some("intake"), None),
    );
    let err_meta = json!({"error_name": "E", "entrypoint": "rest_continuation"});
    assert_same(
        Event::System {
            content: "E: d".to_string(),
            speaker_id: Some("agent-a"),
            metadata: Some(err_meta.clone()),
        },
        old_row(
            "system",
            "E: d",
            Some("agent-a"),
            Some(&err_meta.to_string()),
        ),
    );
    // typed rows: tool_cancelled, steer, inner_voice, task_event.
    let steer_meta = json!({"subtask_id": "s1", "from_session": "p"});
    assert_same(
        Event::Typed {
            log_type: "steer",
            content: "こうして",
            speaker_id: None,
            metadata: Some(steer_meta.clone()),
        },
        old_row("steer", "こうして", None, Some(&steer_meta.to_string())),
    );
    assert_same(
        Event::Typed {
            log_type: "inner_voice",
            content: "考え中",
            speaker_id: Some("agent-a"),
            metadata: None,
        },
        old_row("inner_voice", "考え中", Some("agent-a"), None),
    );
}

#[test]
fn record_writes_exactly_one_row_with_that_content() {
    let conn = opencrab_db::init_memory().expect("db");
    let id = record(
        &conn,
        "agent-a",
        "session-s",
        &Event::AgentSpeech {
            text: "一度だけ",
            metadata: None,
        },
    )
    .expect("recorded");
    let rows: Vec<(i64, String, String, Option<String>)> = conn
        .prepare("SELECT id, log_type, content, speaker_id FROM memory_sessions")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![(
            id,
            "speech".into(),
            "一度だけ".into(),
            Some("agent-a".into())
        )]
    );
}

/// D-1111: history rows are written only through this module. Tests, the db crate and the import
/// CLI may still call `insert_session_log` directly.
#[test]
fn no_production_code_outside_history_inserts_session_logs() {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut offenders = Vec::new();
    let mut stack = vec![crates.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(&crates)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if path.is_dir() {
                let skip = rel.ends_with("/target")
                    || rel.ends_with("/tests")
                    || rel == "db"
                    || rel.ends_with("/core_process_e2e");
                if !skip {
                    stack.push(path);
                }
                continue;
            }
            if !rel.ends_with(".rs")
                || rel == "core/src/history.rs"
                || rel.ends_with("tests.rs")
                || rel.ends_with("_tests.rs")
                || rel.ends_with("_test.rs")
                || rel.ends_with("test_support.rs")
                || rel.contains("/bin/opencrab-import")
            {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // Production part only: everything before the first `#[cfg(test)]`.
            let production = text.split("#[cfg(test)]").next().unwrap_or("");
            for (i, line) in production.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                if code.contains("insert_session_log") {
                    offenders.push(format!("{rel}:{}", i + 1));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "write history rows through opencrab_core::history, not insert_session_log: {offenders:?}"
    );
}
