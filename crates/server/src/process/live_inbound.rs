use super::*;

/// 1 回の poll で注入する新着発言の上限（#289）。
///
/// 溢れた分は捨てない。watermark は返した行まで進むので、次のイテレーションで続きが
/// 拾われる。上限は「1 イテレーションでプロンプトが跳ねない」ための安全弁である。
const LIVE_INBOUND_POLL_LIMIT: usize = 20;

/// 走行中のターンへ新着ユーザー発言を届ける実体（#289）。
///
/// 会話履歴はターン開始時に 1 度だけ組まれるので、ツール往復が長引く間に届いた発言は
/// 次ターンまでエンジンから見えなかった。この実体はエンジンのループから毎イテレーション
/// 引かれ、**前回以降の差分だけ**を返す。
///
/// 重複注入の防止は `watermark`（取得済みの最大 log id）で行う。id は単調増加なので、
/// 一度返した行が再び返ることはない。初期値は**会話文字列を組んだ後**の最大 id
/// （＝この時点までの発言は履歴側に載っている）。
pub(super) struct SessionLiveInbound {
    db: opencrab_db::Db,
    session_id: String,
    /// 応答するエージェント。`speaker_id != agent_id` が「自分以外の発言」の述語で、
    /// DB 側（`list_user_speech_logs_after`）と同じ比較をする（#286 の注意書き参照）。
    agent_id: String,
    /// 取得済みの最大 log id。これより後の行だけを次回返す。
    watermark: std::sync::atomic::AtomicI64,
    /// 注入の対象範囲（#323 / B2）。Nostr だけが相手を絞る（既定は全ての他者）。
    scope: opencrab_actions::LiveInboundScope,
}

impl SessionLiveInbound {
    /// 現在の最新 log id を watermark の初期値として組み立てる。
    ///
    /// 取得に失敗した場合は `i64::MAX` を置く（＝何も注入しない）。走行中の注入は
    /// あくまで改善であって、失敗しても既存のターンを壊さないことを優先する。
    pub(super) fn new(db: opencrab_db::Db, session_id: &str, agent_id: &str) -> Self {
        let latest = match db.lock() {
            Ok(conn) => opencrab_db::queries::list_recent_session_logs(&conn, session_id, 1)
                .ok()
                .and_then(|rows| rows.first().and_then(|l| l.id))
                .unwrap_or(0),
            Err(e) => {
                tracing::warn!(session_id = %session_id, "live inbound watermark unavailable: {e}");
                i64::MAX
            }
        };
        Self {
            db,
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            watermark: std::sync::atomic::AtomicI64::new(latest),
            scope: opencrab_actions::LiveInboundScope::AllOthers,
        }
    }

    /// 注入の対象範囲を差し替える（#323 / B2）。既定（`AllOthers`）は Discord / heartbeat
    /// の従来挙動。Nostr は inbound で `OnlySpeaker`、resume で `Silent` を渡す。
    pub(super) fn with_scope(mut self, scope: opencrab_actions::LiveInboundScope) -> Self {
        self.scope = scope;
        self
    }
}

impl opencrab_core::LiveInboundSource for SessionLiveInbound {
    fn poll_new_messages(&self) -> Vec<String> {
        // origin つき経路へ委譲し、本文だけ取り出す（watermark 前進は 1 経路に集約）。
        self.poll_new_with_origin()
            .into_iter()
            .map(|f| f.text)
            .collect()
    }

    /// #930: 新着を origin つきで返す。各行の `metadata_json.external_origin`（record_inbound が
    /// 保存）を read（👀）の付け先として載せる。origin が無い行（system 由来等）は None。
    fn poll_new_with_origin(&self) -> Vec<opencrab_core::FoldedInbound> {
        use std::sync::atomic::Ordering;

        // 対象範囲（#323 / B2）。Silent は相手が不定なので DB を引くまでもなく空。
        let only_speaker = match &self.scope {
            opencrab_actions::LiveInboundScope::AllOthers => None,
            opencrab_actions::LiveInboundScope::OnlySpeaker(pk) => Some(pk.as_str()),
            opencrab_actions::LiveInboundScope::Silent => return Vec::new(),
        };

        let after_id = self.watermark.load(Ordering::Relaxed);
        let conn = match self.db.lock() {
            Ok(conn) => conn,
            // ロックが取れないだけでターンを落とさない（次のイテレーションで拾える）。
            Err(_) => return Vec::new(),
        };
        let rows = match opencrab_db::queries::list_user_speech_logs_after(
            &conn,
            &self.session_id,
            &self.agent_id,
            after_id,
            only_speaker,
            LIVE_INBOUND_POLL_LIMIT,
        ) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(session_id = %self.session_id, "live inbound poll failed: {e}");
                return Vec::new();
            }
        };
        drop(conn);

        if rows.is_empty() {
            return Vec::new();
        }
        // 返した行まで watermark を進める（＝同じ発言を二度注入しない）。
        if let Some(max_id) = rows.iter().filter_map(|r| r.id).max() {
            self.watermark.store(max_id, Ordering::Relaxed);
        }
        rows.iter()
            .map(|row| opencrab_core::FoldedInbound {
                text: format_live_inbound(row),
                origin: external_origin_of(row),
            })
            .collect()
    }
}

/// #930: session log 行の `metadata_json.external_origin`（record_inbound が保存する発端 origin）
/// を取り出す。read（👀）の付け先。metadata が無い / JSON 解析失敗 / field 無しは None。
fn external_origin_of(log: &opencrab_db::queries::SessionLogRow) -> Option<String> {
    let meta = log.metadata_json.as_deref()?;
    let value: serde_json::Value = serde_json::from_str(meta).ok()?;
    value
        .get("external_origin")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// 走行中に届いた発言を LLM へ見せる形に整える（#289）。
///
/// 本文の整形は履歴と同じ [`format_single_log`] を使い、走行中に届いたという**事実**
/// だけを 1 行足す。ここに「必ず返せ」等の指示は書かない — 届けるのが仕事であって、
/// 応答するかどうかはエージェントの判断に委ねる。
fn format_live_inbound(log: &opencrab_db::queries::SessionLogRow) -> String {
    format!(
        "[新着メッセージ: あなたがこのターンを処理している間に届きました]\n{}",
        format_single_log(log)
    )
}

/// #975: 会話構築後に到着したbackground tool完了を同じturnへ差分注入する。
pub(super) struct SessionLiveToolCompletions {
    db: opencrab_db::Db,
    session_id: String,
}

impl SessionLiveToolCompletions {
    pub(super) fn new(db: opencrab_db::Db, session_id: &str) -> Self {
        Self {
            db,
            session_id: session_id.to_string(),
        }
    }
}

impl opencrab_core::LiveToolCompletionSource for SessionLiveToolCompletions {
    fn poll_tool_completions(&self) -> Vec<opencrab_core::FoldedToolCompletion> {
        let conn = match self.db.lock() {
            Ok(conn) => conn,
            Err(_) => return Vec::new(),
        };
        let events = match opencrab_db::queries::list_unconsumed_tool_completion_events(
            &conn,
            &self.session_id,
        ) {
            Ok(events) => events,
            Err(error) => {
                tracing::warn!(session_id = %self.session_id, "tool completion poll failed: {error}");
                return Vec::new();
            }
        };
        // crash前のincluded requestを最優先でexact replayする。後着queued eventは
        // 混ぜずに次のLLM stepまで保持する。
        let included_request_id = events.iter().find_map(|event| {
            (event.state == opencrab_db::queries::ToolCompletionState::Included)
                .then(|| event.included_request_id.clone())
                .flatten()
        });
        events
            .into_iter()
            .filter(|event| {
                included_request_id
                    .as_ref()
                    .is_none_or(|request_id| event.included_request_id.as_ref() == Some(request_id))
            })
            .filter_map(|event| {
                let mut row =
                    opencrab_db::queries::get_session_log_by_id(&conn, event.result_log_id)
                        .ok()
                        .flatten()?;
                let mut event_metadata = row
                    .metadata_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                    .filter(serde_json::Value::is_object)
                    .unwrap_or_else(|| serde_json::json!({}));
                let fields = event_metadata
                    .as_object_mut()
                    .expect("object checked above");
                fields.insert(
                    "conversation_tool_id".to_string(),
                    serde_json::json!(event.tool_call_id),
                );
                fields.insert(
                    "conversation_tool_ids".to_string(),
                    serde_json::json!([event.tool_call_id]),
                );
                row.metadata_json = Some(event_metadata.to_string());
                let text = opencrab_core::conversation::format_single_log(&row);
                let mut omitted_row = row.clone();
                let mut metadata = omitted_row
                    .metadata_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                    .filter(serde_json::Value::is_object)
                    .unwrap_or_else(|| serde_json::json!({}));
                let fields = metadata.as_object_mut().expect("object checked above");
                fields.insert("result_omitted".to_string(), serde_json::json!(true));
                if fields
                    .get("result_path")
                    .is_none_or(|value| value.is_null())
                {
                    fields.insert(
                        "result_path".to_string(),
                        serde_json::json!(format!(
                            "read_my_history(around_id={})",
                            event.result_log_id
                        )),
                    );
                }
                omitted_row.metadata_json = Some(metadata.to_string());
                Some(opencrab_core::FoldedToolCompletion {
                    event_id: event.event_id,
                    text,
                    omitted_text: opencrab_core::conversation::format_single_log(&omitted_row),
                })
            })
            .collect()
    }

    fn recover_pending_effect(
        &self,
    ) -> Result<
        Option<(
            String,
            opencrab_llm::ChatRequest,
            opencrab_llm::ChatResponse,
        )>,
        String,
    > {
        let conn = self.db.lock().map_err(|error| error.to_string())?;
        opencrab_db::queries::load_pending_tool_completion_effect(&conn, &self.session_id)
            .map_err(|error| error.to_string())?
            .map(|(request_id, request_json, response_json)| {
                let request = serde_json::from_str(&request_json).map_err(|e| e.to_string())?;
                let response = serde_json::from_str(&response_json).map_err(|e| e.to_string())?;
                Ok((request_id, request, response))
            })
            .transpose()
    }

    fn recover_tool_effect(
        &self,
        tool_call_id: &str,
    ) -> Option<opencrab_core::RecoveredToolEffect> {
        let conn = self.db.lock().ok()?;
        let (content, is_error, lifecycle_status) =
            opencrab_db::queries::load_persisted_tool_effect(&conn, &self.session_id, tool_call_id)
                .ok()??;
        Some(opencrab_core::RecoveredToolEffect {
            content,
            is_error,
            lifecycle_status,
        })
    }

    fn resolve_conversation_tool_id(&self, provider_call_id: &str) -> Option<String> {
        let conn = self.db.lock().ok()?;
        opencrab_db::queries::resolve_tool_call_correlation(
            &conn,
            &self.session_id,
            provider_call_id,
        )
        .ok()?
    }

    fn mark_effect_applied(&self, request_id: &str) -> Result<(), String> {
        let conn = self.db.lock().map_err(|error| error.to_string())?;
        opencrab_db::queries::mark_tool_completion_effect_applied(&conn, request_id)
            .map_err(|error| error.to_string())
    }

    fn recover_included_request(
        &self,
        event_ids: &[String],
    ) -> Result<Option<(String, opencrab_llm::ChatRequest)>, String> {
        let conn = self.db.lock().map_err(|error| error.to_string())?;
        opencrab_db::queries::load_included_tool_completion_request(&conn, event_ids)
            .map_err(|error| error.to_string())?
            .map(|(request_id, request_json)| {
                serde_json::from_str(&request_json)
                    .map(|request| (request_id, request))
                    .map_err(|error| error.to_string())
            })
            .transpose()
    }

    fn mark_included(
        &self,
        event_ids: &[String],
        request_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> Result<(), String> {
        let conn = self.db.lock().map_err(|error| error.to_string())?;
        opencrab_db::queries::mark_tool_completion_events_included(
            &conn,
            event_ids,
            request_id,
            request_digest,
            request_json,
        )
        .map_err(|error| error.to_string())
    }

    fn mark_consumed(&self, event_ids: &[String], request_id: &str) -> Result<(), String> {
        let conn = self.db.lock().map_err(|error| error.to_string())?;
        opencrab_db::queries::mark_tool_completion_events_consumed(&conn, event_ids, request_id)
            .map_err(|error| error.to_string())
    }
}

/// 走行中サブタスクへ届いた steer（追加指示）を反復の合間に注入する実体（#647）。
///
/// サブタスクは `run_agent_response` を depth+1 で再入し、親ターンと同じ engine ループ
/// （毎イテレーション `LiveInboundSource::poll_new_messages` を引く / #289）を通る。steer は
/// この既存機構をそのままサブへ通したもので、`SessionLiveInbound`（ユーザー発話版）の
/// steer 版にあたる。sub-session（`subtask-{id}`）に `steer_subtask` が積んだ
/// `log_type='steer'` の行だけを watermark 差分で読み、次の LLM 呼び出しへ user メッセージ
/// として足す。
///
/// 差分（`SessionLiveInbound` との違い）:
/// - 対象 `log_type` は `speech` ではなく `steer`（`STEER_LOG_TYPE`）。発話者フィルタは無い
///   （steer は親/オーナーの明示指示であり、送り主は認可済み）。
/// - depth>0（サブタスク）で配線する。親ターン（depth==0）は従来どおり `SessionLiveInbound`。
///
/// 重複注入防止は `watermark`（取得済み最大 log id）。初期値は engine 起動時点の最新 id
/// なので、以後に届いた steer だけが注入される。
pub(super) struct SubtaskSteerInbound {
    db: opencrab_db::Db,
    /// サブタスク自身のセッション ID（`subtask-{id}`）。steer はここへ積まれる。
    sub_session_id: String,
    /// 取得済みの最大 log id。これより後の steer 行だけを次回返す。
    watermark: std::sync::atomic::AtomicI64,
}

impl SubtaskSteerInbound {
    /// watermark 初期値を **0（セッション先頭）** にして組み立てる。
    ///
    /// `SessionLiveInbound`（親ターン用）は「起動時点の最新 id」で初期化する。あちらは
    /// 会話履歴をターン開始時に 1 度組んでおり、既に履歴へ載った過去発言を二重注入しない
    /// ためにその値が要る。だが steer の宛先は **spawn したばかりの新規 sub-session**で、
    /// engine が動き出す前に steer が積まれることは無い（過去の steer が存在しない）。
    /// にもかかわらず「最新 id」で初期化すると、spawn 直後〜この `new()` までの窓に届いた
    /// steer を取りこぼす（`steer_subtask` は Accepted を返したのに読まれない）。リプレイの
    /// 心配が無い場所なので 0 から読む方が正しく、「Accepted なのに読まれない」を settle
    /// race（doc 明記済みの許容窓）だけに絞れる。
    pub(super) fn new(db: opencrab_db::Db, sub_session_id: &str) -> Self {
        Self {
            db,
            sub_session_id: sub_session_id.to_string(),
            watermark: std::sync::atomic::AtomicI64::new(0),
        }
    }
}

impl opencrab_core::LiveInboundSource for SubtaskSteerInbound {
    fn poll_new_messages(&self) -> Vec<String> {
        use std::sync::atomic::Ordering;

        let after_id = self.watermark.load(Ordering::Relaxed);
        let conn = match self.db.lock() {
            Ok(conn) => conn,
            // ロックが取れないだけで反復を落とさない（次のイテレーションで拾える）。
            Err(_) => return Vec::new(),
        };
        let rows = match opencrab_db::queries::list_steer_logs_after(
            &conn,
            &self.sub_session_id,
            after_id,
            LIVE_INBOUND_POLL_LIMIT,
        ) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(session_id = %self.sub_session_id, "steer inbound poll failed: {e}");
                return Vec::new();
            }
        };
        drop(conn);

        if rows.is_empty() {
            return Vec::new();
        }
        // 返した行まで watermark を進める（＝同じ steer を二度注入しない）。
        if let Some(max_id) = rows.iter().filter_map(|r| r.id).max() {
            self.watermark.store(max_id, Ordering::Relaxed);
        }
        rows.iter()
            .map(|r| format_steer_inbound(&r.content))
            .collect()
    }
}

/// 走行中サブへ届いた steer を LLM へ見せる形に整える（#647）。
///
/// 親/オーナーからの**明示の追加指示**であることを明記し、受領/反映を親へ返すよう促す。
/// ただし tool 呼び出しを system レベルで強制はしない（`SessionLiveInbound` と同じ「足すだけ・
/// 応答は判断に委ねる」方針 / #288。steer は指示の性質が強いので促し文言を添える点だけが差）。
fn format_steer_inbound(message: &str) -> String {
    format!(
        "[追加指示 (steer): 親/オーナーからの指示が、あなたがこのタスクを実行している間に届きました]\n\
         {message}\n\
         （この指示を踏まえて以後の方針を調整し、受領した旨と反映内容を report_progress で親へ返してください。）"
    )
}

/// 変動コンテキストを最後のuserメッセージに前置するヘルパー（実体は
/// [`opencrab_core::runtime_context`] / #190 S2）。
///
/// 純関数なので下位層へ移した。transport 側のクレートが
/// `crates/server` を参照せずに使えるようにするため。既存の呼び出し元
/// （`process::prepend_runtime_context(..)`）を変えずに済むよう再エクスポートを残す。
pub use opencrab_core::runtime_context::prepend_runtime_context;

/// Discord用: message_idを含む変動コンテキストを前置するヘルパー
pub fn prepend_runtime_context_discord(
    user_message: &str,
    session_theme: &str,
    message_id: &str,
) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z");
    let tz_name = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string());
    let now = format!("{now} ({tz_name})");
    format!(
        "[Context]\nCurrent date and time: {now}\nCurrent discussion topic: {session_theme}\nDiscord message_id: {message_id}\n\n{user_message}"
    )
}

#[cfg(test)]
mod completion_recovery_tests {
    use super::*;
    use opencrab_core::LiveToolCompletionSource;

    fn stage(included_at: &str, queued_at: &str) -> (opencrab_db::Db, SessionLiveToolCompletions) {
        let db = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        let conn = db.lock().unwrap();
        for (id, call_id, execution, completed_at) in [
            ("included", "t1", "exec-1", included_at),
            ("queued", "t2", "exec-2", queued_at),
        ] {
            let result_log_id = opencrab_db::queries::insert_session_log(
                &conn,
                &opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: "agent".into(),
                    session_id: "session".into(),
                    log_type: "tool_result".into(),
                    content: format!("[<{call_id}] status:completed\n{id}"),
                    speaker_id: None,
                    turn_number: None,
                    metadata_json: Some(
                        serde_json::json!({
                            "conversation_tool_id": call_id,
                            "lifecycle_status": "completed",
                        })
                        .to_string(),
                    ),
                    created_at: None,
                },
            )
            .unwrap();
            opencrab_db::queries::enqueue_tool_completion_event(
                &conn,
                &opencrab_db::queries::NewToolCompletionEvent {
                    event_id: id,
                    session_id: "session",
                    causal_turn_id: "turn",
                    tool_call_id: call_id,
                    execution_id: execution,
                    result_log_id,
                    completed_at,
                },
            )
            .unwrap();
        }
        opencrab_db::queries::mark_tool_completion_events_included(
            &conn,
            &["included".into()],
            "request-1",
            "digest-1",
            r#"{"model":"model","messages":[]}"#,
        )
        .unwrap();
        drop(conn);
        let source = SessionLiveToolCompletions::new(db.clone(), "session");
        (db, source)
    }

    #[test]
    fn mixed_included_and_queued_recovers_only_included_in_both_orders() {
        for (included_at, queued_at) in [
            ("2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z"),
            ("2026-01-02T00:00:00Z", "2026-01-01T00:00:00Z"),
        ] {
            let (db, source) = stage(included_at, queued_at);
            let first = source.poll_tool_completions();
            assert_eq!(first[0].event_id, "included");
            assert_eq!(first.len(), 1);
            let conn = db.lock().unwrap();
            opencrab_db::queries::mark_tool_completion_events_consumed(
                &conn,
                &["included".into()],
                "request-1",
            )
            .unwrap();
            drop(conn);
            let second = source.poll_tool_completions();
            assert_eq!(second[0].event_id, "queued");
            assert_eq!(second.len(), 1);
        }
    }
}
