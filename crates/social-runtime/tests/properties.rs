//! 詳細設計 §13 のうち、チャネルを必要としない性質をテストで守る。
//! 機構で守るもの（型で閉じる 4 つ・§02）にはテストを書かない。
//!
//! 推論は差し替えた偽物（ScriptedEngine）で回す。時間は tokio::time::pause() で進める。

use opencrab_engine::*;
use opencrab_port::*;
use opencrab_social_runtime::*;
use opencrab_store::{NewEvent, Store};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

struct Harness {
    sys: System,
    eng: ScriptedEngine,
    host: ScriptedToolHost,
    notif: RecordingNotifier,
}

/// テストの実効モデル（ScriptedEngine が名乗る既定名）。ハーネスが store へ context_window を登録する。
const TEST_MODEL: &str = "scripted";
/// 既定の登録 context_window。compaction_ratio 既定 0.5 と掛けて会話予算 100_000（旧固定既定と同値）。
/// 予算に依存しないテストはこのまま `build` を使う。予算を測るテストは `build_win` で窓を指定する。
const DEFAULT_TEST_CONTEXT_WINDOW: i64 = 200_000;

fn build(cfg: Config) -> Harness {
    build_win(cfg, DEFAULT_TEST_CONTEXT_WINDOW)
}

/// 実効モデルの context_window を明示して組む（会話予算 = context_window × compaction_ratio・§06）。
/// CharCounter は 1 文字 = 1 トークンなので、`cfg.compaction_ratio = 1.0` にすれば window がそのまま
/// 会話予算になる（旧 `context_budget_tokens: N` の移行先）。
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
    Harness {
        sys,
        eng,
        host,
        notif,
    }
}

/// spawn されたタスクを進める（タイマ待ちは advance が別途進める）。
async fn settle() {
    for _ in 0..400 {
        tokio::task::yield_now().await;
    }
}

fn provision_gate_tools(h: &Harness, place: PlaceId, names: &[&str]) {
    let kind = GateName::new("test-gate");
    h.sys
        .register_gate(GateSpec {
            name: kind.clone(),
            protocol: PROTOCOL_VERSION,
            address_form: ".*".into(),
            tools: names
                .iter()
                .map(|name| ToolDef {
                    name: (*name).to_string(),
                    description: (*name).to_string(),
                    params: serde_json::json!({}),
                })
                .collect(),
            effects: Default::default(),
            capabilities: Default::default(),
            actions: Vec::new(),
        })
        .unwrap();
    h.sys
        .provision_channel(place, kind.as_str(), &format!("place:{place}"))
        .unwrap();
}

fn edited(author: SubjectId, target: Seq, text: &str) -> Incoming {
    Incoming {
        kind: EventKind::Edited,
        author_subject: Some(author),
        author_external: None,
        content: Content::text(text),
        mentions: vec![],
        reply_to: None,
        target: Some(target),
    }
}

fn retracted(author: SubjectId, target: Seq) -> Incoming {
    Incoming {
        kind: EventKind::Retracted,
        author_subject: Some(author),
        author_external: None,
        content: Content::default(),
        mentions: vec![],
        reply_to: None,
        target: Some(target),
    }
}

// 1. 後続ターンが先行の発話を見る。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn later_turn_sees_prior_utterance() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    h.eng.push(Step::say_done("reply1"));
    h.eng.push(Step::say_done("reply2"));

    h.sys.deliver(p, Incoming::said(human, "first")).unwrap();
    settle().await;
    h.sys.deliver(p, Incoming::said(human, "second")).unwrap();
    settle().await;

    let ctxs = h.eng.contexts();
    assert!(ctxs.len() >= 2, "two turns expected, got {}", ctxs.len());
    assert!(
        ctxs[1].contains("reply1"),
        "2本目の文脈に1本目の発話が入るべき: {}",
        ctxs[1]
    );
}

// 2. 即応が走っているターンを早期に終わらせる（割り込み）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn immediate_interrupts_running_turn() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    h.eng
        .push(Step::no_reply_cont().gated(gate.clone(), entered.clone())); // 走っている推論
    h.eng.push(Step::no_reply());

    h.sys.deliver(p, Incoming::said(human, "start")).unwrap();
    entered.notified().await; // ターンが推論に入った
    h.sys.deliver(p, Incoming::said(human, "stop")).unwrap(); // 即応 → 早期終了要求
    gate.notify_one();
    settle().await;

    let recs = h.sys.store().turn_records(p).unwrap();
    assert!(!recs.is_empty());
    assert_eq!(recs[0].end_reason, "interrupted", "早期終了で終わるべき");
    assert_eq!(recs[0].iterations, 1, "反復が打ち切られる");
}

// steer: 他の場への発話が、走っているターンを早期に終わらせる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn steer_via_effect_to_other_place() {
    let h = build(Config::default());
    let pa = h
        .sys
        .create_subject(SubjectKind::Agent, "PA", "PA", Standing::Trusted);
    let ca = h
        .sys
        .create_subject(SubjectKind::Agent, "CA", "CA", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);

    let pp = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(pa),
        None,
    );
    let cc = h.sys.create_place(
        Some("cc"),
        Some(pp),
        &Policy::immediate_on(&[Property::Direct, Property::MentionsMe]).with_default(ca),
        None,
    );
    h.sys.join(pp, pa, Role::Participant);
    h.sys.join(pp, human, Role::Participant);
    h.sys.join(cc, ca, Role::Participant);
    h.sys.join(cc, pa, Role::Participant); // PA は CC にも参加（他の場へ発話できる）

    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    h.eng
        .push(Step::no_reply_cont().gated(gate.clone(), entered.clone())); // CA の長いターン
    let steer = EffectSpec {
        kind: EffectKind::Say,
        place: Some(cc),
        target: None,
        content: Content::text("向きを変えて"),
        mentions: vec![ca],
        verb: None,
    };
    h.eng.push(Step::done().with_effect(steer)); // PA が CC へ発話
    h.eng.push(Step::no_reply()); // steer を読んで CA が起きる 2 本目のターン

    h.sys.deliver(cc, Incoming::said(pa, "go")).unwrap(); // CA ターン起動（author pa → default ca）
    entered.notified().await;
    h.sys.deliver(pp, Incoming::said(human, "work")).unwrap(); // PA ターン起動
    settle().await; // PA が steer 効果を出す → CC へ早期終了要求
    gate.notify_one();
    settle().await;

    let recs = h.sys.store().turn_records(cc).unwrap();
    assert!(!recs.is_empty());
    assert_eq!(
        recs[0].end_reason, "interrupted",
        "他の場への発話で CA のターンが早期終了するべき"
    );
}

// 4 & 3. 切り詰めが起き、読み位置は文脈に入った範囲までしか進まず、切り詰め範囲が記録に残る。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn truncation_records_skipped_and_read_position() {
    // 会話予算 40（CharCounter で 40 文字）。compaction_ratio=1.0 で window がそのまま予算になる。
    let cfg = Config {
        compaction_ratio: 1.0,
        ..Config::default()
    };
    let h = build_win(cfg, 40);
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    // 読み込み中は発火しない方針で溜める。
    let p = h.sys.create_place(None, None, &Policy::default(), None);
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    for i in 0..10 {
        h.sys
            .deliver(p, Incoming::said(human, &format!("msg{i:02} aaaaaaaa")))
            .unwrap();
    }
    // 方針を即応に変え、1 件で発火させる。
    h.sys.set_policy(
        p,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
    );
    h.eng.push(Step::no_reply());
    let trigger = h.sys.deliver(p, Incoming::said(human, "trigger")).unwrap();
    settle().await;

    let recs = h.sys.store().turn_records(p).unwrap();
    assert_eq!(recs.len(), 1);
    // 文脈の事実（範囲・切り詰め）は反復ごとの記録に置く（重複させない・§10）。この即応ターンは 1 反復。
    let crs = h.sys.store().context_records(recs[0].id).unwrap();
    assert_eq!(crs.len(), 1);
    let c0 = &crs[0];
    assert!(c0.skipped_from_seq.is_some(), "切り詰めが起きるべき");
    assert!(c0.skipped_to_seq.is_some());
    assert_eq!(c0.ctx_to_seq, Some(trigger), "文脈の末尾は最新");
    let read = h
        .sys
        .store()
        .get_membership(p, a)
        .unwrap()
        .unwrap()
        .read_seq;
    assert_eq!(
        read,
        c0.ctx_to_seq.unwrap(),
        "読み位置は文脈に入った範囲まで"
    );
    assert!(h.eng.contexts()[0].contains("省略"), "省略を文脈に明記する");
}

// 6 & 7. 引き継ぎが記録から再現でき、子の連番と読み位置を汚さない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn inheritance_reproducible_and_clean_child_seq() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);

    let pp = h.sys.create_place(None, None, &Policy::default(), None);
    h.sys.join(pp, human, Role::Participant);
    h.sys.deliver(pp, Incoming::said(human, "p1")).unwrap();
    h.sys.deliver(pp, Incoming::said(human, "p2")).unwrap();
    h.sys.deliver(pp, Incoming::said(human, "p3")).unwrap();
    let up_to = h.sys.store().latest_seq(pp).unwrap();
    assert_eq!(up_to, 3);

    let cc = h.sys.create_place(
        None,
        Some(pp),
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        Some((pp, up_to)),
    );
    h.sys.join(cc, a, Role::Participant);
    h.sys.join(cc, human, Role::Participant);

    h.eng.push(Step::no_reply());
    h.sys.deliver(cc, Incoming::said(human, "child1")).unwrap();
    settle().await;

    // 子の 1 件目は seq=1。
    let ev1 = h.sys.store().get_event(cc, 1).unwrap().unwrap();
    assert_eq!(ev1.seq, 1);
    assert_eq!(ev1.content.text.as_deref(), Some("child1"));

    // 2 値（親の場・どこまでの連番）が記録され、そこから内容を再現できる。
    let cc_row = h.sys.store().get_place(cc).unwrap().unwrap();
    assert_eq!(cc_row.inherit_from_place, Some(pp));
    assert_eq!(cc_row.inherit_up_to_seq, Some(3));
    let recs = h.sys.store().turn_records(cc).unwrap();
    assert_eq!(recs[0].inherit_to_seq, Some(3));
    // 再現: 2 値から親ログ [1,3] を読み直すと同じ内容。
    let reproduced = h.sys.store().read_range(pp, 0, 3).unwrap();
    assert_eq!(reproduced.len(), 3);
    let ctx = &h.eng.contexts()[0];
    for m in ["p1", "p2", "p3"] {
        assert!(ctx.contains(m), "引き継いだ内容が文脈に入る: {ctx}");
    }

    // 子の読み位置は子の連番（=1）で、親の 3 に汚されない。
    let read = h
        .sys
        .store()
        .get_membership(cc, a)
        .unwrap()
        .unwrap()
        .read_seq;
    assert_eq!(read, 1);
}

// 8. 親の文脈に子の中身が入らない（識別子・題・状態だけ）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn parent_context_excludes_child_content() {
    let h = build(Config::default());
    let pa = h
        .sys
        .create_subject(SubjectKind::Agent, "PA", "PA", Standing::Trusted);
    let ca = h
        .sys
        .create_subject(SubjectKind::Agent, "CA", "CA", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);

    let pp = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(pa),
        None,
    );
    h.sys.join(pp, pa, Role::Participant);
    h.sys.join(pp, human, Role::Participant);
    // 子は発火しない方針（子ターンを起こさないため）。
    let cc = h
        .sys
        .create_place(Some("cc"), Some(pp), &Policy::default(), None);
    h.sys.join(cc, ca, Role::Participant);
    h.sys
        .deliver(cc, Incoming::said(ca, "SECRET_CHILD_TEXT"))
        .unwrap();

    h.eng.push(Step::no_reply());
    h.sys
        .deliver(pp, Incoming::said(human, "parent msg"))
        .unwrap();
    settle().await;

    let ctx = &h.eng.contexts()[0];
    assert!(!ctx.contains("SECRET_CHILD_TEXT"), "子の中身は入らない");
    assert!(ctx.contains(&format!("子 #{cc}")), "子の識別子は入る");
}

// 9. まとめが固定窓で 1 ターンになる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn batch_fires_once_on_fixed_window() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::default()
            .with_batch_ms(20 * 60 * 1000)
            .with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    h.eng.push(Step::no_reply());
    h.sys.deliver(p, Incoming::said(human, "m1")).unwrap();
    settle().await;
    assert_eq!(h.eng.call_count(), 0, "窓の前は撃たない");
    h.sys.deliver(p, Incoming::said(human, "m2")).unwrap(); // 窓を動かさない
    settle().await;
    assert_eq!(h.eng.call_count(), 0);

    tokio::time::advance(Duration::from_secs(20 * 60)).await;
    settle().await;

    let recs = h.sys.store().turn_records(p).unwrap();
    assert_eq!(recs.len(), 1, "固定窓で 1 ターン");
    let ctx = &h.eng.contexts()[0];
    assert!(
        ctx.contains("m1") && ctx.contains("m2"),
        "未読全部で 1 ターン"
    );
}

// 9b. batch は自分の発話だけでは再発火しない（自己ループ防止・§5.5）。
//     直前のターンで自分が残した spoke が read より先にあるだけの状態では、窓が明けても新しいターンを
//     起こさない（batch_fire が著者自身の出来事を発火理由に数えない・即応経路の targets と同じ規則）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn batch_does_not_refire_on_own_utterance_only() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let win = Duration::from_secs(10);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::default()
            .with_batch_ms(win.as_millis() as i64)
            .with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    // 1 ターン目は実際に発話する（自分の spoke が read より先に積まれる状況を作る）。
    h.eng.push(Step::say_done("うけとった"));
    h.sys.deliver(p, Incoming::said(human, "m1")).unwrap();
    settle().await;
    assert_eq!(h.eng.call_count(), 0, "窓の前は撃たない");

    // 窓明け → 1 ターン。エージェントが話し、その spoke が batch を再武装する。
    tokio::time::advance(win).await;
    settle().await;
    assert_eq!(h.eng.call_count(), 1, "溜まった他者の発話で 1 ターン");
    assert_eq!(h.sys.store().turn_records(p).unwrap().len(), 1);

    // さらに窓を何度跨いでも、未読が自分の spoke だけなら発火しない（自己ループが起きない）。
    tokio::time::advance(win * 3).await;
    settle().await;
    assert_eq!(h.eng.call_count(), 1, "自分の発話だけでは再発火しない");
    assert_eq!(
        h.sys.store().turn_records(p).unwrap().len(),
        1,
        "自己ループが起きない（ターンは 1 本のまま）"
    );
}

// 9c. 自分の発話の後でも、他者の新しい出来事が来れば従来どおり発火する（過剰抑制でないこと）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn batch_still_fires_on_others_after_own_utterance() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let win = Duration::from_secs(10);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::default()
            .with_batch_ms(win.as_millis() as i64)
            .with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    h.eng.push(Step::say_done("いち"));
    h.eng.push(Step::say_done("に"));
    h.sys.deliver(p, Incoming::said(human, "m1")).unwrap();
    tokio::time::advance(win).await;
    settle().await;
    assert_eq!(h.eng.call_count(), 1, "1 ターン目");

    // 自分の spoke が未読に残る状態で、他者の新しい発話が来る → 従来どおり発火する。
    h.sys.deliver(p, Incoming::said(human, "m2")).unwrap();
    tokio::time::advance(win).await;
    settle().await;
    assert_eq!(
        h.eng.call_count(),
        2,
        "他者の新しい発話では従来どおり発火する（過剰抑制でない）"
    );
    assert_eq!(h.sys.store().turn_records(p).unwrap().len(), 2);
}

// 10. 無条件が溜まりゼロでも撃つ。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unconditional_fires_with_empty_backlog() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::default()
            .with_unconditional_ms(30 * 60 * 1000)
            .with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);

    h.eng.push(Step::no_reply());
    assert_eq!(h.eng.call_count(), 0);
    tokio::time::advance(Duration::from_secs(30 * 60)).await;
    settle().await;

    assert!(h.eng.call_count() >= 1, "溜まりゼロでも撃つ");
    assert!(!h.sys.store().turn_records(p).unwrap().is_empty());
}

// 11 & 13. ターンの上限が切れても背景が生き、決着から起きるターンの主体が同じ。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn background_survives_and_settles_same_subject() {
    let cfg = Config {
        bg_cap: Duration::from_secs(600),
        ..Config::default()
    };
    let h = build(cfg);
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);
    provision_gate_tools(&h, p, &["gate-long"]);

    h.host
        .set_slow("gate-long", Duration::from_secs(120), "SLOWRESULT");
    h.eng.push(Step::done().with_tool("gate-long")); // ターン: ツール呼び出し → 即受理 → done
    h.eng.push(Step::no_reply()); // 決着から起きるターン

    h.sys.deliver(p, Incoming::said(human, "do it")).unwrap();
    settle().await; // 常時切り離し: ツールは即座に背景へ移り、ターンは done で終わる（30 秒窓は無い）
                    // 即受理の確認: 時間を進める前に、背景の活動が走っている（切り離しは即座）。
    let running_bg = h
        .sys
        .store()
        .all_activities()
        .unwrap()
        .into_iter()
        .find(|x| x.kind == ActivityKindTag::Background && x.ended_at.is_none());
    assert!(
        running_bg.is_some(),
        "ツールは即座に背景へ移る（閾値待ちが無い）"
    );

    tokio::time::advance(Duration::from_secs(120)).await; // 背景のツール完了
    settle().await;

    let acts = h.sys.store().all_activities().unwrap();
    let bg = acts
        .iter()
        .find(|x| x.kind == ActivityKindTag::Background)
        .expect("背景の活動があるべき");
    assert!(bg.ended_at.is_some(), "ターンが切れても背景は完走する");
    assert_eq!(bg.end_reason.as_deref(), Some("done"));

    // 決着イベントに**結果の実文字列**が載る（成功が判る・§15）——旧実装は固定文言で結果を捨てていた。
    let last = h.sys.store().latest_seq(p).unwrap();
    let settled = h
        .sys
        .store()
        .read_range(p, 0, last)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == EventKind::Settled)
        .expect("決着イベントがあるべき");
    let text = settled.content.text.unwrap_or_default();
    assert!(text.contains("SLOWRESULT"), "決着に結果が載る: {text}");
    assert!(text.contains("成功"), "成功/失敗が判る: {text}");

    // 決着から起きたターンの主体は同じ a。
    let recs = h.sys.store().turn_records(p).unwrap();
    assert!(recs.len() >= 2, "初回 + 決着の 2 ターン: {}", recs.len());
    assert!(recs.iter().all(|r| r.subject == a), "全ターンが同じ主体");
}

// 12. 続きの無い呼び出しを勝手に再実行しない（中断後に同じ呼び出しが起きない）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn interrupted_background_not_reexecuted() {
    let cfg = Config {
        bg_cap: Duration::from_secs(60),
        ..Config::default()
    };
    let h = build(cfg);
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);
    provision_gate_tools(&h, p, &["gate-forever"]);

    h.host
        .set_slow("gate-forever", Duration::from_secs(600), "never");
    h.eng.push(Step::done().with_tool("gate-forever"));
    h.eng.push(Step::no_reply()); // 決着（中断）から起きるターン: ツールを呼ばない

    h.sys.deliver(p, Incoming::said(human, "do it")).unwrap();
    settle().await; // 常時切り離し: 即座に背景へ（30 秒窓は無い）
    tokio::time::advance(Duration::from_secs(60)).await; // 背景の上限
    settle().await;

    let acts = h.sys.store().all_activities().unwrap();
    let bg = acts
        .iter()
        .find(|x| x.kind == ActivityKindTag::Background)
        .unwrap();
    assert_eq!(bg.end_reason.as_deref(), Some("deadline"), "中断として決着");
    assert_eq!(h.host.invoke_count("gate-forever"), 1, "勝手に再実行しない");
}

// 交錯（常時切り離しの核・§07/§15）: 発話A→返答+ツール開始（即受理）→（実行中に）発話B→B返答→
// 決着（結果内容入り）→結果を踏まえた発話、の並びを固定する。決着ターンの文脈に**結果の実文字列**が
// 現れることを assert する（旧実装は結果を捨てていたので、ここが本 PR の心臓）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn interleaved_utterances_and_settle_carry_result_into_context() {
    let cfg = Config {
        bg_cap: Duration::from_secs(600),
        ..Config::default()
    };
    let h = build(cfg);
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);
    provision_gate_tools(&h, p, &["slow"]);

    h.host
        .set_slow("slow", Duration::from_secs(100), "SLOWRESULT-XYZ");
    h.eng.push(Step::say_done("A-受けた").with_tool("slow")); // A ターン: 返答 + ツール開始（即受理）
    h.eng.push(Step::say_done("B-返答")); // B ターン（ツール実行中）
    h.eng.push(Step::say_done("結果を踏まえた")); // 決着から起きるターン

    // 発話 A → 返答 + ツール開始。
    h.sys.deliver(p, Incoming::said(human, "A")).unwrap();
    settle().await;
    // 即受理: 時間を進める前に slow が背景で走っている（B の発話を待たせない）。
    let running = h
        .sys
        .store()
        .all_activities()
        .unwrap()
        .into_iter()
        .find(|x| x.kind == ActivityKindTag::Background && x.ended_at.is_none());
    assert!(running.is_some(), "ツールは即座に背景で走る");

    // 実行中に発話 B → B に返答（ツールはまだ走っている）。
    h.sys.deliver(p, Incoming::said(human, "B")).unwrap();
    settle().await;
    let still = h
        .sys
        .store()
        .all_activities()
        .unwrap()
        .into_iter()
        .find(|x| x.kind == ActivityKindTag::Background)
        .unwrap();
    assert!(
        still.ended_at.is_none(),
        "B 応答の時点でツールはまだ走っている"
    );

    // ツール完了 → 決着（結果入り）→ 結果を踏まえた発話。
    tokio::time::advance(Duration::from_secs(100)).await;
    settle().await;

    // 決着ターンの文脈に**結果の実文字列**が現れる（本 PR の核の assert）。
    let ctxs = h.eng.contexts();
    let last = ctxs.last().expect("決着ターンの文脈");
    assert!(
        last.contains("SLOWRESULT-XYZ"),
        "決着ターンの文脈に結果の実文字列が載る: {last}"
    );

    // 並びの固定: A受け → B返答 → 結果を踏まえた、が発話ログに順に出ている。
    let latest = h.sys.store().latest_seq(p).unwrap();
    let says: Vec<String> = h
        .sys
        .store()
        .read_range(p, 0, latest)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::Spoke)
        .filter_map(|e| e.content.text)
        .collect();
    assert_eq!(
        says,
        vec![
            "A-受けた".to_string(),
            "B-返答".to_string(),
            "結果を踏まえた".to_string()
        ],
        "発話の並びが固定される"
    );
}

// 決着は成功/失敗が判る（§15）: 失敗したツールは「失敗」とエラー文字列を載せて決着する
// （旧実装は成否も結果も捨てていた）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn failed_background_settles_as_failure_with_error() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);
    provision_gate_tools(&h, p, &["no-such-tool"]);

    // 未登録のツールは host が Err（unknown tool）を返す（近いものへ寄せない）。常時切り離しで背景へ移り、
    // 失敗として決着する。
    h.eng.push(Step::done().with_tool("no-such-tool"));
    h.eng.push(Step::no_reply()); // 決着から起きるターン

    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await;

    let bg = h
        .sys
        .store()
        .all_activities()
        .unwrap()
        .into_iter()
        .find(|x| x.kind == ActivityKindTag::Background)
        .unwrap();
    assert_eq!(bg.end_reason.as_deref(), Some("failed"), "失敗として決着");

    let latest = h.sys.store().latest_seq(p).unwrap();
    let settled = h
        .sys
        .store()
        .read_range(p, 0, latest)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == EventKind::Settled)
        .expect("決着イベント");
    let text = settled.content.text.unwrap_or_default();
    assert!(text.contains("失敗"), "失敗が判る: {text}");
    assert!(text.contains("unknown tool"), "エラー文字列が載る: {text}");
}

// core-bg-stop（暴走 kill・§07）: 自分の背景の活動を止められる。停止として決着し、勝手に再実行しない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn bg_stop_kills_own_background_activity() {
    let cfg = Config {
        bg_cap: Duration::from_secs(600),
        ..Config::default()
    };
    let h = build(cfg);
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);
    provision_gate_tools(&h, p, &["runaway"]);

    // 走り続けるツールを切り離す。
    h.host
        .set_slow("runaway", Duration::from_secs(500), "never-seen");
    h.eng.push(Step::done().with_tool("runaway"));
    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await;

    // 走っている背景の活動 id を取り、それを止めるターンを組む（id は実行時に決まるので後から script）。
    let bg_id = h
        .sys
        .store()
        .all_activities()
        .unwrap()
        .into_iter()
        .find(|x| x.kind == ActivityKindTag::Background && x.ended_at.is_none())
        .expect("走っている背景")
        .id;

    h.eng.push(
        Step::done().with_tool_args("core-bg-stop", serde_json::json!({ "activity": bg_id })),
    );
    h.eng.push(Step::no_reply()); // 停止の決着から起きるターン

    h.sys.deliver(p, Incoming::said(human, "stop it")).unwrap();
    settle().await;

    let bg = h.sys.store().get_activity(bg_id).unwrap().unwrap();
    assert_eq!(bg.end_reason.as_deref(), Some("stopped"), "停止として決着");
    // ツールは 1 度だけ呼ばれた（勝手に再実行しない）。
    assert_eq!(h.host.invoke_count("runaway"), 1);
    // 停止が決着イベントに出る。
    let latest = h.sys.store().latest_seq(p).unwrap();
    let stopped_text = h
        .sys
        .store()
        .read_range(p, 0, latest)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::Settled)
        .filter_map(|e| e.content.text)
        .find(|t| t.contains("停止"));
    assert!(stopped_text.is_some(), "停止の決着が出る");
}

// 大きな結果の退避と読み（案 A・項目 4）: 決着本文は生データを載せず退避し、core-bg-read が
// 行範囲で読み返せる。read_offload は主体で絞るので、決着ターンの主体（= 退避の所有者）が読める。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn large_result_is_offloaded_and_read_back_by_bg_read() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);
    provision_gate_tools(&h, p, &["bigtool"]);

    // CharCounter では 1 文字 = 1 トークン。300 行（各 ~12 文字）で inline 上限 2,500 を超える。
    let big: String = (0..300)
        .map(|i| format!("LINE{i:05}zzzz"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(big.chars().count() > 2_500, "前提: 上限超え");
    h.host.set_immediate("bigtool", &big);

    h.eng.push(Step::done().with_tool("bigtool")); // A ターン: 大結果ツール → 即受理
    h.eng.push(Step::no_reply()); // 決着から起きるターン
    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await;

    // 決着本文は生データを 1 バイトも載せず、読み方（core-bg-read）を案内する。
    let latest = h.sys.store().latest_seq(p).unwrap();
    let notice = h
        .sys
        .store()
        .read_range(p, 0, latest)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::Settled)
        .filter_map(|e| e.content.text)
        .find(|t| t.contains("退避"))
        .expect("退避の決着");
    assert!(
        !notice.contains("LINE00000"),
        "生データが決着に載っている: {notice}"
    );
    assert!(
        notice.contains("core-bg-read"),
        "読み方の案内が無い: {notice}"
    );

    // 退避の活動 id を取り、core-bg-read で先頭 5 行を読み返すターンを組む。
    let bg_id = h
        .sys
        .store()
        .all_activities()
        .unwrap()
        .into_iter()
        .find(|x| x.kind == ActivityKindTag::Background)
        .unwrap()
        .id;
    h.eng.push(Step::cont().with_tool_args(
        "core-bg-read",
        serde_json::json!({ "activity": bg_id, "start_line": 1, "line_count": 5 }),
    ));
    h.eng.push(Step::no_reply()); // 読み結果を見て終える

    h.sys.deliver(p, Incoming::said(human, "read")).unwrap();
    settle().await;

    // 読み結果（実データ）が tool_result として会話に戻る。
    let hists = h.eng.histories();
    let last_hist = hists.last().expect("履歴");
    assert!(
        last_hist.contains("LINE00000"),
        "退避を core-bg-read で読み返せる: {last_hist}"
    );
    // 返り値は inline 上限未満（天井で構造的に守る）——全 300 行のうち一部だけ。
    assert!(
        !last_hist.contains("LINE00299"),
        "1 回の読みで全行を返さない（天井で切る）: {last_hist}"
    );
}

// 14. サブの場が親と同じことをできる（子の場が自分で起き、孫を作る）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sub_place_wakes_itself_and_creates_grandchild() {
    let h = build(Config::default());
    let ca = h
        .sys
        .create_subject(SubjectKind::Agent, "CA", "CA", Standing::Trusted);
    // 子の場: 無条件で自分から起きる。
    let cc = h.sys.create_place(
        Some("cc"),
        None,
        &Policy::default()
            .with_unconditional_ms(30 * 60 * 1000)
            .with_default(ca),
        None,
    );
    h.sys.join(cc, ca, Role::Participant);

    h.eng.push(
        Step::done().with_tool_args("core-create-place", serde_json::json!({"address":"gc"})),
    );

    tokio::time::advance(Duration::from_secs(30 * 60)).await;
    settle().await;

    assert!(
        !h.sys.store().turn_records(cc).unwrap().is_empty(),
        "無条件で自分から起きる"
    );
    let grandchildren = h.sys.store().child_places(cc).unwrap();
    assert_eq!(grandchildren.len(), 1, "孫を作れる");
    assert_eq!(grandchildren[0].address.as_deref(), Some("gc"));
}

// 15. 編集・取り消しがログを書き換えない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn edit_and_retract_do_not_rewrite_log() {
    let h = build(Config::default());
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(None, None, &Policy::default(), None);
    h.sys.join(p, human, Role::Participant);

    h.sys.deliver(p, Incoming::said(human, "original")).unwrap();
    let before = h.sys.store().get_event(p, 1).unwrap().unwrap();
    h.sys
        .deliver(p, edited(human, 1, "edited-content"))
        .unwrap();
    h.sys.deliver(p, retracted(human, 1)).unwrap();

    let after = h.sys.store().get_event(p, 1).unwrap().unwrap();
    assert_eq!(before.content, after.content, "前の行は変わらない");
    assert_eq!(h.sys.store().event_count(p).unwrap(), 3, "行数が増える");
}

// 16. 経過の表示に推論を 1 回も使わない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn progress_uses_no_inference() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    h.eng.push(Step::no_reply());
    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await;
    let base = h.eng.call_count();
    assert_eq!(base, 1);

    h.sys.emit_progress(p, 999, "3 件目を読んでいます");
    h.sys.emit_progress(p, 999, "4 件目を読んでいます");

    assert_eq!(h.eng.call_count(), base, "経過の更新で推論は増えない");
    assert_eq!(h.notif.count_progress(), 2);
}

// 16b. PROGRESS（2 つ目の core 共通語・進捗の揮発表示）: `PROGRESS::<文>` は say でもイベントでもない。
// activity progress 通知を出し、走行中ターンの activities.label を更新するが、会話ログには何も残さない
// （read_range に PROGRESS 文言が現れない・spoke イベントも増えない）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn progress_line_emits_notice_sets_label_and_keeps_log_clean() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    // 応答は PROGRESS 1 行だけ（say も明示アクションも無い）。
    h.eng
        .push(Step::say_done("PROGRESS::いま 3 件目を読んでいます"));
    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await;

    // activity progress 通知が 1 回出る（揮発配送の材料・§05）。
    assert_eq!(h.notif.count_progress(), 1, "PROGRESS 通知が 1 回出る");

    // 走行中ターンの活動（kind=Turn）の label が PROGRESS 文言に更新されている（記録は残る）。
    let acts = h.sys.store().all_activities().unwrap();
    let turn = acts
        .iter()
        .find(|r| r.kind == ActivityKindTag::Turn)
        .expect("ターンの活動があるべき");
    assert_eq!(
        turn.label.as_deref(),
        Some("いま 3 件目を読んでいます"),
        "activities.label が PROGRESS 文言に更新される"
    );

    // 会話ログは汚れない: PROGRESS は say でもイベントでもないので、場のログには人の発話だけ。
    // spoke（エージェント発話）イベントは 1 件も無く、PROGRESS 文言はどのイベント本文にも現れない。
    let last = h.sys.store().latest_seq(p).unwrap();
    let log = h.sys.store().read_range(p, 0, last).unwrap();
    assert!(
        log.iter().all(|e| e.kind != EventKind::Spoke),
        "PROGRESS は spoke を生まない: {log:?}"
    );
    assert!(
        log.iter().all(|e| !e
            .content
            .text
            .as_deref()
            .unwrap_or("")
            .contains("読んでいます")),
        "PROGRESS 文言は会話ログに現れない: {log:?}"
    );
}

// 16c. PROGRESS は NO_REPLY と独立。同じ応答に NO_REPLY と PROGRESS があるとき、残余 say は
// 配送されず（withheld）、PROGRESS は状態表示なので必ず出る（通知が出る・label も更新される）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn progress_fires_even_with_no_reply() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    // 地の文（内緒）＋ NO_REPLY ＋ PROGRESS。NO_REPLY で地の文は配送されず、PROGRESS は出る。
    h.eng.push(Step::say_done(
        "内緒の独り言\nNO_REPLY\nPROGRESS::下ごしらえ中",
    ));
    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await;

    // PROGRESS は NO_REPLY に関わらず出る（状態表示）。
    assert_eq!(h.notif.count_progress(), 1, "NO_REPLY でも PROGRESS は出る");
    let acts = h.sys.store().all_activities().unwrap();
    let turn = acts
        .iter()
        .find(|r| r.kind == ActivityKindTag::Turn)
        .expect("ターンの活動があるべき");
    assert_eq!(turn.label.as_deref(), Some("下ごしらえ中"));

    // 地の文は配送されず（NO_REPLY）ターン記録の withheld_text に残る。会話ログには出ない。
    let recs = h.sys.store().turn_records(p).unwrap();
    let rec = recs.last().expect("ターン記録があるべき");
    assert_eq!(rec.end_reason, "no_reply");
    assert_eq!(rec.withheld_text.as_deref(), Some("内緒の独り言"));
    let last = h.sys.store().latest_seq(p).unwrap();
    let log = h.sys.store().read_range(p, 0, last).unwrap();
    assert!(
        log.iter().all(|e| {
            let t = e.content.text.as_deref().unwrap_or("");
            !t.contains("内緒") && !t.contains("下ごしらえ")
        }),
        "地の文も PROGRESS 文言も会話ログに出ない: {log:?}"
    );
}

// 17. 知らない著者に権限が付かない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unknown_author_gets_no_authority() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let u = h
        .sys
        .create_subject(SubjectKind::Agent, "U", "U", Standing::Unknown);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, u, Role::Participant);

    // Unknown は場を作れない。Trusted は作れる。
    assert!(!h.sys.tool_allowed(p, u, "core-create-place"));
    assert!(h.sys.tool_allowed(p, a, "core-create-place"));

    // 名寄せに無い外界の著者 → 主体は付かない（権限ゼロ）。ログには載る。
    let seq = h
        .sys
        .deliver(
            p,
            Incoming {
                kind: EventKind::Said,
                author_subject: None,
                author_external: Some(("discord".into(), "ghost".into())),
                content: Content::text("hi"),
                mentions: vec![],
                reply_to: None,
                target: None,
            },
        )
        .unwrap();
    let ev = h.sys.store().get_event(p, seq).unwrap().unwrap();
    assert!(ev.author_subject.is_none(), "主体は付かない");
    // 名寄せに無い外界識別子は、どの主体にも解決しない（居ないことと引けなかったことを区別する）。
    // ※ 以前ここは存在しない主体 9999 の membership を見ており、機構の有無に関わらず常に真だった。
    assert!(
        h.sys
            .store()
            .resolve_subject(&GateName::new("discord"), "ghost")
            .unwrap()
            .is_none(),
        "名寄せに無い外界識別子は主体に解決しない"
    );
}

// 18. 再起動で中断が出来事になり、同じ主体のターンが起きる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn restart_turns_interruption_into_event_and_turn() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    // 走り残しの活動（クラッシュ時に走っていたターン）を直接残す。
    h.sys
        .store()
        .start_activity(p, a, ActivityKindTag::Turn, None, 0, 0, None)
        .unwrap();

    // 同じ store で「再起動」。engine は新しくして数え直す。
    let eng2 = ScriptedEngine::new();
    eng2.push(Step::no_reply());
    let sys2 = System::new(
        h.sys.store().clone(),
        Arc::new(eng2.clone()),
        Arc::new(ScriptedToolHost::new()),
        Arc::new(ScriptedShellHost::new()),
        Arc::new(RecordingNotifier::new()),
        Arc::new(CharCounter),
        Config::default(),
    );
    sys2.startup();
    settle().await;

    // 中断が出来事として載っている。
    let latest = sys2.store().latest_seq(p).unwrap();
    let evs = sys2.store().read_range(p, 0, latest).unwrap();
    let interrupted = evs
        .iter()
        .find(|e| e.kind == EventKind::Interrupted)
        .expect("中断が出来事になる");
    // その出来事の時刻は壁時計（兄弟の追記と桁を揃える・§10 の観測を汚さない）。
    assert!(
        interrupted.created_at > 1_600_000_000_000_000_000,
        "再起動の中断も壁時計で刻む: got {}",
        interrupted.created_at
    );
    // 同じ主体 a のターンが起きた。
    let recs = sys2.store().turn_records(p).unwrap();
    assert!(
        recs.iter().any(|r| r.subject == a),
        "同じ主体のターンが起きる"
    );
}

// 19. 親が閉じると、子の走っているターンが早期に終わる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn closing_place_interrupts_running_turn() {
    let h = build(Config::default());
    let ca = h
        .sys
        .create_subject(SubjectKind::Agent, "CA", "CA", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let cc = h.sys.create_place(
        Some("cc"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(ca),
        None,
    );
    h.sys.join(cc, ca, Role::Participant);
    h.sys.join(cc, human, Role::Participant);

    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    h.eng
        .push(Step::no_reply_cont().gated(gate.clone(), entered.clone()));

    h.sys.deliver(cc, Incoming::said(human, "go")).unwrap();
    entered.notified().await;
    h.sys.close_place(cc, "parent closed");
    gate.notify_one();
    settle().await;

    let recs = h.sys.store().turn_records(cc).unwrap();
    assert!(!recs.is_empty());
    assert_eq!(recs[0].end_reason, "interrupted", "閉じると早期終了する");
}

// 20. 推論に上限が掛かる: プロバイダがストールしても、ターンが枠を永久に握らない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn stalled_inference_hits_idle_cap() {
    let cfg = Config {
        idle_cap: Duration::from_secs(5),
        ..Config::default()
    };
    let h = build(cfg);
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    let gate = Arc::new(Notify::new()); // 二度と notify しない → 推論はストール
    let entered = Arc::new(Notify::new());
    h.eng
        .push(Step::cont().gated(gate.clone(), entered.clone()));

    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    entered.notified().await; // 推論に入った（＝ストール開始）
    tokio::time::advance(Duration::from_secs(6)).await; // アイドル上限を跨ぐ
    settle().await;

    let recs = h.sys.store().turn_records(p).unwrap();
    assert!(!recs.is_empty(), "記録は必ず書く");
    assert_eq!(
        recs[0].end_reason, "idle_timeout",
        "ストールはアイドル上限で終わる"
    );
    // 枠が解放されている（別のターンが取れる）。
    assert!(h.sys.store().running_activities().unwrap().is_empty());
}

// 20b. 長い生成は切らない: 断片が流れている限り（アイドルが取り直される限り）、
//      総時間がアイドル上限を超えても推論は切られず、正当に完了する（詳細§05）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn long_streaming_inference_is_not_cut() {
    let cfg = Config {
        idle_cap: Duration::from_secs(10),
        ..Config::default()
    };
    let h = build(cfg);
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    // 6 秒ごとに断片を 5 回（総 30 秒 > アイドル上限 10 秒、各間隔 6 秒 < 10 秒）。
    h.eng
        .push(Step::no_reply().with_chunks(vec![Duration::from_secs(6); 5]));

    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await; // 推論に入り、最初の断片待ちへ
    for _ in 0..5 {
        tokio::time::advance(Duration::from_secs(6)).await; // 次の断片（アイドルは取り直される）
        settle().await;
    }
    settle().await;

    let recs = h.sys.store().turn_records(p).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(
        recs[0].end_reason, "no_reply",
        "断片が流れている限り、総時間が上限を超えても切らない"
    );
}

// #13: content 欠落・空文字・空白だけの 3 形は、done の値に依らず失敗理由を残す。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn empty_inference_shapes_fail_loud_and_leave_reason() {
    let missing = EffectSpec {
        kind: EffectKind::Say,
        place: None,
        target: None,
        content: Content::default(),
        mentions: vec![],
        verb: None,
    };
    for (label, step) in [
        ("no effect", Step::done()),
        ("missing", Step::done().with_effect(missing)),
        ("empty", Step::say_done("")),
        ("whitespace", Step::say_done("  \n\t")),
    ] {
        let h = build(Config::default());
        let a = h
            .sys
            .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
        let human = h
            .sys
            .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
        let p = h.sys.create_place(
            None,
            None,
            &Policy::immediate_on(&[Property::Direct]).with_default(a),
            None,
        );
        h.sys.join(p, a, Role::Participant);
        h.sys.join(p, human, Role::Participant);

        h.eng.push(step);
        h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
        settle().await;

        let recs = h.sys.store().turn_records(p).unwrap();
        assert_eq!(recs.len(), 1, "{label}: 失敗記録が一つ残る");
        assert_eq!(recs[0].end_reason, "failed", "{label}");
        let detail = recs[0]
            .failure_detail
            .as_deref()
            .expect("意味的な空の理由が残る");
        assert!(detail.starts_with("empty_response:"), "{label}: {detail}");
        assert!(detail.contains("non-whitespace Say"), "{label}: {detail}");
        assert!(detail.contains("no other effect"), "{label}: {detail}");
        assert!(detail.contains("no tool call"), "{label}: {detail}");
        assert_eq!(h.eng.call_count(), 1, "{label}: retry しない");
        assert_eq!(
            h.sys.store().latest_seq(p).unwrap(),
            1,
            "{label}: 失敗印の EventKind を場へ追加しない"
        );

        let contexts = h.sys.store().context_records(recs[0].id).unwrap();
        assert_eq!(contexts.len(), 1, "{label}: 失敗反復の観測が残る");
        assert_eq!(contexts[0].iteration, recs[0].iterations);
    }
}

// #13: tool call は Say が空でも有効な一手。結果を同じターンの次の推論へ還流できる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn tool_call_without_say_is_not_an_empty_response() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    h.eng.push(Step::cont().with_tool("core-child-list"));
    h.eng.push(Step::no_reply());
    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await;

    let recs = h.sys.store().turn_records(p).unwrap();
    assert_eq!(h.eng.call_count(), 2, "tool result を還流して再推論する");
    assert_eq!(recs[0].iterations, 2);
    assert_eq!(recs[0].end_reason, "no_reply");
    assert_eq!(recs[0].failure_detail, None);
    assert!(
        !h.eng.histories()[1].is_empty(),
        "2 回目の推論は tool result を受け取る"
    );
}

// 21. 失敗した推論でも、EngineError の本文ごと記録される（§08「それでも記録は書く」）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn failed_inference_still_writes_record() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    h.eng.push(Step::fail_with("sentinel engine failure"));
    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await;

    let recs = h.sys.store().turn_records(p).unwrap();
    assert_eq!(recs.len(), 1, "失敗でも記録は 1 本書かれる");
    assert_eq!(recs[0].end_reason, "failed");
    assert_eq!(
        recs[0].failure_detail.as_deref(),
        Some("sentinel engine failure"),
        "EngineError 本文を欠落させず記録する"
    );
    assert_eq!(h.eng.call_count(), 1, "推論失敗を retry しない");
    let contexts = h.sys.store().context_records(recs[0].id).unwrap();
    assert_eq!(contexts.len(), 1);
    assert_eq!(
        contexts[0].iteration, recs[0].iterations,
        "turn_records と失敗反復のローカル prompt tokens を iteration で結べる"
    );
}

// 22. 場を閉じる権限は、引数で指定した対象の場に対して判定される（§02）。
//     非オーナーが無関係な場を閉じられない。自分の子は閉じられる。Owner はどこでも閉じられる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn close_place_authorized_against_target_place() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let owner = h
        .sys
        .create_subject(SubjectKind::Agent, "O", "O", Standing::Owner);

    // A と Owner が走る場。
    let p1 = h.sys.create_place(None, None, &Policy::default(), None);
    h.sys.join(p1, a, Role::Participant);
    h.sys.join(p1, owner, Role::Participant);

    let unrelated = h.sys.create_place(None, None, &Policy::default(), None); // 無関係（親なし）
    let child = h.sys.create_place(None, Some(p1), &Policy::default(), None); // p1 の子

    // A は Trusted で Owner でなく、unrelated の親でもない → 閉じられない。
    assert!(
        !h.sys.tool_call_allowed(
            p1,
            a,
            "core-close-place",
            serde_json::json!({"place": unrelated})
        ),
        "非オーナーは無関係な場を閉じられない"
    );
    // child の親は p1、A は p1 の参加者 → child の親として閉じられる。
    assert!(
        h.sys.tool_call_allowed(
            p1,
            a,
            "core-close-place",
            serde_json::json!({"place": child})
        ),
        "自分の子の場は閉じられる"
    );
    // Owner はどこでも閉じられる。
    assert!(
        h.sys.tool_call_allowed(
            p1,
            owner,
            "core-close-place",
            serde_json::json!({"place": unrelated})
        ),
        "Owner は無関係な場でも閉じられる"
    );
}

// 22b. エージェントが組んだ壊れた引数で core は落ちない（§15「外から来たもの → 失敗を返す」）。
//      壊れた発火方針で場を作ろうとしても、ツールは失敗を返し、場は作られず、core は生き続ける。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn bad_policy_arg_does_not_kill_core() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    // immediate_from が未知値の壊れた policy。以前は panic して core を殺していた。
    h.eng.push(Step::done().with_tool_args(
        "core-create-place",
        serde_json::json!({"address": "x", "policy": {"immediate_from": "bogus"}}),
    ));
    // 続けて別のターンが普通に回る（core が生きている証拠）。
    h.eng.push(Step::no_reply());

    h.sys.deliver(p, Incoming::said(human, "make it")).unwrap();
    settle().await;

    // 子は作られていない（壊れた引数で作らせない）。
    assert!(
        h.sys.store().child_places(p).unwrap().is_empty(),
        "壊れた引数では場を作らない"
    );
    // ターンは記録され、core は次のターンも回せる。
    let recs = h.sys.store().turn_records(p).unwrap();
    assert_eq!(recs[0].end_reason, "done", "落ちずにターンは終わる");
    h.sys.deliver(p, Incoming::said(human, "again")).unwrap();
    settle().await;
    assert!(
        h.sys.store().turn_records(p).unwrap().len() >= 2,
        "core は生きていて次のターンも回る"
    );
}

// 23. 常時切り離し（§07）: ツール呼び出しは即座に「受理（活動ID）」で同じターンへ戻り、**実結果は
//     後のターンの決着イベント**として会話へ入る。同じターンの history には受理だけが載り、実結果は
//     載らない（速いツールでも同じ——閾値で「速いものだけ同期」という経路を残さない）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn tool_receipt_same_turn_and_result_via_settle() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);
    provision_gate_tools(&h, p, &["echo"]);

    // 即返るツールでも常時切り離しの対象（速いものだけ同期、という経路は無い）。
    h.host.set_immediate("echo", "TOOLOUT_MARKER");
    h.eng.push(Step::cont().with_tool("echo")); // 反復1: echo を呼ぶ（まだ done でない）
    h.eng.push(Step::no_reply()); // 反復2: 受理を見て終える
    h.eng.push(Step::no_reply()); // 決着から起きるターン: 実結果を見る

    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await;

    // 同じターンの history には**受理（活動ID）**だけが入り、実結果（TOOLOUT_MARKER）は入らない。
    let hists = h.eng.histories();
    assert!(hists.len() >= 2, "2 反復回るべき: {}", hists.len());
    assert!(
        hists[1].contains("活動"),
        "受理（活動ID）が呼び出しと対で次の推論の会話に入る: {}",
        hists[1]
    );
    assert!(
        !hists[1].contains("TOOLOUT_MARKER"),
        "実結果は同じターンには載らない（常時切り離し）: {}",
        hists[1]
    );

    // 実結果は決着イベントとして**後のターンの文脈**に載る（§15）。
    let ctxs = h.eng.contexts();
    assert!(
        ctxs.iter().any(|c| c.contains("TOOLOUT_MARKER")),
        "実結果は決着で後のターンに載る: {ctxs:?}"
    );
}

// 24. 背景の活動の本数に上限が掛かる（§10）。上限を超える切り離しは失敗になる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn background_activities_are_capped() {
    let cfg = Config {
        bg_cap: Duration::from_secs(3600),
        bg_per_place: 1,
        ..Config::default()
    };
    let h = build(cfg);
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);
    provision_gate_tools(&h, p, &["t1", "t2"]);

    h.host.set_slow("t1", Duration::from_secs(300), "r1");
    h.host.set_slow("t2", Duration::from_secs(300), "r2");
    h.eng.push(Step::done().with_tool("t1"));
    h.eng.push(Step::done().with_tool("t2"));

    // ターン 1: t1 を即座に切り離す（背景 1 本目・常時切り離し）。t1 は 300 秒走り続けるので
    // 時間を進めなければ走行中のまま——上限を占める。
    h.sys.deliver(p, Incoming::said(human, "go1")).unwrap();
    settle().await;

    // ターン 2: t2 を切り離そうとするが、場ごと上限 1 に達している → 失敗（入口で断る・§07）。
    h.sys.deliver(p, Incoming::said(human, "go2")).unwrap();
    settle().await;

    let bg: Vec<_> = h
        .sys
        .store()
        .all_activities()
        .unwrap()
        .into_iter()
        .filter(|x| x.kind == ActivityKindTag::Background)
        .collect();
    assert_eq!(bg.len(), 1, "上限を超えて背景を作らない");
}

// 25. 予定の時刻は壁時計で永続化される（§04）。単調時計（起点からの経過）ではない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn schedule_is_persisted_in_wall_clock() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::default()
            .with_unconditional_ms(30 * 60 * 1000)
            .with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);

    let sched = h.sys.store().schedule_all().unwrap();
    assert_eq!(sched.len(), 1);
    let next_fire_at = sched[0].2;
    // 壁時計の nanos は UNIX エポックからの絶対値（>1e18 相当）。
    // 単調時計（起点からの 30 分 ≒ 1.8e12）ならこの下限を割る。
    assert!(
        next_fire_at > 1_600_000_000_000_000_000,
        "予定は壁時計で持つ（単調時計の起点相対ではない）: got {next_fire_at}"
    );
}

// 26. 2 体を指名すると、両方が順にターンを得る（§04）。枠は 1 つなので順に回る。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn two_mentions_each_get_a_turn_in_order() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let b = h
        .sys
        .create_subject(SubjectKind::Agent, "B", "B", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    // 指名（MentionsMe）で発火。既定の参加者は要らない — 指名が宛先を決める。
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::MentionsMe]),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, b, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    h.eng.push(Step::say_done("from-A"));
    h.eng.push(Step::say_done("from-B"));

    // 「@A @B どう思う？」相当。
    h.sys
        .deliver(p, Incoming::said(human, "hey").with_mentions(vec![a, b]))
        .unwrap();
    settle().await;

    let recs = h.sys.store().turn_records(p).unwrap();
    let subjects: Vec<_> = recs.iter().map(|r| r.subject).collect();
    assert!(
        subjects.contains(&a) && subjects.contains(&b),
        "両方が返す: {subjects:?}"
    );
    // 枠は 1 つなので順に回る。先に指名され、先に発火した A が先。
    assert_eq!(recs[0].subject, a, "A が先");
    assert_eq!(recs[1].subject, b, "次に B");
}

// 27. 「自分の場のログを範囲で読む」で、切り詰めで落ちた分を手に取れる（§12・§06 と対）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn read_log_recovers_skipped_range() {
    // 会話予算 60（CharCounter で 60 文字）。compaction_ratio=1.0 で window がそのまま予算になる。
    let cfg = Config {
        compaction_ratio: 1.0,
        ..Config::default()
    };
    let h = build_win(cfg, 60);
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    // まず溜める（既定方針は発火しない）。
    let p = h.sys.create_place(None, None, &Policy::default(), None);
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);
    for i in 0..6 {
        h.sys
            .deliver(p, Incoming::said(human, &format!("SEED{i} xxxxxxxx")))
            .unwrap();
    }
    // 即応に変えて 1 件で発火。予算が小さいので古い方は文脈から落ちる。
    h.sys.set_policy(
        p,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
    );
    // 1 反復目: 落ちた seq 1..2 を読む。2 反復目: 結果を見て終える。
    h.eng.push(
        Step::cont().with_tool_args("core-read-log", serde_json::json!({"from": 1, "to": 2})),
    );
    h.eng.push(Step::no_reply());

    h.sys.deliver(p, Incoming::said(human, "trigger")).unwrap();
    settle().await;

    let ctxs = h.eng.contexts();
    assert!(ctxs.len() >= 2, "2 反復回るべき");
    // 1 反復目の文脈（最初の user テキスト）では古い SEED0 は切り詰められている。
    assert!(
        !ctxs[0].contains("SEED0"),
        "SEED0 は文脈から落ちている: {}",
        ctxs[0]
    );
    // ツールで読み直した結果は、tool_result ブロックとして 2 反復目の会話に戻る（テキストに混ぜない・§05）。
    let hists = h.eng.histories();
    assert!(
        hists[1].contains("SEED0"),
        "落ちた分をツールで手に取れる（tool_result で還る）: {}",
        hists[1]
    );
}

// 28. 文脈の観測は反復ごとに残る（§10）。**組み直さず積む**新設計（§05）では、
//     文脈は最初に 1 度組み、反復ごとに会話が積み上がる——トークン数が反復ごとに増える。
//     切り詰めは最初の組み立てのもので、反復では変わらない（後から再切り詰めしない）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn context_records_capture_each_iteration() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    // 各反復で発話 → 会話に積まれる（ログにも載るが、ターン内は組み直さず積む・§05）。4 反復回す。
    for _ in 0..3 {
        h.eng
            .push(Step::cont().with_effect(EffectSpec::say("blah blah blah blah")));
    }
    h.eng.push(Step::no_reply());

    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await;

    let recs = h.sys.store().turn_records(p).unwrap();
    assert_eq!(recs.len(), 1);
    let turn = &recs[0];
    let crs = h.sys.store().context_records(turn.id).unwrap();
    // 反復数ぶんの観測がある。
    assert_eq!(crs.len() as i64, turn.iterations, "反復ごとに 1 行");
    assert_eq!(crs.len(), 4);
    // 反復番号が 1..=4 で並ぶ。
    assert_eq!(
        crs.iter().map(|c| c.iteration).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    // 会話が積み上がるので、トークン数は反復ごとに増える（丸ごと組み直していない証拠・§05）。
    assert!(crs.iter().all(|c| c.prompt_tokens > 0));
    assert!(
        crs[0].prompt_tokens < crs[3].prompt_tokens,
        "会話が積み上がり、後の反復ほどトークンが多い: {:?}",
        crs.iter().map(|c| c.prompt_tokens).collect::<Vec<_>>()
    );
    // 切り詰めは最初の組み立てのもの——反復で変わらない（後から再切り詰めしない・§05）。
    assert!(
        crs.iter()
            .all(|c| c.skipped_from_seq == crs[0].skipped_from_seq),
        "切り詰め範囲は反復で一定"
    );
}

// 29. 「子の発火方針を変える」ツール（§12）。親が子の間隔・即応の条件を調整できる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn set_policy_changes_child_policy() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    // A が走る親の場。
    let parent = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(parent, a, Role::Participant);
    h.sys.join(parent, human, Role::Participant);
    // 子（A は parent の参加者なので「子の親」）。最初は方針なし。
    let child = h
        .sys
        .create_place(None, Some(parent), &Policy::default(), None);

    // 無条件間隔を積むので default_subject を同梱する。これが無いと set-policy は fail loud で拒否する
    // （default_subject の無い場に無条件を武装すると発火時に黙って止まるため・整理の場の裁定 2）。
    let new_policy = serde_json::json!({
        "immediate": ["mentions_me"],
        "immediate_from": "anyone",
        "unconditional_interval_ms": 1_800_000i64,
        "default_subject": a,
    });
    h.eng.push(Step::done().with_tool_args(
        "core-set-policy",
        serde_json::json!({"place": child, "policy": new_policy}),
    ));

    h.sys.deliver(parent, Incoming::said(human, "go")).unwrap();
    settle().await;

    // 子の保存された方針が変わっている。
    let row = h.sys.store().get_place(child).unwrap().unwrap();
    let pol = Policy::from_json(&row.policy_json).unwrap();
    assert_eq!(pol.unconditional_interval_ms, Some(1_800_000));
    assert!(
        pol.immediate.contains(&Property::MentionsMe),
        "即応条件が変わる"
    );
}

// read（プロトコル§02）の上限: 超える指定は上限（READ_LIMIT_MAX）に丸める。
// 上限＋1 件を入れて、巨大な limit で読んでも上限ちょうどで切れ、続きに next が返ることを直接確かめる。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn read_log_rounds_limit_to_the_cap() {
    let h = build(Config::default());
    let gate = GateName::new("web");
    let addr = "room:main";
    let place = h
        .sys
        .create_place(Some(addr), None, &Policy::default(), None);
    // 住所 → 場を結ぶ（read はこの表で解決する）。
    h.sys.store().add_channel(place, &gate, addr).unwrap();

    // 上限 + 1 件を積む（発火する主体は居ないので、ただ並ぶ）。
    let n = READ_LIMIT_MAX + 1;
    for i in 0..n {
        let ne = NewEvent {
            kind: EventKind::Said,
            author_subject: None,
            author_external: Some("test-owner".into()),
            content: Content::text(format!("m{i}")),
            mentions: vec![],
            reply_to: None,
            target: None,
            for_subject: None,
            attachments: vec![],
        };
        h.sys.store().append(place, &ne, i).unwrap();
    }

    // 巨大な limit → 上限に丸められ、ちょうど READ_LIMIT_MAX 件。続きがあるので next=上限+1。
    let page = h.sys.read_log(&gate, addr, 1, 1_000_000).expect("read ok");
    assert_eq!(
        page.events.len() as i64,
        READ_LIMIT_MAX,
        "超える指定は上限に丸める（§02）"
    );
    assert_eq!(
        page.next,
        Some(READ_LIMIT_MAX + 1),
        "続きがあれば next（次の from）"
    );

    // 残り 1 件を読む → 尽きるので next 無し。
    let tail = h
        .sys
        .read_log(&gate, addr, READ_LIMIT_MAX + 1, 1_000_000)
        .expect("read ok");
    assert_eq!(tail.events.len(), 1);
    assert!(tail.next.is_none(), "尽きたら next は返らない（§02）");

    // 巨大な from（外から来る値）でも溢れて落ちない——空を返すだけ（§00/§15）。
    let huge = h
        .sys
        .read_log(&gate, addr, i64::MAX, 1_000_000)
        .expect("no panic");
    assert!(huge.events.is_empty() && huge.next.is_none());

    // 結んでいない住所は NotBound（§02）。
    assert!(
        matches!(
            h.sys.read_log(&gate, "room:other", 1, 10),
            Err(ReadReject::NotBound)
        ),
        "結んでいない住所は not_bound"
    );
}

// 28. 会話予算は「実効モデルの context_window × compaction_ratio」で決まる（§06）。
//     未登録モデルは fail loud——既定値へ寄せない（本体 #412 の流儀・§15）。
#[test]
fn context_budget_is_window_times_ratio_and_unregistered_fails_loud() {
    let store = Store::new_in_memory().unwrap();
    // 登録済み: window 1_000 × 0.5 = 500。
    store.register_model_context_window("m-500", 1_000).unwrap();
    assert_eq!(
        opencrab_social_runtime::resolve_context_budget_tokens(&store, "m-500", 0.5).unwrap(),
        500,
        "会話予算 = context_window × compaction_ratio"
    );
    // 未登録は Err（既定値へ落とさない）。
    let err = opencrab_social_runtime::resolve_context_budget_tokens(&store, "m-unknown", 0.5)
        .expect_err("未登録は fail loud");
    assert!(
        err.contains("no context_window registered"),
        "登録の仕方まで案内するメッセージ: {err}"
    );
    // 非正値（0）も未登録扱い（予算が消えるのを通さない）。
    store.register_model_context_window("m-zero", 0).unwrap();
    assert!(opencrab_social_runtime::resolve_context_budget_tokens(&store, "m-zero", 0.5).is_err());
}

// System::new は未登録モデルを起動時に倒す（ターン spawn 後の黙った失敗にしない・§15）。
#[test]
#[should_panic(expected = "no context_window registered")]
fn system_new_panics_on_unregistered_model() {
    // store に何も登録しない → ScriptedEngine の既定モデル "scripted" は未登録。
    let store = Store::new_in_memory().unwrap();
    let eng = ScriptedEngine::new();
    let host = ScriptedToolHost::new();
    let notif = RecordingNotifier::new();
    let _sys = System::new(
        store,
        Arc::new(eng),
        Arc::new(host),
        Arc::new(ScriptedShellHost::new()),
        Arc::new(notif),
        Arc::new(CharCounter),
        Config::default(),
    );
}

// 29. 割合の妥当域を起動時に fail loud で検査する（window>0 ガードと同じ流儀・§15）。
//     負値が as usize で黙って 0 予算へ落ちる経路を塞ぐ。
#[test]
#[should_panic(expected = "compaction_ratio は 0<r<=1")]
fn system_new_panics_on_out_of_range_compaction_ratio() {
    // window は登録済み（未登録 panic と取り違えないため）。ratio だけが妥当域外。
    let _h = build_win(
        Config {
            compaction_ratio: 0.0, // 0 は不可（予算が消える）
            ..Config::default()
        },
        200_000,
    );
}

#[test]
#[should_panic(expected = "memory_index_ratio は 0<r<1")]
fn system_new_panics_on_out_of_range_memory_index_ratio() {
    let _h = build_win(
        Config {
            memory_index_ratio: 1.0, // 1.0 は不可（索引が会話予算を丸ごと食う）
            ..Config::default()
        },
        200_000,
    );
}

// 参考: NewEvent を直接使わないことの確認（store の型が閉じていること）。
#[allow(dead_code)]
fn _typecheck(_e: NewEvent) {}

// 表示回帰（name/persona 分離・統括裁定）: ログの著者表示（read_log の author_display）は主体の `name` 列を
// 使う——人格本文（persona）ではない。name と persona を別値にして、表示に出るのは name だけだと固定する。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn read_log_author_display_uses_name_not_persona() {
    let h = build(Config::default());
    let gate = GateName::new("web");
    let addr = "room:main";
    let place = h
        .sys
        .create_place(Some(addr), None, &Policy::default(), None);
    h.sys.store().add_channel(place, &gate, addr).unwrap();

    // name（表示名）と persona（人格本文）を別値にする。
    let a = h.sys.create_subject(
        SubjectKind::Agent,
        "エージェントA",
        "あなたはエージェントAです（人格本文）",
        Standing::Trusted,
    );
    h.sys.join(place, a, Role::Participant);

    // その主体が著者の発話を積む。
    let ne = NewEvent {
        kind: EventKind::Spoke,
        author_subject: Some(a),
        author_external: None,
        content: Content::text("やあ"),
        mentions: vec![],
        reply_to: None,
        target: None,
        for_subject: None,
        attachments: vec![],
    };
    h.sys.store().append(place, &ne, 1).unwrap();

    let page = h.sys.read_log(&gate, addr, 1, 10).expect("read ok");
    let ev = page
        .events
        .iter()
        .find(|e| e.seq == 1)
        .expect("その発話がある");
    assert_eq!(
        ev.author_display.as_deref(),
        Some("エージェントA"),
        "著者表示は name（表示名）"
    );
    assert_ne!(
        ev.author_display.as_deref(),
        Some("あなたはエージェントAです（人格本文）"),
        "人格本文（persona）を表示に使わない"
    );
}
