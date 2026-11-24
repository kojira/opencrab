use super::*;

/// usage_count, last_used_at and the sessions in skill_usage_log for `name`.
fn skill_use(ctx: &ActionContext, name: &str) -> (i64, Option<String>, Vec<String>) {
    let conn = ctx.db.lock().unwrap();
    let (id, usage, last_used): (String, i64, Option<String>) = conn
        .query_row(
            "SELECT id, usage_count, last_used_at FROM skills WHERE agent_id = ?1 AND name = ?2",
            rusqlite::params![ctx.agent_id, name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    let sessions = conn
        .prepare("SELECT session_id FROM skill_usage_log WHERE skill_id = ?1 ORDER BY id")
        .unwrap()
        .query_map([&id], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    (usage, last_used, sessions)
}

// D-1146: a skill is used when read_skill returns its body. Each open counts.
#[tokio::test]
async fn read_skill_counts_a_use_for_each_open() {
    let (_dir, ctx) = test_context();
    CreateMySkillAction
        .execute(
            &json!({
                "name": "Deploy Steps",
                "description": "d",
                "situation_pattern": "s",
                "guidance": "g"
            }),
            &ctx,
        )
        .await;
    assert_eq!(skill_use(&ctx, "Deploy Steps"), (0, None, vec![]));

    let r = ReadSkillAction
        .execute(&json!({ "name": "Deploy Steps" }), &ctx)
        .await;
    assert!(r.success);
    let (usage, last_used, sessions) = skill_use(&ctx, "Deploy Steps");
    assert_eq!(usage, 1);
    assert!(last_used.is_some());
    assert_eq!(sessions, ["session-1"]);

    ReadSkillAction
        .execute(&json!({ "name": "Deploy Steps" }), &ctx)
        .await;
    let (usage, _, sessions) = skill_use(&ctx, "Deploy Steps");
    assert_eq!(usage, 2);
    assert_eq!(sessions, ["session-1", "session-1"]);
}

// D-1146: a refused read, a missing skill and the owner-skills index are not uses.
#[tokio::test]
async fn refused_read_and_owner_skills_index_are_not_uses() {
    let (_dir, owner_ctx) = test_context();
    let agent_ctx = ctx_with_caller(&owner_ctx, CallerIdentity::Agent);
    CreateMySkillAction
        .execute(
            &json!({
                "name": "Internal",
                "description": "d",
                "situation_pattern": "s",
                "guidance": "g"
            }),
            &owner_ctx,
        )
        .await;

    let denied = ReadSkillAction
        .execute(&json!({ "name": "Internal" }), &agent_ctx)
        .await;
    assert!(!denied.success);
    let index = ReadSkillAction
        .execute(&json!({ "name": OWNER_SKILLS_INDEX_NAME }), &owner_ctx)
        .await;
    assert!(index.success);
    let missing = ReadSkillAction
        .execute(&json!({ "name": "nope" }), &owner_ctx)
        .await;
    assert!(!missing.success);

    assert_eq!(skill_use(&owner_ctx, "Internal"), (0, None, vec![]));
    let logged: i64 = owner_ctx
        .db
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM skill_usage_log", [], |r| r.get(0))
        .unwrap();
    assert_eq!(logged, 0);
}
