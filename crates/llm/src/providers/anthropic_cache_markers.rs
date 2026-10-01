//! OpenAI 形式のボディへ Anthropic のプロンプトキャッシュ目印（`cache_control`）を置く（D-1056）。
//!
//! hermit-shell は OpenAI 形式を受けて Anthropic へ変換し、system の text part・tools・
//! user / tool の text part に付いた `cache_control` をそのまま渡す。目印が 1 つも無いと
//! hermit が末尾 1 点だけの自動キャッシュを付けるが、それでは会話が 1 行伸びるたびに
//! 手前の system / tools まで読めなくなる。そこで次の位置へ明示的に置く（Anthropic の上限
//! 4 点、1h が 5m より前）。Anthropic の prefix は tools → system → messages の順なので、
//! system の目印は tools も含めて読む。
//!
//! 先頭 user メッセージの区切り（[`USER_CACHE_SEGMENTS_METADATA`]）があるとき:
//!
//! 1. system の固定部の末尾（1h）
//! 2. system の caller 依存部の末尾（1h）
//! 3. 先頭 user の会話履歴の手前（台帳・Memory Index 等）の末尾（5m）
//! 4. 先頭 user の最後の履歴エントリ（5m）。先頭 user の後にメッセージ（tool 往復等）が
//!    あれば、代わりに最後のメッセージの最後の text（5m）
//!
//! 先頭 user は区切り位置で text part に割る（連結すると元の本文に戻る）。会話は 1 エントリ
//! ずつ伸びるので、前回までのエントリの part 境界が prefix として一致し、そこまで読める。
//!
//! 区切りが無いとき（履歴ブロックの無い user 等）:
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

use crate::message::{ChatRequest, SYSTEM_CACHE_SEGMENTS_METADATA, USER_CACHE_SEGMENTS_METADATA};

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
    let segment_ends = metadata_offsets(request, SYSTEM_CACHE_SEGMENTS_METADATA);
    let user_offsets = metadata_offsets(request, USER_CACHE_SEGMENTS_METADATA);
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
    let has_later_messages = messages.len() > 2;
    let user_split = messages
        .get_mut(1)
        .filter(|msg| msg["role"] == "user" && !user_offsets.is_empty())
        .and_then(|msg| {
            let text = msg["content"].as_str()?.to_string();
            let parts = user_parts(&text, &user_offsets, !has_later_messages)?;
            msg["content"] = Value::Array(parts);
            Some(())
        });
    if user_split.is_none() {
        if let Some(last) = body["tools"].as_array_mut().and_then(|t| t.last_mut()) {
            last["cache_control"] = ttl_1h();
        }
    }
    let Some(messages) = body["messages"].as_array_mut() else {
        return;
    };
    if user_split.is_none() || has_later_messages {
        if let Some(last) = messages.last_mut() {
            mark_last_text(last);
        }
    }
}

fn metadata_offsets(request: &ChatRequest, key: &str) -> Vec<usize> {
    request
        .metadata
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_u64())
                .map(|v| v as usize)
                .collect()
        })
        .unwrap_or_default()
}

/// 先頭 user 本文を `offsets`（`[履歴開始, 各エントリ先頭..., 閉じタグ]`）で text part に割る。
///
/// 履歴の手前の part に 5m。`mark_last_entry` なら閉じタグ直前の part（最後の履歴エントリ）
/// にも 5m。空 part は作らず、連結すると `text` に戻る。区切りが本文と合わなければ `None`
/// （割らずに従来の目印へ戻す）。
fn user_parts(text: &str, offsets: &[usize], mark_last_entry: bool) -> Option<Vec<Value>> {
    let valid = offsets.len() >= 2
        && offsets.windows(2).all(|w| w[0] < w[1])
        && offsets
            .iter()
            .all(|&o| o <= text.len() && text.is_char_boundary(o));
    if !valid {
        return None;
    }
    let history_start = offsets[0];
    let closing = offsets[offsets.len() - 1];
    let mut cuts = vec![0];
    cuts.extend(offsets.iter().copied().filter(|&o| o > 0));
    cuts.push(text.len());
    cuts.dedup();
    let mut parts = Vec::new();
    for w in cuts.windows(2) {
        let (start, end) = (w[0], w[1]);
        if start == end {
            continue;
        }
        let mut part = json!({"type": "text", "text": &text[start..end]});
        let is_prefix = end == history_start;
        let is_last_entry = mark_last_entry && end == closing && start > history_start;
        if is_prefix || is_last_entry {
            part["cache_control"] = ttl_5m();
        }
        parts.push(part);
    }
    Some(parts)
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
        SYSTEM_CACHE_SEGMENTS_METADATA, USER_CACHE_SEGMENTS_METADATA,
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

    const HISTORY_USER: &str = "[Memory Index]\nmi\n\n<conversation_history>\n[u1|owner][t]:\nhi\n[me][t]:\nyo\n</conversation_history>";

    fn history_request(later: Vec<Message>) -> ChatRequest {
        let system = "stable\n\nskills";
        let mut req = request(system, Some(vec![6, 14]), true);
        req.messages[1] = Message::user(HISTORY_USER);
        req.messages.extend(later);
        let at = |needle: &str| HISTORY_USER.find(needle).unwrap();
        let offsets = vec![
            at("<conversation_history>"),
            at("[u1|owner]"),
            at("[me]"),
            at("</conversation_history>"),
        ];
        req.metadata
            .insert(USER_CACHE_SEGMENTS_METADATA.to_string(), json!(offsets));
        req
    }

    fn marker_ttls(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                if let Some(cc) = m.get("cache_control") {
                    out.push(cc["ttl"].as_str().unwrap_or("5m").to_string());
                }
                for value in m.values() {
                    marker_ttls(value, out);
                }
            }
            Value::Array(a) => a.iter().for_each(|x| marker_ttls(x, out)),
            _ => {}
        }
    }

    /// 履歴区切りあり: user を区切りで割り（連結で元に戻る）、system 1h×2・履歴手前 5m・
    /// 最後の履歴エントリ 5m の 4 点。tools には付けない。1h が 5m より前（messages 順）。
    #[test]
    fn enabled_history_user_split_marks_prefix_and_last_entry() {
        let body = OpenAiProvider::new("k")
            .with_anthropic_cache_control(true)
            .build_request_body(&history_request(vec![]));
        assert!(body["tools"][1].get("cache_control").is_none());
        let parts = body["messages"][1]["content"].as_array().unwrap();
        let texts: Vec<&str> = parts.iter().map(|p| p["text"].as_str().unwrap()).collect();
        assert_eq!(texts.concat(), HISTORY_USER);
        assert!(texts.iter().all(|t| !t.is_empty()));
        assert_eq!(
            texts,
            vec![
                "[Memory Index]\nmi\n\n",
                "<conversation_history>\n",
                "[u1|owner][t]:\nhi\n",
                "[me][t]:\nyo\n",
                "</conversation_history>",
            ]
        );
        let marked: Vec<bool> = parts
            .iter()
            .map(|p| p.get("cache_control").is_some())
            .collect();
        assert_eq!(marked, vec![true, false, false, true, false]);
        let mut ttls = Vec::new();
        marker_ttls(&body["messages"], &mut ttls);
        assert_eq!(ttls, vec!["1h", "1h", "5m", "5m"]);
        assert_eq!(count_markers(&body), 4);
    }

    /// 先頭 user の後にメッセージがあれば、最後の履歴エントリの代わりに最後のメッセージへ 5m。
    #[test]
    fn enabled_history_user_with_later_messages_marks_last_message() {
        let mut assistant = Message::assistant("");
        assistant.tool_calls = Some(vec![ToolCall {
            id: "c1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "a".to_string(),
                arguments: "{}".to_string(),
            },
        }]);
        let req = history_request(vec![assistant, Message::tool("c1", "result")]);
        let body = OpenAiProvider::new("k")
            .with_anthropic_cache_control(true)
            .build_request_body(&req);
        let parts = body["messages"][1]["content"].as_array().unwrap();
        let marked: Vec<bool> = parts
            .iter()
            .map(|p| p.get("cache_control").is_some())
            .collect();
        assert_eq!(marked, vec![true, false, false, false, false]);
        assert_eq!(
            body["messages"][3]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(count_markers(&body), 4);
    }

    /// 区切りが本文と合わなければ割らずに従来の目印（tools 1h・最後のメッセージ 5m）。
    #[test]
    fn enabled_history_user_invalid_offsets_falls_back() {
        let mut req = history_request(vec![]);
        req.metadata.insert(
            USER_CACHE_SEGMENTS_METADATA.to_string(),
            json!([HISTORY_USER.len() + 10]),
        );
        let body = OpenAiProvider::new("k")
            .with_anthropic_cache_control(true)
            .build_request_body(&req);
        assert_eq!(body["tools"][1]["cache_control"]["ttl"], "1h");
        let parts = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], HISTORY_USER);
        assert_eq!(count_markers(&body), 4);
    }

    /// 無効なら履歴区切りがあってもボディは従来と同一。
    #[test]
    fn disabled_body_with_history_offsets_is_unchanged() {
        let req = history_request(vec![]);
        let plain = OpenAiProvider::new("k").build_request_body(&req);
        let off = OpenAiProvider::new("k")
            .with_anthropic_cache_control(false)
            .build_request_body(&req);
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            serde_json::to_string(&off).unwrap()
        );
        assert_eq!(count_markers(&off), 0);
        assert_eq!(off["messages"][1]["content"], HISTORY_USER);
    }
}
