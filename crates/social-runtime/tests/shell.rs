//! shell（core builtin `core-shell`）の性質を固定する（DESIGN-shell.md「テスト」節）。
//!
//! - allowlist 外の argv[0] は理由つきで拒否される。空 allowlist は全拒否。
//! - core-allow-command は owner 発話のないターンでは通らない（自己拡張の禁止）。owner 発話のある
//!   ターンでだけ許可が広がる。
//! - 切り離し → 決着イベント → 退避 → core-bg-read の一連が shell で通る。
//! - 直接 exec であること（`; rm` 入り引数が 1 要素のまま ShellHost へ渡る＝シェル文字列を組まない）。
//! - shell は既定では subject_allowed_tools に入っていない（許可した主体にだけ広告・実行される）。
//!
//! 推論は差し替えた偽物（ScriptedEngine）、shell は fake（ScriptedShellHost）で回す。時間は
//! tokio::time::pause()（start_paused）で進める。say に書いた**平文ツール行**（`名前::内容`）を core が
//! 解釈して実行する経路を使う（本物のプロバイダの native tool 呼び出しと同じ core の判定・実行を通る）。

use opencrab_engine::*;
use opencrab_port::*;
use opencrab_social_runtime::*;
use opencrab_store::Store;
use std::collections::BTreeSet;
use std::sync::Arc;

struct Harness {
    sys: System,
    eng: ScriptedEngine,
    shell: ScriptedShellHost,
}

const TEST_MODEL: &str = "scripted";

fn build() -> Harness {
    build_cfg(Config::default())
}

fn build_cfg(cfg: Config) -> Harness {
    let store = Store::new_in_memory().unwrap();
    store
        .register_model_context_window(TEST_MODEL, 200_000)
        .unwrap();
    let eng = ScriptedEngine::new();
    let host = ScriptedToolHost::new();
    let shell = ScriptedShellHost::new();
    let notif = RecordingNotifier::new();
    let sys = System::new(
        store,
        Arc::new(eng.clone()),
        Arc::new(host),
        Arc::new(shell.clone()),
        Arc::new(notif),
        Arc::new(CharCounter),
        cfg,
    );
    Harness { sys, eng, shell }
}

async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

fn web_gate() -> GateSpec {
    GateSpec {
        name: GateName::new("web"),
        protocol: 1,
        address_form: ".*".into(),
        tools: vec![],
        effects: [EffectKind::Say].into_iter().collect::<BTreeSet<_>>(),
        capabilities: BTreeSet::new(),
        actions: vec![],
    }
}

/// 場と 1 体のエージェント A（Direct で即応）を用意し、web ゲートを結ぶ。A の standing を選べる。
/// 場・エージェント・**owner の主体**を用意する。
///
/// #14 で shell がオーナー起点のターンでだけ使えるようになったので、テストは owner の身元
/// （`npubOwner`）で起こす必要がある。非オーナー起点を試すテストは別の身元（`npubX`）を使う。
fn place_with(h: &Harness, standing: Standing) -> (PlaceId, SubjectId, SubjectId) {
    let a = h.sys.create_subject(SubjectKind::Agent, "A", "A", standing);
    let place = h.sys.create_place(
        Some("room:main"),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(place, a, Role::Participant);
    h.sys.register_gate(web_gate()).unwrap();
    h.sys
        .store()
        .add_channel(place, &GateName::new("web"), "room:main")
        .unwrap();
    let o = h
        .sys
        .create_subject(SubjectKind::Human, "O", "O", Standing::Owner);
    h.sys.add_identity(o, "web", "npubOwner");
    (place, a, o)
}

/// A を起こす外来メッセージ（外界の著者・主体に解決しない＝owner ではない）。`nonce` は origin を
/// 一意にする——同じ origin は dedup で握り潰され二度目の起こしが効かなくなる（複数ターンを回す時に要る）。
fn wake_external(h: &Harness, nonce: &str) {
    h.sys
        .deliver_event(&GateName::new("web"), inbound("npubOwner", "やあ", nonce))
        .unwrap();
}

fn inbound(author_external: &str, text: &str, nonce: &str) -> GateEvent {
    GateEvent {
        kind: EventKind::Said,
        address: "room:main".into(),
        author_external: author_external.into(),
        author_display: None,
        content: Content::text(text),
        mentions: vec![],
        reply_to: None,
        target: None,
        origin: Some(format!("note-{nonce}")),
        attachments: vec![],
        discovery: None,
        metadata: serde_json::json!({}),
    }
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

// ---- allowlist（argv[0] の完全一致・空は全拒否）----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn empty_command_allowlist_denies_everything_with_reason() {
    let h = build();
    let (place, a, _o) = place_with(&h, Standing::Trusted);
    // shell は使えるが、コマンドの許可は空（既定）。
    h.sys.allow_tool(a, "core-shell");
    // argv は配列なので 1 行 JSON で渡す。
    h.eng
        .push(Step::say_done("core-shell::{\"argv\":[\"echo\",\"hi\"]}"));
    h.eng.push(Step::no_reply()); // 拒否の決着から起きるターン
    wake_external(&h, "w1");
    settle().await;

    // 実行されない（ShellHost は呼ばれない）。
    assert_eq!(h.shell.run_count(), 0, "空 allowlist では実行しない");
    // 拒否は理由つきで決着イベントに出る（argv[0] を名指す）。
    let all: Vec<String> = h
        .sys
        .store()
        .read_range(place, 0, h.sys.store().latest_seq(place).unwrap())
        .unwrap()
        .into_iter()
        .map(|e| {
            format!(
                "{:?}:{}",
                e.kind,
                e.content
                    .text
                    .unwrap_or_default()
                    .chars()
                    .take(40)
                    .collect::<String>()
            )
        })
        .collect();
    eprintln!("DIAG_EVENTS {all:#?}");
    let settles = settle_texts(&h, place);
    assert_eq!(settles.len(), 1, "拒否の決着が 1 つ: {settles:?}");
    assert!(
        settles[0].contains("echo"),
        "argv[0] を名指す: {}",
        settles[0]
    );
    assert!(
        settles[0].contains("許可されていない"),
        "理由が判る: {}",
        settles[0]
    );
    let failed = bg_activities(&h)
        .into_iter()
        .find(|act| act.end_reason.as_deref() == Some("failed"))
        .expect("拒否は背景活動として決着する");
    let offload = h
        .sys
        .store()
        .read_offload(a, failed.id)
        .unwrap()
        .expect("失敗理由は offload に残る");
    assert!(
        offload.body.contains("許可されていない"),
        "read_offload が拒否理由を含む: {}",
        offload.body
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn command_outside_allowlist_is_denied_but_allowed_one_runs() {
    let h = build();
    let (place, a, _o) = place_with(&h, Standing::Trusted);
    h.sys.allow_tool(a, "core-shell");
    h.sys.allow_command(a, "echo"); // echo だけ許可
    h.shell.set_output("done");
    // 許可外の ls は拒否される。
    h.eng
        .push(Step::say_done("core-shell::{\"argv\":[\"ls\",\"-la\"]}"));
    h.eng.push(Step::no_reply());
    wake_external(&h, "w2");
    settle().await;
    assert_eq!(h.shell.run_count(), 0, "許可外 ls は実行しない");
    let settles = settle_texts(&h, place);
    assert!(
        settles
            .iter()
            .any(|s| s.contains("ls") && s.contains("許可されていない")),
        "ls の拒否が理由つき: {settles:?}"
    );

    // 許可済みの echo は実行され、成功として決着する。
    h.eng
        .push(Step::say_done("core-shell::{\"argv\":[\"echo\",\"hi\"]}"));
    h.eng.push(Step::no_reply());
    wake_external(&h, "w3");
    settle().await;
    assert_eq!(h.shell.run_count(), 1, "許可済み echo は実行される");
    assert_eq!(
        h.shell.last_argv(),
        Some(vec!["echo".to_string(), "hi".to_string()])
    );
}

// ---- subject_allowed_tools（shell は既定で入っていない）----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shell_not_available_until_allowed_as_a_tool() {
    let h = build();
    let (place, a, _o) = place_with(&h, Standing::Trusted);
    // 許可していない → core-shell は広告に出ない（可視面と実行が同じ判定・§09）。
    assert!(
        !h.sys.tool_allowed(place, a, "core-shell"),
        "既定では shell は使えない"
    );
    let tools = h.sys.advertised_tools(place, a).unwrap();
    assert!(
        !tools.iter().any(|t| t.name == "core-shell"),
        "許可前は広告に出ない"
    );
    // 許可すると主体の権限としては使える。
    h.sys.allow_tool(a, "core-shell");
    assert!(
        h.sys.tool_allowed(place, a, "core-shell"),
        "許可後は主体の権限として使える"
    );
    // ただし #14 で shell は**オーナー起点のターンでだけ**広告される。ターン外の
    // `advertised_tools`（owner_follow_up を持たない）には出ない——見せる側と実行側で
    // 同じ条件を通すため（見えるのに呼べない、を作らない）。
    let tools = h.sys.advertised_tools(place, a).unwrap();
    assert!(
        !tools.iter().any(|t| t.name == "core-shell"),
        "ターン外の広告には出ない（オーナー起点のターンでだけ出る）"
    );
}

// ---- 直接 exec（シェル文字列を組まない＝注入不可）----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn argv_is_passed_directly_without_shell_interpretation() {
    let h = build();
    let (_place, a, _o) = place_with(&h, Standing::Trusted);
    h.sys.allow_tool(a, "core-shell");
    h.sys.allow_command(a, "echo");
    // `; rm -rf /` を含む引数。直接 exec なら 1 要素のまま渡り、シェルとしては解釈されない。
    h.eng.push(Step::say_done(
        "core-shell::{\"argv\":[\"echo\",\"hello; rm -rf /\"]}",
    ));
    h.eng.push(Step::no_reply());
    wake_external(&h, "w4");
    settle().await;

    assert_eq!(h.shell.run_count(), 1, "1 回実行される");
    assert_eq!(
        h.shell.last_argv(),
        Some(vec![
            "echo".to_string(),
            "hello; rm -rf /".to_string() // `; rm` は分割されず 1 引数のまま
        ]),
        "argv は構造化されたまま渡る（シェル文字列を組まない）"
    );
    // cwd は subject ごとの作業領域（主体を跨がない）。
    assert_eq!(h.shell.last_cwd(), Some(format!("subject-{a}")));
}

// ---- 切り離し → 決着 → 退避 → core-bg-read の一連 ----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shell_detaches_settles_offloads_and_is_read_by_bg_read() {
    let h = build();
    let (place, a, _o) = place_with(&h, Standing::Trusted);
    h.sys.allow_tool(a, "core-shell");
    h.sys.allow_command(a, "cat");
    // 大きい出力（inline 上限 2,500 トークン超）。CharCounter は 1 文字 = 1 トークン。
    let big: String = (0..200)
        .map(|i| format!("line-{i:04}-xxxxxxxxxxxxxxxxxxxx"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(big.len() > 2_500, "退避されるだけの大きさ");
    h.shell.set_output(&big);

    // ターン1: shell を切り離す。
    h.eng.push(Step::say_done(
        "core-shell::{\"argv\":[\"cat\",\"big.txt\"]}",
    ));
    h.eng.push(Step::no_reply()); // 決着（退避通知）から起きるターン
    wake_external(&h, "w5");
    settle().await;

    // 切り離し: 背景活動になり、成功で決着した。
    let bgs = bg_activities(&h);
    let shell_bg: Vec<_> = bgs
        .iter()
        .filter(|b| {
            b.label
                .as_deref()
                .map(|l| l.starts_with("shell:"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(shell_bg.len(), 1, "shell は 1 つの背景活動になる: {bgs:?}");
    let bg_id = shell_bg[0].id;
    assert_eq!(shell_bg[0].end_reason.as_deref(), Some("done"));

    // 退避: 決着本文は案内＋読み方レシピだけで、生データを 1 バイトも載せない。
    let settles = settle_texts(&h, place);
    assert_eq!(settles.len(), 1, "決着イベントが 1 つ: {settles:?}");
    assert!(settles[0].contains("退避"), "退避された: {}", settles[0]);
    assert!(
        settles[0].contains(&format!("core-bg-read（activity={bg_id}")),
        "読み方（core-bg-read）が載る: {}",
        settles[0]
    );
    assert!(
        !settles[0].contains("line-0100"),
        "生データは載らない: {}",
        settles[0]
    );
    // 退避先（store）に本文がまるごと残っている。
    let row = h
        .sys
        .store()
        .read_offload(a, bg_id)
        .unwrap()
        .expect("退避がある");
    assert_eq!(row.body, big, "退避本文は完全");

    // core-bg-read で行範囲を読む（別ターン）。返り値は inline 上限未満に収まる。
    h.eng.push(Step::say_done(&format!(
        "core-bg-read::{{\"activity\":{bg_id},\"start_line\":1,\"line_count\":5}}"
    )));
    h.eng.push(Step::no_reply()); // bg-read の決着から起きるターン
    wake_external(&h, "w6");
    settle().await;

    let settles = settle_texts(&h, place);
    // 2 つ目の決着（bg-read の結果）に退避の先頭行が載る。
    assert!(
        settles.iter().any(|s| s.contains("line-0000")),
        "bg-read が退避の先頭を返す: {settles:?}"
    );
}

// ---- core-allow-command（owner-only・OwnerFollowUp）----

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn allow_command_is_denied_without_owner_in_this_turn() {
    let h = build();
    let (_place, a, _o) = place_with(&h, Standing::Trusted);
    // owner の発話がないターン（身元の無い外来 npubX が起こす）で自己拡張を試みる。
    h.eng
        .push(Step::say_done("core-allow-command::{\"command\":\"git\"}"));
    h.eng.push(Step::no_reply());
    h.sys
        .deliver_event(&GateName::new("web"), inbound("npubX", "やあ", "w7"))
        .unwrap();
    settle().await;

    // 許可は広がらない（自己拡張の禁止）。
    assert!(
        !h.sys.store().subject_allows_command(a, "git").unwrap(),
        "owner 発話の無いターンでは許可を広げられない"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn allow_command_passes_when_owner_spoke_this_turn() {
    let h = build();
    let (_place, a, _o) = place_with(&h, Standing::Trusted);
    // owner は place_with が用意済み（身元 npubOwner）。
    // owner O の発話が A を起こす（未読に owner の発話がある = OwnerFollowUp が立つ）。
    h.eng
        .push(Step::say_done("core-allow-command::{\"command\":\"git\"}"));
    h.eng.push(Step::no_reply()); // 許可の決着から起きるターン
    h.sys
        .deliver_event(
            &GateName::new("web"),
            inbound("npubOwner", "git を許可する", "owner1"),
        )
        .unwrap();
    settle().await;

    assert!(
        h.sys.store().subject_allows_command(a, "git").unwrap(),
        "owner 発話のあるターンでは許可が広がる"
    );
}
