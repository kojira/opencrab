use super::{
    completion_requests::{
        append_live_completions, assign_conversation_tool_ids, mark_completion_effect_applied,
        prepare_completion_request, recover_dispatch_effects, recover_tool_effect,
        register_tool_result_alternate,
    },
    request_builder::{append_live_inbound, build_chat_request, turn_state_digest},
    run_helpers::{
        initialize_turn, normalize_response, partition_tool_calls_for_dispatch,
        strip_continue_marker, InitialTurn,
    },
    turn_budget::{apply_turn_budget, seat_tool_result},
    SkillEngine,
};
use crate::engine::types::EngineResult;
use anyhow::Result;
use opencrab_llm_types::{Message, MessageContent, Role, ToolCall};

impl SkillEngine {
    /// Run the action loop with the given system context and user message.
    ///
    /// Returns the final text response from the LLM after all tool calls
    /// have been resolved.
    pub async fn run(
        &self,
        system_context: &str,
        user_message: &str,
        model: &str,
    ) -> Result<EngineResult> {
        self.run_with_model_override(system_context, user_message, model, None, &[])
            .await
    }

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
        // source側のwatermarkに加え、同じrun内でもevent IDを防御的に冪等化する。
        let mut folded_completion_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut pending_completion_event_ids: Vec<String> = Vec::new();
        // messages上のcompletion本文と、安全な永続参照版。requestごとにmeterで選ぶ。
        let mut completion_message_alternates: Vec<(usize, String)> = Vec::new();
        let mut total_tool_calls = 0;
        let mut xml_fallback_parses = 0;
        // #915: 各生成で最後に成功した投稿系 utterance-op の call_id。生成開始時にリセットし、
        // 上限到達時だけ直前（打ち切られた最終生成）の値を保持して返す。
        let mut last_posting_utterance_id: Option<String> = None;
        let mut last_generation_had_continuation_speech = false;
        // #975: 同じ会話状態での空CONTINUEは一度だけ再試行し、busy loopを止める。
        let mut last_empty_continue_state: Option<[u8; 32]> = None;
        let mut running_background_batches: usize = 0;

        loop {
            iterations += 1;

            if iterations > self.max_iterations {
                tracing::warn!(
                    iterations = iterations,
                    max = self.max_iterations,
                    "SkillEngine reached max iterations, stopping"
                );
                return Ok(EngineResult {
                    response: "I've reached the maximum number of steps for this task. Here's what I've done so far.".to_string(),
                    iterations,
                    tool_calls_made: total_tool_calls,
                    stopped_by_limit: true,
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
                append_live_inbound(
                    self,
                    &mut messages,
                    &mut turn_ledger,
                    &mut pending_read_origins,
                    &read_emitted_origins,
                );
            }
            // #975: turn開始後に決着したbackground toolの結果を、固定済みmessagesへ差分追加する。
            // provider実行中には割り込まず、次の反復のrequest構築直前だけで取り込む。
            append_live_completions(
                self,
                &mut messages,
                &mut turn_ledger,
                &mut folded_completion_ids,
                &mut pending_completion_event_ids,
                &mut completion_message_alternates,
                &mut running_background_batches,
            );
            apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;

            let recovered_effect = (iterations == 1)
                .then_some(self.live_tool_completions.as_ref())
                .flatten()
                .map(|source| source.recover_pending_effect())
                .transpose()
                .map_err(anyhow::Error::msg)?
                .flatten();

            // Check for dynamic model override.
            let mut model = model_override
                .as_ref()
                .and_then(|o| o.lock().ok().and_then(|m| m.clone()))
                .unwrap_or_else(|| default_model.to_string());

            // LLM呼び出し前を記録し、宙吊り時の最後の観測点にする。
            tracing::debug!(
                iteration = iterations,
                model = %model,
                messages = messages.len(),
                stage = "llm_call",
                "turn: LLM リクエスト 開始（入）"
            );

            // describe_toolsで活性化したツールを反映するため毎イテレーション取り直す。
            let tools = self.executor.list_tools();
            let request = build_chat_request(self, model.clone(), &messages, tools)?;
            let (request, completion_request_id, recovered_response) =
                if let Some((request_id, exact_request, response)) = recovered_effect {
                    (exact_request, Some(request_id), Some(response))
                } else {
                    let (request, request_id) = prepare_completion_request(
                        self,
                        request,
                        &mut messages,
                        &completion_message_alternates,
                        &pending_completion_event_ids,
                    )?;
                    (request, request_id, None)
                };
            model = request.model.clone();

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

            let (response, request_for_log, replayed_effect) = self
                .execute_exchange(
                    request,
                    &model,
                    iterations,
                    completion_request_id.as_deref(),
                    &mut pending_completion_event_ids,
                    recovered_response,
                )
                .await?;

            // 応答本文とツールコールをローカルに抽出（正準モデルは choices[0] を持つ）。
            let normalized = normalize_response(&response);
            let mut content = normalized.content;
            let mut tool_calls = normalized.tool_calls;
            assign_conversation_tool_ids(self, &mut tool_calls, replayed_effect)?;

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

            // #890 §11 / §11.7: content の最終行が CONTINUE 単独なら「このターンを続ける意思」と
            // みなし、その行を剥がして次イテレーションへ進む（継続を起こすのは text-only 経路のみ・
            // 下の最終応答分岐で `continue`）。ツール呼び出しと併記された場合はツール経路が優先し、
            // マーカーは剥がすだけ。NO_REPLY が同居する場合は NO_REPLY 優先で終端する（継続しない・
            // 剥がしは配送層が担う）。同一行併記・途中出現は継続もしない（WARN は配送層が出す）。
            // 剥がしは on_response_text 配送前・会話保存前に行う（§11.6: マーカーを残さない）。
            let (stripped_content, continue_requested) = strip_continue_marker(content);
            content = stripped_content;

            let empty_continue =
                continue_requested && content.as_deref().map(str::trim).unwrap_or("").is_empty();
            if empty_continue {
                let state = turn_state_digest(&request_for_log.messages)?;
                if last_empty_continue_state == Some(state) {
                    crate::continue_marker::warn_no_progress_continuation(iterations);
                    mark_completion_effect_applied(self, completion_request_id.as_deref())?;
                    if running_background_batches > 0 {
                        return Ok(EngineResult {
                            response: String::new(),
                            iterations,
                            tool_calls_made: total_tool_calls,
                            stopped_by_limit: false,
                            last_posting_utterance_id,
                            last_generation_had_continuation_speech,
                            xml_fallback_parses,
                        });
                    }
                    anyhow::bail!("no_progress_continuation: empty CONTINUE repeated without a changed turn state");
                }
                last_empty_continue_state = Some(state);
            } else {
                last_empty_continue_state = None;
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

            // If there are tool calls, execute them. A generation containing only allowed
            // utterance calls completes the turn without another LLM call; query/tool calls
            // still produce tool results and continue the loop.
            if !tool_calls.is_empty() {
                // 発話クラスだけで完結した生成は、普通の発話と同じく 1 生成で終了する（R7・
                // row360 / #880）。照会/道具、または permission denied の発話が 1 つでもあれば
                // provider の tool_call/tool_result 対を作って次の LLM 呼び出しへ進む。
                let next_llm_call_needed = tool_calls.iter().any(|tc| {
                    !self.is_utterance_tool(&tc.function.name)
                        || !self.is_action_allowed(&tc.function.name)
                });

                // #900: 純発話でも末尾 CONTINUE が併記されていれば、発話を配送してから次イテレー
                // ションへ進む（発話クラスのみ＋末尾 CONTINUE → 継続）。この場合は下の混在パスへ落とし、
                // 各発話を最小 ack で満たして次の LLM 呼び出しを起こす（本文＝マーカー剥がし済みの content）。
                if !next_llm_call_needed && !continue_requested {
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
                    mark_completion_effect_applied(self, completion_request_id.as_deref())?;
                    return Ok(EngineResult {
                        response: content.unwrap_or_default(),
                        iterations,
                        tool_calls_made: total_tool_calls,
                        stopped_by_limit: false,
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

                // #916 §13 #10: 本文＋照会/道具（query/dispatch）クラスの生成の本文は「宣言（holding）」。
                // 既存の中間発話配送フック（on_continuation_speech → 配送＋保存）で 1 件だけ配送・保存する。
                // 配送したら on_tool_call へは本文を渡さず二重保存を避ける（配送は保存と対）。フックが
                // 無いレーン（旧 discord は on_response_text で反復配送・core 単体テストは配送なし）は
                // 従来どおり on_tool_call が本文を保存する（挙動不変）。content は末尾 CONTINUE 剥がし済み。
                // 配送する holding 本文は NO_REPLY 終端解釈後の可視発言。判定は core 単一実装
                // terminate_at_no_reply().speech()（配送層 visible_speech_after_markers と同じ 1 実装・
                // 部分文字列の別判定を作らない・#916 レビュー）。content は末尾 CONTINUE 剥がし済み
                // （§11.6）なので visible_speech_after_markers（NO_REPLY→CONTINUE 剥がし）と同一結果。
                // 沈黙（可視本文なし・単独/行頭 NO_REPLY）は配送しない。
                let mut holding_delivered = false;
                if !persisted.is_empty() {
                    if let Some(c) = content.as_deref() {
                        let term = crate::continue_marker::terminate_at_no_reply(c);
                        if let Some(body) = term.speech().filter(|b| !b.trim().is_empty()) {
                            if let Some(ref cb) = self.on_continuation_speech {
                                cb(body.to_string()).await.map_err(|e| {
                                    anyhow::anyhow!("holding speech delivery failed: {e:#}")
                                })?;
                                holding_delivered = true;
                            }
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

                    // durable response replayでは、同じ短縮tool IDの結果が既に永続化済みなら
                    // executorを再度呼ばず、そのexact resultを会話へ戻す。
                    let recovered = recover_tool_effect(
                        self.live_tool_completions.as_deref(),
                        replayed_effect,
                        tool_call,
                    );
                    if let Some((result_json, _running)) = recovered {
                        messages.push(Message::tool(tool_call.id.clone(), result_json.clone()));
                        if self.model_input_limits_resolver.is_some() {
                            register_tool_result_alternate(
                                &messages,
                                &mut completion_message_alternates,
                                &tool_call.id,
                                tool_name,
                                &result_json,
                            );
                        }
                        turn_ledger.record(format!("tool:{}", messages.len()), &result_json);
                        apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;
                        continue;
                    }

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
                        if self.model_input_limits_resolver.is_some() {
                            register_tool_result_alternate(
                                &messages,
                                &mut completion_message_alternates,
                                &tool_call.id,
                                tool_name,
                                &result_json,
                            );
                        }
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
                    if self.model_input_limits_resolver.is_some() {
                        register_tool_result_alternate(
                            &messages,
                            &mut completion_message_alternates,
                            &tool_call.id,
                            tool_name,
                            &result_json,
                        );
                    }
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

                // dispatch接尾辞を、inline接頭辞の後に一つのsubtaskとして起動する。
                if !dispatch_calls.is_empty() {
                    let recovered_dispatch = recover_dispatch_effects(
                        self.live_tool_completions.as_deref(),
                        replayed_effect,
                        dispatch_calls,
                    );
                    if let Some(effects) = recovered_dispatch {
                        total_tool_calls += dispatch_calls.len();
                        for (tool_call, (result_json, running)) in
                            dispatch_calls.iter().zip(effects)
                        {
                            running_background_batches += usize::from(running);
                            messages.push(Message::tool(tool_call.id.clone(), result_json.clone()));
                            turn_ledger.record(format!("tool:{}", messages.len()), &result_json);
                            apply_turn_budget(&mut turn_gov, &mut turn_ledger, &mut messages, 0)?;
                        }
                    } else {
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
                        running_background_batches += 1;
                        dispatcher.defer_dispatch_start();
                        let outcome = dispatcher.dispatch_batch(&calls);
                        tracing::debug!(
                            tools = calls.len(),
                            subtask_id = %outcome.subtask_id,
                            "tool batch auto-dispatched as a single background subtask"
                        );
                        for tool_call in dispatch_calls {
                            let spawned = serde_json::json!({
                                "status": "spawned",
                                "subtask_id": outcome.subtask_id.clone(),
                                "tool": tool_call.function.name,
                                "label": outcome.label.clone(),
                            });
                            let result_json = serde_json::to_string(&spawned)
                                .unwrap_or_else(|_| r#"{"status":"spawned"}"#.to_string());
                            let running_for_model = format!(
                                "[<{}] status:running tool:{}",
                                tool_call.id, tool_call.function.name
                            );
                            messages.push(Message::tool(
                                tool_call.id.clone(),
                                running_for_model.clone(),
                            ));
                            turn_ledger
                                .record(format!("tool:{}", messages.len()), &running_for_model);
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
                        dispatcher.release_dispatch(&outcome.subtask_id);
                    }
                }

                // #898 §13 #8: 発話クラスのみ＋末尾 CONTINUE で継続するとき、併記された本文
                // （content・マーカー剥がし済み）を reply 配送のあと・次イテレーション前に、継続分岐と
                // 同じフックで配送・保存する（extgate 途中発話配送 / memory_sessions speech / REST
                // responses / intake 保存）。会話文脈は上の assistant メッセージ（tool_calls＋content）で
                // 積み済みなのでここでは配送・保存だけ。配送失敗（Err）は継続を止める（§13.1 j）。
                // 照会/道具が混じる（next_llm_call_needed）ときは本文 say を配送しない（holding は従来経路）。
                if continue_requested && !next_llm_call_needed {
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

                mark_completion_effect_applied(self, completion_request_id.as_deref())?;
                continue;
            }

            // #890 §11: 末尾 CONTINUE でこのターンを継続（ツール呼び出しが無い text-only 経路）。
            // 剥がし後の本文を assistant メッセージとして積み（マーカー除去済み・§11.6）、次イテレー
            // ションへ。本文が空（CONTINUE 単独）なら何も積まずに次イテレーションへ。上限は既存
            // max_iterations。
            if continue_requested {
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
                // continuation speechはgateway delivery ACKがoutboxを確定する。
                continue;
            }

            // No tool calls: this is the final response.
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

            // final textのoutbox適用は、outer delivery層の送信ACK後に確定する。
            return Ok(EngineResult {
                response: final_text,
                iterations,
                tool_calls_made: total_tool_calls,
                stopped_by_limit: false,
                last_posting_utterance_id,
                last_generation_had_continuation_speech,
                xml_fallback_parses,
            });
        }
    }
}
