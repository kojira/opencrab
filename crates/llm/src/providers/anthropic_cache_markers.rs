//! OpenAI 形式のボディへ Anthropic のプロンプトキャッシュ目印（`cache_control`）を置く（D-1056）。
//!
//! hermit-shell は OpenAI 形式を受けて Anthropic へ変換し、system の text part・tools・
//! user / tool の text part に付いた `cache_control` をそのまま渡す。目印が 1 つも無いと
//! hermit が末尾 1 点だけの自動キャッシュを付けるが、それでは会話が 1 行伸びるたびに
//! 手前の system / tools まで読めなくなる。そこで次の位置へ明示的に置く（最大 4 点、
//! 1h が 5m より前）:
//!
//! 1. tools の最後（1h）
//! 2. system の固定部の末尾（1h）
//! 3. system の caller 依存部の末尾（1h）
//! 4. 最後のメッセージの最後の text（5m）
//!
//! 本物の OpenAI は `cache_control` を未知パラメータとして 400 で拒否するため、この処理は
//! プロバイダ設定 `anthropic_cache_control` を有効にした接続先（hermit）だけで呼ぶ。
//!
//! tools 付きリクエストに限る: hermit の tools 無し経路（`convertRequest`）は system を
//! 文字列としてしか扱えず、また単発呼び出しは後続が無く書き込みの割増だけになる
//! （`anthropic.rs` と同じ理由）。

use serde_json::{json, Value};

use crate::message::{ChatRequest, SYSTEM_CACHE_SEGMENTS_METADATA};

fn ttl_1h() -> Value {
    json!({"type": "ephemeral", "ttl": "1h"})
}

fn ttl_5m() -> Value {
    json!({"type": "ephemeral"})
}

/// `body`（`openai.rs` が組んだ OpenAI 形式）へ目印を置く。
pub(super) fn apply(body: &mut Value, request: &ChatRequest) {
    let has_tools = request.functions.as_ref().is_some_and(|f| !f.is_empty());
    if !has_tools {
        return;
    }
    if let Some(last) = body["tools"].as_array_mut().and_then(|t| t.last_mut()) {
        last["cache_control"] = ttl_1h();
    }
    let segment_ends: Vec<usize> = request
        .metadata
        .get(SYSTEM_CACHE_SEGMENTS_METADATA)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_u64())
                .map(|v| v as usize)
                .collect()
        })
        .unwrap_or_default();
    let Some(messages) = body["messages"].as_array_mut() else {
        return;
    };
    for msg in messages.iter_mut() {
        if msg["role"] == "system" {
            if let Some(text) = msg["content"].as_str() {
                msg["content"] = Value::Array(system_parts(text, &segment_ends));
            }
        }
    }
    if let Some(last) = messages.last_mut() {
        mark_last_text(last);
    }
}

/// system 本文を区切り位置で text part に割り、各区切りの part に 1h を付ける。
/// 区切りの後ろ（リクエスト毎の部分）には付けない。区切りが無ければ全体で 1 part・1h。
fn system_parts(text: &str, segment_ends: &[usize]) -> Vec<Value> {
    let mut ends: Vec<usize> = segment_ends
        .iter()
        .copied()
        .filter(|&e| e <= text.len() && text.is_char_boundary(e))
        .collect();
    ends.dedup();
    if ends.is_empty() {
        ends.push(text.len());
    }
    let mut parts = Vec::new();
    let mut start = 0;
    for end in ends {
        if end > start {
            parts.push(
                json!({"type": "text", "text": &text[start..end], "cache_control": ttl_1h()}),
            );
            start = end;
        }
    }
    if start < text.len() {
        parts.push(json!({"type": "text", "text": &text[start..]}));
    }
    parts
}

/// 最後のメッセージの最後の text に 5m の目印を付ける。
///
/// tool_calls 付き assistant は付けない: hermit がその content を text ブロックの `text` へ
/// そのまま入れるため、配列にすると壊れる。空本文も付けない（空 text は Anthropic が拒否）。
fn mark_last_text(msg: &mut Value) {
    let is_assistant_with_calls =
        msg["role"] == "assistant" && msg["tool_calls"].as_array().is_some_and(|c| !c.is_empty());
    if is_assistant_with_calls {
        return;
    }
    match &mut msg["content"] {
        Value::String(text) if !text.is_empty() => {
            let text = std::mem::take(text);
            msg["content"] = json!([{"type": "text", "text": text, "cache_control": ttl_5m()}]);
        }
        Value::Array(parts) => {
            if let Some(part) = parts
                .iter_mut()
                .rev()
                .find(|p| p["type"] == "text" && p["text"].as_str().is_some_and(|t| !t.is_empty()))
            {
                part["cache_control"] = ttl_5m();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use crate::message::{
        ChatRequest, FunctionCall, FunctionDefinition, Message, MessageContent, ToolCall,
        SYSTEM_CACHE_SEGMENTS_METADATA,
    };
    use crate::providers::OpenAiProvider;
    use serde_json::{json, Value};

    fn tool(name: &str) -> FunctionDefinition {
        FunctionDefinition {
            name: name.to_string(),
            description: Some("d".to_string()),
            parameters: json!({"type": "object"}),
        }
    }

    fn request(system: &str, ends: Option<Vec<usize>>, tools: bool) -> ChatRequest {
        let mut req = ChatRequest::new(
            "claude-sonnet-5-5",
            vec![Message::system(system), Message::user("hello")],
        );
        if tools {
            req.functions = Some(vec![tool("a"), tool("b")]);
        }
        if let Some(ends) = ends {
            req.metadata
                .insert(SYSTEM_CACHE_SEGMENTS_METADATA.to_string(), json!(ends));
        }
        req
    }

    fn count_markers(v: &Value) -> usize {
        match v {
            Value::Object(m) => {
                usize::from(m.contains_key("cache_control"))
                    + m.values().map(count_markers).sum::<usize>()
            }
            Value::Array(a) => a.iter().map(count_markers).sum(),
            _ => 0,
        }
    }

    /// 既定（無効）では目印を一切載せず、従来どおり system は文字列のまま（本物の OpenAI 互換）。
    #[test]
    fn disabled_body_is_unchanged() {
        let system = "stable\n\nskills\n\nnostr";
        let req = request(system, Some(vec![6, 14]), true);
        let plain = OpenAiProvider::new("k").build_request_body(&req);
        let off = OpenAiProvider::new("k")
            .with_anthropic_cache_control(false)
            .build_request_body(&req);
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            serde_json::to_string(&off).unwrap()
        );
        assert_eq!(count_markers(&off), 0);
        assert_eq!(off["messages"][0]["content"], system);
    }

    /// 有効: tools 末尾・固定部末尾・caller 部末尾に 1h、最後のメッセージに 5m の計 4 点。
    /// system は区切り位置で割られ、連結すると元の本文に戻る（中身は変えない）。
    #[test]
    fn enabled_body_marks_tools_system_segments_and_last_message() {
        let system = "stable\n\nskills\n\nnostr";
        let req = request(system, Some(vec![6, 14]), true);
        let body = OpenAiProvider::new("k")
            .with_anthropic_cache_control(true)
            .build_request_body(&req);

        let tools = body["tools"].as_array().unwrap();
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(
            tools[1]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );

        let parts = body["messages"][0]["content"].as_array().unwrap();
        let texts: Vec<&str> = parts.iter().map(|p| p["text"].as_str().unwrap()).collect();
        assert_eq!(texts, vec!["stable", "\n\nskills", "\n\nnostr"]);
        assert_eq!(texts.concat(), system);
        assert_eq!(parts[0]["cache_control"]["ttl"], "1h");
        assert_eq!(parts[1]["cache_control"]["ttl"], "1h");
        assert!(parts[2].get("cache_control").is_none());

        assert_eq!(
            body["messages"][1]["content"],
            json!([{"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}}])
        );
        assert_eq!(count_markers(&body), 4);
    }

    /// 区切り情報が無ければ system 全体を 1 part・1h にする。
    #[test]
    fn enabled_without_segments_marks_whole_system() {
        let req = request("whole", None, true);
        let body = OpenAiProvider::new("k")
            .with_anthropic_cache_control(true)
            .build_request_body(&req);
        assert_eq!(
            body["messages"][0]["content"],
            json!([{"type": "text", "text": "whole", "cache_control": {"type": "ephemeral", "ttl": "1h"}}])
        );
        assert_eq!(count_markers(&body), 3);
    }

    /// tools 無し（単発呼び出し）には載せない（hermit の tools 無し経路は system 文字列前提）。
    #[test]
    fn enabled_without_tools_adds_nothing() {
        let req = request("stable\n\nx", Some(vec![6]), false);
        let body = OpenAiProvider::new("k")
            .with_anthropic_cache_control(true)
            .build_request_body(&req);
        assert_eq!(count_markers(&body), 0);
        assert_eq!(body["messages"][0]["content"], "stable\n\nx");
    }

    /// 末尾が tool 結果なら tool の本文に 5m。tool_calls 付き assistant が末尾なら付けない。
    #[test]
    fn enabled_last_message_variants() {
        let call = ToolCall {
            id: "c1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "a".to_string(),
                arguments: "{}".to_string(),
            },
        };
        let mut assistant = Message::assistant("");
        assistant.tool_calls = Some(vec![call]);

        let mut req = request("s", None, true);
        req.messages.push(assistant.clone());
        req.messages.push(Message::tool("c1", "result"));
        let body = OpenAiProvider::new("k")
            .with_anthropic_cache_control(true)
            .build_request_body(&req);
        assert_eq!(
            body["messages"][3]["content"],
            json!([{"type": "text", "text": "result", "cache_control": {"type": "ephemeral"}}])
        );
        assert_eq!(body["messages"][3]["tool_call_id"], "c1");

        let mut req = request("s", None, true);
        req.messages.push(assistant);
        let body = OpenAiProvider::new("k")
            .with_anthropic_cache_control(true)
            .build_request_body(&req);
        assert!(
            body["messages"][2]["content"].is_string() || body["messages"][2]["content"].is_null()
        );
        assert_eq!(count_markers(&body), 2);

        // マルチパート user は最後の text part に付ける（画像 part には付けない）。
        let mut req = request("s", None, true);
        req.messages.push(Message {
            content: Some(MessageContent::Multi(vec![
                crate::message::ContentPart::Text {
                    text: "look".to_string(),
                },
                crate::message::ContentPart::ImageUrl {
                    image_url: crate::message::ImageUrl {
                        url: "https://example.com/a.png".to_string(),
                        detail: None,
                    },
                },
            ])),
            ..Message::user("")
        });
        let body = OpenAiProvider::new("k")
            .with_anthropic_cache_control(true)
            .build_request_body(&req);
        let parts = body["messages"][2]["content"].as_array().unwrap();
        assert_eq!(parts[0]["cache_control"], json!({"type": "ephemeral"}));
        assert!(parts[1].get("cache_control").is_none());
    }
}
