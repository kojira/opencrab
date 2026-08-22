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

use opencrab_app::{bind_unix, parse_places_config, Host};
use opencrab_store::Store;
use std::path::Path;

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
        Store::open(&db_path).expect("open store")
    };

    let host = Host::boot(store);

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
    host.serve_unix(listener).await
}
