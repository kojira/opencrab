use super::*;

/// 「読んだが黙る」を表すプロジェクト全体のセンチネル（下流は `trim() == NO_REPLY` で判定）。
/// #844: verbosity=medium だと 1 応答に message アイテムが複数（実測最大7・ほぼ同一）出て、
/// 「返答→NO_REPLY→返答」のロールプレイ軌跡形を取ることがある。これを無区切り連結すると
/// 下流の全文一致を素通りしてセンチネルが平文露出するため、parse_response で
/// アイテム境界を保持し、非センチネルの先頭アイテムを採用する（全アイテムがセンチネルなら
/// NO_REPLY を残す）。判定をこの集約直後の 1 箇所へ前倒しし、下流の全文一致は backstop に残す。
const NO_REPLY_SENTINEL: &str = "NO_REPLY";

impl ChatGptProvider {
    /// Parse a fully-collected SSE response body into a `ChatResponse`.
    // #676: pub —— chatgpt の SSE パース（incomplete→Length を含む）を server 側の
    // 「incomplete→ターン失敗」end-to-end テストから直接叩けるようにする（純パーサ）。
    pub fn parse_response(&self, sse_text: &str, model: &str) -> Result<ChatResponse> {
        // #844: message アイテムごとのテキストを別々に集める。`current` が進行中アイテムの
        // 蓄積で、アイテム境界（output_item.done）で `items` に確定する。無区切り連結せず
        // アイテム境界を保持することで、下流でセンチネルが平文連結されるのを防ぐ。
        let mut items: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut id = String::new();
        let mut usage = Usage::default();
        // #676（方針3）: Responses API が出力上限で応答を打ち切ったか。status=="incomplete"
        // かつ incomplete_details.reason=="max_output_tokens" のとき真。合成する finish_reason を
        // Length に倒し、エンジンがターンを失敗させられるようにする（切り捨てを黙って最終回答に
        // しない）。chatgpt は cap 値を送らないので、これがモデル内部既定に当たった gpt-5.6 等を
        // 守る実質的な防衛線になる。
        let mut truncated_by_max_tokens = false;
        let mut dbg_data_line_count: usize = 0;
        let mut dbg_delta_event_count: usize = 0;
        let mut current_event = String::new();

        for line in sse_text.lines() {
            let line = line.trim();
            if let Some(ev) = line.strip_prefix("event:") {
                current_event = ev.trim().to_string();
                continue;
            }
            let data = match line.strip_prefix("data:") {
                Some(d) => d.trim(),
                None => continue,
            };
            dbg_data_line_count += 1;
            if data == "[DONE]" {
                current_event.clear();
                continue;
            }
            let parsed: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => {
                    current_event.clear();
                    continue;
                }
            };
            // Effective event type: prefer parsed["type"], fall back to current_event.
            let effective_event = parsed["type"].as_str().unwrap_or(&current_event);
            match effective_event {
                "response.output_text.delta" => {
                    if let Some(delta) = parsed["delta"].as_str() {
                        dbg_delta_event_count += 1;
                        current.push_str(delta);
                    }
                }
                "response.output_item.done" | "response.output_item.completed" => {
                    if let Some(call) = Self::parse_function_call_item(&parsed["item"]) {
                        tool_calls.push(call);
                    }
                    // #844: message アイテムの境界。蓄積テキストがあれば 1 アイテムとして
                    // 確定し、次アイテムのテキストと無区切り連結されるのを断ち切る。
                    // function_call / reasoning アイテムはテキストを積まないので current は空。
                    if !current.is_empty() {
                        items.push(std::mem::take(&mut current));
                    }
                }
                "response.completed" | "response.done" | "response.incomplete" => {
                    if let Some(rid) = parsed["response"]["id"].as_str() {
                        id = rid.to_string();
                    }
                    // #676（方針3）: 出力上限による打ち切りを拾う。incomplete イベントだけでなく、
                    // completed に status/incomplete_details が載る実装差にも耐えるよう、イベント
                    // 名でなく response 本体の status/reason で判定する。
                    if parsed["response"]["status"].as_str() == Some("incomplete")
                        && parsed["response"]["incomplete_details"]["reason"].as_str()
                            == Some("max_output_tokens")
                    {
                        truncated_by_max_tokens = true;
                    }
                    if let Some(output) = parsed["response"]["output"].as_array() {
                        for item in output {
                            if let Some(call) = Self::parse_function_call_item(item) {
                                if !tool_calls.iter().any(|tc| tc.id == call.id) {
                                    tool_calls.push(call);
                                }
                            }
                        }
                    }
                    let u = &parsed["response"]["usage"];
                    // Responses API は cached 分を usage.input_tokens_details.cached_tokens
                    // にネストして返す（codex CLI が flat な cached_input_tokens で返すのとは
                    // 構造が違う点に注意）。フィールドが無い/ null のときは 0 に倒す。
                    let cached = u["input_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .unwrap_or(0) as u32;
                    usage = Usage {
                        prompt_tokens: u["input_tokens"].as_u64().unwrap_or(0) as u32,
                        completion_tokens: u["output_tokens"].as_u64().unwrap_or(0) as u32,
                        total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as u32,
                        cache_read_input_tokens: cached,
                        // OpenAI はキャッシュ書き込みを別課金しない（cache write は
                        // 通常の input と同じ料金）ため、Anthropic のような
                        // cache_creation の概念が無く、Responses API も該当フィールドを
                        // 返さない。ここが 0 なのはバグではなく仕様。
                        cache_creation_input_tokens: 0,
                    };
                }
                "error" => {
                    let msg = parsed["message"]
                        .as_str()
                        .or_else(|| parsed["error"]["message"].as_str())
                        .unwrap_or("unknown error");
                    anyhow::bail!("ChatGPT API error: {}", msg);
                }
                _ => {}
            }
            current_event.clear();
        }

        // #844: output_item.done が来ないまま終わる系（既存テストの delta のみ SSE 等）の
        // 取りこぼしを防ぐ最終フラッシュ。1 アイテムだけの通常応答はここで確定する。
        if !current.is_empty() {
            items.push(std::mem::take(&mut current));
        }

        // #844: アイテム毎に NO_REPLY 判定し、非センチネル（trim 後 NO_REPLY 全文一致でない）
        // かつ非空の先頭アイテムを採用する（実測で first が正解）。実体のあるアイテムが
        // 無い場合、いずれかがセンチネルなら NO_REPLY を残し（下流の沈黙判定を活かす）、
        // そうでなければ空応答（None）にする。単一アイテムの通常応答は従来と同じ結果になる。
        let selected: Option<String> = match items
            .iter()
            .find(|t| {
                let s = t.trim();
                !s.is_empty() && s != NO_REPLY_SENTINEL
            })
            .cloned()
        {
            Some(text) => Some(text),
            None if items.iter().any(|t| t.trim() == NO_REPLY_SENTINEL) => {
                Some(NO_REPLY_SENTINEL.to_string())
            }
            None => None,
        };

        tracing::warn!(
            "chatgpt parse_response: data_lines={} delta_events={} message_items={} selected_bytes={} tool_calls={}",
            dbg_data_line_count,
            dbg_delta_event_count,
            items.len(),
            selected.as_deref().map(str::len).unwrap_or(0),
            tool_calls.len(),
        );

        let content = selected.map(MessageContent::Text);
        // #676（方針3）: 出力上限による打ち切りは tool_calls / content の有無より優先して
        // Length にする。切り捨てられた応答は tool_call JSON も本文も途中で切れており、最終回答
        // にもツール往復の一手にもしてはならない（エンジンがこの Length を見てターンを失敗させる）。
        let finish_reason = if truncated_by_max_tokens {
            FinishReason::Length
        } else if tool_calls.is_empty() {
            FinishReason::Stop
        } else {
            FinishReason::ToolCalls
        };
        let tool_calls = if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        };

        Ok(ChatResponse {
            id,
            model: model.to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content,
                    name: None,
                    function_call: None,
                    tool_calls,
                    tool_call_id: None,
                },
                finish_reason: Some(finish_reason),
            }],
            usage,
            created: 0,
        })
    }

    fn parse_function_call_item(item: &Value) -> Option<ToolCall> {
        if item["type"].as_str()? != "function_call" {
            return None;
        }

        let name = item["name"].as_str()?.to_string();
        let arguments = match item.get("arguments") {
            Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
            Some(Value::Object(_)) | Some(Value::Array(_)) => item["arguments"].to_string(),
            _ => "{}".to_string(),
        };
        let id = item["call_id"]
            .as_str()
            .or_else(|| item["id"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        Some(ToolCall {
            id,
            call_type: "function".to_string(),
            function: FunctionCall { name, arguments },
        })
    }
}
