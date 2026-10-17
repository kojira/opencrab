use super::*;
use crate::continue_marker::terminate_at_no_reply;

fn released(deltas: &[&str]) -> String {
    let mut gate = PartialTextGate::new();
    let mut last = String::new();
    for delta in deltas {
        if let Some(text) = gate.push(delta) {
            assert!(text.len() > last.len(), "released text only grows");
            assert!(text.starts_with(&last), "released text is cumulative");
            last = text;
        }
    }
    last
}

/// C3: only complete sentences are released.
#[test]
fn gate_releases_through_the_last_terminator() {
    assert_eq!(released(&["今日は", "晴れ。明日", "は雨"]), "今日は晴れ。");
    assert_eq!(
        released(&["本当！", "そう?", "うん\nまた"]),
        "本当！そう?うん\n"
    );
}

/// C4: a buffer that may become the bare NO_REPLY marker releases nothing.
#[test]
fn gate_releases_nothing_for_a_no_reply_prefix() {
    assert_eq!(released(&["NO_", "REPLY"]), "");
    assert_eq!(released(&["NO_REPLY", "\n"]), "");
}

/// C5: released text is a prefix of the speech kept by `terminate_at_no_reply`.
#[test]
fn gate_release_is_a_prefix_of_no_reply_speech() {
    let full = "了解。\nNO_REPLY";
    let speech = terminate_at_no_reply(full).speech().unwrap().to_string();
    let text = released(&["了解。\nNO_", "REPLY"]);
    assert!(
        speech.starts_with(text.trim_end()),
        "{text:?} vs {speech:?}"
    );
    assert_eq!(text, "了解。\n");
}

/// C5b (R2b): a cut never ends on a trailing NO_REPLY marker.
#[test]
fn gate_never_releases_a_trailing_no_reply_marker() {
    assert_eq!(released(&["了解。\nNO_REPLY\n"]), "了解。\n");
    assert_eq!(released(&["了解。NO_REPLY\n"]), "了解。");
    // Text after the marker makes it ordinary speech again.
    assert_eq!(
        released(&["了解。NO_REPLY\n", "続き。"]),
        "了解。NO_REPLY\n続き。"
    );
}

/// C6: nothing from the tool markup on is released, now or later.
#[test]
fn gate_stops_at_tool_markup() {
    assert_eq!(
        released(&["見ます。<function_calls>", "<invoke name=\"x\">。\n"]),
        "見ます。"
    );
    assert_eq!(released(&["見ます。<func", "tion_calls>。"]), "見ます。");
    // A `<` that is not tool markup does not stop the stream.
    assert_eq!(released(&["a<b です。"]), "a<b です。");
}

/// C7: `、` is not a terminator.
#[test]
fn gate_does_not_release_at_a_comma() {
    assert_eq!(released(&["今日は、晴れ"]), "");
}

/// Engine mock: each call forwards scripted events to the sink, then returns the scripted text.
struct ObservedScriptLlm {
    script: std::sync::Mutex<Vec<(Vec<OutputTextEvent>, String)>>,
    observed_calls: std::sync::atomic::AtomicUsize,
    unobserved_calls: std::sync::atomic::AtomicUsize,
}

impl ObservedScriptLlm {
    fn new(script: Vec<(Vec<OutputTextEvent>, &str)>) -> Arc<Self> {
        Arc::new(Self {
            script: std::sync::Mutex::new(
                script
                    .into_iter()
                    .map(|(events, text)| (events, text.to_string()))
                    .collect(),
            ),
            observed_calls: Default::default(),
            unobserved_calls: Default::default(),
        })
    }

    fn next(&self) -> (Vec<OutputTextEvent>, LlmExchange) {
        let (events, text) = self.script.lock().unwrap().remove(0);
        let response = opencrab_llm_types::ChatResponse {
            id: String::new(),
            model: String::new(),
            choices: vec![opencrab_llm_types::Choice {
                index: 0,
                message: opencrab_llm_types::Message::assistant(text),
                finish_reason: None,
            }],
            usage: Default::default(),
            created: 0,
        };
        (
            events,
            LlmExchange {
                response,
                provider_tool_history: Default::default(),
            },
        )
    }
}

struct SharedLlm(Arc<ObservedScriptLlm>);

#[async_trait::async_trait]
impl crate::engine::types::LlmClient for SharedLlm {
    async fn chat(
        &self,
        _request: ChatRequest,
    ) -> anyhow::Result<opencrab_llm_types::ChatResponse> {
        unreachable!("the engine uses the history path")
    }

    async fn chat_with_history(&self, _request: ChatRequest) -> anyhow::Result<LlmExchange> {
        self.0
            .unobserved_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.0.next().1)
    }

    async fn chat_with_history_observed(
        &self,
        _request: ChatRequest,
        sink: OutputTextSink,
    ) -> anyhow::Result<LlmExchange> {
        self.0
            .observed_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (events, exchange) = self.0.next();
        for event in events {
            sink(event);
        }
        Ok(exchange)
    }
}

struct NoTools;

#[async_trait::async_trait]
impl crate::engine::types::ActionExecutor for NoTools {
    async fn execute(
        &self,
        name: &str,
        _args: &serde_json::Value,
    ) -> crate::engine::types::ActionResult {
        unreachable!("no tool is called: {name}")
    }
    fn list_tools(&self) -> Vec<opencrab_llm_types::FunctionDefinition> {
        Vec::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Observed {
    Begin(String),
    Text(String, String),
    Say(String),
}

fn delta(text: &str) -> OutputTextEvent {
    OutputTextEvent::Delta(text.to_string())
}

fn engine_with_log(
    llm: &Arc<ObservedScriptLlm>,
    with_partial_hook: bool,
) -> (SkillEngine, Arc<std::sync::Mutex<Vec<Observed>>>) {
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut engine = SkillEngine::new(Box::new(SharedLlm(Arc::clone(llm))), Box::new(NoTools), 4);
    engine.set_on_continuation_speech({
        let log = Arc::clone(&log);
        Arc::new(move |speech| {
            log.lock().unwrap().push(Observed::Say(speech));
            Box::pin(async { Ok(()) })
        })
    });
    if with_partial_hook {
        let log = Arc::clone(&log);
        engine.set_on_partial_text(Arc::new(move |event| {
            log.lock().unwrap().push(match event {
                PartialTextEvent::Begin { stream_id } => Observed::Begin(stream_id),
                PartialTextEvent::Text { stream_id, text } => Observed::Text(stream_id, text),
            });
            Box::pin(async {})
        }));
    }
    (engine, log)
}

/// C8: one stream per LLM call; each call's partials precede that call's say.
#[tokio::test]
async fn engine_streams_each_call_before_its_say() {
    let llm = ObservedScriptLlm::new(vec![
        (vec![delta("こんにちは。")], "こんにちは。"),
        (
            vec![delta("またね。\nNO_"), delta("REPLY")],
            "またね。\nNO_REPLY",
        ),
    ]);
    let (engine, log) = engine_with_log(&llm, true);

    let result = engine.run("system", "hi", "model").await.unwrap();

    assert_eq!(result.response, "またね。");
    let log = log.lock().unwrap().clone();
    let [Observed::Begin(first), Observed::Text(first_text_id, first_text), Observed::Say(say), Observed::Begin(second), Observed::Text(second_text_id, second_text)] =
        log.as_slice()
    else {
        panic!("unexpected partial/say order: {log:?}");
    };
    assert_ne!(first, second, "every LLM call gets a fresh stream id");
    assert_eq!(first_text_id, first);
    assert_eq!(second_text_id, second);
    assert_eq!(first_text, "こんにちは。");
    assert_eq!(say, "こんにちは。");
    assert_eq!(second_text, "またね。\n");
    assert_eq!(
        llm.observed_calls.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
}

/// C8: a provider retry (Reset) starts a fresh stream id within the same call.
#[tokio::test]
async fn engine_starts_a_new_stream_on_reset() {
    let llm = ObservedScriptLlm::new(vec![(
        vec![delta("一回目。"), OutputTextEvent::Reset, delta("二回目。")],
        "二回目。\nNO_REPLY",
    )]);
    let (engine, log) = engine_with_log(&llm, true);

    engine.run("system", "hi", "model").await.unwrap();

    let log = log.lock().unwrap().clone();
    let [Observed::Begin(first), Observed::Text(a, first_text), Observed::Begin(second), Observed::Text(b, second_text)] =
        log.as_slice()
    else {
        panic!("unexpected events: {log:?}");
    };
    assert_ne!(first, second);
    assert_eq!((a, first_text.as_str()), (first, "一回目。"));
    assert_eq!((b, second_text.as_str()), (second, "二回目。"));
}

/// C8 GREEN: without the hook the engine uses the unobserved call and emits no partials.
#[tokio::test]
async fn engine_without_partial_hook_keeps_the_unobserved_call() {
    let llm = ObservedScriptLlm::new(vec![
        (vec![delta("こんにちは。")], "こんにちは。"),
        (vec![delta("またね。")], "またね。\nNO_REPLY"),
    ]);
    let (engine, log) = engine_with_log(&llm, false);

    let result = engine.run("system", "hi", "model").await.unwrap();

    assert_eq!(result.response, "またね。");
    assert_eq!(
        *log.lock().unwrap(),
        vec![Observed::Say("こんにちは。".into())]
    );
    assert_eq!(
        llm.unobserved_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(
        llm.observed_calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}
