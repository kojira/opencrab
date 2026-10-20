use super::*;

/// 失敗行の `error_body` に、`llm_logs.prompt` 列と**同じ全体シリアライズ**のサイズ
/// （文字数）を一様に付ける（#706）。空応答など「なぜ答えられなかったか」の行を見た人が、
/// 別列 `prompt` を辿らずに **1 クエリで「長さが原因か」の当たり**を付けられるようにする。
///
/// - `error_str` が `None`（＝成功行）のときは `None` を返し、**サイズを一切測らない**
///   （毎リクエストで 100 万文字規模を再走査しない）。
/// - `prompt_json` は呼び出し側が `prompt` 列用に既に持っている文字列を渡す前提
///   （追加のシリアライズはしない）。文字数は `llm_logs.prompt` と同一スケールなので、
///   運用者の実測帯（プロンプト全体で測った値）と直接比較できる。tool 定義・過去の
///   tool_call arguments も含まれる（本文のみだと過小になり読み違いを招く）。
/// - **閾値判定はしない**。事実（送ったサイズ）だけを残し、判断は読む人に委ねる。
/// - error_code に依らず失敗行へ一様に付くので、**新しい失敗種別を足しても自動で載る**
///   （種別ごとの手書き補間を engine 側に散らさない）。
pub(super) fn error_body_with_prompt_size(
    error_str: Option<&str>,
    prompt_json: &str,
) -> Option<String> {
    error_str.map(|body| {
        let prompt_chars = prompt_json.chars().count();
        format!(
            "{body} [prompt_chars={prompt_chars}（llm_logs.prompt と同じ全体シリアライズの\
             文字数。provider の usage は当てにならないため実測。閾値判定なし）]"
        )
    })
}

/// LLM 呼び出しログ（llm_logs テーブル）記録コールバックの配線（#33: 段の分解）。
pub(super) fn set_llm_log_callback(
    engine: &mut opencrab_core::SkillEngine,
    log_db: opencrab_db::Db,
    log_agent_id: String,
    log_session_id: String,
    log_trigger_message_id: Option<String>,
) {
    engine.set_exchange_log_callback(move |exchange: &opencrab_core::LlmExchangeLog| {
        let log = &exchange.call;
        let (prompt_tokens, completion_tokens, total_tokens) = log
            .response
            .as_ref()
            .map(|r| &r.usage)
            .map(|u| {
                (
                    Some(u.prompt_tokens as i64),
                    Some(u.completion_tokens as i64),
                    Some(u.total_tokens as i64),
                )
            })
            .unwrap_or((None, None, None));

        let cache_read_tokens = log
            .response
            .as_ref()
            .map(|r| &r.usage)
            .map(|u| u.cache_read_input_tokens as i64);
        let cache_creation_tokens = log
            .response
            .as_ref()
            .map(|r| &r.usage)
            .map(|u| u.cache_creation_input_tokens as i64);

        if let Some(text) = log
            .response
            .as_ref()
            .and_then(|response| response.first_message())
            .and_then(|message| message.text_content())
        {
            let termination = opencrab_actions::terminate_at_no_reply(text);
            opencrab_actions::no_reply::log_trailing_discard(
                &termination,
                opencrab_actions::DeliveryContext {
                    session_id: &log_session_id,
                    agent_id: &log_agent_id,
                    origin: "llm_response",
                },
            );
        }

        let response_str = log
            .response
            .as_ref()
            .map(|r| serde_json::to_string(r).unwrap_or_default())
            .unwrap_or_default();

        // #706: リクエスト全体のシリアライズは prompt 列用に元々ここで走る。空応答など
        // 失敗行の原因（プロンプト長）の当たり付けに、この**同じ**文字列のサイズを使い回す
        // （追加のシリアライズも、成功行での再走査もしない。error_body_with_prompt_size 参照）。
        let prompt_json = serde_json::to_string(&log.request).unwrap_or_default();

        let log_row = opencrab_db::queries::LlmLogRow {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: log_agent_id.clone(),
            session_id: Some(log_session_id.clone()),
            model: Some(log.request.model.clone()),
            prompt: prompt_json.clone(),
            response: response_str,
            tool_calls: log
                .response
                .as_ref()
                .and_then(|r| r.first_message())
                .and_then(|m| m.tool_calls.as_ref())
                .filter(|tc| !tc.is_empty())
                .and_then(|tc| serde_json::to_string(tc).ok()),
            latency_ms: Some(log.latency_ms),
            prompt_tokens,
            completion_tokens,
            total_tokens,
            // #706 / #676 / #539: error_code の判定は engine 側で一元化済み
            // （transport error / context 超過 / 空応答 / 出力上限切り捨て）。ここは
            // その値を写すだけ——文字列一致を process 側で再実装しない（判断は core、
            // ゲート/writer は配送）。
            error_code: log.error_code.clone(),
            error_body: error_body_with_prompt_size(log.error_str.as_deref(), &prompt_json),
            requested_at: Some(log.requested_at.clone()),
            trigger_message_id: log_trigger_message_id.clone(),
            is_bot_iteration: log.is_bot_iteration,
            cache_read_tokens,
            cache_creation_tokens,
            provider_tool_history: serde_json::to_string(&exchange.provider_tool_history)
                .unwrap_or_else(|_| "{}".to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Ok(conn) = log_db.lock() {
            if let Err(e) = opencrab_db::queries::insert_llm_log(&conn, &log_row) {
                tracing::error!("Failed to insert llm_log: {e}");
            }
        }
    });
}

/// Where the turn-log callbacks write their rows.
///
/// D-1085 §12: a provisional run holds its rows until its promotion gate opens. Each row waits
/// as a hook of the gate, so the promotion writes them (after the said's own rows) in the order
/// they were made, interleaved with the other hooks of the run (§13.2: its continuation speech);
/// once promoted, rows go straight to the DB. An aborted gate drops them.
#[derive(Clone)]
pub(super) struct TurnLogWriter {
    db: opencrab_db::Db,
    gate: Option<opencrab_core::PromotionGate>,
}

impl TurnLogWriter {
    pub(super) fn direct(db: opencrab_db::Db) -> Self {
        Self { db, gate: None }
    }

    pub(super) fn until_promoted(db: opencrab_db::Db, gate: &opencrab_core::PromotionGate) -> Self {
        Self {
            db,
            gate: Some(gate.clone()),
        }
    }

    pub(super) fn write(&self, row: opencrab_db::queries::SessionLogRow, what: &str) {
        match &self.gate {
            Some(gate) => {
                let writer = self.clone();
                let what = what.to_string();
                gate.on_promoted(move || writer.insert(&row, &what));
            }
            None => self.insert(&row, what),
        }
    }

    fn insert(&self, row: &opencrab_db::queries::SessionLogRow, what: &str) {
        if let Ok(conn) = self.db.lock() {
            if let Err(e) = opencrab_db::queries::insert_session_log(&conn, row) {
                tracing::error!(agent_id = %row.agent_id, session_id = %row.session_id, "Failed to insert {what}: {e}");
            }
        }
    }
}

/// ターンの tool_call / tool_result を session_logs に記録するコールバックの配線
/// （#33: 段の分解。tool_result はサイズ上限超過時にワークスペースへ退避）。
pub(super) fn set_turn_log_callbacks(
    engine: &mut opencrab_core::SkillEngine,
    writer: TurnLogWriter,
    agent_id: String,
    session_id: String,
    tool_result_workspace: std::path::PathBuf,
    // §9A.1 / row292: gateway 宣言 DI operation の tool_call は arguments を会話へ verbatim 保持
    // する（reply 本文が次ターンで消えない）。ここに名前が入る call は digest から除外する。
    // 名前は runtime の RunRequest.gateway_actions 由来で core に platform 語彙を持たない。
    di_op_names: std::collections::HashSet<String>,
) {
    {
        let tc_writer = writer.clone();
        let tc_agent = agent_id.clone();
        let tc_session = session_id.clone();
        engine.add_on_tool_call(move |content: String, tool_calls_json: String| {
            {
                // LLMがtext+tool_callsを同時に返した場合、textをspeechとして記録する。
                // #899 §12.6: 保存前に NO_REPLY 終端解釈（単一実装 visible_speech_after_markers）を
                // typed 履歴へ assistant 'NO_REPLY' として再注入する）。
                // ツールのみ生成（content 空）は `Some("")` になるため、旧 `!content.trim().is_empty()`
                // と同じく空/空白を弾く（空 speech 行＝typed の空 assistant を作らない）。
                let visible = opencrab_actions::visible_speech_after_markers(
                    &content,
                    opencrab_actions::DeliveryContext {
                        session_id: &tc_session,
                        agent_id: &tc_agent,
                        origin: "engine",
                    },
                )
                .filter(|body| !body.trim().is_empty());
                if let Some(body) = visible {
                    let speech_log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: tc_agent.clone(),
                        session_id: tc_session.clone(),
                        log_type: "speech".to_string(),
                        content: body,
                        speaker_id: Some(tc_agent.clone()),
                        turn_number: None,
                        metadata_json: None,
                        created_at: None,
                    };
                    tc_writer.write(speech_log, "speech log (with tool_call)");
                }
                // 発話クラス（reply/reaction/repost・§3.3.1 C6）は engine が tool_calls_json から
                // 除外して渡す。除外の結果ツールが 1 つも残らない（空配列）ターンは、機械行
                // （空の tool_call ログ）を残さない。発話本文は配送経路が speech として永続する。
                let has_persistable_calls =
                    serde_json::from_str::<serde_json::Value>(&tool_calls_json)
                        .ok()
                        .and_then(|v| v.as_array().map(|a| !a.is_empty()))
                        .unwrap_or(!tool_calls_json.trim().is_empty());
                if !has_persistable_calls {
                    return;
                }
                // DI operation の call id を記録し、会話再構成で arguments を digest 除外する。
                let preserve_ids: Vec<String> =
                    serde_json::from_str::<serde_json::Value>(&tool_calls_json)
                        .ok()
                        .and_then(|v| {
                            v.as_array().map(|items| {
                                items
                                    .iter()
                                    .filter_map(|it| {
                                        let id = it.get("id")?.as_str()?;
                                        let name = it
                                            .get("function")
                                            .and_then(|f| f.get("name"))
                                            .or_else(|| it.get("name"))
                                            .and_then(|n| n.as_str())?;
                                        di_op_names.contains(name).then(|| id.to_string())
                                    })
                                    .collect()
                            })
                        })
                        .unwrap_or_default();
                let metadata = if preserve_ids.is_empty() {
                    serde_json::json!({ "tool_calls_json": tool_calls_json })
                } else {
                    serde_json::json!({
                        "tool_calls_json": tool_calls_json,
                        "preserve_arg_call_ids": preserve_ids,
                    })
                };
                let log = opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: tc_agent.clone(),
                    session_id: tc_session.clone(),
                    log_type: "tool_call".to_string(),
                    content,
                    speaker_id: Some(tc_agent.clone()),
                    turn_number: None,
                    metadata_json: Some(metadata.to_string()),
                    created_at: None,
                };
                tc_writer.write(log, "tool_call log");
            }
        });
    }

    // on_tool_result callback: save tool_result to DB.
    {
        let tr_writer = writer;
        let tr_agent = agent_id;
        let tr_session = session_id;
        let tr_workspace = tool_result_workspace;
        engine.add_on_tool_result(
            move |tool_call_id: String, tool_name: String, result_json: String, is_error: bool| {
                // 永続化前の無害化（秘密フィールドのマスク ＋ サイズ上限/ワークスペース
                // 退避）は background dispatch 経路（`SubtaskToolDispatcher` →
                // `settle_completed`）と**共通の関数**を使う。片方だけ素通りすると、
                // 巨大結果や秘密鍵がそのまま session_logs に入り、次ターンの会話
                // 再構築へ再注入される。
                let content = opencrab_actions::sanitize_tool_result_for_log(
                    &tool_name,
                    &result_json,
                    &tr_session,
                    &tool_call_id,
                    Some(tr_workspace.as_path()),
                );

                {
                    let log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: tr_agent.clone(),
                        session_id: tr_session.clone(),
                        log_type: "tool_result".to_string(),
                        content,
                        speaker_id: Some(tr_agent.clone()),
                        turn_number: None,
                        metadata_json: Some(
                            serde_json::json!({
                                "tool_call_id": tool_call_id,
                                "tool_name": tool_name,
                                "is_error": is_error,
                            })
                            .to_string(),
                        ),
                        created_at: None,
                    };
                    tr_writer.write(log, "tool_result log");
                }
            },
        );
    }
}

/// 引数の image_urls と、直近ユーザーログ metadata の image_urls をマージする
/// （#33: 段の分解）。
pub(super) fn merge_image_urls(
    state: &AppState,
    session_id: &str,
    agent_id: &str,
    base: &[String],
) -> Vec<String> {
    {
        let mut urls: Vec<String> = base.to_vec();
        if let Ok(conn) = state.db.lock() {
            if let Ok(logs) = opencrab_db::queries::list_session_logs_by_session(&conn, session_id)
            {
                if let Some(latest_user_log) = logs.iter().rev().find(|log| {
                    log.log_type == "speech"
                        && log
                            .speaker_id
                            .as_deref()
                            .map(|s| s != agent_id)
                            .unwrap_or(true)
                }) {
                    if let Some(ref meta_json) = latest_user_log.metadata_json {
                        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_json) {
                            if let Some(arr) = meta["image_urls"].as_array() {
                                for v in arr {
                                    if let Some(s) = v.as_str() {
                                        let url = s.to_string();
                                        if !urls.contains(&url) {
                                            urls.push(url);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // #272 P1: 画像がどのターンで LLM に載ったかを後追いできるよう INFO に上げる。
        // 署名付き URL は秘匿情報になりうるので件数のみ出す。
        if !urls.is_empty() {
            tracing::info!(
                session_id = %session_id,
                count = urls.len(),
                "run_agent_response: merging image_urls for LLM"
            );
        }
        urls
    }
}

/// 受信画像 URL を取得・縮小して data URL にそろえる（D-1060）。取得できなかった
/// URL は添付せず（URL のまま流さない）、理由を本文末尾へ足す注記として返す。
pub(super) async fn normalize_image_urls(urls: &[String]) -> (Vec<String>, String) {
    let mut data_urls = Vec::with_capacity(urls.len());
    let mut note = String::new();
    for url in urls {
        match opencrab_llm::image_input::fetch_image_data_url(url).await {
            Ok(img) => data_urls.push(img.data_url),
            Err(e) => {
                tracing::warn!(error = %e, url = %url, "受信画像を取得できなかったので添付しない");
                note.push_str(&format!("\n[画像を取得できなかった: {url} ({e})]"));
            }
        }
    }
    (data_urls, note)
}
