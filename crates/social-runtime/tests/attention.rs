//! DESIGN-attention.md のテスト節（着火の元栓と返答の絞り）。
//!
//! 推論は差し替えた偽物（ScriptedEngine）で回す。時間は tokio::time::pause() で進める。
//! 元栓（§1）は外界ゲート経路（`deliver_event`）で、絞り（§2）は着火作者の会計で測る。

use opencrab_engine::*;
use opencrab_port::*;
use opencrab_social_runtime::*;
use opencrab_store::Store;
use std::sync::Arc;
use std::time::Duration;

struct Harness {
    sys: System,
    eng: ScriptedEngine,
}

const TEST_MODEL: &str = "scripted";

/// 会話予算を測らないテスト向け（元栓は予算に依存しない）。o200k ではなく CharCounter を差す
/// （1 文字 = 1 トークン——絞りの閾値の意味がトークナイザ内部に左右されないため）。
fn build(cfg: Config) -> Harness {
    build_win(cfg, 1_000_000)
}

fn build_win(cfg: Config, context_window: i64) -> Harness {
    let store = Store::new_in_memory().unwrap();
    store
        .register_model_context_window(TEST_MODEL, context_window)
        .unwrap();
    let eng = ScriptedEngine::new();
    let host = ScriptedToolHost::new();
    let notif = RecordingNotifier::new();
    let sys = System::new(
        store,
        Arc::new(eng.clone()),
        Arc::new(host.clone()),
        Arc::new(ScriptedShellHost::new()),
        Arc::new(notif.clone()),
        Arc::new(CharCounter),
        cfg,
    );
    Harness { sys, eng }
}

async fn settle() {
    for _ in 0..400 {
        tokio::task::yield_now().await;
    }
}

const GATE: &str = "nostr";
const ADDR: &str = "wss://relay/pub";

/// 外界ゲートから届く said（作者は外界識別子）。origin つき（重複畳みの対象）。
fn said_event(author_external: &str, text: &str, origin: &str) -> GateEvent {
    GateEvent {
        kind: EventKind::Said,
        address: ADDR.to_string(),
        author_external: author_external.to_string(),
        author_display: None,
        content: Content::text(text),
        mentions: vec![],
        reply_to: None,
        target: None,
        origin: Some(origin.to_string()),
        attachments: vec![],
        discovery: None,
    }
}

/// 発火する場（Direct 即応・既定はエージェント）と、外界ゲートのチャネルを 1 本用意する。
/// (place, agent) を返す。owner は Standing::Owner の主体として nostr 素性つきで作る。
fn firing_place_with_channel(h: &Harness, owner_external: &str) -> (i64, i64) {
    let agent = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let owner = h
        .sys
        .create_subject(SubjectKind::Human, "O", "O", Standing::Owner);
    h.sys.add_identity(owner, GATE, owner_external);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(agent),
        None,
    );
    h.sys.join(p, agent, Role::Participant);
    // 外界ゲートのチャネルを結ぶ（deliver_event の place_for_channel が解決できるように）。
    h.sys
        .store()
        .add_channel(p, &GateName::new(GATE), ADDR)
        .unwrap();
    (p, agent)
}

// ---- §1 着火の元栓 ----

// 元栓未設定（同期なし）では従来どおり全通し——「許可集合が源」で、設定必須ではない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn inactive_gate_admits_everyone() {
    let h = build(Config::default());
    let (p, _agent) = firing_place_with_channel(&h, "ownerpk");
    h.eng.push(Step::say_done("hi"));

    // 見知らぬ作者でも、元栓が未設定なら通る（従来動作を壊さない）。
    let out = h
        .sys
        .deliver_event(&GateName::new(GATE), said_event("stranger", "hello", "o1"))
        .unwrap();
    assert!(out.is_some(), "元栓未設定なら記録される");
    settle().await;
    assert_eq!(h.eng.call_count(), 1, "元栓未設定ならターンが着火する");
    assert_eq!(h.sys.fire_drop_count(), 0);
    let _ = p;
}

// 許可集合を同期したあと: 未許可作者は store に残らず・着火もしない。許可作者・owner は従来どおり。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn gate_drops_unauthorized_admits_followee_and_owner() {
    let h = build(Config::default());
    let (p, _agent) = firing_place_with_channel(&h, "ownerpk");
    let gate = GateName::new(GATE);

    // フォローリスト同期（ゲートが事実として配送）。owner は core が合成（常に含まれる）。
    h.sys
        .sync_firing_followees(vec!["followee_pk".into()])
        .unwrap();

    let before = h.sys.store().latest_seq(p).unwrap();

    // 未許可作者 → 捨てる。Ok(None)・store 不変・着火なし・揮発カウンタだけ増える。
    let out = h
        .sys
        .deliver_event(&gate, said_event("stranger_pk", "math proof please", "s1"))
        .unwrap();
    settle().await;
    assert!(out.is_none(), "未許可作者は捨てる（seq を返さない）");
    assert_eq!(
        h.sys.store().latest_seq(p).unwrap(),
        before,
        "捨てた出来事は store に残らない"
    );
    assert_eq!(h.eng.call_count(), 0, "捨てた出来事でターンは着火しない");
    assert_eq!(h.sys.fire_drop_count(), 1, "揮発カウンタだけ数える");

    // 許可作者（フォロイー） → 従来どおり記録・着火。
    h.eng.push(Step::say_done("reply-followee"));
    let out = h
        .sys
        .deliver_event(&gate, said_event("followee_pk", "hi", "f1"))
        .unwrap();
    settle().await;
    assert!(out.is_some(), "フォロイーは記録される");
    assert_eq!(h.eng.call_count(), 1, "フォロイーはターンを着火する");

    // owner → フォロイー集合に無くても常に通る（core が合成・特例コード無し）。
    h.eng.push(Step::say_done("reply-owner"));
    let out = h
        .sys
        .deliver_event(&gate, said_event("ownerpk", "hi", "ow1"))
        .unwrap();
    settle().await;
    assert!(out.is_some(), "owner は常に許可");
    assert_eq!(h.eng.call_count(), 2, "owner はターンを着火する");
    // 未許可 1 件を捨てたきり、増えていない。
    assert_eq!(h.sys.fire_drop_count(), 1);
}

// フォローリスト同期で許可集合が更新される（追加・削除の両方向）。owner は常に不変で通る。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn follow_sync_updates_allow_set_both_directions() {
    let h = build(Config::default());
    let (_p, _agent) = firing_place_with_channel(&h, "ownerpk");
    let gate = GateName::new(GATE);

    // 追加方向: alice を同期 → alice 通る。
    h.sys.sync_firing_followees(vec!["alice".into()]).unwrap();
    assert!(h
        .sys
        .deliver_event(&gate, said_event("alice", "hi", "a1"))
        .unwrap()
        .is_some());
    settle().await;

    // 削除方向: 空へ同期 → alice はもう通らない（前のフォロイーが落ちる）。
    h.sys.sync_firing_followees(vec![]).unwrap();
    assert!(
        h.sys
            .deliver_event(&gate, said_event("alice", "hi again", "a2"))
            .unwrap()
            .is_none(),
        "フォロー解除された作者は捨てる"
    );
    // owner は同期内容に関わらず常に通る。
    h.eng.push(Step::say_done("ok"));
    assert!(
        h.sys
            .deliver_event(&gate, said_event("ownerpk", "hi", "ow2"))
            .unwrap()
            .is_some(),
        "owner は同期内容に関わらず常に通る"
    );

    // 別の相手を同期 → alice は依然だめ・bob は通る。
    h.sys.sync_firing_followees(vec!["bob".into()]).unwrap();
    assert!(h
        .sys
        .deliver_event(&gate, said_event("alice", "x", "a3"))
        .unwrap()
        .is_none());
    h.eng.push(Step::say_done("hi-bob"));
    assert!(h
        .sys
        .deliver_event(&gate, said_event("bob", "y", "b1"))
        .unwrap()
        .is_some());
}

// 経路網羅: said・メンション・リプライのどの着火経路も同じ元栓判定を通る（黙って通る箇所を作らない）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn all_firing_shapes_pass_through_the_same_gate() {
    let h = build(Config::default());
    let (p, agent) = firing_place_with_channel(&h, "ownerpk");
    let gate = GateName::new(GATE);
    // agent の nostr 素性（メンション先の解決に使う）。
    h.sys.add_identity(agent, GATE, "agent_pk");
    h.sys
        .sync_firing_followees(vec!["followee_pk".into()])
        .unwrap();

    let before = h.sys.store().latest_seq(p).unwrap();

    // (1) 素の said。
    let said = said_event("stranger_pk", "plain", "e1");
    // (2) メンション（agent を名指す）——別の口だが同じ判定を通るべき。
    let mut mention = said_event("stranger_pk", "hey @agent", "e2");
    mention.mentions = vec!["agent_pk".into()];
    // (3) リプライ（target/reply_to つき）。
    let mut reply = said_event("stranger_pk", "re:", "e3");
    reply.reply_to = Some("agent_pk".into());

    for ev in [said, mention, reply] {
        let out = h.sys.deliver_event(&gate, ev).unwrap();
        assert!(out.is_none(), "未許可作者はどの着火形でも捨てる");
    }
    settle().await;
    assert_eq!(
        h.sys.store().latest_seq(p).unwrap(),
        before,
        "どの着火形も store に残さない"
    );
    assert_eq!(h.eng.call_count(), 0, "どの着火形もターンを着火しない");
    assert_eq!(
        h.sys.fire_drop_count(),
        3,
        "3 形すべてが同じ元栓で数えられる"
    );
}

// ---- §2 返答の絞り ----

fn throttle_cfg(threshold: i64) -> Config {
    Config {
        throttle: Some(ThrottleConfig {
            window: Duration::from_secs(3600), // 十分広い窓（時間は pause 中でほぼ 0）
            threshold_tokens: threshold,
            reduced_max_output_tokens: 64,
            reduced_effort: Effort::Low,
        }),
        ..Config::default()
    }
}

// 高消費の着火作者の着火ターンで、短文指示・出力上限・努力ヒントが 3 点で組まれる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn high_consumption_author_gets_throttled() {
    let h = build(throttle_cfg(10));
    let agent = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    // 非オーナーの相手（会計対象）。
    let peer = h
        .sys
        .create_subject(SubjectKind::Human, "P", "P", Standing::Unknown);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(agent),
        None,
    );
    h.sys.join(p, agent, Role::Participant);

    // 1 本目: 過去消費ゼロ → 絞られない。
    h.eng.push(Step::say_done("r1"));
    h.sys
        .deliver(p, Incoming::said(peer, "long expensive request one"))
        .unwrap();
    settle().await;

    // 2 本目以降: 1 本目の消費（context_records の prompt_tokens・CharCounter で十分 >10）で閾値超え。
    h.eng.push(Step::say_done("r2"));
    h.sys.deliver(p, Incoming::said(peer, "another")).unwrap();
    settle().await;

    let throttles = h.eng.throttles();
    assert_eq!(throttles.len(), 2, "2 ターン走った");
    assert!(throttles[0].is_none(), "1 本目は過去消費ゼロで絞られない");
    let t = throttles[1].expect("2 本目は高消費で絞られる");
    assert_eq!(
        t.max_output_tokens,
        Some(64),
        "出力トークン上限が config の値に絞られる"
    );
    assert_eq!(t.effort, Effort::Low, "努力ヒントが下がる");
    // 生成点への短文指示が rendered 末尾に入る。
    let last = h.eng.last_context().unwrap();
    assert!(
        last.contains(THROTTLE_HINT.trim()),
        "絞りターンの文脈に短文指示が入る: {last:?}"
    );
}

// オーナーは窓消費に関わらず素通し（会計対象外・常に無制限）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn owner_is_never_throttled() {
    let h = build(throttle_cfg(1)); // 閾値 1（すぐ超える設定でも owner は絞られない）
    let agent = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let owner = h
        .sys
        .create_subject(SubjectKind::Human, "O", "O", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(agent),
        None,
    );
    h.sys.join(p, agent, Role::Participant);

    // owner から何度も着火させ、消費を積み上げる。
    for i in 0..3 {
        h.eng.push(Step::say_done(&format!("r{i}")));
        h.sys
            .deliver(p, Incoming::said(owner, "big expensive owner request"))
            .unwrap();
        settle().await;
    }

    let throttles = h.eng.throttles();
    assert_eq!(throttles.len(), 3, "3 ターン走った");
    assert!(
        throttles.iter().all(|t| t.is_none()),
        "owner はどのターンも絞られない（会計対象外）: {throttles:?}"
    );
}

// 絞りが config 未設定（None）なら、高消費でも絞られない（オプトイン）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn throttle_is_opt_in() {
    let h = build(Config::default()); // throttle: None
    let agent = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let peer = h
        .sys
        .create_subject(SubjectKind::Human, "P", "P", Standing::Unknown);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(agent),
        None,
    );
    h.sys.join(p, agent, Role::Participant);

    for i in 0..2 {
        h.eng.push(Step::say_done(&format!("r{i}")));
        h.sys
            .deliver(p, Incoming::said(peer, "expensive request repeated"))
            .unwrap();
        settle().await;
    }

    assert!(
        h.eng.throttles().iter().all(|t| t.is_none()),
        "throttle 未設定なら絞らない（オプトイン）"
    );
}
