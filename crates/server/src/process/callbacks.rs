use super::*;

/// sub-engine のツール呼び出しを進捗テキストへ要約する（#175 S4）。
///
/// 旧 Discord 実装（`execute_spawn_subtask`）から移設。`{function:{name}}`（正準）と
/// `{name}`（旧形状）の両方に対応し、assistant 本文は先頭 500 文字だけ添える。
fn summarize_tool_calls(assistant_content: &str, tool_calls_json: &str) -> String {
    let mut names = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(tool_calls_json) {
        if let Some(calls) = value.as_array() {
            for call in calls {
                let name = call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .or_else(|| call.get("name").and_then(|v| v.as_str()));
                if let Some(name) = name {
                    names.push(format!("`{name}`"));
                }
            }
        }
    }
    let tools = if names.is_empty() {
        "tool call".to_string()
    } else {
        names.join(", ")
    };
    let preview: String = assistant_content.trim().chars().take(500).collect();
    if preview.is_empty() {
        format!("calling {tools}")
    } else {
        format!("calling {tools}\n{preview}")
    }
}
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
fn error_body_with_prompt_size(error_str: Option<&str>, prompt_json: &str) -> Option<String> {
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
    engine.set_log_callback(move |log: &LlmCallLog| {
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
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Ok(conn) = log_db.lock() {
            if let Err(e) = opencrab_db::queries::insert_llm_log(&conn, &log_row) {
                tracing::error!("Failed to insert llm_log: {e}");
            }
        }
    });
}

/// サブタスク走行の実況（#175 S4）の配線。ツール呼び出しと結果を進捗として通知口へ流す。
///
/// 購読していない（`wants_progress()` が false）ならフック自体を挿さず、要約の計算も
/// 省く（旧 `execute_spawn_subtask` と同じ判定）。
///
/// #397: ここと [`set_turn_log_callbacks`] は**同じ engine の同じフック**に載る。engine
/// 側が代入だった頃は、後から呼ばれる `set_turn_log_callbacks`（`persist_turn_logs` が
/// true のとき）がこの実況を丸ごと上書きして消していた。今は `add_on_tool_*` で足すので
/// 両方生き、配線の順序にも依存しない。
pub(super) fn set_run_notifier_callbacks(
    engine: &mut opencrab_core::SkillEngine,
    notifier: &std::sync::Arc<dyn opencrab_actions::SubtaskRunNotifier>,
    session_id: String,
) {
    if !notifier.wants_progress() {
        return;
    }
    let on_call = notifier.clone();
    engine.add_on_tool_call(move |assistant_content, tool_calls_json| {
        on_call.on_progress(&summarize_tool_calls(&assistant_content, &tool_calls_json));
    });
    let on_result = notifier.clone();
    engine.add_on_tool_result(move |tool_call_id, tool_name, result_json, is_error| {
        on_result.on_progress(&tool_result_progress_line(
            &tool_name,
            &result_json,
            is_error,
            &session_id,
            &tool_call_id,
        ));
    });
}

/// 実況として通知口へ流す 1 行を組む（ツール名・成否・結果のプレビュー）。
///
/// **無害化してからプレビューを切る**。実況は webhook で系の外へ出る経路なので、
/// 永続化側（[`set_turn_log_callbacks`]）と同じ `sanitize_tool_result_for_log` を通し、
/// nsec が生のまま 500 文字に混ざらないようにする。秘密が結果のどこに現れるかは
/// ツール次第で、先頭 500 文字を見て安全と判断はできないため、**切る前に**通す。
///
/// `workspace_root` は `None` を渡す。engine は callback より手前で `cap_tool_result` を
/// かけており、ここへ来る本文は上限内なので退避は起きない。仮に起きても実況が
/// ワークスペースへ書く必要はない（永続化側と二重に書くことになる）。
fn tool_result_progress_line(
    tool_name: &str,
    result_json: &str,
    is_error: bool,
    session_id: &str,
    tool_call_id: &str,
) -> String {
    let status = if is_error { "failed" } else { "completed" };
    let safe = opencrab_actions::sanitize_tool_result_for_log(
        tool_name,
        result_json,
        session_id,
        tool_call_id,
        None,
    );
    let preview: String = safe.chars().take(500).collect();
    format!("tool `{tool_name}` {status}\n{preview}")
}

/// ターンの tool_call / tool_result を session_logs に記録するコールバックの配線
/// （#33: 段の分解。tool_result はサイズ上限超過時にワークスペースへ退避）。
pub(super) fn set_turn_log_callbacks(
    engine: &mut opencrab_core::SkillEngine,
    db: opencrab_db::Db,
    agent_id: String,
    session_id: String,
    tool_result_workspace: std::path::PathBuf,
    // §9A.1 / row292: gateway 宣言 DI operation の tool_call は arguments を会話へ verbatim 保持
    // する（reply 本文が次ターンで消えない）。ここに名前が入る call は digest から除外する。
    // 名前は runtime の RunRequest.gateway_actions 由来で core に platform 語彙を持たない。
    di_op_names: std::collections::HashSet<String>,
) {
    {
        let tc_db = db.clone();
        let tc_agent = agent_id.clone();
        let tc_session = session_id.clone();
        engine.add_on_tool_call(move |content: String, tool_calls_json: String| {
            if let Ok(conn) = tc_db.lock() {
                // LLMがtext+tool_callsを同時に返した場合、textをspeechとして記録する。
                // #899 §12.6: 保存前に NO_REPLY 終端解釈（単一実装 visible_speech_after_markers）を
                // 通す。沈黙（前段が空）は監査行を残さない（残すと conversation_typed が次ターンの
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
                    if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &speech_log) {
                        tracing::error!(agent_id = %tc_agent, session_id = %tc_session, "Failed to insert speech log (with tool_call): {e}");
                    }
                }
                // 発話クラス（reply/reaction/repost・§3.3.1 C6）は engine が tool_calls_json から
                // 除外して渡す。除外の結果ツールが 1 つも残らない（空配列）ターンは、機械行
                // （空の tool_call ログ）を残さない。発話本文は配送経路が speech として永続する。
                let has_persistable_calls = serde_json::from_str::<serde_json::Value>(
                    &tool_calls_json,
                )
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
                                        di_op_names
                                            .contains(name)
                                            .then(|| id.to_string())
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
                if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
                    tracing::error!(agent_id = %tc_agent, session_id = %tc_session, "Failed to insert tool_call log: {e}");
                }
            }
        });
    }

    // on_tool_result callback: save tool_result to DB.
    {
        let tr_db = db;
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

                if let Ok(conn) = tr_db.lock() {
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
                    if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
                        tracing::error!(agent_id = %tr_agent, session_id = %tr_session, "Failed to insert tool_result log: {e}");
                    }
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

#[cfg(test)]
#[path = "tests/error_body_with_prompt_size.rs"]
mod error_body_with_prompt_size_tests;

#[cfg(test)]
#[path = "tests/tool_result_progress_line.rs"]
mod tool_result_progress_line_tests;
