use super::*;

fn insert_skill_with_source(ctx: &ActionContext, name: &str, source_type: &str) {
    let conn = ctx.db.lock().unwrap();
    opencrab_db::queries::insert_skill(
        &conn,
        &opencrab_db::queries::SkillRow {
            id: format!("id-{name}"),
            agent_id: ctx.agent_id.clone(),
            name: name.to_string(),
            description: format!("{name} desc"),
            situation_pattern: String::new(),
            guidance: format!("{name} guidance"),
            source_type: source_type.to_string(),
            source_context: None,
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
            permission: "\"agent\"".to_string(),
            archived: false,
            created_caller: None,
            agent_visible: false,
            tools: Vec::new(),
        },
    )
    .unwrap();
}

// D-1144: エージェントが学んだスキル（experience / sleep_curated / peer）は、オーナーの指定が
// 無くても caller=Agent / TrustedUser のターンで本文を読める。手で入れた・自分で作った・取り込んだ
// スキルは従来どおり指定が無ければ「owner が要る」。`owner-skills` には学んだスキルを含めない。
#[tokio::test]
async fn learned_skills_are_readable_by_every_caller() {
    let (_dir, owner_ctx) = test_context();
    let agent_ctx = ctx_with_caller(&owner_ctx, CallerIdentity::Agent);
    let trusted_ctx = ctx_with_caller(&owner_ctx, CallerIdentity::TrustedUser);
    for (name, source_type) in [
        ("from-experience", "experience"),
        ("from-sleep", "sleep_curated"),
        ("from-peer", "peer"),
        ("manual-one", "manual"),
        ("self-made", "self_created"),
        ("imported", "openclaw_import"),
        ("acquired-one", "acquired"),
    ] {
        insert_skill_with_source(&owner_ctx, name, source_type);
    }

    for ctx in [&agent_ctx, &trusted_ctx] {
        for name in ["from-experience", "from-sleep", "from-peer"] {
            let ok = ReadSkillAction.execute(&json!({ "name": name }), ctx).await;
            assert!(
                ok.success,
                "caller {:?} must read {name}: {ok:?}",
                ctx.caller
            );
            let data = ok.data.unwrap();
            assert_eq!(data["guidance"], format!("{name} guidance"));
            assert!(data.get("unlocked_tools").is_none(), "{data}");
        }
        for name in ["manual-one", "self-made", "imported", "acquired-one"] {
            let denied = ReadSkillAction.execute(&json!({ "name": name }), ctx).await;
            assert_eq!(
                denied.error.as_deref(),
                Some(format!("skill '{name}' requires owner").as_str()),
                "caller {:?}",
                ctx.caller
            );
        }
    }

    let index = ReadSkillAction
        .execute(&json!({ "name": OWNER_SKILLS_INDEX_NAME }), &owner_ctx)
        .await;
    let names: Vec<String> = index.data.unwrap()["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        ["acquired-one", "imported", "manual-one", "self-made"]
    );
}
