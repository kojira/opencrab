//! 826-B 必須 4 点（mock LLM のみ）。SkillEngine の MockLlm 基盤で回す。

use super::checkpoint::{
    checkpoint_event_body, select_checkpoint_lane, ContextCheckpoint, CHECKPOINT_EMPTY_MARKER,
};
use super::compact::{
    compact_to_low_water, should_compact, CompactItem, CompactLane, CompactPhase,
};
use super::governor::{items_from_logs, GovernorEvent, TurnGovernor};
use crate::conversation::fit_logs_to_budget;
use crate::engine::{ActionExecutor, ActionResult, ChatRequest, SkillEngine};
use crate::{LlmClient, ToolCall};
use async_trait::async_trait;
use opencrab_llm_types::{
    ChatResponse, Choice, FunctionCall, FunctionDefinition, Message, MessageContent, Role, Usage,
};

const HIGH: usize = 45_000;
const LOW: usize = 20_000;
const AGENT: &str = "agent-1";
const SESSION: &str = "sess-826b";
const CHECKPOINT_NEEDLE: &str = "到達点:確認済み-step-17";

fn compact_item(key: &str, tokens: usize, lane: CompactLane, log_id: Option<i64>) -> CompactItem {
    CompactItem {
        key: key.into(),
        tokens,
        text: format!("[{key}:{tokens}]"),
        lane,
        log_id,
        must_keep: false,
    }
}

fn insert_speech(
    conn: &rusqlite::Connection,
    speaker: &str,
    content: &str,
) -> opencrab_db::queries::SessionLogRow {
    let row = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: AGENT.into(),
        session_id: SESSION.into(),
        log_type: "speech".into(),
        content: content.into(),
        speaker_id: Some(speaker.into()),
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    let id = opencrab_db::queries::insert_session_log(conn, &row).unwrap();
    let mut out = row;
    out.id = Some(id);
    out
}

fn insert_tool_pair(conn: &rusqlite::Connection, i: usize) {
    let call = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: AGENT.into(),
        session_id: SESSION.into(),
        log_type: "tool_call".into(),
        content: format!("call-{i}"),
        speaker_id: Some(AGENT.into()),
        turn_number: None,
        metadata_json: Some(
            serde_json::json!({
                "tool_calls_json": serde_json::json!([{
                    "id": format!("tc-{i}"),
                    "function": {
                        "name": "confirm_step",
                        "arguments": format!("{{\"n\":{i},\"pad\":\"{}\"}}", "x".repeat(80))
                    }
                }]).to_string()
            })
            .to_string(),
        ),
        created_at: None,
    };
    opencrab_db::queries::insert_session_log(conn, &call).unwrap();
    let result = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: AGENT.into(),
        session_id: SESSION.into(),
        log_type: "tool_result".into(),
        content: format!("{{\"success\":true,\"data\":\"{}\"}}", "y".repeat(80)),
        speaker_id: Some(AGENT.into()),
        turn_number: None,
        metadata_json: Some(
            serde_json::json!({"tool_call_id": format!("tc-{i}"), "tool_name": "confirm_step"})
                .to_string(),
        ),
        created_at: None,
    };
    opencrab_db::queries::insert_session_log(conn, &result).unwrap();
}

/// (a) 高水位超過→低水位まで削減の数値アサート（境界値込み）
#[test]
fn a_high_water_cut_to_low_with_boundaries() {
    let empty = select_checkpoint_lane(None, None);
    let at_high: Vec<CompactItem> = (0..45)
        .map(|i| {
            compact_item(
                &format!("h{i}"),
                1_000,
                CompactLane::RecentVerbatim,
                Some(i),
            )
        })
        .collect();
    assert!(!should_compact(45_000, HIGH));
    let stay = compact_to_low_water(&at_high, &empty, HIGH, LOW);
    assert!(!stay.fired);
    assert_eq!(stay.before_tokens, 45_000);
    assert_eq!(stay.after_tokens, 45_000);

    let mut over = at_high;
    over.push(compact_item("over", 1, CompactLane::OldHistory, Some(45)));
    assert!(should_compact(45_001, HIGH));
    let cut = compact_to_low_water(&over, &empty, HIGH, LOW);
    assert!(cut.fired);
    assert_eq!(cut.before_tokens, 45_001);
    assert!(
        cut.after_tokens <= 20_000,
        "after={} が低水位超",
        cut.after_tokens
    );
    assert!(cut.reduction() >= 25_001, "reduction={}", cut.reduction());
}

/// (b) 発火点 3 態をイベント順で
#[test]
fn b_fire_points_event_order() {
    let conn = opencrab_db::init_memory().unwrap();
    insert_speech(&conn, "owner", "start");
    let items: Vec<CompactItem> = (0..46)
        .map(|i| compact_item(&format!("u{i}"), 1_000, CompactLane::OldHistory, Some(i)))
        .collect();
    let cp = select_checkpoint_lane(None, Some("assistant kept"));
    let mut gov = TurnGovernor::new(HIGH, LOW);

    let end = gov.finish_turn(&conn, SESSION, &items, &cp, 46).unwrap();
    assert!(end.fired);

    let assembled = super::assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
    gov.inspect_turn_start(assembled.tokens);

    let mut ledger = crate::context_budget::TokenLedger::new();
    for i in 0..46 {
        ledger.record_tokens(format!("mid{i}"), 1_000);
    }
    ledger.record_tokens("burst", 1);
    let mid_items: Vec<CompactItem> = (0..46)
        .map(|i| {
            compact_item(
                &format!("m{i}"),
                1_000,
                CompactLane::OldHistory,
                Some(100 + i),
            )
        })
        .collect();
    let mid = gov
        .inspect_append(&ledger, &mid_items, &cp)
        .expect("途中超過では走る");
    assert!(mid.fired);

    let fired: Vec<CompactPhase> = gov
        .events()
        .iter()
        .filter_map(|e| match e {
            GovernorEvent::CompactFired { phase, .. } => Some(*phase),
            _ => None,
        })
        .collect();
    assert_eq!(
        fired,
        vec![CompactPhase::TurnEnd, CompactPhase::MidTurn],
        "イベント順: {fired:?} / all={:?}",
        gov.events()
    );
    assert!(gov.events().iter().any(|e| matches!(
        e,
        GovernorEvent::Inspect {
            phase: CompactPhase::TurnStart,
            ..
        }
    )));
    assert!(!gov.events().iter().any(|e| matches!(
        e,
        GovernorEvent::CompactFired {
            phase: CompactPhase::TurnStart,
            ..
        }
    )));
}

/// 「チェックポイントが見えなければ確認をやり直す」mock。
struct CheckpointSeekingLlm {
    needle: String,
    confirms: std::sync::Arc<std::sync::Mutex<usize>>,
}

#[async_trait]
impl LlmClient for CheckpointSeekingLlm {
    async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        let visible = request.messages.iter().any(|m| match &m.content {
            Some(MessageContent::Text(t)) => t.contains(&self.needle),
            _ => false,
        });
        if visible {
            return Ok(text_resp("確認完了"));
        }
        *self.confirms.lock().unwrap() += 1;
        Ok(tool_resp("confirm_step"))
    }
}

struct LoopingExecutor;

#[async_trait]
impl ActionExecutor for LoopingExecutor {
    async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
        ActionResult {
            success: true,
            data: serde_json::json!({"ok": true}),
            error: None,
        }
    }
    fn list_tools(&self) -> Vec<FunctionDefinition> {
        vec![FunctionDefinition {
            name: "confirm_step".into(),
            description: Some("確認をやり直す".into()),
            parameters: serde_json::json!({}),
        }]
    }
}

fn text_resp(text: &str) -> ChatResponse {
    ChatResponse {
        id: String::new(),
        model: String::new(),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: Some(MessageContent::Text(text.into())),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
            finish_reason: None,
        }],
        usage: Usage::default(),
        created: 0,
    }
}

fn tool_resp(name: &str) -> ChatResponse {
    ChatResponse {
        id: String::new(),
        model: String::new(),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: None,
                name: None,
                function_call: None,
                tool_calls: Some(vec![ToolCall {
                    id: "c1".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: name.into(),
                        arguments: "{}".into(),
                    },
                }]),
                tool_call_id: None,
            },
            finish_reason: None,
        }],
        usage: Usage::default(),
        created: 0,
    }
}

async fn run_seek(conversation: &str) -> (String, usize, usize) {
    let confirms = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let llm = CheckpointSeekingLlm {
        needle: CHECKPOINT_NEEDLE.into(),
        confirms: confirms.clone(),
    };
    let engine = SkillEngine::new(Box::new(llm), Box::new(LoopingExecutor), 8);
    let result = engine.run("system", conversation, "mock").await.unwrap();
    let n = *confirms.lock().unwrap();
    (result.response, result.iterations, n)
}

/// (c) 到達点生存の新旧対比
#[tokio::test]
async fn c_checkpoint_survival_old_loops_new_oneshot() {
    let conn = opencrab_db::init_memory().unwrap();
    insert_speech(&conn, "owner", "最初の指示");
    insert_speech(&conn, AGENT, CHECKPOINT_NEEDLE);
    let cp = ContextCheckpoint {
        confirmed: vec!["step-17".into()],
        position: CHECKPOINT_NEEDLE.into(),
        next: "finish".into(),
    };
    let ev = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: AGENT.into(),
        session_id: SESSION.into(),
        log_type: "system".into(),
        content: checkpoint_event_body(&cp),
        speaker_id: Some(AGENT.into()),
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    opencrab_db::queries::insert_session_log(&conn, &ev).unwrap();
    for i in 0..46 {
        insert_tool_pair(&conn, i);
    }
    let logs = opencrab_db::queries::list_session_logs_by_session(&conn, SESSION).unwrap();
    let retained = crate::conversation::retain_conversation_logs(logs);

    let old_text = fit_logs_to_budget(&retained, AGENT, 1_200);
    assert!(
        !old_text.contains(CHECKPOINT_NEEDLE),
        "旧実装は中間到達点を落とす前提: {old_text}"
    );
    let (old_resp, old_iters, old_confirms) = run_seek(&old_text).await;
    assert!(
        old_iters > 1 || old_confirms > 0,
        "旧実装は確認をやり直してループする: resp={old_resp} iters={old_iters} confirms={old_confirms}"
    );

    let (items, lane) = items_from_logs(&conn, SESSION, AGENT, &retained).unwrap();
    assert!(
        lane.render().contains(CHECKPOINT_NEEDLE)
            || matches!(lane, crate::context_budget::CheckpointLane::Explicit(_))
    );
    let new_out = compact_to_low_water(&items, &lane, HIGH, LOW);
    assert!(
        new_out.text.contains(CHECKPOINT_NEEDLE)
            || new_out.text.contains("step-17")
            || new_out.text.contains("[context_checkpoint]"),
        "新実装は到達点を再注入する: {}",
        new_out.text
    );
    assert!(!new_out.text.contains(CHECKPOINT_EMPTY_MARKER) || new_out.text.contains("step-17"));
    let (new_resp, new_iters, new_confirms) = run_seek(&new_out.text).await;
    assert_eq!(new_resp, "確認完了");
    assert_eq!(new_iters, 1, "新実装は 1 発完了");
    assert_eq!(new_confirms, 0, "確認のやり直しはしない");
}

/// (d) 連続投稿しても圧縮が毎回発火しない
#[test]
fn d_hysteresis_consecutive_posts_do_not_refire() {
    let empty = select_checkpoint_lane(None, None);
    let mut items: Vec<CompactItem> = (0..46)
        .map(|i| compact_item(&format!("p{i}"), 1_000, CompactLane::OldHistory, Some(i)))
        .collect();
    let first = compact_to_low_water(&items, &empty, HIGH, LOW);
    assert!(first.fired);
    assert!(first.after_tokens <= LOW);

    let mut fires = 1usize;
    for n in 0..5 {
        items.push(compact_item(
            &format!("post{n}"),
            80,
            CompactLane::RecentVerbatim,
            Some(200 + n),
        ));
        let kept: Vec<CompactItem> = items
            .iter()
            .rev()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let mut after_cut = first.after_tokens;
        after_cut += 80;
        assert!(
            !should_compact(after_cut, HIGH),
            "低水位へ戻った後の連続投稿 {n} で高水位を再超過した"
        );
        let again = compact_to_low_water(&kept, &empty, HIGH, LOW);
        if again.fired {
            fires += 1;
        }
    }
    assert_eq!(fires, 1, "ヒステリシスが効かず毎回発火した");
}
