//! 平文アクション文法（設計）のテスト。散文 say の本文を行ごとに解釈し、その場のメニューにある
//! verb だけをアクション効果へ展開する——それ以外は地の文（残余 say）。不成立は 3 段（置換ゼロ・
//! 逐語で残す・turn を失敗させない）。NO_REPLY は残余 say を配送しない唯一の core 共通語。
//!
//! 推論は差し替えた偽物（ScriptedEngine）、配送は記録するだけの Transport で回す。時間は pause。

use async_trait::async_trait;
use opencrab_engine::*;
use opencrab_port::*;
use opencrab_social_runtime::*;
use opencrab_store::Store;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 配送された効果を記録するだけの Transport。bind/unbind は成功、open は使わない。
/// deliver_effect は届いたことにして、外に出たものの識別子（origin）を一意に返す
///（自分の投稿に後から反応・取り消しできるよう external_ref が記録される）。
#[derive(Clone, Default)]
struct RecordingTransport {
    delivered: Arc<Mutex<Vec<(GateName, String, OutgoingEffect)>>>,
    seq: Arc<AtomicU64>,
}

impl RecordingTransport {
    fn new() -> RecordingTransport {
        RecordingTransport::default()
    }
    fn all(&self) -> Vec<(GateName, String, OutgoingEffect)> {
        self.delivered.lock().unwrap().clone()
    }
}

#[async_trait]
impl Transport for RecordingTransport {
    async fn compat_bind(&self, _gate: &GateName, _address: &str) -> Result<(), TransportError> {
        Ok(())
    }
    async fn compat_unbind(&self, _gate: &GateName, _address: &str) -> Result<(), TransportError> {
        Ok(())
    }
    async fn compat_open(
        &self,
        _gate: &GateName,
        _under: &str,
        _hint: Option<&str>,
    ) -> Result<String, TransportError> {
        Err(TransportError("open not supported in test".into()))
    }
    async fn compat_deliver_effect(
        &self,
        gate: &GateName,
        address: &str,
        effect: OutgoingEffect,
    ) -> Result<DeliveryAck, TransportError> {
        self.delivered
            .lock()
            .unwrap()
            .push((gate.clone(), address.to_string(), effect));
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        Ok(DeliveryAck {
            delivered: true,
            origin: Some(format!("out-{n}")),
        })
    }
    async fn bind_route(&self, route: &GateRoute) -> Result<(), TransportError> {
        self.compat_bind(&route.kind_id, &route.address).await
    }
    async fn unbind_route(&self, route: &GateRoute) -> Result<(), TransportError> {
        self.compat_unbind(&route.kind_id, &route.address).await
    }
    async fn deliver_effect_route(
        &self,
        route: &GateRoute,
        _seq: Seq,
        effect: OutgoingEffect,
    ) -> TransportDeliveryResult {
        match self
            .compat_deliver_effect(&route.kind_id, &route.address, effect)
            .await
        {
            Ok(ack) => TransportDeliveryResult::DefiniteAck(ack),
            Err(error) => TransportDeliveryResult::DefiniteFailure(error),
        }
    }
}

struct Harness {
    sys: System,
    eng: ScriptedEngine,
    tx: RecordingTransport,
}

fn build() -> Harness {
    let store = Store::new_in_memory().unwrap();
    // 会話予算の物差し（§06）。ScriptedEngine の既定モデル "scripted" に context_window を登録する
    // （200_000 × compaction_ratio 0.5 = 100_000・旧固定既定と同値）。未登録だと System::new が fail loud。
    store
        .register_model_context_window("scripted", 200_000)
        .unwrap();
    let eng = ScriptedEngine::new();
    let host = ScriptedToolHost::new();
    let notif = RecordingNotifier::new();
    let tx = RecordingTransport::new();
    let sys = System::new(
        store,
        Arc::new(eng.clone()),
        Arc::new(host),
        Arc::new(ScriptedShellHost::new()),
        Arc::new(notif),
        Arc::new(CharCounter),
        Config::default(),
    );
    sys.attach_transport(Arc::new(tx.clone()));
    Harness { sys, eng, tx }
}

async fn settle() {
    for _ in 0..400 {
        tokio::task::yield_now().await;
    }
}

fn action(name: &str, kind: EffectKind) -> ActionDef {
    ActionDef {
        name: name.into(),
        description: name.into(),
        params: serde_json::json!({}),
        kind,
    }
}

fn action_enum(name: &str, kind: EffectKind, vals: &[&str]) -> ActionDef {
    ActionDef {
        name: name.into(),
        description: name.into(),
        params: serde_json::json!({ "enum": vals }),
        kind,
    }
}

fn gate(name: &str, effects: &[EffectKind], actions: Vec<ActionDef>) -> GateSpec {
    GateSpec {
        name: GateName::new(name),
        protocol: PROTOCOL_VERSION,
        address_form: ".*".into(),
        tools: vec![],
        effects: effects.iter().copied().collect::<BTreeSet<_>>(),
        capabilities: BTreeSet::new(),
        actions,
    }
}

/// ゲートを名乗らせ、場をその住所へ provision する（接続後なので route も同じ入口で整合する）。
fn bind(h: &Harness, place: PlaceId, spec: GateSpec, address: &str) {
    let name = spec.name.clone();
    h.sys.register_gate(spec).unwrap();
    h.sys
        .provision_channel(place, name.as_str(), address)
        .unwrap();
}

fn inbound(address: &str, author: &str, text: &str, origin: &str) -> GateEvent {
    GateEvent {
        kind: EventKind::Said,
        address: address.into(),
        author_external: author.into(),
        author_display: None,
        content: Content::text(text),
        mentions: vec![],
        reply_to: None,
        target: None,
        origin: Some(origin.into()),
        attachments: vec![],
        discovery: None,
        metadata: serde_json::json!({}),
    }
}

/// 場と 1 体のエージェント A（default_subject・Direct で即応）を用意し、web ゲートを結ぶ。
/// 返り値は (place, A)。gate は effects と actions を指定して差し替える。
fn place_with_gate(
    h: &Harness,
    effects: &[EffectKind],
    actions: Vec<ActionDef>,
) -> (PlaceId, SubjectId) {
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let place = h.sys.create_place(
        Some("room:main"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(place, a, Role::Participant);
    bind(h, place, gate("web", effects, actions), "room:main");
    (place, a)
}

/// Owner identity は `attention.rs:79-81` と同型。
fn seed_owner(h: &Harness, external: &str) {
    let owner = h
        .sys
        .create_subject(SubjectKind::Human, "O", "O", Standing::Owner);
    h.sys.add_identity(owner, "web", external);
}

// 1. アクションを 1 つも含まない本文は、単一の say（target None・全チャネル broadcast）になる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn plain_text_becomes_single_untargeted_say() {
    let h = build();
    let (_place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say, EffectKind::React],
        vec![action("reply", EffectKind::Say)],
    );
    h.eng.push(Step::say_done("ただの文です"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "やあ", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    assert_eq!(d.len(), 1, "単一の効果が配送される: {d:?}");
    let (g, _addr, eff) = &d[0];
    assert_eq!(*g, GateName::new("web"));
    assert_eq!(eff.kind, EffectKind::Say);
    assert_eq!(eff.text.as_deref(), Some("ただの文です"));
    assert!(eff.target_origin.is_none(), "宛先なし（target None）");
    assert!(eff.verb.is_none(), "アクションではない（verb なし）");
}

// 2. reply 相当（Say-kind の verb＋番号）は target 付き say になり、配送先が宛先の origin のゲートに絞られる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn reply_becomes_targeted_say_narrowed_to_origin_gate() {
    let h = build();
    let (_place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say],
        vec![action("reply", EffectKind::Say)],
    );
    // inbound(note1) が seq1、A がそれへ返信する。
    h.eng.push(Step::say_done("reply:1:見たよ"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    assert_eq!(d.len(), 1, "返信 1 本: {d:?}");
    let (g, _addr, eff) = &d[0];
    assert_eq!(*g, GateName::new("web"), "宛先の origin のゲートへ");
    assert_eq!(eff.kind, EffectKind::Say);
    assert_eq!(eff.text.as_deref(), Some("見たよ"));
    assert_eq!(
        eff.target_origin.as_deref(),
        Some("note1"),
        "宛先の外界識別子"
    );
    assert_eq!(eff.verb.as_deref(), Some("reply"));
}

// 3. react 相当（React-kind の verb＋番号）は symbol を載せて配送される。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn react_carries_symbol() {
    let h = build();
    let (_place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say, EffectKind::React],
        vec![action("react", EffectKind::React)],
    );
    h.eng.push(Step::say_done("react:1:👍"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    assert_eq!(d.len(), 1, "反応 1 本: {d:?}");
    let (_g, _addr, eff) = &d[0];
    assert_eq!(eff.kind, EffectKind::React);
    assert_eq!(
        eff.symbol.as_deref(),
        Some("👍"),
        "content は symbol に載る"
    );
    assert!(eff.text.is_none());
    assert_eq!(eff.target_origin.as_deref(), Some("note1"));
    assert_eq!(eff.verb.as_deref(), Some("react"));
}

// 4. 動的解決: actions を宣言しないゲートだけの場では、同じ本文でも verb が散文のまま。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn verb_stays_prose_where_no_actions_declared() {
    let h = build();
    // actions=[] のゲート。メニューが空なので "reply:1:..." もアクションにならない。
    let (_place, _a) = place_with_gate(&h, &[EffectKind::Say], vec![]);
    h.eng
        .push(Step::say_done("reply:1:これはアクションではない"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    assert_eq!(d.len(), 1);
    let (_g, _addr, eff) = &d[0];
    assert_eq!(eff.kind, EffectKind::Say);
    assert_eq!(
        eff.text.as_deref(),
        Some("reply:1:これはアクションではない"),
        "メニューに無い verb は逐語で地の文"
    );
    assert!(eff.target_origin.is_none());
    assert!(eff.verb.is_none());
}

// 5. 不成立 3 段: 未宣言=散文／形不正=逐語で残す／stale seq=逐語で残す＋turn 継続。1 つの本文に混ぜる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn three_tiers_of_non_establishment_in_one_body() {
    let h = build();
    let (place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say, EffectKind::React, EffectKind::Ui],
        vec![
            action("reply", EffectKind::Say),
            action("react", EffectKind::React),
            action("smile", EffectKind::Ui),
        ],
    );
    // 1 行目: 成立する reply（seq1 は inbound で外界識別子あり）。
    // 2 行目: 未宣言 verb（段1・散文）。
    // 3 行目: 形不正（react は番号必須なのに空・段2）。
    // 4 行目: stale seq（reply:99 は解決しない・段3）。
    h.eng.push(Step::say_done(
        "reply:1:成立\nunknownverb:1:未宣言\nreact::番号なし\nreply:99:未解決",
    ));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    // 成立した reply（target 付き say）＋残余 say（3 行が逐語で結合）＝ 2 本。
    assert_eq!(d.len(), 2, "成立 reply ＋ 残余 say の 2 本: {d:?}");
    let reply = d
        .iter()
        .find(|(_, _, e)| e.verb.as_deref() == Some("reply"))
        .expect("成立した reply がある");
    assert_eq!(reply.2.text.as_deref(), Some("成立"));
    let remainder = d
        .iter()
        .find(|(_, _, e)| e.verb.is_none())
        .expect("残余 say がある");
    let rem = remainder.2.text.as_deref().unwrap();
    assert!(rem.contains("unknownverb:1:未宣言"), "未宣言は逐語: {rem}");
    assert!(rem.contains("react::番号なし"), "形不正は逐語: {rem}");
    assert!(rem.contains("reply:99:未解決"), "stale seq は逐語: {rem}");
    assert!(!rem.contains("成立"), "成立した行は残余に残らない: {rem}");

    // turn は失敗しない（段2/3 は本文に逃がして継続する）。
    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].end_reason, "done", "不成立でも turn は継続する");
}

// 6. seq→origin 不解決: 場に在る出来事だが外界識別子を持たない seq への reply は段3（逐語で残す）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unresolvable_seq_to_origin_is_kept_verbatim() {
    let h = build();
    let (place, a) = place_with_gate(
        &h,
        &[EffectKind::Say],
        vec![action("reply", EffectKind::Say)],
    );
    // 外界識別子を持たない内部の発話を 1 件置く（deliver は ref を作らない）。
    h.sys
        .deliver(place, Incoming::said(a, "内部の発話"))
        .unwrap();
    settle().await;
    let internal_seq = h.sys.store().latest_seq(place).unwrap();

    // その seq へ返信しようとする → 出来事は在るが origin に解決しない → 段3。
    h.eng
        .push(Step::say_done(&format!("reply:{internal_seq}:届かない")));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    // 成立した reply は無い。残余 say（逐語）だけが配送される。
    assert!(
        d.iter().all(|(_, _, e)| e.verb.is_none()),
        "成立したアクションは無い: {d:?}"
    );
    let joined: String = d
        .iter()
        .filter_map(|(_, _, e)| e.text.clone())
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        joined.contains(&format!("reply:{internal_seq}:届かない")),
        "解決しない seq の行は逐語で残る: {joined}"
    );
    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs[0].end_reason, "done", "turn は継続する");
}

// 7a. retract 所有: 自分の投稿は取り消せる（通る）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn retract_own_post_is_allowed() {
    let h = build();
    let (place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say, EffectKind::Retract],
        vec![
            action("reply", EffectKind::Say),
            action("retract", EffectKind::Retract),
        ],
    );
    // ターン1: A が発話する（配送され、外界識別子 out-N が記録される）。
    h.eng.push(Step::say_done("私の投稿"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "口火", "in1"),
        )
        .unwrap();
    settle().await;
    let own_seq = h.sys.store().latest_seq(place).unwrap(); // A の spoke の seq

    // ターン2: A が自分の投稿を取り消す。
    h.eng.push(Step::say_done(&format!("retract:{own_seq}:")));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "もう一声", "in2"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    let retract = d
        .iter()
        .find(|(_, _, e)| e.kind == EffectKind::Retract)
        .expect("自分の投稿の取り消しは通る");
    assert_eq!(retract.2.verb.as_deref(), Some("retract"));
}

// 7b. retract 所有: 他人（外来・主体なし）の投稿は取り消せない（Denied → 段3 で逐語で残す・turn 継続）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn retract_others_post_is_denied_and_kept() {
    let h = build();
    let (place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say, EffectKind::Retract],
        vec![action("retract", EffectKind::Retract)],
    );
    // inbound(note1)=seq1 は外来の著者（主体なし）。A がそれを取り消そうとする。
    h.eng.push(Step::say_done("retract:1:"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "他人の発言", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    assert!(
        d.iter().all(|(_, _, e)| e.kind != EffectKind::Retract),
        "他人の投稿は取り消されない: {d:?}"
    );
    let joined: String = d
        .iter()
        .filter_map(|(_, _, e)| e.text.clone())
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        joined.contains("retract:1:"),
        "Denied 行は逐語で残る: {joined}"
    );
    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs[0].end_reason, "done", "turn は失敗しない");
}

// 8. zap 拡張: hello に「zap→React」を足すだけで通り、OutgoingEffect.verb="zap" が届く（core 差分ゼロ）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn zap_extension_passes_verb_through_with_zero_core_change() {
    let h = build();
    // zap は React-kind の新しい verb。core にはどこにも "zap" を書いていない。
    let (_place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say, EffectKind::React],
        vec![action("zap", EffectKind::React)],
    );
    h.eng.push(Step::say_done("zap:1:"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    assert_eq!(d.len(), 1, "zap 1 本: {d:?}");
    let (_g, _addr, eff) = &d[0];
    assert_eq!(eff.kind, EffectKind::React, "kind は React として運ばれる");
    assert_eq!(eff.verb.as_deref(), Some("zap"), "verb はゲートへ素通し");
    assert_eq!(eff.target_origin.as_deref(), Some("note1"));
}

// 9. エスケープ: 先頭に空白のある行・非数字 seq の行は、アクションにならず地の文になる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn leading_space_and_non_digit_seq_are_prose() {
    let h = build();
    let (_place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say],
        vec![action("reply", EffectKind::Say)],
    );
    // 1 行目: 先頭空白でエスケープ。2 行目: 非数字 seq で regex 不一致。
    h.eng
        .push(Step::say_done(" reply:1:先頭空白\nreply:x:非数字"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    assert!(
        d.iter().all(|(_, _, e)| e.verb.is_none()),
        "どちらもアクションにならない: {d:?}"
    );
    let joined: String = d
        .iter()
        .filter_map(|(_, _, e)| e.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains(" reply:1:先頭空白"),
        "先頭空白は逐語: {joined}"
    );
    assert!(
        joined.contains("reply:x:非数字"),
        "非数字 seq は逐語: {joined}"
    );
}

// 10. 対象なし往復: smile::（Ui）は Ui を名乗ったゲートにだけ配送され、Ui を名乗らない nostr には漏れない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn targetless_ui_delivered_only_to_declaring_gate() {
    let h = build();
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let place = h.sys.create_place(
        Some("room:main"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(place, a, Role::Participant);
    // web は Ui を名乗り smile アクションを持つ。nostr は Say だけ（Ui を名乗らない）。
    bind(
        &h,
        place,
        gate(
            "web",
            &[EffectKind::Say, EffectKind::Ui],
            vec![action("smile", EffectKind::Ui)],
        ),
        "room:main",
    );
    bind(
        &h,
        place,
        gate("nostr", &[EffectKind::Say], vec![]),
        "filter:x",
    );

    h.eng.push(Step::say_done("smile::"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    let ui: Vec<_> = d
        .iter()
        .filter(|(_, _, e)| e.kind == EffectKind::Ui)
        .collect();
    assert_eq!(ui.len(), 1, "Ui は 1 本だけ: {d:?}");
    assert_eq!(ui[0].0, GateName::new("web"), "Ui を名乗った web にだけ");
    assert_eq!(ui[0].2.verb.as_deref(), Some("smile"));
    assert!(
        d.iter().all(|(g, _, _)| *g != GateName::new("nostr")),
        "Ui を名乗らない nostr には漏れない: {d:?}"
    );
}

// 11. 内容型: params の enum に合う content は成立し、外れる content は不成立（段2・逐語で残す）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn content_type_enum_is_enforced() {
    let h = build();
    let (_place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say, EffectKind::Ui],
        vec![action_enum("dance", EffectKind::Ui, &["excited"])],
    );
    // 成立: excited。不成立: foo（enum 外）。
    h.eng.push(Step::say_done("dance::excited\ndance::foo"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    let dance: Vec<_> = d
        .iter()
        .filter(|(_, _, e)| e.verb.as_deref() == Some("dance"))
        .collect();
    assert_eq!(dance.len(), 1, "成立するのは excited だけ: {d:?}");
    // foo は残余 say に逐語で残る。
    let joined: String = d
        .iter()
        .filter(|(_, _, e)| e.verb.is_none())
        .filter_map(|(_, _, e)| e.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("dance::foo"),
        "enum 外は逐語で残る: {joined}"
    );
    assert!(
        !joined.contains("dance::excited"),
        "成立行は残らない: {joined}"
    );
}

// 12. 対称違反（オーナー裁定の seq 規則）: Ui **以外**は番号必須、Ui は番号禁止。どちらの違反も段2。
//     Say も番号必須なので `reply::x`（番号欠け）は不成立——黙って target 無し say に降格させない
//     （隠れフォールバックにしない）。両 kind（Say/React）で番号欠けを、Ui で番号過剰を固定する。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn symmetry_violations_do_not_establish() {
    let h = build();
    let (_place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say, EffectKind::React, EffectKind::Ui],
        vec![
            action("reply", EffectKind::Say),
            action("react", EffectKind::React),
            action("smile", EffectKind::Ui),
        ],
    );
    // reply::（Say 番号欠け・不成立）／react::（React 番号欠け・不成立）／smile:12:（Ui 番号過剰・不成立）。
    h.eng.push(Step::say_done(
        "reply::番号忘れ\nreact::記号なし\nsmile:12:番号あり",
    ));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    assert!(
        d.iter().all(|(_, _, e)| e.verb.is_none()),
        "対称違反はどれもアクションにならない: {d:?}"
    );
    let joined: String = d
        .iter()
        .filter_map(|(_, _, e)| e.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("reply::番号忘れ"),
        "Say の番号欠けは逐語で残る（broadcast へ降格させない）: {joined}"
    );
    assert!(
        joined.contains("react::記号なし"),
        "React の番号欠けは逐語: {joined}"
    );
    assert!(
        joined.contains("smile:12:番号あり"),
        "Ui の番号過剰は逐語: {joined}"
    );
}

// 13a. NO_REPLY 単独: 残余 say を 1 本も配送せず、end_reason=no_reply を立てる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn no_reply_alone_delivers_nothing_and_sets_end_reason() {
    let h = build();
    let (place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say],
        vec![action("reply", EffectKind::Say)],
    );
    h.eng.push(Step::say_done("NO_REPLY"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    assert!(h.tx.all().is_empty(), "配送はゼロ: {:?}", h.tx.all());
    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs[0].end_reason, "no_reply", "end_reason=no_reply");
    assert!(recs[0].withheld_text.is_none(), "保留する地の文は無い");
}

// 13b. NO_REPLY 併用: 明示アクション（react）は発火するが、地の文は配送されず withheld_text に残る。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn no_reply_with_action_delivers_only_the_action_and_withholds_prose() {
    let h = build();
    let (place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say, EffectKind::React],
        vec![action("react", EffectKind::React)],
    );
    // react は発火・NO_REPLY で地の文「秘密の独り言」は配送しない。
    h.eng
        .push(Step::say_done("react:1:👍\nNO_REPLY\n秘密の独り言"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    assert_eq!(d.len(), 1, "react だけが配送される: {d:?}");
    assert_eq!(d[0].2.kind, EffectKind::React);
    assert_eq!(d[0].2.verb.as_deref(), Some("react"));

    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs[0].end_reason, "no_reply");
    assert_eq!(
        recs[0].withheld_text.as_deref(),
        Some("秘密の独り言"),
        "保留した地の文は withheld_text に残る"
    );
    // 地の文は場の共有ログにも載らない。
    let latest = h.sys.store().latest_seq(place).unwrap();
    let evs = h.sys.store().read_range(place, 0, latest).unwrap();
    assert!(
        evs.iter()
            .all(|e| e.content.text.as_deref() != Some("秘密の独り言")),
        "保留した地の文はログに載らない"
    );
}

// 13c. #747: sentinel の前後に地の文があっても fail-closed で全地の文を保留する。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn embedded_no_reply_sentinel_withholds_all_prose() {
    let h = build();
    let (place, _a) = place_with_gate(&h, &[EffectKind::Say], vec![]);
    h.eng
        .push(Step::say_done("内部の考えを先に書く NO_REPLY 末尾にも文章"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    assert!(h.tx.all().is_empty(), "sentinel を含む出力は公開しない");
    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs[0].end_reason, "no_reply");
    assert_eq!(
        recs[0].withheld_text.as_deref(),
        Some("内部の考えを先に書く NO_REPLY 末尾にも文章"),
        "握った本文は turn record に残す"
    );
}

// 13d. #750: sentinel は個々の Say ではなく InferOutput 全体へ効く。前後どちらの順でも別 Say を公開せず、
// 握った散文は turn record に残す。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn no_reply_in_one_say_withholds_other_says_in_both_orders() {
    let h = build();
    let (place, _a) = place_with_gate(&h, &[EffectKind::Say], vec![]);
    h.eng.push(
        Step::done()
            .with_effect(EffectSpec::say("NO_REPLY"))
            .with_effect(EffectSpec::say("sentinel より後の散文")),
    );
    h.eng.push(
        Step::done()
            .with_effect(EffectSpec::say("sentinel より前の散文"))
            .with_effect(EffectSpec::say("NO_REPLY")),
    );

    for (origin, request) in [("synthetic-one", "一件目"), ("synthetic-two", "二件目")] {
        h.sys
            .deliver_event(
                &GateName::new("web"),
                inbound("room:main", "synthetic-author", request, origin),
            )
            .unwrap();
        settle().await;
    }

    assert!(h.tx.all().is_empty(), "同じ推論の別 Say も配送しない");
    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0].end_reason, "no_reply");
    assert_eq!(
        recs[0].withheld_text.as_deref(),
        Some("sentinel より後の散文")
    );
    assert_eq!(recs[1].end_reason, "no_reply");
    assert_eq!(
        recs[1].withheld_text.as_deref(),
        Some("sentinel より前の散文")
    );
}

// 13e. #750: tool result を受けた次反復で sentinel が出ても、先の反復と同じ反復の別 Say をともに公開しない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn no_reply_after_tool_result_withholds_prose_across_iterations() {
    let h = build();
    let (place, _a) = place_with_gate(&h, &[EffectKind::Say], vec![]);
    h.eng.push(
        Step::cont()
            .with_effect(EffectSpec::say("一反復目の散文"))
            .with_tool("core-child-list"),
    );
    h.eng.push(
        Step::done()
            .with_effect(EffectSpec::say("NO_REPLY"))
            .with_effect(EffectSpec::say("二反復目の散文")),
    );
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound(
                "room:main",
                "synthetic-author",
                "子の場を確認して",
                "synthetic-origin",
            ),
        )
        .unwrap();
    settle().await;

    assert_eq!(h.eng.call_count(), 2, "tool result の後に再推論した");
    assert!(h.tx.all().is_empty(), "どちらの反復の散文も配送しない");
    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].iterations, 2);
    assert_eq!(recs[0].end_reason, "no_reply");
    assert_eq!(
        recs[0].withheld_text.as_deref(),
        Some("一反復目の散文\n二反復目の散文")
    );
}

// 13f. #750: sentinel は独立 token / 正しい空 action 形だけ。部分文字列、引用、説明中の隣接文字列、
// content/seq を持つ不正 action 形は通常の発話として逐語で残す。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn no_reply_lookalikes_are_delivered_as_ordinary_prose() {
    let h = build();
    let (place, _a) = place_with_gate(&h, &[EffectKind::Say], vec![]);
    let prose = [
        "NO_REPLYING は別の語",
        "`NO_REPLY` は制御語の説明",
        "\"NO_REPLY\" は引用",
        "NO_REPLYという文字列を説明する",
        "NO_REPLY:12:本文",
        "NO_REPLY::説明",
        " NO_REPLY",
        "\tNO_REPLY::",
    ];
    for text in prose {
        h.eng.push(Step::say_done(text));
    }
    for (index, text) in prose.iter().enumerate() {
        h.sys
            .deliver_event(
                &GateName::new("web"),
                inbound(
                    "room:main",
                    "synthetic-author",
                    "通常応答を求める",
                    &format!("synthetic-origin-{index}"),
                ),
            )
            .unwrap();
        settle().await;
        assert_eq!(
            h.tx.all().len(),
            index + 1,
            "通常本文を 1 件ずつ配送する: {text}"
        );
    }

    let delivered: Vec<String> =
        h.tx.all()
            .into_iter()
            .filter_map(|(_, _, effect)| effect.text)
            .collect();
    assert_eq!(delivered, prose);
    assert!(
        h.sys
            .store()
            .turn_records(place)
            .unwrap()
            .iter()
            .all(|record| record.end_reason == "done" && record.withheld_text.is_none()),
        "lookalike は no_reply にしない"
    );
}

// 13f. #786: Owner + 地の文 + NO_REPLY → Spoke は NOTICE のみ。ログに地の文なし。withheld 残。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn owner_prose_no_reply_delivers_notice_only() {
    let h = build();
    let (place, _a) = place_with_gate(&h, &[EffectKind::Say], vec![]);
    seed_owner(&h, "ownerpk");
    h.eng.push(Step::say_done("秘密の独り言\nNO_REPLY"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "ownerpk", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    let delivered: Vec<String> =
        h.tx.all()
            .into_iter()
            .filter_map(|(_, _, effect)| effect.text)
            .collect();
    assert_eq!(
        delivered,
        vec![OWNER_DIRECT_NO_REPLY_NOTICE.to_string()],
        "Spoke は NOTICE のみ: {delivered:?}"
    );

    let latest = h.sys.store().latest_seq(place).unwrap();
    let evs = h.sys.store().read_range(place, 0, latest).unwrap();
    assert!(
        evs.iter()
            .all(|e| e.content.text.as_deref() != Some("秘密の独り言")),
        "ログに地の文なし"
    );
    assert!(
        evs.iter().any(|e| {
            e.kind == EventKind::Spoke
                && e.content.text.as_deref() == Some(OWNER_DIRECT_NO_REPLY_NOTICE)
        }),
        "NOTICE が Spoke として残る"
    );

    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs[0].end_reason, "no_reply");
    assert_eq!(
        recs[0].withheld_text.as_deref(),
        Some("秘密の独り言"),
        "withheld はモデル地の文のまま"
    );
}

// 13g. #786: Owner + bare NO_REPLY → NOTICE。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn owner_bare_no_reply_delivers_notice() {
    let h = build();
    let (place, _a) = place_with_gate(&h, &[EffectKind::Say], vec![]);
    seed_owner(&h, "ownerpk");
    h.eng.push(Step::say_done("NO_REPLY"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "ownerpk", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    let delivered: Vec<String> =
        h.tx.all()
            .into_iter()
            .filter_map(|(_, _, effect)| effect.text)
            .collect();
    assert_eq!(
        delivered,
        vec![OWNER_DIRECT_NO_REPLY_NOTICE.to_string()],
        "bare NO_REPLY も NOTICE: {delivered:?}"
    );
    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs[0].end_reason, "no_reply");
    assert!(recs[0].withheld_text.is_none(), "保留する地の文は無い");
}

// 13h. #786: Owner + tool 行 + NO_REPLY → 0 通知（A3 / settle 待ち）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn owner_tool_line_no_reply_delivers_no_notice() {
    let h = build();
    let (place, _a) = place_with_gate(&h, &[EffectKind::Say], vec![]);
    seed_owner(&h, "ownerpk");
    h.eng.push(Step::say_done("core-child-list::{}\nNO_REPLY"));
    h.eng.push(Step::say_done("synthetic-settle"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "ownerpk", "子を見て", "note1"),
        )
        .unwrap();
    settle().await;

    let notices: Vec<_> =
        h.tx.all()
            .into_iter()
            .filter(|(_, _, effect)| effect.text.as_deref() == Some(OWNER_DIRECT_NO_REPLY_NOTICE))
            .collect();
    assert!(
        notices.is_empty(),
        "tool 行 + NO_REPLY は A3（0 通知）: {notices:?}"
    );
    let recs = h.sys.store().turn_records(place).unwrap();
    let first = recs
        .iter()
        .find(|r| r.end_reason == "no_reply")
        .expect("no_reply のターン");
    assert!(
        first
            .tool_lines
            .as_deref()
            .is_some_and(|lines| lines.contains("core-child-list::{}")),
        "受理した tool 行が残る: {:?}",
        first.tool_lines
    );
}

// 13i. #786: 非 Owner + 地の文 + NO_REPLY → A3 のまま 0 配送。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn non_owner_prose_no_reply_delivers_nothing() {
    let h = build();
    let (place, _a) = place_with_gate(&h, &[EffectKind::Say], vec![]);
    h.eng.push(Step::say_done("秘密の独り言\nNO_REPLY"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    assert!(
        h.tx.all().is_empty(),
        "非 Owner は A3 のまま 0 配送: {:?}",
        h.tx.all()
    );
    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs[0].end_reason, "no_reply");
    assert_eq!(recs[0].withheld_text.as_deref(), Some("秘密の独り言"));
}

// 14. メニュー描画: 文脈に NO_REPLY が最初に無条件注入され、宣言されたアクションがテンプレートで
//     列挙される（番号欄は Ui 以外が持つ）。内容枠の意味（絵文字等）は core が発明せず description に委ねる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn action_menu_is_rendered_into_context() {
    let h = build();
    let (_place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say, EffectKind::React, EffectKind::Ui],
        vec![
            action("reply", EffectKind::Say),   // 番号必須
            action("react", EffectKind::React), // 番号必須
            action("smile", EffectKind::Ui),    // 番号を持てない
        ],
    );
    h.eng.push(Step::say_done("こんにちは"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    // アクションメニューは system（人格＋枠づけ＋文法前文＋メニュー）に載る（設計で rendered から移設）。
    let ctx = h.eng.systems().into_iter().next().expect("1 反復ある");
    assert!(ctx.contains("NO_REPLY"), "NO_REPLY が注入される: {ctx}");
    assert!(
        ctx.contains("PROGRESS::<文>"),
        "PROGRESS（2 つ目の core 共通語）も注入される: {ctx}"
    );
    assert!(
        ctx.contains("reply:<番号>:<内容>"),
        "Say の verb は番号欄つき: {ctx}"
    );
    assert!(
        ctx.contains("react:<番号>:<内容>"),
        "React の verb は番号欄つき: {ctx}"
    );
    assert!(
        ctx.contains("smile::<内容>"),
        "Ui の verb は番号欄なし・内容枠つき: {ctx}"
    );
    // 内容枠の意味（絵文字等）を core が発明しない——"絵文字" という語をメニューに書かない。
    assert!(
        !ctx.contains("絵文字"),
        "内容の型を core が発明しない: {ctx}"
    );
}

// 15. React の content は絵文字とは限らない: カスタム絵文字のショートコード（`:smile:`）も、core は
//     content 文法を判定せず symbol スロットへ素通しする（NIP-30 等の解釈は線の向こうのゲートの仕事）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn react_shortcode_content_passes_through_to_symbol() {
    let h = build();
    let (_place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say, EffectKind::React],
        vec![action("react", EffectKind::React)],
    );
    // 内容枠にショートコード（絵文字 1 字ではない）。core は妥当性を判定せず素通しする。
    h.eng.push(Step::say_done("react:1::smile:"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "ねえ", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    assert_eq!(d.len(), 1, "react 1 本: {d:?}");
    assert_eq!(d[0].2.kind, EffectKind::React);
    assert_eq!(
        d[0].2.symbol.as_deref(),
        Some(":smile:"),
        "ショートコードがそのまま symbol に載る（core は判定しない）"
    );
    assert_eq!(d[0].2.verb.as_deref(), Some("react"));
    assert_eq!(d[0].2.target_origin.as_deref(), Some("note1"));
}

// 16. PROGRESS（2 つ目の core 共通語・進捗の揮発表示）: `PROGRESS::<文>` は say でもイベントでもない
//     ので、配送は 1 本も起きない（揮発配送は activity 通知の側・ここでは効果が出ないことを確かめる）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn progress_line_delivers_no_effect() {
    let h = build();
    let (_place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say, EffectKind::React],
        vec![action("reply", EffectKind::Say)],
    );
    h.eng.push(Step::say_done("PROGRESS::読み込み中"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "やあ", "note1"),
        )
        .unwrap();
    settle().await;

    assert!(
        h.tx.all().is_empty(),
        "PROGRESS は効果を配送しない: {:?}",
        h.tx.all()
    );
}

// 16b. PROGRESS の形不正は段2（逐語で残余 say）: 空文（`PROGRESS::`）も seq 付き（`PROGRESS:12:…`）も、
//     制御行にならず地の文として逐語で 1 本の say に残る（黙って消さない）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn empty_and_seq_progress_fall_back_to_verbatim_say() {
    let h = build();
    let (_place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say, EffectKind::React],
        vec![action("reply", EffectKind::Say)],
    );
    // 1 行目は空文（content 無し）、2 行目は seq 付き。どちらも成立せず段2 で残余 say に逐語。
    h.eng.push(Step::say_done("PROGRESS::\nPROGRESS:12:あとで"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "やあ", "note1"),
        )
        .unwrap();
    settle().await;

    let d = h.tx.all();
    assert_eq!(d.len(), 1, "形不正の 2 行は 1 本の say に逐語で残る: {d:?}");
    assert_eq!(d[0].2.kind, EffectKind::Say);
    assert_eq!(
        d[0].2.text.as_deref(),
        Some("PROGRESS::\nPROGRESS:12:あとで"),
        "空文・seq 付きは逐語のまま（段2）"
    );
    assert!(d[0].2.verb.is_none(), "アクションではない（verb なし）");
}

// #796: QC 形（PROGRESS 行 + reply 行 + 末尾空行）は公開 Spoke ちょうど 1 本。空 Spoke は作らない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn progress_reply_trailing_blank_is_one_public_spoke() {
    let h = build();
    let (place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say],
        vec![action("reply", EffectKind::Say)],
    );
    h.eng.push(Step::say_done(
        "PROGRESS::読み込み中\n\nreply:1:実行結果は以下の通りです。\n",
    ));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "やあ", "note1"),
        )
        .unwrap();
    settle().await;

    let delivered: Vec<_> =
        h.tx.all()
            .into_iter()
            .filter(|(_, _, e)| e.kind == EffectKind::Say)
            .collect();
    assert_eq!(delivered.len(), 1, "公開 say は 1 本: {delivered:?}");
    assert_eq!(
        delivered[0].2.text.as_deref(),
        Some("実行結果は以下の通りです。")
    );
    assert!(
        delivered
            .iter()
            .all(|(_, _, e)| e.text.as_deref().is_some_and(|t| !t.trim().is_empty())),
        "空／空白の配送は無い: {delivered:?}"
    );

    let last = h.sys.store().latest_seq(place).unwrap();
    let log = h.sys.store().read_range(place, 0, last).unwrap();
    let spokes: Vec<_> = log.iter().filter(|e| e.kind == EventKind::Spoke).collect();
    assert_eq!(spokes.len(), 1, "公開 Spoke はちょうど 1: {spokes:?}");
    assert_eq!(
        spokes[0].content.text.as_deref(),
        Some("実行結果は以下の通りです。")
    );
    assert!(
        spokes.iter().all(|e| e
            .content
            .text
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())),
        "空 Spoke は無い: {spokes:?}"
    );

    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].end_reason, "done", "ターンは失敗にしない");
    assert_eq!(
        recs[0].failure_detail.as_deref(),
        Some(EMPTY_SAY_DROPPED_NOTE),
        "破棄した事実をターン記録に残す"
    );
}

// #796: 空白のみの残余 say は公開 Spoke 0 本。破棄した事実だけをターン記録に残す。
// 推論全体が空（#719）ではない——PROGRESS が非空なので interpret まで届く。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn whitespace_only_remainder_is_zero_spokes_and_note() {
    let h = build();
    let (place, _a) = place_with_gate(
        &h,
        &[EffectKind::Say],
        vec![action("reply", EffectKind::Say)],
    );
    h.eng.push(Step::say_done("PROGRESS::作業中\n   \n\t"));
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "やあ", "note1"),
        )
        .unwrap();
    settle().await;

    assert!(
        h.tx.all().iter().all(|(_, _, e)| e.kind != EffectKind::Say),
        "空白のみ残余は配送しない: {:?}",
        h.tx.all()
    );

    let last = h.sys.store().latest_seq(place).unwrap();
    let log = h.sys.store().read_range(place, 0, last).unwrap();
    assert!(
        log.iter().all(|e| e.kind != EventKind::Spoke),
        "空白のみ残余は Spoke を生まない: {log:?}"
    );

    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].end_reason, "done", "#719 の空推論失敗にはしない");
    assert_eq!(
        recs[0].failure_detail.as_deref(),
        Some(EMPTY_SAY_DROPPED_NOTE)
    );
}
