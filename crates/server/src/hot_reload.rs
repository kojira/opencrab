use std::path::Path;
use std::thread::JoinHandle;

use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};

use crate::config::AppConfig;
use crate::AppState;

/// 再読み込みした設定を適用してよいか検証する（#412）。
///
/// `[llm] default_provider` / `default_model` は `agents.model` が空のエージェント
/// 全員が使う実効モデル。`model_pricing` に `context_window` が無いモデルへ**差し替えると**
/// 文脈予算が黙って既定値へ落ちる。**落ちた設定は適用しない**: ここで Err を返した
/// リロードは丸ごと捨て、プロセスは旧設定のまま走り続ける。
///
/// **検証するのは spec が実際に変わったときだけ。** 方針は「設定する時に弾く」であって、
/// 変えていないものを理由に弾くのはそれより広い（モデルと無関係な編集まで巻き添えで捨てない）。
///
/// `running_default_model` は稼働中の実効 spec（[`AppState::default_model`]）。「変わったか」の
/// 比較は `model_pricing` を引くキーと**同じ正規化**を通す（[`crate::process::normalize_model_spec`]）。
fn validate_reloaded_config(
    db: &opencrab_db::Db,
    running_default_model: &str,
    cfg: &AppConfig,
) -> Result<(), String> {
    let spec = crate::config::default_model_spec(&cfg.llm);
    if crate::process::normalize_model_spec(&spec)
        == crate::process::normalize_model_spec(running_default_model)
    {
        return Ok(());
    }
    let conn = db.lock().map_err(|e| format!("db lock failed: {e}"))?;
    crate::process::ensure_model_context_window_registered(&conn, &spec)?;
    crate::process::ensure_model_max_output_tokens_registered(&conn, &spec)
}

/// Keys a running process cannot change: the HTTP port is bound, the gate socket is listening
/// and the DB is open. A reload that changes one of them still applies everything else and
/// logs which key needs a restart (D-1113).
fn restart_only_changes(running: &AppConfig, next: &AppConfig) -> Vec<&'static str> {
    let mut keys = Vec::new();
    if running.gateway.rest.port != next.gateway.rest.port {
        keys.push("[gateway.rest] port");
    }
    if running.gate.listen_socket != next.gate.listen_socket {
        keys.push("[gate] listen_socket");
    }
    if running.database.path != next.database.path {
        keys.push("[database] path");
    }
    keys
}

/// The one reload path (D-1113): parse the file the same way startup does, validate it (#412),
/// and on success make it the live config of `state`. A rejected file changes nothing. When
/// `[llm]` changed, the LLM router is rebuilt from it (with the dashboard overrides), exactly
/// as a dashboard provider change does.
pub fn apply_config_text(state: &AppState, raw: &str) -> Result<(), String> {
    let next = crate::config::parse_config(raw).map_err(|e| format!("{e:#}"))?;
    let running = state.config();
    validate_reloaded_config(&state.db, &state.default_model(), &next)?;
    let llm_changed = format!("{:?}", running.llm) != format!("{:?}", next.llm);
    let router = if llm_changed {
        Some(
            crate::config::build_effective_llm_router(&state.db, &next.llm)
                .map_err(|e| format!("{e:#}"))?,
        )
    } else {
        None
    };
    for key in restart_only_changes(&running, &next) {
        tracing::warn!(
            key,
            "config reload: this key cannot change while the server runs; the old value stays until a restart"
        );
    }
    state.config.replace(next);
    if let Some(router) = router {
        state.llm_router.swap(router);
        tracing::info!("config reload: LLM router rebuilt from the new [llm]");
    }
    Ok(())
}

/// Start a background file watcher on the config directory.
///
/// When a `.toml` file in the directory changes, [`apply_config_text`] makes it the live
/// config: every setting in the file takes effect without a restart (D-1113).
pub fn start_config_watcher(config_dir: impl AsRef<Path>, state: AppState) -> JoinHandle<()> {
    let config_dir = config_dir.as_ref().to_path_buf();

    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();

        let mut debouncer = match new_debouncer(std::time::Duration::from_millis(300), tx) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("Failed to create config file watcher: {}", e);
                return;
            }
        };

        if let Err(e) = debouncer
            .watcher()
            .watch(&config_dir, notify::RecursiveMode::Recursive)
        {
            tracing::error!("Failed to watch config directory {:?}: {}", config_dir, e);
            return;
        }

        tracing::info!("Config hot-reload watcher started on {:?}", config_dir);

        // Keep the debouncer alive by holding it in scope.
        let _debouncer = debouncer;

        for result in rx {
            let events = match result {
                Ok(events) => events,
                Err(e) => {
                    tracing::error!("Config watcher error: {:?}", e);
                    continue;
                }
            };

            for event in events {
                if event.kind != DebouncedEventKind::Any {
                    continue;
                }

                let path = &event.path;

                // Only process .toml files
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }

                tracing::info!("Config file changed: {:?}", path);

                let raw = match std::fs::read_to_string(path) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("Failed to read config file {:?}: {}", path, e);
                        continue;
                    }
                };

                // #412: 検証に落ちた設定は**一切適用しない**（旧設定のまま動き続ける）。
                match apply_config_text(&state, &raw) {
                    Ok(()) => tracing::info!("Config hot-reloaded from {:?}", path),
                    Err(e) => tracing::error!(
                        "Config reload rejected for {:?}; keeping the previous config: {e}",
                        path
                    ),
                }
            }
        }
    })
}

/// 落ちた設定を適用しないこと、そして**変えていないものは理由にしないこと**（#412）。
#[cfg(test)]
mod reload_validation_tests {
    use super::validate_reloaded_config;

    /// 稼働中の実効 spec。`main.rs` と同じ組み立て方（`provider:model`）。
    const RUNNING: &str = "p1:m1";

    fn empty_db() -> opencrab_db::Db {
        opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap())
    }

    fn db_with(provider: &str, model: &str, window: Option<i32>) -> opencrab_db::Db {
        let db = empty_db();
        {
            let conn = db.lock().unwrap();
            opencrab_db::queries::upsert_model_pricing(
                &conn,
                &opencrab_db::queries::ModelPricingRow {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    input_price_per_1m: 0.0,
                    output_price_per_1m: 0.0,
                    context_window: window,
                    max_output_tokens: Some(4_096),
                    cached_input_price_per_1m: None,
                    cache_write_price_per_1m: None,
                },
            )
            .unwrap();
        }
        db
    }

    fn cfg(provider: &str, model: &str) -> crate::config::AppConfig {
        toml::from_str(&format!(
            "[llm]\ndefault_provider = \"{provider}\"\ndefault_model = \"{model}\"\n"
        ))
        .unwrap()
    }

    /// **本題**: default_model を変えていないリロードは、そのモデルが未登録でも通る。
    ///
    /// 変えていない値を根拠に、モデルと無関係な編集まで捨てるのは「設定する時に弾く」より広い。
    #[test]
    fn unchanged_default_model_passes_even_when_unregistered() {
        let db = empty_db();
        assert!(validate_reloaded_config(&db, RUNNING, &cfg("p1", "m1")).is_ok());
    }

    /// 空白が付いた / 外れただけの config は「変わっていない」。
    /// 比較を生文字列でやると、同じモデルを指しているのに検証が走る。
    #[test]
    fn whitespace_only_difference_is_not_a_change() {
        let db = empty_db();
        assert!(validate_reloaded_config(&db, RUNNING, &cfg(" p1 ", " m1 ")).is_ok());
        assert!(validate_reloaded_config(&db, " p1 : m1 ", &cfg("p1", "m1")).is_ok());
    }

    /// 変えたなら検証する。未登録への差し替えは拒否。
    #[test]
    fn changed_default_model_must_be_registered() {
        let db = db_with("p1", "m1", Some(200_000));
        let err = validate_reloaded_config(&db, RUNNING, &cfg("p1", "m2")).unwrap_err();
        assert!(err.contains("/api/llm/model-pricing"), "{err}");
    }

    /// 変えた先が登録済みなら通る。
    #[test]
    fn changed_default_model_passes_when_registered() {
        let db = db_with("p1", "m2", Some(200_000));
        assert!(validate_reloaded_config(&db, RUNNING, &cfg("p1", "m2")).is_ok());
    }

    /// provider だけ変えても「変わった」扱い（比較は spec 全体）。
    #[test]
    fn changed_provider_alone_is_also_validated() {
        let db = empty_db();
        assert!(validate_reloaded_config(&db, RUNNING, &cfg("p2", "m1")).is_err());
    }

    /// `model_pricing` が空でも、**差し替えないリロードは止まらない**。
    /// 登録前に config を触れなくなるのは方針より広い制約なので、そこは通す。
    #[test]
    fn empty_model_pricing_only_blocks_actual_changes() {
        let db = empty_db();
        assert!(validate_reloaded_config(&db, RUNNING, &cfg("p1", "m1")).is_ok());
        assert!(validate_reloaded_config(&db, RUNNING, &cfg("p1", "m2")).is_err());
    }
}

/// watcher 本体の振る舞い（#412）: 拒否したリロードが live config に
/// **到達しない**こと。
///
/// 検証の単体テストだけでは「Err を返す」までしか押さえられず、呼び出し側が
/// その Err を無視していても気づけない。ここはファイルを実際に書いて watcher を
/// 通す。**先に正常な変更が適用されることを確認**してから異常系を見る（陽性対照が
/// 無いと、単にイベントが届いていないだけで「適用されなかった」と誤判定する）。
#[cfg(test)]
mod watcher_rejection_tests {
    use super::start_config_watcher;
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// 正常な変更が反映されるまでの待ち上限。遅いマシンでも落ちないよう長めに取る。
    const APPLY_WAIT: Duration = Duration::from_secs(20);
    /// 拒否されたはずの変更が現れないことを見る観測窓。陽性対照が直前にこの経路の
    /// 実レイテンシを示しているので、同じ書き直しを数回繰り返せる長さがあれば足りる。
    const REJECT_WINDOW: Duration = Duration::from_secs(3);
    /// 書き直しの間隔。デバウンス（300ms）より長くしないと、書き直しがデバウンスを
    /// 延々とリセットして一度も発火しない。
    const REWRITE_INTERVAL: Duration = Duration::from_millis(500);

    fn config_text(provider: &str, model: &str, tools_enabled: bool) -> String {
        format!(
            "[llm]\ndefault_provider = \"{provider}\"\ndefault_model = \"{model}\"\n\
             [tools]\nenabled = {tools_enabled}\n"
        )
    }

    /// `content` を書き直しながら `cond` の成立を最大 `limit` 待つ。戻り値は成立したか。
    ///
    /// watcher スレッドの起動と最初の書き込みが競合すると、イベントを 1 度取りこぼした
    /// まま二度と発火しない（CI で実際に踏んだ）。同じ内容でも書き直せば再びイベントに
    /// なるので、成立するまで書き直しながら待つ。異常系でも「拒否されるはずの内容」を
    /// 何度も与えることになり、観測の機会が増える方向にしか働かない。
    fn poll_with_rewrites(
        path: &Path,
        content: &str,
        limit: Duration,
        mut cond: impl FnMut() -> bool,
    ) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            std::fs::write(path, content).unwrap();
            let next_write = Instant::now() + REWRITE_INTERVAL;
            while Instant::now() < next_write {
                if cond() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if Instant::now() >= deadline {
                return cond();
            }
        }
    }

    #[test]
    fn rejected_reload_does_not_touch_tools() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.toml");
        std::fs::write(&path, config_text("p1", "m1", false)).unwrap();

        // `model_pricing` は空。よって「変えたら拒否 / 変えなければ通る」の両方が出る。
        let state = crate::AppState::new(
            opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap()),
            opencrab_llm::router::LlmRouter::new(),
            crate::test_config("p1", "m1"),
        );
        let _handle = start_config_watcher(dir.path(), state.clone());
        let tools_enabled = || state.config().tools.enabled;

        // 陽性対照（前）: default_model を変えない編集は適用される（watcher が生きている証拠）。
        assert!(
            poll_with_rewrites(&path, &config_text("p1", "m1", true), APPLY_WAIT, || {
                tools_enabled()
            }),
            "default_model を変えない編集は適用されるはず（watcher が動いていない）"
        );

        // 本題: 未登録モデルへ差し替える編集は tools を動かさない。
        assert!(
            !poll_with_rewrites(
                &path,
                &config_text("p1", "m2", false),
                REJECT_WINDOW,
                || !tools_enabled()
            ),
            "拒否したリロードが live config へ到達した"
        );

        // 陽性対照（後）: 窓の**あと**にも適用が通ること。前の対照だけだと、窓の途中で
        // watcher が死んでいても「届かなかった」を「拒否された」と読んでしまう。
        // ここが通れば、窓のあいだ watcher は生きていたことになる。
        assert!(
            poll_with_rewrites(&path, &config_text("p1", "m1", false), APPLY_WAIT, || {
                !tools_enabled()
            }),
            "陰性の窓のあとも watcher は生きているはず（窓の間に止まっていた）"
        );
    }
}

/// D-1113: every setting in the config file takes effect without a restart, through the one
/// reload path the watcher uses ([`super::apply_config_text`]).
#[cfg(test)]
mod live_config_tests {
    use super::apply_config_text;

    fn state() -> crate::AppState {
        crate::AppState::new(
            opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap()),
            opencrab_llm::router::LlmRouter::new(),
            crate::test_config("p1", "m1"),
        )
    }

    fn register(state: &crate::AppState, provider: &str, model: &str) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_model_pricing(
            &conn,
            &opencrab_db::queries::ModelPricingRow {
                provider: provider.to_string(),
                model: model.to_string(),
                input_price_per_1m: 0.0,
                output_price_per_1m: 0.0,
                context_window: Some(200_000),
                max_output_tokens: Some(4_096),
                cached_input_price_per_1m: None,
                cache_write_price_per_1m: None,
            },
        )
        .unwrap();
    }

    const LLM: &str = "[llm]\ndefault_provider = \"p1\"\ndefault_model = \"m1\"\n";

    /// Turning sleep's declare run off in the file stops the next maintenance tick from running
    /// it, with no restart; turning it back on runs it again.
    #[tokio::test]
    async fn d1113_memory_declare_enabled_follows_the_file_without_restart() {
        let state = state();
        let agent = "agent-live";
        apply_config_text(
            &state,
            &format!("{LLM}[memory_declare]\nenabled = true\nmin_new_logs = 1\n"),
        )
        .unwrap();
        assert!(state.config().memory_declare.enabled);
        apply_config_text(&state, &format!("{LLM}[memory_declare]\nenabled = false\n")).unwrap();
        assert!(!state.config().memory_declare.enabled);
        // The declare run reads the live value: disabled means no run at all.
        assert!(
            !crate::memory_declare::maybe_run_memory_declare(&state, agent)
                .await
                .unwrap()
        );
        apply_config_text(&state, &format!("{LLM}[memory_declare]\nenabled = true\n")).unwrap();
        assert!(state.config().memory_declare.enabled);
    }

    /// Changing the maintenance interval changes the wait before the next tick.
    #[test]
    fn d1113_maintenance_interval_follows_the_file() {
        let state = state();
        apply_config_text(
            &state,
            &format!("{LLM}[agent]\nmemory_maintenance_interval_secs = 90\n"),
        )
        .unwrap();
        assert_eq!(
            crate::memory_maintenance::maintenance_interval(&state.config().agent),
            std::time::Duration::from_secs(90)
        );
        apply_config_text(
            &state,
            &format!("{LLM}[agent]\nmemory_maintenance_interval_secs = 600\n"),
        )
        .unwrap();
        assert_eq!(
            crate::memory_maintenance::maintenance_interval(&state.config().agent),
            std::time::Duration::from_secs(600)
        );
    }

    /// A file that does not parse, or that points the default model at an unregistered model,
    /// changes nothing.
    #[test]
    fn d1113_invalid_file_changes_nothing() {
        let state = state();
        apply_config_text(&state, &format!("{LLM}[memory_declare]\nenabled = true\n")).unwrap();
        assert!(apply_config_text(&state, "[memory_declare\nenabled = false").is_err());
        assert!(apply_config_text(
            &state,
            "[llm]\ndefault_provider = \"p1\"\ndefault_model = \"unregistered\"\n[memory_declare]\nenabled = false\n"
        )
        .is_err());
        assert!(state.config().memory_declare.enabled);
        assert_eq!(state.default_model(), "p1:m1");
    }

    /// A changed `[llm]` default model is the model the next turn resolves to.
    #[test]
    fn d1113_default_model_follows_the_file() {
        let state = state();
        register(&state, "p1", "m2");
        apply_config_text(
            &state,
            "[llm]\ndefault_provider = \"p1\"\ndefault_model = \"m2\"\n",
        )
        .unwrap();
        assert_eq!(state.default_model(), "p1:m2");
        let conn = state.db.lock().unwrap();
        assert_eq!(
            opencrab_db::queries::effective_model_for_agent(
                &conn,
                "no-such-agent",
                &state.default_model()
            )
            .unwrap(),
            "p1:m2"
        );
    }

    /// Keys a running process cannot change are reported, and everything else still applies.
    #[test]
    fn d1113_restart_only_keys_are_named() {
        let running = crate::test_config("p1", "m1");
        let mut next = running.clone();
        next.gateway.rest.port = 9999;
        next.gate.listen_socket = "/tmp/other.sock".into();
        next.database.path = "other.db".into();
        assert_eq!(
            super::restart_only_changes(&running, &next),
            vec![
                "[gateway.rest] port",
                "[gate] listen_socket",
                "[database] path"
            ]
        );
    }
}
