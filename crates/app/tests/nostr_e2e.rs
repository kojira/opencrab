//! 実リレーで Nostr の場を**通しで**動かす E2E（タスク#1・配線漏れの是正の検証）。ネットワークに触るので
//! 既定の `cargo test` を汚さないよう `#[ignore]`。設計で一番新しいところ——「流れてくるものが溜まり、
//! メンションと返信だけ即応、残りは窓でまとめて 1 ターン、取りに行かない」——が、**生きた系で**動くことを示す。
//!
//! これは「決定的テストの中にしか無い」機構を、実物の配線（設定→場→ゲート→実リレー）で走らせる。
//!
//! 走らせ方（要: 事前に nostr-gate をビルド）:
//! ```sh
//!   export CARGO_TARGET_DIR=$PWD/.cargo-target   # 内蔵ディスクを使わない
//!   cargo build -p opencrab-nostr-gate --bin nostr-gate
//!   E2E_NOSTR_GATE_BIN=$PWD/.cargo-target/debug/nostr-gate \
//!   E2E_RELAY=wss://yabu.me \
//!     cargo test -p opencrab-app --test nostr_e2e -- --ignored --nocapture
//! ```
//!
//! 鍵の安全: ゲートは**起動のたびに使い捨て鍵を生成**し、その npub を stderr に出す（本番の鍵は一切読まない）。
//! 投稿側の鍵もこのテストプロセス内で毎回生成する使い捨て。本番の身元は構造的に紛れ込まない。

use futures_util::{SinkExt, StreamExt};
use opencrab_app::{bind_unix, EchoEngine, Host, PlaceSpec};
use opencrab_nostr_gate::nostr::{self, Key};
use opencrab_nostr_gate::relay::{self, RelayMsg};
use opencrab_port::{EventKind, GateName, Property};
use opencrab_social_runtime::{ImmediateFrom, Policy};
use opencrab_store::Store;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_tungstenite::tungstenite::Message;

const NOSTR: &str = "nostr";
const BATCH_MS: i64 = 6000;

/// この場（nostr）の spoke（エージェントの発話＝ターンが起きた印）の数を数える。
fn spoke_count(host: &Host, address: &str) -> usize {
    let page = host
        .sys
        .read_log(&GateName::new(NOSTR), address, 1, 500)
        .expect("read_log");
    page.events
        .iter()
        .filter(|e| e.kind == EventKind::Spoke)
        .count()
}

async fn wait(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// リレーへ 1 件発行し、その id への OK（真）を待つ。失敗は panic（E2E なので明示的に落とす）。
async fn publish(
    sink: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    id_hex: &str,
    event: &serde_json::Value,
    label: &str,
) {
    sink.send(relay::event_frame(event))
        .await
        .expect("send EVENT");
    loop {
        let text = match tokio::time::timeout(Duration::from_secs(20), stream.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => t,
            Ok(Some(Ok(Message::Ping(p)))) => {
                let _ = sink.send(Message::Pong(p)).await;
                continue;
            }
            Ok(Some(Ok(_))) => continue,
            other => panic!("{label}: リレーからの OK を待てなかった: {other:?}"),
        };
        if let RelayMsg::Ok { id, ok, msg } = relay::parse_relay(&text) {
            if id == id_hex {
                assert!(ok, "{label}: リレーが発行を拒否した: {msg}");
                eprintln!("== {label}: relay OK");
                return;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
#[ignore]
async fn nostr_place_accumulates_fires_immediate_on_mention_and_batches() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let relay_url = std::env::var("E2E_RELAY").unwrap_or_else(|_| "wss://yabu.me".to_string());
    let gate_bin = std::env::var("E2E_NOSTR_GATE_BIN")
        .expect("E2E_NOSTR_GATE_BIN（nostr-gate バイナリのパス）を設定して走らせる（ヘッダ参照）");

    // --- core をこのプロセス内で起こす（EchoEngine・実ソケットでプラグインを受ける）。---
    let scratch = std::env::temp_dir().join(format!(
        "opencrab-nostr-gate-e2e-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&scratch);
    let store = Store::new_in_memory().expect("store");
    let host = Host::boot_with_engine(store, Arc::new(EchoEngine));
    let listener = bind_unix(&scratch).expect("bind unix");
    let serve_host = host.clone();
    tokio::spawn(async move {
        let _ = serve_host.serve_unix(listener).await;
    });

    // --- 実プロセスの nostr-gate を起動（使い捨て鍵を生成・npub を stderr に出す）。---
    let mut child = tokio::process::Command::new(&gate_bin)
        .arg(scratch.to_str().unwrap())
        .env("NOSTR_GATE_RELAY", &relay_url)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn nostr-gate");
    let stderr = child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();

    // 使い捨て npub を stderr から読む（本番の鍵でないことをオペレータが確認できる印・そのものを掴む）。
    let gate_npub = {
        let read = async {
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[gate] {line}");
                if let Some(pos) = line.find("npub1") {
                    return line[pos..].split_whitespace().next().unwrap().to_string();
                }
            }
            panic!("gate stderr に npub が出なかった");
        };
        tokio::time::timeout(Duration::from_secs(10), read)
            .await
            .expect("npub を待てなかった")
    };
    eprintln!("== ゲートの使い捨て npub（エージェントの Nostr 上の宛先）= {gate_npub}");
    // 残りの stderr は捨てずに流し続ける（証跡）。
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[gate] {line}");
        }
    });

    // gate が hello を送って core に登録されるまで待つ。
    for _ in 0..50 {
        if host.sys.gate_spec(&GateName::new(NOSTR)).is_some() {
            break;
        }
        wait(100).await;
    }
    assert!(
        host.sys.gate_spec(&GateName::new(NOSTR)).is_some(),
        "nostr ゲートが接続・登録されなかった"
    );

    // --- 投稿側の使い捨て鍵（このプロセス内。秘密はここから出ない）。---
    let poster = Key::generate();
    let poster_npub = poster.npub.clone();
    eprintln!("== 投稿側の使い捨て npub = {poster_npub}");

    // --- Nostr の場を「設定で」起こす（配線漏れの是正の実物）。---
    // 住所＝投稿側の kind-1 を購読するフィルタ。発火方針: メンション・返信だけ即応、残りは窓でまとめて 1 ターン。
    // identities: エージェントの Nostr 上の宛先（ゲートの npub）。これで「自分宛の言及」が名寄せで解決する。
    let address = format!("filter:kind=1&author={poster_npub}");
    let policy = Policy::immediate_on(&[Property::MentionsMe, Property::RepliesToMe])
        .with_from(ImmediateFrom::Anyone)
        .with_batch_ms(BATCH_MS);
    let spec = PlaceSpec {
        address: address.clone(),
        gate: NOSTR.to_string(),
        name: "エージェントA".to_string(),
        persona: "エージェントA".to_string(),
        policy,
        identities: vec![(NOSTR.to_string(), gate_npub.clone())],
    };
    let (place, _agent) = host.provision_place(&spec);
    // ゲートは既に繋がっているので、設定の記録に続けて実際の購読を張る（bind を送る・プロトコル§02）。
    host.sys
        .bind_place(place, NOSTR, &address)
        .await
        .expect("bind nostr place");
    eprintln!("== 場を起こした place={place} gate={NOSTR} address={address}");
    // 購読が実リレーに届くまで少し待つ。
    wait(1500).await;

    // --- 投稿側をリレーへ繋ぐ。---
    let ws = relay::connect(&relay_url)
        .await
        .expect("connect relay (poster)");
    let (mut sink, mut stream) = ws.split();

    // === (1) 溜まる: 3 件の平の note（誰宛でもない）。即応せず、窓が来るまでターンが起きない。===
    for i in 1..=3u32 {
        let now = nostr::now_secs();
        let (id_hex, ev) = nostr::build_signed(
            &poster,
            1,
            json!([]),
            &format!("e2e タイムライン投稿 {i}（誰宛でもない）"),
            now,
        );
        publish(&mut sink, &mut stream, &id_hex, &ev, &format!("plain#{i}")).await;
        wait(300).await;
    }
    // 窓の途中で確認: note は届いているがターンはまだ起きていない（溜まっている）。
    wait(2000).await;
    let mid = spoke_count(&host, &address);
    eprintln!("== 窓の途中（3 件投稿後 ~2s）: spoke（ターン）= {mid}");
    assert_eq!(mid, 0, "溜まっている間はターンが起きない（即応しない）");

    // 窓が明けるまで待つ。まとめて 1 ターンだけ起きる。
    wait((BATCH_MS as u64) + 2500).await;
    let after_batch = spoke_count(&host, &address);
    eprintln!("== 窓明け後: spoke（ターン）= {after_batch}（3 件が 1 ターンにまとまった）");
    assert_eq!(after_batch, 1, "溜まった 3 件は窓で 1 ターンにまとまる");

    // === (2) 即応: エージェントを p-tag で言及した note。窓を待たず即座にターンが起きる。===
    let now = nostr::now_secs();
    let gate_hex = nostr::npub_to_hex(&gate_npub).expect("npub->hex");
    let (id_hex, ev) = nostr::build_signed(
        &poster,
        1,
        json!([["p", gate_hex]]),
        "エージェントA、これ見た？（メンション）",
        now,
    );
    publish(&mut sink, &mut stream, &id_hex, &ev, "mention").await;
    // 窓（6s）より十分短い時間で、もう 1 ターン起きること。
    let mut immediate_ok = false;
    for _ in 0..30 {
        wait(200).await;
        if spoke_count(&host, &address) >= 2 {
            immediate_ok = true;
            break;
        }
    }
    let total = spoke_count(&host, &address);
    eprintln!("== メンション後（~<3s）: spoke（ターン）= {total}");
    assert!(
        immediate_ok,
        "メンションは窓を待たず即応する（spoke が 2 に増える）: total={total}"
    );

    // --- 後始末 ---
    let _ = child.kill().await;
    let _ = std::fs::remove_file(&scratch);
    eprintln!("== E2E 完了: 溜まった(0) → 窓でまとまった(1) → メンションで即応(2)");
}
