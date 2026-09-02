//! 受信箱（`agent_inbox`）の消化ループ（webhook intake / issue #454）。
//!
//! # なぜ heartbeat と別ループ・別セッションか
//!
//! 中央スケジューラ（#439・#465）のタスク自体は常時起動だが、heartbeat の**発火は enabled な
//! セッションに対してだけ**行われる（`scheduler.rs` は `list_enabled_session_heartbeat_configs`
//! で enabled 行だけを発火エントリに組み、`discord-` にはさらに live G ゲートも掛ける）。inbox
//! 消化を heartbeat の発火へ相乗りさせると、webhook 対象エージェントの heartbeat が無効なとき
//! **inbox が黙って消化されない**（silent no-op）。それを避けるため、heartbeat の有効・無効に
//! 依存しない常時起動の専用ループにする（`spawn_intake_process_loop`）。
//!
//! さらに **heartbeat の agent-scoped ターン（`channel_id=""`）を再利用しない**。当時のそれは
//! 発話時に稼働中 transport へ配送し、Nostr の text_delivery は宛先を無視して kind:1 を broadcast
//! する（`crates/nostr/src/text_delivery.rs`）。それを通すと **webhook 起点で外部タイムラインへ
//! broadcast する経路**を新設してしまう（#454 の意図外・owner 決定 #456 の「agent スコープ全廃」
//! とも逆行）。加えて heartbeat と別経路で同じセッション id を走らせると直列化ロックを共有せず
//! 二重発話・DB 競合が起きうる。
//!
//! # 何をするか
//!
//! 専用セッション `intake-{agent}` で [`run_agent_response`] を直接呼ぶ。未処理イベントを
//! **会話として渡す**だけで、エージェントは自分のツール経由でのみ作用する（sample-source への返信
//! 等）。**heartbeat の SPEAK 配送（broadcast）は通さない**。応答は監査用に intake セッションへ
//! 記録する。
//!
//! # コスト制御（受け入れ基準）
//!
//! 「inbox 空の tick では LLM 呼び出しが発生しない」を満たすため、未処理行を持つエージェント
//! だけを [`agents_with_unprocessed_inbox`] で絞り、**未処理が 1 件も無ければ turn を起こさない**。
//!
//! # 再試行
//!
//! `processed_at` は **turn が Ok を返したときだけ**刻む。エラー（LLM 障害等）は未処理のまま
//! 残し次 tick で再試行する（at-least-once。外部イベントを黙って失わない方を採る）。

use std::time::Duration;

use opencrab_actions::{CallerIdentity, RunRequest};
use opencrab_db::queries::{
    agents_with_unprocessed_inbox, insert_session, insert_session_log, list_unprocessed_inbox,
    mark_inbox_processed, AgentInboxRow, SessionLogRow, SessionRow,
};
use opencrab_server::process::{build_agent_context, run_agent_response};
use opencrab_server::AppState;

/// intake 専用セッション id の接頭辞（heartbeat の `heartbeat-` と別空間に分ける）。
const INTAKE_SESSION_PREFIX: &str = "intake-";

/// 1 エージェントから 1 tick で消化する未処理イベントの上限（バッチ）。
const INBOX_BATCH_LIMIT: i64 = 20;

/// 1 イベントの payload を prompt に載せるときの最大文字数（文脈予算の暴発を防ぐ）。
const PAYLOAD_PREVIEW_CHARS: usize = 4000;

/// 1 tick で会話（inbox 本文）に載せる合計文字数の上限。system prompt（ペルソナ/記憶/スキル）
/// と合わせても小さめのモデルの文脈に収まる余裕を残す。これを超える分は**次の tick へ回す**
/// （採用した件数だけ processed を刻み、残りは未処理のまま）。バッチ全文を無条件に載せて
/// 文脈溢れで turn ごと失敗するのを防ぐ（レビュー指摘: per-item truncate と別に全体 budget）。
const TOTAL_PROMPT_BUDGET_CHARS: usize = 24000;

/// ループの下限間隔（秒）。設定値はそのまま保持し、ここで床を効かせる（既存ループと同流儀）。
const MIN_INTERVAL_SECS: u64 = 10;

/// 受信箱消化ループを起動する（常時。source アダプタや heartbeat 設定に依存しない）。
///
/// 各周回は `process_all_inboxes` を **inline に await して直列処理**し、その後
/// [`wait_for_tick`] で「ポーリング間隔の満了」か「`intake_wake` の通知」のどちらか早い方まで
/// 待つ（#499）。webhook が新規イベントを積むと [`AppState::intake_wake`] が鳴り、間隔を待たず
/// 即消化する。ポーリングは取りこぼし・再試行の安全網として**間隔既定のまま残す**。
///
/// ループは単一タスクなので消化は決して並行しない。処理中に通知が複数来ても `Notify` の permit は
/// 1 つに畳まれ、現在の処理が終わったあと 1 回だけ再消化される（多重ターンにならない・#499 注意点）。
pub fn spawn_intake_process_loop(state: AppState) {
    let interval_secs = state.intake.process_interval_secs.max(MIN_INTERVAL_SECS);
    tokio::spawn(async move {
        let interval = Duration::from_secs(interval_secs);
        let wake = state.intake_wake.clone();
        tracing::info!(interval_secs, "intake process loop started");
        loop {
            process_all_inboxes(&state).await;
            wait_for_tick(interval, &wake).await;
        }
    });
}

/// ポーリング間隔が満了するか、`wake` が鳴るまで待つ（どちらか早い方 / #499）。
///
/// `wake` は webhook が新規イベントを積んだ直後に鳴る。`Notify` は待機者が居ないときの通知を
/// permit として 1 つ記憶するので、enqueue → notify の直後にこの関数へ入っても取りこぼさず
/// 即起きる（記憶した permit を `notified()` が消費する）。間隔満了は安全網（取りこぼし・再試行）
/// として常に効く。scheduler の `sleep_or_wake` と同じ形。
async fn wait_for_tick(interval: Duration, wake: &tokio::sync::Notify) {
    tokio::select! {
        _ = tokio::time::sleep(interval) => {}
        _ = wake.notified() => {}
    }
}

/// 未処理を持つエージェントだけを順に消化する。空なら turn を一切起こさない。
async fn process_all_inboxes(state: &AppState) {
    let agent_ids = {
        let Ok(conn) = state.db.lock() else {
            return;
        };
        match agents_with_unprocessed_inbox(&conn) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "intake process: 未処理エージェントの走査に失敗");
                return;
            }
        }
    };
    for stored_agent_id in agent_ids {
        process_agent_inbox(state, &stored_agent_id).await;
    }
}

/// 1 エージェント分を消化する。
///
/// `stored_agent_id` は受信時に保存した値（config のルート値 = 名前 or UUID）。turn は
/// heartbeat と同じく解決した UUID で走らせる（名前→UUID は `resolve_agent_id`）。
async fn process_agent_inbox(state: &AppState, stored_agent_id: &str) {
    // (a) 未処理を取得（短いロック）。
    let rows = {
        let Ok(conn) = state.db.lock() else {
            return;
        };
        match list_unprocessed_inbox(&conn, stored_agent_id, INBOX_BATCH_LIMIT) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(agent_id = %stored_agent_id, error = %e, "intake process: 取得失敗");
                return;
            }
        }
    };
    if rows.is_empty() {
        return; // 直前に他所が処理した等。turn は起こさない。
    }

    // (b) 名前→UUID 解決 + intake セッション確保 + agent 文脈の組み立て（短いロック）。
    let prepared = {
        let Ok(conn) = state.db.lock() else {
            return;
        };
        let resolved_agent_id = crate::resolve_agent_id(&conn, stored_agent_id);
        let session_id = format!("{INTAKE_SESSION_PREFIX}{resolved_agent_id}");
        ensure_intake_session(&conn, &session_id, &resolved_agent_id);
        let (system_prompt, agent_name) =
            build_agent_context(&conn, &resolved_agent_id, &CallerIdentity::Owner);
        (resolved_agent_id, session_id, system_prompt, agent_name)
    };
    let (resolved_agent_id, session_id, system_prompt, agent_name) = prepared;

    // (c) 未処理イベントを会話として渡す。**session 履歴からは組まない**（外部イベントを
    //     その場で処理するだけ・継続性は各エージェントの記憶系が担う）。合計 budget を
    //     超える分は次 tick へ回すため、採用件数 `included` だけを今回処理する。
    let (conversation, included) = build_inbox_prompt(&rows);
    let included_rows = &rows[..included];

    // 監査用にイベントを intake セッションへ system ログとして残す（配送はしない）。
    if let Ok(conn) = state.db.lock() {
        let log = SessionLogRow {
            id: None,
            agent_id: resolved_agent_id.clone(),
            session_id: session_id.clone(),
            log_type: "system".to_string(),
            content: conversation.clone(),
            speaker_id: Some("intake".to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        if let Err(e) = insert_session_log(&conn, &log) {
            tracing::warn!(agent_id = %resolved_agent_id, error = %e, "intake process: 監査ログ記録失敗");
        }
    }

    // (d) turn（ロック無し・await）。**purpose=intake / caller=Owner / dispatch なし・配送なし**。
    //     エージェントはツール経由でのみ作用する。SPEAK を外部へ broadcast しない。
    let req = RunRequest::new(
        &resolved_agent_id,
        &agent_name,
        &session_id,
        &system_prompt,
        &conversation,
        "intake",
        CallerIdentity::Owner,
    );
    match run_agent_response(state, req).await {
        Ok(result) => {
            // 応答を監査用に記録し、処理済みを刻む（Ok のときだけ / at-least-once）。
            if let Ok(conn) = state.db.lock() {
                let log = SessionLogRow {
                    id: None,
                    agent_id: resolved_agent_id.clone(),
                    session_id: session_id.clone(),
                    log_type: "speech".to_string(),
                    content: result.response.clone(),
                    speaker_id: Some(resolved_agent_id.clone()),
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                };
                let _ = insert_session_log(&conn, &log);
                // 今回の会話に載せた分（budget 内）だけを処理済みにする。残りは次 tick。
                for r in included_rows {
                    if let Err(e) = mark_inbox_processed(&conn, &r.id) {
                        tracing::warn!(agent_id = %resolved_agent_id, inbox_id = %r.id, error = %e, "intake process: processed マーク失敗");
                    }
                }
            }
        }
        Err(e) => {
            // 未処理のまま残す（次 tick で再試行）。外部イベントを黙って失わない。
            tracing::warn!(agent_id = %resolved_agent_id, error = %e, "intake process: turn 失敗（未処理のまま保持し再試行）");
        }
    }
}

/// intake 専用セッションを無ければ作る（mode="intake"）。
fn ensure_intake_session(conn: &rusqlite::Connection, session_id: &str, agent_id: &str) {
    if let Ok(Some(_)) = opencrab_db::queries::get_session(conn, session_id) {
        return;
    }
    let session = SessionRow {
        id: session_id.to_string(),
        mode: "intake".to_string(),
        theme: "外部イベント受信箱の消化".to_string(),
        phase: "active".to_string(),
        turn_number: 0,
        status: "active".to_string(),
        participant_ids_json: serde_json::json!([agent_id]).to_string(),
        facilitator_id: None,
        done_count: 0,
        max_turns: None,
        metadata_json: None,
    };
    if let Err(e) = insert_session(conn, &session) {
        tracing::warn!(agent_id = %agent_id, error = %e, "intake process: セッション作成失敗");
    }
}

/// 未処理イベントを合計文字数の予算内で**先頭から**選び、会話文字列と採用件数を返す。
///
/// rows は受信順（古い順）。予算 [`TOTAL_PROMPT_BUDGET_CHARS`] を超える分は含めず、呼び出し
/// 側は**採用した件数だけ processed を刻む**（残りは未処理のまま次 tick へ）。**最低 1 件は
/// 必ず含める**（1 件で予算超過でも処理しないと永久に詰まるため）。
fn build_inbox_prompt(rows: &[AgentInboxRow]) -> (String, usize) {
    let mut body = String::new();
    let mut included = 0usize;
    for (i, r) in rows.iter().enumerate() {
        let payload = truncate_chars(&r.payload_json, PAYLOAD_PREVIEW_CHARS);
        let entry = format!(
            "\n{}. [{}/{}] (received_at={})\n{}\n",
            i + 1,
            r.source,
            r.event_type,
            r.received_at,
            payload
        );
        // 2 件目以降は合計予算を超えない範囲でのみ追加する（1 件目は無条件）。
        if included > 0 && body.chars().count() + entry.chars().count() > TOTAL_PROMPT_BUDGET_CHARS
        {
            break;
        }
        body.push_str(&entry);
        included += 1;
    }
    let header = format!(
        "[受信箱] 外部から届いた未処理イベントが {included} 件あります。内容を確認し、必要なら\
         あなたのツールで対応してください（この受信箱の消化は外部への発話配信を行いません）。\n"
    );
    (format!("{header}{body}"), included)
}

/// 文字数（char 単位）で切り詰める。マルチバイト境界を割らない。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…（{}文字を省略）", s.chars().count() - max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, source: &str, ev: &str, payload: &str) -> AgentInboxRow {
        AgentInboxRow {
            id: id.to_string(),
            agent_id: "agent_alpha".to_string(),
            source: source.to_string(),
            event_type: ev.to_string(),
            dedup_key: format!("{ev}:{id}"),
            payload_json: payload.to_string(),
            received_at: "2026-08-09T00:00:00Z".to_string(),
            processed_at: None,
        }
    }

    #[test]
    fn prompt_lists_all_events_with_source_and_type() {
        let rows = vec![
            row(
                "1",
                "sample-source",
                "comment.created",
                "{\"id\":1,\"text\":\"hi\"}",
            ),
            row("2", "sample-source", "chat.message", "{\"id\":2}"),
        ];
        let (p, included) = build_inbox_prompt(&rows);
        assert_eq!(included, 2, "小さい 2 件は両方載る");
        assert!(p.contains("2 件"));
        assert!(p.contains("sample-source/comment.created"));
        assert!(p.contains("sample-source/chat.message"));
        assert!(p.contains("\"text\":\"hi\""));
        // 消化ターンは外部配信しないことを本文で明示している（broadcast 誤解の防止）。
        assert!(p.contains("外部への発話配信を行いません"));
    }

    #[test]
    fn prompt_caps_total_budget_but_always_includes_one() {
        // 各イベントが per-item 上限いっぱいの payload を持つと、合計 budget で件数が絞られる。
        let big = "x".repeat(PAYLOAD_PREVIEW_CHARS);
        let rows: Vec<AgentInboxRow> = (0..20)
            .map(|i| row(&i.to_string(), "sample-source", "comment.created", &big))
            .collect();
        let (p, included) = build_inbox_prompt(&rows);
        // 全 20 件は載らない（budget で切れる）が、少なくとも 1 件は載る。
        assert!(included >= 1, "最低 1 件は必ず載せる");
        assert!(included < rows.len(), "budget 超過分は次 tick へ回す");
        // ヘッダの件数は実際に載せた数と一致する（残数を誤って処理済みにしない担保）。
        assert!(p.contains(&format!("{included} 件")));
        // 1 件だけで budget を超える極端ケースでも 1 件は返す。
        let huge = vec![row(
            "0",
            "sample-source",
            "comment.created",
            &"y".repeat(PAYLOAD_PREVIEW_CHARS),
        )];
        assert_eq!(build_inbox_prompt(&huge).1, 1);
    }

    #[test]
    fn truncate_respects_char_boundary_and_marks_omission() {
        let long = "あ".repeat(5000);
        let out = truncate_chars(&long, PAYLOAD_PREVIEW_CHARS);
        // char 数で切る（バイトではない）。省略マーカーが付く。
        assert!(out.chars().count() < 5000);
        assert!(out.contains("文字を省略"));
        // 短い入力はそのまま。
        assert_eq!(truncate_chars("short", 100), "short");
    }

    /// #499: webhook が積んだ直後に `intake_wake` が鳴っていれば、消化ループはポーリング間隔を
    /// 待たずに即起きる。間隔を 1 時間に取り、事前に notify した状態で 1 秒以内に返れば
    /// **wake ブランチで起きた**ことの証明になる（sleep ブランチなら 1 時間眠って timeout する）。
    /// notify を外すと（`wait_for_tick` の wake ブランチ削除 / 呼び出し側の notify 削除）この
    /// テストは timeout で赤くなる（変異検出）。
    #[tokio::test]
    async fn wait_for_tick_wakes_immediately_when_prenotified() {
        let wake = tokio::sync::Notify::new();
        // 待機者が居なくても permit を 1 つ記憶する（enqueue→notify→待機 の競合を再現）。
        wake.notify_one();
        let r = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_tick(Duration::from_secs(3600), &wake),
        )
        .await;
        assert!(
            r.is_ok(),
            "事前 notify があれば間隔（1時間）を待たず即起きるはず（wake ブランチ）"
        );
    }

    /// #499: 通知が無くてもポーリング間隔の満了で必ず起きる（取りこぼし・再試行の安全網が残る）。
    /// 短い間隔で notify せず、間隔満了で返ることを確認する。`wait_for_tick` の sleep ブランチを
    /// 削除するとこのテストは timeout で赤くなる。
    #[tokio::test]
    async fn wait_for_tick_falls_back_to_polling_without_notify() {
        let wake = tokio::sync::Notify::new();
        let r = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_tick(Duration::from_millis(20), &wake),
        )
        .await;
        assert!(
            r.is_ok(),
            "通知が無くても間隔満了（sleep ブランチ）で起きる安全網が残っているはず"
        );
    }

    // ===== #898 §13.1 e: intake 起点のターンでも CONTINUE 途中発話が保存される =====
    // DESIGN-TURN-CONTINUATION §13.1 e「scheduler / intake / heartbeat 起点のターン（ユーザー
    // 発話なし）… 表の期待は同じ。CONTINUE も有効」。plain3（本文＋CONTINUE ×2 → 本文）で
    // intake セッションに speech 3 件・LLM 3 回（intake は外部配送しないので観測は保存件数と
    // LLM 回数）。現状は最終応答 1 件しか保存されない。

    struct E13Mock {
        responses:
            std::sync::Mutex<std::collections::VecDeque<opencrab_llm::message::ChatResponse>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl E13Mock {
        fn new(texts: &[&str]) -> Self {
            use opencrab_llm::message::{Choice, FinishReason, Message, Usage};
            let mut q = std::collections::VecDeque::new();
            for t in texts {
                q.push_back(opencrab_llm::message::ChatResponse {
                    id: "e13".to_string(),
                    model: "mock-model".to_string(),
                    choices: vec![Choice {
                        index: 0,
                        message: Message::assistant(*t),
                        finish_reason: Some(FinishReason::Stop),
                    }],
                    usage: Usage {
                        prompt_tokens: 1,
                        completion_tokens: 1,
                        total_tokens: 2,
                        cache_read_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                    },
                    created: 0,
                });
            }
            Self {
                responses: std::sync::Mutex::new(q),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl opencrab_llm::traits::LlmProvider for E13Mock {
        fn name(&self) -> &str {
            "mock"
        }
        fn sends_max_output_tokens(&self) -> bool {
            false
        }
        async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
            Ok(vec![])
        }
        async fn chat_completion(
            &self,
            _req: opencrab_llm::message::ChatRequest,
        ) -> anyhow::Result<opencrab_llm::message::ChatResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("E13Mock: no more responses"))
        }
    }

    fn intake_test_state(mock: std::sync::Arc<E13Mock>) -> AppState {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        {
            let conn = db.lock().unwrap();
            opencrab_db::queries::upsert_model_pricing(
                &conn,
                &opencrab_db::queries::ModelPricingRow {
                    provider: "mock".to_string(),
                    model: "gpt-4o".to_string(),
                    input_price_per_1m: 0.0,
                    output_price_per_1m: 0.0,
                    context_window: Some(200_000),
                    max_output_tokens: Some(4_096),
                },
            )
            .unwrap();
            opencrab_db::queries::upsert_agent(
                &conn,
                &opencrab_db::queries::AgentRow {
                    agent_id: "agent_alpha".to_string(),
                    name: "Alpha".to_string(),
                    job_title: None,
                    organization: None,
                    image_url: None,
                    persona_name: "p".to_string(),
                    personality: None,
                    instructions: String::new(),
                    heartbeat_instructions: String::new(),
                    model: None,
                    reasoning_effort: None,
                    web_search: None,
                    metadata_json: None,
                },
            )
            .unwrap();
        }
        let mut router = opencrab_llm::router::LlmRouter::new();
        router.add_provider(mock as std::sync::Arc<dyn opencrab_llm::traits::LlmProvider>);
        router.set_default_provider("mock");
        let timed = opencrab_actions::TimedFireRouter::new();
        opencrab_server::register_production_descriptors(&timed);
        AppState {
            db,
            llm_router: opencrab_server::SharedLlmRouter::new(router),
            llm_config: std::sync::Arc::new(toml::from_str("").unwrap()),
            subtask_auto_dispatch: true,
            voice_config: std::sync::Arc::new(Default::default()),
            voice_runtime: std::sync::Arc::new(std::sync::Mutex::new(None)),
            workspace_base: std::env::temp_dir().to_string_lossy().to_string(),
            #[cfg(feature = "nostr")]
            nostr_master_key: None,
            default_model: "mock:gpt-4o".to_string(),
            tools_config: std::sync::Arc::new(std::sync::RwLock::new(
                opencrab_actions::tools::ToolsConfig::default(),
            )),
            compaction_ratio: 0.5,
            typed_history_enabled: false,
            typed_history_drop_directive: false,
            evaluator: opencrab_server::config::EvaluatorConfig::default(),
            skill_consolidation: opencrab_server::config::SkillConsolidationConfig::default(),
            category_maintenance: opencrab_server::config::CategoryMaintenanceConfig::default(),
            memory_organize: opencrab_server::config::MemoryOrganizeConfig::default(),
            memory_declare: opencrab_server::config::MemoryDeclareConfig::default(),
            memory_condense: opencrab_server::config::MemoryCondenseConfig::default(),
            loop_restart_enabled: false,
            index_build_inflight: std::sync::Arc::new(dashmap::DashMap::new()),
            intake: std::sync::Arc::new(Default::default()),
            intake_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
            mcp_manager: None,
            gateways: std::sync::Arc::new(opencrab_actions::AgentGatewayRegistry::new()),
            subtask_registries: std::sync::Arc::new(
                opencrab_server::subtask_registries::SubtaskRegistries::new(),
            ),
            session_locks: std::sync::Arc::new(opencrab_actions::SessionLocks::new()),
            timed_fire_router: std::sync::Arc::new(timed),
            progress_debounce: std::sync::Arc::new(
                opencrab_server::subtask_registries::ProgressDebounce::new(),
            ),
            subtask_notifiers: std::sync::Arc::new(dashmap::DashMap::new()),
            subtask_lifecycle_notifier: std::sync::Arc::new(std::sync::Mutex::new(None)),
            default_subtask_webhook: None,
            heartbeat_limits: opencrab_server::config::HeartbeatLimits::default(),
            scheduler_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
            heartbeat_config_rx: opencrab_server::disconnected_heartbeat_config_rx(
                opencrab_core::heartbeat::HeartbeatConfig::default(),
            ),
        }
    }

    #[tokio::test]
    async fn intake_continue_saves_each_intermediate_speech() {
        let mock = std::sync::Arc::new(E13Mock::new(&[
            "E13-1回目。まず一つ⚡\nCONTINUE",
            "E13-2回目。次いこう⚡\nCONTINUE",
            "E13-3回目。これで最後⚡",
        ]));
        let state = intake_test_state(mock.clone());

        // 未処理の inbox イベントを 1 件積む（これが intake ターンの起点）。
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::enqueue_inbox_event(
                &conn,
                &opencrab_db::queries::InboxInsert {
                    id: "ev-e13".to_string(),
                    agent_id: "agent_alpha".to_string(),
                    source: "sample-source".to_string(),
                    event_type: "comment.created".to_string(),
                    dedup_key: "comment.created:ev-e13".to_string(),
                    payload_json: "{\"text\":\"3回に分けて\"}".to_string(),
                },
            )
            .unwrap();
        }

        process_all_inboxes(&state).await;

        let session_id = format!("{INTAKE_SESSION_PREFIX}agent_alpha");
        let speeches: Vec<String> = {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::list_session_logs_by_session(&conn, &session_id)
                .unwrap()
                .into_iter()
                .filter(|l| l.log_type == "speech" && l.content.contains("E13-"))
                .map(|l| l.content)
                .collect()
        };
        assert_eq!(
            speeches.len(),
            3,
            "intake 起点でも途中発話が保存されるはず（現状は最終のみ）: {speeches:?}"
        );
        assert!(
            speeches.iter().all(|s| !s.contains("CONTINUE")),
            "保存された speech に CONTINUE 残留: {speeches:?}"
        );
        assert_eq!(mock.calls(), 3, "末尾 CONTINUE で LLM が 3 回呼ばれるはず");
    }
}
