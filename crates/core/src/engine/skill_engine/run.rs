use anyhow::Result;
use opencrab_llm_types::{Message, MessageContent, Role, ToolCall};

use super::{
    run_helpers::{
        self, classify_call_failure, initialize_turn, normalize_response,
        partition_tool_calls_for_dispatch, InitialTurn,
    },
    turn_budget::{apply_turn_budget, seat_tool_result},
    SkillEngine,
};
use crate::engine::types::{ChatRequest, EngineResult, LlmCallLog, LlmExchangeLog};

impl SkillEngine {
    /// Run the action loop with optional dynamic model override.
    ///
    /// If `model_override` is provided, the engine checks it before each LLM call
    /// and uses the overridden model if set (e.g., by `select_llm` action).
    pub async fn run_with_model_override(
        &self,
        system_context: &str,
        user_message: &str,
        default_model: &str,
        model_override: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
        image_urls: &[String],
    ) -> Result<EngineResult> {
        // プロンプトキャッシュはプロバイダの能力としてプロバイダ側が適用する（#44）。
        // 以前はここで Anthropic 固有の cache_control を全リクエストに無条件付与して
        // いたが、読むのは anthropic だけ・system 分は黙って落ちる偽ユニバーサル
        // 抽象だった。エンジンはプロバイダ非依存のリクエストだけを組む。
        // §2.7: functions はループ内で毎イテレーション list_tools を取り直して組む（活性集合を
        // 反映）。ここでの事前取得は結果を捨てる死んだ呼び出しだったので置かない。

        let InitialTurn {
            mut messages,
            ledger: mut turn_ledger,
            governor: mut turn_gov,
        } = initialize_turn(
            system_context,
            user_message,
            image_urls,
            self.typed_conversation.as_ref(),
            (self.conversation_high, self.conversation_low),
        );

        let mut iterations = 0;
        // #964: 次の request に新しく含める origin。発端は初回だけここへ入り、走行中の新着は
        // 実際に messages へ append したイテレーションで加える。loop restart が同じ engine を
        // 再利用しても発端を再通知しないよう、engine 側の値はここで consume する。
        let initial_read_origin = self
            .initial_read_origin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let mut pending_read_origins: Vec<String> = initial_read_origin.into_iter().collect();
        // 同じ run 内で既に通知した origin は再び pending に入れない。
        let mut read_emitted_origins: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut total_tool_calls = 0;
        let mut xml_fallback_parses = 0;
        // #915: 各生成で最後に成功した投稿系 utterance-op の call_id。生成開始時にリセットし、
        // 上限到達時だけ直前（打ち切られた最終生成）の値を保持して返す。
        let mut last_posting_utterance_id: Option<String> = None;
        let mut last_generation_had_continuation_speech = false;

        loop {
            iterations += 1;

            if iterations > self.max_iterations {
                tracing::warn!(
                    iterations = iterations,
                    max = self.max_iterations,
                    "SkillEngine reached max iterations, stopping"
                );
                return Ok(EngineResult {
                    // 資源切れを assistant 発言に偽装しない。gateway は既に配送した最後の
                    // 投稿へ完了リアクションを付け、server は turn_exhausted を履歴へ残す。
                    response: String::new(),
                    iterations,
                    tool_calls_made: total_tool_calls,
                    stopped_by_limit: true,
                    explicit_termination: None,
                    last_posting_utterance_id,
                    last_generation_had_continuation_speech,
                    xml_fallback_parses,
                });
            }
            last_posting_utterance_id = None;
            last_generation_had_continuation_speech = false;

            // #289: 走行中に届いた新着ユーザー発言を、この呼び出しの入力へ足す。
            //
            // 会話履歴はターン開始時に 1 度だけ組まれるため、ツール往復が長引くと
            // その間の発言は次ターンまで見えなかった（実測でオーナーの「やめて」が
            // 9 秒、#284 の例では約 1 分遅れた）。ここで**差分だけ**を user メッセージ
            // として積む。履歴全体は組み直さない（重い＋コンテキストが膨らむ）。
            //
            // 足すだけで応答は強制しない。見て答えるか作業を続けるかはエージェントの
            // 判断に委ねる（#288 の強制を撤回した方針）。
            //
            // 1 周目は引かない: ターン開始時の履歴がその時点の発言を既に含んでおり、
            // 引くと同じ発言が二重に載る。重複防止の残りは実装側（poll は「前回以降」
            // だけを返す契約）。
            //
            // 位置はツール結果を積み終えた後・LLM 呼び出しの直前。tool_result の直後に
            // user メッセージが並ぶ形になるが、連続 user ロールは許容される
            // （Anthropic は同ロールを 1 ターンへ併合する）。
            if iterations > 1 {
                if let Some(source) = &self.live_inbound {
                    // #964: origin つきで引く。ここでは request に含める本文と origin の組を
                    // pending に積むだけにし、read 通知は request 構築後の `llm.chat` 直前まで遅らせる。
                    for folded in source.poll_new_with_origin() {
                        let crate::FoldedInbound { text, origin } = folded;
                        tracing::info!(
                            iteration = iterations,
                            bytes = text.len(),
                            "injecting newly arrived user speech into the running turn"
                        );
                        messages.push(Message {
                            role: Role::User,
                            content: Some(MessageContent::Text(text.clone())),
                            name: None,
                            function_call: None,
                            tool_calls: None,
                            tool_call_id: None,
                        });
                        turn_ledger.record(format!("live:{}", messages.len()), &text);
                        // 同じ origin が同一 poll や以前の request に重なっても通知は 1 回だけ。
                        if let Some(origin) = origin {
                            if !read_emitted_origins.contains(&origin)
                                && !pending_read_origins.contains(&origin)
                            {
                                pending_read_origins.push(origin);
                            }
                        }
                    }
                }
            }
            apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;

            // Check for dynamic model override.
            let model = model_override
                .as_ref()
                .and_then(|o| o.lock().ok().and_then(|m| m.clone()))
                .unwrap_or_else(|| default_model.to_string());

            // #665: LLM 呼び出しの入り。この後の `self.llm.chat(...).await` が返らなければここが
            // 最後の行になる（宙吊りの典型＝推論に入って戻らない／プロキシ未到達）。agent_id / session_id /
            // turn_id は run_agent_response が張った span から継承する。
            tracing::debug!(
                iteration = iterations,
                model = %model,
                messages = messages.len(),
                stage = "llm_call",
                "turn: LLM リクエスト 開始（入）"
            );

            // §2.7: describe_tools でこのターンに活性化したツールを次イテレーションの関数集合へ
            // 反映するため、毎イテレーション list_tools を取り直す（階層化しても depth>0 なら
            // 常に同じ集合を返すので従来挙動と等価）。
            let tools = self.executor.list_tools();

            let request = ChatRequest {
                model: model.clone(),
                messages: messages.clone(),
                functions: if tools.is_empty() {
                    None
                } else {
                    Some(tools.clone())
                },
                function_call: None,
                temperature: Some(0.7),
                max_tokens: self.max_output_tokens,
                stop: None,
                stream: None,
                metadata: {
                    let mut m: std::collections::HashMap<String, serde_json::Value> =
                        Default::default();
                    if self.web_search {
                        m.insert("web_search".to_string(), serde_json::json!(true));
                    }
                    m
                },
                agent_id: None,
                reasoning_effort: self.reasoning_effort.clone(),
            };

            let request_for_log = request.clone();
            let requested_at =
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

            // #964: この exact request に新しく含めた origin の read 通知を、request が完成した後、
            // `llm.chat(request).await` の直前に逐次 emit する。対象が無ければ何もしない。
            // drain してから呼ぶことで、同じ origin は次の request で重複通知しない。
            if let Some(cb) = &self.on_read_origin {
                for origin in pending_read_origins.drain(..) {
                    if read_emitted_origins.insert(origin.clone()) {
                        cb(origin).await;
                    }
                }
            } else {
                pending_read_origins.clear();
            }

            let call_start = std::time::Instant::now();
            let exchange_result = self.llm.chat_with_history(request).await;
            let latency_ms = call_start.elapsed().as_millis() as i64;
            // #665: LLM 呼び出しの出。入と対で出す（入だけだと「入って止まった」と「戻った」が
            // 区別できない）。成否と latency を載せ、この後のツール往復／最終応答へ進む。
            tracing::debug!(
                iteration = iterations,
                latency_ms,
                ok = exchange_result.is_ok(),
                stage = "llm_call",
                "turn: LLM リクエスト 完了（出）"
            );

            // #706 / #676: transport の成否とは別に「このターンの応答は使えるか」を
            // **log_callback の前に**判定する。log_callback（process.rs 側）はこの時点で
            // llm_logs へ即 INSERT するので、判定結果を載せずに呼ぶと、切り捨て（#676）や
            // 意味的に空（#706）の応答が error 欄空の「成功行」として残り、fail loud に
            // しても理由がログに載らない（設計 §1-c の落とし穴）。空応答と出力上限切り捨てを
            // 同じ 1 経路で捕まえ、種別（error_code）を engine 側で確定させる——process は
            // その値を写すだけにする。判定は中身の形だけで行い、finish_reason=Length は
            // 「上限切り捨て」の特定にのみ使う（空判定には混ぜない＝stop を名乗る空応答を
            // 取りこぼさない）。
            let response_result = match &exchange_result {
                Ok(exchange) => Ok(exchange.response.clone()),
                Err(error) => Err(anyhow::anyhow!(error.to_string())),
            };
            let call_failure =
                classify_call_failure(&response_result, &model, self.max_output_tokens);
            let call_log = LlmCallLog {
                request: request_for_log.clone(),
                response: exchange_result
                    .as_ref()
                    .ok()
                    .map(|exchange| exchange.response.clone()),
                error_str: call_failure.as_ref().map(|failure| failure.body.clone()),
                error_code: call_failure.as_ref().map(|failure| failure.code.clone()),
                latency_ms,
                requested_at: requested_at.clone(),
                is_bot_iteration: iterations > 1,
            };

            if let Some(cb) = &self.log_callback {
                cb(&call_log);
            }
            if let Some(cb) = &self.exchange_log_callback {
                let provider_tool_history = exchange_result
                    .as_ref()
                    .map(|exchange| exchange.provider_tool_history.clone())
                    .unwrap_or_else(|_| opencrab_llm_types::ProviderToolHistory {
                        // A transport error carries no reliable resolved-provider identity here.
                        // Do not label non-ChatGPT failures as native-search parse failures.
                        state: opencrab_llm_types::ProviderToolHistoryState::NotRequested,
                        provider: None,
                        calls: Vec::new(),
                        citations: Vec::new(),
                    });
                cb(&LlmExchangeLog {
                    call: call_log,
                    provider_tool_history,
                });
            }

            // transport 失敗はここで打ち切り（理由は上で llm_logs に残した）。
            let response = exchange_result?.response;

            // Ok だが意味的に使えない応答（空 #706 / 切り捨て #676）は fail loud で打ち切る。
            // tool_calls / content を抽出する**前**に見る——切り捨てられた tool_call JSON が
            // 「空の tool_calls → 最終応答扱い」で黙って消える形をここ 1 点で塞ぐ。
            if let Some(run_helpers::CallFailure { code, body }) = call_failure {
                tracing::error!(
                    iteration = iterations,
                    error_code = %code,
                    model = %model,
                    stage = "turn_failed",
                    "turn: LLM 応答が使えないためターン失敗（fail loud）"
                );
                anyhow::bail!("{body}");
            }

            // 応答本文とツールコールをローカルに抽出（正準モデルは choices[0] を持つ）。
            let normalized = normalize_response(&response);
            let mut content = normalized.content;
            let tool_calls = normalized.tool_calls;

            // If the LLM returned no structured tool calls but embedded
            // <function_calls> XML in the content (e.g. DeepSeek via OpenRouter),
            // parse them out and treat them as normal tool calls.
            if normalized.xml_tool_count > 0 {
                // 発火は harness 剪定の判断材料として計測する（EngineResult 経由で
                // agent_logs にも記録される）。codex プロバイダは意図的にこの
                // フォールバックへ依存するため、発火＝異常ではない（毎イテレーション
                // 発火し得るのでログは debug に留め、run 単位の集計を agent_logs で見る）。
                xml_fallback_parses += 1;
                tracing::debug!(
                    count = normalized.xml_tool_count,
                    model = %model,
                    "Parsed XML function_calls from content (harness fallback fired)"
                );
            }

            // ターンは既定で継続する。LLM が NO_REPLY を明示した生成だけが終了を要求する。
            // marker は配送本文から分離して EngineResult::explicit_termination に保持する。
            // query/tool call と併記された場合は結果を読む必要があるため tool 経路を優先する。
            let termination = content
                .as_deref()
                .map(crate::continue_marker::terminate_at_no_reply);
            let termination_requested = termination
                .as_ref()
                .is_some_and(|termination| termination.terminated());
            if termination_requested {
                content = termination
                    .as_ref()
                    .and_then(|termination| termination.speech().map(str::to_string));
            }

            // Fire on_response_text for every LLM reply that has non-empty text.
            if let Some(ref text) = content {
                if !text.trim().is_empty() {
                    if let Some(ref cb) = self.on_response_text {
                        tracing::warn!(
                            iteration = iterations,
                            text_len = text.len(),
                            text_preview = %text.chars().take(100).collect::<String>(),
                            "LLM response text received, firing on_response_text callback"
                        );
                        cb(text.clone());
                        tracing::warn!(iteration = iterations, "on_response_text callback fired");
                    }
                }
            }

            // If there are tool calls, execute them. Utterance-only generations are also
            // followed by another LLM call unless this generation explicitly terminates;
            // query/tool calls produce tool results before the next call.
            if !tool_calls.is_empty() {
                // 照会/道具、またはpermission deniedの発話が1つでもあればproviderの
                // tool_call/tool_result対を作る。許可済み発話だけでも明示終了が無ければ、
                // 最小ackを積んで次のLLM呼び出しへ進む。
                let next_llm_call_needed = tool_calls.iter().any(|tc| {
                    !self.is_utterance_tool(&tc.function.name)
                        || !self.is_action_allowed(&tc.function.name)
                });

                // 純発話でも NO_REPLY が無ければ、最小 ack を積んで次の LLM 呼び出しへ進む。
                // NO_REPLY があるときだけ発話を配送して、この generation で明示終了する。
                if !next_llm_call_needed && termination_requested {
                    for tool_call in &tool_calls {
                        total_tool_calls += 1;
                        let tool_name = &tool_call.function.name;
                        let args = tool_call.arguments_json();
                        let result = self
                            .executor
                            .execute_with_id(tool_name, &args, &tool_call.id)
                            .await;
                        if tool_name == "reply" && result.success {
                            last_posting_utterance_id = Some(tool_call.id.clone());
                        }
                        tracing::debug!(
                            iteration = iterations,
                            tool = %tool_name,
                            id = %tool_call.id,
                            stage = "utterance",
                            "turn: 純発話生成を配送（1 生成で完結・機械行なし）"
                        );
                    }
                    return Ok(EngineResult {
                        response: content.unwrap_or_default(),
                        iterations,
                        tool_calls_made: total_tool_calls,
                        stopped_by_limit: false,
                        explicit_termination: Some(crate::engine::ExplicitTermination::NoReply),
                        last_posting_utterance_id,
                        last_generation_had_continuation_speech,
                        xml_fallback_parses,
                    });
                }

                // Add the assistant message with tool calls (arguments already
                // canonical Strings, so no Value->String conversion needed).
                messages.push(Message {
                    role: Role::Assistant,
                    content: content.clone().map(MessageContent::Text),
                    name: None,
                    function_call: None,
                    tool_calls: Some(tool_calls.clone()),
                    tool_call_id: None,
                });
                turn_ledger.record(
                    format!("asst:{}", messages.len()),
                    content.as_deref().unwrap_or(""),
                );
                apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;

                // 発話クラス（reply/reaction/repost・§3.3.1 C6）の tool_call は**機械行を
                // 永続しない**。発話の本文は配送経路が speech ログとして残す（本文＋関係注記）
                // ので、ここで永続 tool_call 行から除外する。照会/道具クラスの call は従来どおり。
                let persisted: Vec<&ToolCall> = tool_calls
                    .iter()
                    .filter(|tc| !self.is_utterance_tool(&tc.function.name))
                    .collect();

                // 本文＋照会/道具クラスの生成本文は holding 発話として1件だけ配送・保存する。
                // `content` は上で終端markerを除去済み。配送後は on_tool_call へ本文を渡さず
                // 二重保存を避ける。
                let mut holding_delivered = false;
                if !persisted.is_empty() {
                    if let Some(body) = content.as_deref().filter(|body| !body.trim().is_empty()) {
                        if let Some(ref cb) = self.on_continuation_speech {
                            cb(body.to_string()).await.map_err(|e| {
                                anyhow::anyhow!("holding speech delivery failed: {e:#}")
                            })?;
                            holding_delivered = true;
                        }
                    }
                }

                // Notify on_tool_call callbacks.
                if !persisted.is_empty() && !self.on_tool_call.is_empty() {
                    let calls_json = serde_json::to_string(&persisted).unwrap_or_default();
                    // 配送済み holding は本文を渡さない（配送側が保存済み・二重保存回避）。未配送
                    // （フック無しレーン）は従来どおり content を渡して on_tool_call が保存する。
                    let assistant_content = if holding_delivered {
                        String::new()
                    } else {
                        content.clone().unwrap_or_default()
                    };
                    for cb in &self.on_tool_call {
                        cb(assistant_content.clone(), calls_json.clone());
                    }
                }

                // 自動 dispatch（RFC #152 S3a・非ブロック）のバッチ分割判定（#671）。
                //
                // **バッチ単位**で決める（tool_call 単位ではない）。同一 assistant
                // メッセージのツールは LLM が並べた順に依存し得る
                // （`write_file` → `execute_shell("cargo build")` / `add_allowed_command`
                // → `execute_shell`）。1 ツールの「dispatch 可」は
                // `is_action_allowed && should_dispatch`、それ以外（配送系・制御系・
                // 共有状態を書くツールなど非 dispatch 可、および未許可ツール）は「inline」。
                //
                // 分割規則:
                //  - 全部 dispatch 可 → **1 本の subtask** にまとめて逐次実行（順序保持・
                //    完了通知も 1 回 = 親の resume も 1 回）。
                //  - **先頭に inline 接頭辞、続く接尾辞が全部 dispatch 可** → 接頭辞を同期
                //    実行し、残りの接尾辞全体を 1 本の subtask として dispatch（#671）。
                //    接頭辞の完了後に接尾辞を dispatch し、接尾辞内は逐次実行のため
                //    バッチ内順序は保たれる。
                //  - **dispatch 可の後ろに inline ツールが来る** → 分割すると inline と
                //    background の相対順序が保証できないため**バッチ全体を inline 実行**
                //    （従来経路）。どのツールが縮退の原因かを debug ログに明示する。
                //  - dispatcher 未設定・全部 inline → 従来どおり全体 inline。
                //
                // `dispatch_start` は inline 接頭辞と dispatch 接尾辞の境界:
                //   Some(k) → inline [0,k) を同期実行、dispatch [k,len) を subtask 化
                //             （k==0 は全体 dispatch）。
                //   None    → 全体 inline。
                let dispatch_partition = partition_tool_calls_for_dispatch(
                    &tool_calls,
                    self.tool_dispatcher.as_deref(),
                    |tool_name| self.is_action_allowed(tool_name),
                );
                if !dispatch_partition.forced_inline.is_empty() {
                    // dispatch 可の後ろに inline ツール → 分割不可、全体 inline
                    // に縮退。縮退原因（first より後ろの inline ツール）を明示。
                    // 相関 ID（agent_id / session_id / turn_id）は #665 の span
                    // から継承する。
                    let forced: Vec<&str> = dispatch_partition
                        .forced_inline
                        .iter()
                        .map(|(_, tool_name)| *tool_name)
                        .collect();
                    tracing::debug!(
                        iteration = iterations,
                        stage = "batch_split",
                        tools = tool_calls.len(),
                        inline_tools = %forced.join(","),
                        "turn: 混在バッチが全体 inline に縮退（dispatch 可の後ろに inline ツール）"
                    );
                }
                let dispatch_start = dispatch_partition.dispatch_start;

                // inline 接頭辞と dispatch 接尾辞に分ける。dispatch_start==None は全体 inline
                // （接尾辞は空）。境界の順で実行するため接頭辞を先に走らせ、その後で接尾辞を
                // 1 本の subtask に dispatch する。
                let (inline_calls, dispatch_calls): (&[ToolCall], &[ToolCall]) =
                    match dispatch_start {
                        Some(k) => (&tool_calls[..k], &tool_calls[k..]),
                        None => (&tool_calls[..], &tool_calls[..0]),
                    };

                for tool_call in inline_calls {
                    total_tool_calls += 1;
                    let tool_name = &tool_call.function.name;

                    // #665: inline ツール実行の入り。この後の `execute_with_id(...).await` が返らなければ
                    // ここが最後の行になる（シェル・MCP・返信送信など外部待ちのツールで固着した形）。
                    tracing::debug!(
                        iteration = iterations,
                        tool = %tool_name,
                        id = %tool_call.id,
                        stage = "tool_call",
                        "turn: ツール実行 開始（入）"
                    );

                    // Check if the action is declared by active skills.
                    if !self.is_action_allowed(tool_name) {
                        let denied = Self::permission_denied(tool_name);
                        let result_json = serde_json::to_string(&denied)
                            .unwrap_or_else(|_| r#"{"error": "Permission denied"}"#.to_string());
                        messages.push(Message::tool(tool_call.id.clone(), result_json.clone()));
                        turn_ledger.record(format!("tool:{}", messages.len()), &result_json);
                        apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;

                        // Notify on_tool_result callbacks for denied action.
                        for cb in &self.on_tool_result {
                            cb(
                                tool_call.id.clone(),
                                tool_name.clone(),
                                result_json.clone(),
                                true,
                            );
                        }
                        continue;
                    }

                    // Canonical tool-call arguments are a JSON string; parse to a
                    // Value for the executor boundary (empty object on malformed).
                    let args = tool_call.arguments_json();

                    // 照会/道具と混在した発話クラス（§3.3.1 C3/C6）: inline 配送するが、
                    // subtask/settle/resume は起こさず、モデルへ領収書本文を返さない。次の LLM
                    // 呼び出しが不可避な混在時だけ、provider の tool_call/tool_result 対要求を
                    // データを持たない最小 ack で満たす（R7）。on_tool_result（永続機械行）は
                    // 呼ばず、本文は配送経路が speech として永続する（C6）。
                    if self.is_utterance_tool(tool_name) {
                        let result = self
                            .executor
                            .execute_with_id(tool_name, &args, &tool_call.id)
                            .await;
                        if tool_name == "reply" && result.success {
                            last_posting_utterance_id = Some(tool_call.id.clone());
                        }
                        // 最小 ack（データを持たない空オブジェクト・capping 不要）。成功/失敗を
                        // 名乗らない——失敗は say と同一経路で ❌/turn_failed に別途表面化する（C9）。
                        let ack = "{}".to_string();
                        messages.push(Message::tool(tool_call.id.clone(), ack.clone()));
                        turn_ledger.record(format!("tool:{}", messages.len()), &ack);
                        apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;
                        tracing::debug!(
                            iteration = iterations,
                            tool = %tool_name,
                            id = %tool_call.id,
                            stage = "utterance",
                            "turn: 発話クラスを配送（撃ちっぱなし・機械行なし）"
                        );
                        continue;
                    }

                    // ここを通るのは inline 実行対象（全体 inline、または混在バッチの
                    // inline 接頭辞）のみ。分割判定はバッチ単位でループ前に済んでいる（#671）。
                    let result = self
                        .executor
                        .execute_with_id(tool_name, &args, &tool_call.id)
                        .await;
                    // #665: inline ツール実行の出。入と対。success を載せ、この後 tool_result を積んで
                    // 次イテレーションへ回る。
                    tracing::debug!(
                        iteration = iterations,
                        tool = %tool_name,
                        id = %tool_call.id,
                        success = result.success,
                        stage = "tool_call",
                        "turn: ツール実行 完了（出）"
                    );

                    let result_json = serde_json::to_string(&result).unwrap_or_else(|_| {
                        r#"{"error": "Failed to serialize result"}"#.to_string()
                    });

                    // #284: LLM へ返す前に上限を効かせる。以降（messages / callback）は
                    // すべてこの capped 本文を使い、同ターンのプロンプトと DB に残る
                    // 本文を一致させる。
                    let result_json = seat_tool_result(
                        &mut turn_gov,
                        &mut turn_ledger,
                        &mut messages,
                        tool_name,
                        &result_json,
                        |remaining| {
                            self.cap_tool_result(
                                tool_name,
                                &tool_call.id,
                                result_json.clone(),
                                remaining,
                            )
                        },
                    )?;

                    messages.push(Message::tool(tool_call.id.clone(), result_json.clone()));
                    turn_ledger.record(format!("tool:{}", messages.len()), &result_json);
                    apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;

                    // Notify on_tool_result callbacks.
                    for cb in &self.on_tool_result {
                        cb(
                            tool_call.id.clone(),
                            tool_name.clone(),
                            result_json.clone(),
                            !result.success,
                        );
                    }
                }

                // dispatch 接尾辞（あれば）を 1 本の subtask にまとめて起動する。
                // inline 接頭辞の同期実行が終わった**後**にここへ来るため、順序保証は保たれる。
                // 各 tool_call には同じ subtask_id を持つ spawned マーカーを同ターンで返す。
                if !dispatch_calls.is_empty() {
                    let dispatcher = self
                        .tool_dispatcher
                        .as_ref()
                        .expect("dispatch_start is Some");
                    let calls: Vec<super::types::DispatchCall> = dispatch_calls
                        .iter()
                        .map(|tc| super::types::DispatchCall {
                            tool_name: tc.function.name.clone(),
                            args: tc.arguments_json(),
                            tool_call_id: tc.id.clone(),
                        })
                        .collect();
                    total_tool_calls += calls.len();
                    let outcome = dispatcher.dispatch_batch(&calls);
                    tracing::debug!(
                        tools = calls.len(),
                        subtask_id = %outcome.subtask_id,
                        "tool batch auto-dispatched as a single background subtask"
                    );
                    for tool_call in dispatch_calls {
                        let spawned = serde_json::json!({
                            "status": "spawned",
                            "subtask_id": outcome.subtask_id,
                            "tool": tool_call.function.name,
                            "label": outcome.label,
                        });
                        let result_json = serde_json::to_string(&spawned)
                            .unwrap_or_else(|_| r#"{"status":"spawned"}"#.to_string());
                        messages.push(Message::tool(tool_call.id.clone(), result_json.clone()));
                        turn_ledger.record(format!("tool:{}", messages.len()), &result_json);
                        apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;
                        for cb in &self.on_tool_result {
                            cb(
                                tool_call.id.clone(),
                                tool_call.function.name.clone(),
                                result_json.clone(),
                                false,
                            );
                        }
                    }
                }

                // 発話クラスのみで明示終了されていない生成に本文もある場合、reply 配送のあと・
                // 次イテレーション前に途中発話として配送・保存する。照会/道具が混じるときの本文は
                // holding の既存経路が担当する。
                if !termination_requested && !next_llm_call_needed {
                    if let Some(ref c) = content {
                        if !c.trim().is_empty() {
                            last_generation_had_continuation_speech = true;
                            if let Some(ref cb) = self.on_continuation_speech {
                                cb(c.clone()).await.map_err(|e| {
                                    anyhow::anyhow!("continuation speech delivery failed: {e:#}")
                                })?;
                            }
                        }
                    }
                }

                continue;
            }

            // tool call が無い本文も、NO_REPLY が無ければ途中発話として積んで次へ進む。
            if !termination_requested {
                if let Some(ref c) = content {
                    // #898 §12.2/§13.1 j: 剥がし後の途中発話を、次イテレーション前に**ループ中で
                    // 配送・保存する**（REST responses への追加 / extgate 途中発話配送 / memory_sessions
                    // speech 保存 / intake 保存）。配送が失敗したら継続を止めてターンを失敗させる
                    // （失敗を隠して次に進まない）。on_response_text は最終・text+tool でも発火する
                    // ため区別できず流用しない（最終二重配送・text+tool 二重保存を避ける）。
                    last_generation_had_continuation_speech = true;
                    if let Some(ref cb) = self.on_continuation_speech {
                        cb(c.clone()).await.map_err(|e| {
                            anyhow::anyhow!("continuation speech delivery failed: {e:#}")
                        })?;
                    }
                    messages.push(Message {
                        role: Role::Assistant,
                        content: Some(MessageContent::Text(c.clone())),
                        name: None,
                        function_call: None,
                        tool_calls: None,
                        tool_call_id: None,
                    });
                    turn_ledger.record(format!("asst:{}", messages.len()), c);
                    apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;
                }
                continue;
            }

            // NO_REPLY生成中に新着が届いていたら、終了より新着を優先する。completion sinkは
            // 親が実行中なら別turnを起動しないため、ここが終了境界での最後の受け渡し点になる。
            let late_inbound = self
                .live_inbound
                .as_ref()
                .map(|source| source.poll_new_with_origin())
                .unwrap_or_default();
            if !late_inbound.is_empty() {
                if let Some(ref speech) = content {
                    if !speech.trim().is_empty() {
                        last_generation_had_continuation_speech = true;
                        if let Some(ref cb) = self.on_continuation_speech {
                            cb(speech.clone()).await.map_err(|e| {
                                anyhow::anyhow!("continuation speech delivery failed: {e:#}")
                            })?;
                        }
                        messages.push(Message {
                            role: Role::Assistant,
                            content: Some(MessageContent::Text(speech.clone())),
                            name: None,
                            function_call: None,
                            tool_calls: None,
                            tool_call_id: None,
                        });
                        turn_ledger.record(format!("asst:{}", messages.len()), speech);
                    }
                }
                for folded in late_inbound {
                    let crate::FoldedInbound { text, origin } = folded;
                    messages.push(Message {
                        role: Role::User,
                        content: Some(MessageContent::Text(text.clone())),
                        name: None,
                        function_call: None,
                        tool_calls: None,
                        tool_call_id: None,
                    });
                    turn_ledger.record(format!("live:{}", messages.len()), &text);
                    if let Some(origin) = origin {
                        if !read_emitted_origins.contains(&origin)
                            && !pending_read_origins.contains(&origin)
                        {
                            pending_read_origins.push(origin);
                        }
                    }
                }
                apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;
                continue;
            }

            // No tool calls and no late inbound: this is the final response.
            let final_text = content.unwrap_or_default();

            tracing::warn!(
                iteration = iterations,
                text_len = final_text.len(),
                text_preview = %final_text.chars().take(100).collect::<String>(),
                "SkillEngine final response ready"
            );

            // ここに来る最終応答は本文がある（#706: content 欠落／空文字／空白のみで
            // tool_call も無いターンは上流の意味的検証で fail loud 済み。空応答が Ok として
            // 通る唯一の穴だった 787 はこれで塞がっている）。

            return Ok(EngineResult {
                response: final_text,
                iterations,
                tool_calls_made: total_tool_calls,
                stopped_by_limit: false,
                explicit_termination: Some(crate::engine::ExplicitTermination::NoReply),
                last_posting_utterance_id,
                last_generation_had_continuation_speech,
                xml_fallback_parses,
            });
        }
    }
}
