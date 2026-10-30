// D-1083: a realtime-profile turn writes id-less `partial` frames before the say that carries the
// same `stream_id`; a default-profile turn writes none.

/// What the conformance runtime does in place of the engine's observed LLM calls.
#[derive(Debug, Clone)]
enum PartialStep {
    Begin(&'static str),
    Text(&'static str, &'static str),
    /// An intermediate say (continuation speech) of the current call.
    Say(&'static str),
}

#[derive(Clone, Default)]
struct PartialScript {
    steps: Arc<Mutex<Vec<PartialStep>>>,
    hook_seen: Arc<Mutex<Vec<bool>>>,
}

impl PartialScript {
    fn set(&self, steps: Vec<PartialStep>) {
        *self.steps.lock().unwrap() = steps;
    }

    async fn play(
        &self,
        hook: &Option<opencrab_core::PartialTextHook>,
        say: &Option<opencrab_actions::run_request::ContinuationSpeechHook>,
    ) {
        self.hook_seen.lock().unwrap().push(hook.is_some());
        let Some(hook) = hook else {
            return;
        };
        let steps = std::mem::take(&mut *self.steps.lock().unwrap());
        for step in steps {
            match step {
                PartialStep::Begin(id) => {
                    hook(opencrab_core::PartialTextEvent::Begin { stream_id: id.into() }).await
                }
                PartialStep::Text(id, text) => {
                    hook(opencrab_core::PartialTextEvent::Text {
                        stream_id: id.into(),
                        text: text.into(),
                    })
                    .await
                }
                PartialStep::Say(text) => say.as_ref().expect("continuation hook")(text.into())
                    .await
                    .unwrap(),
            }
        }
    }
}

/// Frames up to the turn's `ended` activity, acknowledging every say.
async fn turn_frames(s: &mut UnixStream) -> Vec<Value> {
    let mut frames = Vec::new();
    for _ in 0..40 {
        let Some(frame) = read_frame_opt(s).await else {
            continue;
        };
        if frame["m"] == "say" {
            write_frame(s, &json!({"id": frame["id"], "m": "ok"})).await;
        }
        let ended = frame["m"] == "activity" && frame["state"] == "ended";
        frames.push(frame);
        if ended {
            return frames;
        }
    }
    panic!("turn did not end: {frames:?}");
}

fn partial_and_say_frames(frames: &[Value]) -> Vec<Value> {
    frames
        .iter()
        .filter(|f| f["m"] == "partial" || f["m"] == "say")
        .cloned()
        .collect()
}

/// C9 + C10: partials of stream S precede the say carrying S, intermediate and final.
#[tokio::test]
async fn realtime_turn_writes_partials_before_the_say_of_the_same_stream() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    *h.runtime.reply.lock().unwrap() = "最後です。".into();
    h.runtime.partial_script.set(vec![
        PartialStep::Begin("stream-1"),
        PartialStep::Text("stream-1", "一文目。"),
        PartialStep::Text("stream-1", "一文目。二文目。"),
        PartialStep::Say("一文目。二文目。"),
        PartialStep::Begin("stream-2"),
        PartialStep::Text("stream-2", "最後です。"),
    ]);

    write_frame(&mut s, &said_frame_with_profile("v1", &binding_id, "o-v1", Some("realtime"))).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    let frames = partial_and_say_frames(&turn_frames(&mut s).await);

    let shape: Vec<(String, Value, Value)> = frames
        .iter()
        .map(|f| {
            let m = f["m"].as_str().unwrap().to_string();
            if m == "partial" {
                (m, f["stream_id"].clone(), f["text"].clone())
            } else {
                (m, f["payload"]["stream_id"].clone(), f["payload"]["text"].clone())
            }
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            ("partial".into(), json!("stream-1"), json!("一文目。")),
            ("partial".into(), json!("stream-1"), json!("一文目。二文目。")),
            ("say".into(), json!("stream-1"), json!("一文目。二文目。")),
            ("partial".into(), json!("stream-2"), json!("最後です。")),
            ("say".into(), json!("stream-2"), json!("最後です。")),
        ]
    );
    for partial in frames.iter().filter(|f| f["m"] == "partial") {
        assert!(partial.get("id").is_none(), "partial frame has no id: {partial}");
        assert_eq!(partial["session"], json!(binding_id));
        assert!(partial.get("stream_id").is_some());
    }
    assert!(frames.iter().filter(|f| f["m"] == "say").all(|f| f.get("stream_id").is_none()));
}

/// C9: a call that released nothing gives its say no stream_id.
#[tokio::test]
async fn realtime_say_without_released_text_has_no_stream_id() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    h.runtime.partial_script.set(vec![PartialStep::Begin("stream-1")]);

    write_frame(&mut s, &said_frame_with_profile("v1", &binding_id, "o-v1", Some("realtime"))).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    let frames = partial_and_say_frames(&turn_frames(&mut s).await);

    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0]["m"], "say");
    assert!(frames[0]["payload"].get("stream_id").is_none(), "{}", frames[0]);
}

/// C9: a default-profile turn gets no partial hook, writes no partial, and its say has no
/// stream_id.
#[tokio::test]
async fn default_turn_has_no_partial_hook_and_no_stream_id() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;

    write_frame(&mut s, &said_frame_with_profile("t1", &binding_id, "o-t1", None)).await;
    assert_eq!(read_said_response(&mut s, "t1").await["m"], "ok");
    let frames = partial_and_say_frames(&turn_frames(&mut s).await);

    assert_eq!(*h.runtime.partial_script.hook_seen.lock().unwrap(), vec![false]);
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0]["m"], "say");
    assert!(frames[0]["payload"].get("stream_id").is_none());
}
