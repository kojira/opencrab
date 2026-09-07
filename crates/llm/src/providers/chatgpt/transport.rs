use super::*;

#[async_trait]
impl LlmProvider for ChatGptProvider {
    fn name(&self) -> &str {
        &self.name
    }

    // #676: Responses API は max_output_tokens が 400（Unsupported parameter）なので
    // 送らない（build_request_body でも載せない）。よって出力上限のモデル登録は不要
    // （opt-out）。切り捨て検知は方針3の incomplete_details→Length→bail が担う。
    fn sends_max_output_tokens(&self) -> bool {
        false
    }

    async fn available_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(vec![
            // GPT-5.6 系（gpt-5.6 は Sol にエイリアス）。codex CLI と同じ
            // codex/responses バックエンドを叩くため、同じサブスクで利用できる。
            // codex サブプロセスと違い画像（image_url）とネイティブ function
            // calling に対応するので、画像を読ませたいエージェントはこちらを使う。
            ModelInfo {
                id: "gpt-5.6".to_string(),
                name: "GPT-5.6 (Sol)".to_string(),
                context_window: 400_000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "gpt-5.6-sol".to_string(),
                name: "GPT-5.6 Sol".to_string(),
                context_window: 400_000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "gpt-5.6-terra".to_string(),
                name: "GPT-5.6 Terra".to_string(),
                context_window: 400_000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "gpt-5.6-luna".to_string(),
                name: "GPT-5.6 Luna".to_string(),
                context_window: 400_000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "gpt-5.5".to_string(),
                name: "GPT-5.5".to_string(),
                context_window: 128000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "gpt-4o".to_string(),
                name: "GPT-4o".to_string(),
                context_window: 128000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "gpt-4.5-preview".to_string(),
                name: "GPT-4.5 Preview".to_string(),
                context_window: 128000,
                supports_function_calling: true,
                supports_vision: true,
            },
        ])
    }

    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse> {
        debug!(model = %request.model, "ChatGPT chat completion");
        let mut token = self.fresh_access_token().await?;
        let mut account_id = extract_account_id(&token)?;
        // http(s) 画像は自分で取得して data URI 化してから送る（後述）。
        let request = self.inline_remote_images(&request).await;
        let body = self.build_request_body(&request, true);
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        tracing::warn!(
            model = %request.model,
            has_instructions = body.get("instructions").is_some(),
            instructions_len = body["instructions"].as_str().map(|s| s.len()).unwrap_or(0),
            input_count = body["input"].as_array().map(|a| a.len()).unwrap_or(0),
            has_reasoning = body.get("reasoning").is_some(),
            reasoning_effort = body["reasoning"]["effort"].as_str().unwrap_or("none"),
            body_len = body_str.len(),
            "ChatGPT chat_completion: sending request"
        );
        // リトライは router が所有する（#46）。以前はここで独自に3回リトライしており、
        // router の同一プロバイダ3回リトライと重なって最大9回の HTTP 試行になっていた。
        // 429/5xx は型付き api_error で返せば router が retryable と分類して再試行する。
        // 例外: 401 だけはトークンリフレッシュがプロバイダの能力なので、ここで
        // 1回だけリフレッシュ→再送する（router は auth エラーをリトライしない）。
        let mut refreshed = false;
        let (status, text) = loop {
            let resp = self
                .request_builder("codex/responses", &token, &account_id)
                .json(&body)
                .send()
                .await
                .context("ChatGPT API request failed")?;

            let status = resp.status();
            let text = resp
                .text()
                .await
                .context("ChatGPT: failed to read response body")?;

            tracing::warn!(status = %status, body_len = text.len(), "ChatGPT chat_completion response received");

            if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                tracing::info!("ChatGPT returned 401; refreshing token and retrying once");
                token = self.refresh_access_token(Some(&token)).await?;
                account_id = extract_account_id(&token)?;
                continue;
            }
            break (status, text);
        };

        if !status.is_success() {
            tracing::warn!(status = %status, body = %text, "ChatGPT chat_completion error response");
            return Err(crate::error::api_error("ChatGPT", status, text));
        }

        let result = self.parse_response(&text, &request.model);
        tracing::warn!(
            success = result.is_ok(),
            "ChatGPT chat_completion parse result"
        );
        result
    }

    async fn chat_completion_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        debug!(model = %request.model, "ChatGPT streaming chat completion");
        let mut token = self.fresh_access_token().await?;
        let mut account_id = extract_account_id(&token)?;
        let request = self.inline_remote_images(&request).await;
        let body = self.build_request_body(&request, true);
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        tracing::warn!(
            model = %request.model,
            has_instructions = body.get("instructions").is_some(),
            instructions_len = body["instructions"].as_str().map(|s| s.len()).unwrap_or(0),
            input_count = body["input"].as_array().map(|a| a.len()).unwrap_or(0),
            has_reasoning = body.get("reasoning").is_some(),
            reasoning_effort = body["reasoning"]["effort"].as_str().unwrap_or("none"),
            body_len = body_str.len(),
            "ChatGPT chat_completion_stream: sending request"
        );
        // リトライは router が所有する（#46: 内部リトライとの重なりで最大9試行に
        // なっていた）。エラーは型付き api_error で返し、router の分類に委ねる。
        // 例外: 401 のみプロバイダ責務としてリフレッシュ→1回だけ再送（非ストリーム側と同じ）。
        let mut refreshed = false;
        let resp = loop {
            let resp = self
                .request_builder("codex/responses", &token, &account_id)
                .json(&body)
                .send()
                .await
                .context("ChatGPT streaming request failed")?;

            let status = resp.status();
            tracing::warn!(status = %status, "ChatGPT chat_completion_stream response received");
            if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                tracing::info!("ChatGPT returned 401 (stream); refreshing token and retrying once");
                token = self.refresh_access_token(Some(&token)).await?;
                account_id = extract_account_id(&token)?;
                continue;
            }
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                tracing::warn!(status = %status, body = %text, "ChatGPT chat_completion_stream error response");
                return Err(crate::error::api_error("ChatGPT", status, text));
            }
            break resp;
        };
        let request_model = request.model.clone();
        // チャンク境界を跨いでバッファし、SSEの `data:` 行ごとに1デルタを emit する。
        // Responses API は data ペイロード自身に `type` を含むため、行を跨ぐ `event:` 状態には
        // 依存しない。これにより、同一チャンク内で後続イベントが直前のテキストデルタを
        // 上書きしてしまう問題を防ぐ。
        let stream =
            crate::providers::sse::line_stream(resp.bytes_stream()).filter_map(move |line_res| {
                let request_model = request_model.clone();
                let out = match line_res {
                    Err(e) => Some(Err(e)),
                    Ok(line) => {
                        let line = line.trim();
                        match line.strip_prefix("data:").map(|d| d.trim()) {
                            None => None,
                            Some("[DONE]") => None,
                            Some(data) => match serde_json::from_str::<Value>(data) {
                                Err(_) => None,
                                Ok(parsed) => match parsed["type"].as_str().unwrap_or_default() {
                                    "response.output_text.delta" => {
                                        let delta_text = parsed["delta"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .to_string();
                                        Some(Ok(ChatStreamDelta {
                                            id: String::new(),
                                            model: request_model,
                                            choices: vec![StreamChoice {
                                                index: 0,
                                                delta: DeltaMessage {
                                                    role: None,
                                                    content: Some(delta_text),
                                                    function_call: None,
                                                    tool_calls: None,
                                                },
                                                finish_reason: None,
                                            }],
                                        }))
                                    }
                                    "response.completed" | "response.done" => {
                                        // Tool calls are ignored for now (future work).
                                        Some(Ok(ChatStreamDelta {
                                            id: String::new(),
                                            model: request_model,
                                            choices: vec![StreamChoice {
                                                index: 0,
                                                delta: DeltaMessage {
                                                    role: None,
                                                    content: Some(String::new()),
                                                    function_call: None,
                                                    tool_calls: None,
                                                },
                                                finish_reason: Some(FinishReason::Stop),
                                            }],
                                        }))
                                    }
                                    _ => None,
                                },
                            },
                        }
                    }
                };
                futures::future::ready(out)
            });
        Ok(Box::pin(stream))
    }

    fn supports_function_calling(&self) -> bool {
        true
    }

    fn supports_vision(&self) -> bool {
        true
    }

    async fn health_check(&self) -> Result<bool> {
        Ok(self.load_access_token().is_ok())
    }
}
