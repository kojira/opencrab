//! tool 実行ログの永続化（#787）。1 実行 = 1 行。同期は invoke の戻り、切り離しは settle。
//! 推論は ScriptedEngine、shell は ScriptedShellHost。時間は pause() で進める。

use opencrab_engine::*;
use opencrab_port::*;
use opencrab_social_runtime::*;
use opencrab_store::Store;
use std::sync::Arc;
use std::time::Duration;

const TEST_MODEL: &str = "scripted";

struct Harness {
    sys: System,
    eng: ScriptedEngine,
    shell: ScriptedShellHost,
}

fn build() -> Harness {
    let store = Store::new_in_memory().unwrap();
    store
        .register_model_context_window(TEST_MODEL, 1_000_000)
        .unwrap();
    let eng = ScriptedEngine::new();
    let shell = ScriptedShellHost::new();
    let sys = System::new(
        store,
        Arc::new(eng.clone()),
        Arc::new(ScriptedToolHost::new()),
        Arc::new(shell.clone()),
        Arc::new(RecordingNotifier::new()),
        Arc::new(CharCounter),
        Config::default(),
    );
    Harness { sys, eng, shell }
}

async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

fn place_with_owner(h: &Harness) -> (PlaceId, SubjectId, SubjectId) {
    let agent = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = h
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let place = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(agent),
        None,
    );
    h.sys.join(place, agent, Role::Participant);
    h.sys.join(place, human, Role::Participant);
    (place, agent, human)
}

// 同期 core ツール: 1 実行 = tool_logs 1 行。outcome=done・tool_name が逐語で残る。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sync_core_tool_writes_one_row() {
    let h = build();
    let (place, agent, human) = place_with_owner(&h);
    h.eng.push(Step::cont().with_tool("core-child-list"));
    h.eng.push(Step::say_done("after"));
    h.sys.deliver(place, Incoming::said(human, "list")).unwrap();
    settle().await;

    let logs = h
        .sys
        .store()
        .list_tool_logs(&agent.to_string(), 10)
        .unwrap();
    assert_eq!(logs.len(), 1, "同期 1 実行 = 1 行: {logs:?}");
    assert_eq!(logs[0].tool_name, "core-child-list");
    assert_eq!(logs[0].outcome, "done");
    assert_eq!(logs[0].args_json, "{}");
    assert_eq!(logs[0].activity_id, None);
    let turns = h.sys.store().turn_records(place).unwrap();
    assert_eq!(logs[0].turn_record_id, Some(turns[0].id));
}

// 平文 ls 拒否: 1 行 outcome=refused。失敗理由の正本は offload。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn plaintext_ls_refusal_writes_row_and_offload() {
    let h = build();
    let (place, agent, human) = place_with_owner(&h);
    h.sys.allow_tool(agent, "core-shell");
    h.eng.push(Step::say_done(r#"core-shell::{"argv":["ls"]}"#));
    h.eng.push(Step::no_reply());
    h.sys.deliver(place, Incoming::said(human, "ls")).unwrap();
    settle().await;

    assert_eq!(h.shell.run_count(), 0, "拒否は実行しない");
    let logs = h
        .sys
        .store()
        .list_tool_logs(&agent.to_string(), 10)
        .unwrap();
    assert_eq!(logs.len(), 1, "断り 1 実行 = 1 行: {logs:?}");
    assert_eq!(logs[0].tool_name, "core-shell");
    assert_eq!(logs[0].outcome, "refused");
    assert!(
        logs[0].result_text.contains("許可されていない"),
        "理由の写し: {}",
        logs[0].result_text
    );
    let bg = logs[0].activity_id.expect("切り離し拒否は activity 付き");
    let offload = h
        .sys
        .store()
        .read_offload(agent, bg)
        .unwrap()
        .expect("失敗は必ず offload");
    assert!(
        offload.body.contains("許可されていない"),
        "offload が正本: {}",
        offload.body
    );
}

// core-bg-list の行は activity=N（場の 子 #N や出来事の連番ではない）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn bg_list_rows_use_activity_namespace() {
    let h = build();
    let (place, agent, human) = place_with_owner(&h);
    h.sys.allow_tool(agent, "core-shell");
    h.sys.allow_command(agent, "sleep");
    h.shell.set_slow(Duration::from_secs(500), "never");
    h.eng
        .push(Step::say_done(r#"core-shell::{"argv":["sleep"]}"#));
    h.sys
        .deliver(place, Incoming::said(human, "sleep"))
        .unwrap();
    settle().await;

    let bg = h
        .sys
        .store()
        .all_activities()
        .unwrap()
        .into_iter()
        .find(|a| a.kind == ActivityKindTag::Background && a.ended_at.is_none())
        .expect("走っている背景");
    h.eng.push(Step::cont().with_tool("core-bg-list"));
    h.eng.push(Step::say_done("listed"));
    h.sys.deliver(place, Incoming::said(human, "list")).unwrap();
    settle().await;

    let logs = h
        .sys
        .store()
        .list_tool_logs(&agent.to_string(), 10)
        .unwrap();
    let list = logs
        .iter()
        .find(|r| r.tool_name == "core-bg-list")
        .expect("core-bg-list が 1 行");
    assert_eq!(list.outcome, "done");
    assert!(
        list.result_text.contains(&format!("activity={}", bg.id)),
        "list 行が activity=: {}",
        list.result_text
    );
    assert!(
        !list.result_text.contains(&format!("#{} ", bg.id)),
        "活動に # を使わない: {}",
        list.result_text
    );
}
