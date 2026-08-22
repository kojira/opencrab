//! ファイルの DB を跨ぐ再起動（詳細§11）。**実ファイルを開き直す**ことで「場もログも生き残る」と
//! 「走り残りは中断として出来事になる」を、確定的に（プロセス spawn の間合いに頼らず）守る。
//!
//! e2e.rs が別プロセスで同じことを見せるが、こちらは同一テスト内で store を開き直すので、
//! 何が残り何が起きるかを直接検査できる。

use opencrab_app::Host;
use opencrab_port::{ActivityKindTag, EventKind, Standing, SubjectKind};
use opencrab_social_runtime::Incoming;
use opencrab_store::Store;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
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
