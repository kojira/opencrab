/// D-1143: オーナーが「誰でも使える」に指定した（`agent_visible`）スキルを `read_skill` で
/// 開いた run は、caller を問わずそのスキルの `tools` を使える。それ以外は従来どおり拒否。
fn insert_grant_skill(ctx: &ActionContext, name: &str, agent_visible: bool, tools: &[&str]) {
    insert_skill_with_source(ctx, name, "manual", agent_visible, tools);
}

fn insert_skill_with_source(
    ctx: &ActionContext,
    name: &str,
    source_type: &str,
    agent_visible: bool,
    tools: &[&str],
) {
    let conn = ctx.db.lock().unwrap();
    opencrab_db::queries::insert_skill(
        &conn,
        &opencrab_db::queries::SkillRow {
            id: format!("id-{name}"),
            agent_id: ctx.agent_id.clone(),
            name: name.to_string(),
            description: "d".to_string(),
            situation_pattern: String::new(),
            guidance: "g".to_string(),
            source_type: source_type.to_string(),
            source_context: None,
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
            permission: "\"agent\"".to_string(),
            archived: false,
            created_caller: None,
            agent_visible,
            tools: tools.iter().map(|t| t.to_string()).collect(),
        },
    )
    .unwrap();
}

fn ws_edit_args() -> serde_json::Value {
    json!({"path": "list.csv", "old_string": "a", "new_string": "b"})
}

fn is_owner_rejection(r: &opencrab_core::ActionResult) -> bool {
    r.error
        .as_deref()
        .is_some_and(|e| e.contains("action 'ws_edit' requires owner"))
}

fn projected(exec: &BridgedExecutor) -> Vec<String> {
    exec.list_tools().into_iter().map(|t| t.name).collect()
}

#[tokio::test]
async fn designated_skill_grants_its_tools_to_agent_caller() {
    let (dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
    std::fs::write(dir.path().join("list.csv"), "a\n").unwrap();
    insert_grant_skill(&ctx, "patrol-list", true, &["ws_edit"]);
    let exec = BridgedExecutor::new(ActionDispatcher::new(), ctx);

    // 開く前: owner ゲートで拒否され、投影にも出ない。
    let r = exec.execute("ws_edit", &ws_edit_args()).await;
    assert!(is_owner_rejection(&r), "{r:?}");
    assert!(exec.rejects_before_run("ws_edit"));
    assert!(!projected(&exec).contains(&"ws_edit".to_string()));

    let read = exec
        .execute("read_skill", &json!({"name": "patrol-list"}))
        .await;
    assert!(read.success, "{read:?}");
    assert_eq!(read.data["unlocked_tools"], json!(["ws_edit"]));

    // 開いた後: 実行でき、この run の投影にも出る。
    assert!(!exec.rejects_before_run("ws_edit"));
    assert!(projected(&exec).contains(&"ws_edit".to_string()));
    let r = exec.execute("ws_edit", &ws_edit_args()).await;
    assert!(r.success, "{r:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("list.csv")).unwrap(),
        "b\n"
    );
    // 挙げていないツールは許されない。
    assert!(exec.rejects_before_run("execute_shell"));
}

#[tokio::test]
async fn undesignated_skill_grants_nothing() {
    let (_dir, ctx) = test_context_with_caller(CallerIdentity::TrustedUser);
    insert_grant_skill(&ctx, "private", false, &["ws_edit"]);
    let exec = BridgedExecutor::new(ActionDispatcher::new(), ctx);
    // 指定の無いスキルは非オーナー等価には開けない（D-1058）。開けても何も許さない。
    let read = exec.execute("read_skill", &json!({"name": "private"})).await;
    assert!(read.data.get("unlocked_tools").is_none(), "{read:?}");
    let r = exec.execute("ws_edit", &ws_edit_args()).await;
    assert!(is_owner_rejection(&r), "{r:?}");
    assert!(!projected(&exec).contains(&"ws_edit".to_string()));
}

#[tokio::test]
async fn designated_skill_without_the_tool_grants_nothing_for_it() {
    let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
    insert_grant_skill(&ctx, "patrol-read", true, &["ws_read"]);
    let exec = BridgedExecutor::new(ActionDispatcher::new(), ctx);
    let read = exec
        .execute("read_skill", &json!({"name": "patrol-read"}))
        .await;
    assert!(read.success, "{read:?}");
    assert!(!exec.rejects_before_run("ws_read"));
    let r = exec.execute("ws_edit", &ws_edit_args()).await;
    assert!(is_owner_rejection(&r), "{r:?}");
}

#[tokio::test]
async fn owner_turn_is_unchanged_by_skill_grants() {
    let (dir, ctx) = test_context_with_caller(CallerIdentity::Owner);
    std::fs::write(dir.path().join("list.csv"), "a\n").unwrap();
    let exec = BridgedExecutor::new(ActionDispatcher::new(), ctx);
    assert!(!exec.rejects_before_run("ws_edit"));
    let r = exec.execute("ws_edit", &ws_edit_args()).await;
    assert!(r.success, "{r:?}");
}

/// 許可は caller ゲートだけを外す。run の許可リスト（#368）はそのまま効く。
#[tokio::test]
async fn skill_grant_does_not_bypass_run_allowlist() {
    let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
    insert_grant_skill(&ctx, "patrol-list", true, &["ws_edit"]);
    let exec = BridgedExecutor::new(ActionDispatcher::new(), ctx)
        .with_tool_allowlist(Some(vec!["read_skill".to_string()]));
    let read = exec
        .execute("read_skill", &json!({"name": "patrol-list"}))
        .await;
    assert!(read.success, "{read:?}");
    let r = exec.execute("ws_edit", &ws_edit_args()).await;
    assert!(
        r.error.as_deref().is_some_and(|e| e.contains("tool allowlist")),
        "{r:?}"
    );
}

/// D-1144: エージェントが学んだスキルは誰の会話でも開けるが、`tools` が入っていても
/// （オーナーが指定していなければ）ツールは何も許さない。
#[tokio::test]
async fn learned_skill_is_readable_by_agent_but_grants_nothing() {
    for source_type in opencrab_db::queries::LEARNED_SKILL_SOURCE_TYPES {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
        insert_skill_with_source(&ctx, "learned", source_type, false, &["ws_edit"]);
        let exec = BridgedExecutor::new(ActionDispatcher::new(), ctx);
        let read = exec.execute("read_skill", &json!({"name": "learned"})).await;
        assert!(read.success, "{source_type}: {read:?}");
        assert!(
            read.data.get("unlocked_tools").is_none(),
            "{source_type}: {read:?}"
        );
        let r = exec.execute("ws_edit", &ws_edit_args()).await;
        assert!(is_owner_rejection(&r), "{source_type}: {r:?}");
        assert!(exec.rejects_before_run("ws_edit"));
        assert!(!projected(&exec).contains(&"ws_edit".to_string()));
    }
}
