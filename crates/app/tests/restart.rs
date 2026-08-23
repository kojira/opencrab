//! ファイルの DB を跨ぐ再起動（詳細§11）。**実ファイルを開き直す**ことで「場もログも生き残る」と
//! 「走り残りは中断として出来事になる」を、確定的に（プロセス spawn の間合いに頼らず）守る。
//!
//! e2e.rs が別プロセスで同じことを見せるが、こちらは同一テスト内で store を開き直すので、
//! 何が残り何が起きるかを直接検査できる。

use opencrab_app::Host;
use opencrab_engine::{ScriptedEngine, Step};
use opencrab_port::{ActivityKindTag, Content, EventKind, Role, Standing, SubjectKind};
use opencrab_social_runtime::Incoming;
use opencrab_store::{BackgroundProvenance, NewEvent, Store};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
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
