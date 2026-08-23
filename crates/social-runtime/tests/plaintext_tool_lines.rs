//! 平文ツール行（設計）のテスト。散文 say の本文を行ごとに解釈し、その場の**併合名簿**（アクション
//! ＋広告ツール）にある verb を解決する——ツール名に当たった行はツール呼びになる（`名前::内容`）。
//! ツールは常に切り離して決着イベント化する（core も含む・must_settle）。不成立は 3 段（段1 地の文／
//! 段2 形不正を逐語／段3 権限 Denied を逐語）で、どれも turn を失敗させない。
//!
//! 推論は差し替えた偽物（ScriptedEngine）、ゲートツールは ScriptedToolHost、配送は記録する Transport。
//! 時間は pause。

use async_trait::async_trait;
use opencrab_engine::*;
use opencrab_port::*;
use opencrab_social_runtime::*;
use opencrab_store::Store;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 配送された効果を記録するだけの Transport（plaintext_actions.rs と同じ流儀）。
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
    /// 配送された say（宛先なし・宛先つき問わず）の本文だけ。
    fn says(&self) -> Vec<String> {
        self.all()
            .into_iter()
            .filter(|(_, _, e)| e.kind == EffectKind::Say)
            .filter_map(|(_, _, e)| e.text)
            .collect()
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
    host: ScriptedToolHost,
    tx: RecordingTransport,
}

fn build() -> Harness {
    build_cfg(Config::default())
}

fn build_cfg(cfg: Config) -> Harness {
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
        Arc::new(host.clone()),
        Arc::new(ScriptedShellHost::new()),
        Arc::new(notif),
        Arc::new(CharCounter),
        cfg,
    );
    sys.attach_transport(Arc::new(tx.clone()));
    Harness { sys, eng, host, tx }
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

/// required がちょうど 1 つの string のツール（位置引数で束ねられる形）。description は名前由来にして、
/// 本文メニューの description が宣言由来であることを測れるようにする。
fn tool_one_string(name: &str, field: &str) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: format!("{name} の説明文"),
        params: serde_json::json!({
            "type": "object",
            "properties": { field: {"type": "string"} },
            "required": [field]
        }),
    }
}

fn gate(
    name: &str,
    effects: &[EffectKind],
    actions: Vec<ActionDef>,
    tools: Vec<ToolDef>,
) -> GateSpec {
    GateSpec {
        name: GateName::new(name),
        protocol: PROTOCOL_VERSION,
        address_form: ".*".into(),
        tools,
        effects: effects.iter().copied().collect::<BTreeSet<_>>(),
        capabilities: BTreeSet::new(),
        actions,
    }
}

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
    }
}

/// 場と 1 体のエージェント A（Direct で即応）を用意し、web ゲート（effects/actions/tools 指定）を結ぶ。
fn place_with(
    h: &Harness,
    standing: Standing,
    effects: &[EffectKind],
    actions: Vec<ActionDef>,
    tools: Vec<ToolDef>,
) -> (PlaceId, SubjectId) {
    let a = h.sys.create_subject(SubjectKind::Agent, "A", "A", standing);
    let place = h.sys.create_place(
        Some("room:main"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(place, a, Role::Participant);
    bind(h, place, gate("web", effects, actions, tools), "room:main");
    (place, a)
}

/// A を起こす外来メッセージ（Direct）。
fn wake(h: &Harness) {
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "やあ", "note1"),
        )
        .unwrap();
}

/// 決着イベント（Settled）の本文だけを場のログから拾う。
fn settle_texts(h: &Harness, place: PlaceId) -> Vec<String> {
    let last = h.sys.store().latest_seq(place).unwrap();
    h.sys
        .store()
        .read_range(place, 0, last)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::Settled)
        .filter_map(|e| e.content.text)
        .collect()
}

fn bg_activities(h: &Harness) -> Vec<opencrab_store::ActivityRow> {
    h.sys
        .store()
        .all_activities()
        .unwrap()
        .into_iter()
        .filter(|a| a.kind == ActivityKindTag::Background)
        .collect()
}

// ---- パース: 位置引数束ね ----

// 1. 位置引数: required がちょうど 1 つの string のツールは、content をその 1 引数に束ねる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn positional_content_binds_to_single_required_string() {
    let h = build();
    let (_p, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.host.set_immediate("gate-look", "みつけた");
    h.eng.push(Step::say_done("gate-look::ねこ"));
    h.eng.push(Step::no_reply()); // 決着から起きるターン
    wake(&h);
    settle().await;

    // 位置引数が {q:"ねこ"} に束ねられて線を渡る。
    assert_eq!(h.host.invoke_count("gate-look"), 1, "ツールが 1 回呼ばれる");
    assert_eq!(
        h.host.last_args("gate-look"),
        Some(serde_json::json!({"q": "ねこ"})),
        "content が required の 1 引数に束ねられる"
    );
    // ツール行だけなので say は配送されない。
    assert!(
        h.tx.says().is_empty(),
        "ツール行のみ: say 配送なし: {:?}",
        h.tx.says()
    );
}

// 2. 1 行 JSON: content が { で始まれば JSON として読み、required の存在と enum 会員を検証する。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn one_line_json_is_parsed_and_validated() {
    let h = build();
    let (_p, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.host.set_immediate("gate-look", "ok");
    h.eng
        .push(Step::say_done(r#"gate-look::{"q":"みず","extra":1}"#));
    h.eng.push(Step::no_reply());
    wake(&h);
    settle().await;

    assert_eq!(h.host.invoke_count("gate-look"), 1);
    assert_eq!(
        h.host.last_args("gate-look"),
        Some(serde_json::json!({"q": "みず", "extra": 1})),
        "1 行 JSON がそのまま引数になる（required 充足・他キーは説明扱いで通す）"
    );
}

// 3. 段2: { で始まるのに JSON として壊れていれば、黙って位置引数に倒さず逐語で残す。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn broken_json_is_tier2_verbatim_not_positional() {
    let h = build();
    let (_p, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.host.set_immediate("gate-look", "ok");
    h.eng.push(Step::say_done(r#"gate-look::{"q":"#)); // 閉じない JSON
    wake(&h);
    settle().await;

    assert_eq!(
        h.host.invoke_count("gate-look"),
        0,
        "壊れた JSON は実行しない（段2）"
    );
    assert!(bg_activities(&h).is_empty(), "背景活動も作らない");
    // 逐語で残余 say に残る（外界へ配送される＝モデルが自分のエコーで自己修正できる）。
    assert_eq!(
        h.tx.says(),
        vec![r#"gate-look::{"q":"#.to_string()],
        "壊れた JSON 行は逐語で残る"
    );
}

// 3b. 段2: required が 0/複数のツールを位置引数形式で呼んだら逐語で残す（束ね先が決まらない）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn positional_with_wrong_arity_is_tier2() {
    let h = build();
    // required が 2 つのツール。
    let two_req = ToolDef {
        name: "gate-pair".into(),
        description: "pair".into(),
        params: serde_json::json!({
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
            "required": ["a", "b"]
        }),
    };
    let (_p, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![two_req],
    );
    h.host.set_immediate("gate-pair", "ok");
    h.eng.push(Step::say_done("gate-pair::ひとつだけ"));
    wake(&h);
    settle().await;

    assert_eq!(
        h.host.invoke_count("gate-pair"),
        0,
        "required 複数を位置引数で呼べない（段2）"
    );
    assert_eq!(
        h.tx.says(),
        vec!["gate-pair::ひとつだけ".to_string()],
        "逐語で残る"
    );
}

// 4. 段1: 併合名簿に無い verb はただの地の文（記録＝ログに載る・逐語）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unknown_verb_is_tier1_prose() {
    let h = build();
    let (_p, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.eng.push(Step::say_done("nosuchtool::x"));
    wake(&h);
    settle().await;

    assert_eq!(h.host.invoke_count("gate-look"), 0);
    assert_eq!(
        h.tx.says(),
        vec!["nosuchtool::x".to_string()],
        "未宣言 verb は地の文"
    );
}

// 5. 段3: 権限 Denied のツール行は逐語で残余 say に残し、turn を失敗させない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn permission_denied_tool_line_is_tier3_verbatim() {
    let h = build();
    // 立場 Unknown の参加者は core-create-place（Trusted 以上）を使えない → Denied。
    let (place, _a) = place_with(&h, Standing::Unknown, &[EffectKind::Say], vec![], vec![]);
    h.eng.push(Step::say_done("core-create-place::{}"));
    wake(&h);
    settle().await;

    // 決着イベントも背景活動も作らない（効果を作る前に段3 で捌く）。
    assert!(settle_texts(&h, place).is_empty(), "実行しない");
    assert!(bg_activities(&h).is_empty(), "背景も作らない");
    assert_eq!(
        h.tx.says(),
        vec!["core-create-place::{}".to_string()],
        "Denied 行は逐語で残る"
    );
    // turn は失敗しない。
    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].end_reason, "done", "段3 でも turn は継続する");
}

// ---- 衝突: action verb == tool 名は両方落として地の文 ----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn action_tool_name_collision_drops_both_to_prose() {
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
    // ゲート 1: アクション "foo"（Say）。ゲート 2: ツール "foo"。別ゲートなので register は通る
    // （register が弾くのは同一ゲート内の衝突だけ）——併合名簿が両方落とす。
    bind(
        &h,
        place,
        gate(
            "web",
            &[EffectKind::Say],
            vec![action("foo", EffectKind::Say)],
            vec![],
        ),
        "room:main",
    );
    bind(
        &h,
        place,
        gate(
            "tools",
            &[EffectKind::Say],
            vec![],
            vec![tool_one_string("foo", "q")],
        ),
        "room:main",
    );
    h.host.set_immediate("foo", "ok");
    h.eng.push(Step::say_done("foo::x"));
    wake(&h);
    settle().await;

    // foo はアクションにもツールにもならず地の文（推測で倒さない）。
    assert_eq!(
        h.host.invoke_count("foo"),
        0,
        "衝突した verb はツール実行されない"
    );
    assert!(bg_activities(&h).is_empty());
    assert!(
        h.tx.says().iter().any(|s| s == "foo::x"),
        "衝突 verb は逐語で地の文: {:?}",
        h.tx.says()
    );
}

// ---- must_settle: core ツール行が平文経路で決着イベント化する（回帰） ----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn core_tool_line_settles_instead_of_returning_synchronously() {
    let h = build();
    let (place, _a) = place_with(&h, Standing::Trusted, &[EffectKind::Say], vec![], vec![]);
    // core-recall は required=["word"] の string。位置引数で束ねる。
    h.eng.push(Step::say_done("core-recall::ねこ"));
    h.eng.push(Step::no_reply()); // 決着から起きるターン
    wake(&h);
    settle().await;

    // core ツールも背景活動＋決着イベントになる（同期返しではない）。
    let bgs = bg_activities(&h);
    assert_eq!(bgs.len(), 1, "core ツールも背景活動になる");
    assert_eq!(bgs[0].end_reason.as_deref(), Some("done"));
    let settles = settle_texts(&h, place);
    assert_eq!(settles.len(), 1, "決着イベントが 1 つ");
    assert!(settles[0].contains("成功"), "成功が判る: {}", settles[0]);
    assert!(
        settles[0].contains("該当なし"),
        "結果本文が載る: {}",
        settles[0]
    );
    // ツール行だけなので say 配送なし。
    assert!(h.tx.says().is_empty(), "say 配送なし: {:?}", h.tx.says());
}

// ---- ゲートツール行の決着 ----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn gate_tool_line_settles_with_result() {
    let h = build();
    let (place, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.host.set_immediate("gate-look", "GATERESULT");
    h.eng.push(Step::say_done("gate-look::しらべて"));
    h.eng.push(Step::no_reply());
    wake(&h);
    settle().await;

    assert_eq!(h.host.invoke_count("gate-look"), 1);
    let settles = settle_texts(&h, place);
    assert_eq!(settles.len(), 1, "決着イベントが 1 つ");
    assert!(
        settles[0].contains("GATERESULT"),
        "ゲートの結果が決着に載る: {}",
        settles[0]
    );
    assert!(settles[0].contains("成功"), "成功が判る: {}", settles[0]);
    assert!(h.tx.says().is_empty(), "say 配送なし");
}

// ---- BackgroundFull の決着可視化 ----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn background_full_refusal_is_settled_visibly() {
    let store = Store::new_in_memory().unwrap();
    store
        .register_model_context_window("scripted", 200_000)
        .unwrap();
    let eng = ScriptedEngine::new();
    let host = ScriptedToolHost::new();
    let notif = RecordingNotifier::new();
    let tx = RecordingTransport::new();
    // 背景の同時数の上限を 0 に絞る（どのゲートツールも始める前に断られる）。
    let cfg = Config {
        bg_per_place: 0,
        ..Config::default()
    };
    let sys = System::new(
        store,
        Arc::new(eng.clone()),
        Arc::new(host.clone()),
        Arc::new(ScriptedShellHost::new()),
        Arc::new(notif),
        Arc::new(CharCounter),
        cfg,
    );
    sys.attach_transport(Arc::new(tx.clone()));
    let h = Harness { sys, eng, host, tx };

    let (place, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.eng.push(Step::say_done("gate-look::しらべて"));
    h.eng.push(Step::no_reply()); // 決着（断り）から起きるターン
    wake(&h);
    settle().await;

    // 断りは決着イベントとして可視化される（黙って落ちない）。ツールは呼ばれない（始める前に断る）。
    assert_eq!(h.host.invoke_count("gate-look"), 0, "上限で始める前に断る");
    let settles = settle_texts(&h, place);
    assert_eq!(settles.len(), 1, "断りが決着イベントとして 1 つ");
    assert!(
        settles[0].contains("gate-look") && settles[0].contains("始められなかった"),
        "断りが見える: {}",
        settles[0]
    );
}

// ---- ツール行のみ = say 配送なし（既に上で確認しているが、地の文ゼロを明示） ----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn tool_line_only_delivers_no_say() {
    let h = build();
    let (_p, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.host.set_immediate("gate-look", "ok");
    h.eng.push(Step::say_done("gate-look::x"));
    h.eng.push(Step::no_reply());
    wake(&h);
    settle().await;

    assert!(h.tx.all().is_empty(), "何も配送されない: {:?}", h.tx.all());
    assert_eq!(h.host.invoke_count("gate-look"), 1, "ツールは走る");
}

// ---- ツール行 + NO_REPLY = 地の文は withhold・ツール行は記録して走る ----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn tool_line_with_no_reply_withholds_prose_and_records_line() {
    let h = build();
    let (place, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.host.set_immediate("gate-look", "ok");
    // 地の文 + ツール行 + NO_REPLY。
    h.eng.push(Step::say_done(
        "これは配送しない\ngate-look::しらべる\nNO_REPLY",
    ));
    h.eng.push(Step::no_reply());
    wake(&h);
    settle().await;

    // say は配送されない（NO_REPLY）。ツールは走る。
    assert!(
        h.tx.says().is_empty(),
        "NO_REPLY で say 配送なし: {:?}",
        h.tx.says()
    );
    assert_eq!(
        h.host.invoke_count("gate-look"),
        1,
        "ツール行は NO_REPLY でも走る"
    );
    // ターン記録: 地の文は withheld_text、ツール行は tool_lines、end_reason=no_reply。
    let recs = h.sys.store().turn_records(place).unwrap();
    let first = recs
        .iter()
        .find(|r| r.end_reason == "no_reply")
        .expect("no_reply のターン");
    assert_eq!(
        first.withheld_text.as_deref(),
        Some("これは配送しない"),
        "地の文を withhold"
    );
    assert_eq!(
        first.tool_lines.as_deref(),
        Some("gate-look::しらべる"),
        "受理したツール行を記録"
    );
}

// ---- render_tool_menu: owner-only（立場不足）は非表示・説明は宣言 description 由来 ----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn tool_menu_hides_privileged_and_uses_declared_description() {
    let h = build();
    // 平文専用 engine（ネイティブ道具を出せない）。core が本文にツールメニューを描く。
    h.eng.set_emits_tool_calls(false);
    // 立場 Unknown の参加者。core-recall（ParticipantTool）は見え、core-create-place（Trusted 以上）は隠れる。
    let (_p, _a) = place_with(
        &h,
        Standing::Unknown,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.eng.push(Step::no_reply()); // 発話しない（配送を起こさない）→ 文脈だけ組ませる
    wake(&h);
    settle().await;

    // ツールメニューは system に載る（平文専用 engine 経路・設計で system へ移設）。
    let ctx = h.eng.last_system().expect("system が組まれた");
    assert!(
        ctx.contains("=== 使える道具（ツール） ==="),
        "system にツールメニュー節がある"
    );
    assert!(
        ctx.contains("情報が足りないときは、まずツールの行だけを書き"),
        "実測で効いた行動指示が入る"
    );
    // 立場で使えるツールは description 由来の説明つきで出る。
    assert!(ctx.contains("core-recall"), "使えるツールは出る");
    assert!(
        ctx.contains("gate-look の説明文"),
        "説明はツール宣言の description 由来"
    );
    // 立場不足のツールは非表示。
    assert!(
        !ctx.contains("core-create-place"),
        "使えないツールは隠れる（owner-only 非表示）"
    );
}

// ---- emits_tool_calls=false: 本文にメニュー・ネイティブ tools は空 ----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn plaintext_engine_gets_body_menu_and_empty_native_tools() {
    let h = build();
    h.eng.set_emits_tool_calls(false);
    let (_p, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.eng.push(Step::no_reply());
    wake(&h);
    settle().await;

    // ネイティブ道具宣言は空（本文のメニューが代わり）。
    let seen = h.eng.tools_seen();
    assert_eq!(seen.len(), 1, "1 ターンぶん");
    assert!(seen[0].is_empty(), "ネイティブ tools は空: {:?}", seen[0]);
    // system にメニューが描かれ、ツールは名前入りで載る（平文専用 engine 経路・設計で system へ移設）。
    let ctx = h.eng.last_system().unwrap();
    assert!(ctx.contains("=== 使える道具（ツール） ==="));
    assert!(ctx.contains("gate-look"));
}

// ---- emits_tool_calls=true（既定）: ネイティブ tools に載り、本文メニューは描かない ----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn native_engine_gets_tools_and_no_body_menu() {
    let h = build();
    // 既定 emits=true のまま。
    let (_p, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.eng.push(Step::no_reply());
    wake(&h);
    settle().await;

    let seen = h.eng.tools_seen();
    assert!(
        seen[0].iter().any(|n| n == "gate-look"),
        "ネイティブ tools にゲートツールが載る: {:?}",
        seen[0]
    );
    let ctx = h.eng.last_context().unwrap();
    assert!(
        !ctx.contains("=== 使える道具（ツール） ==="),
        "本文にツールメニューは描かない"
    );
}

// ---- ツールメニューの符号化例: 単一 string=素の値・必須複数=1 行 JSON・enum=候補 ----
// 実測（composer-2.5）で、例が無いと `key=value` 形式を書いて不成立になった。メニューが content の
// 書き方（1 行 JSON か位置引数の値か）を宣言 params から自動生成して教える（tool_call_example）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn tool_menu_shows_argument_encoding_examples() {
    let h = build();
    h.eng.set_emits_tool_calls(false);
    // 必須が複数（string × 2）＝1 行 JSON 経路のツール。
    let two_req = ToolDef {
        name: "gate-pair".into(),
        description: "pair の説明文".into(),
        params: serde_json::json!({
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
            "required": ["a", "b"]
        }),
    };
    let (_p, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![
            tool_one_string("gate-look", "q"), // 単一 string ＝位置引数
            two_req,                           // 必須複数 ＝1 行 JSON
            tool_enum("gate-mode", "mode", &["a", "b"]), // enum ＝第 1 候補を素の値で
        ],
    );
    h.eng.push(Step::no_reply());
    wake(&h);
    settle().await;

    let ctx = h.eng.last_system().expect("system が組まれた");
    // 前文が符号化を教える（key=value は使えない・プロバイダ名は名指ししない）。
    assert!(
        ctx.contains("`key=value` の形式は使えない"),
        "前文が引数の符号化を教える: {ctx}"
    );
    // 単一 string の必須は位置引数（値をそのまま書く）。フィールド名がプレースホルダに出る。
    assert!(
        ctx.contains("- gate-look::<q>  gate-look の説明文"),
        "単一 string ツールは素の値の例: {ctx}"
    );
    // 必須複数は 1 行 JSON。両キーが宣言順で入り、値はプレースホルダ。
    assert!(
        ctx.contains(r#"- gate-pair::{"a":"…","b":"…"}  pair の説明文"#),
        "必須複数ツールは 1 行 JSON の例: {ctx}"
    );
    // enum は第 1 候補を素の値で（位置引数経路・dance::excited の流儀）。
    assert!(
        ctx.contains("- gate-mode::a  "),
        "enum ツールは第 1 候補を例に反映: {ctx}"
    );
    // 古い `<内容>` 一律プレースホルダは各ツール行から消えている（符号化を教えない旧形）。
    // アクションメニュー側の `verb::<内容>`（Ui 種）とは衝突しないよう、ツール名付きで確認する。
    assert!(
        !ctx.contains("gate-look::<内容>") && !ctx.contains("gate-pair::<内容>"),
        "ツール行に一律 `<内容>` プレースホルダは残っていない: {ctx}"
    );
}

// ---- register_gate: core- 予約拒否・同一ゲート action==tool 拒否 ----

#[test]
fn register_gate_rejects_reserved_and_same_gate_collision() {
    let h = build();
    // ツール名が core- → 予約名で拒否。
    let g1 = gate(
        "g1",
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("core-foo", "q")],
    );
    assert_eq!(h.sys.register_gate(g1), Err(HelloReject::ReservedName));
    // アクション名が core- → 予約名で拒否。
    let g2 = gate(
        "g2",
        &[EffectKind::Say],
        vec![action("core-bar", EffectKind::Say)],
        vec![],
    );
    assert_eq!(h.sys.register_gate(g2), Err(HelloReject::ReservedName));
    // 同一ゲート内で action 名 == tool 名 → 衝突で拒否。
    let g3 = gate(
        "g3",
        &[EffectKind::Say],
        vec![action("dup", EffectKind::Say)],
        vec![tool_one_string("dup", "q")],
    );
    assert_eq!(
        h.sys.register_gate(g3),
        Err(HelloReject::ActionToolCollision)
    );
    // 正常なゲートは通る。
    let ok = gate(
        "g4",
        &[EffectKind::Say],
        vec![action("reply", EffectKind::Say)],
        vec![tool_one_string("look", "q")],
    );
    assert_eq!(h.sys.register_gate(ok), Ok(()));
}

// ---- :smile: 類のコロン入り content がツール行でも壊れない ----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn colon_content_in_tool_line_survives() {
    let h = build();
    let (_p, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-emoji", "s")],
    );
    h.host.set_immediate("gate-emoji", "ok");
    // content が :smile: （コロンを含む）。regex の content は行末まで貪欲に取るので壊れない。
    h.eng.push(Step::say_done("gate-emoji:::smile:"));
    h.eng.push(Step::no_reply());
    wake(&h);
    settle().await;

    assert_eq!(
        h.host.invoke_count("gate-emoji"),
        1,
        "コロン入り content でも実行される"
    );
    assert_eq!(
        h.host.last_args("gate-emoji"),
        Some(serde_json::json!({"s": ":smile:"})),
        "コロンを含む content がそのまま引数に束ねられる"
    );
    assert!(h.tx.says().is_empty(), "ツール行なので say 配送なし");
}

// ---- seq 付きツール行は段2（ツール行は seq 欄が空の形） ----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn tool_line_with_seq_is_tier2() {
    let h = build();
    let (_p, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.host.set_immediate("gate-look", "ok");
    // seq（番号）を付けたツール行は形不正（段2）——ツールは走らず逐語で残る。
    h.eng.push(Step::say_done("gate-look:5:ねこ"));
    wake(&h);
    settle().await;

    assert_eq!(
        h.host.invoke_count("gate-look"),
        0,
        "seq 付きツール行は実行しない"
    );
    assert!(bg_activities(&h).is_empty());
    assert_eq!(
        h.tx.says(),
        vec!["gate-look:5:ねこ".to_string()],
        "逐語で残る（段2）"
    );
}

// ---- enum × 位置引数: 非会員は段2・会員は実行（JSON 経路と対称） ----

fn tool_enum(name: &str, field: &str, vals: &[&str]) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: format!("{name} の説明文"),
        params: serde_json::json!({
            "type": "object",
            "properties": { field: {"type": "string", "enum": vals} },
            "required": [field]
        }),
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn positional_enum_membership_is_enforced() {
    let h = build();
    let (place, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_enum("gate-mode", "mode", &["a", "b"])],
    );
    h.host.set_immediate("gate-mode", "ok");
    // 1 行目: 非会員 "c" → 段2（逐語）。2 行目: 会員 "a" → 実行。
    h.eng.push(Step::say_done("gate-mode::c\ngate-mode::a"));
    h.eng.push(Step::no_reply());
    wake(&h);
    settle().await;

    assert_eq!(h.host.invoke_count("gate-mode"), 1, "会員のみ実行される");
    assert_eq!(
        h.host.last_args("gate-mode"),
        Some(serde_json::json!({"mode": "a"})),
        "会員の値が束ねられる"
    );
    // 非会員行は逐語で残余 say に残る（外界へ配送）。
    assert!(
        h.tx.says().iter().any(|s| s == "gate-mode::c"),
        "非会員は段2で逐語: {:?}",
        h.tx.says()
    );
    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs[0].end_reason, "done", "非会員でも turn は継続する");
}

// ---- 1 反復の平文ツール行に上限: 上限内は全実行・超過は段2 ----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn tool_lines_per_turn_cap_overflow_is_tier2() {
    // 上限 2 に絞る（背景の同時上限は既定 4 のまま——先に平文の上限が効く）。
    let cfg = Config {
        plaintext_tools_per_turn: 2,
        ..Config::default()
    };
    let h = build_cfg(cfg);
    let (place, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.host.set_immediate("gate-look", "ok");
    // 3 本のツール行。上限内の 2 本は実行、3 本目は段2（逐語で残余 say）。
    h.eng.push(Step::say_done(
        "gate-look::x1\ngate-look::x2\ngate-look::x3",
    ));
    h.eng.push(Step::no_reply()); // 決着から起きるターン（複数決着でも同じターンで足りる）
    h.eng.push(Step::no_reply());
    h.eng.push(Step::no_reply());
    wake(&h);
    settle().await;

    assert_eq!(h.host.invoke_count("gate-look"), 2, "上限 2 まで実行する");
    // 3 本目は逐語で残る（見える形）。
    assert_eq!(
        h.tx.says(),
        vec!["gate-look::x3".to_string()],
        "超過分は段2で逐語（見える）: {:?}",
        h.tx.says()
    );
    // ツール行はすべて記録に残る（受理した 2 本・黙って消さない）。超過分は say に出るので記録は 2 本。
    let recs = h.sys.store().turn_records(place).unwrap();
    let first = &recs[0];
    assert_eq!(
        first.tool_lines.as_deref(),
        Some("gate-look::x1\ngate-look::x2"),
        "受理した 2 本を記録"
    );
    assert_eq!(first.end_reason, "done");
}

// ---- system（人格＋場の枠づけ＋文法前文＋メニュー）の構成（persona-system-prompt 設計）----

/// place_with と同じだが、エージェントの persona（人格本文）を指定できる。表示名（name）は persona とは
/// 別のマーカーにして、system に載るのは persona 本文であって name ではないことも測れるようにする。
fn place_with_persona(
    h: &Harness,
    standing: Standing,
    persona: &str,
    effects: &[EffectKind],
    actions: Vec<ActionDef>,
    tools: Vec<ToolDef>,
) -> (PlaceId, SubjectId) {
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "表示名マーカー", persona, standing);
    let place = h.sys.create_place(
        Some("room:main"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(place, a, Role::Participant);
    bind(h, place, gate("web", effects, actions, tools), "room:main");
    (place, a)
}

// A. system は「① persona 本文（逐語・先頭）→ ② 場の枠づけ → ③ 文法前文 → ④ アクションメニュー」の順。
//    文法前文は**形と NO_REPLY だけ**を語り、具体的な verb（reply/react）は名指ししない（オーナー裁定
//    「core はアクション語彙を持たない」）。verb は宣言駆動のメニュー（④）だけが教える。「地の文はそのまま
//    発話として配送される」は必ず含む（E2E 欠落の核心）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn system_composition_orders_persona_framing_grammar_menu() {
    let h = build();
    let persona = "テストエージェントの人格本文___PMARK___";
    let (_p, _a) = place_with_persona(
        &h,
        Standing::Trusted,
        persona,
        &[EffectKind::Say, EffectKind::React],
        vec![
            action("reply", EffectKind::Say),
            action("react", EffectKind::React),
        ],
        vec![],
    );
    h.eng.push(Step::no_reply());
    wake(&h);
    settle().await;

    let sys = h.eng.last_system().expect("system が組まれた");
    // ① persona が逐語で先頭（core は枠を被せない）。表示名（name）は system に載らない。
    assert!(sys.starts_with(persona), "persona が逐語で先頭: {sys}");
    assert!(
        !sys.contains("表示名マーカー"),
        "name は system に載らない: {sys}"
    );
    // ②③④ が順に並ぶ。メニューはヘッダで位置を取る（NO_REPLY は前文にも出るので曖昧になる）。
    let i_framing = sys.find("この場のチャットに参加").expect("② 場の枠づけ");
    let i_grammar = sys.find("アクション文法で書く").expect("③ 文法前文");
    let i_menu = sys
        .find("=== できること（アクション） ===")
        .expect("④ アクションメニュー");
    assert!(persona.len() <= i_framing, "① persona → ② 枠づけ");
    assert!(i_framing < i_grammar, "② 枠づけ → ③ 文法前文");
    assert!(i_grammar < i_menu, "③ 文法前文 → ④ メニュー");
    // 文法前文の核心: 地の文がそのまま発話として配送される旨（E2E で欠けていた）。
    assert!(
        sys.contains("地の文") && sys.contains("そのまま発話として配送"),
        "地の文の配送を明言: {sys}"
    );
    // NO_REPLY は前文で名指ししてよい（唯一の core 共通語）。
    assert!(sys.contains("NO_REPLY"), "NO_REPLY を含む: {sys}");
    // 具体 verb（reply/react）は前文（③〜④の間）には出ず、メニュー（④以降）にだけ出る。
    let preamble = &sys[i_grammar..i_menu];
    assert!(
        !preamble.contains("reply") && !preamble.contains("react"),
        "前文は具体 verb を名指ししない: {preamble}"
    );
    let menu = &sys[i_menu..];
    assert!(
        menu.contains("reply:<番号>") && menu.contains("react:<番号>"),
        "宣言した verb はメニューが教える: {menu}"
    );
}

// B. ネイティブに道具を出せる engine（emits=true・既定）: `ctx.tools` に道具が載り、system には
//    ツールメニューもツール行文法前文も描かない（宣言できる engine に平文メニューは要らない）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn native_engine_declares_tools_and_omits_tool_menu_from_system() {
    let h = build();
    // emits は既定で true（set_emits_tool_calls を呼ばない）。
    let (_p, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.eng.push(Step::no_reply());
    wake(&h);
    settle().await;

    // ネイティブ道具宣言に載る。
    let seen = h.eng.tools_seen();
    assert_eq!(seen.len(), 1, "1 ターンぶん");
    assert!(
        seen[0].iter().any(|n| n == "gate-look"),
        "ctx.tools に道具が載る: {:?}",
        seen[0]
    );
    // system にはツールメニューもツール行文法前文も無い。
    let sys = h.eng.last_system().expect("system が組まれた");
    assert!(
        !sys.contains("=== 使える道具（ツール） ==="),
        "ネイティブ engine には system にツールメニューを描かない: {sys}"
    );
    assert!(
        !sys.contains("まずツールの行だけを書き"),
        "ツール行文法前文は emits=false のときだけ: {sys}"
    );
    // アクション文法前文は無条件（emits に依らず入る）。
    assert!(
        sys.contains("アクション文法で書く"),
        "アクション文法前文は無条件: {sys}"
    );
    assert!(
        sys.contains("NO_REPLY"),
        "アクションメニューは常に入る: {sys}"
    );
}

// C. Agent の persona が空: **fail loud**。engine を回さず（call_count=0）、turn を end_reason=empty_persona で
//    終える（黙って空 system を engine へ渡さない）。配送も起きない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn empty_persona_agent_fails_loud_without_running_engine() {
    let h = build();
    let (place, _a) = place_with_persona(
        &h,
        Standing::Trusted,
        "", // 空 persona
        &[EffectKind::Say],
        vec![action("reply", EffectKind::Say)],
        vec![],
    );
    h.eng.push(Step::say_done("これは配送されないはず"));
    wake(&h);
    settle().await;

    assert_eq!(
        h.eng.call_count(),
        0,
        "空 persona では engine を回さない（fail loud）"
    );
    let recs = h.sys.store().turn_records(place).unwrap();
    assert_eq!(recs.len(), 1, "ターンは記録される");
    assert_eq!(recs[0].end_reason, "empty_persona", "理由は empty_persona");
    assert!(h.tx.says().is_empty(), "配送は起きない");
}

// D. 同じ場に 2 体の Agent（別々の persona）。それぞれのターンの system には**自分の**persona だけが載る
//    （他方の persona は混ざらない）。default_subject を差し替えて両者のターンを同じ場で回す。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn each_agent_in_a_place_sees_only_its_own_persona_in_system() {
    let h = build();
    let persona_a = "甲の人格___AMARK___";
    let persona_b = "乙の人格___BMARK___";
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "甲", persona_a, Standing::Trusted);
    let b = h
        .sys
        .create_subject(SubjectKind::Agent, "乙", persona_b, Standing::Trusted);
    let place = h.sys.create_place(
        Some("room:main"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(place, a, Role::Participant);
    h.sys.join(place, b, Role::Participant);
    bind(
        &h,
        place,
        gate("web", &[EffectKind::Say], vec![], vec![]),
        "room:main",
    );

    // 甲（default）のターン。
    h.eng.push(Step::no_reply());
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "やあ甲", "note-a"),
        )
        .unwrap();
    settle().await;
    // 既定を乙へ差し替えて、同じ場で乙のターンを回す（origin は別にする——同じ origin は重複として落ちる）。
    h.sys.set_policy(
        place,
        &Policy::immediate_on(&[Property::Direct]).with_default(b),
    );
    h.eng.push(Step::no_reply());
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("room:main", "npubX", "やあ乙", "note-b"),
        )
        .unwrap();
    settle().await;

    let systems = h.eng.systems();
    assert_eq!(systems.len(), 2, "2 ターン分の system");
    assert!(
        systems[0].starts_with(persona_a),
        "甲の system は甲の persona: {}",
        systems[0]
    );
    assert!(
        !systems[0].contains("___BMARK___"),
        "甲の system に乙の persona は混ざらない: {}",
        systems[0]
    );
    assert!(
        systems[1].starts_with(persona_b),
        "乙の system は乙の persona: {}",
        systems[1]
    );
    assert!(
        !systems[1].contains("___AMARK___"),
        "乙の system に甲の persona は混ざらない: {}",
        systems[1]
    );
}

// 1 応答に say ＋ PROGRESS ＋ ツール行を併記できる（行単位で独立解釈・既存どおり）。say は配送され、
// ツール行は実行され、PROGRESS は say として配送されない（進捗の揮発表示は activity 通知の側）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn say_progress_and_tool_line_coexist_in_one_response() {
    let h = build();
    let (_p, _a) = place_with(
        &h,
        Standing::Trusted,
        &[EffectKind::Say],
        vec![],
        vec![tool_one_string("gate-look", "q")],
    );
    h.host.set_immediate("gate-look", "みつけた");
    // 3 行: 地の文（say）・PROGRESS・ツール行。行ごとに独立解釈される。
    h.eng.push(Step::say_done(
        "調べてみるね\nPROGRESS::ねこを検索中\ngate-look::ねこ",
    ));
    h.eng.push(Step::no_reply()); // 決着から起きるターン
    wake(&h);
    settle().await;

    // ツール行は実行される。
    assert_eq!(h.host.invoke_count("gate-look"), 1, "ツール行は実行される");
    assert_eq!(
        h.host.last_args("gate-look"),
        Some(serde_json::json!({"q": "ねこ"})),
    );
    // 地の文は say として 1 本だけ配送される（PROGRESS 行は say にならない）。
    let says = h.tx.says();
    assert_eq!(
        says.len(),
        1,
        "say は 1 本（PROGRESS は say にならない）: {says:?}"
    );
    assert_eq!(says[0], "調べてみるね");
    assert!(
        says.iter().all(|s| !s.contains("検索中")),
        "PROGRESS 文言は say に混ざらない: {says:?}"
    );
}
