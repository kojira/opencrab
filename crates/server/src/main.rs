use std::sync::Arc;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

use opencrab_core::heartbeat::{
    heartbeat_loop, HeartbeatCallback, HeartbeatConfig, HeartbeatDecision,
};
use opencrab_server::{config, create_router, AppState};
use tokio::sync::watch;

#[cfg(feature = "discord")]
type DiscordHttpArc = Arc<Mutex<Option<Arc<serenity::http::Http>>>>;
#[cfg(not(feature = "discord"))]
type DiscordHttpArc = Arc<Mutex<Option<()>>>;

/// ハートビート用セッションを取得または作成する。
fn get_or_create_heartbeat_session(
    db: &opencrab_db::Db,
    agent_id: &str,
    channel_id: &str,
) -> String {
    let session_id = format!("heartbeat-{}-{}", agent_id, channel_id);
    let conn = db.lock().unwrap();
    if let Ok(Some(_)) = opencrab_db::queries::get_session(&conn, &session_id) {
        return session_id;
    }
    let session = opencrab_db::queries::SessionRow {
        id: session_id.clone(),
        mode: "heartbeat".to_string(),
        theme: "ハートビート自律行動".to_string(),
        phase: "active".to_string(),
        turn_number: 0,
        status: "active".to_string(),
        participant_ids_json: serde_json::json!([agent_id]).to_string(),
        facilitator_id: None,
        done_count: 0,
        max_turns: None,
        metadata_json: None,
    };
    if let Err(e) = opencrab_db::queries::insert_session(&conn, &session) {
        tracing::warn!("Failed to create heartbeat session: {e}");
    }
    session_id
}

/// heartbeat 経路の `RunRequest` を組む（#169）。
///
/// 非ブロック dispatch（RFC #152 S3a）を有効化する。これにより heartbeat の tick は
/// 長時間ツールで塞がれず、`cancel_subtask`（#161）からも停止できる。
///
/// - registry: **agent 単位**で `AppState` が保持しているものを共有する。tick /
///   チャンネル / heartbeat ループ再起動（設定変更）を跨いで同一 Arc なので、前 tick で
///   dispatch した subtask を後続 tick の `cancel_subtask` が引ける（使い捨ての DashMap
///   では常に not found）。
/// - sink: `NoopCompletionSink` = **即時 resume はしない**。完了本文は
///   `settle_completed` が親セッションログへ永続化し、heartbeat は毎 tick 同じ
///   session_id で `build_conversation_string` により会話を再構築するため、次 tick で
///   自然に文脈へ載る。sink で resume させると `SPEAK:` パースと heartbeat ログ記録を
///   sink 側へ複製する必要があり、かつ session ロックが無いため次 tick と競合して
///   二重応答の不変条件（RFC §6）を壊す。
fn heartbeat_run_request(
    registries: &opencrab_server::subtask_registries::SubtaskRegistries,
    agent_id: &str,
    agent_name: &str,
    session_id: &str,
    system_prompt: &str,
    conversation: &str,
) -> opencrab_actions::RunRequest {
    opencrab_actions::RunRequest::new(
        agent_id,
        agent_name,
        session_id,
        system_prompt,
        conversation,
        "heartbeat",
        opencrab_actions::CallerIdentity::Owner,
    )
    .with_dispatch(
        Some(registries.registry_for(agent_id)),
        Arc::new(opencrab_actions::NoopCompletionSink),
    )
}

/// ハートビートコールバックを生成する。
/// 初期起動とhot-reload再起動の両方で使用。
fn make_heartbeat_callback(
    db: opencrab_db::Db,
    agent_id_owned: String,
    discord_http: DiscordHttpArc,
    state: AppState,
    global_interval_secs: u64,
    last_channel_ticks: Arc<Mutex<std::collections::HashMap<String, std::time::Instant>>>,
) -> HeartbeatCallback {
    Box::new(move |_agent_id: &str, tick: u64| {
        let _agent_id = _agent_id.to_string();
        let db = db.clone();
        let agent_id_owned = agent_id_owned.clone();
        let discord_http = discord_http.clone();
        let state = state.clone();
        let global_interval_secs = global_interval_secs;
        let last_channel_ticks = last_channel_ticks.clone();
        Box::pin(async move {
            // heartbeat_enabled=trueのチャンネルを取得。
            // グローバル設定（agent_id="")とエージェント固有設定の両方が同一channel_idに
            // 存在しうるため、(1) 当該エージェントに無関係な行を除外し、
            // (2) 同一channel_idではエージェント固有行をグローバル行より優先して
            // 重複処理を防ぐ。
            let whitelisted_channels: Vec<(String, String, Option<u64>)> = {
                let conn = db.lock().unwrap();
                match opencrab_db::queries::list_heartbeat_channels(&conn) {
                    Ok(channels) => {
                        // channel_id -> 選択された行。エージェント固有行を優先。
                        let mut selected: std::collections::HashMap<
                            String,
                            opencrab_db::queries::ChannelConfigRow,
                        > = std::collections::HashMap::new();
                        for c in channels {
                            // 当該エージェント向けでもグローバルでもない行は無視。
                            if !c.agent_id.is_empty() && c.agent_id != agent_id_owned {
                                continue;
                            }
                            match selected.get(&c.channel_id) {
                                // 既にエージェント固有行を選択済みならグローバル行で上書きしない。
                                Some(existing)
                                    if !existing.agent_id.is_empty() && c.agent_id.is_empty() =>
                                {
                                    continue;
                                }
                                _ => {
                                    selected.insert(c.channel_id.clone(), c);
                                }
                            }
                        }
                        selected
                            .into_values()
                            .map(|c| (c.channel_id, c.channel_name, c.heartbeat_interval_secs))
                            .collect()
                    }
                    Err(e) => {
                        tracing::warn!("Failed to list whitelisted channels: {e}");
                        vec![]
                    }
                }
            };

            if whitelisted_channels.is_empty() {
                tracing::debug!(agent_id = %agent_id_owned, tick, "No whitelisted channels, skipping heartbeat tick");
                return HeartbeatDecision::Idle;
            }

            // 最後の決定を返す（全チャンネルを処理した後）
            let mut last_decision = HeartbeatDecision::Idle;

            for (channel_id_str, channel_name, channel_interval_secs) in &whitelisted_channels {
                // per-channel interval チェック
                let effective_interval = channel_interval_secs.unwrap_or(global_interval_secs);
                let should_fire = {
                    let ticks = last_channel_ticks.lock().unwrap();
                    let now = std::time::Instant::now();
                    let last = ticks.get(channel_id_str.as_str());
                    match last {
                        None => true,
                        Some(last_time) => {
                            now.duration_since(*last_time).as_secs() >= effective_interval
                        }
                    }
                };
                if !should_fire {
                    tracing::debug!(
                        agent_id = %agent_id_owned,
                        channel_id = %channel_id_str,
                        effective_interval,
                        "Heartbeat: channel interval not elapsed, skipping"
                    );
                    continue;
                }
                // last_tickを更新
                {
                    let mut ticks = last_channel_ticks.lock().unwrap();
                    ticks.insert(channel_id_str.clone(), std::time::Instant::now());
                }

                tracing::debug!(
                    agent_id = %agent_id_owned,
                    channel_id = %channel_id_str,
                    channel_name = %channel_name,
                    tick,
                    "Heartbeat tick for channel"
                );

                // 1. チャンネルごとのセッション取得/作成
                let session_id =
                    get_or_create_heartbeat_session(&db, &agent_id_owned, channel_id_str);

                // 2. ハートビートプロンプトをsession_logsに挿入
                //    指示部分（方針・頻度・トーン）は設定可能、出力形式の規約はランタイム固定。
                let hb_source;
                {
                    let conn = db.lock().unwrap();
                    let resolved = opencrab_db::queries::resolve_heartbeat_instructions(
                        &conn,
                        &agent_id_owned,
                        channel_id_str,
                    );
                    hb_source = resolved.source;
                    // 場所の呼称は transport 中立にする（#158 S2）。名前自体は設定由来。
                    let prompt = format!(
                        "[ハートビート] 現在の会話「{}」。{}\n出力形式: SPEAK/LEARN/IDLE のいずれか。SPEAKの場合のみ 'SPEAK: <メッセージ>' の形式で一言。",
                        channel_name, resolved.text
                    );
                    let log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: agent_id_owned.clone(),
                        session_id: session_id.clone(),
                        log_type: "system".to_string(),
                        content: prompt,
                        speaker_id: Some("heartbeat".to_string()),
                        turn_number: None,
                        metadata_json: None,
                        created_at: None,
                    };
                    if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
                        tracing::error!(agent_id = %agent_id_owned, "Failed to insert heartbeat prompt log: {e}");
                        continue;
                    }
                }

                // 3-4. エージェントコンテキストと会話文字列を構築
                let (system_prompt, agent_name, conversation) = {
                    let conn = db.lock().unwrap();
                    let (sp, name) =
                        opencrab_server::process::build_agent_context(&conn, &agent_id_owned);
                    // Use per-agent model from DB, fallback to global default
                    let agent_model = opencrab_db::queries::effective_model_for_agent(
                        &conn,
                        &agent_id_owned,
                        &state.default_model,
                    )
                    .unwrap_or_else(|_| state.default_model.clone());
                    let budget = opencrab_server::process::compute_context_budget(
                        &conn,
                        agent_model.split(':').next().unwrap_or(""),
                        agent_model.split(':').nth(1).unwrap_or(""),
                        state.compaction_ratio,
                    );
                    let conv = match opencrab_server::process::build_conversation_string(
                        &conn,
                        &session_id,
                        &agent_id_owned,
                        budget,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!(agent_id = %agent_id_owned, session_id = %session_id, "build_conversation_string failed: {e}");
                            continue;
                        }
                    };
                    (sp, name, conv)
                };
                let conversation = opencrab_server::process::prepend_runtime_context(
                    &conversation,
                    "ハートビート自律行動",
                );

                // 5. run_agent_response を呼び出す（非ブロック dispatch 有効 / #169）
                let engine_result = opencrab_server::process::run_agent_response(
                    &state,
                    heartbeat_run_request(
                        &state.subtask_registries,
                        &agent_id_owned,
                        &agent_name,
                        &session_id,
                        &system_prompt,
                        &conversation,
                    ),
                )
                .await;

                let decision = match engine_result {
                    Ok(result) => {
                        // 6. エージェント応答をsession_logsに記録
                        {
                            let conn = db.lock().unwrap();
                            let log = opencrab_db::queries::SessionLogRow {
                                id: None,
                                agent_id: agent_id_owned.clone(),
                                session_id: session_id.clone(),
                                log_type: "speech".to_string(),
                                content: result.response.clone(),
                                speaker_id: Some(agent_id_owned.clone()),
                                turn_number: None,
                                metadata_json: None,
                                created_at: None,
                            };
                            if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
                                tracing::error!(agent_id = %agent_id_owned, "Failed to insert heartbeat response log: {e}");
                            }
                        }

                        // 7. 応答からSPEAK/LEARN/IDLEを解析
                        let response_text = result.response.trim().to_string();
                        if response_text.contains("SPEAK:") {
                            let content = response_text
                                .lines()
                                .find(|l| l.contains("SPEAK:"))
                                .and_then(|l| l.splitn(2, "SPEAK:").nth(1))
                                .unwrap_or("")
                                .trim()
                                .to_string();
                            if !content.is_empty() {
                                HeartbeatDecision::Speak(content)
                            } else {
                                HeartbeatDecision::Idle
                            }
                        } else if response_text.to_uppercase().contains("LEARN") {
                            HeartbeatDecision::Learn
                        } else {
                            HeartbeatDecision::Idle
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Heartbeat agent response failed for channel {}: {e}",
                            channel_id_str
                        );
                        HeartbeatDecision::Idle
                    }
                };

                tracing::debug!(agent_id = %_agent_id, tick, channel_id = %channel_id_str, decision = %decision, "Heartbeat tick result");

                // heartbeat_logに記録
                if let Ok(conn) = db.lock() {
                    let decision_str = match &decision {
                        HeartbeatDecision::Idle => "idle",
                        HeartbeatDecision::Speak(_) => "speak",
                        HeartbeatDecision::Learn => "learn",
                        HeartbeatDecision::ManageSkills { .. } => "manage_skills",
                    };
                    let result_json = serde_json::json!({
                        "channel_id": channel_id_str,
                        "source": hb_source,
                    })
                    .to_string();
                    if let Err(e) = opencrab_db::queries::insert_heartbeat_log(
                        &conn,
                        &agent_id_owned,
                        decision_str,
                        Some(&result_json),
                    ) {
                        tracing::error!(agent_id = %agent_id_owned, "Failed to insert heartbeat log: {}", e);
                    }
                }

                // Speak/Learn後続処理
                match &decision {
                    HeartbeatDecision::Speak(content) => {
                        let content = content.clone();
                        let discord_http = discord_http.clone();
                        let channel_id_u64: Option<u64> = channel_id_str.parse().ok();
                        let agent_id_log = agent_id_owned.clone();
                        let ch_id_str = channel_id_str.clone();
                        tokio::spawn(async move {
                            let http_opt = discord_http.lock().unwrap().clone();
                            if let (Some(_http), Some(_ch_id)) = (http_opt.clone(), channel_id_u64)
                            {
                                #[cfg(feature = "discord")]
                                {
                                    use serenity::builder::CreateMessage;
                                    use serenity::model::id::ChannelId;
                                    let ch = ChannelId::new(_ch_id);
                                    if let Err(e) = ch
                                        .send_message(
                                            &_http,
                                            CreateMessage::new().content(&content),
                                        )
                                        .await
                                    {
                                        tracing::error!(agent_id = %agent_id_log, channel_id = %ch_id_str, "Heartbeat send_speech failed: {e}");
                                    } else {
                                        tracing::info!(agent_id = %agent_id_log, channel_id = %ch_id_str, "Heartbeat spoke: {}", content);
                                    }
                                }
                                #[cfg(not(feature = "discord"))]
                                {
                                    tracing::info!(agent_id = %agent_id_log, channel_id = %ch_id_str, "Heartbeat Speak (discord disabled): {}", content);
                                }
                            } else {
                                tracing::debug!(agent_id = %agent_id_log, "Heartbeat Speak: no Discord http or invalid channel_id");
                            }
                        });
                    }
                    HeartbeatDecision::Learn => {
                        let db = db.clone();
                        let agent_id_log = agent_id_owned.clone();
                        let tick_val = tick;
                        let ch_id_str = channel_id_str.clone();
                        tokio::spawn(async move {
                            if let Ok(conn) = db.lock() {
                                let memory = opencrab_db::queries::CuratedMemoryRow {
                                    id: uuid::Uuid::new_v4().to_string(),
                                    agent_id: agent_id_log.clone(),
                                    category: "reflection".to_string(),
                                    content: format!(
                                        "ハートビート内省 (tick {}, channel {}): 静かに自己を振り返る。",
                                        tick_val, ch_id_str
                                    ),
                                    created_at: String::new(),
                                };
                                if let Err(e) =
                                    opencrab_db::queries::upsert_curated_memory(&conn, &memory)
                                {
                                    tracing::error!(agent_id = %agent_id_log, "Heartbeat reflect_and_learn failed: {e}");
                                } else {
                                    tracing::info!(agent_id = %agent_id_log, channel_id = %ch_id_str, "Heartbeat reflect_and_learn: saved at tick {}", tick_val);
                                }
                            }
                        });
                    }
                    HeartbeatDecision::Idle => {}
                    HeartbeatDecision::ManageSkills { .. } => {}
                }

                last_decision = decision;
            }

            last_decision
        })
    })
}

/// config名またはUUIDのagent_idを、DBのUUIDに解決する。
/// "crab"のような名前が渡された場合、find_agentsで検索してUUIDを返す。
fn resolve_agent_id(conn: &rusqlite::Connection, agent_id: &str) -> String {
    // まず直接lookupを試みる
    if let Ok(Some(_)) = opencrab_db::queries::get_agent(conn, agent_id) {
        return agent_id.to_string();
    }
    // 名前で検索（完全一致またはUUID前方一致のみ。部分一致は複数エージェント時に誤マッチするため使わない）
    if let Ok(agents) = opencrab_db::queries::find_agents(conn, agent_id) {
        if let Some((uuid, _name)) = agents.iter().find(|(id, name)| {
            id.starts_with(agent_id) || name.to_lowercase() == agent_id.to_lowercase()
        }) {
            tracing::info!(config_id = %agent_id, uuid = %uuid, "Resolved agent_id config name to UUID (exact match)");
            return uuid.clone();
        }
    }
    // シングルエージェントフォールバック: DBに登録済みのエージェントが1つだけの場合はそれを使う
    // (config名"crab"などがDBの名前と一致しない場合の対応)
    if let Ok(all_agents) = opencrab_db::queries::find_agents(conn, "") {
        if all_agents.len() == 1 {
            let (uuid, name) = &all_agents[0];
            tracing::info!(
                config_id = %agent_id,
                uuid = %uuid,
                name = %name,
                "Resolved agent_id to only registered agent (single-agent fallback)"
            );
            return uuid.clone();
        }
    }
    tracing::warn!(agent_id = %agent_id, "Could not resolve agent_id to UUID, using as-is");
    agent_id.to_string()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("opencrab=info".parse()?))
        .init();

    tracing::info!("Starting OpenCrab server...");

    // Load config from TOML (with env var expansion)
    let cfg = config::load_config("config/default.toml")?;

    // 新しい通知先キーを空 url で書くと、旧キーの有効な値が黙って無効化される（#207）。
    // 挙動（新キー優先 / 空 url は無効）は意図したものなので変えず、気づけるようにだけする。
    cfg.warn_if_legacy_webhook_masked();

    // DB初期化（本番はコネクションプール）
    let db = opencrab_db::Db::open(&cfg.database.path)?;

    // Build LLM router from config + DB のダッシュボード設定オーバーライド
    let llm_overrides = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_llm_provider_overrides(&conn).unwrap_or_default()
    };
    let effective_llm = config::apply_llm_overrides(&cfg.llm, &llm_overrides);
    let llm_router = config::build_llm_router(&effective_llm)?;

    // 実効 voice 設定: DB オーバーライド（完全置換）> TOML。
    // 起動時の VC ランタイム構築にのみ使う（discord feature 無効時は未使用）。
    #[cfg_attr(not(feature = "discord"), allow(unused_variables))]
    let effective_voice: opencrab_voice::VoiceConfig = {
        let conn = db.lock().unwrap();
        match opencrab_db::queries::get_voice_config_override(&conn) {
            Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_else(|e| {
                tracing::warn!("voice_config_override JSON is broken; using TOML: {e}");
                cfg.voice.clone()
            }),
            _ => cfg.voice.clone(),
        }
    };

    let default_model = format!("{}:{}", cfg.llm.default_provider, cfg.llm.default_model);

    // NOTE: エージェント個別の許可コマンド（DB管理）はグローバル tools_config に
    // マージしない。全エージェントの許可が混ざり、あるエージェントの許可が他へ漏れるため。
    // 個別コマンドは実行時に run_agent_response 内でそのエージェント分だけ適用する。
    let tools_cfg = cfg.tools.clone();

    #[allow(unused_mut)]
    let mut state = AppState {
        db,
        llm_router: opencrab_server::SharedLlmRouter::new(llm_router),
        llm_config: Arc::new(cfg.llm.clone()),
        // 非ブロック dispatch の kill switch（`[subtask] auto_dispatch` / env 上書き）。
        subtask_auto_dispatch: cfg.subtask.auto_dispatch,
        // 純 TOML を保持する（DB オーバーライド適用前の土台）。API の GET は
        // DB 行が無いときこれを "toml" として返すため、リセット後に古い実効値を
        // TOML と誤表示しないよう effective ではなく cfg.voice を入れる（レビュー指摘）。
        voice_config: Arc::new(cfg.voice.clone()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: cfg.agent.workspace_path.clone(),
        tools_config: Arc::new(std::sync::RwLock::new(tools_cfg)),
        default_model,
        compaction_ratio: cfg.llm.compaction_ratio,
        evaluator: cfg.evaluator.clone(),
        skill_consolidation: cfg.skill_consolidation.clone(),
        loop_restart_enabled: cfg.agent.loop_restart_enabled,
        index_build_inflight: Arc::new(dashmap::DashMap::new()),
        #[cfg(feature = "discord")]
        discord_manager: None,
        nostr_manager: None,
        mcp_manager: None,
        web_gateway: Arc::new(opencrab_web_gateway::WebGateway::new()),
        subtask_registries: Arc::new(opencrab_server::subtask_registries::SubtaskRegistries::new()),
        progress_debounce: Arc::new(opencrab_server::subtask_registries::ProgressDebounce::new()),
        subtask_notifiers: Arc::new(dashmap::DashMap::new()),
        subtask_lifecycle_notifier: Arc::new(Mutex::new(None)),
        // 設定ファイル由来の通知先フォールバック（#157 S5）。**Discord 機能フラグの
        // 外**で 1 度だけ解決し、以降の利用者（gateway 非依存の管理ツール / lifecycle
        // 通知 / Discord gateway_actions）は全てこの 1 つの値を参照する。
        default_subtask_webhook: cfg.default_subtask_webhook(),
    };

    // サブタスク lifecycle 通知の実装を配線する（#175 S4）。`spawn_subtask` は gateway
    // 非依存層にあるため、通知先の解決（DB の webhook 設定 + TOML の既定）だけを持つ
    // この実装を `AppState` へ差し込む。Discord ゲートウェイの稼働有無とは独立に効く
    // （web / REST から起動したサブタスクにも lifecycle 通知が出る）。
    #[cfg(feature = "discord")]
    {
        let default_subtask_webhook = state.default_subtask_webhook.clone();
        *state.subtask_lifecycle_notifier.lock().unwrap() = Some(Arc::new(
            opencrab_discord::DiscordWebhookNotifier::new(
                state.db.clone(),
                default_subtask_webhook,
            ),
        )
            as Arc<dyn opencrab_actions::subtask_notify::SubtaskLifecycleNotifier>);
    }

    // 前プロセスから残った保留対話を**期限切れとして明示的に閉じる**（#196）。
    // 保留状態のメモリ上の登録簿はプロセスと寿命を共にするので、ここに残っている
    // `pending` 行は誰も応答を受け取れない。無言で放置すると「ボタンを押しても何も
    // 起きない」行が DB に溜まり続けるため、起動時に 1 度だけ閉じてログに残す。
    // transport に依存しない処理なので、Discord 機能フラグやゲートウェイの稼働有無の
    // **外**で行う（nostr / web / REST だけの構成でも効く）。
    {
        use opencrab_actions::AgentRuntime as _;
        state.cleanup_stale_interactions();
    }

    #[cfg(feature = "discord")]
    let heartbeat_discord_http: Arc<Mutex<Option<Arc<serenity::http::Http>>>> =
        Arc::new(Mutex::new(None));
    #[cfg(not(feature = "discord"))]
    let heartbeat_discord_http: Arc<Mutex<Option<()>>> = Arc::new(Mutex::new(None));
    let heartbeat_channel_id_arc: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

    // Start Discord gateway if configured and feature is enabled.
    #[cfg(feature = "discord")]
    {
        // Per-agent Discord gateway manager（#40: 共有ループが「専用ゲートウェイが
        // 稼働中か」を参照できるよう、共有ゲートウェイへ渡す AppState clone より
        // **前に**生成して配線する。実際の復元は共有ゲートウェイ起動後に行う）。
        let manager = Arc::new(opencrab_discord::DiscordGatewayManager::new(state.clone()));
        state.discord_manager = Some(manager.clone());

        let discord_cfg = &cfg.gateway.discord;

        // owner 未設定は「無音で権限モデルが変わる」ので起動時に必ず知らせる。
        // `.env` の OWNER_DISCORD_ID を入れ忘れると `${OWNER_DISCORD_ID}` が空文字に
        // 展開され、設定ファイルを見ても気づけない。
        // 共有ゲートウェイが実際に起動する条件（enabled かつトークンあり）でだけ出す。
        // per-agent ゲートウェイ側の警告は DiscordGatewayManager::start_agent_gateway が出す。
        opencrab_discord::warn_if_shared_gateway_owner_unset(
            discord_cfg.enabled,
            &discord_cfg.token,
            &discord_cfg.owner_discord_id,
        );

        // Fallback: config-based shared gateway (existing behavior).
        // 起動条件は警告条件と同じ述語を使う（条件の二重管理を避ける。理由は
        // `gateway_will_start` の doc コメント参照）。
        if opencrab_discord::gateway_will_start(discord_cfg.enabled, &discord_cfg.token) {
            tracing::info!("Starting Discord gateway (config-based fallback)...");

            // Validate agent IDs against the database
            let valid_agent_ids: Vec<String> = {
                let conn = state.db.lock().unwrap();
                let ids: Vec<String> = discord_cfg
                    .agent_ids
                    .iter()
                    .map(|agent_id| resolve_agent_id(&conn, agent_id))
                    .filter(
                        |agent_id| match opencrab_db::queries::get_agent(&conn, agent_id) {
                            Ok(Some(_)) => true,
                            _ => {
                                tracing::warn!(
                                    "Agent '{}' not found in database, skipping",
                                    agent_id
                                );
                                false
                            }
                        },
                    )
                    .collect();
                // #40: enabled な per-agent Discord 設定を持つエージェントは、専用
                // ゲートウェイの**稼働中**は共有ループが per-message でスキップする
                // （liveness ベース）。ここでリストから除外はしない: 専用側が起動失敗
                // した場合に共有側がフォールバックとして応答を続けるため。
                for agent_id in &ids {
                    if matches!(
                        opencrab_db::queries::get_agent_discord_config(&conn, agent_id),
                        Ok(Some(cfg)) if cfg.enabled
                    ) {
                        tracing::info!(
                            agent_id = %agent_id,
                            "Agent has an enabled per-agent Discord config; shared gateway \
                             will defer to it while its dedicated gateway is running"
                        );
                    }
                }
                ids
            };

            if valid_agent_ids.is_empty() {
                tracing::error!("No valid agents found for Discord gateway, not starting");
            } else {
                let gateway = Arc::new(opencrab_gateway::DiscordGateway::new(&discord_cfg.token));
                gateway.start().await?;

                // auto-dispatch の登録簿。停止（`cancel_subtask`）は gateway 非依存層の実装が
                // 同じ Arc を run 経由（`RunRequest::with_dispatch`）で受け取るため、この
                // registry はループへ渡すだけでよい（#157 S2 で gateway_actions からは外した）。
                let subtask_registry: opencrab_actions::SubtaskRegistry =
                    Arc::new(dashmap::DashMap::new());
                let subtask_registry_for_loop = subtask_registry.clone();
                // subtask 完了/進捗の通知はイベントループへの直接送信になった（#39）ため、
                // gateway_actions とループで同じチャンネルを共有する必要がある。
                let (event_tx, event_rx) = opencrab_discord::message_loop::create_event_channel();
                // 設定ファイル由来の通知先フォールバック（#157 S5 で `AppState` へ
                // 持ち上げ済み）。Discord にはもう `ensure_*` しか残っていないが、
                // 解決経路が全 transport で同じ値を見ることをここで担保する。
                let default_subtask_webhook = state.default_subtask_webhook.clone();
                // VC 対話（STT/TTS）: 実効設定（DB オーバーライド適用済み）で構築する。
                // プロバイダ構築失敗（未知の provider 等）は起動を止めず警告して無効化。
                let voice_cfg = &effective_voice;
                let voice_manager: Option<
                    Arc<opencrab_discord::voice_session::VoiceSessionManager>,
                > = if voice_cfg.enabled {
                    match (
                        opencrab_voice::build_stt(&voice_cfg.stt),
                        opencrab_voice::build_tts(&voice_cfg.tts),
                    ) {
                        (Ok(stt), Ok(tts)) => {
                            tracing::info!(
                                stt = %voice_cfg.stt.provider,
                                tts = %voice_cfg.tts.provider,
                                "voice (VC) conversation enabled"
                            );
                            let mgr = opencrab_discord::voice_session::VoiceSessionManager::new(
                                gateway.voice(),
                                stt,
                                tts,
                                voice_cfg.tts.clone(),
                                voice_cfg.stt.language.clone(),
                                event_tx.clone(),
                                gateway.http().clone(),
                            );
                            // ダッシュボードからの設定変更をホットスワップで受ける
                            *state.voice_runtime.lock().unwrap() =
                                Some(mgr.clone() as Arc<dyn opencrab_voice::VoiceRuntime>);
                            Some(mgr)
                        }
                        (stt, tts) => {
                            if let Err(e) = stt {
                                tracing::warn!("voice STT provider init failed: {e}");
                            }
                            if let Err(e) = tts {
                                tracing::warn!("voice TTS provider init failed: {e}");
                            }
                            None
                        }
                    }
                } else {
                    None
                };

                let gateway_actions_base = opencrab_discord::DiscordGatewayActions::new(
                    gateway.http().clone(),
                    state.db.clone(),
                    state.workspace_base.clone(),
                    default_subtask_webhook,
                )
                .with_event_tx(event_tx.clone())
                .with_owner_discord_id(discord_cfg.owner_discord_id.clone());
                let gateway_actions: Arc<dyn opencrab_gateway::GatewayActions> =
                    Arc::new(match &voice_manager {
                        Some(v) => gateway_actions_base.with_voice(v.clone()),
                        None => gateway_actions_base,
                    });

                *heartbeat_discord_http.lock().unwrap() = Some(gateway.http().clone());

                let discord_state = state.clone();
                let owner_discord_id = discord_cfg.owner_discord_id.clone();
                tokio::spawn(async move {
                    opencrab_discord::run_discord_loop(
                        gateway,
                        discord_state,
                        valid_agent_ids,
                        gateway_actions,
                        owner_discord_id,
                        None, // pending_registry
                        Some((event_tx, event_rx)),
                        // 共有（TOML）ゲートウェイ: ランタイムに per-agent 設定が
                        // enable されたエージェントはメッセージ処理時にスキップ（#40）。
                        true,
                        voice_manager,
                        subtask_registry_for_loop,
                    )
                    .await;
                });
                if let Some(ch_id) = discord_cfg.heartbeat_channel_id {
                    *heartbeat_channel_id_arc.lock().unwrap() = Some(ch_id);
                }

                tracing::info!(
                    agents = ?discord_cfg.agent_ids,
                    owner = %discord_cfg.owner_discord_id,
                    "Discord gateway started (config-based)"
                );
            }
        }

        // DB から per-agent ゲートウェイを復元する（manager 自体はこのブロック冒頭で
        // 生成・配線済み。復元は共有ゲートウェイ起動後: 起動直後の短い窓では共有側が
        // メッセージを処理し、専用ゲートウェイが上がり次第 per-message スキップが効く）。
        manager.restore_from_db().await;

        // Per-agentゲートウェイのHTTPクライアントをheartbeatに設定
        let heartbeat_agent_id_for_http = {
            let conn = state.db.lock().unwrap();
            cfg.gateway
                .discord
                .agent_ids
                .first()
                .map(|id| resolve_agent_id(&conn, id))
                .unwrap_or_default()
        };
        if let Some(http) = manager.get_http_for_agent(&heartbeat_agent_id_for_http) {
            *heartbeat_discord_http.lock().unwrap() = Some(http);
            tracing::info!(agent_id = %heartbeat_agent_id_for_http, "Set heartbeat Discord HTTP from per-agent gateway");
        }
        if let Some(ch_id) = cfg.gateway.discord.heartbeat_channel_id {
            *heartbeat_channel_id_arc.lock().unwrap() = Some(ch_id);
            tracing::info!(channel_id = %ch_id, "Set heartbeat channel ID from config");
        }

        tracing::info!("Per-agent Discord gateway manager initialized");
    }

    // メモリインデックスのアイドル時メンテナンス（増分ビルドの取りこぼし回収 /
    // キーワードバックフィル / 月次ロールアップ）。全エージェントを毎 tick 巡回。
    if cfg.agent.memory_maintenance_enabled {
        opencrab_server::memory_maintenance::spawn_memory_maintenance_loop(
            state.clone(),
            cfg.agent.memory_maintenance_interval_secs,
        );
    }

    // ハートビートの初期設定
    let initial_hb_config = HeartbeatConfig {
        interval_secs: cfg.agent.heartbeat_interval_secs,
        enabled: cfg.agent.heartbeat_enabled,
        heartbeat_channel_id: cfg.gateway.discord.heartbeat_channel_id,
    };

    let (heartbeat_config_tx, mut heartbeat_config_rx) = watch::channel(initial_hb_config.clone());

    let agent_ids: Vec<String> = {
        #[cfg(feature = "discord")]
        {
            cfg.gateway.discord.agent_ids.clone()
        }
        #[cfg(not(feature = "discord"))]
        {
            vec!["default".to_string()]
        }
    };

    // heartbeat設定変更を監視してループを再起動するタスク
    let heartbeat_agent_ids = agent_ids.clone();
    let heartbeat_db = state.db.clone();
    let heartbeat_state = state.clone();
    tokio::spawn(async move {
        let mut prev_config = initial_hb_config.clone();
        let mut current_shutdown_tx: Option<watch::Sender<bool>> = None;
        let mut _handles: Vec<tokio::task::JoinHandle<()>> = vec![];

        // ハートビートループを起動するヘルパークロージャ的ブロック
        // 初期起動
        if prev_config.enabled {
            tracing::info!(
                agent_ids = ?heartbeat_agent_ids,
                interval_secs = prev_config.interval_secs,
                "Starting heartbeat loops"
            );
            let (tx, rx_tmpl) = watch::channel(false);
            for agent_id in &heartbeat_agent_ids {
                let config_clone = prev_config.clone();
                let shutdown_rx = rx_tmpl.clone();
                let db = heartbeat_db.clone();
                let db_for_resolve = heartbeat_db.clone();
                let agent_id = agent_id.clone();
                let hb_discord_http = heartbeat_discord_http.clone();
                let _hb_channel_id = heartbeat_channel_id_arc.clone();
                let state_for_hb = heartbeat_state.clone();
                let last_channel_ticks = Arc::new(Mutex::new(std::collections::HashMap::<
                    String,
                    std::time::Instant,
                >::new()));
                _handles.push(tokio::spawn(async move {
                    let resolved_agent_id = if let Ok(conn) = db_for_resolve.lock() {
                        resolve_agent_id(&conn, &agent_id)
                    } else {
                        agent_id.clone()
                    };
                    let callback = make_heartbeat_callback(
                        db,
                        resolved_agent_id,
                        hb_discord_http,
                        state_for_hb,
                        config_clone.interval_secs,
                        last_channel_ticks,
                    );
                    heartbeat_loop(agent_id, config_clone, callback, shutdown_rx).await;
                }));
            }
            current_shutdown_tx = Some(tx);
        } else {
            tracing::info!("Heartbeat disabled (heartbeat_enabled = false in config)");
        }

        loop {
            if heartbeat_config_rx.changed().await.is_err() {
                break; // sender dropped
            }
            let new_config = heartbeat_config_rx.borrow().clone();
            if new_config.enabled != prev_config.enabled
                || new_config.interval_secs != prev_config.interval_secs
                || new_config.heartbeat_channel_id != prev_config.heartbeat_channel_id
            {
                tracing::info!(
                    enabled = new_config.enabled,
                    interval_secs = new_config.interval_secs,
                    "Heartbeat config changed, restarting loops"
                );
                // 既存ループを停止
                if let Some(tx) = current_shutdown_tx.take() {
                    let _ = tx.send(true);
                }
                _handles.clear();

                // heartbeat_channel_id_arcも更新
                if let Ok(mut guard) = heartbeat_channel_id_arc.lock() {
                    *guard = new_config.heartbeat_channel_id;
                }

                // 新設定で起動
                if new_config.enabled {
                    let (tx, rx_tmpl) = watch::channel(false);
                    for agent_id in &heartbeat_agent_ids {
                        let config_clone = new_config.clone();
                        let shutdown_rx = rx_tmpl.clone();
                        let db = heartbeat_db.clone();
                        let db_for_resolve = heartbeat_db.clone();
                        let agent_id = agent_id.clone();
                        let hb_discord_http = heartbeat_discord_http.clone();
                        let _hb_channel_id = heartbeat_channel_id_arc.clone();
                        let state_for_hb = heartbeat_state.clone();
                        let last_channel_ticks = Arc::new(Mutex::new(std::collections::HashMap::<
                            String,
                            std::time::Instant,
                        >::new(
                        )));
                        _handles.push(tokio::spawn(async move {
                            let resolved_agent_id = if let Ok(conn) = db_for_resolve.lock() {
                                resolve_agent_id(&conn, &agent_id)
                            } else {
                                agent_id.clone()
                            };
                            let callback = make_heartbeat_callback(
                                db,
                                resolved_agent_id,
                                hb_discord_http,
                                state_for_hb,
                                config_clone.interval_secs,
                                last_channel_ticks,
                            );
                            heartbeat_loop(agent_id, config_clone, callback, shutdown_rx).await;
                        }));
                    }
                    current_shutdown_tx = Some(tx);
                }
                prev_config = new_config;
            }
        }
    });

    let _watcher_handle = opencrab_server::hot_reload::start_config_watcher(
        "config",
        state.tools_config.clone(),
        heartbeat_config_tx,
    );

    // Per-agent Nostr sub-gateway マネージャ（discord と同様に、state clone より前に
    // 生成して配線する。nostr は重い依存が無いので feature ゲート無しの常時配線）。
    {
        let manager: opencrab_server::SharedNostrManager =
            Arc::new(opencrab_nostr::NostrGatewayManager::new(state.clone()));
        state.nostr_manager = Some(manager.clone());
        manager.restore_from_db().await;
    }

    // Per-agent MCP 接続マネージャ。enabled なサーバへ起動時に接続する。
    {
        let manager: opencrab_server::SharedMcpManager =
            Arc::new(opencrab_mcp::McpClientManager::new(state.db.clone()));
        state.mcp_manager = Some(manager.clone());
        manager.restore_from_db().await;
        // 自己修復: 切断された（クラッシュ/終了した）サーバを周期的に再接続する。
        let sweeper = manager.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.tick().await; // 最初の即時発火を捨てる
            loop {
                tick.tick().await;
                sweeper.reconnect_dead().await;
            }
        });
    }

    let app = create_router(state);

    let addr = format!("0.0.0.0:{}", cfg.gateway.rest.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Server listening on {}", addr);
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_server::subtask_registries::SubtaskRegistries;

    /// #169: heartbeat の `RunRequest` に非ブロック dispatch が配線される
    /// （`completion_sink` が Some のときだけ `run_agent_response` が dispatcher を注入する）。
    #[test]
    fn heartbeat_run_request_enables_dispatch() {
        let registries = SubtaskRegistries::new();
        let req = heartbeat_run_request(
            &registries,
            "agent-a",
            "A",
            "heartbeat-agent-a-123",
            "sys",
            "conv",
        );
        assert!(
            req.completion_sink.is_some(),
            "heartbeat は dispatch を有効化する（sink 未配線だと全ツール inline 実行）"
        );
        assert!(
            req.subtask_registry.is_some(),
            "registry を渡さないと run 内で使い捨てが作られ cancel_subtask が not found になる"
        );
        assert_eq!(req.gateway, "heartbeat");
    }

    /// #169: registry は **agent 単位**で共有される。tick を跨いで同一 Arc なので、
    /// 前 tick で dispatch した subtask を後続 tick の `cancel_subtask` が引ける。
    #[test]
    fn heartbeat_registry_is_shared_across_ticks_per_agent() {
        let registries = SubtaskRegistries::new();
        // 同一エージェントの別 tick（チャンネル違いで session_id も違う）。
        let tick1 =
            heartbeat_run_request(&registries, "agent-a", "A", "heartbeat-agent-a-1", "", "");
        let tick2 =
            heartbeat_run_request(&registries, "agent-a", "A", "heartbeat-agent-a-2", "", "");
        let r1 = tick1.subtask_registry.unwrap();
        let r2 = tick2.subtask_registry.unwrap();
        assert!(
            std::sync::Arc::ptr_eq(&r1, &r2),
            "同一エージェントの tick は同じ registry を共有する"
        );

        // 別エージェントは独立（他エージェントの subtask が混ざらない）。
        let other =
            heartbeat_run_request(&registries, "agent-b", "B", "heartbeat-agent-b-1", "", "");
        assert!(!std::sync::Arc::ptr_eq(
            &r1,
            &other.subtask_registry.unwrap()
        ));
    }

    /// #169: heartbeat の sink は再注入しない（`NoopCompletionSink`）。
    /// 呼んでも resume を起こさず、完了本文は次 tick の会話再構築で拾う。
    #[tokio::test]
    async fn heartbeat_sink_does_not_reinject() {
        use opencrab_actions::{SettleKind, SubtaskSettled};

        let registries = SubtaskRegistries::new();
        let req = heartbeat_run_request(&registries, "agent-a", "A", "heartbeat-agent-a-1", "", "");
        // Noop sink は呼んでも副作用が無い（panic せず、resume も配送もしない）。
        req.completion_sink
            .unwrap()
            .on_subtask_settled(SubtaskSettled {
                session_id: "heartbeat-agent-a-1".to_string(),
                agent_id: "agent-a".to_string(),
                subtask_id: "st-1".to_string(),
                exit_reason: "completed".to_string(),
                kind: SettleKind::Completed,
                reply_target: None,
            });
    }
}
