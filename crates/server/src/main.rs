use std::sync::Arc;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

use opencrab_core::heartbeat::HeartbeatConfig;
use opencrab_server::{config, create_router, AppState};
use tokio::sync::watch;

mod intake_process;
mod scheduler;

// #599: ハートビートの発火本体（`run_one_heartbeat`）と表示ラベル
// `HEARTBEAT_NOSTR_CHANNEL_LABEL` は lib（`opencrab_server::heartbeat_fire`）へ移した。
// scheduler（時刻発火）と `run_my_heartbeat`（手動発火）が同じ 1 つの関数を共有するため。

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

    // #620: Nostr の at-rest 暗号化マスターキーを **load_config 直後・全 tokio::spawn より前**に
    // env から読み、**即 remove_var** する。以降 spawn される execute_shell は inherit_env=true で
    // `std::env::vars()` を子へコピーする（crates/actions/src/tools/shell.rs）ので、ここで消せば
    // エージェントのシェルの環境に平文で出ない。config の `${}` 展開（hot-reload 経路が env を
    // 読む）を経由せず、直接 std::env::var で読む。
    //
    // **env スクラブ（読み取り＋ remove_var）は feature 非依存で常に走らせる**（多層防御）。
    // これは「Nostr 専用の処理」ではなく「秘密を env に残さない」ための処理で、`nostr` を外した
    // ビルドでも `OPENCRAB_SECRET_MASTER_KEY` を env から消さないと、その秘密が起動する全シェルへ
    // 平文継承される（PR-1B のレビュー指摘 / 退行防止）。**この remove_var を nostr feature の
    // 内側へ戻さないこと。** 一方、値を `MasterKey` へ parse する部分だけは型が `opencrab_nostr`
    // にあるので `nostr` feature の内側に置く（nostr-off では at-rest 暗号機構ごと不要）。
    #[cfg_attr(not(feature = "nostr"), allow(unused_variables))]
    let master_key_env = std::env::var("OPENCRAB_SECRET_MASTER_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty());
    std::env::remove_var("OPENCRAB_SECRET_MASTER_KEY");
    #[cfg(feature = "nostr")]
    let master_key_parsed: Option<anyhow::Result<opencrab_nostr::MasterKey>> = master_key_env
        .as_deref()
        .map(|b64| opencrab_core::secret_box::parse_master_key(b64).map(std::sync::Arc::new));

    // DB初期化（本番はコネクションプール）
    let db = opencrab_db::Db::open(&cfg.database.path)?;

    // #620: マスターキーの要否は「Nostr が設定されているエージェントが 1 つ以上あるか」で
    // 決める（既存データから判定・新設定は足さない）。**プロセス全体は止めない**（Nostr を
    // 使っていない構成はマスターキー無しでも通常起動する）。マスターキーが在るときだけ Nostr
    // サブシステムを起動し、at-rest 移行を行う。
    #[cfg(feature = "nostr")]
    let nostr_configured = match db.lock() {
        Ok(conn) => opencrab_db::queries::has_any_agent_nostr_config(&conn).unwrap_or(false),
        Err(_) => false,
    };
    #[cfg(feature = "nostr")]
    let mut nostr_master_key: Option<opencrab_nostr::MasterKey> = match master_key_parsed {
        Some(Ok(key)) => Some(key),
        Some(Err(e)) => {
            if nostr_configured {
                emit_master_key_banner(&format!(
                    "OPENCRAB_SECRET_MASTER_KEY が不正です（base64 32 バイトが必要）: {e}"
                ));
            } else {
                tracing::warn!(error = %e, "OPENCRAB_SECRET_MASTER_KEY が不正ですが Nostr 未設定のため無視して起動します");
            }
            None
        }
        None => {
            if nostr_configured {
                emit_master_key_banner(
                    "環境変数 OPENCRAB_SECRET_MASTER_KEY が未設定です（Nostr が設定済みのため必須）",
                );
            }
            None
        }
    };
    // #620: 形式は正しいが**中身が違う**マスターキー（別環境の貼り間違え等）を、既存の暗号文の
    // 試し復号で捕まえる。ここで捕まえないと、移行は `enc:` を skip し provider の復号だけが
    // 後で失敗して post/watch がエラー連発になり、起動時に何も見えない。移行の**前**に判定し、
    // 不一致なら既存のバナー経路で大きく知らせて Nostr を起動しない。
    #[cfg(feature = "nostr")]
    if let Some(key) = nostr_master_key.clone() {
        if let Some(reason) =
            opencrab_server::nostr_secret_migration::master_key_mismatch_reason(&db, &key)
        {
            emit_master_key_banner(&reason);
            nostr_master_key = None;
        }
    }
    // Nostr サブシステムを起動してよいのは、（一致する）マスターキーが在るときだけ（#620）。
    // 無ければ（未設定 / 不正形式 / 既存暗号文と不一致）Nostr は起動しない＝送信も受信も止まる。
    #[cfg(feature = "nostr")]
    let start_nostr = nostr_master_key.is_some();

    // #620: 平文の at-rest 秘密を暗号化する移行（起動時 1 回・冪等・対象が無ければ no-op）。
    #[cfg(feature = "nostr")]
    if let Some(mk) = &nostr_master_key {
        let report = opencrab_server::nostr_secret_migration::migrate_nostr_secrets_at_rest(
            &db,
            mk,
            std::path::Path::new("data/agents"),
        );
        if report.changed_anything() {
            tracing::info!(?report, "#620: Nostr 秘密の at-rest 移行を実施した");
        }
    }

    // #553: 起動時リコンサイル。新プロセスの subtask registry（in-memory）は必ず空なので、
    // この時点で status='active' の subtask セッションは定義上すべて孤児（前プロセスと共に
    // 実行タスクが消滅済み）。「何分止まったら死」の判定を要せず 'interrupted' へ終端化する。
    // 述語 mode='subtask' は他モードに触れない（reconcile_orphaned_subtasks を参照）。
    match db.lock() {
        Ok(conn) => match opencrab_db::queries::reconcile_orphaned_subtasks(&conn) {
            Ok(n) if n > 0 => {
                tracing::info!(
                    reconciled = n,
                    "startup: 孤児化した active subtask を interrupted へ終端化した（#553）"
                )
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("startup subtask reconcile に失敗: {e}"),
        },
        Err(e) => tracing::warn!("startup subtask reconcile: db lock 取得に失敗: {e}"),
    }

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
        // #620: DB 本鍵・生成鍵の at-rest 暗号/復号に使うマスターキー（runner の encrypt-on-write
        // が使う）。**有効（形式が正しく既存暗号文とも一致）なマスターキーがあるときだけ Some**
        // で、Nostr 未設定の構成でも env に有効なキーがあれば Some になる。未設定 / 不正形式 /
        // 既存暗号文と不一致のときは None（暗号化を有効化していない＝従来挙動）。
        #[cfg(feature = "nostr")]
        nostr_master_key: nostr_master_key.clone(),
        // #722: readiness が「discord enabled なのに token 空＝黙って起動しない」を検出する
        // ために、共有ゲートウェイの実効設定を保持する（起動判定に使う値と同じ源）。
        #[cfg(feature = "discord")]
        discord_shared_gateway: Arc::new(cfg.gateway.discord.clone()),
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
        #[cfg(feature = "web")]
        web_gateway: Arc::new(opencrab_web_gateway::WebGateway::new()),
        subtask_registries: Arc::new(opencrab_server::subtask_registries::SubtaskRegistries::new()),
        // #588 Stage 2: プロセス全体で 1 つの per-session 直列化ロック。heartbeat・scheduler・
        // Discord 受信ループ・Nostr ランタイムが同じ実体を共有し、同一セッションのターンを直列化する。
        session_locks: Arc::new(opencrab_actions::SessionLocks::new()),
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
        // #588 TimedFire: 時刻発火の受け口レジストリ。各ゲートウェイのループが起動時に自分の
        // 受け口を登録し、scheduler が発火時に per-agent→共有で引く（空で作り後から register）。
        timed_fire_router: Arc::new(opencrab_actions::TimedFireRouter::new()),
    };

    // #628: transport の発火先 descriptor を**生存非依存で**登録する（ゲートウェイの起動有無・
    // 資格情報の有無に関わらず常時。受理判定・ゲート理由表示・parse はゲートウェイ停止中でも
    // 要る）。sink（生存で register/unregister）とは別の登録で、ここは起動ブロックの**外**に
    // 置く（#627 で「Discord 有効ブロックの中に置く」設計が隔離環境で発火しない罠になった）。
    // 各 descriptor の実装はその transport の crate にあり、登録の源は 1 本化した
    // `register_production_descriptors` だけ（main.rs / test_app_state / scheduler の test_router /
    // 登録簿を反復する generic テストがすべてこれを呼ぶ）。散らすと本番へ足してテスト側への追記を
    // 忘れる隙ができ、prefix 衝突が本番でだけ顕在化しうる（#628 のブロッカー対応）。
    opencrab_server::register_production_descriptors(&state.timed_fire_router);

    // #627 / #628 段階7: web の受け口（sink）も生存非依存で登録する。web には常駐ループが無く、
    // Discord / Nostr のように「ループ起動時に sink を登録」できないので、ここで共有受け口として
    // 1 度だけ登録する（web は外部接続を持たず常に立ち上がる＝隔離環境でもハートビートが届く）。
    // sink は自分で `tokio::spawn` して per-session 直列化込みの入口を回す（`WebTimedFireSink`）。
    // web は会話ゲートなので web feature の内側（PR-1B: web を外すと受け口は登録されない）。
    #[cfg(feature = "web")]
    {
        state.timed_fire_router.register_shared(
            opencrab_web_gateway::WEB_TIMED_FIRE_KIND,
            std::sync::Arc::new(opencrab_web_gateway::WebTimedFireSink::new(state.clone())),
        );
        tracing::info!(
            transport = opencrab_web_gateway::WEB_TIMED_FIRE_KIND,
            "timed-fire: 受け口を登録（web・生存非依存）"
        );
    }

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

    // Start Discord gateway if configured and feature is enabled.
    #[cfg(feature = "discord")]
    {
        // Per-agent Discord gateway manager（#40: 共有ループが「専用ゲートウェイが
        // 稼働中か」を参照できるよう、共有ゲートウェイへ渡す AppState clone より
        // **前に**生成して配線する。実際の復元は共有ゲートウェイ起動後に行う）。
        //
        // #603: 時刻発火の受け口レジストリは `new` の**必須引数**。忘れるとコンパイルエラーに
        // なる（#602 は Option + builder の呼び忘れで Discord の発火が全 skip し本番が止まった）。
        let manager = Arc::new(opencrab_discord::DiscordGatewayManager::new(
            state.clone(),
            state.timed_fire_router.clone(),
        ));
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
                let gateway = Arc::new(opencrab_discord::DiscordGateway::new(&discord_cfg.token));
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
                // #588 TimedFire: この共有（TOML）ループを Discord の共有受け口として登録する。
                // per-agent ゲートウェイを持たないエージェントの時刻発火はここへ落ちる（#400 と同型）。
                state.timed_fire_router.register_shared(
                    opencrab_actions::gateway_kinds::DISCORD,
                    Arc::new(opencrab_discord::message_loop::DiscordTimedFireSink {
                        event_tx: event_tx.clone(),
                    }),
                );
                // #601: 登録が起きたことを起動時に 1 行残す（per-agent を持たない体の時刻発火は
                // ここへ落ちる。これが出ない＝共有受け口が無い、を運用で即検知できるように）。
                tracing::info!(
                    transport = "discord",
                    "timed-fire: 受け口を登録（共有 TOML Discord loop）"
                );
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

    // 退避ファイル（workspace/tmp）の掃除（#711）。退避経路は書くだけで消す実装が無く、
    // ファイルが無限に増える。全エージェントの `workspace/tmp/` を日次で巡回し、mtime が
    // 保持日数より古い**通常ファイルのみ**を個別 remove_file で消す（グロブ・再帰なし）。
    // 発火判定用マーカーは DB ファイルの親（DB と同じボリューム = 内蔵ディスクに置かない）
    // 直下に置き、どのエージェントの tmp とも混ざらないようにする。
    if cfg.offload_cleanup.enabled {
        let marker_dir = std::path::Path::new(&cfg.database.path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        opencrab_server::offload_cleanup::spawn_offload_cleanup_loop(
            state.db.clone(),
            cfg.agent.workspace_path.clone(),
            marker_dir,
            cfg.offload_cleanup.retention_days,
            cfg.offload_cleanup.interval_secs,
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

    // 中央ハートビートスケジューラ（#439 / #437 / #438 / 設計 §3）へ切替。
    //
    // 旧実装はエージェントごとに `core::heartbeat::heartbeat_loop` を立て、固定グリッド
    // sleep + メモリ位相（`Instant`）で回していた（再起動で位相消失=#439-1・設定変更が
    // 張り直しまで効かない=#437・sleep グリッドと設定間隔の乖離=#438）。ここでは**単一
    // タスク**が `session_heartbeat_config` を毎ウェイクで読み直し、永続アンカーから正確な
    // 次回発火まで眠り、`scheduler_wake` で即時反映する。
    //
    // #588 TimedFire: ハートビートは専用のターン実装・専用配送を持たない。時刻が来たら scheduler は
    // 発火先ゲートウェイのループへ `TimedFire` イベントを 1 本流すだけ（`run_one_heartbeat`）で、以降の
    // ターン（配送・ロック・記録・継続）はそのループの**通常ルート**が回す。固有なのは「時間のトリガー＋
    // 渡すプロンプト」と「発火の記録（`heartbeat_log`）」だけ。受け口の解決は `AppState::timed_fire_router`
    // （per-agent→共有・#400 と同型）で行うので、scheduler へ Discord 送信ハンドルを渡す必要はなくなった。
    //
    // per-session 直列化ロック（`SessionLocks`）の唯一のインスタンスは `AppState` が
    // 持ち（#588 Stage 2・`AppState::session_locks`）、scheduler・各ゲートウェイの受信ループ
    // （Discord）・Nostr ランタイムはその `Arc` を clone して**同じ実体**を共有する。これで
    // 時間トリガーと通常メッセージ処理のターンが同一 session id 上で直列化される。
    //
    // live G（global kill-switch = `cfg.agent.heartbeat_enabled`）は scheduler が
    // **発火時に** `heartbeat_config_rx` から読む（hot-reload 追従・起動時スナップにしない。
    // さもないと後から G=false にしても止まらない退行が出る・設計 §4.2）。config 変更・
    // set_my_heartbeat（PR3）・schedule CRUD（PR4）・発火ターン完了は `scheduler_wake` で
    // rebuild を促す。
    {
        let scheduler_state = state.clone();
        tokio::spawn(async move {
            scheduler::run_scheduler(scheduler_state, heartbeat_config_rx).await;
        });
    }

    // #603 / #628 条件 A: 時刻発火の**起動時セルフチェック**を「descriptor 登録簿 ↔ sink 登録簿の
    // 双方向照合」へ集約する。型で配線は強制した（マネージャは router 無しでは構築できない）が、
    // ゲートウェイのループが実際に起動して受け口を登録するのは非同期（特に Nostr は spawn 後）。
    // **手書きの kind 列挙を持たない**: 両登録簿の kind 集合を突き合わせ、(a) sink はあるが
    // descriptor が無い＝発火先を parse できない配線バグ、(b) descriptor が「立ち上がるべき」
    // （`should_be_running` が env を引く）なのに受け口が 0＝時刻発火がどこにも届かない、を ERROR で
    // 知らせる（#602 の黙った全 skip を、コンパイルに加えて運用でも二重に検知する）。新 transport を
    // 足しても、この照合はその descriptor と sink を自動で拾う（手書きリストの更新漏れが起きない）。
    {
        let check_state = state.clone();
        // TOML 共有ゲートウェイが設定されている kind（db に無い設定を畳んで env へ渡す）。
        // per-agent（DB）だけの体は各 descriptor が env.conn を引いて拾う（#602 の本番対象）。
        #[cfg_attr(not(feature = "discord"), allow(unused_mut))]
        let mut configured_shared_kinds: std::collections::HashSet<&'static str> =
            std::collections::HashSet::new();
        #[cfg(feature = "discord")]
        if !cfg.gateway.discord.agent_ids.is_empty() {
            configured_shared_kinds.insert(opencrab_actions::gateway_kinds::DISCORD);
        }
        tokio::spawn(async move {
            // ループの起動→受け口登録は非同期なので猶予を置く（Discord は同期登録だが Nostr は
            // spawn 後に登録するため）。
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let Ok(conn) = check_state.db.lock() else {
                tracing::error!("timed-fire: 起動時セルフチェックで db lock 取得に失敗");
                return;
            };
            let env = opencrab_actions::TransportFireEnv {
                conn: &conn,
                configured_shared_kinds: &configured_shared_kinds,
            };
            let issues = check_state.timed_fire_router.self_check(&env);
            if issues.is_empty() {
                tracing::info!(
                    "timed-fire: 起動時セルフチェック OK（descriptor ↔ sink 双方向照合・prefix 排他・受け口あり）"
                );
            }
            for issue in issues {
                match issue {
                    opencrab_actions::TimedFireSelfCheckIssue::SinkWithoutDescriptor { kind } => {
                        tracing::error!(
                            kind,
                            "timed-fire: sink はあるが descriptor が無い（発火先を parse できない＝配線バグ）"
                        );
                    }
                    opencrab_actions::TimedFireSelfCheckIssue::ExpectedSinkMissing { kind } => {
                        tracing::error!(
                            kind,
                            "timed-fire: 有効な受信ゲートウェイがあるのに受け口が 0（時刻発火が届かない）。配線/起動を確認"
                        );
                    }
                    opencrab_actions::TimedFireSelfCheckIssue::PrefixCollision {
                        owner,
                        shadowed_by,
                    } => {
                        tracing::error!(
                            owner,
                            shadowed_by,
                            "timed-fire: prefix 排他違反（2 つの transport が同じ session_id を parse する）。first-match で発火先が横取りされる。descriptor の parse 書式を分離せよ"
                        );
                    }
                }
            }
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
    // 生成して配線する）。
    //
    // #620: **マスターキーが在るときだけ**登録する。無ければ Nostr は起動しない（送信も受信も
    // 止まる）。Nostr 未設定の構成ではそもそもマスターキー不要なので、ここを飛ばして通常起動する。
    // PR-1B: Nostr は会話ゲートなので nostr feature の内側。外した構成ではこのブロック自体が無い。
    #[cfg(feature = "nostr")]
    if let Some(master_key) = nostr_master_key.clone() {
        // nostaro は**エージェントの workspace ルートを cwd にして**起動する（#299）。
        // `execute_shell` / `ws_*` と同じ `agent.workspace_path` を渡して基準を揃える
        // （`nostr_run event --file <相対>` / `--out <相対>` がそれらと噛み合う）。
        //
        // #620: 本鍵は config へ書かず、`base_command` が spawn ごとに **本鍵プロバイダ**で DB の
        // 暗号文を復号して env 注入する。生成鍵ファイルの復号用に **マスターキー**も注入する。
        let provider = opencrab_nostr::db_main_key_provider(state.db.clone(), master_key.clone());
        let cli = opencrab_nostr::NostaroCli::new()
            .with_workspace_base(state.workspace_base.clone())
            .with_master_key(master_key)
            .with_main_key_provider(provider);
        // #588 TimedFire / #603: 時刻発火の受け口レジストリは `new` の必須引数（Discord と同型・
        // per-agent→共有の解決はルータが行う）。忘れるとコンパイルエラーになる。
        let manager: opencrab_server::SharedNostrManager = Arc::new(
            opencrab_nostr::NostrGatewayManager::new(
                state.clone(),
                state.timed_fire_router.clone(),
            )
            .with_cli(cli),
        );
        // 共通操作も transport 固有の操作（nostaro の鍵生成 = `key_provisioning`）も
        // この登録簿から引く（#191 段階2 PR3・PR4）。名指しフィールドは無い。
        state.gateways.register(manager);
    } else {
        tracing::info!(
            start_nostr,
            "Nostr サブシステムは起動しない（マスターキー未設定 / 不正）。Nostr 未設定の構成なら正常。"
        );
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

/// #620: マスターキー欠落/不正で Nostr を起動できないことを**起動ログに埋もれない形**で知らせる。
/// 区切り線付きの複数行バナーを `error` レベルで出す。**送信も受信も止まる**ことを明記する
/// （プロセスは止めない — Nostr を使っていない機能は動き続ける）。
#[cfg(feature = "nostr")]
fn emit_master_key_banner(reason: &str) {
    let line = "=".repeat(72);
    tracing::error!(
        "\n{line}\n\
         [#620] Nostr を起動できません: {reason}\n\
         at-rest 暗号化のマスターキーが無い/不正なため、この構成では Nostr の秘密鍵を\n\
         復号できません。よって **Nostr の送信も受信も停止** します（Discord など他の機能は\n\
         そのまま動きます）。\n\
         対処: base64 でエンコードした 32 バイトのマスターキーを環境変数\n\
         OPENCRAB_SECRET_MASTER_KEY に設定して再起動してください。\n\
         {line}"
    );
}

// #588 TimedFire / #599: ハートビートの発火本体は `opencrab_server::heartbeat_fire::run_one_heartbeat`
// （時刻が来たら発火先ゲートウェイのループへ `TimedFire` を 1 本流すだけの free 関数）に集約した。lib へ
// 置いてあるので scheduler（時刻発火）と `run_my_heartbeat`（手動発火）が同じ 1 つの関数を共有する。
// 専用のターン実装・専用配送（旧 `heartbeat_delivery.rs`）・scheduler 側の継続ターン機構は撤去し、以降の
// ターンはゲートウェイ既存の通常ルート（Discord=`SubtaskCompleted` / Nostr=`NostrResponder`）が回す。
// 指示文の整形テストは `heartbeat_fire` の `#[cfg(test)]` にある。
