//! 実リレー `wss://yabu.me` に繋ぐ統合テスト。**既定の `cargo test` を汚さないよう `#[ignore]`**。
//! 走らせ方: `cargo test -p opencrab-nostr-gate --test relay -- --ignored --nocapture`
//!
//! 確かめること（タスクの検証）:
//!   1. 使い捨て npub を stderr に出す。
//!   2. REQ 購読で EVENT を受け取れる（EOSE まで件数を数える）。
//!   3. 自分で署名した kind-1 note を発行し OK が真で通る。
//!   4. その id で REQ し読み戻せる＝往復（origin note1 ↔ event id）。

use futures_util::{SinkExt, StreamExt};
use opencrab_nostr_gate::nostr::{self, Key};
use opencrab_nostr_gate::relay::{self, RelayMsg};
use serde_json::json;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

const RELAY: &str = "wss://yabu.me";

async fn next_text(
    stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    sink: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    secs: u64,
) -> Option<String> {
    loop {
        match tokio::time::timeout(Duration::from_secs(secs), stream.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => return Some(t),
            Ok(Some(Ok(Message::Ping(p)))) => {
                let _ = sink.send(Message::Pong(p)).await;
            }
            Ok(Some(Ok(Message::Binary(b)))) => {
                return Some(String::from_utf8_lossy(&b).to_string())
            }
            Ok(Some(Ok(_))) => {} // Pong/Close 等
            Ok(Some(Err(_))) | Ok(None) => return None,
            Err(_) => return None, // 期限切れ
        }
    }
}

/// イベントを発行し、その id への OK を待つ（真偽を返す）。
async fn publish_wait_ok(
    stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    sink: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    event: &serde_json::Value,
    id_hex: &str,
    label: &str,
) -> bool {
    sink.send(relay::event_frame(event))
        .await
        .expect("send EVENT");
    loop {
        let text = match next_text(stream, sink, 20).await {
            Some(t) => t,
            None => return false,
        };
        if let RelayMsg::Ok { id, ok, msg } = relay::parse_relay(&text) {
            if id == id_hex {
                eprintln!("== {label} の OK: ok={ok} msg={msg:?}");
                return ok;
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn live_roundtrip_against_yabu() {
    // wss:// のため暗号プロバイダを入れる（main と同じ）。
    let _ = rustls::crypto::ring::default_provider().install_default();

    // 1. 使い捨て鍵。
    let key = Key::generate();
    eprintln!("== 使い捨て npub = {}", key.npub);
    eprintln!("== pubkey(hex) = {}", key.pubkey_hex);

    let ws = relay::connect(RELAY).await.expect("connect yabu.me");
    let (mut sink, mut stream) = ws.split();

    // 2. 購読して EVENT を数える（EOSE まで）。
    let subid = "it-sub";
    sink.send(relay::req_frame(subid, &json!({"kinds": [1], "limit": 20})))
        .await
        .expect("send REQ");
    let mut received = 0usize;
    loop {
        let text = match next_text(&mut stream, &mut sink, 15).await {
            Some(t) => t,
            None => break,
        };
        match relay::parse_relay(&text) {
            RelayMsg::Event { .. } => received += 1,
            RelayMsg::Eose { .. } => break,
            _ => {}
        }
    }
    eprintln!("== 購読で受け取った EVENT = {received} 件（EOSE まで）");
    assert!(received > 0, "yabu.me の kind-1 が 1 件も来ないのは不自然");

    // 3. 自分で署名した kind-1 note を発行し OK を待つ。
    let (id_hex, event) = nostr::build_say(
        &key,
        "opencrab2 nostr-gate 疎通テスト（使い捨て鍵・自動投稿）",
        None,
        nostr::now_secs(),
    )
    .expect("build say");
    // 自分の署名は自分で検証できる（発行前の健全性）。
    nostr::verify_event(&event).expect("own signature verifies");
    eprintln!("== 発行する event id = {id_hex}");
    sink.send(relay::event_frame(&event))
        .await
        .expect("send EVENT");

    let mut ok = false;
    loop {
        let text = match next_text(&mut stream, &mut sink, 20).await {
            Some(t) => t,
            None => break,
        };
        if let RelayMsg::Ok { id, ok: o, msg } = relay::parse_relay(&text) {
            if id == id_hex {
                eprintln!("== 発行の OK: ok={o} msg={msg:?}");
                ok = o;
                break;
            }
        }
    }
    assert!(ok, "発行した note が OK で通らなかった");

    // 4. その id で読み戻す＝往復。origin(note1) ↔ event id も確かめる。
    let origin = nostr::note_of(&id_hex).expect("note_of");
    assert_eq!(
        nostr::event_id_of_origin(&origin).unwrap(),
        id_hex,
        "origin(note1) ↔ event id の往復"
    );
    let sub2 = "it-readback";
    sink.send(relay::req_frame(sub2, &json!({"ids": [id_hex]})))
        .await
        .expect("send readback REQ");
    let mut got = false;
    loop {
        let text = match next_text(&mut stream, &mut sink, 15).await {
            Some(t) => t,
            None => break,
        };
        match relay::parse_relay(&text) {
            RelayMsg::Event { event, .. } => {
                if event.get("id").and_then(|x| x.as_str()) == Some(id_hex.as_str()) {
                    // リレー越しに戻ってきた自分の note の署名も検証（id・sig が正しい）。
                    nostr::verify_event(&event).expect("read-back event verifies");
                    got = true;
                }
            }
            RelayMsg::Eose { .. } => break,
            _ => {}
        }
    }
    assert!(got, "発行した note を id で読み戻せなかった");
    eprintln!("== 読み戻し OK・origin 往復 OK（npub={}）", key.npub);

    // 5. 足した効果を実リレーで確かめる（boost・quote・retract）。対象は先に発行した自分の note（origin）。
    // boost（リポスト・kind6）。
    let (boost_id, boost_ev) =
        nostr::build_boost(&key, &origin, nostr::now_secs()).expect("build boost");
    nostr::verify_event(&boost_ev).expect("boost verifies");
    assert!(
        publish_wait_ok(&mut stream, &mut sink, &boost_ev, &boost_id, "boost").await,
        "boost が OK で通らなかった"
    );

    // quote（引用・kind1+q）。origin(note1) が付き、読み戻せる。
    let (quote_id, quote_ev) =
        nostr::build_quote(&key, "引用テスト（使い捨て鍵）", &origin, nostr::now_secs())
            .expect("build quote");
    nostr::verify_event(&quote_ev).expect("quote verifies");
    assert!(
        publish_wait_ok(&mut stream, &mut sink, &quote_ev, &quote_id, "quote").await,
        "quote が OK で通らなかった"
    );
    let quote_origin = nostr::note_of(&quote_id).expect("note_of quote");
    assert_eq!(
        nostr::event_id_of_origin(&quote_origin).unwrap(),
        quote_id,
        "quote の origin(note1) ↔ event id の往復"
    );
    sink.send(relay::req_frame(
        "it-quote-readback",
        &json!({"ids": [quote_id]}),
    ))
    .await
    .expect("send quote readback REQ");
    let mut quote_got = false;
    loop {
        let text = match next_text(&mut stream, &mut sink, 15).await {
            Some(t) => t,
            None => break,
        };
        match relay::parse_relay(&text) {
            RelayMsg::Event { event, .. } => {
                if event.get("id").and_then(|x| x.as_str()) == Some(quote_id.as_str()) {
                    nostr::verify_event(&event).expect("read-back quote verifies");
                    quote_got = true;
                }
            }
            RelayMsg::Eose { .. } => break,
            _ => {}
        }
    }
    assert!(quote_got, "quote を id で読み戻せなかった");

    // retract（取り消し・kind5）。対象は自分が出したもの（最初の note の origin）。
    let (retract_id, retract_ev) =
        nostr::build_retract(&key, &origin, nostr::now_secs()).expect("build retract");
    nostr::verify_event(&retract_ev).expect("retract verifies");
    assert!(
        publish_wait_ok(&mut stream, &mut sink, &retract_ev, &retract_id, "retract").await,
        "retract が OK で通らなかった"
    );

    eprintln!(
        "== boost/quote/retract すべて OK（quote origin={quote_origin}・npub={}）",
        key.npub
    );
}
