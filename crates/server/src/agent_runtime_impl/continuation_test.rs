// D-1085 §13.2: a lightweight turn continues like a normal turn. A text-only generation without
// NO_REPLY is continuation speech, the turn goes on to its tools and ends at the final answer.

type Delivered = Arc<Mutex<Vec<String>>>;

/// Records each continuation speech. With `gate`, its row is recorded at the promotion, in order
/// with the run's held rows, as the external gate records the say of a provisional run.
fn speech_hook(
    f: &Fixture,
    gate: Option<opencrab_core::PromotionGate>,
) -> (opencrab_actions::run_request::ContinuationSpeechHook, Delivered) {
    let delivered: Delivered = Arc::new(Mutex::new(Vec::new()));
    let db = f.state.db.clone();
    let hook: opencrab_actions::run_request::ContinuationSpeechHook = {
        let delivered = delivered.clone();
        Arc::new(move |speech: String| {
            delivered.lock().unwrap().push(speech.clone());
            let db = db.clone();
            let record = move || {
                let conn = db.lock().unwrap();
                opencrab_db::queries::insert_session_log(
                    &conn,
                    &log_row("speech", Some("a1"), &speech, None),
                )
                .unwrap();
            };
            match &gate {
                Some(gate) => gate.on_promoted(record),
                None => record(),
            }
            Box::pin(async { Ok(()) })
        })
    };
    (hook, delivered)
}

/// A text-only generation, then a memory search, then the final answer.
fn speech_then_search(f: &Fixture) {
    *f.script.lock().unwrap() = vec![
        Message::assistant("探すね。"),
        tool_call_message("c1", "search_memory_index", serde_json::json!({"query": "x"})),
    ];
    *f.reply.lock().unwrap() = "見つけた。\nNO_REPLY".to_string();
}

async fn turn_with_speech(
    f: &Fixture,
    profile: TurnModelProfile,
    hook: opencrab_actions::run_request::ContinuationSpeechHook,
) -> opencrab_core::EngineResult {
    let (system, name) = f.state.build_agent_context("a1", &CallerIdentity::Owner);
    let inbound = NormalizedInbound {
        session_id: "sess-a",
        agent_id: "a1",
        sender_id: "u1",
        sender_name: "",
        avatar_url: None,
        channel_id: None,
        pubkey: None,
        text: "hi",
        image_urls: &[],
        external_id: "origin-1",
    };
    start_session_turn(
        &f.state,
        TranscriptSource::new("external", "external_response"),
        &inbound,
        profile,
        &system,
        "",
        |raw| raw.to_string(),
        |conversation| {
            RunRequest::new(
                "a1",
                name.clone(),
                "sess-a",
                system.clone(),
                conversation,
                "test",
                CallerIdentity::Owner,
            )
            .with_on_continuation_speech(hook)
        },
    )
    .await
    .expect("turn ran")
    .expect("turn succeeded")
}

/// §13.2: a realtime turn speaks 「探すね。」, searches, and answers; the default profile does the
/// same as before.
#[tokio::test]
async fn realtime_turn_continues_after_text_only_speech_to_the_tool_and_the_answer() {
    for profile in [TurnModelProfile::Realtime, TurnModelProfile::Default] {
        let f = fixture();
        speech_then_search(&f);
        let hw = high_water(&f);
        let (hook, delivered) = speech_hook(&f, None);
        let result = turn_with_speech(&f, profile, hook).await;
        assert_eq!(*delivered.lock().unwrap(), ["探すね。"], "{profile:?}");
        assert_eq!(result.response, "見つけた。", "{profile:?}");
        assert_eq!(f.requests.lock().unwrap().len(), 3, "{profile:?}");
        let rows = rows_after(&f, hw);
        let speech = rows.iter().position(|r| r == "speech:探すね。");
        let call = rows.iter().position(|r| r == "call:search_memory_index");
        assert!(speech.is_some() && speech < call, "{profile:?}: {rows:?}");
    }
}

/// §13.2: a provisional run promoted by its said speaks, searches and answers; its speech row and
/// tool rows are recorded at the promotion in their real order, after the said's rows.
#[tokio::test]
async fn promoted_provisional_records_speech_then_tool_rows_in_order() {
    let f = fixture();
    speech_then_search(&f);
    let hw = high_water(&f);
    let gate = opencrab_core::PromotionGate::new();
    let (hook, delivered) = speech_hook(&f, Some(gate.clone()));
    let pending = [log_row("speech", Some("u1"), "SAID-USER", None)];
    let (system, name) = f.state.build_agent_context("a1", &CallerIdentity::Owner);
    let run = run_provisional_turn(
        &f.state,
        "sess-a",
        "a1",
        &system,
        hw,
        &pending,
        gate.clone(),
        |conversation| {
            RunRequest::new(
                "a1",
                name.clone(),
                "sess-a",
                system.clone(),
                conversation,
                "test",
                CallerIdentity::Owner,
            )
            .with_on_continuation_speech(hook)
        },
    );
    let said = async {
        wait_for_requests(&f, 3).await;
        assert!(rows_after(&f, hw).is_empty(), "nothing recorded before the said");
        record_said_rows(&f);
        assert!(gate.promote());
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(run, said)
    })
    .await
    .expect("run ends");
    assert_eq!(result.expect("ran").unwrap().response, "見つけた。");
    assert_eq!(*delivered.lock().unwrap(), ["探すね。"]);
    let rows = rows_after(&f, hw);
    assert_eq!(
        rows[..5],
        [
            "speech:SAID-USER",
            "speech:SAID-PRELUDE",
            "speech:探すね。",
            "call:search_memory_index",
            "result:search_memory_index",
        ],
        "{rows:?}"
    );
}
