//! 記憶（記憶とワーカー）の性質をテストで守る。設計 §06 の「守るべき性質」のうち、記憶に関わる
//! 4 つ——由来から会話を再現できる／索引が予算を超えたら申告する／整理が途中で切れても畳んだ分が
//! 残る／整理の場が他の場のターンを止めない——を固定する。加えて、道具の主体分離（自分の記憶しか
//! 触れない）と、書き直しが由来を残すこと・索引は「読んだ」に数えないことを確かめる。
//!
//! 「主体を引数で受け取らない（型で守る）」は機構なのでテストしない（§02）——道具の入力スキーマに
//! subject が無く、`run_core_tool` はターンの主体を使う。ここで測るのはその配線の帰結（store が
//! subject で絞るので、他人の記憶を指す道具呼び出しが 0 行に落ちて失敗になる）。
//!
//! 推論は差し替えた偽物（ScriptedEngine）で回す。時間は tokio::time::pause() で進める。

use opencrab_engine::*;
use opencrab_port::*;
use opencrab_social_runtime::*;
use opencrab_store::Store;
use std::sync::Arc;
use tokio::sync::Notify;

struct Harness {
    sys: System,
    eng: ScriptedEngine,
    #[allow(dead_code)]
    host: ScriptedToolHost,
    #[allow(dead_code)]
    notif: RecordingNotifier,
}

/// テストの実効モデル（ScriptedEngine が名乗る既定名）。ハーネスが store へ context_window を登録する。
const TEST_MODEL: &str = "scripted";
/// 既定の登録 context_window。compaction_ratio 既定 0.5 と掛けて会話予算 100_000（旧固定既定と同値）。
const DEFAULT_TEST_CONTEXT_WINDOW: i64 = 200_000;

fn build(cfg: Config) -> Harness {
    build_win(cfg, DEFAULT_TEST_CONTEXT_WINDOW)
}

/// 実効モデルの context_window を明示して組む（会話予算 = context_window × compaction_ratio・§06）。
/// 記憶索引予算 = 会話予算 × memory_index_ratio。CharCounter は 1 文字 = 1 トークン。
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

async fn settle() {
    for _ in 0..400 {
        tokio::task::yield_now().await;
    }
}

// ---- store 層: 探す（新しい順・語で引く・上限つき）と、忘れる・書き直す（由来を残す）----

// 探すは語で引き、新しい順、上限つき（記憶とワーカー §03）。当たりは「最後に読まれた時刻」が進む。
#[test]
fn recall_matches_word_newest_first_and_capped() {
    let s = Store::new_in_memory().unwrap();
    let a = s
        .create_subject(SubjectKind::Agent, "A", "A", "engine", Standing::Trusted, 0)
        .unwrap();
    let p = s.create_place(Some("p"), None, "{}", None, 0).unwrap();

    // 「りんご」を含む記憶を 3 つ、含まないものを 1 つ。id 昇順＝古い順に書く。
    let m1 = s.remember(a, "りんご 1 個", p, 1, 1, 10).unwrap();
    let _m2 = s.remember(a, "みかん", p, 2, 2, 20).unwrap();
    let m3 = s.remember(a, "りんご 2 個", p, 3, 3, 30).unwrap();
    let m4 = s.remember(a, "りんご 3 個", p, 4, 4, 40).unwrap();

    // 語の一致・新しい順（id DESC）。読んだので last_read_at が now に進む。
    let hits = s.recall(a, "りんご", 100, 999).unwrap();
    assert_eq!(
        hits.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![m4, m3, m1],
        "語の一致・新しい順"
    );
    assert!(
        hits.iter().all(|m| m.last_read_at == Some(999)),
        "探した記憶は読まれた印が付く"
    );

    // 上限つき: 2 件だけ、なお新しい順。
    let two = s.recall(a, "りんご", 2, 1000).unwrap();
    assert_eq!(two.iter().map(|m| m.id).collect::<Vec<_>>(), vec![m4, m3]);

    // 公開 API が事前条件を守る（表側で守る・F2）。空語は全件を返さず「該当なし」。
    assert!(
        s.recall(a, "", 100, 1001).unwrap().is_empty(),
        "空語は全 TRUE で全件を返さない（該当なし）"
    );
    // 負の limit は SQLite で「無制限」——非負へ丸め、全件を返さない。
    assert!(
        s.recall(a, "りんご", -1, 1002).unwrap().is_empty(),
        "負の limit は無制限にならない（非負へ丸める）"
    );
}

// 書き直すは本文を差し替え、由来（場＋範囲）と書かれた時刻を残す（記憶とワーカー §03）。忘れるは消す。
#[test]
fn rewrite_keeps_origin_and_forget_removes() {
    let s = Store::new_in_memory().unwrap();
    let a = s
        .create_subject(SubjectKind::Agent, "A", "A", "engine", Standing::Trusted, 0)
        .unwrap();
    let p = s.create_place(Some("p"), None, "{}", None, 0).unwrap();

    let id = s.remember(a, "もとの本文", p, 5, 9, 100).unwrap();
    assert!(s.rewrite(a, id, "畳んだ要約").unwrap(), "書き直せる");

    let after = s.memories_newest_first(a).unwrap();
    assert_eq!(after.len(), 1);
    let m = &after[0];
    assert_eq!(m.body, "畳んだ要約", "本文は差し替わる");
    assert_eq!(
        (m.origin_place, m.origin_from_seq, m.origin_to_seq),
        (Some(p), Some(5), Some(9)),
        "由来は残る"
    );
    assert_eq!(m.written_at, 100, "書かれた時刻は残る");

    assert!(s.forget(a, id).unwrap(), "忘れられる");
    assert!(s.memories_newest_first(a).unwrap().is_empty(), "消えた");
    assert!(!s.forget(a, id).unwrap(), "二度目は 0 行（無い）");
}

// ---- §06 #1: 記憶の由来から、生まれた会話を再現できる ----

// 覚える道具が由来 = (いまの場, from, to) を記録し、その 2 値からログを読み直すと同じ会話に戻る。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn memory_origin_reproduces_the_conversation() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    // まず溜める（既定方針は発火しない）。会話が seq 1..3 に載る。
    let p = h.sys.create_place(None, None, &Policy::default(), None);
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);
    for t in ["会話その 1", "会話その 2", "会話その 3"] {
        h.sys.deliver(p, Incoming::said(human, t)).unwrap();
    }

    // 即応に変え、1 件で発火。エージェントは由来 1..3 で覚える。
    h.sys.set_policy(
        p,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
    );
    h.eng.push(Step::cont().with_tool_args(
        "core-remember",
        serde_json::json!({"body": "3 人で決めた方針", "from": 1, "to": 3}),
    ));
    h.eng.push(Step::no_reply());
    h.sys.deliver(p, Incoming::said(human, "trigger")).unwrap();
    settle().await;

    // 記憶が 1 件。由来 = (p, 1, 3)。
    let mems = h.sys.store().memories_newest_first(a).unwrap();
    assert_eq!(mems.len(), 1, "覚えた記憶が 1 件");
    let m = &mems[0];
    assert_eq!(m.origin_place, Some(p));
    assert_eq!((m.origin_from_seq, m.origin_to_seq), (Some(1), Some(3)));

    // 2 値（場＋範囲）からログを読み直すと、生まれた会話に完全に戻る。
    let reproduced = h
        .sys
        .store()
        .read_range(
            m.origin_place.expect("remembered origin place"),
            m.origin_from_seq.expect("remembered origin from") - 1,
            m.origin_to_seq.expect("remembered origin to"),
        )
        .unwrap();
    assert_eq!(
        reproduced
            .iter()
            .map(|e| e.content.text.clone().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["会話その 1", "会話その 2", "会話その 3"],
        "由来から会話を再現できる"
    );
}

// 覚える道具は由来の範囲を締める（F3）: いまの場の連番 1..=末尾 の中で from<=to でなければ Failed。
// 実在しない範囲を由来に持たせない（フォールバックで丸めない）——記憶は作られず、core は死なない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn remember_rejects_out_of_range_origin() {
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

    // ターン発火時の場の末尾は seq=1（"go"）。to=5 は末尾を超える → 覚えられない。
    // 続けて別のターンが普通に回る（core が生きている証拠）。
    h.eng.push(Step::done().with_tool_args(
        "core-remember",
        serde_json::json!({"body": "未来の範囲", "from": 1, "to": 5}),
    ));
    h.eng.push(Step::no_reply());

    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await;

    assert!(
        h.sys.store().memories_newest_first(a).unwrap().is_empty(),
        "範囲外の由来では記憶を作らない"
    );
    let recs = h.sys.store().turn_records(p).unwrap();
    assert_eq!(recs[0].end_reason, "done", "落ちずにターンは終わる");
    h.sys.deliver(p, Incoming::said(human, "again")).unwrap();
    settle().await;
    assert!(
        h.sys.store().turn_records(p).unwrap().len() >= 2,
        "core は生きていて次のターンも回る"
    );
}

// ---- §06 #2: 索引が予算を超えたら、超えたと言う ----

// 文脈には記憶の索引だけが載る。予算を超えたら黙って落とさず「省略」と申告し、
// 新しいものは載り、古いものは落ちる（記憶とワーカー §03・§06 と対）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn memory_index_announces_budget_overflow() {
    // 記憶索引予算を 20 に絞って超過を起こす（CharCounter で 1 文字=1 トークン）。索引予算 = 会話予算
    // × memory_index_ratio(0.02)。会話予算 1_000（window 1_000 × compaction_ratio 1.0）× 0.02 = 20。
    // 会話予算 1_000 はこの test の会話（数件）には十分で、索引の切り詰めだけを測れる。
    let cfg = Config {
        compaction_ratio: 1.0,
        ..Config::default()
    };
    let h = build_win(cfg, 1_000);
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

    // 記憶を 8 件、古い順に書く（MEM0 が最古、MEM7 が最新）。
    for i in 0..8 {
        h.sys
            .store()
            .remember(a, &format!("MEM{i}marker"), p, 1, 1, i)
            .unwrap();
    }

    h.eng.push(Step::no_reply());
    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await;

    let ctx = &h.eng.contexts()[0];
    assert!(
        ctx.contains("=== 記憶の索引 ==="),
        "索引が文脈に載る: {ctx}"
    );
    assert!(
        ctx.contains("省略"),
        "超過を申告する（黙って落とさない）: {ctx}"
    );
    assert!(ctx.contains("MEM7marker"), "新しいものは索引に載る");
    assert!(
        !ctx.contains("MEM0marker"),
        "最古は予算超過で索引から落ちる: {ctx}"
    );
}

// 索引に載ることは「読んだ」に数えない——last_read_at を進めるのは能動的に探した（recall）ときだけ。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn index_does_not_mark_read_but_recall_does() {
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
    h.sys.store().remember(a, "とある記憶", p, 1, 1, 5).unwrap();

    // ターンが 1 本回り、その文脈に索引が載る。だが探してはいない。
    h.eng.push(Step::no_reply());
    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await;
    assert!(h.eng.contexts()[0].contains("とある記憶"), "索引に載った");
    let before = h.sys.store().memories_newest_first(a).unwrap();
    assert_eq!(
        before[0].last_read_at, None,
        "索引に載っても読んだ印は付かない"
    );

    // 能動的に探すと読んだ印が付く。
    let hits = h.sys.store().recall(a, "とある", 10, 777).unwrap();
    assert_eq!(hits.len(), 1);
    let after = h.sys.store().memories_newest_first(a).unwrap();
    assert_eq!(after[0].last_read_at, Some(777), "探すと読んだ印が付く");
}

// ---- 主体分離: 自分の記憶しか触れない（道具経由でも）----

// a のターンで b の記憶を忘れようとしても、store が subject で絞るので 0 行——失敗になり、b の記憶は
// 残る。自分の記憶は忘れられる。主体を引数に取らない（型）ことの帰結を配線で確かめる（§06）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_cannot_forget_bs_memory_through_the_tool() {
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
    let p = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(p, a, Role::Participant);
    h.sys.join(p, human, Role::Participant);

    let m_a = h.sys.store().remember(a, "A の記憶", p, 1, 1, 1).unwrap();
    let m_b = h.sys.store().remember(b, "B の記憶", p, 1, 1, 1).unwrap();

    // a のターン: b の記憶 id を忘れようとする（失敗するはず）→ 続けて自分の記憶を忘れる（成功）。
    h.eng
        .push(Step::cont().with_tool_args("core-forget", serde_json::json!({"id": m_b})));
    h.eng
        .push(Step::cont().with_tool_args("core-forget", serde_json::json!({"id": m_a})));
    h.eng.push(Step::no_reply());
    h.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    settle().await;

    // b の記憶は残っている（a は他人の記憶を触れない）。
    assert_eq!(
        h.sys.store().memories_newest_first(b).unwrap().len(),
        1,
        "他人（B）の記憶は消せない"
    );
    // a 自身の記憶は消えている。
    assert!(
        h.sys.store().memories_newest_first(a).unwrap().is_empty(),
        "自分の記憶は忘れられる"
    );
}

// ---- §06 #3: 整理が途中で切れても、畳んだ分が残る ----

// 整理の場（＝ふつうの場）で、エージェントが記憶を畳み（書き直し）、その後ターンが早期終了しても、
// 畳んだ分は既に記憶へ書かれて残る。上限が切るのはターンで、作業ではない（記憶とワーカー §05）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn folding_survives_turn_interruption() {
    let h = build(Config::default());
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    // 整理の場（ここでは即応で駆動するが、無条件モードでも同じ——場とターンでやる・§02）。
    let org = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(org, a, Role::Participant);
    h.sys.join(org, human, Role::Participant);

    // 畳む前の記憶を 2 件。
    let m1 = h.sys.store().remember(a, "断片 1", org, 1, 1, 1).unwrap();
    let m2 = h.sys.store().remember(a, "断片 2", org, 1, 1, 1).unwrap();

    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    // 反復 1: m1 を畳んだ要約に書き直し、m2 を忘れる（畳む）。まだ done でない。
    h.eng.push(
        Step::cont()
            .with_tool_args(
                "core-rewrite",
                serde_json::json!({"id": m1, "body": "断片 1+2 の要約"}),
            )
            .with_tool_args("core-forget", serde_json::json!({"id": m2})),
    );
    // 反復 2: ここで詰まる。テストが場を閉じて早期終了させる。
    h.eng
        .push(Step::no_reply_cont().gated(gate.clone(), entered.clone()));

    h.sys
        .deliver(org, Incoming::said(human, "整理して"))
        .unwrap();
    entered.notified().await; // 反復 1 の畳みは済み、反復 2 に入って詰まっている
    h.sys.close_place(org, "打ち切り"); // 早期終了を要求
    gate.notify_one();
    settle().await;

    // ターンは早期終了している。
    let recs = h.sys.store().turn_records(org).unwrap();
    assert_eq!(recs[0].end_reason, "interrupted", "整理は途中で切れた");

    // それでも畳んだ分は残る。
    let mems = h.sys.store().memories_newest_first(a).unwrap();
    assert_eq!(mems.len(), 1, "m2 は畳まれて消え、m1 が要約として残る");
    assert_eq!(mems[0].id, m1);
    assert_eq!(mems[0].body, "断片 1+2 の要約", "畳んだ要約が残る");
}

// ---- §06 #4: 整理の場が、他の場のターンを止めない ----

// 整理の場で長いターンが詰まっていても、別の場のターンは走り、完結する。枠は場ごと（別スロット・
// 別タスク）なので、片方の場の長い作業が他方の場の枠を握らない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn stuck_organizing_turn_does_not_block_another_place() {
    let h = build(Config::default());
    let org_subj = h
        .sys
        .create_subject(SubjectKind::Agent, "ORG", "ORG", Standing::Trusted);
    let other_subj = h
        .sys
        .create_subject(SubjectKind::Agent, "OTHER", "OTHER", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);

    let org = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(org_subj),
        None,
    );
    h.sys.join(org, org_subj, Role::Participant);
    h.sys.join(org, human, Role::Participant);

    let other = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(other_subj),
        None,
    );
    h.sys.join(other, other_subj, Role::Participant);
    h.sys.join(other, human, Role::Participant);

    let gate = Arc::new(Notify::new()); // 整理の場のターンはここで詰まる
    let entered = Arc::new(Notify::new());
    h.eng
        .push(Step::no_reply().gated(gate.clone(), entered.clone())); // org の長いターン（pop 1）
    h.eng.push(Step::no_reply()); // other のターン（pop 2）

    // 整理の場のターンを起こし、推論に入って詰まらせる。
    h.sys
        .deliver(org, Incoming::said(human, "整理して"))
        .unwrap();
    entered.notified().await;

    // その間に別の場へ発話 → 別の場のターンが走って完結する。
    h.sys.deliver(other, Incoming::said(human, "やあ")).unwrap();
    settle().await;

    // 別の場のターンは完結した。
    let other_recs = h.sys.store().turn_records(other).unwrap();
    assert_eq!(other_recs.len(), 1, "他の場のターンは止まらず走る");
    assert_eq!(other_recs[0].end_reason, "no_reply");
    // 整理の場のターンはまだ走っている（記録はまだ無い）。
    assert!(
        h.sys.store().turn_records(org).unwrap().is_empty(),
        "整理の場のターンはまだ詰まっている（が他方を止めていない）"
    );

    // 後始末: 詰まりを解いて畳む。
    gate.notify_one();
    settle().await;
}
