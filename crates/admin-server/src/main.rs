//! admin-server — ダッシュボード API + React SPA 配信。
//!
//! 会話ゲート（web-gate）とは別プロセス・別クレート。会話ゲートに管理 API を混ぜない
//! （DESIGN-gateway-takein §「管理 REST は会話 gateway に載せない」）。
//!
//! store は owner ID 日次書き込みのため RW で開くが、`Store::open` は使わない
//! （稼働中 core の epoch を閉じない）。旧テーブル観測は引き続き読み取り専用。
//!
//! 使い方:
//!   admin-server <db_path> [http_port] [web_dist_dir]
//! 省略時の既定:
//!   db_path      = env OPENCRAB_ADMIN_DB / "data/opencrab.db"
//!   http_port    = env OPENCRAB_ADMIN_PORT / 8787
//!   web_dist_dir = env OPENCRAB_ADMIN_WEB_DIST / "web/dist"（`pnpm build` の出力）

mod agent_routes;
mod api;
mod owner_routes;
mod schedule_cron;
mod voice_routes;

use std::sync::Arc;

use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};

use opencrab_db::Db;
use opencrab_store::Store;

fn arg_or(args: &[String], idx: usize, env_key: &str, default: &str) -> String {
    args.get(idx)
        .cloned()
        .or_else(|| std::env::var(env_key).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| default.to_string())
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let db_path = arg_or(&args, 0, "OPENCRAB_ADMIN_DB", "data/opencrab.db");
    let port: u16 = arg_or(&args, 1, "OPENCRAB_ADMIN_PORT", "8787")
        .parse()
        .expect("http_port は数値である必要があります");
    let web_dist = arg_or(&args, 2, "OPENCRAB_ADMIN_WEB_DIST", "web/dist");
    // 文脈予算 = context_window × compaction_ratio。model-pricing 応答に載せる server-global 値。
    let compaction_ratio: f64 = arg_or(&args, 3, "OPENCRAB_ADMIN_COMPACTION_RATIO", "0.5")
        .parse()
        .expect("compaction_ratio は小数である必要があります");

    // 新テーブル（oc2 store）を RW で開く。schema 初期化も runtime 回収もしない
    // （稼働中 core と同じ DB を Store::open すると epoch を閉じる）。
    let store = Store::open_read_write_no_recover(&db_path).unwrap_or_else(|e| {
        panic!("DB（store）を開けませんでした（{db_path}）: {e}");
    });
    // 旧テーブル（本体 DB スキーマ・正本）。schema 初期化はしない。
    // D25 は `voice_config_override`（B 単独の家）へ書くので RW。他経路は読み取りのまま。
    let db_conn = rusqlite::Connection::open(&db_path)
        .unwrap_or_else(|e| panic!("DB（db）を開けませんでした（{db_path}）: {e}"));
    let db = Db::from_connection(db_conn);

    let state = api::AdminState {
        store: Arc::new(store),
        db: Arc::new(db),
        compaction_ratio,
    };

    // SPA 配信: web/dist を静的配信し、未知パスは index.html を返す（クライアント側ルーティング）。
    let index = std::path::Path::new(&web_dist).join("index.html");
    let spa = ServeDir::new(&web_dist).fallback(ServeFile::new(index));

    let app = api::create_router(state)
        .fallback_service(spa)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("admin-server listening on http://{addr} (db={db_path}, web_dist={web_dist})");
    axum::serve(listener, app).await
}
