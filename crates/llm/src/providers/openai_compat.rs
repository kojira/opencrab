//! OpenAI 互換 API のリクエスト組立 / レスポンスパースの共通実装（#38）。
//!
//! openai / openrouter / llamacpp は同じ wire 形状（`choices[]`・`data:` SSE）を
//! 使うのに、変換・パースをコピペで重複させていた。パース修正（新 finish_reason、
//! ストリーミング tool-call 等）が N 箇所に散らばりドリフトするため、ここに一本化する。
//! transport（URL・認証ヘッダ・リトライ）は各プロバイダの責務のまま。

use serde_json::Value;

use crate::message::*;

/// 統一 Message を OpenAI 互換のメッセージ JSON に変換する。
///
/// 注意: `cache_control` は Anthropic 固有のフィールドで、OpenAI 互換 API は
/// 未知のパラメータとして 400 で拒否するため出力しない。
pub fn message_to_json(msg: &Message) -> Value {
    let mut obj = serde_json::json!({
        "role": msg.role,
    });

    if let Some(ref content) = msg.content {
        match content {
            MessageContent::Text(text) => {
                obj["content"] = serde_json::json!(text);
            }
            MessageContent::Image { image_url, .. } => {
                obj["content"] = serde_json::json!([
                    {
                        "type": "image_url",
                        "image_url": { "url": image_url.url }
                    }
                ]);
            }
            MessageContent::Multi(parts) => {
                let parts_json: Vec<Value> = parts
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text { text } => {
                            serde_json::json!({"type": "text", "text": text})
                        }
                        ContentPart::ImageUrl { image_url } => {
                            serde_json::json!({
                                "type": "image_url",
                                "image_url": {"url": image_url.url}
                            })
                        }
                    })
                    .collect();
                obj["content"] = serde_json::json!(parts_json);
            }
        }
    }

    if let Some(ref name) = msg.name {
        obj["name"] = serde_json::json!(name);
    }
    if let Some(ref tool_calls) = msg.tool_calls {
        obj["tool_calls"] = serde_json::to_value(tool_calls).unwrap_or_default();
    }
    if let Some(ref tool_call_id) = msg.tool_call_id {
        obj["tool_call_id"] = serde_json::json!(tool_call_id);
    }

    obj
}

pub fn messages_to_json(messages: &[Message]) -> Vec<Value> {
    messages.iter().map(message_to_json).collect()
}

fn parse_finish_reason(fr: &Value) -> Option<FinishReason> {
    match fr.as_str()? {
        "stop" => Some(FinishReason::Stop),
        "length" => Some(FinishReason::Length),
        "function_call" => Some(FinishReason::FunctionCall),
        "tool_calls" => Some(FinishReason::ToolCalls),
        "content_filter" => Some(FinishReason::ContentFilter),
        _ => None,
    }
}

/// OpenAI 互換の chat completion レスポンス（`choices[]` 形状）を統一形式にパースする。
pub fn parse_chat_response(body: &Value) -> ChatResponse {
    let id = body["id"].as_str().unwrap_or_default().to_string();
    let model = body["model"].as_str().unwrap_or_default().to_string();
    let created = body["created"]
        .as_i64()
        .unwrap_or_else(|| chrono::Utc::now().timestamp());

    let usage = if let Some(u) = body.get("usage") {
        Usage {
            prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0) as u32,
            completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0) as u32,
            total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as u32,
            cache_read_input_tokens: u["cache_read_input_tokens"].as_u64().unwrap_or(0) as u32,
            cache_creation_input_tokens: u["cache_creation_input_tokens"].as_u64().unwrap_or(0)
                as u32,
        }
    } else {
        Usage::default()
    };

    let choices = body["choices"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|c| {
                    let msg = &c["message"];
                    let role = match msg["role"].as_str().unwrap_or("assistant") {
                        "system" => Role::System,
                        "user" => Role::User,
                        "assistant" => Role::Assistant,
                        "tool" => Role::Tool,
                        _ => Role::Assistant,
                    };

                    let content = msg
                        .get("content")
                        .and_then(|v| v.as_str().map(|s| MessageContent::Text(s.to_string())));

                    let function_call = msg
                        .get("function_call")
                        .and_then(|fc| serde_json::from_value::<FunctionCall>(fc.clone()).ok());

                    let tool_calls = msg
                        .get("tool_calls")
                        .and_then(|tc| serde_json::from_value::<Vec<ToolCall>>(tc.clone()).ok());

                    let tool_call_id = msg
                        .get("tool_call_id")
                        .and_then(|v| v.as_str().map(String::from));

                    Choice {
                        index: c["index"].as_u64().unwrap_or(0) as u32,
                        message: Message {
                            role,
                            content,
                            name: None,
                            function_call,
                            tool_calls,
                            tool_call_id,
                            cache_control: None,
                        },
                        finish_reason: c.get("finish_reason").and_then(parse_finish_reason),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    ChatResponse {
        id,
        model,
        choices,
        usage,
        created,
    }
}

/// SSE の1行（`sse::line_stream` が yield した完全行）から streaming delta を抽出する。
///
/// `data:` 以外の行（keep-alive コメント等）、`[DONE]`、パース不能な JSON は None。
pub fn delta_from_sse_line(line: &str) -> Option<ChatStreamDelta> {
    let line = line.trim();
    let data = line.strip_prefix("data:")?.trim();
    if data == "[DONE]" {
        return None;
    }
    let parsed = serde_json::from_str::<Value>(data).ok()?;

    let id = parsed["id"].as_str().unwrap_or_default().to_string();
    let model = parsed["model"].as_str().unwrap_or_default().to_string();
    let choices = parsed["choices"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|c| {
                    let delta = &c["delta"];
                    StreamChoice {
                        index: c["index"].as_u64().unwrap_or(0) as u32,
                        delta: DeltaMessage {
                            role: delta
                                .get("role")
                                .and_then(|r| serde_json::from_value(r.clone()).ok()),
                            content: delta.get("content").and_then(|v| v.as_str().map(String::from)),
                            function_call: delta
                                .get("function_call")
                                .and_then(|fc| serde_json::from_value(fc.clone()).ok()),
                            tool_calls: delta
                                .get("tool_calls")
                                .and_then(|tc| serde_json::from_value(tc.clone()).ok()),
                        },
                        finish_reason: c
                            .get("finish_reason")
                            .and_then(|fr| serde_json::from_value(fr.clone()).ok()),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    Some(ChatStreamDelta { id, model, choices })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_response_full_shape() {
        let body = json!({
            "id": "chatcmpl-1",
            "model": "gpt-x",
            "created": 123,
            "usage": {
                "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
                "cache_read_input_tokens": 3
            },
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "hello",
                    "tool_calls": [{
                        "id": "call_1", "type": "function",
                        "function": {"name": "f", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let resp = parse_chat_response(&body);
        assert_eq!(resp.id, "chatcmpl-1");
        assert_eq!(resp.usage.total_tokens, 15);
        assert_eq!(resp.usage.cache_read_input_tokens, 3);
        let choice = &resp.choices[0];
        assert_eq!(choice.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(choice.message.tool_calls.as_ref().unwrap()[0].function.name, "f");
        assert_eq!(resp.first_text(), Some("hello"));
    }

    #[test]
    fn parse_response_missing_fields_defaults() {
        let resp = parse_chat_response(&json!({}));
        assert!(resp.choices.is_empty());
        assert_eq!(resp.usage.total_tokens, 0);
    }

    #[test]
    fn message_round_trip_shapes() {
        let msg = Message::user("hi");
        let j = message_to_json(&msg);
        assert_eq!(j["role"], "user");
        assert_eq!(j["content"], "hi");

        let mut tool_msg = Message::user("result");
        tool_msg.role = Role::Tool;
        tool_msg.tool_call_id = Some("call_1".to_string());
        let j = message_to_json(&tool_msg);
        assert_eq!(j["role"], "tool");
        assert_eq!(j["tool_call_id"], "call_1");
    }

    #[test]
    fn delta_line_variants() {
        // 通常の delta
        let d = delta_from_sse_line(
            r#"data: {"id":"1","model":"m","choices":[{"index":0,"delta":{"content":"He"}}]}"#,
        )
        .unwrap();
        assert_eq!(d.choices[0].delta.content.as_deref(), Some("He"));

        // [DONE] / コメント行 / 非 data 行 / 壊れた JSON は None
        assert!(delta_from_sse_line("data: [DONE]").is_none());
        assert!(delta_from_sse_line(": keep-alive").is_none());
        assert!(delta_from_sse_line("event: ping").is_none());
        assert!(delta_from_sse_line("data: {broken").is_none());
    }
}
