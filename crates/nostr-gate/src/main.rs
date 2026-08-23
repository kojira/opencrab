//! nostr-gate — Nostr ゲートのプラグイン（プロトコル 版1）。**別プロセス・別クレート**。
//!
//! 片側は core（Unix ソケットの 1 行 1 メッセージ JSON）、片側は実リレー `wss://yabu.me`（WebSocket・NIP-01）。
//! core の型（opencrab-*）は 1 つも使わない。線に載る JSON を仕様書だけを見て自分で組む（タスクの規律）。
//!
//! 場のチャネル＝watch のフィルタ（§02）:
//!   bind(address) → リレーへ REQ 購読、unbind → CLOSE。
//! 流れてくる note は core へ `event`（kind=said）として送るだけ——**ターンを起こすかは core が決める**（§03）。
//! 取りに行かない: 購読はライブ。ハートビートでタイムラインを取り直す機構は持たない。切れている間の
//! 取りこぼしは拾わない（§08）。
//!
//! 効果（§04）: say（返信は target 付き say）を kind-1 note に署名して EVENT で流し、**ack で origin
//! （note1...）を返す**（自分の投稿を後から指せる）。react（kind-7）は origin を返さない。
//!
//! **鍵は使い捨て**（既定は起動のたびに secp256k1 で新規生成し永続しない）。起動時に npub を stderr に出す。
//! 鍵ファイルや本番の鍵パスは一切読まない。env `NOSTR_GATE_NSEC` は明示的な使い捨て用途に限る。
//!
//! 使い方: `nostr-gate <core_socket>`（リレーは env `NOSTR_GATE_RELAY`、既定 wss://yabu.me）

use futures_util::{SinkExt, StreamExt};
use opencrab_nostr_gate::nostr::{self, Key};
use opencrab_nostr_gate::relay::{self, RelayMsg};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_tungstenite::tungstenite::Message;

const PROTOCOL: u64 = 1;
const HELLO_ID: &str = "hello-1";
const ADDRESS_FORM: &str = "^(npub1[a-z0-9]+|filter:.+)$";
const DEFAULT_RELAY: &str = "wss://yabu.me";
const MAX_LINE: usize = 1024 * 1024;
/// 発行（EVENT → OK）を待つ期限。§04 は effect を 5 分まで許すが、リレーの OK は速い。レート制限で
/// 待たされることは正常なので短く切りすぎない——60 秒は余裕を持った値（実測して詰める・§00）。
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(60);

/// 1 つの購読（§02 の bind）。addr → subid / Nostr フィルタ。**永続しない**（接続ごと・§08）。
struct Sub {
    subid: String,
    filter: Value,
    requested: bool,
}

struct Shared {
    key: Key,
    /// core への書き出し口。切れていれば None。
    outbound: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// 現在の core 接続で checked hello が受理されたか。relay の購読開始条件。
    core_hello_accepted: AtomicBool,
    /// 初回の checked hello 受理まで relay link の起動を待たせる。
    core_hello_ready: Notify,
    /// リレーへの書き出し口（フレーム）。切れていれば None。
    relay_tx: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    /// 結んでいる住所（§02）。core が切れたら畳んで CLOSE する（再接続で core が結び直す・§08）。
    subs: Mutex<HashMap<String, Sub>>,
    /// subid → 住所（受信 EVENT を正しい住所の `event` にするため）。
    subid_to_addr: Mutex<HashMap<String, String>>,
    /// 発行した EVENT の OK 待ち（event id → 待ち手）。
    pending_pub: Mutex<HashMap<String, oneshot::Sender<(bool, String)>>>,
    /// core へ送る `event` の id 用（応答は待たない・§03）。
    evctr: AtomicU64,
    /// core に送った inbound event の応答待ち。成功は忘れ、拒否は stderr へ出す。
    /// 配送自体は従来どおり fire-and-forget で、ターンを待たせない。
    pending_inbound: Mutex<HashSet<String>>,
    subctr: AtomicU64,
}

impl Shared {
    fn next_event_id(&self) -> String {
        format!("ev-{}", self.evctr.fetch_add(1, Ordering::SeqCst))
    }
    fn next_subid(&self) -> String {
        format!("sub-{}", self.subctr.fetch_add(1, Ordering::SeqCst))
    }
    fn send_core(&self, line: String) -> bool {
        match self.outbound.lock().unwrap().as_ref() {
            Some(tx) => tx.send(line).is_ok(),
            None => false,
        }
    }
    fn send_relay(&self, msg: Message) -> bool {
        match self.relay_tx.lock().unwrap().as_ref() {
            Some(tx) => tx.send(msg).is_ok(),
            None => false,
        }
    }
    fn accept_core_hello(&self) {
        self.core_hello_accepted.store(true, Ordering::Release);
        self.core_hello_ready.notify_one();
        request_subscriptions(self);
    }
    async fn wait_for_core_hello(&self) {
        while !self.core_hello_accepted.load(Ordering::Acquire) {
            let notified = self.core_hello_ready.notified();
            if self.core_hello_accepted.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
    /// core が切れた: 書き出し口を落とし、結びを畳んでリレーの購読を閉じる（再接続で結び直す・§08）。
    fn drop_core(&self) {
        self.core_hello_accepted.store(false, Ordering::Release);
        *self.outbound.lock().unwrap() = None;
        self.pending_inbound.lock().unwrap().clear();
        let subs: Vec<Sub> = self.subs.lock().unwrap().drain().map(|(_, s)| s).collect();
        self.subid_to_addr.lock().unwrap().clear();
        for s in subs {
            let _ = self.send_relay(relay::close_frame(&s.subid));
        }
    }

    /// EVENT を発行し OK を待つ（§04）。運べたら Ok、断られた/切れた/期限切れは Err（core は err を失敗として扱う）。
    async fn publish(&self, id_hex: String, event: &Value) -> Result<(), String> {
        let (tx, rx) = oneshot::channel::<(bool, String)>();
        self.pending_pub.lock().unwrap().insert(id_hex.clone(), tx);
        if !self.send_relay(relay::event_frame(event)) {
            self.pending_pub.lock().unwrap().remove(&id_hex);
            return Err("relay link down".to_string());
        }
        match tokio::time::timeout(PUBLISH_TIMEOUT, rx).await {
            Ok(Ok((true, _))) => Ok(()),
            Ok(Ok((false, m))) => Err(format!("relay rejected: {m}")),
            Ok(Err(_)) => Err("relay link dropped".to_string()), // 待ち手が落ちた＝切断
            Err(_) => {
                self.pending_pub.lock().unwrap().remove(&id_hex);
                Err("relay OK timeout".to_string())
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // wss:// のため rustls の暗号プロバイダを入れる（rustls 0.23 は既定を内蔵しない）。二重呼びは無害。
    let _ = rustls::crypto::ring::default_provider().install_default();

    let socket_path = std::env::args()
        .nth(1)
        .expect("usage: nostr-gate <core_socket>");
    let relay_url = std::env::var("NOSTR_GATE_RELAY").unwrap_or_else(|_| DEFAULT_RELAY.to_string());

    // 鍵は使い捨て。既定は新規生成。env NOSTR_GATE_NSEC は明示的な使い捨て用途に限る（既定では使わない）。
    let key = match std::env::var("NOSTR_GATE_NSEC") {
        Ok(nsec) if !nsec.trim().is_empty() => match Key::from_nsec(&nsec) {
            Ok(k) => {
                eprintln!("nostr-gate: NOSTR_GATE_NSEC を使用（明示的な使い捨て鍵）");
                k
            }
            Err(e) => {
                eprintln!("nostr-gate: NOSTR_GATE_NSEC が不正: {e}");
                std::process::exit(2);
            }
        },
        _ => Key::generate(),
    };
    // オペレータが「本番でない」ことを確認できるように、使い捨ての npub を stderr に出す。
    eprintln!("nostr-gate: 使い捨て npub = {}", key.npub);
    eprintln!("nostr-gate: relay = {relay_url}");

    let shared = Arc::new(Shared {
        key,
        outbound: Mutex::new(None),
        core_hello_accepted: AtomicBool::new(false),
        core_hello_ready: Notify::new(),
        relay_tx: Mutex::new(None),
        subs: Mutex::new(HashMap::new()),
        subid_to_addr: Mutex::new(HashMap::new()),
        pending_pub: Mutex::new(HashMap::new()),
        evctr: AtomicU64::new(1),
        pending_inbound: Mutex::new(HashSet::new()),
        subctr: AtomicU64::new(1),
    });

    let link = tokio::spawn(run_core_link(socket_path, shared.clone()));
    let relay = tokio::spawn({
        let shared = shared.clone();
        async move {
            // relay は checked hello が受理されるまで起動しない。これにより、core が bind を送り
            // gate が受理した後の REQ より先に relay EVENT が存在する起動順を構造的に除く。
            shared.wait_for_core_hello().await;
            run_relay_link(shared, relay_url).await;
        }
    });
    let _ = tokio::join!(link, relay);
}

// ================= core への線（Unix ソケット・プロトコル §00-§06）=================

async fn run_core_link(socket_path: String, shared: Arc<Shared>) {
    loop {
        // core が落ちている間は繋がらない。繋がるまで待つ（誰が起こすかはプロトコルの外・§08）。
        let stream = match UnixStream::connect(&socket_path).await {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        serve_core(stream, &shared).await;
        // 切れた。名乗りからやり直す（§08）。結びは畳み、購読は閉じる（core が改めて bind する）。
        shared.drop_core();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn serve_core(stream: UnixStream, shared: &Arc<Shared>) {
    let (mut read_half, mut write_half) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

    let writer = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if write_half.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = write_half.flush().await;
        }
    });

    // 名乗り（接続して最初に 1 回・§01）。**hello を先に列へ入れてから** outbound を見せる——さもないと
    // リレーからの `event` が hello より先に流れ、core が「名乗る前に喋った」として切る。順序は 1 本の列で守る。
    // 運べる効果（§04）: say・react・boost（リポスト・kind6）・quote（引用・kind1+q）・retract（削除・kind5）。
    // ツール（§06・§10）: nostr-whoami — この使い捨て鍵の npub（公開鍵）を返す。**秘密鍵は境界の中に留まる**
    //   ——エージェントは鍵を持ち回らずに自分の Nostr 上の宛先（npub）を知れる。他の場（web 等）から展開して
    //   使える（§10 の索引・展開の対象はゲートのツール）。capabilities 無し（open は要らない・住所を先に決める）。
    let hello = json!({
        "id": HELLO_ID, "m": "hello", "protocol": PROTOCOL,
        "name": "nostr", "address_form": ADDRESS_FORM,
        "tools": [
            {"name": "nostr-whoami",
             "description": "この Nostr ゲートの公開鍵（npub1...）を返す。秘密鍵は返さない——自分の Nostr 上の宛先を、鍵を持ち回さずに知るためのツール。",
             "params": {"type": "object", "properties": {}, "required": []}}
        ],
        "effects": ["say", "react", "boost", "quote", "retract"], "capabilities": []
    });
    let _ = out_tx.send(hello.to_string());
    *shared.outbound.lock().unwrap() = Some(out_tx.clone());

    // 読み取りループ。**枠組みが壊れたら切る**（UTF-8 でない・JSON でない・物体でない・1 MiB 超・§00）。
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    'read: loop {
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            if pos > MAX_LINE {
                break 'read;
            }
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = &line[..line.len() - 1];
            let s = match std::str::from_utf8(line) {
                Ok(s) => s,
                Err(_) => break 'read,
            };
            let v: Value = match serde_json::from_str(s) {
                Ok(v) => v,
                Err(_) => break 'read,
            };
            if !v.is_object() {
                break 'read;
            }
            handle_core_message(&v, shared);
        }
        if buf.len() > MAX_LINE {
            break;
        }
        match read_half.read(&mut chunk).await {
            Ok(0) => break, // EOF = core が切れた／落ちた
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    drop(out_tx);
    let _ = writer.await;
}

/// core からの 1 メッセージを処理する。`m` があれば要求、無ければ自分の要求への応答。
fn handle_core_message(v: &Value, shared: &Arc<Shared>) {
    let id = v.get("id").and_then(|x| x.as_str()).map(|s| s.to_string());
    let m = match v.get("m").and_then(|x| x.as_str()) {
        Some(m) => m,
        None => {
            if let Some(id) = id {
                if id == HELLO_ID {
                    if v.get("ok").is_some() {
                        shared.accept_core_hello();
                    } else if let Some(error) = v.get("err") {
                        eprintln!("nostr-gate: core rejected hello: {error}");
                    }
                    return;
                }
                let was_inbound = shared.pending_inbound.lock().unwrap().remove(&id);
                if was_inbound {
                    if let Some(error) = v.get("err") {
                        eprintln!("nostr-gate: core rejected inbound event id={id}: {error}");
                    }
                }
            }
            return;
        }
    };
    match m {
        // 通知（応答しない・§05）。入力中などは今は描かない。
        "activity" => {}
        "bind" => {
            let id = match id {
                Some(id) => id,
                None => return,
            };
            let body = match v.get("address").and_then(|x| x.as_str()) {
                Some(addr) => on_bind(shared, addr),
                None => json!({"err": {"code": "missing_field", "at": "bind.address"}}),
            };
            respond(shared, &id, body);
        }
        "unbind" => {
            let id = match id {
                Some(id) => id,
                None => return,
            };
            if let Some(addr) = v.get("address").and_then(|x| x.as_str()) {
                on_unbind(shared, addr);
                respond(shared, &id, json!({"ok": {}}));
            } else {
                respond(
                    shared,
                    &id,
                    json!({"err": {"code": "missing_field", "at": "unbind.address"}}),
                );
            }
        }
        "effect" => {
            // 発行は OK 待ちで長引き得る（§04・最大 5 分）。読み取りを止めないよう別タスクへ逃がす。
            if let Some(id) = id {
                let shared = shared.clone();
                let v = v.clone();
                tokio::spawn(async move {
                    handle_effect(&shared, &id, &v).await;
                });
            }
        }
        // ツールの呼び出し（§06）。権限は core が判定済み・引数は名乗りの JSON Schema で検証済み。
        // 名乗ったツールだけが来る。結果は文字列（エージェントが読む）。失敗は err（近いものに寄せない・§00）。
        "tool" => {
            let id = match id {
                Some(id) => id,
                None => return,
            };
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let body = match name {
                // 公開鍵（npub）を返す。秘密鍵は境界の中に留まる——鍵を持ち回さずに宛先を知れる。
                "nostr-whoami" => json!({"ok": {"result": shared.key.npub.clone()}}),
                other => json!({"err": {"code": "unknown_tool", "detail": other}}),
            };
            respond(shared, &id, body);
        }
        // open は名乗っていないので来ないはず。来たら err（近いものに寄せない・§00）。
        other => {
            if let Some(id) = id {
                respond(
                    shared,
                    &id,
                    json!({"err": {"code": "unknown_message", "detail": other}}),
                );
            }
        }
    }
}

/// 応答を返す（`body` は `{"ok":..}` か `{"err":..}`・id は呼び手が足す）。
fn respond(shared: &Arc<Shared>, id: &str, mut body: Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("id".into(), Value::String(id.to_string()));
    }
    let _ = shared.send_core(body.to_string());
}

/// bind: 住所を Nostr フィルタへ写して REQ を張る（§02）。冪等——既に結んでいれば ok。
fn on_bind(shared: &Arc<Shared>, addr: &str) -> Value {
    if shared.subs.lock().unwrap().contains_key(addr) {
        return json!({"ok": {}}); // 冪等（§02）
    }
    let filter = match nostr::parse_address(addr) {
        Ok(f) => f,
        // 住所は address_form で検証済みだが Nostr フィルタへ写せない → 購読できないので err（§02）。
        Err(e) => {
            return json!({"err": {"code": "cannot_subscribe", "at": "bind.address", "detail": e}})
        }
    };
    let subid = shared.next_subid();
    shared.subs.lock().unwrap().insert(
        addr.to_string(),
        Sub {
            subid: subid.clone(),
            filter: filter.clone(),
            requested: false,
        },
    );
    shared
        .subid_to_addr
        .lock()
        .unwrap()
        .insert(subid.clone(), addr.to_string());
    // checked hello 受理後かつ relay 接続済みなら REQ。どちらかが未完了なら subs に残り、
    // relay 接続時に resubscribe される（§08）。
    request_subscriptions(shared);
    json!({"ok": {}})
}

/// unbind: 購読を閉じる（§02）。冪等——結んでいなくても ok。
fn on_unbind(shared: &Arc<Shared>, addr: &str) {
    if let Some(sub) = shared.subs.lock().unwrap().remove(addr) {
        shared.subid_to_addr.lock().unwrap().remove(&sub.subid);
        let _ = shared.send_relay(relay::close_frame(&sub.subid));
    }
}

/// 効果を Nostr へ運ぶ（§04）。say/react のみ名乗っている。運べたら ok、運べなければ err。
async fn handle_effect(shared: &Arc<Shared>, id: &str, v: &Value) {
    let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("");
    let target = v.get("target").and_then(|x| x.as_str());
    let now = nostr::now_secs();

    let body = match kind {
        "say" => {
            let text = match v.pointer("/payload/text").and_then(|x| x.as_str()) {
                Some(t) => t,
                None => {
                    respond(
                        shared,
                        id,
                        json!({"err": {"code": "missing_field", "at": "effect.payload.text"}}),
                    );
                    return;
                }
            };
            // target があれば返信（e-tag）。origin から event id を復元できなければ err。
            match nostr::build_say(&shared.key, text, target, now) {
                Ok((id_hex, event)) => match shared.publish(id_hex.clone(), &event).await {
                    // say は ack で origin（note1...）を返す——自分の発話を後から指せるように（§04）。
                    Ok(()) => match nostr::note_of(&id_hex) {
                        Ok(note) => json!({"ok": {"delivered": true, "origin": note}}),
                        Err(e) => {
                            json!({"err": {"code": "internal", "at": "effect.say.origin", "detail": e}})
                        }
                    },
                    Err(e) => {
                        json!({"err": {"code": "delivery_failed", "at": "effect.say", "detail": e}})
                    }
                },
                Err(e) => {
                    json!({"err": {"code": "bad_target", "at": "effect.target", "detail": e}})
                }
            }
        }
        "react" => {
            let symbol = match v.pointer("/payload/symbol").and_then(|x| x.as_str()) {
                Some(s) => s,
                None => {
                    respond(
                        shared,
                        id,
                        json!({"err": {"code": "missing_field", "at": "effect.payload.symbol"}}),
                    );
                    return;
                }
            };
            let target = match target {
                Some(t) => t,
                None => {
                    respond(
                        shared,
                        id,
                        json!({"err": {"code": "missing_field", "at": "effect.target"}}),
                    );
                    return;
                }
            };
            match nostr::build_react(&shared.key, symbol, target, now) {
                Ok((id_hex, event)) => match shared.publish(id_hex, &event).await {
                    // react は origin を返さない（§04）。
                    Ok(()) => json!({"ok": {"delivered": true}}),
                    Err(e) => {
                        json!({"err": {"code": "delivery_failed", "at": "effect.react", "detail": e}})
                    }
                },
                Err(e) => {
                    json!({"err": {"code": "bad_target", "at": "effect.target", "detail": e}})
                }
            }
        }
        "boost" => {
            // リポスト。target（相手の origin）必須・payload {}・**ack で origin を返す**（§04）。
            // リポストは外界に新しい投稿（kind6）を作る効果——その識別子（note1...）を返さないと、
            // 自分のリポストを後から取り消せない・反応できない。
            let target = match target {
                Some(t) => t,
                None => {
                    respond(
                        shared,
                        id,
                        json!({"err": {"code": "missing_field", "at": "effect.target"}}),
                    );
                    return;
                }
            };
            match nostr::build_boost(&shared.key, target, now) {
                Ok((id_hex, event)) => match shared.publish(id_hex.clone(), &event).await {
                    Ok(()) => match nostr::note_of(&id_hex) {
                        Ok(note) => json!({"ok": {"delivered": true, "origin": note}}),
                        Err(e) => {
                            json!({"err": {"code": "internal", "at": "effect.boost.origin", "detail": e}})
                        }
                    },
                    Err(e) => {
                        json!({"err": {"code": "delivery_failed", "at": "effect.boost", "detail": e}})
                    }
                },
                Err(e) => {
                    json!({"err": {"code": "bad_target", "at": "effect.target", "detail": e}})
                }
            }
        }
        "quote" => {
            // 引用。target（引用元の origin）必須・payload {text}・**ack で origin を返す**（新しい投稿・§04）。
            let text = match v.pointer("/payload/text").and_then(|x| x.as_str()) {
                Some(t) => t,
                None => {
                    respond(
                        shared,
                        id,
                        json!({"err": {"code": "missing_field", "at": "effect.payload.text"}}),
                    );
                    return;
                }
            };
            let target = match target {
                Some(t) => t,
                None => {
                    respond(
                        shared,
                        id,
                        json!({"err": {"code": "missing_field", "at": "effect.target"}}),
                    );
                    return;
                }
            };
            match nostr::build_quote(&shared.key, text, target, now) {
                Ok((id_hex, event)) => match shared.publish(id_hex.clone(), &event).await {
                    Ok(()) => match nostr::note_of(&id_hex) {
                        Ok(note) => json!({"ok": {"delivered": true, "origin": note}}),
                        Err(e) => {
                            json!({"err": {"code": "internal", "at": "effect.quote.origin", "detail": e}})
                        }
                    },
                    Err(e) => {
                        json!({"err": {"code": "delivery_failed", "at": "effect.quote", "detail": e}})
                    }
                },
                Err(e) => {
                    json!({"err": {"code": "bad_target", "at": "effect.target", "detail": e}})
                }
            }
        }
        "retract" => {
            // 取り消し。target（自分が出したものの origin）必須・payload {}・ack は origin を返さない（§04）。
            let target = match target {
                Some(t) => t,
                None => {
                    respond(
                        shared,
                        id,
                        json!({"err": {"code": "missing_field", "at": "effect.target"}}),
                    );
                    return;
                }
            };
            match nostr::build_retract(&shared.key, target, now) {
                Ok((id_hex, event)) => match shared.publish(id_hex, &event).await {
                    Ok(()) => json!({"ok": {"delivered": true}}),
                    Err(e) => {
                        json!({"err": {"code": "delivery_failed", "at": "effect.retract", "detail": e}})
                    }
                },
                Err(e) => {
                    json!({"err": {"code": "bad_target", "at": "effect.target", "detail": e}})
                }
            }
        }
        // 名乗っていない種別は近いものへ寄せない（§00）。
        other => json!({"err": {"code": "unknown_enum", "at": "effect.kind", "detail": other}}),
    };
    respond(shared, id, body);
}

// ================= 実リレーへの線（WebSocket・NIP-01・§08 の再接続）=================

async fn run_relay_link(shared: Arc<Shared>, url: String) {
    loop {
        let ws = match relay::connect(&url).await {
            Ok(w) => w,
            Err(e) => {
                eprintln!("nostr-gate: relay 接続失敗: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        eprintln!("nostr-gate: relay 接続 {url}");
        let (mut sink, mut stream) = ws.split();
        let (wtx, mut wrx) = mpsc::unbounded_channel::<Message>();
        *shared.relay_tx.lock().unwrap() = Some(wtx.clone());

        let writer = tokio::spawn(async move {
            while let Some(m) = wrx.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
        });

        // いま結んでいる購読を張り直す（§08・再接続後は生きている bind を改めて購読する）。
        // core 再接続中なら、新しい checked hello と bind が完了するまで REQ は出さない。
        request_subscriptions(&shared);

        while let Some(item) = stream.next().await {
            let msg = match item {
                Ok(m) => m,
                Err(_) => break,
            };
            match msg {
                Message::Text(t) => dispatch_relay(&shared, &t),
                Message::Binary(b) => dispatch_relay(&shared, &String::from_utf8_lossy(&b)),
                Message::Ping(p) => {
                    let _ = wtx.send(Message::Pong(p)); // 生存応答（多くのリレーが ping する）
                }
                Message::Close(_) => break,
                _ => {}
            }
        }

        // 切れた。発行の待ち手を落とし（再送しない・§08）、購読は subs に残して再接続で張り直す。
        *shared.relay_tx.lock().unwrap() = None;
        for sub in shared.subs.lock().unwrap().values_mut() {
            sub.requested = false;
        }
        let waiters: Vec<_> = shared.pending_pub.lock().unwrap().drain().collect();
        for (_id, w) in waiters {
            let _ = w.send((false, "relay disconnected".to_string()));
        }
        writer.abort();
        eprintln!("nostr-gate: relay 切断、再接続します");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn request_subscriptions(shared: &Shared) {
    if !shared.core_hello_accepted.load(Ordering::Acquire) {
        return;
    }
    let relay_tx = match shared.relay_tx.lock().unwrap().as_ref().cloned() {
        Some(relay_tx) => relay_tx,
        None => return,
    };
    let mut subs = shared.subs.lock().unwrap();
    for sub in subs.values_mut().filter(|sub| !sub.requested) {
        if relay_tx
            .send(relay::req_frame(&sub.subid, &sub.filter))
            .is_ok()
        {
            sub.requested = true;
        }
    }
}

/// リレーからの 1 メッセージを処理する。EVENT は core の `event` へ、OK は発行の待ち手へ。
fn dispatch_relay(shared: &Arc<Shared>, text: &str) {
    match relay::parse_relay(text) {
        RelayMsg::Event { subid, event } => {
            let addr = shared.subid_to_addr.lock().unwrap().get(&subid).cloned();
            if let Some(addr) = addr {
                // 自分の投稿は None（自己エコーを入力へ戻さない）。他人の note だけ core へ送る。
                if let Some(mut ce) =
                    nostr::incoming_to_core_event(&event, &addr, &shared.key.pubkey_hex)
                {
                    let id = shared.next_event_id();
                    if let Some(obj) = ce.as_object_mut() {
                        obj.insert("id".into(), Value::String(id.clone()));
                    }
                    // ターンを起こすかは core が決める（§03）。応答（seq）は待たない。
                    shared.pending_inbound.lock().unwrap().insert(id.clone());
                    if !shared.send_core(ce.to_string()) {
                        shared.pending_inbound.lock().unwrap().remove(&id);
                        eprintln!(
                            "nostr-gate: could not send inbound event id={id}: core link unavailable"
                        );
                    }
                }
            }
            // 未知の subid（畳んだ後に届いた EVENT 等）は無視。
        }
        RelayMsg::Ok { id, ok, msg } => {
            if let Some(w) = shared.pending_pub.lock().unwrap().remove(&id) {
                let _ = w.send((ok, msg));
            }
            // 待ち手が無い OK は捨てる（期限切れ・別プロセス由来と線上で区別できない・§00）。
        }
        // 購読の終端・通知・購読打ち切り・未知はログに載せるだけ（記録は持たない・§10）。
        RelayMsg::Eose { .. }
        | RelayMsg::Notice(_)
        | RelayMsg::Closed { .. }
        | RelayMsg::Other(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_with_links() -> (
        Arc<Shared>,
        mpsc::UnboundedReceiver<String>,
        mpsc::UnboundedReceiver<Message>,
    ) {
        let (core_tx, core_rx) = mpsc::unbounded_channel();
        let (relay_tx, relay_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            key: Key::generate(),
            outbound: Mutex::new(Some(core_tx)),
            core_hello_accepted: AtomicBool::new(false),
            core_hello_ready: Notify::new(),
            relay_tx: Mutex::new(Some(relay_tx)),
            subs: Mutex::new(HashMap::new()),
            subid_to_addr: Mutex::new(HashMap::new()),
            pending_pub: Mutex::new(HashMap::new()),
            evctr: AtomicU64::new(1),
            pending_inbound: Mutex::new(HashSet::new()),
            subctr: AtomicU64::new(1),
        });
        (shared, core_rx, relay_rx)
    }

    #[test]
    fn initial_bind_is_acked_before_req_and_req_waits_for_core_hello() {
        let (shared, mut core_rx, mut relay_rx) = shared_with_links();
        let address = "filter:kind=1";
        handle_core_message(
            &json!({"id": "bind-1", "m": "bind", "address": address}),
            &shared,
        );
        let bind_ack: Value = serde_json::from_str(
            &core_rx
                .try_recv()
                .expect("initial bind is acknowledged before hello completes"),
        )
        .expect("bind ack is JSON");
        assert_eq!(bind_ack, json!({"id": "bind-1", "ok": {}}));
        assert!(relay_rx.try_recv().is_err(), "REQ preceded core hello");

        handle_core_message(
            &json!({"id": HELLO_ID, "err": {"code": "rejected"}}),
            &shared,
        );
        assert!(relay_rx.try_recv().is_err(), "REQ followed rejected hello");

        handle_core_message(&json!({"id": HELLO_ID, "ok": {}}), &shared);
        let frame = relay_rx
            .try_recv()
            .expect("REQ follows accepted core hello");
        let frame: Value =
            serde_json::from_str(&frame.into_text().expect("REQ is text")).expect("REQ is JSON");
        assert_eq!(frame[0], json!("REQ"));
        assert_eq!(frame[2], json!({"kinds": [1]}));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn say_effect_is_published_and_acknowledged() {
        let (shared, mut core_rx, mut relay_rx) = shared_with_links();
        let effect = json!({
            "id": "effect-1",
            "m": "effect",
            "kind": "say",
            "address": "filter:x",
            "payload": {"text": "synthetic reply"}
        });
        let task = tokio::spawn({
            let shared = shared.clone();
            async move { handle_effect(&shared, "effect-1", &effect).await }
        });

        let frame = relay_rx.recv().await.expect("say publishes a relay frame");
        let text = frame.into_text().expect("relay frame is text");
        let frame: Value = serde_json::from_str(&text).expect("relay frame is JSON");
        assert_eq!(frame[0], json!("EVENT"));
        let event_id = frame[1]["id"].as_str().expect("published event id");
        dispatch_relay(
            &shared,
            &json!(["OK", event_id, true, "stored"]).to_string(),
        );
        task.await.expect("effect task completes");

        let response: Value =
            serde_json::from_str(&core_rx.recv().await.expect("effect response reaches core"))
                .expect("effect response is JSON");
        assert_eq!(response["id"], json!("effect-1"));
        assert_eq!(response["ok"]["delivered"], json!(true));
        assert!(response["ok"]["origin"]
            .as_str()
            .is_some_and(|origin| origin.starts_with("note1")));
    }
}
