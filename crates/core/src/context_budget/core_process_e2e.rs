//! 826-B 必須 4 点（mock LLM のみ）。SkillEngine の MockLlm 基盤と本番組立/終了経路で回す。

use super::compact::{
    compact_to_low_water, should_compact, CompactItem, CompactLane, CompactPhase,
};
use super::governor::{
    assemble_from_snapshot, items_from_logs, take_governor_events, GovernorEvent, TurnGovernor,
};
use crate::conversation::build_conversation_string_with_waters;
use crate::engine::{ActionExecutor, ActionResult, ChatRequest, SkillEngine};
use crate::tokens::estimate_tokens;
use crate::LlmClient;
use async_trait::async_trait;
use opencrab_llm_types::{
    ChatResponse, Choice, FunctionDefinition, Message, MessageContent, Role, Usage,
};

const HIGH: usize = 45_000;
const LOW: usize = 20_000;
const AGENT: &str = "agent-1";
const SESSION: &str = "sess-826b";
const ORIGIN_UTTERANCE: &str = "発端: 大きなタスクを完了せよ";

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
    insert_speech(conn, "owner", ORIGIN_UTTERANCE);
    insert_speech(conn, AGENT, "着手した");
    let blob = "hist ".repeat(2_000);
    for i in 0..30 {
        insert_speech(conn, "owner", &format!("turn-{i} {blob}"));
    }
    let pad = "pad ".repeat(80);
    for i in 0..46 {
        insert_tool_pair(conn, i, &pad);
    }
}

/// 発端 1 + 直近 4 の user speech（must_keep 5）と 46 往復の tool。user を増やさないので発端が窓に残る。
fn seed_long_task_with_recent_speech(conn: &rusqlite::Connection) -> Vec<String> {
    insert_speech(conn, "owner", ORIGIN_UTTERANCE);
    insert_speech(conn, AGENT, "着手した");
    let blob = "hist ".repeat(2_000);
    for i in 0..30 {
        insert_speech(conn, AGENT, &format!("old-ack-{i} {blob}"));
    }
    let pad = "pad ".repeat(80);
    for i in 0..46 {
        insert_tool_pair(conn, i, &pad);
    }
    let mut recent = Vec::new();
    for i in 1..=4 {
        let text = format!("到達点-speech-{i}");
        insert_speech(conn, "owner", &text);
        recent.push(text);
        insert_speech(conn, AGENT, &format!("ack-speech-{i}"));
    }
    recent
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

fn assert_verbatim_window(text: &str, recent: &[String]) {
    assert!(
        text.contains(ORIGIN_UTTERANCE),
        "発端 user 発話が落ちた: {text}"
    );
    for needle in recent {
        assert!(
            text.contains(needle),
            "must_keep speech が落ちた ({needle}): {text}"
        );
    }
}

/// (a) 高水位超過→低水位まで削減の数値アサート（境界値込み）。
/// compact_to_low_water が本番圧縮器。SkillEngine も同じ水位で発火することを見る。
#[test]
fn a_high_water_cut_to_low_with_boundaries() {
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
    let stay = compact_to_low_water(&at_high, HIGH, LOW);
    assert!(!stay.fired);
    assert_eq!(stay.before_tokens, 45_000);
    assert_eq!(stay.after_tokens, 45_000);

    let mut over = at_high;
    over.push(compact_item("over", 1, CompactLane::OldHistory, Some(45)));
    assert!(should_compact(45_001, HIGH));
    let cut = compact_to_low_water(&over, HIGH, LOW);
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

/// (c) 46 往復級の長タスクを checkpoint 無しで圧縮しても、発端と直近 5 speech が残る。
#[test]
fn c_verbatim_window_keeps_origin_and_recent_speech_without_checkpoint() {
    let conn = opencrab_db::init_memory().unwrap();
    let recent = seed_long_task_with_recent_speech(&conn);
    let logs = opencrab_db::queries::list_session_logs_by_session(&conn, SESSION).unwrap();
    let retained = crate::conversation::retain_conversation_logs(logs);

    let items = items_from_logs(&conn, SESSION, AGENT, &retained).unwrap();
    let before: usize = items.iter().map(|i| i.tokens).sum();
    assert!(
        before > HIGH,
        "長タスク fixture が高水位を超えること: before={before}"
    );
    let compacted = compact_to_low_water(&items, HIGH, LOW);
    assert!(compacted.fired, "圧縮が発火すること: before={before}");
    assert_verbatim_window(&compacted.text, &recent);

    let assembled = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
    let mut gov = TurnGovernor::new(HIGH, LOW);
    let finished = gov
        .finish_turn(
            &conn,
            SESSION,
            &assembled.items,
            assembled.through_log_id,
            &assembled.text,
        )
        .unwrap();
    assert!(finished.fired, "本番 finish_turn でも圧縮する");
    assert_verbatim_window(&finished.text, &recent);

    let started = build_conversation_string_with_waters(&conn, SESSION, AGENT, HIGH, LOW, false)
        .expect("圧縮後の開始組立");
    assert_verbatim_window(&started, &recent);
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

/// 撤去の完全性。SQLite WAL の `wal_checkpoint` など別物は対象外。
#[test]
fn context_arrival_point_mechanism_is_gone() {
    let tool = format!("{}_{}", "update_context", "checkpoint");
    let marker = format!("{}_{}", "context", "checkpoint");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let mut hits = Vec::new();
    walk_for_needles(&root, &[&tool, &marker], &mut hits);
    assert!(
        hits.is_empty(),
        "到達点チェックポイント参照が残っている:\n{}",
        hits.join("\n")
    );
}

fn walk_for_needles(dir: &std::path::Path, needles: &[&str], hits: &mut Vec<String>) {
    const SKIP: &[&str] = &["target", ".git", "node_modules"];
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if SKIP.iter().any(|s| name == *s) {
            continue;
        }
        if path.is_dir() {
            walk_for_needles(&path, needles, hits);
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !matches!(ext, "rs" | "md" | "json" | "toml" | "ts" | "tsx" | "js") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for needle in needles {
            if text.contains(needle) {
                hits.push(format!("{}: {needle}", path.display()));
            }
        }
    }
}
