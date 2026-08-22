//! 整理の場（＝専用機構ゼロのふつうの場）の性質をテストで守る。
//!
//! 整理の場は「無条件発火で自分から起き、新着ゼロ・記憶の索引だけの文脈から記憶を畳む」場として
//! 運用で枠づけるだけで、core には専用スケジューラも専用上限も専用文脈も無い（承認済み設計）。
//! ここで固定するのは、その運用が成り立つための唯一の配線——**core-create-place が default_subject
//! 未設定なら作成主体に結ぶ**——と、その結びが無条件発火（§04 batch_fire）に効くこと、そして
//! **core-set-policy は自動結びをせず fail loud で拒否する**という裁定。
//!
//! 推論は差し替えた偽物（ScriptedEngine）で回す。時間は tokio::time::pause() で進める。

use opencrab_engine::*;
use opencrab_port::*;
use opencrab_social_runtime::*;
use opencrab_store::Store;
use std::sync::Arc;
use std::time::Duration;

struct Harness {
    sys: System,
    eng: ScriptedEngine,
    #[allow(dead_code)]
    host: ScriptedToolHost,
    #[allow(dead_code)]
    notif: RecordingNotifier,
}

const TEST_MODEL: &str = "scripted";
const DEFAULT_TEST_CONTEXT_WINDOW: i64 = 200_000;

fn build(cfg: Config) -> Harness {
    let store = Store::new_in_memory().unwrap();
    store
        .register_model_context_window(TEST_MODEL, DEFAULT_TEST_CONTEXT_WINDOW)
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

/// 発火方針 JSON を組む（Policy::from_json が読む形。immediate_from は省略不可）。
fn policy_json(
    unconditional_ms: Option<i64>,
    default_subject: Option<SubjectId>,
) -> serde_json::Value {
    serde_json::json!({
        "immediate": [],
        "immediate_from": "anyone",
        "batch_window_ms": null,
        "unconditional_interval_ms": unconditional_ms,
        "default_subject": default_subject,
    })
}

/// 場に保存された発火方針を読み直す。
fn stored_policy(h: &Harness, place: PlaceId) -> Policy {
    let row = h.sys.store().get_place(place).unwrap().unwrap();
    Policy::from_json(&row.policy_json).unwrap()
}

const HALF_HOUR: Duration = Duration::from_secs(30 * 60);
const HALF_HOUR_MS: i64 = 30 * 60 * 1000;

// ---- ① 無条件駆動で、新着ゼロ・索引だけの文脈から記憶を畳める（memory.rs の口約束を実測に）----

// 整理の場は無条件で自分から起き、未読が 1 件も無くても記憶の索引だけを持った文脈でターンが回り、
// そこから core-rewrite / core-forget が出て記憶を畳める（記憶とワーカー §05 の「無条件モードでも同じ」）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unconditional_folds_memory_from_index_only_context() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "整理する人", Standing::Trusted);
    // 整理の場: 無条件で自分から起きる。新着は一切来ない。
    let org = h.sys.create_place(
        None,
        None,
        &Policy::default()
            .with_unconditional_ms(HALF_HOUR_MS)
            .with_default(a),
        None,
    );
    h.sys.join(org, a, Role::Participant);

    // 畳む前の記憶を 2 件（索引に載る）。
    let m1 = h.sys.store().remember(a, "断片 1", org, 1, 1, 1).unwrap();
    let m2 = h.sys.store().remember(a, "断片 2", org, 1, 1, 1).unwrap();

    // 無条件ターン 1 回で m1 を要約に書き直し、m2 を忘れる（畳む）。
    h.eng.push(
        Step::no_reply()
            .with_tool_args(
                "core-rewrite",
                serde_json::json!({"id": m1, "body": "断片 1・2 をまとめた要約"}),
            )
            .with_tool_args("core-forget", serde_json::json!({"id": m2})),
    );

    tokio::time::advance(HALF_HOUR).await;
    settle().await;

    // 文脈は「索引だけ」——未読の出来事は 1 件も載らず、記憶の索引が載っていた。
    let ctx = &h.eng.contexts()[0];
    assert!(
        ctx.contains("=== 記憶の索引 ==="),
        "索引が文脈に載る: {ctx}"
    );
    assert!(
        ctx.contains("断片 1") && ctx.contains("断片 2"),
        "畳む前の索引が見えている"
    );

    // 畳めた: m1 は要約に、m2 は消えた。
    let mems = h.sys.store().memories_newest_first(a).unwrap();
    assert_eq!(mems.len(), 1, "1 件に畳まれた");
    assert_eq!(mems[0].body, "断片 1・2 をまとめた要約", "書き直しが効いた");
    assert_eq!(mems[0].id, m1, "残ったのは書き直した方");
}

// ---- ② 発火 → 再武装 → 次の間隔で再発火（2 周）----

// 無条件は撃ったあと位相を保って次を入れる（§04）。2 間隔進めれば 2 回撃つ。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unconditional_rearms_and_fires_two_rounds() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::default()
            .with_unconditional_ms(HALF_HOUR_MS)
            .with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);

    h.eng.push(Step::no_reply());
    h.eng.push(Step::no_reply());

    tokio::time::advance(HALF_HOUR).await;
    settle().await;
    assert_eq!(
        h.sys.store().turn_records(p).unwrap().len(),
        1,
        "1 周目で 1 回"
    );

    tokio::time::advance(HALF_HOUR).await;
    settle().await;
    assert_eq!(
        h.sys.store().turn_records(p).unwrap().len(),
        2,
        "再武装して 2 周目でもう 1 回"
    );
}

// ---- ③ default_subject 未指定の core-create-place でも作成主体に結ばれ、繰り返し発火する（配線の回帰）----

// 唯一の本質の配線: core-create-place が default_subject 未設定の場を作ると、作成主体に結ぶ。
// これが無いと、作った場に無条件間隔を積んでも batch_fire が default_subject None で予定をクリアして
// 止まる。結びが効くことを「孫の場が繰り返し発火する」まで観測して固定する。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn created_place_without_default_subject_is_bound_to_creator_and_fires() {
    let h = build(Config::default());
    let ca = h
        .sys
        .create_subject(SubjectKind::Agent, "CA", "作る人", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    // 親の場: 即応で 1 回だけ起こす（無条件で自走させず、作成を 1 回に固定する）。
    let cc = h.sys.create_place(
        Some("cc"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(ca),
        None,
    );
    h.sys.join(cc, ca, Role::Participant);
    h.sys.join(cc, human, Role::Participant);

    // CA のターンで、default_subject を **指定せず** 無条件間隔つきの子（整理の場）を作る。
    h.eng.push(Step::done().with_tool_args(
        "core-create-place",
        serde_json::json!({
            "address": "gc",
            "policy": policy_json(Some(HALF_HOUR_MS), None),
        }),
    ));
    // 子は CA に結ばれ、無条件で 2 周ぶん自分から起きる。
    h.eng.push(Step::no_reply());
    h.eng.push(Step::no_reply());

    h.sys
        .deliver(cc, Incoming::said(human, "整理の場を作れ"))
        .unwrap();
    settle().await;

    let children = h.sys.store().child_places(cc).unwrap();
    assert_eq!(children.len(), 1, "子が 1 つできた");
    let gc = children[0].id;
    // 作成主体（CA）に結ばれている——JSON に default_subject が無くても core が埋めた。
    assert_eq!(
        stored_policy(&h, gc).default_subject,
        Some(ca),
        "default_subject 未指定でも作成主体に結ばれる"
    );

    // 結びが効くので、子は無条件で繰り返し起きる。
    tokio::time::advance(HALF_HOUR).await;
    settle().await;
    assert_eq!(
        h.sys.store().turn_records(gc).unwrap().len(),
        1,
        "子が 1 周目で起きる"
    );
    tokio::time::advance(HALF_HOUR).await;
    settle().await;
    assert_eq!(
        h.sys.store().turn_records(gc).unwrap().len(),
        2,
        "子が再武装して 2 周目でも起きる（結びが無ければ 1 周目で止まっていた）"
    );
    // 親は即応 1 回きり（自走していない）。
    assert_eq!(
        h.sys.store().turn_records(cc).unwrap().len(),
        1,
        "親は即応の 1 回だけ"
    );
}

// ---- ④ 前ターンが詰まっている間は撃たず、次の間隔で撃つ ----

// 無条件は枠が塞がっていたら飛ばす（撃たない）——予定は次の間隔に残る（§04）。走っているターンの
// 最中に来た無条件は新しいターンを起こさず、ターンが空いた次の間隔で撃つ。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unconditional_skips_while_busy_then_fires_next_interval() {
    // 1 周目のターンを gate で止めて「走りっぱなし」にする。無条件発火を跨いで走り続けても
    // idle_cap / turn_cap に切られないよう、両者を間隔よりずっと大きく取る（詰まりの検査に専念する）。
    let h = build(Config {
        idle_cap: Duration::from_secs(3600),
        turn_cap: Duration::from_secs(3600),
        ..Config::default()
    });
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::default()
            .with_unconditional_ms(HALF_HOUR_MS)
            .with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);

    let gate = Arc::new(tokio::sync::Notify::new());
    let entered = Arc::new(tokio::sync::Notify::new());
    // 1 周目（t=30min 発火）: gate で止めて枠を握り続ける。gate を解いたら done で終わる。
    h.eng
        .push(Step::no_reply().gated(gate.clone(), entered.clone()));
    // 枠が空いてからの 2 周目のターン。
    h.eng.push(Step::no_reply());

    // 1 周目発火 → ターンが推論に入り、gate で止まる（枠を握る）。
    tokio::time::advance(HALF_HOUR).await;
    settle().await;
    entered.notified().await;
    assert_eq!(h.eng.call_count(), 1, "1 周目のターンが走り出した");

    // 2 周目発火 → 1 周目がまだ枠を握っているので撃たない（詰まっている間は飛ばす・§04）。
    // skip 後は次の間隔（3 周目）へ再武装される。
    tokio::time::advance(HALF_HOUR).await;
    settle().await;
    assert_eq!(h.eng.call_count(), 1, "詰まっている間は撃たない");
    assert!(
        h.sys.store().turn_records(p).unwrap().is_empty(),
        "1 周目はまだ走行中（記録はまだ無い）"
    );

    // 枠を空ける（1 周目が決着）。時間は進めない——ここで 1 周目だけが終わる。
    gate.notify_one();
    settle().await;
    assert_eq!(
        h.sys.store().turn_records(p).unwrap().len(),
        1,
        "詰まっていた 1 周目が決着した（まだ新しいターンは起きていない）"
    );
    assert_eq!(h.eng.call_count(), 1, "枠が空いただけではまだ撃たない");

    // 3 周目発火 → 枠が空いたので撃つ（skip 後に再武装された次の間隔）。
    tokio::time::advance(HALF_HOUR).await;
    settle().await;
    assert_eq!(h.eng.call_count(), 2, "次の間隔で撃つ");
    assert_eq!(
        h.sys.store().turn_records(p).unwrap().len(),
        2,
        "起きたターンは 2 回（詰まっていた 1 回は起きていない）"
    );
}

// ---- ⑤ set-policy の fail loud（裁定 2 の固定）----

// set-policy は自動結びをしない。無条件間隔を積もうとする場に default_subject が無ければ、
// 発火時に黙って止まる（silent gap）代わりに、その場で明示エラーで拒否する。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn set_policy_fails_loud_when_arming_unconditional_without_default() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "親", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let parent = h.sys.create_place(
        Some("parent"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(parent, a, Role::Participant);
    h.sys.join(parent, human, Role::Participant);
    // 子は default_subject = A・無条件なしで作る（A は親なので子に set-policy できる）。
    let child = h.sys.create_place(
        Some("child"),
        Some(parent),
        &Policy::default().with_default(a),
        None,
    );

    // ターン 1: default_subject を欠いたまま無条件間隔を積もうとする → 拒否され、子の方針は変わらない。
    h.eng.push(Step::done().with_tool_args(
        "core-set-policy",
        serde_json::json!({
            "place": child,
            "policy": policy_json(Some(HALF_HOUR_MS), None),
        }),
    ));
    h.sys
        .deliver(parent, Incoming::said(human, "武装しろ"))
        .unwrap();
    settle().await;

    let pol = stored_policy(&h, child);
    assert_eq!(
        pol.default_subject,
        Some(a),
        "拒否されたので default_subject は元のまま（None に上書きされていない）"
    );
    assert_eq!(
        pol.unconditional_interval_ms, None,
        "拒否されたので無条件間隔は積まれていない"
    );
    assert_eq!(
        h.sys.store().turn_records(parent).unwrap()[0].end_reason,
        "done",
        "拒否は失敗を返すだけで core もターンも落ちない"
    );

    // ターン 2: default_subject を同梱すれば通る（正しい形は受け入れる）。
    h.eng.push(Step::done().with_tool_args(
        "core-set-policy",
        serde_json::json!({
            "place": child,
            "policy": policy_json(Some(HALF_HOUR_MS), Some(a)),
        }),
    ));
    h.sys
        .deliver(parent, Incoming::said(human, "主体つきで武装しろ"))
        .unwrap();
    settle().await;

    let pol2 = stored_policy(&h, child);
    assert_eq!(
        pol2.unconditional_interval_ms,
        Some(HALF_HOUR_MS),
        "主体を同梱した set-policy は通る"
    );
    assert_eq!(pol2.default_subject, Some(a), "主体は結ばれたまま");
}

// ---- ⑥ 新着ゼロ（unread 空）でも build_context が組めて engine が回る（要ビルド検証 1 を兼ねる）----

// 無条件で起きたターンは、未読が 1 件も無くても文脈を組める——記憶の索引と出力指示だけを載せて
// engine を呼ぶ。空 unread の経路で build_context が破綻しないことを固定する。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unconditional_turn_builds_context_with_empty_unread() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "整理する人", Standing::Trusted);
    let p = h.sys.create_place(
        None,
        None,
        &Policy::default()
            .with_unconditional_ms(HALF_HOUR_MS)
            .with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    // 記憶を 1 件だけ置く（索引に載る）。出来事は一切追記しない（unread は空）。
    h.sys
        .store()
        .remember(a, "覚えていること", p, 1, 1, 1)
        .unwrap();

    h.eng.push(Step::no_reply());

    tokio::time::advance(HALF_HOUR).await;
    settle().await;

    // engine が回り、ターンが記録された（空 unread でも build_context が組めた）。
    assert!(h.eng.call_count() >= 1, "空 unread でも engine が回る");
    assert!(
        !h.sys.store().turn_records(p).unwrap().is_empty(),
        "ターンが記録される"
    );
    // 文脈には記憶の索引が載り、未読の出来事は無い。
    let ctx = &h.eng.contexts()[0];
    assert!(ctx.contains("=== 記憶の索引 ==="), "索引が載る: {ctx}");
    assert!(ctx.contains("覚えていること"), "記憶本文が索引に見える");
    assert!(
        !ctx.contains("=== 引き継ぎ ==="),
        "引き継ぎは無い（新着ゼロの純粋な索引駆動）"
    );
}
