//! プラグインプロトコル（版 1）の往復と、状態機械の「不正。落とす」升目を、実物で確かめる。
//!
//! テスト用ゲートは **core の型を一切使わない** — 線に載る JSON を仕様書だけを見て手で組む。
//! これで検証しているのは「Rust の型が合うこと」ではなく「プロトコルどおりに喋れること」であり、
//! 同時に「仕様書だけでプラグインが書けるか」も確かめている（タスクの規律）。
//!
//! core 側のハーネス（System・ScriptedEngine・store の観測）は core の型を使ってよい——
//! それはプラグインではなく系の中身だから。線を組むのはゲートだけが JSON でやる。

use opencrab_engine::{CharCounter, ScriptedEngine, ScriptedShellHost, Step};
use opencrab_plugd::Plugd;
use opencrab_port::{
    AttachmentKind, Content, EffectKind, EffectSpec, EventKind, GateName, IngressDiscovery,
    Notifier as _Notifier, OriginScope, Property, Role, SubjectKind, ToolHost, Transport,
};
use opencrab_social_runtime::{Config, Policy, System};
use opencrab_store::Store;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

// ===== core 側ハーネス =====

struct H {
    sys: System,
    eng: ScriptedEngine,
    plugd: Plugd,
}

fn build() -> H {
    build_cfg(Config::default())
}

fn build_cfg(cfg: Config) -> H {
    let store = Store::new_in_memory().unwrap();
    // 本番 app の起動時 seed を再現する。hello は compatibility instance を作らず、
    // ここで設定済みとした v1 gate だけを exact-one で解決する。
    for name in ["web", "nostr", "discord", "other"] {
        store
            .seed_compatibility_instance(&GateName::new(name))
            .unwrap();
    }
    // 会話予算の物差し（§06）。ScriptedEngine の既定モデル "scripted" に context_window を登録する
    // （200_000 × compaction_ratio 0.5 = 100_000・旧固定既定と同値）。未登録だと System::new が fail loud。
    store
        .register_model_context_window("scripted", 200_000)
        .unwrap();
    let eng = ScriptedEngine::new();
    let plugd = Plugd::new();
    let sys = System::new(
        store,
        Arc::new(eng.clone()),
        Arc::new(plugd.clone()) as Arc<dyn ToolHost>,
        Arc::new(ScriptedShellHost::new()),
        Arc::new(plugd.clone()) as Arc<dyn _Notifier>,
        Arc::new(CharCounter),
        cfg,
    );
    plugd.attach_system(sys.clone());
    sys.attach_transport(Arc::new(plugd.clone()) as Arc<dyn Transport>);
    H { sys, eng, plugd }
}

// ===== テスト用ゲート（線を JSON で喋る。core の型を使わない）=====

#[derive(Clone)]
struct GateCfg {
    /// effect の ack で返す外界識別子（§04 の origin）。
    effect_origin: Option<String>,
    delivered: bool,
    tool_result: String,
    open_address: String,
    /// 要求に自動で ok を返すか。落とす升目のテストでは false にして手で扱う。
    auto: bool,
}

impl Default for GateCfg {
    fn default() -> GateCfg {
        GateCfg {
            effect_origin: None,
            delivered: true,
            tool_result: "TOOL_OK".into(),
            open_address: "opened/addr-1".into(),
            auto: true,
        }
    }
}

struct TestGate {
    out: mpsc::UnboundedSender<String>,
    log: Arc<Mutex<Vec<Value>>>,
    disconnected: Arc<AtomicBool>,
    cfg: Arc<Mutex<GateCfg>>,
}

impl TestGate {
    fn connect(plugd: &Plugd) -> TestGate {
        let (a, b) = tokio::io::duplex(4 * 1024 * 1024);
        plugd.serve(a);
        let (rh, mut wh) = tokio::io::split(b);

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        // 書き出しタスク（プラグイン側）。
        tokio::spawn(async move {
            while let Some(line) = out_rx.recv().await {
                if wh.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                let _ = wh.write_all(b"\n").await;
                let _ = wh.flush().await;
            }
        });

        let log = Arc::new(Mutex::new(Vec::<Value>::new()));
        let disconnected = Arc::new(AtomicBool::new(false));
        let cfg = Arc::new(Mutex::new(GateCfg::default()));

        // 読み取り＋自動応答タスク（プラグイン側の runtime を真似る）。
        let log2 = log.clone();
        let disc2 = disconnected.clone();
        let cfg2 = cfg.clone();
        let out2 = out_tx.clone();
        tokio::spawn(async move {
            let mut br = BufReader::new(rh);
            loop {
                let mut line = String::new();
                match br.read_line(&mut line).await {
                    Ok(0) => {
                        disc2.store(true, Ordering::SeqCst); // 切断された（core が結びを解いた）
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        disc2.store(true, Ordering::SeqCst);
                        break;
                    }
                }
                let v: Value = match serde_json::from_str(line.trim_end()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                log2.lock().unwrap().push(v.clone());

                let c = cfg2.lock().unwrap().clone();
                if !c.auto {
                    continue;
                }
                // core → plugin の要求に自動で ok を返す（プロトコル§02/§04/§06）。
                let id = v.get("id").and_then(|x| x.as_str()).map(|s| s.to_string());
                let m = v.get("m").and_then(|x| x.as_str());
                if let (Some(id), Some(m)) = (id, m) {
                    let reply = match m {
                        "bind" | "unbind" => Some(json!({"id": id, "ok": {}})),
                        "open" => Some(json!({"id": id, "ok": {"address": c.open_address}})),
                        "effect" => {
                            let mut ok = serde_json::Map::new();
                            ok.insert("delivered".into(), c.delivered.into());
                            if let Some(o) = &c.effect_origin {
                                ok.insert("origin".into(), o.clone().into());
                            }
                            Some(json!({"id": id, "ok": Value::Object(ok)}))
                        }
                        "tool" => Some(json!({"id": id, "ok": {"result": c.tool_result}})),
                        _ => None,
                    };
                    if let Some(r) = reply {
                        let _ = out2.send(r.to_string());
                    }
                }
            }
        });

        TestGate {
            out: out_tx,
            log,
            disconnected,
            cfg,
        }
    }

    fn set_cfg(&self, f: impl FnOnce(&mut GateCfg)) {
        f(&mut self.cfg.lock().unwrap());
    }

    fn send(&self, v: Value) {
        let _ = self.out.send(v.to_string());
    }
    /// 生の 1 行を送る（too_large の検査など、Value に収めない用途）。
    fn send_raw(&self, s: String) {
        let _ = self.out.send(s);
    }

    fn log(&self) -> Vec<Value> {
        self.log.lock().unwrap().clone()
    }
    fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::SeqCst)
    }

    /// 述語が満たされるまでタスクを進める（時間は進めない）。満たされれば true。
    async fn wait_for(&self, pred: impl Fn(&[Value]) -> bool) -> bool {
        for _ in 0..4000 {
            if pred(&self.log()) {
                return true;
            }
            if self.is_disconnected() {
                // もう線は来ない。最後にもう一度だけ判定。
                return pred(&self.log());
            }
            tokio::task::yield_now().await;
        }
        false
    }

    async fn wait_disconnect(&self) -> bool {
        for _ in 0..4000 {
            if self.is_disconnected() {
                return true;
            }
            tokio::task::yield_now().await;
        }
        false
    }

    /// hello を送って ok が返るまで待つ（多くのテストの前段）。
    async fn hello_ok(
        &self,
        name: &str,
        addr_form: &str,
        effects: Value,
        caps: Value,
        tools: Value,
    ) {
        self.send(json!({
            "id": "h1", "m": "hello", "protocol": 1,
            "name": name, "address_form": addr_form,
            "tools": tools, "effects": effects, "capabilities": caps,
        }));
        let ok = self
            .wait_for(|l| {
                l.iter().any(|v| {
                    v.get("id").and_then(|x| x.as_str()) == Some("h1") && v.get("ok").is_some()
                })
            })
            .await;
        assert!(ok, "hello に ok が返るべき: {:?}", self.log());
    }
}

fn has_reply_ok(log: &[Value], id: &str) -> bool {
    log.iter()
        .any(|v| v.get("id").and_then(|x| x.as_str()) == Some(id) && v.get("ok").is_some())
}
/// その id の ok に載った seq を引く（event の受理応答・プロトコル§03）。
fn ok_seq(log: &[Value], id: &str) -> Option<i64> {
    log.iter()
        .find(|v| v.get("id").and_then(|x| x.as_str()) == Some(id) && v.get("ok").is_some())
        .and_then(|v| v.pointer("/ok/seq").and_then(|x| x.as_i64()))
}
fn err_code(log: &[Value], id: &str) -> Option<String> {
    log.iter()
        .find(|v| v.get("id").and_then(|x| x.as_str()) == Some(id) && v.get("err").is_some())
        .and_then(|v| {
            v.get("err")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str())
        })
        .map(|s| s.to_string())
}
/// core → plugin の要求（m 付き）を種別で探す。
fn find_msg<'a>(log: &'a [Value], m: &str) -> Option<&'a Value> {
    log.iter()
        .find(|v| v.get("m").and_then(|x| x.as_str()) == Some(m))
}
/// read の ok（events/next を含む物体）を id で引く（プロトコル§02）。
fn read_ok(log: &[Value], id: &str) -> Option<Value> {
    log.iter()
        .find(|v| v.get("id").and_then(|x| x.as_str()) == Some(id) && v.get("ok").is_some())
        .and_then(|v| v.get("ok").cloned())
}

// ===== 目標 1: 6 つのメッセージが往復する =====

// hello → ok（プロトコル§01）。名乗りが core にとってのゲートの全部。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn m1_hello_roundtrips() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok(
        "nostr",
        "npub1[a-z0-9]+",
        json!(["say", "react"]),
        json!([]),
        json!([]),
    )
    .await;
    // 名乗りが core に届いている（値として読める）。
    let spec = h
        .sys
        .gate_spec(&GateName::new("nostr"))
        .expect("gate registered");
    assert_eq!(spec.protocol, 1);
    assert!(spec.effects.contains(&EffectKind::React));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn protocol2_requires_ready_and_membership_read_never_hits_store() {
    let h = build();
    let instance =
        opencrab_port::GateInstanceId::parse("018f0000-0000-7000-8000-000000000001".to_string())
            .unwrap();
    let kind = GateName::new("discord");
    h.sys
        .store()
        .install_gate_instance_revision(
            &instance,
            &kind,
            "synthetic",
            None,
            1,
            true,
            OriginScope::KindAddress,
            IngressDiscovery::Membership,
            "gate-config/discord/v1",
            &[],
            1,
        )
        .unwrap();
    let g = TestGate::connect(&h.plugd);
    g.send(json!({
        "id":"h2","m":"hello","protocol":2,"kind_id":"discord",
        "instance_id":instance.as_str(),"revision":1,"origin_scope":"kind_address",
        "address_form":".+","ingress_discovery":"membership",
        "tools":[],"effects":["say"],"capabilities":[],"actions":[]
    }));
    assert!(g.wait_for(|log| has_reply_ok(log, "h2")).await);
    let epoch = g
        .log()
        .iter()
        .find(|value| value.get("id").and_then(Value::as_str) == Some("h2"))
        .and_then(|value| value.pointer("/ok/connection_epoch"))
        .and_then(Value::as_u64)
        .unwrap();

    g.send(json!({
        "id":"early","m":"event","kind":"said","address":"channel-a",
        "author":{"id":"principal-a"},"content":{"text":"hello"},"origin":"message-a",
        "discovery":{"address_kind":"guild","guild_id":"guild-a"}
    }));
    assert!(
        g.wait_for(|log| err_code(log, "early").as_deref() == Some("instance_not_ready"))
            .await
    );

    g.send(json!({"id":"ready","m":"ready","connection_epoch":epoch}));
    assert!(g.wait_for(|log| has_reply_ok(log, "ready")).await);
    g.send(json!({"id":"read2","m":"read","address":"channel-a"}));
    assert!(
        g.wait_for(|log| err_code(log, "read2").as_deref() == Some("membership_read_unsupported"))
            .await
    );

    g.send(json!({
        "id":"observed","m":"event","kind":"said","address":"channel-a",
        "author":{"id":"principal-a"},"content":{"text":"hello"},"origin":"message-a",
        "discovery":{"address_kind":"guild","guild_id":"guild-a","label":"synthetic"}
    }));
    assert!(
        g.wait_for(|log| {
            log.iter().any(|value| {
                value.get("id").and_then(Value::as_str) == Some("observed")
                    && value.pointer("/ok/seq").is_some_and(Value::is_null)
            })
        })
        .await
    );
}

// bind → ok（プロトコル§02）。場を作るのは core。プラグインは購読を頼まれる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn m2_bind_roundtrips() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!(["say"]), json!([]), json!([]))
        .await;
    let place = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);

    h.sys
        .bind_place(place, "nostr", "filter:kind=1")
        .await
        .expect("bind ok");
    // ゲートは bind 要求を受け取った。
    assert!(find_msg(&g.log(), "bind").is_some(), "bind 要求が届く");
    // チャネルが記録された（住所 → 場が引ける）。
    assert_eq!(
        h.sys
            .store()
            .place_for_channel(&GateName::new("nostr"), "filter:kind=1")
            .unwrap(),
        Some(place)
    );
}

// event → ok{seq}（プロトコル§03）。外で起きたことを送る。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn m3_event_roundtrips() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!(["say"]), json!([]), json!([]))
        .await;
    let place = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);
    h.sys.bind_place(place, "nostr", "filter:x").await.unwrap();

    g.send(json!({
        "id": "e1", "m": "event", "kind": "said",
        "address": "filter:x",
        "author": {"id": "npubABC", "display": "test-owner"},
        "content": {"text": "エージェントA、これ見た？"},
    }));
    assert!(
        g.wait_for(|l| has_reply_ok(l, "e1")).await,
        "event に ok が返る: {:?}",
        g.log()
    );
    // ログに載っている。
    assert_eq!(h.sys.store().latest_seq(place).unwrap(), 1);
    let ev = h.sys.store().get_event(place, 1).unwrap().unwrap();
    assert_eq!(
        ev.content.text.as_deref(),
        Some("エージェントA、これ見た？")
    );
    // 添付なしの出来事は attachments が空（後方互換・DESIGN-images §1）。
    assert!(ev.attachments.is_empty(), "添付なしは空で往復する");
}

// 添付つき event が線 → GateEvent → store まで往復する（DESIGN-images §1・§7）。kind/url と由来作者を
// そのまま持ち越す。attachments なしとの後方互換は上の m3 が担う。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn m3_event_with_attachments_roundtrips() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!(["say"]), json!([]), json!([]))
        .await;
    let place = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);
    h.sys.bind_place(place, "nostr", "filter:x").await.unwrap();

    g.send(json!({
        "id": "e1", "m": "event", "kind": "said",
        "address": "filter:x",
        "author": {"id": "npubABC", "display": "test-owner"},
        "content": {"text": "見て"},
        "attachments": [
            {"kind": "image", "url": "https://ex/a.png", "origin_author": "npubABC"},
            {"kind": "image", "url": "https://ex/b.jpg"}
        ],
    }));
    assert!(
        g.wait_for(|l| has_reply_ok(l, "e1")).await,
        "添付つき event に ok が返る: {:?}",
        g.log()
    );
    let ev = h.sys.store().get_event(place, 1).unwrap().unwrap();
    assert_eq!(ev.attachments.len(), 2, "2 つの添付が往復する");
    assert_eq!(ev.attachments[0].kind, AttachmentKind::Image);
    assert_eq!(ev.attachments[0].url, "https://ex/a.png");
    assert_eq!(
        ev.attachments[0].origin_author.as_deref(),
        Some("npubABC"),
        "由来作者を持ち越す（§5）"
    );
    // origin_author を送らない添付は None（由来不明＝未信頼扱いの入口・§5）。
    assert_eq!(ev.attachments[1].origin_author, None);
}

// 未知の添付 kind は err（近い型に寄せない・§00）。ゲートが正しい形で載せる前提を線で守る。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn event_with_unknown_attachment_kind_is_rejected() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!(["say"]), json!([]), json!([]))
        .await;
    let place = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);
    h.sys.bind_place(place, "nostr", "filter:x").await.unwrap();

    g.send(json!({
        "id": "e1", "m": "event", "kind": "said", "address": "filter:x",
        "author": {"id": "u"}, "content": {"text": "x"},
        "attachments": [{"kind": "video", "url": "https://ex/v.mp4"}],
    }));
    assert!(
        g.wait_for(|l| err_code(l, "e1").is_some()).await,
        "未知の添付 kind は err: {:?}",
        g.log()
    );
    assert_eq!(err_code(&g.log(), "e1").as_deref(), Some("unknown_enum"));
    assert_eq!(h.sys.store().latest_seq(place).unwrap(), 0, "書かれない");
}

// 同じ外界識別子が二度届いたら core で畳む（詳細§04）。二重に書かず、既にある連番を返す。数える（§10）。
// 識別子を持たないものは畳めないので、そのまま追記する。ゲート側では絞らない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn duplicate_external_event_folds_to_existing_seq() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!(["say"]), json!([]), json!([]))
        .await;
    let place = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);
    h.sys.bind_place(place, "nostr", "filter:x").await.unwrap();

    // 同じ origin の出来事を 2 度送る（繋ぎ直し・向こうが保存分を先に返した、を模す）。
    let dup = |id: &str| {
        json!({
            "id": id, "m": "event", "kind": "said", "address": "filter:x",
            "author": {"id": "npubABC"},
            "content": {"text": "同じ note"},
            "origin": "note1SAME",
        })
    };
    g.send(dup("d1"));
    assert!(g.wait_for(|l| has_reply_ok(l, "d1")).await, "1 度目に ok");
    g.send(dup("d2"));
    assert!(
        g.wait_for(|l| has_reply_ok(l, "d2")).await,
        "2 度目にも ok（断らない）"
    );

    // 二重に書かない：ログは 1 件のまま。
    assert_eq!(
        h.sys.store().latest_seq(place).unwrap(),
        1,
        "二重に書かない"
    );
    // 両方の ok が同じ本物の連番を返す。
    let log = g.log();
    assert_eq!(ok_seq(&log, "d1"), Some(1));
    assert_eq!(ok_seq(&log, "d2"), Some(1), "同じものには同じ連番");
    // 数えている（§10）——このゲートから 1 件の重複。
    assert_eq!(
        h.sys.store().dedup_count(&GateName::new("nostr")).unwrap(),
        1
    );

    // 識別子を持たない出来事は畳めない——毎回そのまま追記される（§04）。
    let noid = |id: &str| {
        json!({
            "id": id, "m": "event", "kind": "said", "address": "filter:x",
            "author": {"id": "x"}, "content": {"text": "no-origin"},
        })
    };
    g.send(noid("n1"));
    assert!(g.wait_for(|l| has_reply_ok(l, "n1")).await);
    g.send(noid("n2"));
    assert!(g.wait_for(|l| has_reply_ok(l, "n2")).await);
    assert_eq!(
        h.sys.store().latest_seq(place).unwrap(),
        3,
        "識別子なしはそのまま追記（畳めない）"
    );
    // 重複件数は増えていない（識別子なしは重複判定の対象外）。
    assert_eq!(
        h.sys.store().dedup_count(&GateName::new("nostr")).unwrap(),
        1
    );
}

// effect → ack、activity（開始・終了）、tool → result。1 つのターンで 4 種が往復する。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn m4_effect_activity_tool_roundtrip() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.set_cfg(|c| c.effect_origin = Some("note1OUT".into()));
    // ゲートは say を運び、ツール nostr-gen を名乗る。
    g.hello_ok(
        "nostr",
        "filter:.+",
        json!(["say"]),
        json!([]),
        json!([{"name":"nostr-gen","description":"作る","params":{"type":"object","properties":{},"required":[]}}]),
    )
    .await;

    let a = h.sys.create_subject(
        SubjectKind::Agent,
        "A",
        "A",
        opencrab_port::Standing::Trusted,
    );
    let place = h.sys.create_place(
        Some("p"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(place, a, Role::Participant);
    h.sys.bind_place(place, "nostr", "filter:x").await.unwrap();

    // ターン: ツールを呼び（受理を見て）発話して終える。常時切り離しでツールは背景へ移るので、
    // その決着から起きる 3 本目のターンぶんの台本も要る。
    h.eng.push(Step::cont().with_tool("nostr-gen"));
    h.eng.push(Step::say_done("見たよ"));
    h.eng.push(Step::no_reply()); // 決着から起きるターン（常時切り離し）

    // 外から出来事 → ターンが起きる。
    g.send(json!({
        "id":"e1","m":"event","kind":"said","address":"filter:x",
        "author":{"id":"npubABC"},"content":{"text":"ねえ"}
    }));

    // 6 つ目まで往復する: tool 要求・effect 要求・activity 開始/終了。
    assert!(
        g.wait_for(|l| find_msg(l, "tool").is_some()).await,
        "tool 要求が届く"
    );
    assert!(
        g.wait_for(|l| find_msg(l, "effect").is_some()).await,
        "effect 要求が届く"
    );
    assert!(
        g.wait_for(|l| l
            .iter()
            .any(|v| v.get("m").and_then(|x| x.as_str()) == Some("activity")
                && v.get("state").and_then(|x| x.as_str()) == Some("started")))
            .await,
        "activity started が届く"
    );
    assert!(
        g.wait_for(|l| l
            .iter()
            .any(|v| v.get("m").and_then(|x| x.as_str()) == Some("activity")
                && v.get("state").and_then(|x| x.as_str()) == Some("ended")))
            .await,
        "activity ended が届く"
    );

    // effect の中身（発話本文）と、活動の住所が線に載っている。
    let log = g.log();
    let eff = find_msg(&log, "effect").unwrap();
    assert_eq!(eff.get("kind").and_then(|x| x.as_str()), Some("say"));
    assert_eq!(
        eff.pointer("/payload/text").and_then(|x| x.as_str()),
        Some("見たよ")
    );
    // 常時切り離し（§07）: ツール呼び出しは即座に「受理（活動ID）」で会話へ戻り、実結果は決着で戻る。
    // 同じターンの history には受理だけが載る。
    let hists = h.eng.histories();
    assert!(
        hists.len() >= 2 && hists[1].contains("活動"),
        "受理（活動ID）が呼び出しと対で会話に入る: {hists:?}"
    );
    // 実結果（TOOL_OK）は決着イベントとして戻る（activity ended は上で待った＝決着済み）。
    let latest = h.sys.store().latest_seq(place).unwrap();
    let rows = h.sys.store().read_range(place, 0, latest).unwrap();
    let settled = rows
        .iter()
        .filter(|e| e.kind == EventKind::Settled)
        .filter_map(|e| e.content.text.clone())
        .find(|t| t.contains("TOOL_OK"));
    assert!(settled.is_some(), "ツール結果が決着で戻る: {rows:?}");
}

// PROGRESS（2 つ目の core 共通語・進捗の揮発表示）が線に載る。エージェントが `PROGRESS::<文>` を返すと、
// activity progress 通知（state:"progress"・label 付き・住所付き）が結ばれたゲートへ揮発配送される
// （§05）。say でもイベントでもないので、場のログには spoke が増えない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn progress_word_reaches_the_wire_as_activity_progress() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!(["say"]), json!([]), json!([]))
        .await;

    let a = h.sys.create_subject(
        SubjectKind::Agent,
        "A",
        "A",
        opencrab_port::Standing::Trusted,
    );
    let place = h.sys.create_place(
        Some("p"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(place, a, Role::Participant);
    h.sys.bind_place(place, "nostr", "filter:x").await.unwrap();

    // エージェントは PROGRESS 1 行だけを返す（say も明示アクションも無い）。
    h.eng
        .push(Step::say_done("PROGRESS::いま 3 件目を読んでいます"));

    g.send(json!({
        "id":"e1","m":"event","kind":"said","address":"filter:x",
        "author":{"id":"npubABC"},"content":{"text":"調子どう？"}
    }));

    // activity progress が label 付きで線に載る。
    assert!(
        g.wait_for(|l| l.iter().any(|v| {
            v.get("m").and_then(|x| x.as_str()) == Some("activity")
                && v.get("state").and_then(|x| x.as_str()) == Some("progress")
                && v.get("label").and_then(|x| x.as_str()) == Some("いま 3 件目を読んでいます")
        }))
        .await,
        "activity progress が label 付きで届く: {:?}",
        g.log()
    );

    // 住所が添う（§05）——結んだ場の住所。
    let log = g.log();
    let prog = log
        .iter()
        .find(|v| {
            v.get("m").and_then(|x| x.as_str()) == Some("activity")
                && v.get("state").and_then(|x| x.as_str()) == Some("progress")
        })
        .unwrap();
    assert_eq!(
        prog.get("address").and_then(|x| x.as_str()),
        Some("filter:x"),
        "進捗通知に住所が添う"
    );

    // PROGRESS は say でもイベントでもない——場のログに spoke（エージェント発話）は増えない。
    let latest = h.sys.store().latest_seq(place).unwrap();
    let rows = h.sys.store().read_range(place, 0, latest).unwrap();
    assert!(
        rows.iter().all(|e| e.kind != EventKind::Spoke),
        "PROGRESS は spoke を生まない: {rows:?}"
    );
}

// open → 住所を返す → core が新しい場を結ぶ（プロトコル§02）。capability を持つゲートだけ。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn open_creates_container_and_binds_place() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.set_cfg(|c| c.open_address = "1234567890/thread-99".into());
    // capabilities に open を名乗る。
    g.hello_ok("discord", ".+", json!(["say"]), json!(["open"]), json!([]))
        .await;
    let parent = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);

    let child = h
        .sys
        .open_container(
            parent,
            "discord",
            "1234567890",
            Some("設計レビュー"),
            &Policy::default(),
        )
        .await
        .expect("open ok");
    // ゲートは open 要求を受け取り、hint も載っている。
    let log = g.log();
    let open = find_msg(&log, "open").unwrap();
    assert_eq!(
        open.get("under").and_then(|x| x.as_str()),
        Some("1234567890")
    );
    assert_eq!(
        open.get("hint").and_then(|x| x.as_str()),
        Some("設計レビュー")
    );
    // 返ってきた住所に新しい場が結ばれた（§02）。
    assert_eq!(
        h.sys
            .store()
            .place_for_channel(&GateName::new("discord"), "1234567890/thread-99")
            .unwrap(),
        Some(child)
    );
}

// open を名乗らないゲートには open を送らない（capabilities は値・§02）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn open_refused_without_capability() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("web", "web:.+", json!(["say"]), json!([]), json!([]))
        .await;
    let parent = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);
    let r = h
        .sys
        .open_container(parent, "web", "x", None, &Policy::default())
        .await;
    assert!(r.is_err(), "open を名乗らないゲートには送らない");
    assert!(find_msg(&g.log(), "open").is_none(), "open 要求は届かない");
}

// ===== 目標 5: 外界の識別子が往復する =====

// 届いた出来事の origin で宛先が解決でき、出した効果の ack の origin が保持され、
// 自分の投稿に後から反応できる（プロトコル§03/§04・詳細§08）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn external_ids_round_trip_and_react_to_own_post() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.set_cfg(|c| c.effect_origin = Some("note1SELF".into())); // 自分の発話の外界識別子
    g.hello_ok(
        "nostr",
        "filter:.+",
        json!(["say", "react"]),
        json!([]),
        json!([]),
    )
    .await;

    let a = h.sys.create_subject(
        SubjectKind::Agent,
        "A",
        "A",
        opencrab_port::Standing::Trusted,
    );
    let place = h.sys.create_place(
        Some("p"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(place, a, Role::Participant);
    h.sys.bind_place(place, "nostr", "filter:x").await.unwrap();

    // ターン1: 届いた投稿(origin=note1IN)へ返信する（宛先= inbound の seq）。
    // ターン2: 自分の発話(seq2)へ反応する。
    let say_reply = EffectSpec {
        kind: EffectKind::Say,
        place: None,
        target: Some(1), // inbound event の seq
        content: Content::text("見たよ"),
        mentions: vec![],
        verb: None,
    };
    h.eng.push(Step::done().with_effect(say_reply));
    let react_own = EffectSpec {
        kind: EffectKind::React,
        place: None,
        target: Some(2), // 自分の say の seq
        content: Content {
            text: None,
            symbol: Some("👍".into()),
        },
        mentions: vec![],
        verb: None,
    };
    h.eng.push(Step::done().with_effect(react_own));

    // 1) 届いた出来事（origin あり）。
    g.send(json!({
        "id":"e1","m":"event","kind":"said","address":"filter:x",
        "author":{"id":"npubABC"},"content":{"text":"ねえ"},"origin":"note1IN"
    }));
    // ターン1の say が配送される。宛先は inbound の外界識別子に解決される。
    assert!(
        g.wait_for(|l| find_msg(l, "effect").is_some()).await,
        "say が配送される"
    );
    let say = find_msg(&g.log(), "effect").unwrap().clone();
    assert_eq!(say.get("kind").and_then(|x| x.as_str()), Some("say"));
    assert_eq!(
        say.get("target").and_then(|x| x.as_str()),
        Some("note1IN"),
        "返信の宛先は届いた出来事の外界識別子に解決される（§03）"
    );
    // ack の origin(note1SELF) が out として保持される。
    assert!(
        g.wait_for(|_| h
            .sys
            .store()
            .external_ref_of(place, 2)
            .unwrap()
            .map(|(_, e)| e == "note1SELF")
            .unwrap_or(false))
            .await,
        "自分の発話の外界識別子が保持される（§08）"
    );

    // 2) 別の出来事でターン2を起こす → 自分の投稿(seq2)へ反応。
    g.send(json!({
        "id":"e2","m":"event","kind":"said","address":"filter:x",
        "author":{"id":"npubXYZ"},"content":{"text":"どう？"},"origin":"note1IN2"
    }));
    assert!(
        g.wait_for(|l| l
            .iter()
            .filter(|v| v.get("m").and_then(|x| x.as_str()) == Some("effect")
                && v.get("kind").and_then(|x| x.as_str()) == Some("react"))
            .count()
            >= 1)
            .await,
        "react が配送される: {:?}",
        g.log()
    );
    let react = g
        .log()
        .into_iter()
        .find(|v| v.get("kind").and_then(|x| x.as_str()) == Some("react"))
        .unwrap();
    assert_eq!(
        react.get("target").and_then(|x| x.as_str()),
        Some("note1SELF"),
        "自分の投稿に、保持した外界識別子で後から反応できる（§08）"
    );
    assert_eq!(
        react.pointer("/payload/symbol").and_then(|x| x.as_str()),
        Some("👍")
    );
}

// web の形（say/react を運び、event に origin を送り、say の ack で origin を返す）のゲートで、
// **宛先つきの効果（返信・反応）が使えるようになったこと**を線の往復で確かめる（タスク目標1）。
// web-gate の実物は別クレートの bin だが、ここではその線の振る舞い（§03/§04 の origin の授受）を
// 手組みの JSON で真似て、core 側の解決（返信先＝inbound の origin、反応先＝自分の発話の origin）を確かめる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn web_shaped_gate_enables_targeted_effects() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    // web が自分の発話へ振る origin（say の ack で返す・§04）。
    g.set_cfg(|c| c.effect_origin = Some("web-out-2".into()));
    // web は say と react を運ぶ（§01）。住所は room 形式。
    g.hello_ok(
        "web",
        "room:[a-z0-9-]+",
        json!(["say", "react"]),
        json!([]),
        json!([]),
    )
    .await;

    let a = h.sys.create_subject(
        SubjectKind::Agent,
        "web-agent",
        "web-agent",
        opencrab_port::Standing::Trusted,
    );
    let place = h.sys.create_place(
        Some("room:main"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(place, a, Role::Participant);
    h.sys.bind_place(place, "web", "room:main").await.unwrap();

    // ターン1: 届いた人の発話(inbound の seq=1)へ**返信**する（say + 宛先）。
    let say_reply = EffectSpec {
        kind: EffectKind::Say,
        place: None,
        target: Some(1),
        content: Content::text("受け取りました"),
        mentions: vec![],
        verb: None,
    };
    h.eng.push(Step::done().with_effect(say_reply));
    // ターン2: 自分の発話(seq=2)へ**反応**する（react + 宛先）。
    let react_own = EffectSpec {
        kind: EffectKind::React,
        place: None,
        target: Some(2),
        content: Content {
            text: None,
            symbol: Some("👍".into()),
        },
        mentions: vec![],
        verb: None,
    };
    h.eng.push(Step::done().with_effect(react_own));

    // 1) 人が発話する。web は origin を添えて送る（§03）——これが無いと返信も反応もできない。
    g.send(json!({
        "id":"e1","m":"event","kind":"said","address":"room:main",
        "author":{"id":"test-owner","display":"test-owner"},"content":{"text":"これ見た？"},
        "origin":"web-in-1"
    }));
    // 返信(say)が web-in-1 を宛先に配送される（返信先＝届いた発話の外界識別子・§03/§08）。
    assert!(
        g.wait_for(|l| l
            .iter()
            .any(|v| v.get("m").and_then(|x| x.as_str()) == Some("effect")
                && v.get("kind").and_then(|x| x.as_str()) == Some("say")))
            .await,
        "返信(say)が配送される: {:?}",
        g.log()
    );
    let say = g
        .log()
        .into_iter()
        .find(|v| v.get("kind").and_then(|x| x.as_str()) == Some("say"))
        .unwrap();
    assert_eq!(
        say.get("target").and_then(|x| x.as_str()),
        Some("web-in-1"),
        "返信の宛先は人の発話の外界識別子に解決される（web が origin を送ったから・§03）"
    );

    // 2) 別の発話でターン2を起こす → 自分の発話(seq2)へ反応。
    g.send(json!({
        "id":"e2","m":"event","kind":"said","address":"room:main",
        "author":{"id":"second-user"},"content":{"text":"どう？"},"origin":"web-in-2"
    }));
    assert!(
        g.wait_for(|l| l
            .iter()
            .any(|v| v.get("m").and_then(|x| x.as_str()) == Some("effect")
                && v.get("kind").and_then(|x| x.as_str()) == Some("react")))
            .await,
        "反応(react)が配送される: {:?}",
        g.log()
    );
    let react = g
        .log()
        .into_iter()
        .find(|v| v.get("kind").and_then(|x| x.as_str()) == Some("react"))
        .unwrap();
    assert_eq!(
        react.get("target").and_then(|x| x.as_str()),
        Some("web-out-2"),
        "自分の発話へ、say の ack で返した外界識別子で反応できる（自分の発話を後から指せる・§04/§08）"
    );
    assert_eq!(
        react.pointer("/payload/symbol").and_then(|x| x.as_str()),
        Some("👍")
    );
}

// ===== 目標 4: ゲートの違いが値で注入される（可能な効果 = チャネルの名乗りの和）=====

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn place_effects_are_the_union_of_bound_channels() {
    let h = build();
    let a = h.sys.create_subject(
        SubjectKind::Agent,
        "A",
        "A",
        opencrab_port::Standing::Trusted,
    );

    // ゲート1: say/react/boost を運ぶ。
    let g1 = TestGate::connect(&h.plugd);
    g1.hello_ok(
        "nostr",
        "filter:.+",
        json!(["say", "react", "boost"]),
        json!([]),
        json!([]),
    )
    .await;
    // ゲート2: say だけ。
    let g2 = TestGate::connect(&h.plugd);
    g2.hello_ok("web", "web:.+", json!(["say"]), json!([]), json!([]))
        .await;

    // 結ぶ前: Say だけ（intrinsic）。
    let p = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);
    h.sys.join(p, a, Role::Participant);
    // 「可能な効果の和」は carriable_effects（宛先の有無に依らないチャネルの能力）で見る。
    let before = h.sys.carriable_effects(p);
    assert!(before.contains(&EffectKind::Say));
    assert!(
        !before.contains(&EffectKind::React),
        "結ぶ前は react を運べない"
    );

    // nostr を結ぶ → 可能な効果に react/boost が入る（名乗りが値として注入される・§02）。
    h.sys.bind_place(p, "nostr", "filter:x").await.unwrap();
    let after = h.sys.carriable_effects(p);
    assert!(
        after.contains(&EffectKind::React),
        "結んだチャネルの効果が和に入る"
    );
    assert!(after.contains(&EffectKind::Boost));
    let _ = a; // 権限つきの提示は別テスト（visible_effects）で見る

    // web だけを結んだ別の場は react を運べない（ゲートの違いが分岐でなく値で出る）。
    let p2 = h
        .sys
        .create_place(Some("p2"), None, &Policy::default(), None);
    h.sys.join(p2, a, Role::Participant);
    h.sys.bind_place(p2, "web", "web:room1").await.unwrap();
    let v2 = h.sys.carriable_effects(p2);
    assert!(v2.contains(&EffectKind::Say));
    assert!(
        !v2.contains(&EffectKind::React),
        "web は react を名乗っていない"
    );

    // 切断すると可能な効果から外れる（その場は外への出入りを失う・§08）。
    drop(g1);
    // 接続タスクは drop では切れない（duplex は out タスク側が保持）。unregister を直接確かめる。
    h.sys.unregister_gate(&GateName::new("nostr"));
    let after_disc = h.sys.carriable_effects(p);
    assert!(
        !after_disc.contains(&EffectKind::React),
        "名乗りが消えたら和から外れる"
    );
}

// 目標 5（提示）: 宛先にできる出来事だけを提示する（詳細§08・§15）。
// 反応など宛先を要する効果は、外界識別子つきの出来事が場に無ければ選択肢に出ない。
// 届いた出来事（origin つき）が 1 つ載ると、初めて提示される。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn targeted_effects_hidden_until_a_target_exists() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok(
        "nostr",
        "filter:.+",
        json!(["say", "react"]),
        json!([]),
        json!([]),
    )
    .await;
    let a = h.sys.create_subject(
        SubjectKind::Agent,
        "A",
        "A",
        opencrab_port::Standing::Trusted,
    );
    let p = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);
    h.sys.join(p, a, Role::Participant);
    h.sys.bind_place(p, "nostr", "filter:x").await.unwrap();

    // 宛先にできる出来事がまだ無い → react は提示されない（say は宛先不要なので出る）。
    let vis0 = h.sys.visible_effects(p, a).unwrap();
    assert!(vis0.contains(&EffectKind::Say));
    assert!(
        !vis0.contains(&EffectKind::React),
        "宛先が無いうちは react を提示しない（§08）"
    );

    // origin つきの出来事が 1 つ届く。
    g.send(json!({
        "id":"e1","m":"event","kind":"said","address":"filter:x",
        "author":{"id":"u"},"content":{"text":"hi"},"origin":"note1IN"
    }));
    assert!(g.wait_for(|l| has_reply_ok(l, "e1")).await);

    // これで宛先にできる出来事があるので react が提示される。
    let vis1 = h.sys.visible_effects(p, a).unwrap();
    assert!(
        vis1.contains(&EffectKind::React),
        "宛先にできる出来事ができたら提示する: {vis1:?}"
    );
}

/// 述語が満たされるまでタスクを進める（時間は進めない）。TestGate に紐づかない状態（store・engine）用。
async fn settle(mut pred: impl FnMut() -> bool) -> bool {
    for _ in 0..8000 {
        if pred() {
            return true;
        }
        tokio::task::yield_now().await;
    }
    pred()
}

// ===== 機構 7（ツール索引と展開・システム設計§10）: ゲート横断のツール共有 =====
//
// web の場にいるエージェントが、選択済み Nostr tool route を **索引 → 展開 → 実行** できる。
//   - 自分の場に繋がっているゲートのツールは全部見える（ここでは web には道具が無い）。
//   - それ以外のゲート（nostr）のツールは索引に 1 行だけ（core-expand-tools の説明・enum は名簿由来）。
//   - 展開すると次のターンから本体として見え、呼ぶと nostr ゲートへ tool 要求が届く。
//   - 展開に権限上の意味は無い（権限は参加者の権限で掛かる）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cross_gate_tool_index_expand_and_use() {
    let h = build();
    // web ゲート（結ぶ）: 道具は無い。
    let web = TestGate::connect(&h.plugd);
    web.hello_ok("web", "web:.+", json!(["say"]), json!([]), json!([]))
        .await;
    // nostr ゲート: nostr-whoami を名乗る。tool の結果は npub を模す。
    let nostr = TestGate::connect(&h.plugd);
    nostr.set_cfg(|c| c.tool_result = "npub1demo".into());
    nostr
        .hello_ok(
            "nostr",
            "filter:.+",
            json!(["say"]),
            json!([]),
            json!([{"name":"nostr-whoami","description":"npub を返す","params":{"type":"object","properties":{},"required":[]}}]),
        )
        .await;

    let a = h.sys.create_subject(
        SubjectKind::Agent,
        "A",
        "A",
        opencrab_port::Standing::Trusted,
    );
    let p = h.sys.create_place(
        Some("web:room1"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.bind_place(p, "web", "web:room1").await.unwrap();
    let (_, nostr_binding) = h
        .sys
        .store()
        .ensure_compatibility_binding(p, &GateName::new("nostr"), "filter:room1")
        .unwrap();
    h.sys
        .store()
        .set_subject_route(
            a,
            p,
            &GateName::new("nostr"),
            &opencrab_port::RoutePurpose::tool("nostr-whoami").unwrap(),
            &nostr_binding,
        )
        .unwrap();

    // 展開前: nostr-whoami は本体としては見えない。索引（core-expand-tools）に nostr が 1 行。
    let names0: Vec<String> = h
        .sys
        .advertised_tools(p, a)
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(
        !names0.contains(&"nostr-whoami".to_string()),
        "展開前は本体が見えない: {names0:?}"
    );
    assert!(
        names0.contains(&"core-expand-tools".to_string()),
        "索引（展開ツール）が見える: {names0:?}"
    );
    let expand0 = h
        .sys
        .advertised_tools(p, a)
        .unwrap()
        .into_iter()
        .find(|t| t.name == "core-expand-tools")
        .unwrap();
    assert!(
        expand0.description.contains("nostr"),
        "索引に nostr の行が載る: {}",
        expand0.description
    );
    // 展開候補（enum）は名簿由来——nostr が候補（ゲート名を書いた列挙は無い・§10）。
    assert_eq!(
        expand0.params.pointer("/properties/gate/enum"),
        Some(&json!(["nostr"]))
    );

    // 展開する: ターンで core-expand-tools{gate:"nostr"} を呼び、終える。
    h.eng
        .push(Step::cont().with_tool_args("core-expand-tools", json!({"gate":"nostr"})));
    h.eng.push(Step::no_reply());
    web.send(json!({
        "id":"e1","m":"event","kind":"said","address":"web:room1",
        "author":{"id":"u1"},"content":{"text":"self"}
    }));
    // 展開が記録されるまで（ターンが回る）。
    assert!(
        settle(|| h
            .sys
            .store()
            .expanded_gates(p, a)
            .unwrap()
            .contains(&GateName::new("nostr")))
        .await,
        "展開が記録される"
    );

    // 展開後: nostr-whoami が本体として見える（次のターンから）。索引はもう nostr を含まない。
    let names1: Vec<String> = h
        .sys
        .advertised_tools(p, a)
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(
        names1.contains(&"nostr-whoami".to_string()),
        "展開後は本体が見える: {names1:?}"
    );
    assert!(
        !names1.contains(&"core-expand-tools".to_string()),
        "展開しきったら索引は消える（他に未展開ゲートが無い）: {names1:?}"
    );

    // 使う: 次のターンで nostr-whoami を呼ぶ → 選択済み instance へ tool 要求が届く。
    // 常時切り離しでツールは背景へ移るので、その決着から起きるターンぶんの台本も要る。
    h.eng.push(Step::cont().with_tool("nostr-whoami"));
    h.eng.push(Step::no_reply());
    h.eng.push(Step::no_reply()); // 決着から起きるターン（常時切り離し）
    web.send(json!({
        "id":"e2","m":"event","kind":"said","address":"web:room1",
        "author":{"id":"u2"},"content":{"text":"whoami?"}
    }));
    assert!(
        nostr.wait_for(|l| find_msg(l, "tool").is_some()).await,
        "web の場から nostr ゲートへ tool 要求が届く: {:?}",
        nostr.log()
    );
    // 常時切り離し（§07）: nostr のツール結果は**決着イベント**として会話へ戻る（history には受理だけ）。
    assert!(
        settle(|| {
            let latest = h.sys.store().latest_seq(p).unwrap();
            h.sys
                .store()
                .read_range(p, 0, latest)
                .unwrap()
                .iter()
                .filter(|e| e.kind == EventKind::Settled)
                .filter_map(|e| e.content.text.clone())
                .any(|t| t.contains("npub1demo"))
        })
        .await,
        "nostr のツール結果が決着で戻る"
    );
}

// ===== 目標 6: ゲートの名前で core が分岐しないことを、既知の名前リストに依存せず守る =====
//
// 主の守りは **型**（`GateName` が `PartialEq<str>` を実装しない）。`if gate == "mastodon"` は
// 名前が何であれコンパイルできない——`port` の GateName の `compile_fail` doctest がそれを示す
// （まだ見ぬ 4 つ目のゲート名でも同じく止まる）。既知の 4 語を探す固定リスト検査は、
// 「`if gate == "mastodon"` が黙って通る」ため廃止した。
//
// ここでは型の抜け道（`as_str()` で比べる／リテラルから GateName を作って比べる）が core に
// 現れていないことを、**名前に依存しない形**で機械検査する（防御の重ね掛け）。
#[test]
fn core_has_no_gate_name_branch_idioms() {
    let core_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("social-runtime")
        .join("src");
    // 名前に依存しない禁止パターン:
    //   `.as_str() == "`   … ゲート名を借用して文字列リテラルと比較する（型の抜け道）
    //   `GateName::new("`  … リテラルから GateName を作る（作って比べる抜け道の起点）
    // どちらも core では現れないはず（構築は境界の引数（変数）から行う）。ゲート名に依らないので、
    // 4 つ目・5 つ目のゲートを名指しする分岐が入っても同じく捕まえる。
    let forbidden = [".as_str() == \"", "GateName::new(\""];
    let mut checked = 0;
    let mut uses_gatename = false;
    for entry in std::fs::read_dir(&core_src).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        if text.contains("GateName") {
            uses_gatename = true;
        }
        for pat in forbidden {
            assert!(
                !text.contains(pat),
                "core のソース {} にゲート名分岐の抜け道 `{}` が現れてはならない",
                path.display(),
                pat
            );
        }
        checked += 1;
    }
    assert!(checked > 0, "core/src を実際に読んでいること");
    assert!(
        uses_gatename,
        "GateName 型が core で使われていること（型で守る前提）"
    );
}

// ===== read（プロトコル§02）: 結んだ場のログを読む =====

// 結んでいない住所を読もうとすると not_bound（プロトコル§02）。切断はしない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn read_unbound_address_errs_not_bound() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("web", "room:.+", json!(["say"]), json!([]), json!([]))
        .await;
    // 結んでいない住所へ read。
    g.send(json!({"id":"r1","m":"read","address":"room:UNBOUND","from":1}));
    assert!(
        g.wait_for(|l| err_code(l, "r1").as_deref() == Some("not_bound"))
            .await,
        "結んでいない住所は not_bound: {:?}",
        g.log()
    );
    assert!(!g.is_disconnected(), "not_bound は切断ではない");
}

// 読めるのは場で起きたこと全部（種別を絞らない）。人の発話(said)もエージェントの発話(spoke)も
// 同じ 1 つの会話として返り、著者・本文・連番が線に載る（プロトコル§02）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn read_returns_the_whole_conversation() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("web", "room:.+", json!(["say"]), json!([]), json!([]))
        .await;
    let a = h.sys.create_subject(
        SubjectKind::Agent,
        "エージェントA",
        "エージェントA",
        opencrab_port::Standing::Trusted,
    );
    let place = h.sys.create_place(
        Some("room:main"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(place, a, Role::Participant);
    h.sys.bind_place(place, "web", "room:main").await.unwrap();

    // 人が発話 → エージェントが 1 度発話して終える（ScriptedEngine）。
    h.eng.push(Step::say_done("見たよ"));
    g.send(json!({
        "id":"e1","m":"event","kind":"said","address":"room:main",
        "author":{"id":"test-owner","display":"test-owner"},"content":{"text":"これ見た？"}
    }));
    // said(1) + spoke(2) までログが伸びるのを待つ。
    assert!(
        g.wait_for(|_| h.sys.store().latest_seq(place).unwrap() >= 2)
            .await,
        "ターンが起きてログが 2 件になる"
    );

    // read で 2 件が返る。種別を絞らず、人の発話もエージェントの発話も同じ会話として。
    g.send(json!({"id":"r1","m":"read","address":"room:main","from":1}));
    assert!(
        g.wait_for(|l| read_ok(l, "r1").is_some()).await,
        "read に ok が返る: {:?}",
        g.log()
    );
    let ok = read_ok(&g.log(), "r1").unwrap();
    let events = ok.get("events").and_then(|x| x.as_array()).unwrap().clone();
    assert_eq!(events.len(), 2, "場で起きたこと全部が返る: {events:?}");
    assert_eq!(events[0].get("kind").and_then(|x| x.as_str()), Some("said"));
    assert_eq!(
        events[0].pointer("/author/id").and_then(|x| x.as_str()),
        Some("test-owner"),
        "外来の発話は著者の外界 id が載る"
    );
    assert_eq!(
        events[0].pointer("/content/text").and_then(|x| x.as_str()),
        Some("これ見た？")
    );
    assert_eq!(
        events[1].get("kind").and_then(|x| x.as_str()),
        Some("spoke")
    );
    assert_eq!(
        events[1]
            .pointer("/author/display")
            .and_then(|x| x.as_str()),
        Some("エージェントA"),
        "主体の発話は人格が display に載る"
    );
    // 2 件で尽きているので next は無い（§02）。
    assert!(ok.get("next").is_none(), "続きが無ければ next は返らない");
}

// 範囲と続きの扱い（プロトコル§02）: limit で切り、続きがあれば next、尽きたら next 無し。
// limit の丸め: 上限（500）を超える指定は上限に丸める。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn read_paginates_with_next_and_rounds_the_limit() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("web", "room:.+", json!([]), json!([]), json!([]))
        .await;
    let place = h
        .sys
        .create_place(Some("room:main"), None, &Policy::default(), None);
    h.sys.bind_place(place, "web", "room:main").await.unwrap();

    // 5 件入れる（発火する主体は居ないので、ただ積まれる）。
    for i in 1..=5 {
        g.send(json!({
            "id": format!("e{i}"), "m":"event","kind":"said","address":"room:main",
            "author":{"id":"test-owner"},"content":{"text": format!("m{i}")}
        }));
    }
    assert!(
        g.wait_for(|_| h.sys.store().latest_seq(place).unwrap() == 5)
            .await,
        "5 件受理"
    );

    // limit=2 で読む → 2 件、続きがあるので next=3（§02）。
    g.send(json!({"id":"r1","m":"read","address":"room:main","from":1,"limit":2}));
    assert!(g.wait_for(|l| read_ok(l, "r1").is_some()).await);
    let p1 = read_ok(&g.log(), "r1").unwrap();
    assert_eq!(
        p1.get("events").and_then(|x| x.as_array()).unwrap().len(),
        2,
        "limit で 2 件に切る"
    );
    assert_eq!(
        p1.get("next").and_then(|x| x.as_i64()),
        Some(3),
        "続きがあれば next（次の from）"
    );

    // next(=3) から続き（limit=2）→ seq 3,4 の 2 件、まだ 5 が残るので next=5。
    g.send(json!({"id":"r2","m":"read","address":"room:main","from":3,"limit":2}));
    assert!(g.wait_for(|l| read_ok(l, "r2").is_some()).await);
    let p2 = read_ok(&g.log(), "r2").unwrap();
    assert_eq!(p2.get("next").and_then(|x| x.as_i64()), Some(5));

    // limit を上限より大きく指定 → 上限に丸めるが、5 件しか無いので全部返り、尽きて next 無し（§02）。
    g.send(json!({"id":"r3","m":"read","address":"room:main","from":1,"limit":1000000}));
    assert!(g.wait_for(|l| read_ok(l, "r3").is_some()).await);
    let p3 = read_ok(&g.log(), "r3").unwrap();
    assert_eq!(
        p3.get("events").and_then(|x| x.as_array()).unwrap().len(),
        5,
        "上限超えの指定でも壊れず、在るだけ返る"
    );
    assert!(
        p3.get("next").is_none(),
        "尽きたら next は返らない（§02）: {p3:?}"
    );
}

// ===== 目標 2: 状態機械の「不正。落とす」升目 =====

// 名乗る前に喋る → 切断（詳細§02 接続済み + hello以外）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn drop_speak_before_hello() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    // hello ではなく event をいきなり送る。
    g.send(
        json!({"id":"x","m":"event","kind":"said","address":"a","author":{"id":"u"},"content":{}}),
    );
    assert!(g.wait_disconnect().await, "名乗る前に喋ったら切断される");
}

// 二度目の名乗り → 切断（詳細§02 使用可 + hello）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn drop_second_hello() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!([]), json!([]), json!([]))
        .await;
    g.send(json!({"id":"h2","m":"hello","protocol":1,"name":"nostr2","address_form":".+","tools":[],"effects":[],"capabilities":[]}));
    assert!(g.wait_disconnect().await, "二度目の名乗りで切断される");
}

// 知らない欄 → err（プロトコル§00）。読み飛ばさない・既定で埋めない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn drop_unknown_field() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!([]), json!([]), json!([]))
        .await;
    let place = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);
    h.sys.bind_place(place, "nostr", "filter:x").await.unwrap();
    // event に知らない欄 is_boosted を入れる。
    g.send(json!({
        "id":"e1","m":"event","kind":"said","address":"filter:x",
        "author":{"id":"u"},"content":{"text":"hi"},"is_boosted":true
    }));
    assert!(
        g.wait_for(|l| err_code(l, "e1").as_deref() == Some("unknown_field"))
            .await,
        "知らない欄は unknown_field: {:?}",
        g.log()
    );
    // 接続は生きている（err は切断ではない）。
    assert!(!g.is_disconnected());
}

// 知らない列挙値 → err（プロトコル§00）。近いものに寄せない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn drop_unknown_enum_in_hello() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    // effects に知らない種別 whisper。
    g.send(json!({
        "id":"h1","m":"hello","protocol":1,"name":"nostr","address_form":".+",
        "tools":[],"effects":["say","whisper"],"capabilities":[]
    }));
    assert!(
        g.wait_for(|l| err_code(l, "h1").as_deref() == Some("unknown_enum"))
            .await,
        "知らない列挙値は unknown_enum: {:?}",
        g.log()
    );
    // hello が落ちた → 切断される（§02）。
    assert!(g.wait_disconnect().await, "名乗りが落ちたら切断");
    assert!(
        h.sys.gate_spec(&GateName::new("nostr")).is_none(),
        "登録されない"
    );
}

// 終わった活動への応答（＝待ち手の居ない id への応答）→ 静かに落とす（プロトコル§00）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn drop_response_to_unknown_id() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!([]), json!([]), json!([]))
        .await;
    let place = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);
    h.sys.bind_place(place, "nostr", "filter:x").await.unwrap();

    // core が要求していない id への応答（期限切れ・終わった活動への遅れた応答に相当）。
    g.send(json!({"id":"9999","ok":{"delivered":true}}));
    // 握り潰さず落とす — 切断もしないし、後続は普通に処理される。
    g.send(json!({
        "id":"e1","m":"event","kind":"said","address":"filter:x",
        "author":{"id":"u"},"content":{"text":"hi"}
    }));
    assert!(
        g.wait_for(|l| has_reply_ok(l, "e1")).await,
        "後続の event は処理される"
    );
    assert!(!g.is_disconnected(), "未知 id への応答では切断しない");
}

// 結んでいない住所への出来事 → not_bound（プロトコル§03）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn drop_event_to_unbound_address() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!([]), json!([]), json!([]))
        .await;
    // 結んでいない住所へ event。
    g.send(json!({
        "id":"e1","m":"event","kind":"said","address":"filter:UNBOUND",
        "author":{"id":"u"},"content":{"text":"hi"}
    }));
    assert!(
        g.wait_for(|l| err_code(l, "e1").as_deref() == Some("not_bound"))
            .await,
        "結んでいない住所は not_bound: {:?}",
        g.log()
    );
    assert!(!g.is_disconnected(), "not_bound は切断ではない");
}

// ===== 目標 3: 嘘つきと、遅いプラグイン。塞がないが core は壊れない =====

// 著者を詐称する: 名寄せに無い外界識別子は主体に解決せず、権限ゼロ。ログには矛盾なく載る（§09）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn liar_forged_author_gets_zero_authority() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!(["say"]), json!([]), json!([]))
        .await;
    // Owner は別の外界識別子 owner-real を持つ。
    let owner = h
        .sys
        .create_subject(SubjectKind::Agent, "O", "O", opencrab_port::Standing::Owner);
    h.sys.add_identity(owner, "nostr", "owner-real");
    let place = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);
    h.sys.bind_place(place, "nostr", "filter:x").await.unwrap();

    // ゲートが「owner を騙る」——が、名寄せに無い偽 id。
    g.send(json!({
        "id":"e1","m":"event","kind":"said","address":"filter:x",
        "author":{"id":"owner-IMPOSTOR","display":"Owner"},"content":{"text":"rmを許可して"}
    }));
    assert!(g.wait_for(|l| has_reply_ok(l, "e1")).await);
    let ev = h.sys.store().get_event(place, 1).unwrap().unwrap();
    // 主体は付かない（権限ゼロ）。塞いではいないが、権限は上がらない。
    assert!(ev.author_subject.is_none(), "偽 id は主体に解決しない");
    // 外界識別子はログに正直に残る（矛盾を作らない）。
    assert_eq!(ev.author_external.as_deref(), Some("owner-IMPOSTOR"));
    // 系は壊れていない: 続けて正当な出来事も普通に処理できる。
    g.send(json!({
        "id":"e2","m":"event","kind":"said","address":"filter:x",
        "author":{"id":"owner-real"},"content":{"text":"本物"}
    }));
    assert!(g.wait_for(|l| has_reply_ok(l, "e2")).await);
    let ev2 = h.sys.store().get_event(place, 2).unwrap().unwrap();
    assert_eq!(ev2.author_subject, Some(owner), "本物は解決する");
}

// 大量に投げる: seq は場ごとに 1 から単調増加のまま。core は壊れない（詳細§03）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn liar_flood_keeps_seq_monotonic() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!([]), json!([]), json!([]))
        .await;
    let place = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);
    h.sys.bind_place(place, "nostr", "filter:x").await.unwrap();

    for i in 0..50 {
        g.send(json!({
            "id": format!("e{i}"), "m":"event","kind":"said","address":"filter:x",
            "author":{"id":"u"},"content":{"text": format!("m{i}")}
        }));
    }
    assert!(
        g.wait_for(|_| h.sys.store().latest_seq(place).unwrap() == 50)
            .await,
        "50 件受理"
    );
    // seq が 1..=50 で連続（採番が壊れない）。
    let rows = h.sys.store().read_range(place, 0, 50).unwrap();
    let seqs: Vec<_> = rows.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, (1..=50).collect::<Vec<_>>(), "seq は単調増加のまま");
}

// 運んでいないのに成功を返す: core は「配送した」と記録するが、それは届いた証拠にならない（§09）。
// core が壊れないこと・記録が矛盾しないことだけを確かめる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn liar_delivered_true_is_only_a_record() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.set_cfg(|c| {
        c.delivered = true; // 運んでいなくても真を返す
        c.effect_origin = None;
    });
    g.hello_ok("nostr", "filter:.+", json!(["say"]), json!([]), json!([]))
        .await;
    let a = h.sys.create_subject(
        SubjectKind::Agent,
        "A",
        "A",
        opencrab_port::Standing::Trusted,
    );
    let place = h.sys.create_place(
        Some("p"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(place, a, Role::Participant);
    h.sys.bind_place(place, "nostr", "filter:x").await.unwrap();

    h.eng.push(Step::say_done("出したことにする"));
    g.send(json!({"id":"e1","m":"event","kind":"said","address":"filter:x","author":{"id":"u"},"content":{"text":"go"}}));

    assert!(g.wait_for(|l| find_msg(l, "effect").is_some()).await);
    // 配送は sent として記録される（記録であって、届いた証拠ではない）。
    assert!(
        g.wait_for(|_| h
            .sys
            .store()
            .deliveries_for(place, 2)
            .unwrap()
            .iter()
            .any(|(g, s)| g.as_str() == "nostr" && s == "sent"))
            .await,
        "配送は記録される（§09 の「記録≠証拠」）: {:?}",
        h.sys.store().deliveries_for(place, 2)
    );
}

// 応答を返さない（遅い）: 期限で失敗として扱い、接続は切らない（プロトコル§00）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn slow_no_response_times_out_without_severing() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.set_cfg(|c| c.auto = false); // 何にも自動応答しない
    g.hello_ok("nostr", "filter:.+", json!([]), json!([]), json!([]))
        .await;
    let place = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);

    // bind は応答が返らない → 60 秒で失敗。接続は切らない。
    let sys = h.sys.clone();
    let bind = tokio::spawn(async move { sys.bind_place(place, "nostr", "filter:x").await });
    // 要求は届いている。
    assert!(
        g.wait_for(|l| find_msg(l, "bind").is_some()).await,
        "bind 要求は届く"
    );
    tokio::time::advance(Duration::from_secs(61)).await;
    let r = bind.await.unwrap();
    assert!(r.is_err(), "応答が返らなければ期限で失敗");
    assert!(!g.is_disconnected(), "遅いだけでは切らない（§00）");
}

// 切断して繋ぎ直す: 名乗りからやり直せる（同じ名前が再び使える・プロトコル§08）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn disconnect_and_reconnect() {
    let h = build();
    {
        let g = TestGate::connect(&h.plugd);
        g.hello_ok("nostr", "filter:.+", json!([]), json!([]), json!([]))
            .await;
        assert!(h.sys.gate_spec(&GateName::new("nostr")).is_some());
        // 二度目の hello で切断させる（回線切れの代わりに確定的に切る）。
        g.send(json!({"id":"h2","m":"hello","protocol":1,"name":"x","address_form":".+","tools":[],"effects":[],"capabilities":[]}));
        assert!(g.wait_disconnect().await);
    }
    // 切断で登録が消えている。
    for _ in 0..1000 {
        if h.sys.gate_spec(&GateName::new("nostr")).is_none() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        h.sys.gate_spec(&GateName::new("nostr")).is_none(),
        "切断で名乗りが消える"
    );

    // 繋ぎ直して同じ名前で名乗れる。
    let g2 = TestGate::connect(&h.plugd);
    g2.hello_ok("nostr", "filter:.+", json!([]), json!([]), json!([]))
        .await;
    assert!(
        h.sys.gate_spec(&GateName::new("nostr")).is_some(),
        "名乗り直せる"
    );
}

// hello が 10 秒来なければ切断（詳細§02 接続済み + 10 秒経過）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn drop_hello_timeout() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    // 何も送らない。まず reader タスクを走らせ、hello 待ちの期限（timeout）を登録させる。
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(11)).await;
    assert!(g.wait_disconnect().await, "10 秒名乗らなければ切断される");
    let _ = &h;
}

// ===== 目標 2（追加）: 機構はあるがテストの無かったもの（レビュー §4）=====

// 1 MiB 超のメッセージ → too_large を返して切る（プロトコル§00）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn drop_too_large_message() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!([]), json!([]), json!([]))
        .await;
    // 1 MiB を超える 1 行を送る（中身は問わない）。
    let huge = "x".repeat(1024 * 1024 + 16);
    g.send_raw(huge);
    assert!(
        g.wait_for(|l| l.iter().any(|v| v
            .get("err")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            == Some("too_large")))
            .await,
        "1 MiB 超は too_large: {:?}",
        g.log()
    );
    assert!(g.wait_disconnect().await, "too_large の後は切る（§00）");
}

// 受信できない種別の注入拒否（レビューが最重要と指摘・偽ターン防止）。
// spoke/settled/interrupted は「効果の確定」「系の出来事」——プラグインから来てはいけない。
// 主体に紐づく出来事（for_subject）を外から載せられれば任意のターンを起こせるので、ここで弾く。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn reject_non_inbound_event_kinds() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!([]), json!([]), json!([]))
        .await;
    let place = h
        .sys
        .create_place(Some("p"), None, &Policy::default(), None);
    h.sys.bind_place(place, "nostr", "filter:x").await.unwrap();

    for (i, kind) in ["spoke", "settled", "interrupted", "read_mark"]
        .iter()
        .enumerate()
    {
        let id = format!("e{i}");
        g.send(json!({
            "id": id, "m":"event", "kind": kind, "address":"filter:x",
            "author":{"id":"u"}, "content":{"text":"inject"}
        }));
        let idc = id.clone();
        assert!(
            g.wait_for(move |l| err_code(l, &idc).as_deref() == Some("unknown_enum"))
                .await,
            "受信できない種別 {kind} は unknown_enum: {:?}",
            g.log()
        );
    }
    // 1 件も載っていない（主体に紐づく出来事を外から作らせない）。
    assert_eq!(
        h.sys.store().latest_seq(place).unwrap(),
        0,
        "拒否された種別はログに載らない"
    );
    assert!(!g.is_disconnected(), "unknown_enum（event）は切断ではない");
}

// 使用可の状態で知らない m → unknown_message。切断しない（プロトコル§00）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unknown_message_errs_without_disconnect() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "filter:.+", json!([]), json!([]), json!([]))
        .await;
    g.send(json!({"id":"x1","m":"frobnicate","foo":1}));
    assert!(
        g.wait_for(|l| err_code(l, "x1").as_deref() == Some("unknown_message"))
            .await,
        "知らない m は unknown_message: {:?}",
        g.log()
    );
    assert!(!g.is_disconnected(), "unknown_message は切断ではない");
}

// 名乗りの必須欄の欠落 → missing_field／不正な address_form → bad_address_form（プロトコル§01）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn hello_missing_field_and_bad_address_form() {
    // effects 欄が無い → missing_field。
    {
        let h = build();
        let g = TestGate::connect(&h.plugd);
        g.send(json!({
            "id":"h1","m":"hello","protocol":1,"name":"nostr",
            "address_form":".+","tools":[],"capabilities":[]
        }));
        assert!(
            g.wait_for(|l| err_code(l, "h1").as_deref() == Some("missing_field"))
                .await,
            "必須欄の欠落は missing_field: {:?}",
            g.log()
        );
        assert!(g.wait_disconnect().await, "名乗りが落ちたら切断");
        let _ = &h;
    }
    // address_form が正規表現として不正 → bad_address_form。
    {
        let h = build();
        let g = TestGate::connect(&h.plugd);
        g.send(json!({
            "id":"h1","m":"hello","protocol":1,"name":"nostr",
            "address_form":"[unclosed","tools":[],"effects":[],"capabilities":[]
        }));
        assert!(
            g.wait_for(|l| err_code(l, "h1").as_deref() == Some("bad_address_form"))
                .await,
            "不正な address_form は bad_address_form: {:?}",
            g.log()
        );
        let _ = &h;
    }
}

// 同名のゲートの二重接続 → name_taken／同名のツール → tool_name_taken。1 本目が生き残る。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn duplicate_gate_name_and_tool_name() {
    let h = build();
    // 1 本目: ゲート nostr、ツール dup-tool。
    let g1 = TestGate::connect(&h.plugd);
    g1.hello_ok(
        "nostr",
        "filter:.+",
        json!([]),
        json!([]),
        json!([{"name":"dup-tool","description":"","params":{"type":"object","properties":{},"required":[]}}]),
    )
    .await;

    // 2 本目: 同じゲート名 nostr → name_taken。
    let g2 = TestGate::connect(&h.plugd);
    g2.send(json!({
        "id":"h1","m":"hello","protocol":1,"name":"nostr",
        "address_form":".+","tools":[],"effects":[],"capabilities":[]
    }));
    assert!(
        g2.wait_for(|l| err_code(l, "h1").as_deref() == Some("name_taken"))
            .await,
        "同名のゲートは name_taken: {:?}",
        g2.log()
    );

    // 3 本目: 別名だが同じツール名 dup-tool → tool_name_taken。
    let g3 = TestGate::connect(&h.plugd);
    g3.send(json!({
        "id":"h1","m":"hello","protocol":1,"name":"other",
        "address_form":".+","tools":[{"name":"dup-tool","description":"","params":{"type":"object","properties":{},"required":[]}}],
        "effects":[],"capabilities":[]
    }));
    assert!(
        g3.wait_for(|l| err_code(l, "h1").as_deref() == Some("tool_name_taken"))
            .await,
        "同名のツールは tool_name_taken: {:?}",
        g3.log()
    );

    // 1 本目は生き残っている（名乗りが残り、ツールも引ける）。
    assert!(
        h.sys.gate_spec(&GateName::new("nostr")).is_some(),
        "1 本目のゲートは生き残る"
    );
    assert!(
        h.sys.gate_spec(&GateName::new("other")).is_none(),
        "衝突した 3 本目は登録されない"
    );
}

// ===== 平文アクション文法: hello の actions 加算と verb の素通し =====

// hello の actions（省略可の加算）を core が値として読む。kind だけが意味を持ち、verb（name）と
// params は不透明に保持される。protocol は据え置き（1）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn hello_with_actions_parses_into_gate_spec() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.send(json!({
        "id":"h1","m":"hello","protocol":1,"name":"web","address_form":"room:.+",
        "tools":[],"effects":["say","react"],"capabilities":[],
        "actions":[
            {"name":"reply","description":"返信する","kind":"say"},
            {"name":"react","description":"反応する","kind":"react","params":{"enum":["👍","🎉"]}}
        ]
    }));
    assert!(
        g.wait_for(|l| has_reply_ok(l, "h1")).await,
        "actions つき hello に ok: {:?}",
        g.log()
    );
    let spec = h.sys.gate_spec(&GateName::new("web")).expect("registered");
    assert_eq!(spec.protocol, 1, "版は据え置き");
    assert_eq!(spec.actions.len(), 2);
    let reply = spec.actions.iter().find(|a| a.name == "reply").unwrap();
    assert_eq!(reply.kind, EffectKind::Say);
    let react = spec.actions.iter().find(|a| a.name == "react").unwrap();
    assert_eq!(react.kind, EffectKind::React);
    // params は不透明に保持される（core は検証にだけ使う）。
    assert_eq!(
        react.params.pointer("/enum/0").and_then(|x| x.as_str()),
        Some("👍")
    );
}

// actions を省いた hello（既存ゲート）は actions=[]（無改変で通る）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn hello_without_actions_defaults_to_empty() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.hello_ok("nostr", "npub1.+", json!(["say"]), json!([]), json!([]))
        .await;
    let spec = h
        .sys
        .gate_spec(&GateName::new("nostr"))
        .expect("registered");
    assert!(spec.actions.is_empty(), "actions 省略時は空");
}

// actions の未知の kind は unknown_enum で落とす（近いものへ寄せない・§00）→ 接続が切れる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn hello_with_unknown_action_kind_is_rejected() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.send(json!({
        "id":"h1","m":"hello","protocol":1,"name":"web","address_form":"room:.+",
        "tools":[],"effects":["say"],"capabilities":[],
        "actions":[{"name":"bogus","kind":"not_a_kind"}]
    }));
    assert!(
        g.wait_for(|l| err_code(l, "h1").as_deref() == Some("unknown_enum"))
            .await,
        "未知の kind は unknown_enum: {:?}",
        g.log()
    );
    assert!(g.wait_disconnect().await, "hello が落ちたら切断される");
    assert!(h.sys.gate_spec(&GateName::new("web")).is_none());
}

// 宣言 action の kind が effects に無い → 接続時に落とす（§01「拒否は接続時にしか起きない」）。
// これを通すと「メニューに出るが毎回 Denied→段3 エコー」の恒久の半端状態になる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn hello_with_action_kind_not_in_effects_is_rejected() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    // Ui のアクションを名乗るが effects に ui が無い。
    g.send(json!({
        "id":"h1","m":"hello","protocol":1,"name":"web","address_form":"room:.+",
        "tools":[],"effects":["say"],"capabilities":[],
        "actions":[{"name":"smile","kind":"ui"}]
    }));
    assert!(
        g.wait_for(|l| err_code(l, "h1").as_deref() == Some("action_kind_not_carried"))
            .await,
        "運べない kind の action は接続時に落とす: {:?}",
        g.log()
    );
    assert!(g.wait_disconnect().await, "hello が落ちたら切断される");
    assert!(h.sys.gate_spec(&GateName::new("web")).is_none());
}

// Say-kind の action は effects に "say" が無くても通る（Say は場に常在＝place_effects が無条件挿入）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn hello_say_action_allowed_without_say_in_effects() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.send(json!({
        "id":"h1","m":"hello","protocol":1,"name":"web","address_form":"room:.+",
        "tools":[],"effects":["react"],"capabilities":[],
        "actions":[{"name":"reply","kind":"say"}]
    }));
    assert!(
        g.wait_for(|l| has_reply_ok(l, "h1")).await,
        "Say-kind の action は say が effects に無くても通る: {:?}",
        g.log()
    );
    let spec = h.sys.gate_spec(&GateName::new("web")).expect("registered");
    assert_eq!(spec.actions.len(), 1);
    assert_eq!(spec.actions[0].kind, EffectKind::Say);
}

// verb の素通し: 平文アクション（zap→react）を宣言したゲートで、エージェントが `zap:1:` と書くと、
// core は kind=react として運びつつ、線の effect に verb="zap" を素通しする（ゲートが出し分ける材料）。
// core にはどこにも "zap" を書いていない（差分ゼロで通る）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn plaintext_action_verb_reaches_the_wire() {
    let h = build();
    let g = TestGate::connect(&h.plugd);
    g.set_cfg(|c| c.effect_origin = Some("web-out-1".into()));
    // web は say/react を運び、zap→react の平文アクションを宣言する。
    g.send(json!({
        "id":"h1","m":"hello","protocol":1,"name":"web","address_form":"room:.+",
        "tools":[],"effects":["say","react"],"capabilities":[],
        "actions":[{"name":"zap","description":"投げ銭","kind":"react"}]
    }));
    assert!(g.wait_for(|l| has_reply_ok(l, "h1")).await, "hello ok");

    let a = h.sys.create_subject(
        SubjectKind::Agent,
        "web-agent",
        "web-agent",
        opencrab_port::Standing::Trusted,
    );
    let place = h.sys.create_place(
        Some("room:main"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(place, a, Role::Participant);
    h.sys.bind_place(place, "web", "room:main").await.unwrap();

    // エージェントは平文で `zap:1:`（届いた発話 seq=1 へ）。ScriptedEngine は生の say を返すだけ——
    // 解釈は core が行う（provider 無改修の経路と同じ）。
    h.eng.push(Step::say_done("zap:1:"));

    g.send(json!({
        "id":"e1","m":"event","kind":"said","address":"room:main",
        "author":{"id":"test-owner"},"content":{"text":"これ良い"},"origin":"web-in-1"
    }));

    assert!(
        g.wait_for(|l| l.iter().any(|v| {
            v.get("m").and_then(|x| x.as_str()) == Some("effect")
                && v.get("kind").and_then(|x| x.as_str()) == Some("react")
                && v.get("verb").and_then(|x| x.as_str()) == Some("zap")
        }))
        .await,
        "react が verb=zap を載せて配送される: {:?}",
        g.log()
    );
    let react = g
        .log()
        .into_iter()
        .find(|v| v.get("kind").and_then(|x| x.as_str()) == Some("react"))
        .unwrap();
    // kind は react として運ばれ、宛先は届いた発話の外界識別子に解決される。
    assert_eq!(react.get("verb").and_then(|x| x.as_str()), Some("zap"));
    assert_eq!(
        react.get("target").and_then(|x| x.as_str()),
        Some("web-in-1")
    );
}
