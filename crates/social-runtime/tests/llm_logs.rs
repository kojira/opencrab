//! LLM ログの永続化（#766）。1 反復 = 1 行で、そのターンで「何を送り・何が返り・どう終わったか」を
//! DB から直接引けること。推論は差し替えた偽物（ScriptedEngine）で回し、時間は pause() で進める。

use opencrab_engine::*;
use opencrab_port::*;
use opencrab_social_runtime::*;
use opencrab_store::Store;
use std::sync::Arc;

const TEST_MODEL: &str = "scripted";
const GATE: &str = "nostr";
const ADDR: &str = "wss://relay/pub";

struct Harness {
    sys: System,
    eng: ScriptedEngine,
}

fn build() -> Harness {
    let store = Store::new_in_memory().unwrap();
    store
        .register_model_context_window(TEST_MODEL, 1_000_000)
        .unwrap();
    let eng = ScriptedEngine::new();
    let sys = System::new(
        store,
        Arc::new(eng.clone()),
        Arc::new(ScriptedToolHost::new()),
        Arc::new(ScriptedShellHost::new()),
        Arc::new(RecordingNotifier::new()),
        Arc::new(CharCounter),
        Config::default(),
    );
    Harness { sys, eng }
}

async fn settle() {
    for _ in 0..400 {
        tokio::task::yield_now().await;
    }
}

fn firing_place(h: &Harness) -> i64 {
    let agent = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(agent),
        None,
    );
    h.sys.join(p, agent, Role::Participant);
    h.sys
        .store()
        .add_channel(p, &GateName::new(GATE), ADDR)
        .unwrap();
    p
}

fn said_event(text: &str, origin: &str) -> GateEvent {
    GateEvent {
        kind: EventKind::Said,
        address: ADDR.to_string(),
        author_external: "stranger".to_string(),
        author_display: None,
        content: Content::text(text),
        mentions: vec![],
        reply_to: None,
        target: None,
        origin: Some(origin.to_string()),
        attachments: vec![],
        discovery: None,
        metadata: serde_json::json!({}),
    }
}

// 通常完了のターン: 送った文脈（rendered に入力が入る）と返った発話が、turn/iteration 紐付きで
// llm_logs に残る。outcome=done・model は実効モデル名・request/response が逐語で引ける。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn done_turn_persists_request_and_response() {
    let h = build();
    let _p = firing_place(&h);
    h.eng.push(Step::say_done("にゃー"));

    h.sys
        .deliver_event(&GateName::new(GATE), said_event("猫について教えて", "o1"))
        .unwrap();
    settle().await;

    let turns = h.sys.store().all_turn_records().unwrap();
    assert_eq!(turns.len(), 1, "1 ターン走る");
    // エージェント（subject）単位で引ける（本体 API `/llm-logs` と同形）。agent_id=subject の文字列。
    let subject = turns[0].subject;
    let logs = h
        .sys
        .store()
        .list_llm_logs(&subject.to_string(), 10)
        .unwrap();
    assert_eq!(logs.len(), 1, "1 反復 = 1 ログ");
    let log = &logs[0];
    assert_eq!(log.iteration, Some(1));
    assert_eq!(log.turn_record_id, Some(turns[0].id));
    assert_eq!(log.model.as_deref(), Some(TEST_MODEL));
    assert!(!log.is_bot_iteration, "初回反復は継続ではない");
    assert_eq!(log.error_code, None);
    // 送ったもの（入力が user メッセージに入っている・ChatRequestSimple 同型）。
    assert!(
        log.prompt.contains("猫について教えて") && log.prompt.contains("\"messages\""),
        "prompt に送った文脈が正規化 JSON で入る: {}",
        log.prompt
    );
    // 返ったもの（発話本文・ChatResponseSimple 同型）。ネイティブ道具は呼んでいないので tool_calls は None。
    assert!(
        log.response.contains("にゃー") && log.response.contains("\"finish_reason\""),
        "response に返答が入る: {}",
        log.response
    );
    // usage は載せない（欠測）。0 埋めするとダッシュボードが「計測済みでゼロ」と誤読するため（#769 で実測）。
    assert!(
        !log.response.contains("usage"),
        "usage は欠測（省略）: {}",
        log.response
    );
    assert_eq!(log.tool_calls, None);

    // turn 単位のドリルダウン（oc2 連結列）でも同じ 1 件。
    let by_turn = h.sys.store().llm_logs(turns[0].id).unwrap();
    assert_eq!(by_turn.len(), 1);
    assert_eq!(by_turn[0].id, log.id);
}

// 失敗のターン（意味的に空の応答）: engine が回っても記録は必ず書かれ、error_code/error_body が
// 残る（挙動調査で「なぜ落ちたか」を失わない）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn failed_turn_persists_reason() {
    let h = build();
    let p = firing_place(&h);
    // 効果も道具も無い応答 → 意味的に空 → 失敗（EMPTY_RESPONSE_DETAIL）。
    h.eng.push(Step::done());

    h.sys
        .deliver_event(&GateName::new(GATE), said_event("hi", "o1"))
        .unwrap();
    settle().await;

    let turns = h.sys.store().all_turn_records().unwrap();
    assert_eq!(turns.len(), 1);
    let logs = h.sys.store().llm_logs(turns[0].id).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].error_code.as_deref(), Some("empty_response"));
    assert!(
        logs[0]
            .error_body
            .as_deref()
            .unwrap()
            .contains("empty_response"),
        "失敗理由が逐語で残る: {:?}",
        logs[0].error_body
    );
    let _ = p;
}
