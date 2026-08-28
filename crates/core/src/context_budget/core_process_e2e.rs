//! 826-B 必須 4 点（mock LLM のみ）。SkillEngine の MockLlm 基盤と本番組立/終了経路で回す。

use super::checkpoint::{
    checkpoint_event_body, select_checkpoint_lane, ContextCheckpoint, CHECKPOINT_EMPTY_MARKER,
};
use super::compact::{
    compact_to_low_water, should_compact, CompactItem, CompactLane, CompactPhase,
};
use super::governor::{
    assemble_from_snapshot, items_from_logs, take_governor_events, GovernorEvent, TurnGovernor,
};
use crate::conversation::{build_conversation_string_with_waters, fit_logs_to_budget};
use crate::engine::{ActionExecutor, ActionResult, ChatRequest, SkillEngine};
use crate::tokens::estimate_tokens;
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
        group_id: None,
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

fn insert_checkpoint(conn: &rusqlite::Connection) {
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
    opencrab_db::queries::insert_session_log(conn, &ev).unwrap();
}

fn insert_tool_pair(conn: &rusqlite::Connection, i: usize, pad: &str) {
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
                        "arguments": format!("{{\"n\":{i},\"pad\":\"{pad}\"}}")
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
        content: format!("{{\"success\":true,\"data\":\"{pad}\"}}"),
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

fn seed_over_high(conn: &rusqlite::Connection) {
    insert_speech(conn, "owner", "最初の指示");
    insert_speech(conn, AGENT, CHECKPOINT_NEEDLE);
    insert_checkpoint(conn);
    let blob = "hist ".repeat(2_000);
    for i in 0..30 {
        insert_speech(conn, "owner", &format!("turn-{i} {blob}"));
    }
    let pad = "pad ".repeat(80);
    for i in 0..46 {
        insert_tool_pair(conn, i, &pad);
    }
}

fn text_over_tokens(min_tokens: usize) -> String {
    let s = "word ".repeat(min_tokens.saturating_add(64));
    assert!(
        estimate_tokens(&s) > min_tokens,
        "fixture tokens={} が {min_tokens} 以下",
        estimate_tokens(&s)
    );
    s
}

fn compact_fired_phases(events: &[GovernorEvent]) -> Vec<CompactPhase> {
    events
        .iter()
        .filter_map(|e| match e {
            GovernorEvent::CompactFired { phase, .. } => Some(*phase),
            _ => None,
        })
        .collect()
}

/// (a) 高水位超過→低水位まで削減の数値アサート（境界値込み）。
/// compact_to_low_water が本番圧縮器。SkillEngine も同じ水位で発火することを見る。
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

#[tokio::test]
async fn a_skill_engine_mid_turn_fires_at_45001() {
    let pad = text_over_tokens(HIGH);
    assert!(estimate_tokens(&pad) > HIGH);
    let _ = take_governor_events();
    let mut engine = SkillEngine::new(
        Box::new(OnceTextLlm { text: "ok".into() }),
        Box::new(LoopingExecutor),
        3,
    );
    engine.set_conversation_waters(HIGH, LOW);
    let result = engine.run("system", &pad, "mock").await.unwrap();
    assert_eq!(result.response, "ok");
    let fired = compact_fired_phases(&take_governor_events());
    assert!(
        fired.contains(&CompactPhase::MidTurn),
        "SkillEngine 本番 append 境界で 45,001 相当が発火する: {fired:?}"
    );
}

/// (b) 本番経路の発火点をイベント順で観測する。
/// 終了直後に走る / 次の開始時には走らない / 途中超過で走る。
#[test]
fn b_fire_points_event_order() {
    let conn = opencrab_db::init_memory().unwrap();
    seed_over_high(&conn);
    let _ = take_governor_events();

    let assembled = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
    assert!(
        assembled.tokens > HIGH,
        "終了圧縮の前提: assembled={} が高水位以下",
        assembled.tokens
    );
    let mut gov = TurnGovernor::new(HIGH, LOW);
    let end = gov
        .finish_turn(
            &conn,
            SESSION,
            &assembled.items,
            &assembled.checkpoint,
            assembled.through_log_id,
            &assembled.text,
        )
        .unwrap();
    assert!(end.fired, "終了直後の本番 finish_turn が発火する");

    let started = build_conversation_string_with_waters(&conn, SESSION, AGENT, HIGH, LOW, false)
        .expect("開始組立");
    assert!(!started.is_empty(), "開始組立が空: {started}");

    let pad = text_over_tokens(HIGH);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut engine = SkillEngine::new(
            Box::new(OnceTextLlm { text: "ok".into() }),
            Box::new(LoopingExecutor),
            3,
        );
        engine.set_conversation_waters(HIGH, LOW);
        engine.run("system", &pad, "mock").await.unwrap();
    });

    let events = take_governor_events();
    let fired = compact_fired_phases(&events);
    assert!(
        fired.contains(&CompactPhase::TurnEnd),
        "終了直後に走る: {fired:?} / {events:?}"
    );
    assert!(
        !fired.contains(&CompactPhase::TurnStart),
        "開始時には走らない: {fired:?} / {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            GovernorEvent::Inspect {
                phase: CompactPhase::TurnStart,
                ..
            }
        )),
        "開始は検査する: {events:?}"
    );
    assert!(
        fired.contains(&CompactPhase::MidTurn),
        "途中超過で走る: {fired:?} / {events:?}"
    );
    let end_pos = fired
        .iter()
        .position(|p| *p == CompactPhase::TurnEnd)
        .unwrap();
    let mid_pos = fired
        .iter()
        .position(|p| *p == CompactPhase::MidTurn)
        .unwrap();
    assert!(end_pos < mid_pos, "終了が先、途中が後: {fired:?}");
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

struct OnceTextLlm {
    text: String,
}

#[async_trait]
impl LlmClient for OnceTextLlm {
    async fn chat(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
        Ok(text_resp(&self.text))
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

/// (c) 到達点生存の新旧対比。新側は高水位を超えて圧縮が起き、針が 1 回だけ残る。
#[tokio::test]
async fn c_checkpoint_survival_old_loops_new_oneshot() {
    let conn = opencrab_db::init_memory().unwrap();
    seed_over_high(&conn);
    let logs = opencrab_db::queries::list_session_logs_by_session(&conn, SESSION).unwrap();
    let retained = crate::conversation::retain_conversation_logs(logs);

    let old_text = fit_logs_to_budget(&retained, AGENT, 1_200);
    assert!(
        !old_text.contains(CHECKPOINT_NEEDLE),
        "旧実装は中間到達点を落とす前提: {old_text}"
    );
    let (old_resp, old_iters, old_confirms) = run_seek(&old_text).await;
    assert!(
        old_iters > 1 && old_confirms > 0,
        "旧実装は確認をやり直してループする: resp={old_resp} iters={old_iters} confirms={old_confirms}"
    );

    let (items, lane) = items_from_logs(&conn, SESSION, AGENT, &retained).unwrap();
    let before: usize = items.iter().map(|i| i.tokens).sum::<usize>() + lane.tokens();
    assert!(
        before > HIGH,
        "新側は高水位を超える fixture: before={before}"
    );
    let new_out = compact_to_low_water(&items, &lane, HIGH, LOW);
    assert!(new_out.fired, "新側は圧縮を跨ぐ: before={before}");
    assert!(
        new_out.after_tokens <= LOW || new_out.low_water_unreachable,
        "圧縮後 after={}",
        new_out.after_tokens
    );
    let hits = new_out.text.matches(CHECKPOINT_NEEDLE).count();
    assert_eq!(
        hits, 1,
        "到達点は byte-exact で一度だけ: hits={hits} text={}",
        new_out.text
    );
    assert!(!new_out.text.contains(CHECKPOINT_EMPTY_MARKER));
    let (new_resp, new_iters, new_confirms) = run_seek(&new_out.text).await;
    assert_eq!(new_resp, "確認完了");
    assert_eq!(new_iters, 1, "新実装は 1 発完了");
    assert_eq!(new_confirms, 0, "確認のやり直しはしない");
}

/// (d) 連続投稿しても圧縮が毎回発火しない。実際の finish_turn 回数で数える。
#[test]
fn d_hysteresis_consecutive_posts_do_not_refire() {
    let conn = opencrab_db::init_memory().unwrap();
    seed_over_high(&conn);
    let _ = take_governor_events();

    let assembled = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
    let mut gov = TurnGovernor::new(HIGH, LOW);
    let first = gov
        .finish_turn(
            &conn,
            SESSION,
            &assembled.items,
            &assembled.checkpoint,
            assembled.through_log_id,
            &assembled.text,
        )
        .unwrap();
    assert!(first.fired, "最初の投稿で圧縮する");
    assert!(first.after_tokens <= LOW || first.low_water_unreachable);

    for n in 0..5 {
        insert_speech(&conn, "owner", &format!("post-{n} {}", "z".repeat(80)));
        let assembled = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
        let mut gov = TurnGovernor::new(HIGH, LOW);
        let again = gov
            .finish_turn(
                &conn,
                SESSION,
                &assembled.items,
                &assembled.checkpoint,
                assembled.through_log_id,
                &assembled.text,
            )
            .unwrap();
        assert!(
            !again.fired,
            "低水位へ戻った後の連続投稿 {n} で再発火: tokens={}",
            assembled.tokens
        );
    }

    let fires = compact_fired_phases(&take_governor_events());
    assert_eq!(
        fires,
        vec![CompactPhase::TurnEnd],
        "ヒステリシスが効かず毎回発火した: {fires:?}"
    );
}
