use std::sync::Arc;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

use opencrab_core::heartbeat::{HeartbeatConfig, HeartbeatDecision};
use opencrab_server::{config, create_router, AppState};
use tokio::sync::watch;

mod heartbeat_delivery;
mod heartbeat_turn;
mod intake_process;
mod scheduler;

/// Discord へ 1 通送るためのハンドル。**ボットのトークンを保持するので、どのハンドルで
/// 送るかが Discord 上の送信者名を決める**（#400 の核心）。
#[cfg(feature = "discord")]
type DiscordHttp = Arc<serenity::http::Http>;
#[cfg(not(feature = "discord"))]
type DiscordHttp = ();

/// 共有（TOML）ゲートウェイのハンドル置き場。ゲートウェイ起動時に埋まる。
type DiscordHttpArc = Arc<Mutex<Option<DiscordHttp>>>;

/// ハートビート用セッションを取得または作成する。
fn get_or_create_heartbeat_session(
    db: &opencrab_db::Db,
    agent_id: &str,
    channel_id: &str,
) -> String {
    // 接頭辞は継続ターンの受け口（`heartbeat_turn::resume_origin`）が親セッションの判定に
    // 使う。書式が割れると「HB の決着なのに継続しない」が無言で起きるので定数を共有する。
    let session_id = format!(
        "{}{}-{}",
        heartbeat_turn::HEARTBEAT_SESSION_PREFIX,
        agent_id,
        channel_id
    );
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

/// ハートビート発話を、本人の実会話（`discord-…`）セッションへ `speech` として二重記録する
/// （#425 案 A）。
///
/// 背景: HB 発話は heartbeat 専用セッションにしか記録されず、自分の投稿が Discord から
/// `message` として戻ってきても `is_own_message` で捨てられるため、本人の会話セッションには
/// 1 行も残らない。結果、後続の通常返信ターン（`discord-…` セッションだけを読む）で本人だけ
/// 自分の HB 投稿を思い出せない。ここで配信成功時に会話セッションへも書くことでその欠落を塞ぐ。
///
/// - `delivered=false`（非 Discord transport が担当・配信失敗・配信先無し）のターンは記録
///   しない。**言っていないことを記憶に残さない**方向を守る。
/// - guild を解決できないエージェント単位 tick（`channel_id`/`guild_id` が空）や Nostr は、
///   記録先の Discord 会話セッションが無いので何もしない（Nostr の二重記録は #515 で別途）。
/// - `opencrab_db::queries::HEARTBEAT_CHANNEL_ECHO_METADATA` を印として付ける。この印の
///   付いた行は**表示専用**で、記憶系（FTS 検索・記憶索引・宣言材料）には一切載らない
///   （`is_heartbeat_channel_echo` で db/core が除外）。記憶材料は heartbeat 専用セッション
///   側が担っており、この PR の前後で記憶系の挙動は不変。HB 経路の文脈組み立ては専用
///   セッションと会話セッションの両方を読むため、この印で実会話セクション側の二重表示も
///   除外する（読み取り側は `process::build_channel_conversation_section`）。通常返信が
///   読む `process::build_conversation_string` は印を見ずに素通しするので、狙いの経路には
///   そのまま載る。
fn record_heartbeat_channel_echo(
    db: &opencrab_db::Db,
    delivered: bool,
    agent_id: &str,
    guild_id: &str,
    channel_id: &str,
    content: &str,
) {
    if !delivered {
        return;
    }
    // Discord 会話セッション（`discord-{agent}-{guild}-{channel}`）へだけ二重記録する。
    // guild/channel が空（エージェント単位 tick）や Nostr（両 ID 空）は記録先が無いので
    // 何もしない（#508 で実会話の解決を発火先種別へ寄せた後の Discord 専用の書き込み経路）。
    if guild_id.is_empty() || channel_id.is_empty() {
        return;
    }
    let session_id = opencrab_db::queries::SessionFireTarget::DiscordChannel {
        guild_id: guild_id.to_string(),
        channel_id: channel_id.to_string(),
    }
    .channel_session_id(agent_id);
    let Ok(conn) = db.lock() else {
        tracing::error!(agent_id = %agent_id, channel_id = %channel_id, "#425: db lock 取得に失敗し、HB 発話を会話セッションへ記録できなかった");
        return;
    };
    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: agent_id.to_string(),
        session_id,
        log_type: "speech".to_string(),
        content: content.to_string(),
        speaker_id: Some(agent_id.to_string()),
        turn_number: None,
        metadata_json: Some(opencrab_db::queries::HEARTBEAT_CHANNEL_ECHO_METADATA.to_string()),
        created_at: None,
    };
    if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
        tracing::error!(agent_id = %agent_id, channel_id = %channel_id, "#425: HB 発話を会話セッションへ記録できなかった: {e}");
    }
}

/// ハートビート応答テキストから決定（SPEAK / LEARN / IDLE）を解く。
///
/// **入力は応答テキストだけ**で、プロンプトに何を積んだかには依存しない（#404 で
/// ハートビート文脈へ実会話を入れたが、この関数のシグネチャがその独立性を担保する）。
/// 判定: `SPEAK:` を含む最初の行の右側を取り、空なら Idle。`SPEAK:` が無く
/// **先頭語が LEARN の決定行**があれば Learn、それ以外は Idle。
///
/// #515: IDLE に短い理由（`IDLE: <理由>`）を残させるようにしたため、決定語の判定を
/// **決定行の先頭語**に寄せた。理由が自由文になり、決定行の中に他の決定語が現れる余地が
/// 生まれたため:
/// - **LEARN**: 「応答全体に LEARN の語を含むか（大小無視）」から先頭語判定へ絞った。
///   `IDLE: 直前に LEARN した` で内省メモ書き込み（`apply_decision` の Learn 分岐）が
///   誤発火するのを防ぐ。
/// - **SPEAK**: `SPEAK:` を含む最初の行を拾う緩さ（**思考行を前置しても拾う**という既存の
///   意図）は保つが、**IDLE / LEARN の決定行の内側にある `SPEAK:` は拾わない**。
///   `IDLE: 今は SPEAK: するほどの話題がない` の右側が発話として**外部チャンネルへ配送**
///   されるのを防ぐ（LEARN の誤発火＝内部メモより結果が重い＝取り消せない外部投稿）。
fn parse_heartbeat_decision(response: &str) -> HeartbeatDecision {
    let response_text = response.trim();
    // SPEAK: <メッセージ> — SPEAK: を含む最初の行の右側を発話にする（思考行が前置されても
    // 拾う）。ただし **IDLE / LEARN の決定行**（先頭語が IDLE/LEARN）の内側にある SPEAK: は
    // その決定の理由の一部なので拾わない。空なら発話しない。
    if let Some(line) = response_text
        .lines()
        .find(|l| l.contains("SPEAK:") && !leads_with_idle_or_learn(l))
    {
        let content = line
            .split_once("SPEAK:")
            .map(|x| x.1)
            .unwrap_or("")
            .trim()
            .to_string();
        return if content.is_empty() {
            HeartbeatDecision::Idle
        } else {
            HeartbeatDecision::Speak(content)
        };
    }
    // 先頭語が LEARN の決定行があるときだけ Learn。`IDLE: <理由>` は理由の有無・中身に
    // 関わらずここには落ちず（先頭語は IDLE）、既定の Idle になる。
    if response_text.lines().any(is_learn_decision_line) {
        return HeartbeatDecision::Learn;
    }
    HeartbeatDecision::Idle
}

/// 行の**先頭語**（`:` か空白までの最初のトークン）。
fn leading_keyword(line: &str) -> &str {
    line.trim()
        .split(|c: char| c == ':' || c.is_whitespace())
        .next()
        .unwrap_or("")
}

/// 決定行としての LEARN 判定。先頭語が `LEARN`（大小無視）のときだけ真。
///
/// `IDLE: 直前に LEARN した` のように理由へ LEARN の語が混じる行は、先頭語が `IDLE` なので
/// 除外される。
fn is_learn_decision_line(line: &str) -> bool {
    leading_keyword(line).eq_ignore_ascii_case("LEARN")
}

/// IDLE / LEARN の決定行か（先頭語が IDLE か LEARN）。SPEAK 判定でこれらの行を除外し、
/// 理由文に紛れた `SPEAK:` を発話へ昇格させないために使う。
fn leads_with_idle_or_learn(line: &str) -> bool {
    let kw = leading_keyword(line);
    kw.eq_ignore_ascii_case("IDLE") || kw.eq_ignore_ascii_case("LEARN")
}

/// Nostr 宛ハートビートターンの**表示用 channel_name**（プロンプト内の会話呼称）。
/// Nostr broadcast は特定チャンネルを持たないため、会話名の代わりにこのラベルを充てる
/// （`scheduler.rs` の `run_one_fire`）。
///
/// **スコープではなく表示ラベル**である点に注意。旧名は「agent スコープ」の語を含んでおり、
/// agent スコープ発火（#456 で全廃済み・現在は session 単位の `nostr-` セッションから発火）が
/// まだ残っているかのように読み手を誤らせたため改名した（#472）。
/// 場所の呼称は transport 中立にする（#158 S2 と同方針）。
const HEARTBEAT_NOSTR_CHANNEL_LABEL: &str = "（自律ハートビート）";

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

    // ハートビートの初期設定と live G の watch チャネル。
    //
    // **AppState 構築より前に作る**のは、`get_my_heartbeat`（PR3）が `discord-` セッションの
    // ゲート理由（G=false）を本人へ見せるために live G を `AppState::heartbeat_config_rx` から
    // 読むため（scheduler が発火時に読むのと同一源・hot-reload 追従）。tx は config watcher へ、
    // rx は AppState と scheduler へ配る（受信端は clone 可能）。
    let (heartbeat_config_tx, heartbeat_config_rx) = watch::channel(HeartbeatConfig {
        interval_secs: cfg.agent.heartbeat_interval_secs,
        enabled: cfg.agent.heartbeat_enabled,
    });

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
        category_maintenance: cfg.category_maintenance.clone(),
        memory_organize: cfg.memory_organize.clone(),
        memory_declare: cfg.memory_declare.clone(),
        memory_condense: cfg.memory_condense.clone(),
        loop_restart_enabled: cfg.agent.loop_restart_enabled,
        index_build_inflight: Arc::new(dashmap::DashMap::new()),
        intake: Arc::new(cfg.intake.clone()),
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
        // 中央スケジューラの起床通知（#437 / #439）。発火ターン完了・global config 変更に
        // 加え、set_my_heartbeat（PR3）からも鳴らして即時反映させる。
        scheduler_wake: Arc::new(tokio::sync::Notify::new()),
        // 受信箱消化ループの起床通知（#499）。webhook が新規イベントを積んだ直後に鳴らし、
        // ポーリング間隔を待たずに即消化させる（ポーリングは安全網として残す）。
        intake_wake: Arc::new(tokio::sync::Notify::new()),
        // live G を読む口（#394 / 設計 §13.1）。scheduler と同一の watch 源。
        heartbeat_config_rx: heartbeat_config_rx.clone(),
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

    // 共有（TOML）ゲートウェイのハンドル。登録簿（`state.gateways`）には載らないので
    // ここで直接持つ（理由は `heartbeat_delivery` モジュール doc）。
    let shared_discord_http: DiscordHttpArc = Arc::new(Mutex::new(None));
    // ハートビート発話の Discord ハンドル解決口（#400）。**発話する体ごと**に
    // per-agent ゲートウェイ → 共有ゲートウェイの順で配送時に引く。
    let heartbeat_discord_http = Arc::new(heartbeat_delivery::HeartbeatDiscordHttp::new(
        shared_discord_http.clone(),
    ));

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

                *shared_discord_http.lock().unwrap() = Some(gateway.http().clone());

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
        // 2. 下の起動時診断（どの体で Discord ハンドルが解決できるか）が、この復元の
        //    **完了**を前提にしている。#400 以降、実際の解決は配送のたびに行うので
        //    「復元が後ろへずれると発話が共有ゲートウェイの HTTP のまま固定される」
        //    という取り返しのつかない依存は無くなったが、診断の意味は復元後にしかない。
        state.gateways.restore_pending().await;

        // heartbeat の Discord ハンドルを **per-agent ゲートウェイから体ごとに**引ける
        // ようにする（#400）。
        //
        // 以前はここで `agent_ids.first()` の 1 体だけを解決して 1 本のハンドルを全体で
        // 共有していた。`Http` はボットのトークンを保持する＝送信者名を決めるので、
        // (1) Discord へ発話できるのは先頭の体だけ、(2) 先頭以外の体が Discord チャンネル
        // へ向いていればその発話は先頭の体の名前で出る、という並び順依存になっていた。
        // 引き口だけ渡し、**どの体のハンドルを使うかは配送時に発話者で決める**。
        //
        // **ここは登録簿の走査に畳んでいない**（#191 段階2 PR5）。必要なのは
        // `Arc<serenity::http::Http>` そのもので、heartbeat の発話経路（`SPEAK:`）が
        // serenity の API を直に叩く Discord 専用コードだから。PR4 の capability
        // （`gateway_actions_for`）が返すのは `GatewayActions` であって生の HTTP
        // クライアントではなく、ここに当てると発話経路ごと書き換えになる（挙動不変で
        // なくなる）。transport 中立化は heartbeat 側の課題として残す。
        heartbeat_discord_http.set_per_agent_source(manager.clone());

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

    // 古い llm_logs の zip アーカイブ（#337）。メンテナンスループが per-agent かつ
    // 高頻度（既定 600 秒）なのに対し、こちらは全 llm_logs を対象にした日次の重い I/O
    // なので別ループにする。出力先は未指定なら DB ファイルの親 + `archive`（DB と同じ
    // ボリューム = 内蔵ディスクに置かない方針）へ導出する。
    if cfg.llm_log_archive.enabled {
        let archive_dir = if cfg.llm_log_archive.dir.trim().is_empty() {
            std::path::Path::new(&cfg.database.path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("archive")
        } else {
            std::path::PathBuf::from(&cfg.llm_log_archive.dir)
        };
        opencrab_server::llm_log_archive::spawn_llm_log_archive_loop(
            state.db.clone(),
            archive_dir,
            cfg.llm_log_archive.retention_days,
            cfg.llm_log_archive.interval_secs,
        );
    }

    // 外部イベント受信（webhook intake / #454）。
    //
    // 消化ループは heartbeat の起動条件（グローバル有効 or opt-in）に**依存させない**。
    // heartbeat ループは有効なエージェントが居ないと張られず、そこに inbox 消化を相乗り
    // させると webhook 対象エージェントの heartbeat が無効なとき黙って消化されない
    // （silent no-op）。専用ループにして常時起動し、未処理が空なら LLM を呼ばない
    // （per-agent の非空ゲート / 受け入れ基準）。消化ターンは heartbeat の SPEAK 配送を
    // 通さない（webhook 起点の外部 broadcast を避ける）— 詳細は intake_process モジュール doc。
    intake_process::spawn_intake_process_loop(state.clone());
    // catch-up ポーリング（起動時 + 定期）。source アダプタ未設定なら中で即 return する。
    opencrab_server::intake::spawn_intake_catchup_loop(state.clone());

    // ハートビートの初期設定と live G の watch チャネルは AppState 構築前に作成済み
    // （`heartbeat_config_tx` / `heartbeat_config_rx`）。tx は下の config watcher へ、
    // rx は scheduler へ渡す（AppState には clone 済み）。

    // 起動時の Discord ハンドル解決診断（下の #400 ブロック）専用のエージェント列挙。
    // 中央スケジューラは `session_heartbeat_config` を直接読むため発火にはこの列挙を使わない。
    // 診断は Discord feature 有効時のみ出すので、列挙も同じ cfg に閉じる（無効ビルドで未使用に
    // ならないように）。
    #[cfg(feature = "discord")]
    let agent_ids: Vec<String> = {
        let base: Vec<String> = cfg.gateway.discord.agent_ids.clone();
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

    // #400: ハートビートを回す体ごとに、Discord ハンドルが解決できるかを起動時に 1 行残す。
    // 以前は先頭 1 体の解決に失敗しても `if let Some` が外れるだけで**何も出ず**、
    // 「Discord へは一切出ない構成のまま動いている」ことに気づけなかった（配送時の WARN は
    // 発火してから出るもので、起動時のハンドル未解決そのものは可視化されていなかった）。
    // ハンドルの実際の解決は配送時に行うので、ここはあくまで起動時点のスナップショット。
    //
    // Discord feature 無効ビルドでは per-agent も共有もハンドルが存在しようがなく、
    // 「解決できない」WARN が毎起動エージェントの数だけ出るだけの雑音になるので診断ごと落とす。
    #[cfg(feature = "discord")]
    {
        let resolved: Vec<String> = match state.db.lock() {
            Ok(conn) => agent_ids
                .iter()
                .map(|id| resolve_agent_id(&conn, id))
                .collect(),
            // DB を引けなければ config の表記のままで診断する（診断のために起動を止めない）。
            Err(_) => agent_ids.clone(),
        };
        for agent_id in &resolved {
            heartbeat_delivery::log_startup_http_resolution(&heartbeat_discord_http, agent_id);
        }
    }

    // 中央ハートビートスケジューラ（#439 / #437 / #438 / 設計 §3）へ切替。
    //
    // 旧実装はエージェントごとに `core::heartbeat::heartbeat_loop` を立て、固定グリッド
    // sleep + メモリ位相（`Instant`）で回していた（再起動で位相消失=#439-1・設定変更が
    // 張り直しまで効かない=#437・sleep グリッドと設定間隔の乖離=#438）。ここでは**単一
    // タスク**が `session_heartbeat_config` を毎ウェイクで読み直し、永続アンカーから正確な
    // 次回発火まで眠り、`scheduler_wake` で即時反映する。
    //
    // **単一の HeartbeatTurnRunner を共有する**のが要点。runner は `SessionLocks` を 1 つ
    // 持ち（`heartbeat_turn.rs` / `session_runtime.rs`）、複数作ると同一 session id でも
    // 直列化されない。中央化で全セッションが 1 つの runner を通るので、tick と継続ターンの
    // 二重応答防止が全域で効く。
    //
    // live G（global kill-switch = `cfg.agent.heartbeat_enabled`）は scheduler が
    // **発火時に** `heartbeat_config_rx` から読む（hot-reload 追従・起動時スナップにしない。
    // さもないと後から G=false にしても止まらない退行が出る・設計 §4.2）。config 変更・
    // set_my_heartbeat（PR3）・schedule CRUD（PR4）・発火ターン完了は `scheduler_wake` で
    // rebuild を促す。
    {
        let scheduler_runner =
            heartbeat_turn::HeartbeatTurnRunner::from_state(&state, heartbeat_discord_http.clone());
        let scheduler_state = state.clone();
        tokio::spawn(async move {
            scheduler::run_scheduler(scheduler_state, scheduler_runner, heartbeat_config_rx).await;
        });
    }

    let _watcher_handle = opencrab_server::hot_reload::start_config_watcher(
        "config",
        state.db.clone(),
        // #412: 「default_model が変わったか」の基準。稼働中の実効 spec そのものを渡す
        // （上の `format!("{provider}:{model}")` と同じ形でないと永久に不一致になる）。
        state.default_model.clone(),
        state.tools_config.clone(),
        heartbeat_config_tx,
    );

    // Per-agent Nostr sub-gateway マネージャ（discord と同様に、state clone より前に
    // 生成して配線する。nostr は重い依存が無いので feature ゲート無しの常時配線）。
    {
        // nostaro は**エージェントの workspace ルートを cwd にして**起動する（#299）。
        // `execute_shell` / `ws_*` と同じ `agent.workspace_path` を渡して基準を揃える
        // （`nostr_run event --file <相対>` / `--out <相対>` がそれらと噛み合う）。
        let cli =
            opencrab_nostr::NostaroCli::new().with_workspace_base(state.workspace_base.clone());
        let manager: opencrab_server::SharedNostrManager =
            Arc::new(opencrab_nostr::NostrGatewayManager::new(state.clone()).with_cli(cli));
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

    // #169 の dispatch 配線（`RunRequest` に registry と sink が載る / registry は agent
    // 単位で共有）と、#440 の継続ターン配線のテストは `heartbeat_turn` へ移した。
    // `RunRequest` を組むのがそちらになったため。

    /// #404: 文脈に実会話を入れても `SPEAK:` / `LEARN` / `IDLE` の解釈は変わらない。
    /// パースの入力は応答テキストだけ（プロンプトに何を積んだかに依存しない）。
    #[test]
    fn heartbeat_decision_parse_depends_only_on_response_text() {
        assert!(matches!(
            parse_heartbeat_decision("SPEAK: おはよう"),
            HeartbeatDecision::Speak(c) if c == "おはよう"
        ));
        // 前置きの行があっても SPEAK: を含む行から取る。
        assert!(matches!(
            parse_heartbeat_decision("考えた結果\nSPEAK: 今日は静かだ\n"),
            HeartbeatDecision::Speak(c) if c == "今日は静かだ"
        ));
        // 中身が空なら発話しない。
        assert!(matches!(
            parse_heartbeat_decision("SPEAK:   "),
            HeartbeatDecision::Idle
        ));
        assert!(matches!(
            parse_heartbeat_decision("learn"),
            HeartbeatDecision::Learn
        ));
        assert!(matches!(
            parse_heartbeat_decision("IDLE"),
            HeartbeatDecision::Idle
        ));
        // 実会話が文脈に入っても、応答が SPEAK: を含まなければ発話にはならない
        // （実会話側の引用が誤って発話へ昇格しないこと）。
        assert!(matches!(
            parse_heartbeat_decision("チャンネルでは雑談が続いている。今は黙っておく。"),
            HeartbeatDecision::Idle
        ));
    }

    /// #515: IDLE に理由を後置しても壊れない。
    ///
    /// - `IDLE: <理由>` は Idle のまま（理由が消えて記録が空にならない = 記録は `speech` へ
    ///   別途残るが、決定はあくまで Idle）。
    /// - **理由に LEARN の語が混じっても** Learn に化けない（旧 `contains("LEARN")` の誤判定を
    ///   直した回帰テスト。ここを緩い実装へ戻すと Learn になり赤くなる = 内省メモ誤発火の検知）。
    /// - 規約を守らない素の理由文（キーワード無し）でも Idle に落ちて壊れない。
    #[test]
    fn heartbeat_idle_reason_does_not_misclassify() {
        assert!(matches!(
            parse_heartbeat_decision("IDLE: TL に新しい話題が無い"),
            HeartbeatDecision::Idle
        ));
        // 理由に LEARN の語が入っても Idle（決定語は先頭の IDLE）。
        assert!(matches!(
            parse_heartbeat_decision("IDLE: 直前に LEARN した話題なので今は黙る"),
            HeartbeatDecision::Idle
        ));
        // 先頭語が LEARN の決定行なら従来どおり Learn（理由付きでも）。
        assert!(matches!(
            parse_heartbeat_decision("LEARN: 巡回の気づきをメモした"),
            HeartbeatDecision::Learn
        ));
        // 素の LEARN（理由なし）も従来どおり Learn。
        assert!(matches!(
            parse_heartbeat_decision("LEARN"),
            HeartbeatDecision::Learn
        ));
        // 規約を無視した素の理由文（キーワード無し）は Idle に落ちる。
        assert!(matches!(
            parse_heartbeat_decision("今は特に動く必要が無いと判断した"),
            HeartbeatDecision::Idle
        ));
    }

    /// #515（SPEAK 側の非対称の是正）: **IDLE の理由文に紛れた `SPEAK:` を発話へ昇格させない**。
    ///
    /// 理由を自由文にしたことで `IDLE: 今は SPEAK: …` のような理由が書ける余地が生まれた。旧
    /// 実装（全行 `contains("SPEAK:")`）だと右側が `Speak(...)` になり**外部チャンネルへ配送**
    /// される（取り消せない）。ここを緩い実装へ戻すとこのテストが赤くなる（＝外部誤投稿の検知）。
    ///
    /// 同時に、**思考行を前置してから `SPEAK:` を書く**既存の意図は保つ（doc 明記）。
    #[test]
    fn speak_inside_an_idle_reason_is_not_promoted_to_speech() {
        // 本題: IDLE の決定行の内側の SPEAK: は発話にしない。
        assert!(matches!(
            parse_heartbeat_decision("IDLE: 今は SPEAK: するほどの話題がない"),
            HeartbeatDecision::Idle
        ));
        // LEARN の決定行の内側の SPEAK: も同様（先頭語が LEARN なので発話にしない → Learn）。
        assert!(matches!(
            parse_heartbeat_decision("LEARN: SPEAK: しようか迷ったが学びに回す"),
            HeartbeatDecision::Learn
        ));
        // 既存の意図は不変: 思考行を前置してから SPEAK: を書くと発話として拾う。
        assert!(matches!(
            parse_heartbeat_decision("少し迷った\nSPEAK: 新機能を告知した"),
            HeartbeatDecision::Speak(c) if c == "新機能を告知した"
        ));
        // 先頭が SPEAK の決定行はそのまま発話（IDLE/LEARN 除外の巻き添えにしない）。
        assert!(matches!(
            parse_heartbeat_decision("SPEAK: おはよう"),
            HeartbeatDecision::Speak(c) if c == "おはよう"
        ));
    }

    /// #425: HB 発話の会話セッションへの二重記録は **Discord へ配信できたターンだけ**行う。
    /// (a) 配信成功 → 本人の `discord-…` 会話セッションへ `speech` が印つきで載る（修正前は
    /// 1 行も無く、本人が自分の投稿を思い出せない再現ケース）。(b) 配信失敗 → 載せない
    /// （言っていないことを記憶に残さない）。guild 無しのエージェント単位 tick → 記録先が
    /// 無いので何もしない。
    #[test]
    fn heartbeat_channel_echo_recorded_only_on_delivery() {
        let db = opencrab_db::Db::memory().unwrap();

        // (a) 配信成功
        record_heartbeat_channel_echo(&db, true, "agent-a", "111", "222", "自律発話A");
        // (b) 配信失敗 → 記録しない
        record_heartbeat_channel_echo(&db, false, "agent-a", "111", "222", "配信失敗の発話");
        // guild 無し（エージェント単位 tick）→ 記録先が無いので何もしない
        record_heartbeat_channel_echo(&db, true, "agent-a", "", "", "guild無しの発話");

        let conn = db.lock().unwrap();
        let logs =
            opencrab_db::queries::list_session_logs_by_session(&conn, "discord-agent-a-111-222")
                .unwrap();

        assert_eq!(
            logs.len(),
            1,
            "配信成功ターンだけが会話セッションへ載る（失敗・guild無しは載らない）: {logs:?}"
        );
        let row = &logs[0];
        assert_eq!(row.log_type, "speech");
        assert_eq!(row.content, "自律発話A");
        assert_eq!(row.speaker_id.as_deref(), Some("agent-a"));
        assert_eq!(
            row.metadata_json.as_deref(),
            Some(opencrab_db::queries::HEARTBEAT_CHANNEL_ECHO_METADATA),
            "HB 二重記録の印が付く（HB 経路の実会話セクションで二重表示を除外するため）"
        );
        // 印は is_heartbeat_channel_echo で表示専用（FTS・索引・宣言材料から除外）と判定される。
        assert!(
            opencrab_db::queries::is_heartbeat_channel_echo(row.metadata_json.as_deref()),
            "記録した印は is_heartbeat_channel_echo で表示専用と判定される"
        );
    }
}
