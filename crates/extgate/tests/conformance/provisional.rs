// D-1085 P5: a `provisional` frame runs the realtime said ahead of its commit without recording
// anything; the said carrying the same `provisional_id` and inputs reuses that run.

/// D-1085 §12: when armed, every run calls one gated tool. A provisional run stops there without
/// running it (and aborts its gate) as the engine does; the normal turn runs it, counted here.
#[derive(Clone, Default)]
struct GatedToolScript {
    armed: Arc<AtomicBool>,
    executed: Arc<AtomicUsize>,
}

impl GatedToolScript {
    fn run(
        &self,
        gate: Option<&opencrab_core::PromotionGate>,
        turns: &AtomicUsize,
    ) -> Option<anyhow::Result<EngineResult>> {
        if !self.armed.load(Ordering::SeqCst) {
            return None;
        }
        let Some(gate) = gate else {
            self.executed.fetch_add(1, Ordering::SeqCst);
            return None;
        };
        gate.abort();
        turns.fetch_add(1, Ordering::SeqCst);
        Some(Err(opencrab_core::StoppedAtGatedTool.into()))
    }
}

const PROVISIONAL_ID: &str = "0b7f2c1e-4a6d-4c1b-9f3e-2d8a5b6c7e10";

fn provisional_frame(id: &str, binding_id: &str, provisional_id: &str, text: &str) -> Value {
    json!({
        "id": id, "m": "provisional", "binding_id": binding_id,
        "provisional_id": provisional_id, "author_id": "u1",
        "caller": {"role": "owner"}, "model_profile": "realtime",
        "system_context": "（音声）", "text": text
    })
}

fn committed_said(id: &str, binding_id: &str, provisional_id: &str, text: &str) -> Value {
    json!({
        "id": id, "m": "said", "binding_id": binding_id,
        "origin": format!("o-{id}"), "author_id": "u1",
        "caller": {"role": "owner"}, "model_profile": "realtime",
        "system_context": "（音声）", "provisional_id": provisional_id,
        "text": text, "attachments": []
    })
}

fn with_prelude(mut frame: Value, prelude: &str) -> Value {
    frame["agent_prelude"] = json!(prelude);
    frame
}

fn table_row_count(h: &Harness, table: &str) -> i64 {
    h.state
        .db
        .lock()
        .unwrap()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn provisional_run_count(h: &Harness) -> usize {
    h.runtime
        .provisional_runs
        .lock()
        .unwrap()
        .iter()
        .filter(|(provisional, _)| *provisional)
        .count()
}

/// Sends a provisional with one scripted partial and returns that partial frame.
async fn start_streaming_provisional(
    h: &Harness,
    s: &mut UnixStream,
    binding_id: &str,
    text: &str,
) -> Value {
    h.runtime.partial_script.set(vec![
        PartialStep::Begin("stream-p"),
        PartialStep::Text("stream-p", "はい。"),
    ]);
    write_frame(s, &provisional_frame("p1", binding_id, PROVISIONAL_ID, text)).await;
    let mut partial = None;
    let mut ok = false;
    for _ in 0..40 {
        let Some(frame) = read_frame_opt(s).await else {
            continue;
        };
        if frame["id"] == "p1" {
            assert_eq!(frame["m"], "ok", "{frame}");
            ok = true;
        } else if frame["m"] == "partial" {
            partial = Some(frame);
        }
        if let (true, Some(partial)) = (ok, &partial) {
            return partial.clone();
        }
    }
    panic!("provisional did not stream (ok={ok})");
}

/// Waits until the provisional run has returned its result (it is then awaited at promotion).
async fn wait_provisional_runs(h: &Harness, n: usize) {
    wait_turns(h, n).await;
}

/// The said's `provisional_result` and the frames up to the turn's `ended` activity.
async fn commit_frames(s: &mut UnixStream, said_id: &str) -> (Value, Vec<Value>) {
    let mut result = None;
    let mut frames = Vec::new();
    for _ in 0..60 {
        let Some(frame) = read_frame_opt(s).await else {
            continue;
        };
        if frame["m"] == "say" {
            assert!(result.is_some(), "say before provisional_result: {frame}");
            write_frame(s, &json!({"id": frame["id"], "m": "ok"})).await;
        }
        if frame["m"] == "provisional_result" {
            assert!(result.is_none(), "second provisional_result: {frame}");
            result = Some(frame.clone());
        }
        let ended = frame["m"] == "activity" && frame["state"] == "ended";
        if frame["id"] != said_id {
            frames.push(frame);
        }
        if ended {
            return (result.expect("provisional_result before ended"), frames);
        }
    }
    panic!("turn did not end: {frames:?}");
}

/// S1: only the realtime profile can be provisional.
#[tokio::test]
async fn provisional_with_default_profile_is_bad_request_and_starts_nothing() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let mut frame = provisional_frame("p1", &binding_id, PROVISIONAL_ID, "（音声）今日は");
    frame.as_object_mut().unwrap().remove("model_profile");
    write_frame(&mut s, &frame).await;
    assert_eq!(read_said_response(&mut s, "p1").await["code"], "bad_request");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(h.runtime.provisional_runs.lock().unwrap().is_empty());
    assert!(h.state.provisionals.is_empty());
}

/// S2: a provisional run streams partials tagged with its id and records nothing.
#[tokio::test]
async fn provisional_run_streams_tagged_partials_and_records_nothing() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let partial = start_streaming_provisional(&h, &mut s, &binding_id, "（音声）今日は").await;
    assert_eq!(partial["provisional_id"], PROVISIONAL_ID);
    assert_eq!(partial["stream_id"], "stream-p");
    wait_provisional_runs(&h, 1).await;
    while let Some(frame) = read_frame_opt(&mut s).await {
        assert_ne!(frame["m"], "say", "{frame}");
        assert_ne!(frame["m"], "activity", "{frame}");
    }
    assert_eq!(*h.runtime.provisional_runs.lock().unwrap(), [(true, true)]);
    assert_eq!(*h.runtime.lightweight_runs.lock().unwrap(), [true]);
    assert_eq!(table_row_count(&h, "memory_sessions"), 0);
    assert_eq!(table_row_count(&h, "external_origins"), 0);
    let conversation = h.runtime.conversations.lock().unwrap().last().cloned().unwrap();
    assert!(conversation.ends_with("speech:（音声）今日は"), "{conversation}");
}

/// S3: cancel aborts the run; nothing of it follows the cancel `ok`.
#[tokio::test]
async fn provisional_cancel_aborts_the_run() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let (_release_tx, release_rx) = oneshot::channel();
    *h.runtime.hold_rx.lock().unwrap() = Some(release_rx);
    start_streaming_provisional(&h, &mut s, &binding_id, "（音声）今日は").await;
    write_frame(
        &mut s,
        &json!({"id": "c1", "m": "provisional_cancel", "binding_id": binding_id,
                "provisional_id": PROVISIONAL_ID}),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "c1").await["m"], "ok");
    assert!(h.state.provisionals.is_empty());
    assert!(read_frame_opt(&mut s).await.is_none());
    // The held run never returned.
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0);
    // Idempotent.
    write_frame(
        &mut s,
        &json!({"id": "c2", "m": "provisional_cancel", "binding_id": binding_id,
                "provisional_id": PROVISIONAL_ID}),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "c2").await["m"], "ok");
}

/// S4: the said with the same inputs reuses the run: one LLM call, the say continues the
/// provisional stream, and the said is recorded once.
#[tokio::test]
async fn said_with_same_inputs_promotes_the_provisional_run() {
    let h = Harness::start().await;
    *h.runtime.reply.lock().unwrap() = "はい。".into();
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);
    start_streaming_provisional(&h, &mut s, &binding_id, "（音声）今日は").await;

    write_frame(&mut s, &committed_said("v1", &binding_id, PROVISIONAL_ID, "（音声）今日は")).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    let (result, frames) = commit_frames(&mut s, "v1").await;

    assert_eq!(
        result,
        json!({"m": "provisional_result", "binding_id": binding_id,
               "provisional_id": PROVISIONAL_ID, "promoted": true})
    );
    assert_eq!(h.runtime.provisional_runs.lock().unwrap().len(), 1, "one LLM call in total");
    let say = frames.iter().find(|f| f["m"] == "say").expect("say");
    assert_eq!(say["payload"]["text"], "はい。");
    assert_eq!(say["payload"]["stream_id"], "stream-p");
    let users: Vec<_> = session_rows(&h, &session_id)
        .into_iter()
        .filter(|(_, speaker, _, _)| speaker.as_deref() == Some("u1"))
        .collect();
    assert_eq!(users.len(), 1, "{users:?}");
    assert!(h.state.provisionals.is_empty());
}

/// S5: different text: the run is aborted and the normal turn runs.
#[tokio::test]
async fn said_with_other_text_falls_back_to_the_normal_turn() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    start_streaming_provisional(&h, &mut s, &binding_id, "（音声）今日は").await;

    write_frame(&mut s, &committed_said("v1", &binding_id, PROVISIONAL_ID, "（音声）今日はいい天気")).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    let (result, frames) = commit_frames(&mut s, "v1").await;

    assert_eq!(result["promoted"], false);
    assert!(frames.iter().any(|f| f["m"] == "say"));
    assert_eq!(
        *h.runtime.provisional_runs.lock().unwrap(),
        [(true, true), (false, false)],
        "provisional + normal turn"
    );
    let conversation = h.runtime.conversations.lock().unwrap().last().cloned().unwrap();
    assert!(conversation.ends_with("speech:（音声）今日はいい天気"), "{conversation}");
}

/// S6: a row recorded after the provisional's snapshot: the normal turn runs.
#[tokio::test]
async fn said_after_history_moved_falls_back_to_the_normal_turn() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);
    start_streaming_provisional(&h, &mut s, &binding_id, "（音声）今日は").await;
    {
        let conn = h.state.db.lock().unwrap();
        opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "agent-1".into(),
                session_id: session_id.clone(),
                log_type: "speech".into(),
                content: "ほかの発言".into(),
                speaker_id: Some("u2".into()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    write_frame(&mut s, &committed_said("v1", &binding_id, PROVISIONAL_ID, "（音声）今日は")).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    let (result, _) = commit_frames(&mut s, "v1").await;

    assert_eq!(result["promoted"], false);
    assert_eq!(provisional_run_count(&h), 1);
    assert_eq!(h.runtime.provisional_runs.lock().unwrap().len(), 2);
}

/// S7: a promoted NO_REPLY run ends the turn silently.
#[tokio::test]
async fn promoted_no_reply_ends_silently() {
    let h = Harness::start().await;
    *h.runtime.reply.lock().unwrap() = "NO_REPLY".into();
    let (mut s, _, binding_id) = ready_pair(&h).await;
    start_streaming_provisional(&h, &mut s, &binding_id, "（音声）うん").await;

    write_frame(&mut s, &committed_said("v1", &binding_id, PROVISIONAL_ID, "（音声）うん")).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    let (result, frames) = commit_frames(&mut s, "v1").await;

    assert_eq!(result["promoted"], true);
    assert!(frames.iter().all(|f| f["m"] != "say"), "{frames:?}");
    let ended = frames.last().unwrap();
    assert_eq!(ended["silent_origins"], json!(["o-v1"]), "{ended}");
    assert_eq!(h.runtime.provisional_runs.lock().unwrap().len(), 1);
}

/// S8: a second provisional of the same speaker replaces the first.
#[tokio::test]
async fn second_provisional_of_the_same_author_replaces_the_first() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let (_release_tx, release_rx) = oneshot::channel();
    *h.runtime.hold_rx.lock().unwrap() = Some(release_rx);
    start_streaming_provisional(&h, &mut s, &binding_id, "（音声）今日は").await;

    let second = "1c2d3e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f";
    write_frame(&mut s, &provisional_frame("p2", &binding_id, second, "（音声）今日はいい")).await;
    assert_eq!(read_said_response(&mut s, "p2").await["m"], "ok");

    assert_eq!(h.state.provisionals.len(), 1);
    write_frame(&mut s, &committed_said("v1", &binding_id, PROVISIONAL_ID, "（音声）今日は")).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    let (result, _) = commit_frames(&mut s, "v1").await;
    assert_eq!(result["promoted"], false, "the first run was replaced");
}

/// S9: a provisional runs while a turn of the session holds the lock.
#[tokio::test]
async fn provisional_streams_while_a_turn_holds_the_session() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let (release_tx, release_rx) = oneshot::channel();
    *h.runtime.hold_rx.lock().unwrap() = Some(release_rx);
    let entered = h.runtime.turn_entered.notified();
    tokio::pin!(entered);
    write_frame(&mut s, &said_with_prelude("t1", &binding_id, None)).await;
    assert_eq!(read_said_response(&mut s, "t1").await["m"], "ok");
    tokio::time::timeout(Duration::from_secs(2), &mut entered)
        .await
        .expect("held turn entered");

    let partial = start_streaming_provisional(&h, &mut s, &binding_id, "（音声）今日は").await;
    assert_eq!(partial["provisional_id"], PROVISIONAL_ID);
    let _ = release_tx.send(());
}

/// S10: the agent prelude is part of the match.
#[tokio::test]
async fn agent_prelude_must_match_for_promotion() {
    for (said_prelude, promoted) in [(Some("うん、"), true), (Some("へえ、"), false), (None, false)] {
        let h = Harness::start().await;
        let (mut s, _, binding_id) = ready_pair(&h).await;
        h.runtime.partial_script.set(vec![
            PartialStep::Begin("stream-p"),
            PartialStep::Text("stream-p", "はい。"),
        ]);
        let frame = with_prelude(
            provisional_frame("p1", &binding_id, PROVISIONAL_ID, "（音声）今日は"),
            "うん、",
        );
        write_frame(&mut s, &frame).await;
        assert_eq!(read_said_response(&mut s, "p1").await["m"], "ok");
        let conversation = loop {
            if let Some(c) = h.runtime.conversations.lock().unwrap().last().cloned() {
                break c;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(
            conversation.ends_with("speech:（音声）今日は\nspeech:うん、"),
            "{conversation}"
        );

        let mut said = committed_said("v1", &binding_id, PROVISIONAL_ID, "（音声）今日は");
        if let Some(prelude) = said_prelude {
            said = with_prelude(said, prelude);
        }
        write_frame(&mut s, &said).await;
        assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
        let (result, _) = commit_frames(&mut s, "v1").await;
        assert_eq!(result["promoted"], promoted, "{said_prelude:?}");
        let calls = h.runtime.provisional_runs.lock().unwrap().len();
        assert_eq!(calls, if promoted { 1 } else { 2 }, "{said_prelude:?}");
    }
}

/// §12: the provisional run gets the operations and the dispatch sink of the said's normal turn,
/// so the two runs offer the same tools.
#[tokio::test]
async fn provisional_run_has_the_normal_turn_tool_sources() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance_config(&h, &instance_id, TOOL_DRIVEN_B64).await;
    put_binding(&h, &binding_id, &instance_id, "chan-1").await;
    let mut s = h.connect().await;
    hello_ok_digest(&mut s, &instance_id, 1, &tool_driven_digest()).await;
    assert_eq!(ack_bind(&mut s).await, binding_id);
    wait_acknowledged(&h, &instance_id, &binding_id).await;
    start_streaming_provisional(&h, &mut s, &binding_id, "（音声）今日は").await;

    write_frame(&mut s, &committed_said("v1", &binding_id, PROVISIONAL_ID, "（音声）今日はいい天気")).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    commit_frames(&mut s, "v1").await;

    let sources = h.runtime.tool_sources.lock().unwrap().clone();
    assert_eq!(sources.len(), 2, "provisional + normal turn: {sources:?}");
    assert_eq!(sources[0], (vec!["reply".to_string()], true));
    assert_eq!(sources[0], sources[1]);
}

/// §12 R1: the provisional stopped at a gated tool before the said: `promoted:false`, and the
/// normal turn runs the tool once.
#[tokio::test]
async fn provisional_stopped_at_a_gated_tool_is_not_promoted() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    h.runtime.gated_tool.armed.store(true, Ordering::SeqCst);
    start_streaming_provisional(&h, &mut s, &binding_id, "（音声）落ちて").await;
    wait_provisional_runs(&h, 1).await;
    assert_eq!(h.runtime.gated_tool.executed.load(Ordering::SeqCst), 0);

    write_frame(&mut s, &committed_said("v1", &binding_id, PROVISIONAL_ID, "（音声）落ちて")).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    let (result, frames) = commit_frames(&mut s, "v1").await;

    assert_eq!(result["promoted"], false);
    assert!(frames.iter().any(|f| f["m"] == "say"));
    assert_eq!(
        *h.runtime.provisional_runs.lock().unwrap(),
        [(true, true), (false, false)],
        "provisional + normal turn"
    );
    assert_eq!(h.runtime.gated_tool.executed.load(Ordering::SeqCst), 1);
}

/// §12 R2: promoted first, then the run reaches a gated tool and stops without running it; the
/// said's normal turn runs it once and delivers on the provisional's stream.
#[tokio::test]
async fn promoted_provisional_that_stops_at_a_gated_tool_reruns_as_the_normal_turn() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    h.runtime.gated_tool.armed.store(true, Ordering::SeqCst);
    let (release_tx, release_rx) = oneshot::channel();
    *h.runtime.hold_rx.lock().unwrap() = Some(release_rx);
    start_streaming_provisional(&h, &mut s, &binding_id, "（音声）落ちて").await;

    write_frame(&mut s, &committed_said("v1", &binding_id, PROVISIONAL_ID, "（音声）落ちて")).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    let mut result = None;
    for _ in 0..40 {
        if let Some(frame) = read_frame_opt(&mut s).await {
            if frame["m"] == "provisional_result" {
                result = Some(frame);
                break;
            }
        }
    }
    assert_eq!(result.expect("provisional_result")["promoted"], true);
    assert_eq!(h.runtime.gated_tool.executed.load(Ordering::SeqCst), 0, "not before the stop");
    release_tx.send(()).unwrap();
    let mut frames = Vec::new();
    for _ in 0..60 {
        let Some(frame) = read_frame_opt(&mut s).await else {
            continue;
        };
        if frame["m"] == "say" {
            write_frame(&mut s, &json!({"id": frame["id"], "m": "ok"})).await;
        }
        let ended = frame["m"] == "activity" && frame["state"] == "ended";
        frames.push(frame);
        if ended {
            break;
        }
    }

    assert_eq!(
        *h.runtime.provisional_runs.lock().unwrap(),
        [(true, true), (false, false)],
        "provisional + the normal turn"
    );
    assert_eq!(h.runtime.gated_tool.executed.load(Ordering::SeqCst), 1);
    let say = frames.iter().find(|f| f["m"] == "say").expect("say");
    assert_eq!(say["payload"]["stream_id"], "stream-p");
    assert!(frames.iter().any(|f| f["m"] == "activity" && f["state"] == "ended"));
}

/// §13.2: sends a provisional whose run speaks 「探すね。」 as continuation speech, then waits until
/// the run has returned.
async fn start_speaking_provisional(h: &Harness, s: &mut UnixStream, binding_id: &str, text: &str) {
    h.runtime.partial_script.set(vec![
        PartialStep::Begin("stream-p"),
        PartialStep::Text("stream-p", "探すね。"),
        PartialStep::Say("探すね。"),
    ]);
    write_frame(s, &provisional_frame("p1", binding_id, PROVISIONAL_ID, text)).await;
    wait_provisional_runs(h, 1).await;
}

fn agent_speech(h: &Harness, session_id: &str) -> Vec<String> {
    session_rows(h, session_id)
        .into_iter()
        .filter(|(kind, speaker, _, _)| kind == "speech" && speaker.as_deref() == Some("agent-1"))
        .map(|(_, _, content, _)| content)
        .collect()
}

/// §13.2: the continuation speech of a provisional run is held: nothing is said or recorded
/// before the said. At the promotion it is said (on the run's stream) and recorded, before the
/// final answer.
#[tokio::test]
async fn promoted_provisional_says_its_held_continuation_speech_then_the_answer() {
    let h = Harness::start().await;
    *h.runtime.reply.lock().unwrap() = "見つけた。".into();
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);
    start_speaking_provisional(&h, &mut s, &binding_id, "（音声）探して").await;
    assert!(agent_speech(&h, &session_id).is_empty(), "nothing recorded before the said");

    write_frame(&mut s, &committed_said("v1", &binding_id, PROVISIONAL_ID, "（音声）探して")).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    let (result, frames) = commit_frames(&mut s, "v1").await;

    assert_eq!(result["promoted"], true);
    let says: Vec<_> = frames.iter().filter(|f| f["m"] == "say").collect();
    assert_eq!(says.len(), 2, "{frames:?}");
    assert_eq!(says[0]["payload"]["text"], "探すね。");
    assert_eq!(says[0]["payload"]["stream_id"], "stream-p");
    assert_eq!(says[1]["payload"]["text"], "見つけた。");
    assert_eq!(agent_speech(&h, &session_id), ["探すね。", "見つけた。"]);
}

/// §13.2: a said that does not match drops the held continuation speech unsaid and unrecorded.
#[tokio::test]
async fn unpromoted_provisional_drops_its_held_continuation_speech() {
    let h = Harness::start().await;
    *h.runtime.reply.lock().unwrap() = "見つけた。".into();
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);
    start_speaking_provisional(&h, &mut s, &binding_id, "（音声）探して").await;

    write_frame(&mut s, &committed_said("v1", &binding_id, PROVISIONAL_ID, "（音声）別の話")).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    let (result, frames) = commit_frames(&mut s, "v1").await;

    assert_eq!(result["promoted"], false);
    let says: Vec<_> = frames
        .iter()
        .filter(|f| f["m"] == "say")
        .map(|f| f["payload"]["text"].clone())
        .collect();
    assert_eq!(says, [json!("見つけた。")], "{frames:?}");
    assert_eq!(agent_speech(&h, &session_id), ["見つけた。"]);
}
