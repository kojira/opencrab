use super::*;

#[test]
fn test_build_prompt() {
    let provider = CodexProvider::new();
    let request = ChatRequest {
        model: "o4-mini".to_string(),
        messages: vec![
            Message {
                role: Role::System,
                content: Some(MessageContent::Text("You are helpful.".to_string())),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: Some(MessageContent::Text("Hello".to_string())),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ],
        functions: None,
        function_call: None,
        temperature: None,
        max_tokens: None,
        stop: None,
        stream: None,
        metadata: Default::default(),
        agent_id: None,
        reasoning_effort: None,
    };

    let prompt = provider.build_prompt(&request);
    assert!(prompt.contains("[System]\nYou are helpful."));
    assert!(prompt.contains("[User]\nHello"));
}

/// 回帰: 画像添付（マルチパート）でも本文が消えないこと。以前は
/// text_content() が Multi に対し None を返すため、画像を貼ると発言が
/// 丸ごと落ちて「何も届かない」状態になっていた。
#[test]
fn test_build_prompt_multipart_preserves_text_and_notes_image() {
    let request = ChatRequest {
        model: "gpt-5.6-sol".to_string(),
        messages: vec![Message {
            role: Role::User,
            content: Some(MessageContent::Multi(vec![
                ContentPart::Text {
                    text: "この画像を見て".to_string(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "https://cdn.discordapp.com/x.png".to_string(),
                        detail: None,
                    },
                },
            ])),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        functions: None,
        function_call: None,
        temperature: None,
        max_tokens: None,
        stop: None,
        stream: None,
        metadata: Default::default(),
        agent_id: None,
        reasoning_effort: None,
    };

    let prompt = build_cli_prompt(&request);
    // 本文は残る（以前は空になっていた）。
    assert!(prompt.contains("この画像を見て"), "text dropped: {prompt}");
    // 画像がある事実は注記される（モデルが状況を把握できる）。
    assert!(prompt.contains("画像"), "image note missing: {prompt}");
    assert!(prompt.contains("[User]"), "user turn missing: {prompt}");
}

#[test]
fn test_build_prompt_injects_tool_definitions() {
    let provider = CodexProvider::new();
    let request = ChatRequest {
        model: "o4-mini".to_string(),
        messages: vec![Message::user("Do the thing")],
        functions: Some(vec![FunctionDefinition {
            name: "spawn_subtask".to_string(),
            description: Some("Launch a subtask asynchronously".to_string()),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"prompt": {"type": "string"}}
            }),
        }]),
        function_call: None,
        temperature: None,
        max_tokens: None,
        stop: None,
        stream: None,
        metadata: Default::default(),
        agent_id: None,
        reasoning_effort: None,
    };

    let prompt = provider.build_prompt(&request);
    assert!(prompt.contains("[Available Tools]"));
    assert!(prompt.contains("<tool name=\"spawn_subtask\">"));
    assert!(prompt.contains("<description>Launch a subtask asynchronously</description>"));
    assert!(prompt.contains("\"prompt\""));
    assert!(prompt.contains("<function_calls>"));
    assert!(prompt.contains("[User]\nDo the thing"));
}

#[test]
fn test_build_prompt_renders_assistant_tool_calls() {
    let provider = CodexProvider::new();
    let request = ChatRequest {
        model: "o4-mini".to_string(),
        messages: vec![
            Message {
                role: Role::Assistant,
                content: Some(MessageContent::Text("Calling a tool now.".to_string())),
                name: None,
                function_call: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "send_message".to_string(),
                        arguments: r#"{"text":"hi","count":3}"#.to_string(),
                    },
                }]),
                tool_call_id: None,
            },
            Message {
                role: Role::Tool,
                content: Some(MessageContent::Text("done".to_string())),
                name: Some("send_message".to_string()),
                function_call: None,
                tool_calls: None,
                tool_call_id: Some("call_1".to_string()),
            },
        ],
        functions: None,
        function_call: None,
        temperature: None,
        max_tokens: None,
        stop: None,
        stream: None,
        metadata: Default::default(),
        agent_id: None,
        reasoning_effort: None,
    };

    let prompt = provider.build_prompt(&request);
    // Assistant text and tool calls both rendered under [Assistant].
    assert!(prompt.contains("[Assistant]\nCalling a tool now."));
    assert!(prompt.contains("<invoke name=\"send_message\">"));
    assert!(prompt.contains("<text>hi</text>"));
    // Non-string JSON values are serialized without quotes.
    assert!(prompt.contains("<count>3</count>"));
    // Tool result identifies the originating call.
    assert!(prompt.contains("[Tool Result: send_message (call_id=call_1)]\ndone"));
}

#[test]
fn test_default_values() {
    let provider = CodexProvider::new();
    assert_eq!(provider.codex_path, "codex");
    assert_eq!(provider.default_model, "o4-mini");
    assert_eq!(provider.sandbox, "read-only");
    assert!(provider.working_dir.is_none());
    assert_eq!(provider.timeout, Duration::from_secs(300));
}

#[test]
fn test_builder_methods() {
    let provider = CodexProvider::new()
        .with_codex_path("/usr/local/bin/codex")
        .with_default_model("o3")
        .with_sandbox("workspace-write")
        .with_working_dir("/home/user/project")
        .with_timeout_secs(600);

    assert_eq!(provider.codex_path, "/usr/local/bin/codex");
    assert_eq!(provider.default_model, "o3");
    assert_eq!(provider.sandbox, "workspace-write");
    assert_eq!(provider.working_dir.as_deref(), Some("/home/user/project"));
    assert_eq!(provider.timeout, Duration::from_secs(600));
}

#[test]
fn test_reasoning_effort_builder() {
    assert!(CodexProvider::new().reasoning_effort.is_none());
    assert!(CodexProvider::new()
        .with_reasoning_effort("")
        .reasoning_effort
        .is_none());
    assert_eq!(
        CodexProvider::new()
            .with_reasoning_effort("medium")
            .reasoning_effort
            .as_deref(),
        Some("medium")
    );
}

#[test]
fn test_resolve_codex_output_nonzero_exit_keeps_response() {
    // 非ゼロ終了でも本文があれば捨てずに使う（gpt-5.6-sol の exit 1 問題）
    let out = resolve_codex_output(
        "exit status: 1",
        false,
        Some("Hi! 👋".to_string()),
        b"",
        b"some agentic warning",
    )
    .expect("non-empty output must be kept even on non-zero exit");
    assert_eq!(out, "Hi! 👋");
}

#[test]
fn test_resolve_codex_output_nonzero_empty_is_error_with_stderr() {
    // 非ゼロ終了 かつ 本文空 は失敗。stderr を握りつぶさないこと。
    let err = resolve_codex_output(
        "exit status: 1",
        false,
        Some("  ".to_string()),
        b"",
        b"boom: real reason",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("boom: real reason"), "{err}");
    assert!(err.contains("exit status: 1"), "{err}");
}

#[test]
fn test_resolve_codex_output_success_uses_file() {
    let out =
        resolve_codex_output("exit status: 0", true, Some("answer".to_string()), b"", b"").unwrap();
    assert_eq!(out, "answer");
}

#[test]
fn test_resolve_codex_output_no_file_falls_back_to_stdout() {
    // -o ファイルが無い場合は stdout を使う（正常終了）
    let out = resolve_codex_output("exit status: 0", true, None, b"stdout answer", b"").unwrap();
    assert_eq!(out, "stdout answer");
    // 非ゼロ終了 かつ ファイル無し は失敗（stderr+stdout 付き）
    let err = resolve_codex_output("exit status: 1", false, None, b"partial", b"why it died")
        .unwrap_err()
        .to_string();
    assert!(err.contains("why it died"), "{err}");
}

#[test]
fn test_extra_models() {
    let provider = CodexProvider::new().with_extra_models(vec![("gpt-5".to_string(), 128_000)]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let models = rt.block_on(provider.available_models()).unwrap();
    assert!(models.iter().any(|m| m.id == "gpt-5"));
    assert!(models.iter().any(|m| m.id == "o4-mini"));
}

#[test]
fn test_gpt56_in_default_models() {
    let provider = CodexProvider::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let models = rt.block_on(provider.available_models()).unwrap();
    // Codex サブスクでも GPT-5.6 系が選択肢に出ること
    for id in ["gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
        assert!(models.iter().any(|m| m.id == id), "{id} が候補に無い");
    }
}

#[test]
fn test_parse_jsonl_event_agent_message() {
    let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"Hello world"}}"#;
    let delta = parse_jsonl_event(line, "o4-mini").unwrap();
    assert_eq!(
        delta.choices[0].delta.content.as_deref(),
        Some("Hello world")
    );
}

#[test]
fn test_parse_jsonl_event_turn_completed() {
    let line = r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":50,"output_tokens":20}}"#;
    let delta = parse_jsonl_event(line, "o4-mini").unwrap();
    assert_eq!(delta.choices[0].finish_reason, Some(FinishReason::Stop));
    assert!(delta.choices[0].delta.content.is_none());
}

#[test]
fn test_parse_jsonl_event_irrelevant() {
    let line = r#"{"type":"thread.started","thread_id":"abc"}"#;
    assert!(parse_jsonl_event(line, "o4-mini").is_none());
}

#[test]
fn test_parse_usage_from_stdout() {
    let stdout = br#"{"type":"thread.started","thread_id":"abc"}
{"type":"turn.started"}
{"type":"turn.completed","usage":{"input_tokens":500,"cached_input_tokens":400,"output_tokens":50}}
"#;
    let usage = parse_usage_from_stdout(stdout);
    assert_eq!(usage.prompt_tokens, 500);
    assert_eq!(usage.completion_tokens, 50);
    assert_eq!(usage.total_tokens, 550);
    assert_eq!(usage.cache_read_input_tokens, 400);
}

/// #148 regression: without `--json`, codex emits no `turn.completed` events,
/// so usage parsing yields all zeros. This documents the exact pre-fix symptom
/// and pairs with the positive test above / the arg-contract test below.
#[test]
fn test_parse_usage_from_stdout_without_json_is_zero() {
    let stdout = b"just the final assistant text, no JSONL events here\n";
    let usage = parse_usage_from_stdout(stdout);
    assert_eq!(usage.prompt_tokens, 0);
    assert_eq!(usage.completion_tokens, 0);
    assert_eq!(usage.total_tokens, 0);
    assert_eq!(usage.cache_read_input_tokens, 0);
}

/// #148 regression: the non-streaming chat_completion invocation must pass
/// `--json` (before `-o`) so usage can be accounted. Guards against the flag
/// being dropped, which would silently zero out codex token accounting.
#[test]
fn test_nonstreaming_command_includes_json_flag() {
    let mut cmd = Command::new("codex");
    append_nonstreaming_output_args(&mut cmd, "/tmp/out.txt");
    let args: Vec<String> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        args.iter().any(|a| a == "--json"),
        "non-streaming codex command must include --json for usage accounting (#148): {args:?}"
    );
    let json_pos = args.iter().position(|a| a == "--json").unwrap();
    let o_pos = args.iter().position(|a| a == "-o").unwrap();
    assert!(json_pos < o_pos, "--json must precede -o: {args:?}");
    // -o still writes the final message file; prompt still comes from stdin (`-`).
    assert_eq!(
        args.get(o_pos + 1).map(String::as_str),
        Some("/tmp/out.txt")
    );
    assert!(
        args.iter().any(|a| a == "-"),
        "prompt must be read from stdin"
    );
}
