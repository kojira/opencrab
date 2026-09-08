use super::*;

impl ChatGptProvider {
    pub(super) fn request_builder(
        &self,
        endpoint: &str,
        token: &str,
        account_id: &str,
    ) -> reqwest::RequestBuilder {
        let url = format!("{}/{}", self.base_url, endpoint);
        self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("chatgpt-account-id", account_id)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "pi")
            .header("accept", "text/event-stream")
            .header("Content-Type", "application/json")
    }
    /// Convert a message's content into the Responses API content value.
    ///
    /// Responses API のマルチモーダル型は `input_text` / `input_image` で、`image_url` は
    /// **文字列**（URL か data URI）である点に注意（Chat Completions の
    /// `{"type":"image_url","image_url":{"url":...}}` とは別物）。以前は Chat
    /// Completions 形式を送っており、codex/responses バックエンドでは画像が無視/拒否
    /// されていた。テキスト単体は文字列コンテンツとして送れるため従来どおり。
    pub(super) fn message_content_value(content: &Option<MessageContent>) -> Option<Value> {
        match content {
            Some(MessageContent::Text(text)) => Some(serde_json::json!(text)),
            Some(MessageContent::Image { image_url, .. }) => {
                Some(serde_json::json!([Self::input_image_part(image_url),]))
            }
            Some(MessageContent::Multi(parts)) => {
                let parts_json: Vec<Value> = parts
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text { text } => {
                            serde_json::json!({"type": "input_text", "text": text})
                        }
                        ContentPart::ImageUrl { image_url } => Self::input_image_part(image_url),
                    })
                    .collect();
                Some(serde_json::json!(parts_json))
            }
            None => None,
        }
    }

    /// Responses API の `input_image` パートを組む。`image_url` は文字列（URL / data URI）。
    fn input_image_part(image_url: &ImageUrl) -> Value {
        let mut part = serde_json::json!({
            "type": "input_image",
            "image_url": image_url.url,
        });
        if let Some(detail) = &image_url.detail {
            part["detail"] = serde_json::json!(detail);
        }
        part
    }

    /// Build the request body in the Responses API format.
    pub(super) fn build_request_body(&self, request: &ChatRequest, stream: bool) -> Value {
        let mut system_prompts: Vec<String> = Vec::new();
        tracing::warn!(
            "chatgpt build_request_body: messages count={}, system_prompts will be extracted",
            request.messages.len()
        );
        let mut input: Vec<Value> = Vec::new();

        tracing::debug!(
            message_count = request.messages.len(),
            "build_request_body: received messages"
        );
        for msg in &request.messages {
            tracing::debug!(role = ?msg.role, "build_request_body: message role");
        }

        for msg in &request.messages {
            if msg.role == Role::System {
                tracing::debug!(
                    role = "system",
                    content_is_some = msg.content.is_some(),
                    "build_request_body: processing system message"
                );
                if let Some(MessageContent::Text(text)) = &msg.content {
                    tracing::debug!(
                        text_len = text.len(),
                        "build_request_body: system message is Text, adding to system_prompts"
                    );
                    system_prompts.push(text.clone());
                } else if let Some(content) = Self::message_content_value(&msg.content) {
                    if let Some(s) = content.as_str() {
                        tracing::debug!(
                            str_len = s.len(),
                            "build_request_body: system message content converted to str via message_content_value"
                        );
                        system_prompts.push(s.to_string());
                    } else {
                        tracing::warn!(
                            content_type = ?&msg.content,
                            "build_request_body: system message content is not a string after message_content_value conversion, SKIPPING"
                        );
                    }
                } else {
                    tracing::warn!(
                        content_is_none = msg.content.is_none(),
                        "build_request_body: system message content is None or could not be converted, SKIPPING"
                    );
                }
                continue;
            }

            if msg.role == Role::Assistant {
                if let Some(tool_calls) = &msg.tool_calls {
                    if !tool_calls.is_empty() {
                        // assistant がツールコールと同時にテキストを返した場合、そのテキストも
                        // 履歴に残す（以前は continue で本文が欠落していた）。
                        // 空テキストは追加しない。
                        let has_text = msg.text_content().is_some_and(|t| !t.is_empty());
                        if has_text {
                            if let Some(content) = Self::message_content_value(&msg.content) {
                                input.push(serde_json::json!({
                                    "role": "assistant",
                                    "content": content,
                                }));
                            }
                        }
                        for tool_call in tool_calls {
                            input.push(serde_json::json!({
                                "type": "function_call",
                                "call_id": tool_call.id,
                                "name": tool_call.function.name,
                                "arguments": tool_call.function.arguments,
                            }));
                        }
                        continue;
                    }
                    // tool_calls が空 (Some(vec![])) の場合は通常の assistant メッセージ
                    // として下の共通処理へフォールスルーする（メッセージ全体の消失を防ぐ）。
                }
            }

            if msg.role == Role::Tool {
                if let Some(tool_call_id) = &msg.tool_call_id {
                    let output = msg.text_content().unwrap_or_default();
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": tool_call_id,
                        "output": output,
                    }));
                    continue;
                }
            }

            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "user",
            };
            let mut m = serde_json::json!({"role": role});
            if let Some(content) = Self::message_content_value(&msg.content) {
                m["content"] = content;
            }
            // #892: Responses API は input item の `name` を受け付けない
            // （400 Unknown parameter: 'input[N].name'）。話者は本文へ埋め込む方針のため
            // Message.name は wire に出さない（防御）。
            input.push(m);
        }

        let mut body = serde_json::json!({
            "model": request.model,
            "store": false,
            "stream": stream,
            "input": input,
            "text": {"verbosity": "medium"},
            "tool_choice": "auto",
            "parallel_tool_calls": true,
        });

        // per-request（エージェント個別）を優先し、無ければ構築時の既定。
        if let Some(value) = request
            .reasoning_effort
            .as_deref()
            .or(self.reasoning_effort.as_deref())
        {
            body["reasoning"] = serde_json::json!({"effort": value});
        }

        // NOTE: max_output_tokens is NOT supported by the chatgpt Responses API
        // (returns 400 "Unsupported parameter: max_output_tokens") — never sent.

        if self.include_encrypted_content {
            body["include"] = serde_json::json!(["reasoning.encrypted_content"]);
        }

        tracing::warn!(
            "chatgpt build_request_body: system_prompts count={}",
            system_prompts.len()
        );
        if system_prompts.is_empty() {
            tracing::warn!(
                total_messages = request.messages.len(),
                "build_request_body: system_prompts is EMPTY! instructions field will NOT be set -> API will return 400 Bad Request"
            );
        }

        if !system_prompts.is_empty() {
            body["instructions"] = serde_json::json!(system_prompts.join("\n\n"));
        }

        let mut tools: Vec<Value> = Vec::new();
        if let Some(ref functions) = request.functions {
            tools.extend(functions.iter().map(|f| {
                serde_json::json!({
                    "type": "function",
                    "name": f.name,
                    "description": f.description,
                    "parameters": f.parameters,
                })
            }));
        }
        // 本文URL読取り（エージェント単位オプトイン）: native web_search を有効化。
        // codex CLI が同じ codex/responses バックエンドへ送るのと同じツール形
        // （external_web_access=true で live 取得、text+image 対応）。モデルが
        // search / open_page アクションでリンク先を読める。
        if request
            .metadata
            .get("web_search")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            tools.push(serde_json::json!({
                "type": "web_search",
                "external_web_access": true,
                "search_content_types": ["text", "image"],
            }));
        }
        if !tools.is_empty() {
            body["tools"] = serde_json::json!(tools);
        }

        if let Some(ref fc) = request.function_call {
            match fc {
                FunctionCallBehavior::Mode(mode) => {
                    body["tool_choice"] = serde_json::json!(mode);
                }
                FunctionCallBehavior::Named { name } => {
                    body["tool_choice"] = serde_json::json!({"type": "function", "name": name});
                }
            }
        }

        debug!(
            model = %request.model,
            stream = stream,
            input_count = input.len(),
            system_prompt_count = system_prompts.len(),
            has_tools = request.functions.is_some(),
            body = %body,
            "chatgpt build_request_body"
        );

        body
    }
}
