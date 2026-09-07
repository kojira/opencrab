#[test]
fn test_parse_response_tool_calls_from_completed_output() {
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",",
        "\"output\":[{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",",
        "\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"Tokyo\\\"}\"}],",
        "\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n",
        "\n",
    );
    let resp = provider
        .parse_response(sse, "gpt-5.5")
        .expect("parse failed");
    assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::ToolCalls));
    let calls = resp.choices[0]
        .message
        .tool_calls
        .as_ref()
        .expect("tool calls must be parsed");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].call_type, "function");
    assert_eq!(calls[0].function.name, "get_weather");
    assert_eq!(calls[0].function.arguments, r#"{"city":"Tokyo"}"#);
    assert_eq!(resp.usage.completion_tokens, 5);
    assert!(resp.choices[0].message.content.is_none());
}

#[test]
fn test_parse_response_tool_calls_from_output_item_done() {
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",",
        "\"id\":\"fc_2\",\"call_id\":\"call_2\",\"name\":\"search\",",
        "\"arguments\":\"{\\\"query\\\":\\\"opencrab\\\"}\"}}\n",
        "\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-2\",",
        "\"output\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":4,\"total_tokens\":11}}}\n",
        "\n",
    );

    let resp = provider
        .parse_response(sse, "gpt-5.5")
        .expect("parse failed");
    assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::ToolCalls));
    let calls = resp.choices[0]
        .message
        .tool_calls
        .as_ref()
        .expect("tool calls must be parsed");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_2");
    assert_eq!(calls[0].function.name, "search");
    assert_eq!(calls[0].function.arguments, r#"{"query":"opencrab"}"#);
}

/// #502: cached_tokens が usage.input_tokens_details.cached_tokens にあるとき
/// cache_read_input_tokens に反映されること。
#[test]
fn test_parse_response_reads_cached_tokens() {
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-c\",",
        "\"output\":[],\"usage\":{\"input_tokens\":100,\"output_tokens\":20,",
        "\"total_tokens\":120,\"input_tokens_details\":{\"cached_tokens\":80}}}}\n",
        "\n",
    );
    let resp = provider
        .parse_response(sse, "gpt-5.5")
        .expect("parse failed");
    assert_eq!(resp.usage.prompt_tokens, 100);
    assert_eq!(resp.usage.completion_tokens, 20);
    assert_eq!(resp.usage.cache_read_input_tokens, 80);
    // OpenAI はキャッシュ書き込みを別課金しないため常に 0。
    assert_eq!(resp.usage.cache_creation_input_tokens, 0);
}

/// #676（方針3）: Responses API が出力上限で応答を打ち切ったとき（status=="incomplete"
/// かつ incomplete_details.reason=="max_output_tokens"）、finish_reason=Length にする。
/// 前置きテキストが出ていても Stop に倒さない（エンジンがこの Length を見て切り捨てを
/// 失敗させる。chatgpt は cap を送らないので、これが in-use の gpt-5.6 系を守る防衛線）。
#[test]
fn test_parse_response_incomplete_max_output_tokens_is_length() {
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"これから報告を書\"}\n",
        "\n",
        "event: response.incomplete\n",
        "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp-inc\",",
        "\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},",
        "\"output\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":4096,",
        "\"total_tokens\":4106}}}\n",
        "\n",
    );
    let resp = provider
        .parse_response(sse, "gpt-5.6-sol")
        .expect("parse failed");
    assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::Length));
    assert_eq!(resp.usage.completion_tokens, 4096);
}

/// #676 回帰防止: status=="completed"（打ち切りなし）は従来どおり Stop のまま。
#[test]
fn test_parse_response_completed_status_stays_stop() {
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"完了\"}\n",
        "\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-ok\",",
        "\"status\":\"completed\",\"output\":[],",
        "\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"total_tokens\":12}}}\n",
        "\n",
    );
    let resp = provider
        .parse_response(sse, "gpt-5.6-sol")
        .expect("parse failed");
    assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::Stop));
}

/// #502: input_tokens_details / cached_tokens が欠落しても panic せず 0 に倒れること。
#[test]
fn test_parse_response_missing_cached_tokens_defaults_to_zero() {
    let provider = ChatGptProvider::new();
    // details ごと欠落。
    let sse_missing = concat!(
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-m\",",
        "\"output\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":4,\"total_tokens\":11}}}\n",
        "\n",
    );
    let resp = provider
        .parse_response(sse_missing, "gpt-5.5")
        .expect("parse failed");
    assert_eq!(resp.usage.prompt_tokens, 7);
    assert_eq!(resp.usage.cache_read_input_tokens, 0);

    // cached_tokens が null のケース。
    let sse_null = concat!(
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-n\",",
        "\"output\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":4,\"total_tokens\":11,",
        "\"input_tokens_details\":{\"cached_tokens\":null}}}}\n",
        "\n",
    );
    let resp_null = provider
        .parse_response(sse_null, "gpt-5.5")
        .expect("parse failed");
    assert_eq!(resp_null.usage.cache_read_input_tokens, 0);
}

// ── parse_response delta text extraction ─────────────────────────────────

#[test]
fn test_parse_response_text_delta_single() {
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello, world!\"}\n",
        "\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"output\":[],",
        "\"usage\":{\"input_tokens\":5,\"output_tokens\":3,\"total_tokens\":8}}}\n",
        "\n",
    );
    let resp = provider
        .parse_response(sse, "gpt-5.5")
        .expect("parse failed");
    let text = match &resp.choices[0].message.content {
        Some(MessageContent::Text(t)) => t.as_str(),
        other => panic!("expected Text content, got: {other:?}"),
    };
    assert_eq!(text, "Hello, world!");
    assert_eq!(resp.usage.completion_tokens, 3);
}

#[test]
fn test_parse_response_text_delta_multiple() {
    let provider = ChatGptProvider::new();
    // Multiple delta chunks must be concatenated in order.
    let sse = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Foo\"}\n",
        "\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\" \"}\n",
        "\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Bar\"}\n",
        "\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\",\"output\":[],",
        "\"usage\":{\"input_tokens\":2,\"output_tokens\":2,\"total_tokens\":4}}}\n",
        "\n",
    );
    let resp = provider
        .parse_response(sse, "gpt-5.5")
        .expect("parse failed");
    let text = match &resp.choices[0].message.content {
        Some(MessageContent::Text(t)) => t.as_str(),
        other => panic!("expected Text content, got: {other:?}"),
    };
    assert_eq!(text, "Foo Bar");
}

/// #844: verbosity=medium で 1 応答に message アイテムが複数出て
/// 「本文A → NO_REPLY → 本文A」のロールプレイ軌跡形になる実測ケース。
/// 無区切り連結（"本文ANO_REPLY本文A"）せず、非センチネルの先頭アイテム "本文A" だけを採用する。
#[test]
fn test_parse_response_multi_message_items_skips_no_reply_sentinel() {
    let provider = ChatGptProvider::new();
    let sse = concat!(
        // item1: 本文A
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n",
        "\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"本文A\"}\n",
        "\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n",
        "\n",
        // item2: NO_REPLY（センチネル）
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n",
        "\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"NO_REPLY\"}\n",
        "\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n",
        "\n",
        // item3: 本文A（逐語再掲）
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n",
        "\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"本文A\"}\n",
        "\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n",
        "\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r-844\",\"output\":[],",
        "\"usage\":{\"input_tokens\":5,\"output_tokens\":6,\"total_tokens\":11}}}\n",
        "\n",
    );
    let resp = provider
        .parse_response(sse, "gpt-5.5")
        .expect("parse failed");
    let text = match &resp.choices[0].message.content {
        Some(MessageContent::Text(t)) => t.as_str(),
        other => panic!("expected Text content, got: {other:?}"),
    };
    assert_eq!(
        text, "本文A",
        "非センチネルの先頭アイテムだけを採用せず、NO_REPLY を連結して露出させている"
    );
}

/// #844: 全アイテムがセンチネルなら NO_REPLY を残す（下流の全文一致で沈黙判定できるように）。
#[test]
fn test_parse_response_all_items_no_reply_preserves_sentinel() {
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n",
        "\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"NO_REPLY\"}\n",
        "\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n",
        "\n",
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n",
        "\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"NO_REPLY\"}\n",
        "\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n",
        "\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r-844b\",\"output\":[],",
        "\"usage\":{\"input_tokens\":3,\"output_tokens\":2,\"total_tokens\":5}}}\n",
        "\n",
    );
    let resp = provider
        .parse_response(sse, "gpt-5.5")
        .expect("parse failed");
    let text = match &resp.choices[0].message.content {
        Some(MessageContent::Text(t)) => t.as_str(),
        other => panic!("expected Text content, got: {other:?}"),
    };
    assert_eq!(text.trim(), "NO_REPLY");
}

/// #844: センチネルが先頭でも、後続の実体アイテムを採用する（NO_REPLY → 本文B）。
#[test]
fn test_parse_response_sentinel_first_picks_later_real_item() {
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n",
        "\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"NO_REPLY\"}\n",
        "\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n",
        "\n",
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n",
        "\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"本文B\"}\n",
        "\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n",
        "\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r-844c\",\"output\":[],",
        "\"usage\":{\"input_tokens\":3,\"output_tokens\":3,\"total_tokens\":6}}}\n",
        "\n",
    );
    let resp = provider
        .parse_response(sse, "gpt-5.5")
        .expect("parse failed");
    let text = match &resp.choices[0].message.content {
        Some(MessageContent::Text(t)) => t.as_str(),
        other => panic!("expected Text content, got: {other:?}"),
    };
    assert_eq!(text, "本文B");
}

#[test]
fn test_parse_response_empty_no_output() {
    // A response with no delta events and no tool calls → empty content is fine.
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r3\",\"output\":[],",
        "\"usage\":{\"input_tokens\":1,\"output_tokens\":0,\"total_tokens\":1}}}\n",
        "\n",
    );
    let resp = provider
        .parse_response(sse, "gpt-5.5")
        .expect("parse failed");
    assert_eq!(resp.usage.completion_tokens, 0);
    assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::Stop));
}

#[test]
fn test_parse_response_unicode_delta() {
    // Multibyte characters must be handled correctly.
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"こんにちは\"}\n",
        "\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r4\",\"output\":[],",
        "\"usage\":{\"input_tokens\":1,\"output_tokens\":5,\"total_tokens\":6}}}\n",
        "\n",
    );
    let resp = provider
        .parse_response(sse, "gpt-5.5")
        .expect("parse failed");
    let text = match &resp.choices[0].message.content {
        Some(MessageContent::Text(t)) => t.as_str(),
        other => panic!("expected Text content, got: {other:?}"),
    };
    assert_eq!(text, "こんにちは");
}
