use std::sync::Arc;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

use opencrab_core::heartbeat::{
    heartbeat_loop, HeartbeatCallback, HeartbeatConfig, HeartbeatDecision,
};
use opencrab_server::{config, create_router, AppState};
use tokio::sync::watch;

mod heartbeat_delivery;

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

/// エージェント単位 tick（channel を持たない発話）のプロンプト内呼称。
/// 場所の呼称は transport 中立にする（#158 S2 と同方針）。
const HEARTBEAT_AGENT_SCOPED_LABEL: &str = "（自律ハートビート）";

/// tick の発火計画（#238 の precedence）。純粋関数 [`heartbeat_firing_plan`] が返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeartbeatFiringPlan {
    /// opt-in 済み: エージェント単位で 1 回発火し、既存の channel 発火はスキップする。
    /// `interval_secs` は resolve が返した実効間隔（#251 段階3）。
    AgentScoped { interval_secs: u64 },
    /// 未 opt-in かつグローバル有効: 従来どおり channel 単位で発火する（互換・不変）。
    ChannelScoped,
    /// 未 opt-in かつグローバル無効: 発火しない（他エージェントの opt-in のためだけに
    /// 立っているループ。既定無効エージェントは 1 バイトも挙動が変わらない）。
    None,
}

/// tick の発火計画を決める（#238 の precedence 判定・純粋関数）。
///
/// opt-in（`resolved_enabled`）が最優先。opt-in 済みなら **channel 発火とは排他**に
/// エージェント単位で 1 回だけ発火する（同一 tick で二度喋らせない）。未 opt-in は
/// グローバル有効時のみ従来の channel 発火。
fn heartbeat_firing_plan(
    resolved_enabled: bool,
    resolved_interval_secs: u64,
    global_enabled: bool,
) -> HeartbeatFiringPlan {
    if resolved_enabled {
        HeartbeatFiringPlan::AgentScoped {
            interval_secs: resolved_interval_secs,
        }
    } else if global_enabled {
        HeartbeatFiringPlan::ChannelScoped
    } else {
        HeartbeatFiringPlan::None
    }
}

/// 発火ループの sleep 周期（秒）を丸める（純粋関数）。0 は `heartbeat_loop` の
/// `sleep(0)` = 0 秒周期のビジーループになるため下限 1 秒に丸める（運用者が
/// `heartbeat_interval_secs = 0` を書いても最低 1 秒 sleep させる）。初期起動・reload の
/// 両経路で同じ丸めを使う。
fn heartbeat_loop_interval_secs(configured: u64) -> u64 {
    configured.max(1)
}

/// 前回発火からの経過が `interval_secs` 以上かを判定する（純粋関数）。
/// 未発火（`last` が `None`）なら常に発火。エージェント単位 tick も channel tick も同じ
/// ゲートを使い、`interval_secs` に resolve の値を与えることで「保存済み間隔を実際に
/// 効かせる」（#251 段階3）。
fn heartbeat_interval_elapsed(
    last: Option<std::time::Instant>,
    now: std::time::Instant,
    interval_secs: u64,
) -> bool {
    match last {
        None => true,
        Some(last_time) => now.duration_since(last_time).as_secs() >= interval_secs,
    }
}

/// heartbeat_enabled = true のチャンネルを、当該エージェント向けに解決して返す
/// （channel_id, channel_name, interval_secs）。グローバル設定（agent_id="")と
/// エージェント固有設定の両方が同一 channel_id に存在しうるため、(1) 当該エージェント
/// に無関係な行を除外し、(2) 同一 channel_id ではエージェント固有行をグローバル行より
/// 優先して重複処理を防ぐ。
fn list_whitelisted_heartbeat_channels(
    db: &opencrab_db::Db,
    agent_id: &str,
) -> Vec<(String, String, Option<u64>)> {
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
                if !c.agent_id.is_empty() && c.agent_id != agent_id {
                    continue;
                }
                match selected.get(&c.channel_id) {
                    // 既にエージェント固有行を選択済みならグローバル行で上書きしない。
                    Some(existing) if !existing.agent_id.is_empty() && c.agent_id.is_empty() => {
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
}

/// ハートビートコールバックを生成する。
/// 初期起動とhot-reload再起動の両方で使用。
fn make_heartbeat_callback(
    db: opencrab_db::Db,
    agent_id_owned: String,
    discord_http: DiscordHttpArc,
    state: AppState,
    global_interval_secs: u64,
    // グローバル `[agent] heartbeat_enabled` の実値（ループ起動時に強制 true へ倒す前の
    // 素の値）。未 opt-in エージェントの channel 単位発火は**これが true のときだけ**
    // 許す。グローバル無効下でループが立つのは他エージェントの opt-in のためだけであり、
    // その巻き添えで既定無効エージェントが channel 発火してはならない（#238 の挙動不変）。
    global_enabled: bool,
    last_channel_ticks: Arc<Mutex<std::collections::HashMap<String, std::time::Instant>>>,
) -> HeartbeatCallback {
    Box::new(move |_agent_id: &str, tick: u64| {
        let _agent_id = _agent_id.to_string();
        let db = db.clone();
        let agent_id_owned = agent_id_owned.clone();
        let discord_http = discord_http.clone();
        let state = state.clone();
        let global_interval_secs = global_interval_secs;
        let global_enabled = global_enabled;
        let last_channel_ticks = last_channel_ticks.clone();
        Box::pin(async move {
            // #238 / #251 段階3: エージェント単位ハートビートの解決（fail-closed）。
            // opt-in 済み（resolved.enabled）なら「エージェント単位 tick」で 1 回だけ
            // 発火し、既存の channel 単位発火はこのエージェントについてスキップする
            // （二重発火防止の precedence）。未 opt-in なら従来の channel 単位発火のまま
            // （既定無効エージェントは 1 バイトも挙動が変わらない）。
            let resolved = {
                let conn = db.lock().unwrap();
                opencrab_db::queries::resolve_agent_heartbeat(
                    &conn,
                    &agent_id_owned,
                    state.heartbeat_limits.default_interval_secs,
                    state.heartbeat_limits.min_interval_secs,
                )
            };

            // 発火対象（channel_id, channel_name, interval_secs）。channel_id が空文字なら
            // 「エージェント単位 tick」（channel を持たない発話）を表す。
            let targets: Vec<(String, String, Option<u64>)> = match heartbeat_firing_plan(
                resolved.enabled,
                resolved.interval_secs,
                global_enabled,
            ) {
                HeartbeatFiringPlan::AgentScoped { interval_secs } => {
                    // opt-in 済み: エージェント単位で 1 回発火。channel 発火はしない。
                    // 間隔は resolve の値（#251 段階3 の「保存済み間隔を実際に効かせる」）。
                    // 宛先 channel_id は空。deliver_heartbeat_speech が Nostr 稼働なら
                    // registry 経由で Nostr へ、Discord 共有 http フォールバックは空 channel
                    // で「ログのみ」に縮退する（Discord エージェントの代表 channel 選択は
                    // 別 PR / スコープ外）。
                    vec![(
                        String::new(),
                        HEARTBEAT_AGENT_SCOPED_LABEL.to_string(),
                        Some(interval_secs),
                    )]
                }
                HeartbeatFiringPlan::ChannelScoped => {
                    // 未 opt-in かつグローバル有効: 従来の channel 単位発火（互換・不変）。
                    list_whitelisted_heartbeat_channels(&db, &agent_id_owned)
                }
                HeartbeatFiringPlan::None => {
                    // 未 opt-in かつグローバル無効: 何もしない。このループは他エージェント
                    // の opt-in のために立っているだけで、既定無効エージェントは発火しない。
                    vec![]
                }
            };

            if targets.is_empty() {
                tracing::debug!(agent_id = %agent_id_owned, tick, "No heartbeat targets, skipping tick");
                return HeartbeatDecision::Idle;
            }

            // 最後の決定を返す（全ターゲットを処理した後）
            let mut last_decision = HeartbeatDecision::Idle;

            for (channel_id_str, channel_name, channel_interval_secs) in &targets {
                // per-target interval チェック。エージェント単位 tick（空 channel_id）は
                // last_channel_ticks を channel_id と衝突しない合成キーで引く。
                let tick_key = if channel_id_str.is_empty() {
                    format!("agent:{agent_id_owned}")
                } else {
                    channel_id_str.clone()
                };
                let effective_interval = channel_interval_secs.unwrap_or(global_interval_secs);
                let should_fire = {
                    let ticks = last_channel_ticks.lock().unwrap();
                    heartbeat_interval_elapsed(
                        ticks.get(tick_key.as_str()).copied(),
                        std::time::Instant::now(),
                        effective_interval,
                    )
                };
                if !should_fire {
                    tracing::debug!(
                        agent_id = %agent_id_owned,
                        channel_id = %channel_id_str,
                        effective_interval,
                        "Heartbeat: interval not elapsed, skipping"
                    );
                    continue;
                }
                // last_tickを更新
                {
                    let mut ticks = last_channel_ticks.lock().unwrap();
                    ticks.insert(tick_key.clone(), std::time::Instant::now());
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
                        // 発話出口（段階3 PR-A / #246）。まず登録簿（`state.gateways`）の
                        // 非 Discord transport を試し、配れなければ既存の Discord 共有 http
                        // 経路へ落ちる。Discord の挙動はバイト単位で不変（詳細は
                        // `heartbeat_delivery` モジュール doc）。fire-and-forget で発火 tick を
                        // 塞がない（#178 系）。
                        let content = content.clone();
                        let discord_http = discord_http.clone();
                        let state = state.clone();
                        let agent_id_log = agent_id_owned.clone();
                        let ch_id_str = channel_id_str.clone();
                        tokio::spawn(async move {
                            heartbeat_delivery::deliver_heartbeat_speech(
                                &state.gateways,
                                &discord_http,
                                &agent_id_log,
                                &ch_id_str,
                                &content,
                            )
                            .await;
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
        mcp_manager: None,
        // 受信を持つ transport の登録簿（#191 段階2 PR2）。空で作り、各マネージャの
        // 生成箇所から後で `register` する（内部可変なので生成順を変えずに済む）。
        gateways: Arc::new(opencrab_actions::AgentGatewayRegistry::new()),
        web_gateway: Arc::new(opencrab_web_gateway::WebGateway::new()),
        subtask_registries: Arc::new(opencrab_server::subtask_registries::SubtaskRegistries::new()),
        progress_debounce: Arc::new(opencrab_server::subtask_registries::ProgressDebounce::new()),
        subtask_notifiers: Arc::new(dashmap::DashMap::new()),
        subtask_lifecycle_notifier: Arc::new(Mutex::new(None)),
        // 設定ファイル由来の通知先フォールバック（#157 S5）。**Discord 機能フラグの
        // 外**で 1 度だけ解決し、以降の利用者（gateway 非依存の管理ツール / lifecycle
        // 通知 / Discord gateway_actions）は全てこの 1 つの値を参照する。
        default_subtask_webhook: cfg.default_subtask_webhook(),
        // エージェントが自分で触るハートビート設定の境界（#247）。下限は運用者が
        // `[agent] heartbeat_min_interval_secs` で決める。
        heartbeat_limits: cfg.agent.heartbeat_limits(),
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

    // Start Discord gateway if configured and feature is enabled.
    #[cfg(feature = "discord")]
    {
        // Per-agent Discord gateway manager（#40: 共有ループが「専用ゲートウェイが
        // 稼働中か」を参照できるよう、共有ゲートウェイへ渡す AppState clone より
        // **前に**生成して配線する。実際の復元は共有ゲートウェイ起動後に行う）。
        let manager = Arc::new(opencrab_discord::DiscordGatewayManager::new(state.clone()));
        // 上位から見える唯一の入口はこの登録簿（#191 段階2 PR3・PR4）。共通操作
        // （起動 / 停止 / 生存確認）も transport 固有の操作（ツール実行の実体 =
        // `gateway_actions_for`）もここから引く。`AppState` の名指しフィールドは無い。
        // 登録簿は `state` の clone 同士で同じ Arc を共有するので、ここで入れた分は
        // 既に clone 済みの state からも見える（#40 の二重処理防止がこれに依存する）。
        state.gateways.register(manager.clone());

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

                tracing::info!(
                    agents = ?discord_cfg.agent_ids,
                    owner = %discord_cfg.owner_discord_id,
                    "Discord gateway started (config-based)"
                );
            }
        }

        // **1 つ目の復元位置。** ここまでに登録簿へ入っていて、まだ復元していない
        // ゲートウェイを登録順に復元する（#191 段階2 PR5）。この時点で登録済みなのは
        // 上で登録した 1 つだけなので、実際に走る内容は移設前の
        // `manager.restore_from_db()` と 1 対 1。
        //
        // **この位置は動かせない**（走査を最後の 1 回に畳めない理由でもある）:
        // 1. 復元は共有（TOML）ゲートウェイの**起動後**。起動直後の短い窓では共有側が
        //    メッセージを処理し、専用ゲートウェイが上がり次第 per-message スキップが効く。
        // 2. すぐ下の heartbeat 用 HTTP クライアントの取得が、この復元の**完了**に
        //    依存する（復元が後ろへずれると per-agent ゲートウェイがまだ無く、
        //    heartbeat の発話が共有ゲートウェイの HTTP のままになる）。
        state.gateways.restore_pending().await;

        // Per-agentゲートウェイのHTTPクライアントをheartbeatに設定
        //
        // **ここは登録簿の走査に畳んでいない**（#191 段階2 PR5）。必要なのは
        // `Arc<serenity::http::Http>` そのもので、heartbeat の発話経路（`SPEAK:`）が
        // serenity の API を直に叩く Discord 専用コードだから。PR4 の capability
        // （`gateway_actions_for`）が返すのは `GatewayActions` であって生の HTTP
        // クライアントではなく、ここに当てると発話経路ごと書き換えになる（挙動不変で
        // なくなる）。transport 中立化は heartbeat 側の課題として残す。
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
    };

    let (heartbeat_config_tx, mut heartbeat_config_rx) = watch::channel(initial_hb_config.clone());

    let agent_ids: Vec<String> = {
        let base: Vec<String> = {
            #[cfg(feature = "discord")]
            {
                cfg.gateway.discord.agent_ids.clone()
            }
            #[cfg(not(feature = "discord"))]
            {
                vec!["default".to_string()]
            }
        };
        // #238: エージェント単位ハートビートに opt-in 済み（agent_heartbeat_config で
        // enabled）のエージェントにも発火ループを立てる。これで Discord チャンネルに
        // 紐づかない Nostr 専用エージェントにもループが立つ。config 由来の id は
        // 名前かもしれないので resolve して UUID で重複除去する（同一エージェントに
        // 二重にループを立てて二重発火させない）。DB 有効化の**動的**反映はスコープ外
        // （起動時列挙のみ。反映は再起動時 / #251 の set_my_heartbeat 応答と整合）。
        if let Ok(conn) = state.db.lock() {
            let mut covered: std::collections::HashSet<String> =
                base.iter().map(|id| resolve_agent_id(&conn, id)).collect();
            let mut out = base;
            match opencrab_db::queries::list_agents_with_heartbeat_enabled(&conn) {
                Ok(enabled_ids) => {
                    for id in enabled_ids {
                        let resolved = resolve_agent_id(&conn, &id);
                        if covered.insert(resolved) {
                            out.push(id);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to list heartbeat-enabled agents: {e}");
                }
            }
            out
        } else {
            base
        }
    };

    // #238: agent_heartbeat_config に enabled 行が 1 つでもあるか。グローバル無効でも
    // opt-in 済みエージェントが居ればループ群を起動する二段ゲートの上段（下段＝個々の
    // 発火可否は callback 内 resolve_agent_heartbeat が握る）。起動時 1 回だけ判定する
    // （動的反映はスコープ外）。
    let heartbeat_has_optin = match state.db.lock() {
        Ok(conn) => opencrab_db::queries::list_agents_with_heartbeat_enabled(&conn)
            .map(|v| !v.is_empty())
            .unwrap_or(false),
        Err(_) => false,
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
        // 初期起動。グローバル有効 OR opt-in 済みエージェントが居ればループ群を起動する。
        if prev_config.enabled || heartbeat_has_optin {
            tracing::info!(
                agent_ids = ?heartbeat_agent_ids,
                interval_secs = prev_config.interval_secs,
                global_enabled = prev_config.enabled,
                has_optin = heartbeat_has_optin,
                "Starting heartbeat loops"
            );
            let global_enabled = prev_config.enabled;
            let (tx, rx_tmpl) = watch::channel(false);
            for agent_id in &heartbeat_agent_ids {
                // core heartbeat_loop は config.enabled=false で即 return するため、起動する
                // ループの enabled は true に倒す。個々の発火可否（グローバル無効下の未
                // opt-in エージェントを黙らせる等）は callback 内で fail-closed に判定する。
                // interval_secs は下限 1 秒に丸める（0 だと heartbeat_loop の sleep が 0 秒
                // 周期のビジーループになる。運用者が 0 を書いても最低 1 秒 sleep させる）。
                let config_clone = HeartbeatConfig {
                    interval_secs: heartbeat_loop_interval_secs(prev_config.interval_secs),
                    enabled: true,
                };
                let shutdown_rx = rx_tmpl.clone();
                let db = heartbeat_db.clone();
                let db_for_resolve = heartbeat_db.clone();
                let agent_id = agent_id.clone();
                let hb_discord_http = heartbeat_discord_http.clone();
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
                        global_enabled,
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

                // 新設定で起動。初期起動と同じ二段ゲート（グローバル有効 OR opt-in）。
                if new_config.enabled || heartbeat_has_optin {
                    let global_enabled = new_config.enabled;
                    let (tx, rx_tmpl) = watch::channel(false);
                    for agent_id in &heartbeat_agent_ids {
                        // core の early-return 回避のため enabled は true に倒す（初期起動と同じ）。
                        // interval_secs は下限 1 秒に丸める（0 でビジーループ化を防ぐ・初期起動と同じ）。
                        let config_clone = HeartbeatConfig {
                            interval_secs: heartbeat_loop_interval_secs(new_config.interval_secs),
                            enabled: true,
                        };
                        let shutdown_rx = rx_tmpl.clone();
                        let db = heartbeat_db.clone();
                        let db_for_resolve = heartbeat_db.clone();
                        let agent_id = agent_id.clone();
                        let hb_discord_http = heartbeat_discord_http.clone();
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
                                global_enabled,
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
        // 共通操作も transport 固有の操作（nostaro の鍵生成 = `key_provisioning`）も
        // この登録簿から引く（#191 段階2 PR3・PR4）。名指しフィールドは無い。
        state.gateways.register(manager);
    }

    // **2 つ目の復元位置**（ルータ構築の直前 / #191 段階2 PR5）。ここまでで未復元なのは
    // 直前に登録した Nostr だけなので（Discord は上のブロックで復元済み・MCP は登録簿に
    // 入れない）、移設前の `manager.restore_from_db()` と 1 対 1。
    //
    // Discord を落とした構成（`--no-default-features`）では 1 つ目の走査ごと消えるため、
    // ここが唯一の復元位置になる。**新しい transport を足すときも呼び出し口は増えない**:
    // 復元させたい位置より前で `register` すればよい。
    state.gateways.restore_pending().await;

    // Per-agent MCP 接続マネージャ。enabled なサーバへ起動時に接続する。
    //
    // **transport 登録簿（`state.gateways`）には入れない**（#191 段階2）。MCP は受信を
    // 持たず、エージェントへ道具を供給する側で transport ではない。道具の注入は
    // 深さ 0（親ターン）限定という遮断が効いており、「受信を持つ transport」と同じ
    // 登録簿に混ぜるとその前提が崩れる。名指しフィールドのまま残す。
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

    // ── #238 発火のエージェント単位化: precedence と間隔ゲート ──────────────

    /// (b)(d) opt-in 済み（resolved.enabled）は **エージェント単位で発火**し、channel 発火
    /// はしない。間隔は resolve の値がそのまま実効間隔になる（#251 段階3）。
    #[test]
    fn firing_plan_optin_is_agent_scoped_and_uses_resolved_interval() {
        // グローバル有効でも無効でも、opt-in が最優先で AgentScoped。
        for global in [true, false] {
            assert_eq!(
                heartbeat_firing_plan(true, 900, global),
                HeartbeatFiringPlan::AgentScoped { interval_secs: 900 },
                "opt-in 済みは channel 発火より優先してエージェント単位で発火（二重発火なし）"
            );
        }
        // 実効間隔は resolve の値をそのまま反映する。
        assert_eq!(
            heartbeat_firing_plan(true, 1800, true),
            HeartbeatFiringPlan::AgentScoped {
                interval_secs: 1800
            }
        );
    }

    /// (c) 未 opt-in はグローバル有効時のみ従来の channel 発火（互換）。グローバル無効なら
    /// 何も発火しない（既定無効エージェントは挙動不変）。
    #[test]
    fn firing_plan_non_optin_preserves_legacy_behavior() {
        assert_eq!(
            heartbeat_firing_plan(false, 900, true),
            HeartbeatFiringPlan::ChannelScoped,
            "未 opt-in × グローバル有効 = 従来の channel 発火"
        );
        assert_eq!(
            heartbeat_firing_plan(false, 900, false),
            HeartbeatFiringPlan::None,
            "未 opt-in × グローバル無効 = 発火しない（挙動不変）"
        );
    }

    /// 指摘#4: 発火ループの sleep 周期は下限 1 秒に丸める（0 でビジーループ化しない）。
    #[test]
    fn loop_interval_floors_zero_to_one_second() {
        assert_eq!(
            heartbeat_loop_interval_secs(0),
            1,
            "0 は 0 秒 sleep のビジーループになるので 1 秒に丸める"
        );
        assert_eq!(heartbeat_loop_interval_secs(1), 1);
        assert_eq!(heartbeat_loop_interval_secs(29), 29, "1 以上はそのまま");
    }

    /// (d) 間隔ゲート: 未発火なら常に発火。経過が interval 以上で発火、未満はスキップ。
    #[test]
    fn interval_elapsed_gates_on_resolved_interval() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        // 未発火は常に発火。
        assert!(heartbeat_interval_elapsed(None, now, 300));
        // 100 秒前に発火・間隔 300 → まだ。
        let last = now - Duration::from_secs(100);
        assert!(!heartbeat_interval_elapsed(Some(last), now, 300));
        // 300 秒前に発火・間隔 300 → 発火（>=）。
        let last = now - Duration::from_secs(300);
        assert!(heartbeat_interval_elapsed(Some(last), now, 300));
        // 間隔を縮めれば（resolve の値が効く）同じ経過でも発火する。
        let last = now - Duration::from_secs(100);
        assert!(heartbeat_interval_elapsed(Some(last), now, 60));
    }
}
