#[test]
fn web_search_call_and_citation_are_captured_without_becoming_tool_calls() {
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{",
        "\"type\":\"web_search_call\",\"id\":\"ws_1\",\"status\":\"completed\",",
        "\"action\":{\"type\":\"search\",\"query\":\"Hokkaido weather\",\"ignored\":\"raw\"}}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-ws\",",
        "\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",",
        "\"annotations\":[{\"type\":\"url_citation\",\"url\":\"https://example.test/weather\",",
        "\"title\":\"Weather\"}]}]}],",
        "\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n\n",
    );

    let exchange = provider
        .parse_exchange(sse, "gpt-5.6-sol", true)
        .expect("parse exchange");
    assert_eq!(
        exchange.provider_tool_history.state,
        opencrab_llm_types::ProviderToolHistoryState::Captured
    );
    assert_eq!(exchange.provider_tool_history.calls.len(), 1);
    assert_eq!(exchange.provider_tool_history.calls[0].id, "ws_1");
    assert_eq!(
        exchange.provider_tool_history.calls[0].action["query"],
        "Hokkaido weather"
    );
    assert!(exchange.provider_tool_history.calls[0]
        .action
        .get("ignored")
        .is_none());
    assert_eq!(exchange.provider_tool_history.citations.len(), 1);
    assert!(exchange.response.choices[0]
        .message
        .tool_calls
        .as_ref()
        .is_none_or(Vec::is_empty));
}

#[test]
fn requested_search_without_events_is_not_used() {
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-none\",",
        "\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n",
    );
    let exchange = provider.parse_exchange(sse, "gpt-5.6-sol", true).unwrap();
    assert_eq!(
        exchange.provider_tool_history.state,
        opencrab_llm_types::ProviderToolHistoryState::NotUsed
    );
}

#[test]
fn open_page_action_does_not_require_query() {
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "data: {\"type\":\"response.output_item.done\",\"item\":{",
        "\"type\":\"web_search_call\",\"id\":\"ws_open\",\"status\":\"completed\",",
        "\"action\":{\"type\":\"open_page\",\"url\":\"https://example.test\"}}}\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-open\",",
        "\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n",
    );
    let exchange = provider.parse_exchange(sse, "gpt-5.6-sol", true).unwrap();
    assert_eq!(
        exchange.provider_tool_history.state,
        opencrab_llm_types::ProviderToolHistoryState::Captured
    );
}

#[test]
fn lifecycle_without_final_search_item_is_incomplete() {
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "data: {\"type\":\"response.web_search_call.in_progress\",\"item_id\":\"ws_life\"}\n",
        "data: {\"type\":\"response.web_search_call.searching\",\"item_id\":\"ws_life\"}\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-life\",",
        "\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n",
    );
    let exchange = provider.parse_exchange(sse, "gpt-5.6-sol", true).unwrap();
    assert_eq!(
        exchange.provider_tool_history.state,
        opencrab_llm_types::ProviderToolHistoryState::Incomplete
    );
}

#[test]
fn malformed_search_event_is_incomplete() {
    let provider = ChatGptProvider::new();
    let sse = concat!(
        "data: {not json}\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-bad\",",
        "\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n",
    );
    let exchange = provider.parse_exchange(sse, "gpt-5.6-sol", true).unwrap();
    assert_eq!(
        exchange.provider_tool_history.state,
        opencrab_llm_types::ProviderToolHistoryState::Incomplete
    );
}
