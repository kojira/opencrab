//! ファイルの DB を跨ぐ再起動（詳細§11）。**実ファイルを開き直す**ことで「場もログも生き残る」と
//! 「走り残りは中断として出来事になる」を、確定的に（プロセス spawn の間合いに頼らず）守る。
//!
//! e2e.rs が別プロセスで同じことを見せるが、こちらは同一テスト内で store を開き直すので、
//! 何が残り何が起きるかを直接検査できる。

use opencrab_app::Host;
use opencrab_engine::{ScriptedEngine, Step};
use opencrab_port::{ActivityKindTag, Content, EventKind, Role, Standing, SubjectKind};
use opencrab_social_runtime::{Incoming, Policy};
use opencrab_store::{
    BackgroundProvenance, BackgroundSettlement, NewBackgroundOffload, NewEvent, Store,
};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

fn tmp_db(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    // 長さ制限のあるソケットと違い DB ファイルはどこでもよい。target 配下（/Volumes/2TB 側）に置く。
    PathBuf::from(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .parent()
        .unwrap()
        .join(format!("restart-{}-{}-{}.db", std::process::id(), n, tag))
}

async fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if pred() {
            return true;
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    false
}

// 場もログも、ファイルの DB を開き直して生き残る。再起動後の設定は同じ場・同じ主体を拾う。
#[tokio::test(flavor = "current_thread")]
async fn place_and_log_survive_a_real_file_reopen() {
    let db = tmp_db("survive");
    let _ = std::fs::remove_file(&db);

    let (place1, agent1, latest1) = {
        let store = Store::open(&db).expect("open");
        let host = Host::boot(store);
        let (place, agent) = host.provision_web_room("room:main", "web-agent", "web-agent");
        // 人が発話 → エージェントのターンが起きて 1 度発話して終わる（EchoEngine）。
        let human = host.sys.create_subject(
            SubjectKind::Human,
            "test-owner",
            "test-owner",
            Standing::Owner,
        );
        host.sys
            .deliver(place, Incoming::said(human, "生き残るはず"))
            .expect("deliver");
        // said(seq1) + spoke(seq2) まで進むのを待つ。
        let sys = host.sys.clone();
        assert!(
            wait_until(Duration::from_secs(5), || sys
                .store()
                .latest_seq(place)
                .unwrap()
                >= 2)
            .await,
            "ターンが起きてログが 2 件になる"
        );
        (place, agent, host.sys.store().latest_seq(place).unwrap())
    }; // ここで host（と store）を drop = プロセスを落とす相当

    // ファイルを開き直す（= 再起動）。
    let store = Store::open(&db).expect("reopen");
    let host = Host::boot(store);
    let (place2, agent2) = host.provision_web_room("room:main", "web-agent", "web-agent");
    assert_eq!(place2, place1, "同じ場を拾う（作り直さない）");
    assert_eq!(agent2, agent1, "同じ主体を拾う");
    let latest2 = host.sys.store().latest_seq(place2).unwrap();
    assert_eq!(latest2, latest1, "ログが生き残る（伸びも縮みもしない）");
    // 発話の中身も残っている。
    let ev = host.sys.store().get_event(place2, 2).unwrap().unwrap();
    assert!(
        ev.content
            .text
            .as_deref()
            .unwrap_or("")
            .contains("受け取りました"),
        "エージェントの発話がログに残る: {:?}",
        ev.content.text
    );

    let _ = std::fs::remove_file(&db);
}

// 走り残っていた活動は、ファイルを開き直したときに「中断」の出来事になり、同じ主体のターンが起きる（§11）。
#[tokio::test(flavor = "current_thread")]
async fn leftover_running_activity_becomes_interruption_after_reopen() {
    let db = tmp_db("interrupt");
    let _ = std::fs::remove_file(&db);

    let (place, agent) = {
        let store = Store::open(&db).expect("open");
        let host = Host::boot(store);
        let (place, agent) = host.provision_web_room("room:main", "web-agent", "web-agent");
        // 走り残りを仕込む: 終わっていない活動（ended_at IS NULL）を 1 つ置いて、そのまま落とす。
        host.sys
            .store()
            .start_activity(place, agent, ActivityKindTag::Turn, None, 0, 0, None)
            .expect("seed running activity");
        (place, agent)
    }; // drop = 落ちる（活動は走り残ったまま）

    // 開き直す。Host::boot が startup() を呼び、走り残りを中断として閉じ、出来事にする。
    let store = Store::open(&db).expect("reopen");
    let host = Host::boot(store);
    let sys = host.sys.clone();
    // 中断の出来事がログに載る。
    assert!(
        wait_until(Duration::from_secs(5), || {
            let latest = sys.store().latest_seq(place).unwrap();
            (1..=latest).any(|s| {
                sys.store()
                    .get_event(place, s)
                    .unwrap()
                    .map(|e| e.kind == EventKind::Interrupted)
                    .unwrap_or(false)
            })
        })
        .await,
        "走り残りが中断の出来事になる"
    );
    // その活動はもう走っていない（interrupted で閉じられた）。
    assert!(
        sys.store().running_activities().unwrap().is_empty(),
        "走り残りは閉じられる"
    );
    let _ = agent;

    let _ = std::fs::remove_file(&db);
}

#[derive(Clone, Copy)]
enum LeftoverTurn {
    Absent,
    BeforeBackground,
    AfterBackground,
}

// Owner 起点の running Background を実ファイルへ残し、reopen 後の Interrupted turn が
// origin range・standing・activity・accepted tool call・中断結果を一対一で復元する。同じ場の汎用
// Turn 中断が無い場合と Background の前後にある場合を同じ assertion で固定する。
async fn assert_background_interruption_recovery(
    tag: &str,
    leftover_turn: LeftoverTurn,
    background_already_closed: bool,
) {
    let db = tmp_db(tag);
    let _ = std::fs::remove_file(&db);

    let (place, agent, activity) = {
        let store = Store::open(&db).expect("open");
        let host = Host::boot(store);
        let (place, agent) =
            host.provision_web_room("room:background", "synthetic-agent", "synthetic-agent");
        let owner = host.sys.create_subject(
            SubjectKind::Human,
            "synthetic-owner",
            "synthetic-owner",
            Standing::Owner,
        );
        host.sys.join(place, owner, Role::Participant);
        let origin = host
            .sys
            .store()
            .append(
                place,
                &NewEvent {
                    kind: EventKind::Said,
                    author_subject: Some(owner),
                    author_external: None,
                    content: Content::text("synthetic background request"),
                    mentions: vec![],
                    reply_to: None,
                    target: None,
                    for_subject: None,
                    attachments: vec![],
                },
                1,
            )
            .expect("seed origin");
        // Background は親 turn が入力を claim した後に切り離される。実際の crash state と同じく、
        // origin は既読にしておき、reopen 後は recovery が作る中断だけを発火対象にする。
        host.sys
            .store()
            .set_read_seq(place, agent, origin)
            .expect("parent turn claimed origin before crash");
        let provenance = BackgroundProvenance {
            origin_from_exclusive: origin - 1,
            origin_to_inclusive: origin,
            origin_standing: Standing::Owner,
            tool_name: "synthetic-background-tool".into(),
            tool_args: serde_json::json!({"mode": "bounded"}),
        };
        if matches!(leftover_turn, LeftoverTurn::BeforeBackground) {
            host.sys
                .store()
                .start_activity(place, agent, ActivityKindTag::Turn, None, 10, 0, None)
                .expect("seed running turn before background");
        }
        let activity = host
            .sys
            .store()
            .start_activity_with_provenance(
                place,
                agent,
                ActivityKindTag::Background,
                Some("synthetic running background"),
                10,
                0,
                None,
                Some(&provenance),
            )
            .expect("seed running background");
        if matches!(leftover_turn, LeftoverTurn::AfterBackground) {
            host.sys
                .store()
                .start_activity(place, agent, ActivityKindTag::Turn, None, 10, 0, None)
                .expect("seed running turn after background");
        }
        if background_already_closed {
            // 旧 startup が activity の終端だけを確定し、Interrupted event の追記前に再停止した
            // 実ファイル状態。次の reopen は running_activities() だけではこの行を拾えない。
            assert!(
                host.sys
                    .store()
                    .end_activity(activity, "interrupted", 2)
                    .expect("close activity before simulated crash"),
                "seed the crash point between the old two transactions"
            );
            assert_eq!(
                host.sys.store().latest_seq(place).unwrap(),
                origin,
                "the interrupted result is still absent at the simulated crash point"
            );
        }
        (place, agent, activity)
    };

    let store = Store::open(&db).expect("reopen");
    store
        .register_model_context_window("scripted", 200_000)
        .expect("register scripted model");
    let engine = ScriptedEngine::new();
    let expected_turns = match leftover_turn {
        LeftoverTurn::Absent => 1,
        LeftoverTurn::BeforeBackground | LeftoverTurn::AfterBackground => 2,
    };
    for _ in 0..expected_turns {
        engine.push(Step::no_reply());
    }
    let host = Host::boot_with_engine(store, Arc::new(engine.clone()));
    assert!(
        wait_until(Duration::from_secs(5), || {
            engine.contexts().len() == expected_turns
                && host.sys.store().running_activities().unwrap().is_empty()
        })
        .await,
        "{tag}: each interrupted activity starts its own follow-up turn; contexts={:?}",
        engine.contexts()
    );
    assert_eq!(engine.call_count() as usize, expected_turns);

    let activity_row = host
        .sys
        .store()
        .get_activity(activity)
        .unwrap()
        .expect("activity survives reopen");
    assert_eq!(activity_row.end_reason.as_deref(), Some("interrupted"));
    let latest = host.sys.store().latest_seq(place).unwrap();
    let interrupted: Vec<_> = host
        .sys
        .store()
        .read_range(place, 0, latest)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == EventKind::Interrupted)
        .collect();
    assert_eq!(interrupted.len(), expected_turns);
    let with_provenance: Vec<_> = interrupted
        .iter()
        .filter_map(|event| {
            host.sys
                .store()
                .settled_provenance(place, event.seq)
                .unwrap()
                .map(|provenance| (event, provenance))
        })
        .collect();
    assert_eq!(
        with_provenance.len(),
        1,
        "generic Turn interruption must neither absorb nor copy Background provenance"
    );
    let (interrupted, restored) = &with_provenance[0];
    assert_eq!(interrupted.for_subject, Some(agent));
    assert_eq!(restored.activity, activity);
    assert_eq!(restored.origin_from_exclusive, 0);
    assert_eq!(restored.origin_to_inclusive, 1);
    assert_eq!(restored.origin_standing, Standing::Owner);
    assert_eq!(restored.tool_name, "synthetic-background-tool");
    assert_eq!(restored.tool_args, serde_json::json!({"mode": "bounded"}));

    let contexts = engine.contexts();
    let background_contexts: Vec<_> = contexts
        .iter()
        .enumerate()
        .filter(|(_, context)| context.contains("受理ツール: synthetic-background-tool"))
        .collect();
    assert_eq!(
        background_contexts.len(),
        1,
        "{tag}: one Background interruption maps to one provenance follow-up turn; contexts={contexts:#?}"
    );
    let (background_context_index, context) = background_contexts[0];
    assert!(
        context.contains("synthetic background request"),
        "{context}"
    );
    assert!(context.contains(&format!("活動 #{activity}")), "{context}");
    assert!(
        context.contains("受理ツール: synthetic-background-tool args={\"mode\":\"bounded\"}"),
        "{context}"
    );
    assert!(context.contains("再起動により中断した"), "{context}");
    assert!(
        engine.tools_seen()[background_context_index]
            .iter()
            .any(|name| name == "core-allow-command"),
        "Owner standing is restored for the follow-up tool boundary"
    );

    // もう一度 reopen しても、同じ activity の Interrupted / provenance / follow-up turn は増えない。
    // 最初の回収 transaction の commit 後にプロセスが再停止した場合の収束を固定する。
    let latest_after_recovery = host.sys.store().latest_seq(place).unwrap();
    drop(host);
    let store = Store::open(&db).expect("second reopen");
    store
        .register_model_context_window("scripted", 200_000)
        .expect("register scripted model after second reopen");
    let second_engine = ScriptedEngine::new();
    second_engine.push(Step::no_reply());
    let second_host = Host::boot_with_engine(store, Arc::new(second_engine.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        second_engine.call_count(),
        0,
        "{tag}: a converged recovery must not start another follow-up turn"
    );
    assert_eq!(
        second_host.sys.store().latest_seq(place).unwrap(),
        latest_after_recovery,
        "{tag}: a converged recovery must not append another event"
    );
    let interrupted_after_second_reopen = second_host
        .sys
        .store()
        .read_range(place, 0, latest_after_recovery)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == EventKind::Interrupted)
        .count();
    assert_eq!(interrupted_after_second_reopen, expected_turns);

    let _ = std::fs::remove_file(&db);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_interruption_restores_full_provenance_after_real_file_reopen() {
    assert_background_interruption_recovery("background-only", LeftoverTurn::Absent, false).await;
    assert_background_interruption_recovery(
        "turn-before-background",
        LeftoverTurn::BeforeBackground,
        false,
    )
    .await;
    assert_background_interruption_recovery(
        "turn-after-background",
        LeftoverTurn::AfterBackground,
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_interruption_recovers_after_activity_close_crash_point() {
    assert_background_interruption_recovery("closed-before-event", LeftoverTurn::Absent, true)
        .await;
}

// main の旧 schema で既に再起動回収された Background は provenance を持たず、汎用 Interrupted event
// だけを持つ。upgrade はこれを PR 途中版の「activity 終端だけ commit 済み」と誤認せず、そのまま起動する。
#[tokio::test(flavor = "current_thread")]
async fn legacy_interrupted_background_upgrades_without_duplicate_recovery() {
    let db = tmp_db("legacy-interrupted-background");
    let _ = std::fs::remove_file(&db);

    let (place, activity, legacy_seq) = {
        let store = Store::open(&db).expect("create pre-upgrade database");
        let place = store
            .create_place(
                Some("synthetic:legacy"),
                None,
                &Policy::default().to_json(),
                None,
                0,
            )
            .expect("seed place");
        let subject = store
            .create_subject(
                SubjectKind::Agent,
                "synthetic-agent",
                "synthetic-agent",
                "echo",
                Standing::Trusted,
                0,
            )
            .expect("seed subject");
        let activity = store
            .start_activity(
                place,
                subject,
                ActivityKindTag::Background,
                Some("synthetic legacy background"),
                10,
                0,
                None,
            )
            .expect("seed legacy background");
        assert!(store
            .end_activity(activity, "interrupted", 1)
            .expect("legacy startup closes activity"));
        let legacy_seq = store
            .append(
                place,
                &NewEvent {
                    kind: EventKind::Interrupted,
                    author_subject: None,
                    author_external: None,
                    content: Content::text("synthetic legacy interruption"),
                    mentions: vec![],
                    reply_to: None,
                    target: None,
                    for_subject: Some(subject),
                    attachments: vec![],
                },
                2,
            )
            .expect("legacy startup appends generic interruption");
        (place, activity, legacy_seq)
    };

    // merge-base schema と同じく provenance 列・settled_provenance table が無いファイルへ戻す。
    let legacy = Connection::open(&db).expect("open legacy schema fixture");
    legacy
        .execute_batch(
            "DROP TABLE settled_provenance;
             ALTER TABLE activities DROP COLUMN origin_from_exclusive;
             ALTER TABLE activities DROP COLUMN origin_to_inclusive;
             ALTER TABLE activities DROP COLUMN origin_standing;
             ALTER TABLE activities DROP COLUMN accepted_tool_name;
             ALTER TABLE activities DROP COLUMN accepted_tool_args_json;",
        )
        .expect("downgrade fixture to merge-base activity schema");
    drop(legacy);

    let store = Store::open(&db).expect("upgrade legacy database");
    let host = Host::boot(store);
    assert_eq!(
        host.sys.store().latest_seq(place).unwrap(),
        legacy_seq,
        "upgrade startup must not duplicate the legacy generic interruption"
    );
    let row = host
        .sys
        .store()
        .get_activity(activity)
        .unwrap()
        .expect("legacy activity survives upgrade");
    assert_eq!(row.end_reason.as_deref(), Some("interrupted"));
    assert_eq!(row.provenance, None, "migrated legacy columns remain NULL");
    assert!(
        host.sys
            .store()
            .activities_needing_interruption()
            .unwrap()
            .is_empty(),
        "already recovered legacy background is not selected again"
    );
    drop(host);

    let second = Host::boot(Store::open(&db).expect("second upgraded reopen"));
    assert_eq!(second.sys.store().latest_seq(place).unwrap(), legacy_seq);
    let _ = std::fs::remove_file(&db);
}

fn seed_atomic_background(store: &Store, tag: &str) -> (i64, i64, i64) {
    let place = store
        .create_place(Some(tag), None, "{}", None, 0)
        .expect("seed atomic place");
    let subject = store
        .create_subject(
            SubjectKind::Agent,
            "synthetic-agent",
            "synthetic-agent",
            "echo",
            Standing::Trusted,
            0,
        )
        .expect("seed atomic subject");
    let provenance = BackgroundProvenance {
        origin_from_exclusive: 0,
        origin_to_inclusive: 0,
        origin_standing: Standing::Owner,
        tool_name: "synthetic-background-tool".into(),
        tool_args: serde_json::json!({"mode": "atomic"}),
    };
    let activity = store
        .start_activity_with_provenance(
            place,
            subject,
            ActivityKindTag::Background,
            Some("synthetic atomic background"),
            10,
            0,
            None,
            Some(&provenance),
        )
        .expect("seed atomic background");
    (place, subject, activity)
}

// transaction の最後（provenance INSERT）を強制失敗させ、先行する activity UPDATE / offload / event も
// 実ファイル reopen 後に一切残らないこと、その後の同じ done が完全な 1 結果として再実行できることを固定する。
#[test]
fn background_done_rolls_back_whole_transaction_and_retries_after_reopen() {
    let db = tmp_db("atomic-done-rollback");
    let _ = std::fs::remove_file(&db);
    let (place, subject, activity) = {
        let store = Store::open(&db).expect("open");
        seed_atomic_background(&store, "synthetic:atomic-done")
    };
    let conn = Connection::open(&db).expect("install synthetic crash point");
    conn.execute_batch(
        "CREATE TRIGGER synthetic_settlement_crash
         BEFORE INSERT ON settled_provenance
         BEGIN SELECT RAISE(ABORT, 'synthetic settlement crash'); END;",
    )
    .unwrap();
    drop(conn);

    let offload = NewBackgroundOffload {
        body: "synthetic saved result".into(),
        truncated: false,
    };
    let store = Store::open(&db).expect("reopen at crash fixture");
    assert!(
        store
            .settle_background_activity(
                activity,
                "done",
                "synthetic done notice",
                Some(&offload),
                1,
                2,
            )
            .is_err(),
        "forced failure at the final write must surface"
    );
    drop(store);

    let after_failure = Store::open(&db).expect("reopen after failed transaction");
    let row = after_failure.get_activity(activity).unwrap().unwrap();
    assert_eq!(row.ended_at, None, "activity end rolls back");
    assert_eq!(
        after_failure.latest_seq(place).unwrap(),
        0,
        "event rolls back"
    );
    assert!(
        after_failure
            .read_offload(subject, activity)
            .unwrap()
            .is_none(),
        "offload rolls back"
    );
    drop(after_failure);

    let conn = Connection::open(&db).expect("remove synthetic crash point");
    conn.execute_batch("DROP TRIGGER synthetic_settlement_crash;")
        .unwrap();
    drop(conn);

    let store = Store::open(&db).expect("retry reopen");
    let seq = match store
        .settle_background_activity(
            activity,
            "done",
            "synthetic done notice",
            Some(&offload),
            3,
            4,
        )
        .unwrap()
    {
        BackgroundSettlement::Appended { place: p, seq } => {
            assert_eq!(p, place);
            seq
        }
        other => panic!("retry must append the complete result: {other:?}"),
    };
    drop(store);

    let reopened = Store::open(&db).expect("reopen committed result");
    assert_eq!(
        reopened
            .settle_background_activity(
                activity,
                "failed",
                "must not replace committed result",
                None,
                5,
                6,
            )
            .unwrap(),
        BackgroundSettlement::AlreadyRecorded { place, seq },
        "commit後の再実行は保存済み結果へ収束する"
    );
    assert_eq!(reopened.latest_seq(place).unwrap(), seq);
    assert_eq!(
        reopened
            .get_activity(activity)
            .unwrap()
            .unwrap()
            .end_reason
            .as_deref(),
        Some("done")
    );
    assert_eq!(
        reopened
            .get_event(place, seq)
            .unwrap()
            .unwrap()
            .content
            .text
            .as_deref(),
        Some("synthetic done notice")
    );
    assert_eq!(
        reopened
            .read_offload(subject, activity)
            .unwrap()
            .unwrap()
            .body,
        "synthetic saved result"
    );
    assert_eq!(
        reopened
            .settled_provenance(place, seq)
            .unwrap()
            .unwrap()
            .activity,
        activity
    );
    let _ = std::fs::remove_file(&db);
}

// stop と自然完走を同時に決着させても、片方だけが activity/event/provenance を commit し、負け側と
// reopen 後の二回目実行は同じ結果へ収束する。
#[test]
fn background_stop_and_natural_completion_race_converges_after_reopen() {
    let db = tmp_db("atomic-stop-race");
    let _ = std::fs::remove_file(&db);
    let store = Store::open(&db).expect("open");
    let (place, _subject, activity) = seed_atomic_background(&store, "synthetic:stop-race");
    let barrier = Arc::new(Barrier::new(3));
    let stopped_store = store.clone();
    let stopped_barrier = barrier.clone();
    let stopped = std::thread::spawn(move || {
        stopped_barrier.wait();
        stopped_store
            .settle_background_activity(activity, "stopped", "synthetic stopped result", None, 1, 2)
            .unwrap()
    });
    let done_store = store.clone();
    let done_barrier = barrier.clone();
    let done = std::thread::spawn(move || {
        done_barrier.wait();
        done_store
            .settle_background_activity(activity, "done", "synthetic natural result", None, 3, 4)
            .unwrap()
    });
    barrier.wait();
    let stopped = stopped.join().unwrap();
    let done = done.join().unwrap();
    let seq = match (stopped, done) {
        (
            BackgroundSettlement::Appended { place: p, seq },
            BackgroundSettlement::AlreadyRecorded { place: q, seq: s },
        )
        | (
            BackgroundSettlement::AlreadyRecorded { place: q, seq: s },
            BackgroundSettlement::Appended { place: p, seq },
        ) => {
            assert_eq!((p, seq), (q, s));
            assert_eq!(p, place);
            seq
        }
        other => panic!("exactly one competing settlement must append: {other:?}"),
    };
    drop(store);

    let reopened = Store::open(&db).expect("reopen race winner");
    let row = reopened.get_activity(activity).unwrap().unwrap();
    let event = reopened.get_event(place, seq).unwrap().unwrap();
    match row.end_reason.as_deref() {
        Some("stopped") => assert_eq!(
            event.content.text.as_deref(),
            Some("synthetic stopped result")
        ),
        Some("done") => assert_eq!(
            event.content.text.as_deref(),
            Some("synthetic natural result")
        ),
        other => panic!("unexpected winning end reason: {other:?}"),
    }
    for (reason, content) in [
        ("stopped", "second stopped result"),
        ("done", "second natural result"),
    ] {
        assert_eq!(
            reopened
                .settle_background_activity(activity, reason, content, None, 5, 6)
                .unwrap(),
            BackgroundSettlement::AlreadyRecorded { place, seq }
        );
    }
    assert_eq!(reopened.latest_seq(place).unwrap(), seq);
    assert_eq!(
        reopened
            .settled_provenance(place, seq)
            .unwrap()
            .unwrap()
            .activity,
        activity
    );
    let _ = std::fs::remove_file(&db);
}
