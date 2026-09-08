use std::sync::{Arc, Mutex};

use opencrab_core::heartbeat::HeartbeatConfig;
use opencrab_server::{config, config::AppConfig, AppState};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

/// Owned results of the pre-spawn startup phase.
pub(super) struct BootstrapContext {
    pub(super) cfg: AppConfig,
    pub(super) extgate: Arc<opencrab_extgate::ExtgateState>,
    pub(super) gate_socket: Option<std::path::PathBuf>,
    #[cfg(feature = "discord")]
    pub(super) gate_socket_for_discord: Option<String>,
    #[cfg(feature = "nostr")]
    pub(super) gate_socket_for_nostr: Option<String>,
    #[cfg(feature = "discord")]
    pub(super) attachment_inbox_root: std::path::PathBuf,
    #[cfg(feature = "nostr")]
    pub(super) nostr_master_key: Option<opencrab_nostr::MasterKey>,
    #[cfg(feature = "nostr")]
    pub(super) start_nostr: bool,
    pub(super) heartbeat_config_tx: watch::Sender<HeartbeatConfig>,
    pub(super) heartbeat_config_rx: watch::Receiver<HeartbeatConfig>,
    pub(super) state: AppState,
}

/// Loads and validates startup configuration, scrubs secrets, recovers the DB,
/// validates/migrates Nostr secrets, and constructs the initial application state.
/// No task is spawned before this function returns.
pub(super) fn initialize() -> anyhow::Result<BootstrapContext> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("opencrab=info".parse()?))
        .init();

    tracing::info!("Starting OpenCrab server...");

    // Load config from TOML (with env var expansion)
    let cfg = config::load_config("config/default.toml")?;

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
    {
        let mut conn = db
            .lock()
            .map_err(|_| anyhow::anyhow!("db lock for extgate recover"))?;
        let now = opencrab_extgate::now_nanos();
        opencrab_extgate::recover_stale_deliveries(&mut conn, now)
            .map_err(|e| anyhow::anyhow!("extgate recover failed: {}", e.code.as_str()))?;
        // DI 拡張 §7.5: 残 sending の operation call も stale indeterminate にする（listener 前）。
        opencrab_extgate::recover_stale_calls(&mut conn, now).map_err(|e| {
            anyhow::anyhow!("extgate operation-call recover failed: {}", e.code.as_str())
        })?;
    }
    let gate_token = opencrab_extgate::OperatorToken::take_from_env();
    let gate_socket = opencrab_extgate::validate_listen_socket(&cfg.gate.listen_socket)?;
    #[cfg(feature = "discord")]
    let discord_enabled = match db.lock() {
        Ok(conn) => opencrab_db::queries::list_enabled_agent_discord_configs(&conn)
            .map(|rows| !rows.is_empty())
            .unwrap_or(false),
        Err(_) => false,
    };
    // Discord is V3-only. Environments with an enabled agent must opt in explicitly.
    #[cfg(feature = "discord")]
    if discord_enabled {
        opencrab_server::discord_provision::DiscordIngress::parse(&cfg.gate.discord_ingress)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "enabled Discord agent requires gate.discord_ingress = v3; got {:?}",
                    cfg.gate.discord_ingress
                )
            })?;
        if gate_socket.is_none() {
            anyhow::bail!("enabled Discord agent requires an absolute gate.listen_socket");
        }
    }
    // Discord V3 点火の placement.core_socket 用に、validate 済み path を文字列で控える
    // （`gate_socket` は下の UDS listener ブロックで move されるため、ここで clone）。
    #[cfg(feature = "discord")]
    let gate_socket_for_discord: Option<String> = gate_socket
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    #[cfg(feature = "nostr")]
    let gate_socket_for_nostr: Option<String> = gate_socket
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    let extgate = Arc::new(opencrab_extgate::ExtgateState::new(db.clone(), gate_token));
    #[cfg(feature = "discord")]
    let attachment_inbox_root = {
        let root = std::path::Path::new(&cfg.database.path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("attachments")
            .join("inbox");
        std::fs::create_dir_all(&root)?;
        root.canonicalize()?
    };
    #[cfg(feature = "discord")]
    extgate.set_attachment_inbox_root(attachment_inbox_root.clone());

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
    let nostr_enabled = match db.lock() {
        Ok(conn) => conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_nostr_config WHERE enabled = 1)",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false),
        Err(_) => false,
    };
    #[cfg(feature = "nostr")]
    let _nostr_ingress = if nostr_enabled {
        match opencrab_nostr::NostrIngress::parse(&cfg.gate.nostr_ingress) {
            Some(opencrab_nostr::NostrIngress::V3) => opencrab_nostr::NostrIngress::V3,
            _ => anyhow::bail!(
                "Nostr 設定済み環境では gate.nostr_ingress = \"v3\" が必須です（legacy fallback は廃止）"
            ),
        }
    } else {
        opencrab_nostr::NostrIngress::V3
    };
    #[cfg(feature = "nostr")]
    if nostr_enabled && gate_socket.is_none() {
        anyhow::bail!("Nostr 設定済み環境では絶対パスの gate.listen_socket が必須です");
    }
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
    if nostr_enabled && nostr_master_key.is_none() {
        anyhow::bail!(
            "enabled Nostr agent がありますが有効な OPENCRAB_SECRET_MASTER_KEY がありません"
        );
    }
    #[cfg(feature = "nostr")]
    let start_nostr = nostr_enabled;

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
        tools_config: Arc::new(std::sync::RwLock::new(tools_cfg)),
        default_model,
        compaction_ratio: cfg.llm.compaction_ratio,
        typed_history_enabled: cfg.conversation.typed_history,
        typed_history_drop_directive: cfg.conversation.drop_response_directive,
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

    Ok(BootstrapContext {
        cfg,
        extgate,
        gate_socket,
        #[cfg(feature = "discord")]
        gate_socket_for_discord,
        #[cfg(feature = "nostr")]
        gate_socket_for_nostr,
        #[cfg(feature = "discord")]
        attachment_inbox_root,
        #[cfg(feature = "nostr")]
        nostr_master_key,
        #[cfg(feature = "nostr")]
        start_nostr,
        heartbeat_config_tx,
        heartbeat_config_rx,
        state,
    })
}

/// #620: Nostr を起動できない理由を起動ログに埋もれない形で知らせる。
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
