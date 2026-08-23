//! opencrab-social-runtime — 走る core のプロセス。実ソケットでプラグインの接続を受ける。
//!
//! 使い方:
//!   opencrab-social-runtime <socket_path> <db_path> [room_address]
//!
//! - socket_path: プラグインが繋いでくる Unix ソケット。
//! - db_path:     SQLite の権威（再起動で場もログも生き残る）。`:memory:` も可。
//! - room_address: web の場の住所（既定 `room:main`）。web ゲートの address_form に合わせる。
//!
//! **どんな場を、どのゲートに、どの発火方針で起こすかは設定**（app の判断1・詳細§01）。既定は 1 つの
//! web の場だが、環境変数 `OPENCRAB_PLACES`（JSON ファイルのパス）を与えると、そこに書いた場を**すべて**
//! 起こす——web でも nostr でも同じ 1 本（`Host::provision_place`）が起こす。ゲート名はバイナリに
//! 直書きしない（配線漏れの是正・タスク#1）。JSON の形は `opencrab_app::parse_places_config` を参照。

use opencrab_app::{bind_unix, ensure_migrated, parse_places_config, Host};
use opencrab_social_runtime::FakeClock;
use opencrab_store::Store;
use rusqlite::Connection;
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

const TEST_CLOCK_SOCKET_ENV: &str = "OPENCRAB_TEST_CLOCK_SOCKET";

async fn serve_test_clock(listener: UnixListener, clock: FakeClock) -> std::io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Some(line) = lines.next_line().await? {
            let millis = line
                .strip_prefix("advance_ms=")
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidData,
                        "test clock command must be advance_ms=<u64>",
                    )
                })?
                .parse::<u64>()
                .map_err(|error| {
                    Error::new(
                        ErrorKind::InvalidData,
                        format!("invalid test clock milliseconds: {error}"),
                    )
                })?;
            clock.advance(Duration::from_millis(millis));
            write
                .write_all(format!("advanced_ms={millis}\n").as_bytes())
                .await?;
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let socket_path = args
        .next()
        .expect("usage: opencrab-social-runtime <socket> <db> [room]");
    let db_path = args
        .next()
        .expect("usage: opencrab-social-runtime <socket> <db> [room]");
    let room = args.next().unwrap_or_else(|| "room:main".to_string());

    let store = if db_path == ":memory:" {
        Store::new_in_memory().expect("open store")
    } else {
        let mut conn = Connection::open(&db_path).expect("open db for migration");
        opencrab_db::schema::initialize(&conn).expect("initialize legacy schema");
        let inputs = migration_inputs().expect("migration inputs");
        ensure_migrated(
            &mut conn,
            &inputs.config,
            &inputs.environment,
            inputs.captured_at,
        )
        .unwrap_or_else(|error| panic!("ensure_migrated: {error}"));
        drop(conn);
        drop(inputs);
        Store::open(&db_path).expect("open store")
    };

    // Process E2E だけが明示する時計制御口。時刻源のみを差し替え、batch/debounce の判断は production と
    // 同じ core 実装を通る。未知コマンドや不正値は test server を失敗させ、通常時計へは倒さない。
    let test_clock = match std::env::var(TEST_CLOCK_SOCKET_ENV) {
        Ok(path) if !path.trim().is_empty() => {
            let clock = FakeClock::new();
            let listener = bind_unix(Path::new(&path))?;
            Some((clock, listener))
        }
        Ok(_) => {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("{TEST_CLOCK_SOCKET_ENV} must not be empty"),
            ))
        }
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("{TEST_CLOCK_SOCKET_ENV} must be Unicode"),
            ))
        }
    };
    let host = match &test_clock {
        Some((clock, _)) => Host::boot_with_clock(store, Arc::new(clock.clone())),
        None => Host::boot(store),
    };

    // 場を用意する（設定）。プラグインが（再）接続した瞬間に core が結び直す（rebind_gate・プロトコル§08）。
    match std::env::var("OPENCRAB_PLACES") {
        Ok(path) if !path.trim().is_empty() => {
            // 設定ファイルから場を起こす。**壊れ・未知は既定へ倒さず即座に止まる**（近いものへ寄せない・§15）——
            // 自分が用意する設定を読む所なので、読めなければ落ちてよい（Policy::from_json の落とし方の対称）。
            let json = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("OPENCRAB_PLACES を読めない（{path}）: {e}"));
            let specs = parse_places_config(&json)
                .unwrap_or_else(|e| panic!("OPENCRAB_PLACES の設定が不正（{path}）: {e}"));
            if specs.is_empty() {
                panic!("OPENCRAB_PLACES に場が 1 つも無い（{path}）");
            }
            for spec in &specs {
                let (place, _agent) = host.provision_place(spec);
                eprintln!(
                    "opencrab-social-runtime: provisioned place={place} gate={} address={}",
                    spec.gate, spec.address
                );
            }
        }
        // 既定: 1 つの web の場（positional の room・後方互換）。
        _ => {
            let (place, _agent) = host.provision_web_room(&room, "web-agent", "web-agent");
            eprintln!("opencrab-social-runtime: provisioned web place={place} room={room}");
        }
    }

    let listener = bind_unix(Path::new(&socket_path))?;
    eprintln!("opencrab-social-runtime: listening on {socket_path} (db={db_path})");
    match test_clock {
        Some((clock, clock_listener)) => {
            tokio::select! {
                result = host.serve_unix(listener) => result,
                result = serve_test_clock(clock_listener, clock) => result,
            }
        }
        None => host.serve_unix(listener).await,
    }
}

struct MigrationInputs {
    config: PathBuf,
    environment: PathBuf,
    captured_at: i64,
    _keep: Option<tempfile::TempDir>,
}

fn migration_inputs() -> std::io::Result<MigrationInputs> {
    let config = std::env::var("OPENCRAB_MIGRATION_CONFIG");
    let environment = std::env::var("OPENCRAB_MIGRATION_ENVIRONMENT");
    let captured = std::env::var("OPENCRAB_MIGRATION_CAPTURED_AT");
    match (config, environment, captured) {
        (Ok(config), Ok(environment), Ok(captured)) => {
            let captured_at = captured.parse::<i64>().map_err(|error| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("OPENCRAB_MIGRATION_CAPTURED_AT is not i64: {error}"),
                )
            })?;
            Ok(MigrationInputs {
                config: PathBuf::from(config),
                environment: PathBuf::from(environment),
                captured_at,
                _keep: None,
            })
        }
        (
            Err(std::env::VarError::NotPresent),
            Err(std::env::VarError::NotPresent),
            Err(std::env::VarError::NotPresent),
        ) => {
            let dir = tempfile::tempdir()?;
            let config = dir.path().join("empty.toml");
            let environment = dir.path().join("empty.env");
            std::fs::write(&config, "")?;
            std::fs::write(&environment, "")?;
            Ok(MigrationInputs {
                config,
                environment,
                captured_at: 0,
                _keep: Some(dir),
            })
        }
        _ => Err(Error::new(
            ErrorKind::InvalidInput,
            "OPENCRAB_MIGRATION_CONFIG, OPENCRAB_MIGRATION_ENVIRONMENT, and \
             OPENCRAB_MIGRATION_CAPTURED_AT must be set together",
        )),
    }
}
