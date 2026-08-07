use rusqlite::{params, Connection};

#[allow(unused_imports)]
use super::*;

#[test]
fn insert_session_dual_writes_agent_sessions() {
    let conn = crate::init_memory().unwrap();
    let session = SessionRow {
        id: "sess-dw".to_string(),
        mode: "discord".to_string(),
        theme: "t".to_string(),
        phase: "active".to_string(),
        turn_number: 0,
        status: "active".to_string(),
        participant_ids_json: "[\"agent-x\",\"agent-y\"]".to_string(),
        facilitator_id: None,
        done_count: 0,
        max_turns: None,
        metadata_json: None,
    };
    insert_session(&conn, &session).unwrap();
    assert_eq!(
        list_session_participants(&conn, "sess-dw").unwrap(),
        vec!["agent-x".to_string(), "agent-y".to_string()]
    );
    assert_eq!(count_sessions_for_agent(&conn, "agent-x").unwrap(), 1);
    // JSON 投影も従来どおり保存されている（wire 契約）
    let row = get_session(&conn, "sess-dw").unwrap().unwrap();
    assert_eq!(row.participant_ids_json, "[\"agent-x\",\"agent-y\"]");
}

fn setup() -> Connection {
    crate::init_memory().expect("failed to init in-memory DB")
}

#[test]
fn test_trusted_user_display_name_round_trip() {
    let conn = setup();
    add_trusted_user(
        &conn,
        TRUSTED_PLATFORM_DISCORD,
        "id-1",
        "a1",
        "42",
        TrustedUserPermission::CoAgent,
        "owner",
        "2026-01-01",
        "Crab B",
    )
    .unwrap();
    let row = get_trusted_user(&conn, TRUSTED_PLATFORM_DISCORD, "42", "a1").unwrap();
    assert_eq!(row.display_name, "Crab B");
    assert_eq!(row.permission, TrustedUserPermission::CoAgent);

    assert!(update_trusted_user_display_name(&conn, "id-1", "Crab B2").unwrap());
    let rows = list_trusted_users(&conn, "a1").unwrap();
    assert_eq!(rows[0].display_name, "Crab B2");

    // v3 以前の行（display_name / platform とも列 DEFAULT）も読み出せる
    conn.execute(
        "INSERT INTO trusted_users (id, user_id, agent_id, permission, created_by, created_at) \
         VALUES ('id-2', '43', 'a1', 'user', 'owner', '2026-01-01')",
        [],
    )
    .unwrap();
    let row = get_trusted_user(&conn, TRUSTED_PLATFORM_DISCORD, "43", "a1").unwrap();
    assert_eq!(row.display_name, "");
    // 列追加前からある行は従来の経路（discord）として生きる（#214）
    assert_eq!(row.platform, TRUSTED_PLATFORM_DISCORD);
}

// ---- 経路（identity platform）で識別子空間が分かれること（#214） ----

/// 1 件登録するテストヘルパ。
fn add_trusted(conn: &Connection, platform: &str, row_id: &str, user_id: &str, agent_id: &str) {
    add_trusted_user(
        conn,
        platform,
        row_id,
        agent_id,
        user_id,
        TrustedUserPermission::User,
        "owner",
        "2026-01-01",
        "",
    )
    .unwrap();
}

/// 同じ識別子でも経路が違えば別扱い（信頼が経路をまたいで引き継がれない）。
#[test]
fn trust_does_not_cross_platforms() {
    let conn = setup();
    // Discord 経路に "42" を登録する。
    add_trusted(&conn, TRUSTED_PLATFORM_DISCORD, "row-d", "42", "a1");
    assert!(is_trusted_user(&conn, TRUSTED_PLATFORM_DISCORD, "42", "a1"));
    // 同じ文字列を web / REST の識別子として名乗っても、その経路では信頼されない。
    assert!(!is_trusted_user(&conn, TRUSTED_PLATFORM_WEB, "42", "a1"));
    assert!(!is_trusted_user(&conn, TRUSTED_PLATFORM_REST, "42", "a1"));
    assert!(get_trusted_user(&conn, TRUSTED_PLATFORM_WEB, "42", "a1").is_none());

    // 逆向きも同じ: web 経路の登録は Discord 経路へ漏れない。
    add_trusted(&conn, TRUSTED_PLATFORM_WEB, "row-w", "dash-user", "a1");
    assert!(is_trusted_user(
        &conn,
        TRUSTED_PLATFORM_WEB,
        "dash-user",
        "a1"
    ));
    assert!(!is_trusted_user(
        &conn,
        TRUSTED_PLATFORM_DISCORD,
        "dash-user",
        "a1"
    ));
}

/// 登録件数の判定も経路で切られている
/// （ある経路に登録があっても、別経路から見れば「0 件」）。
#[test]
fn trusted_user_count_is_scoped_by_platform() {
    let conn = setup();
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_DISCORD, "a1"), 0);

    add_trusted(&conn, TRUSTED_PLATFORM_WEB, "row-w", "dash-user", "a1");
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_WEB, "a1"), 1);
    // web に 1 件あっても Discord から見れば未登録（= owner のみ許可の段が生きる）。
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_DISCORD, "a1"), 0);
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_REST, "a1"), 0);

    add_trusted(&conn, TRUSTED_PLATFORM_DISCORD, "row-d", "42", "a1");
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_DISCORD, "a1"), 1);
    // エージェントでも切れている
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_DISCORD, "a2"), 0);
}

/// 互換読みの撤去（#159）で**何が変わったか**を明示する。
///
/// 撤去前: 従来経路（`discord`）の行しか無いユーザーも web / REST で信頼されていた。
/// 撤去後: 自経路の行が無ければ引けない = そのユーザーは web / REST で権限を失う。
/// ここが緑のままなら、互換読みが別名で復活していないということ。
#[test]
fn legacy_discord_rows_no_longer_grant_trust_on_other_platforms() {
    let conn = setup();
    add_trusted(&conn, TRUSTED_PLATFORM_DISCORD, "row-d", "42", "a1");

    // 従来経路の行は自経路（discord）でだけ効く。
    assert!(get_trusted_user(&conn, TRUSTED_PLATFORM_DISCORD, "42", "a1").is_some());
    // web / REST から同じ識別子で来ても引けない（＝移行前のユーザーは信頼を失う）。
    assert!(get_trusted_user(&conn, TRUSTED_PLATFORM_WEB, "42", "a1").is_none());
    assert!(get_trusted_user(&conn, TRUSTED_PLATFORM_REST, "42", "a1").is_none());

    // 経路ごとの行を登録し直せば、その経路でだけ信頼が戻る。
    add_trusted(&conn, TRUSTED_PLATFORM_WEB, "row-w", "dash-user", "a1");
    let own = get_trusted_user(&conn, TRUSTED_PLATFORM_WEB, "dash-user", "a1").expect("web row");
    assert_eq!(own.platform, TRUSTED_PLATFORM_WEB);
    assert!(get_trusted_user(&conn, TRUSTED_PLATFORM_DISCORD, "dash-user", "a1").is_none());
}

/// 登録 API が受け付ける経路の集合＝読み出し側が引く経路の集合。
///
/// `nostr` は #319 で読み出し側（Nostr 受信ターンの呼び出し元解決）が引くように
/// なったので、登録 API も受け付ける。
#[test]
fn known_platforms_are_exactly_the_read_paths() {
    assert!(is_known_trusted_platform(TRUSTED_PLATFORM_DISCORD));
    assert!(is_known_trusted_platform(TRUSTED_PLATFORM_WEB));
    assert!(is_known_trusted_platform(TRUSTED_PLATFORM_REST));
    assert!(is_known_trusted_platform(TRUSTED_PLATFORM_NOSTR));
    // 綴り間違い・未定義の経路は弾く（登録できても誰とも一致しない行になるため）。
    assert!(!is_known_trusted_platform("Discord"));
    assert!(!is_known_trusted_platform("Nostr"));
    assert!(!is_known_trusted_platform("mastodon"));
    assert!(!is_known_trusted_platform(""));
}

/// ロスターも経路で切られている（#159: 受理ゲートと揃えた）。
#[test]
fn co_agent_roster_is_scoped_by_platform() {
    let conn = setup();
    add_trusted_user(
        &conn,
        TRUSTED_PLATFORM_DISCORD,
        "row-d",
        "a1",
        "42",
        TrustedUserPermission::CoAgent,
        "owner",
        "2026-01-01",
        "Crab D",
    )
    .unwrap();
    add_trusted_user(
        &conn,
        TRUSTED_PLATFORM_WEB,
        "row-w",
        "a1",
        "dash-user",
        TrustedUserPermission::CoAgent,
        "owner",
        "2026-01-01",
        "Crab W",
    )
    .unwrap();

    let discord = list_co_agent_reviewers(&conn, TRUSTED_PLATFORM_DISCORD, "a1").unwrap();
    assert_eq!(discord.len(), 1);
    assert_eq!(discord[0].display_name, "Crab D");

    let web = list_co_agent_reviewers(&conn, TRUSTED_PLATFORM_WEB, "a1").unwrap();
    assert_eq!(web.len(), 1);
    assert_eq!(web[0].display_name, "Crab W");

    assert!(list_co_agent_reviewers(&conn, TRUSTED_PLATFORM_REST, "a1")
        .unwrap()
        .is_empty());

    // permission と agent_id の絞り込みは維持されている。
    add_trusted(&conn, TRUSTED_PLATFORM_DISCORD, "row-u", "43", "a1");
    assert_eq!(
        list_co_agent_reviewers(&conn, TRUSTED_PLATFORM_DISCORD, "a1")
            .unwrap()
            .len(),
        1
    );
    assert!(
        list_co_agent_reviewers(&conn, TRUSTED_PLATFORM_DISCORD, "a2")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn test_task_ledger_insert_and_get_active() {
    let conn = setup();
    let id =
        insert_task_ledger(&conn, "a1", "s1", "build feature", Some("tests pass")).expect("insert");

    let task = get_active_task_for_session(&conn, "a1", "s1")
        .expect("query")
        .expect("active task");
    assert_eq!(task.id, id);
    assert_eq!(task.goal, "build feature");
    assert_eq!(task.contract.as_deref(), Some("tests pass"));
    assert_eq!(task.status, "active");

    // 別セッション / 別エージェントからは見えない
    assert!(get_active_task_for_session(&conn, "a1", "s2")
        .unwrap()
        .is_none());
    assert!(get_active_task_for_session(&conn, "a2", "s1")
        .unwrap()
        .is_none());
    assert!(get_task_ledger(&conn, "a2", id).unwrap().is_none());
}

#[test]
fn test_task_ledger_status_update() {
    let conn = setup();
    let id = insert_task_ledger(&conn, "a1", "s1", "g", None).unwrap();

    assert!(update_task_status(&conn, "a1", id, "done").unwrap());
    assert!(get_active_task_for_session(&conn, "a1", "s1")
        .unwrap()
        .is_none());
    let task = get_task_ledger(&conn, "a1", id).unwrap().unwrap();
    assert_eq!(task.status, "done");

    // 未知の id / 他エージェントは Ok(false)
    assert!(!update_task_status(&conn, "a1", 9999, "done").unwrap());
    assert!(!update_task_status(&conn, "a2", id, "abandoned").unwrap());
}

#[test]
fn test_task_ledger_restart_count_increment() {
    let conn = setup();
    let id = insert_task_ledger(&conn, "a1", "s1", "g", None).unwrap();

    // 新規タスクは 0 から始まる
    let task = get_task_ledger(&conn, "a1", id).unwrap().unwrap();
    assert_eq!(task.restart_count, 0);

    assert!(increment_task_restart_count(&conn, "a1", id).unwrap());
    assert!(increment_task_restart_count(&conn, "a1", id).unwrap());
    let task = get_task_ledger(&conn, "a1", id).unwrap().unwrap();
    assert_eq!(task.restart_count, 2);

    // 未知の id / 他エージェントは Ok(false)（カウントは動かない）
    assert!(!increment_task_restart_count(&conn, "a1", 9999).unwrap());
    assert!(!increment_task_restart_count(&conn, "a2", id).unwrap());
    let task = get_task_ledger(&conn, "a1", id).unwrap().unwrap();
    assert_eq!(task.restart_count, 2);
}

#[test]
fn test_task_ledger_update_goal_contract() {
    let conn = setup();
    let id = insert_task_ledger(&conn, "a1", "s1", "old goal", Some("old contract")).unwrap();

    // contract のみ更新 → goal は据え置き
    assert!(update_task_goal_contract(&conn, "a1", id, None, Some("new contract")).unwrap());
    let task = get_task_ledger(&conn, "a1", id).unwrap().unwrap();
    assert_eq!(task.goal, "old goal");
    assert_eq!(task.contract.as_deref(), Some("new contract"));

    // goal のみ更新 → contract は据え置き
    assert!(update_task_goal_contract(&conn, "a1", id, Some("new goal"), None).unwrap());
    let task = get_task_ledger(&conn, "a1", id).unwrap().unwrap();
    assert_eq!(task.goal, "new goal");
    assert_eq!(task.contract.as_deref(), Some("new contract"));
}

#[test]
fn test_task_ledger_second_active_insert_rejected_by_db() {
    let conn = setup();
    insert_task_ledger(&conn, "a1", "s1", "first", None).unwrap();
    // 部分ユニークインデックスにより同一セッションの2件目の active は DB 層で拒否される
    let err = insert_task_ledger(&conn, "a1", "s1", "second", None).unwrap_err();
    assert!(err.to_string().contains("UNIQUE constraint failed"));
    // close 後は再度 open できる
    let first = get_active_task_for_session(&conn, "a1", "s1")
        .unwrap()
        .unwrap();
    assert!(update_task_status(&conn, "a1", first.id, "done").unwrap());
    insert_task_ledger(&conn, "a1", "s1", "second", None).unwrap();
}

#[test]
fn test_task_progress_bumps_ledger_updated_at() {
    let conn = setup();
    let id = insert_task_ledger(&conn, "a1", "s1", "g", None).unwrap();
    let before = get_task_ledger(&conn, "a1", id)
        .unwrap()
        .unwrap()
        .updated_at;
    insert_task_progress(&conn, id, "progress", "step").unwrap();
    let after = get_task_ledger(&conn, "a1", id)
        .unwrap()
        .unwrap()
        .updated_at;
    assert!(after > before, "updated_at must advance on progress append");
}

#[test]
fn test_task_progress_append_count_and_recent() {
    let conn = setup();
    let id = insert_task_ledger(&conn, "a1", "s1", "g", None).unwrap();
    for i in 1..=15 {
        insert_task_progress(&conn, id, "progress", &format!("step {i}")).unwrap();
    }

    assert_eq!(count_task_progress(&conn, id).unwrap(), 15);
    let recent = list_recent_task_progress(&conn, id, 10).unwrap();
    assert_eq!(recent.len(), 10);
    // 直近10件が時系列順（step 6 .. step 15）
    assert_eq!(recent.first().unwrap().content, "step 6");
    assert_eq!(recent.last().unwrap().content, "step 15");
}

#[test]
fn test_task_progress_cascade_delete() {
    let conn = setup();
    // init_memory は configure_connection を通らないため FK を明示的に有効化する
    conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
    let id = insert_task_ledger(&conn, "a1", "s1", "g", None).unwrap();
    insert_task_progress(&conn, id, "progress", "p1").unwrap();

    conn.execute("DELETE FROM task_ledger WHERE id = ?1", params![id])
        .unwrap();
    assert_eq!(count_task_progress(&conn, id).unwrap(), 0);
}

#[test]
fn test_agent_upsert_and_get() {
    let conn = setup();
    let agent = AgentRow {
        agent_id: "agent-1".to_string(),
        name: "Alice".to_string(),
        job_title: Some("Engineer".to_string()),
        organization: Some("OpenCrab Inc.".to_string()),
        image_url: Some("https://example.com/avatar.png".to_string()),
        persona_name: "Crab".to_string(),
        personality: Some(r#"{"hobby":"coding"}"#.to_string()),
        instructions: String::new(),
        heartbeat_instructions: String::new(),
        model: None,
        reasoning_effort: None,
        web_search: None,
        metadata_json: Some(r#"{"lang":"en"}"#.to_string()),
    };

    upsert_agent(&conn, &agent).unwrap();

    let fetched = get_agent(&conn, "agent-1").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.agent_id, "agent-1");
    assert_eq!(fetched.name, "Alice");
    assert_eq!(fetched.persona_name, "Crab");
    assert_eq!(
        fetched.personality,
        Some(r#"{"hobby":"coding"}"#.to_string())
    );
    assert_eq!(fetched.job_title, Some("Engineer".to_string()));
    assert_eq!(
        fetched.image_url,
        Some("https://example.com/avatar.png".to_string())
    );
    assert_eq!(fetched.metadata_json, Some(r#"{"lang":"en"}"#.to_string()));
}

#[test]
fn test_agent_get_nonexistent() {
    let conn = setup();
    let result = get_agent(&conn, "nonexistent-agent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_effective_model_for_agent() {
    let conn = setup();
    let agent = AgentRow {
        agent_id: "a1".to_string(),
        name: "N".to_string(),
        job_title: None,
        organization: None,
        image_url: None,
        persona_name: "p".to_string(),
        personality: None,
        instructions: String::new(),
        heartbeat_instructions: String::new(),
        model: Some("openai:gpt-4o".to_string()),
        reasoning_effort: None,
        web_search: None,
        metadata_json: None,
    };
    upsert_agent(&conn, &agent).unwrap();
    let m = effective_model_for_agent(&conn, "a1", "anthropic:claude").unwrap();
    assert_eq!(m, "openai:gpt-4o");
    let m2 = effective_model_for_agent(&conn, "a1", "anthropic:claude").unwrap();
    assert_eq!(m2, "openai:gpt-4o");

    let agent2 = AgentRow {
        agent_id: "a2".to_string(),
        name: "N2".to_string(),
        job_title: None,
        organization: None,
        image_url: None,
        persona_name: "p".to_string(),
        personality: None,
        instructions: String::new(),
        heartbeat_instructions: String::new(),
        model: None,
        reasoning_effort: None,
        web_search: None,
        metadata_json: None,
    };
    upsert_agent(&conn, &agent2).unwrap();
    let m3 = effective_model_for_agent(&conn, "a2", "global:default").unwrap();
    assert_eq!(m3, "global:default");
}

// 4. test_curated_memory_crud
#[test]
fn test_curated_memory_crud() {
    let conn = setup();

    let mem1 = CuratedMemoryRow {
        id: "mem-1".to_string(),
        agent_id: "agent-1".to_string(),
        category: "facts".to_string(),
        content: "Rust is a systems programming language.".to_string(),
        created_at: String::new(),
    };
    let mem2 = CuratedMemoryRow {
        id: "mem-2".to_string(),
        agent_id: "agent-1".to_string(),
        category: "facts".to_string(),
        content: "Crabs have ten legs.".to_string(),
        created_at: String::new(),
    };

    upsert_curated_memory(&conn, &mem1).unwrap();
    upsert_curated_memory(&conn, &mem2).unwrap();

    let results = get_curated_memories(&conn, "agent-1", "facts").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content, "Crabs have ten legs.");
}

// 5. test_curated_memory_list_all
#[test]
fn test_curated_memory_list_all() {
    let conn = setup();

    let mem1 = CuratedMemoryRow {
        id: "mem-1".to_string(),
        agent_id: "agent-1".to_string(),
        category: "facts".to_string(),
        content: "The sky is blue.".to_string(),
        created_at: String::new(),
    };
    let mem2 = CuratedMemoryRow {
        id: "mem-2".to_string(),
        agent_id: "agent-1".to_string(),
        category: "opinions".to_string(),
        content: "Rust is great.".to_string(),
        created_at: String::new(),
    };

    upsert_curated_memory(&conn, &mem1).unwrap();
    upsert_curated_memory(&conn, &mem2).unwrap();

    let (all, _total) = list_curated_memories(&conn, "agent-1", 10000, 0).unwrap();
    assert_eq!(all.len(), 2);

    let categories: Vec<&str> = all.iter().map(|m| m.category.as_str()).collect();
    assert!(categories.contains(&"facts"));
    assert!(categories.contains(&"opinions"));
}

// 6. test_session_log_insert_and_fts
#[test]
fn test_session_log_insert_and_fts() {
    let conn = setup();

    let log1 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "The weather is sunny today.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(1),
        metadata_json: None,
        created_at: None,
    };
    let log2 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "I enjoy programming in Rust.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(2),
        metadata_json: None,
        created_at: None,
    };
    let log3 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "Crabs live near the ocean.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(3),
        metadata_json: None,
        created_at: None,
    };

    insert_session_log(&conn, &log1).unwrap();
    insert_session_log(&conn, &log2).unwrap();
    insert_session_log(&conn, &log3).unwrap();

    let results = search_session_logs(&conn, "agent-1", "sunny", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("sunny"));
}

/// #425: エコー行（表示専用の二重記録）は memory_sessions には入るが FTS には載せない。
/// 記憶検索（search_session_logs / count_matching_session_logs）に二重ヒット・過大計上を
/// 出さない。本体テーブルには残る（会話文脈の表示に使う）。
#[test]
fn heartbeat_channel_echo_excluded_from_fts_but_kept_in_table() {
    let conn = setup();

    // 通常の発話（印なし）と、同内容のエコー行（印つき）を入れる。
    let normal = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "discord-agent-1-111-222".to_string(),
        log_type: "speech".to_string(),
        content: "pineapple diagnostics report".to_string(),
        speaker_id: Some("human-9".to_string()),
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    let echo = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "discord-agent-1-111-222".to_string(),
        log_type: "speech".to_string(),
        content: "pineapple diagnostics report".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: None,
        metadata_json: Some(HEARTBEAT_CHANNEL_ECHO_METADATA.to_string()),
        created_at: None,
    };
    insert_session_log(&conn, &normal).unwrap();
    insert_session_log(&conn, &echo).unwrap();

    // FTS 検索は印なしの 1 件だけ（エコーは載らない ＝ 二重ヒットしない）。
    let results = search_session_logs(&conn, "agent-1", "pineapple", 10).unwrap();
    assert_eq!(
        results.len(),
        1,
        "エコー行は FTS に載らないので検索ヒットは 1 件だけ: {results:?}"
    );
    assert_eq!(
        count_matching_session_logs(&conn, "agent-1", "pineapple").unwrap(),
        1,
        "count も過大計上しない"
    );

    // 本体テーブルには両方残る（会話文脈の表示に使う）。
    let rows = list_session_logs_by_session(&conn, "discord-agent-1-111-222").unwrap();
    assert_eq!(rows.len(), 2, "本体テーブルには印つき行も残る: {rows:?}");
}

/// #425: 判定は `source` フィールドの値で行い、キー順・空白の違いに強い。
/// 無関係な metadata（tool_call 等）・None は false。
#[test]
fn is_heartbeat_channel_echo_matches_on_source_field() {
    // 書き手が入れる正準形。
    assert!(is_heartbeat_channel_echo(Some(
        HEARTBEAT_CHANNEL_ECHO_METADATA
    )));
    // 空白・キー順が違っても source の値で判定する。
    assert!(is_heartbeat_channel_echo(Some(
        r#"{ "extra": 1, "source" : "heartbeat_channel_echo" }"#
    )));
    // 無関係な metadata は false（substring ゲートで早期に弾かれる）。
    assert!(!is_heartbeat_channel_echo(Some(
        r#"{"tool_calls_json":"[...]"}"#
    )));
    // source の値が別物なら false。
    assert!(!is_heartbeat_channel_echo(Some(r#"{"source":"other"}"#)));
    // None・空・壊れた JSON は false。
    assert!(!is_heartbeat_channel_echo(None));
    assert!(!is_heartbeat_channel_echo(Some("")));
    assert!(!is_heartbeat_channel_echo(Some("not json")));
}

// 7. test_fts_multi_word_search
#[test]
fn test_fts_multi_word_search() {
    let conn = setup();

    let log1 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "Quantum computing will revolutionize cryptography.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(1),
        metadata_json: None,
        created_at: None,
    };
    let log2 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "Classical computing is still dominant.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(2),
        metadata_json: None,
        created_at: None,
    };

    insert_session_log(&conn, &log1).unwrap();
    insert_session_log(&conn, &log2).unwrap();

    let results = search_session_logs(&conn, "agent-1", "quantum cryptography", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("Quantum"));
}

// 8. test_fts_no_results
#[test]
fn test_fts_no_results() {
    let conn = setup();

    let log = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "Hello world from the test.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(1),
        metadata_json: None,
        created_at: None,
    };
    insert_session_log(&conn, &log).unwrap();

    let results = search_session_logs(&conn, "agent-1", "nonexistenttermxyz", 10).unwrap();
    assert!(results.is_empty());
}

// 9. test_skills_crud
#[test]
fn test_skills_crud() {
    let conn = setup();

    let skill = SkillRow {
        id: "skill-1".to_string(),
        agent_id: "agent-1".to_string(),
        name: "Summarization".to_string(),
        description: "Summarize long texts concisely.".to_string(),
        situation_pattern: "when asked to summarize".to_string(),
        guidance: "Extract key points and present them briefly.".to_string(),
        source_type: "acquired".to_string(),
        source_context: Some("learned from session-1".to_string()),
        file_path: None,
        effectiveness: None,
        usage_count: 0,
        is_active: true,
        permission: "\"agent\"".to_string(),
        archived: false,
        created_caller: None,
        agent_visible: false,
    };

    insert_skill(&conn, &skill).unwrap();

    let skills = list_skills(&conn, "agent-1", true).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id, "skill-1");
    assert_eq!(skills[0].name, "Summarization");
    assert!(skills[0].is_active);
    assert_eq!(skills[0].usage_count, 0);
    assert_eq!(skills[0].source_type, "acquired");
}

// 9b. created_caller が insert / select / update で往復すること（#335）。
#[test]
fn test_skill_created_caller_roundtrip() {
    let conn = setup();

    let mut skill = SkillRow {
        id: "skill-cc".to_string(),
        agent_id: "agent-1".to_string(),
        name: "Gated".to_string(),
        description: "d".to_string(),
        situation_pattern: String::new(),
        guidance: "g".to_string(),
        source_type: "self_created".to_string(),
        source_context: None,
        file_path: None,
        effectiveness: None,
        usage_count: 0,
        is_active: true,
        permission: "\"agent\"".to_string(),
        archived: false,
        created_caller: Some("agent".to_string()),
        agent_visible: false,
    };
    insert_skill(&conn, &skill).unwrap();

    let got = find_skill_by_id(&conn, "skill-cc").unwrap().unwrap();
    assert_eq!(got.created_caller.as_deref(), Some("agent"));

    // update_skill も created_caller を書き戻す。
    skill.created_caller = Some("owner".to_string());
    update_skill(&conn, &skill).unwrap();
    let got = find_skill_by_id(&conn, "skill-cc").unwrap().unwrap();
    assert_eq!(got.created_caller.as_deref(), Some("owner"));

    // NULL（legacy）も往復する。
    let legacy = SkillRow {
        id: "skill-legacy".to_string(),
        created_caller: None,
        ..skill.clone()
    };
    // 別 id / 別 name で入れ直す（同名 UNIQUE 衝突回避）。
    let legacy = SkillRow {
        name: "LegacyGate".to_string(),
        ..legacy
    };
    insert_skill(&conn, &legacy).unwrap();
    let got = find_skill_by_id(&conn, "skill-legacy").unwrap().unwrap();
    assert!(got.created_caller.is_none());
}

// 10. test_skill_usage_increment
#[test]
fn test_skill_usage_increment() {
    let conn = setup();

    let skill = SkillRow {
        id: "skill-1".to_string(),
        agent_id: "agent-1".to_string(),
        name: "Translation".to_string(),
        description: "Translate between languages.".to_string(),
        situation_pattern: "when translation is needed".to_string(),
        guidance: "Use context-aware translation.".to_string(),
        source_type: "acquired".to_string(),
        source_context: None,
        file_path: None,
        effectiveness: None,
        usage_count: 0,
        is_active: true,
        permission: "\"agent\"".to_string(),
        archived: false,
        created_caller: None,
        agent_visible: false,
    };

    insert_skill(&conn, &skill).unwrap();
    increment_skill_usage(&conn, "skill-1").unwrap();

    let skills = list_skills(&conn, "agent-1", true).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].usage_count, 1);
}

// 11a. test_find_skill_by_name_any_includes_archived
#[test]
fn test_find_skill_by_name_any_includes_archived() {
    let conn = setup();

    let skill = SkillRow {
        id: "skill-arch-1".to_string(),
        agent_id: "agent-1".to_string(),
        name: "ArchivedSkill".to_string(),
        description: "Some description".to_string(),
        situation_pattern: "".to_string(),
        guidance: "".to_string(),
        source_type: "acquired".to_string(),
        source_context: None,
        file_path: None,
        effectiveness: None,
        usage_count: 0,
        is_active: false,
        permission: "\"agent\"".to_string(),
        archived: true,
        created_caller: None,
        agent_visible: false,
    };
    insert_skill(&conn, &skill).unwrap();

    // find_skill_by_name should NOT find archived
    let not_found = find_skill_by_name(&conn, "agent-1", "ArchivedSkill").unwrap();
    assert!(
        not_found.is_none(),
        "find_skill_by_name should not find archived skill"
    );

    // find_skill_by_name_any SHOULD find archived
    let found = find_skill_by_name_any(&conn, "agent-1", "ArchivedSkill").unwrap();
    assert!(
        found.is_some(),
        "find_skill_by_name_any should find archived skill"
    );
    assert_eq!(found.unwrap().archived, true);
}

// 11b. test_update_skill_full_fields
#[test]
fn test_update_skill_full_fields() {
    let conn = setup();

    let skill = SkillRow {
        id: "skill-upd-1".to_string(),
        agent_id: "agent-1".to_string(),
        name: "UpdateMe".to_string(),
        description: "Original description".to_string(),
        situation_pattern: "original pattern".to_string(),
        guidance: "original guidance".to_string(),
        source_type: "acquired".to_string(),
        source_context: None,
        file_path: None,
        effectiveness: None,
        usage_count: 0,
        is_active: true,
        permission: "\"agent\"".to_string(),
        archived: true,
        created_caller: None,
        agent_visible: false,
    };
    insert_skill(&conn, &skill).unwrap();

    // Update with new values including archived=false restore
    let mut updated = skill.clone();
    updated.description = "Updated description".to_string();
    updated.guidance = "Updated guidance".to_string();
    updated.archived = false;
    updated.is_active = true;
    update_skill(&conn, &updated).unwrap();

    let found = find_skill_by_name(&conn, "agent-1", "UpdateMe").unwrap();
    assert!(found.is_some(), "should find restored skill");
    let s = found.unwrap();
    assert_eq!(s.description, "Updated description");
    assert_eq!(s.guidance, "Updated guidance");
    assert_eq!(s.archived, false);
    assert_eq!(s.is_active, true);
}

// 11. test_impressions_upsert_and_get
#[test]
fn test_impressions_upsert_and_get() {
    let conn = setup();

    let impression = ImpressionRow {
        id: "imp-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        target_id: "agent-2".to_string(),
        target_name: "Bob".to_string(),
        personality: "thoughtful and calm".to_string(),
        communication_style: "concise".to_string(),
        recent_behavior: "asked good questions".to_string(),
        agreement: "mostly agree".to_string(),
        notes: "potential collaborator".to_string(),
        last_updated_turn: 5,
    };

    upsert_impression(&conn, &impression).unwrap();

    let results = get_impressions(&conn, "agent-1").unwrap();
    assert_eq!(results.len(), 1);
    let fetched = &results[0];
    assert_eq!(fetched.id, "imp-1");
    assert_eq!(fetched.target_id, "agent-2");
    assert_eq!(fetched.target_name, "Bob");
    assert_eq!(fetched.personality, "thoughtful and calm");
    assert_eq!(fetched.communication_style, "concise");
    assert_eq!(fetched.recent_behavior, "asked good questions");
    assert_eq!(fetched.agreement, "mostly agree");
    assert_eq!(fetched.notes, "potential collaborator");
    assert_eq!(fetched.last_updated_turn, 5);
}

/// 人物像は agent スコープ（#314）: 別セッションで書いても同じ 1 行を更新し、
/// どのセッションからでも同じ内容が読める。
#[test]
fn test_impressions_are_agent_scoped_across_sessions() {
    let conn = setup();

    let base = ImpressionRow {
        id: "imp-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: "discord-1".to_string(),
        target_id: "person-x".to_string(),
        target_name: "Bob".to_string(),
        personality: "thoughtful".to_string(),
        communication_style: String::new(),
        recent_behavior: String::new(),
        agreement: "中立".to_string(),
        notes: String::new(),
        last_updated_turn: 1,
    };
    upsert_impression(&conn, &base).unwrap();

    // 別セッション・別経路から同じ相手を更新しても行は増えない。
    let updated = ImpressionRow {
        id: "imp-2".to_string(),
        session_id: "nostr-1".to_string(),
        personality: "thoughtful and warm".to_string(),
        ..base.clone()
    };
    upsert_impression(&conn, &updated).unwrap();

    let all = get_impressions(&conn, "agent-1").unwrap();
    assert_eq!(all.len(), 1, "same person must stay a single row");
    // 既存行の id は保たれ、内容と「最後に更新したセッション」が更新される。
    assert_eq!(all[0].id, "imp-1");
    assert_eq!(all[0].personality, "thoughtful and warm");
    assert_eq!(all[0].session_id, "nostr-1");

    let one = get_impression(&conn, "agent-1", "person-x")
        .unwrap()
        .expect("impression");
    assert_eq!(one.personality, "thoughtful and warm");

    // 別エージェント / 別の相手とは混ざらない。
    assert!(get_impression(&conn, "agent-2", "person-x")
        .unwrap()
        .is_none());
    assert!(get_impression(&conn, "agent-1", "person-y")
        .unwrap()
        .is_none());
}

// 12. test_session_crud
#[test]
fn test_session_crud() {
    let conn = setup();

    let session = SessionRow {
        id: "session-1".to_string(),
        mode: "facilitated".to_string(),
        theme: "AI Ethics Discussion".to_string(),
        phase: "divergent".to_string(),
        turn_number: 0,
        status: "active".to_string(),
        participant_ids_json: r#"["agent-1","agent-2"]"#.to_string(),
        facilitator_id: Some("agent-1".to_string()),
        done_count: 0,
        max_turns: Some(10),
        metadata_json: None,
    };

    insert_session(&conn, &session).unwrap();

    let fetched = get_session(&conn, "session-1").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.id, "session-1");
    assert_eq!(fetched.mode, "facilitated");
    assert_eq!(fetched.theme, "AI Ethics Discussion");
    assert_eq!(fetched.phase, "divergent");
    assert_eq!(fetched.turn_number, 0);
    assert_eq!(fetched.status, "active");
    assert_eq!(fetched.facilitator_id, Some("agent-1".to_string()));
    assert_eq!(fetched.max_turns, Some(10));

    let all = list_sessions(&conn).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, "session-1");
}

// 13. test_llm_metrics_insert_and_summary
#[test]
fn test_llm_metrics_insert_and_summary() {
    let conn = setup();

    let metrics1 = LlmMetricsRow {
        id: "metrics-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("session-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4".to_string(),
        purpose: "discussion".to_string(),
        task_type: Some("chat".to_string()),
        complexity: Some("medium".to_string()),
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        estimated_cost_usd: 0.005,
        latency_ms: 1200,
        time_to_first_token_ms: Some(200),
    };

    let metrics2 = LlmMetricsRow {
        id: "metrics-2".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("session-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4".to_string(),
        purpose: "summarization".to_string(),
        task_type: Some("summary".to_string()),
        complexity: Some("low".to_string()),
        input_tokens: 200,
        output_tokens: 80,
        total_tokens: 280,
        estimated_cost_usd: 0.008,
        latency_ms: 800,
        time_to_first_token_ms: Some(150),
    };

    insert_llm_metrics(&conn, &metrics1).unwrap();
    insert_llm_metrics(&conn, &metrics2).unwrap();

    let summary = get_llm_metrics_summary(&conn, "agent-1", "2020-01-01").unwrap();
    assert_eq!(summary.count, 2);
    assert_eq!(summary.total_tokens, Some(430));
    let total_cost = summary.total_cost.unwrap();
    assert!((total_cost - 0.013).abs() < 1e-9);
    let avg_latency = summary.avg_latency.unwrap();
    assert!((avg_latency - 1000.0).abs() < 1e-9);
}

// 14. test_llm_metrics_evaluation_update
#[test]
fn test_llm_metrics_evaluation_update() {
    let conn = setup();

    let metrics = LlmMetricsRow {
        id: "metrics-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("session-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4".to_string(),
        purpose: "discussion".to_string(),
        task_type: Some("chat".to_string()),
        complexity: Some("medium".to_string()),
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        estimated_cost_usd: 0.005,
        latency_ms: 1200,
        time_to_first_token_ms: Some(200),
    };

    insert_llm_metrics(&conn, &metrics).unwrap();
    update_llm_metrics_evaluation(&conn, "metrics-1", 0.95, true, "excellent response").unwrap();

    // Read back via raw SQL to verify the evaluation columns
    let (quality_score, task_success, self_evaluation): (f64, i32, String) = conn
        .query_row(
            "SELECT quality_score, task_success, self_evaluation FROM llm_usage_metrics WHERE id = ?1",
            params!["metrics-1"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    assert!((quality_score - 0.95).abs() < 1e-9);
    assert_eq!(task_success, 1);
    assert_eq!(self_evaluation, "excellent response");
}

// 14b. test_llm_metrics_by_model
#[test]
fn test_llm_metrics_by_model() {
    let conn = setup();

    let m1 = LlmMetricsRow {
        id: "m-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("s-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        purpose: "conversation".to_string(),
        task_type: Some("chat".to_string()),
        complexity: Some("medium".to_string()),
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        estimated_cost_usd: 0.005,
        latency_ms: 1200,
        time_to_first_token_ms: Some(200),
    };
    let m2 = LlmMetricsRow {
        id: "m-2".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("s-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        purpose: "conversation".to_string(),
        task_type: Some("chat".to_string()),
        complexity: Some("low".to_string()),
        input_tokens: 80,
        output_tokens: 40,
        total_tokens: 120,
        estimated_cost_usd: 0.001,
        latency_ms: 400,
        time_to_first_token_ms: Some(100),
    };
    let m3 = LlmMetricsRow {
        id: "m-3".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("s-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        purpose: "analysis".to_string(),
        task_type: Some("summary".to_string()),
        complexity: Some("low".to_string()),
        input_tokens: 60,
        output_tokens: 30,
        total_tokens: 90,
        estimated_cost_usd: 0.0008,
        latency_ms: 300,
        time_to_first_token_ms: Some(80),
    };

    insert_llm_metrics(&conn, &m1).unwrap();
    insert_llm_metrics(&conn, &m2).unwrap();
    insert_llm_metrics(&conn, &m3).unwrap();

    let stats = get_llm_metrics_by_model(&conn, "agent-1", "2020-01-01").unwrap();
    assert_eq!(stats.len(), 2);

    // gpt-4o-mini has 2 records, gpt-4o has 1 → sorted by count DESC
    assert_eq!(stats[0].model, "gpt-4o-mini");
    assert_eq!(stats[0].count, 2);
    assert_eq!(stats[0].total_tokens, 210);
    assert!((stats[0].total_cost - 0.0018).abs() < 1e-9);

    assert_eq!(stats[1].model, "gpt-4o");
    assert_eq!(stats[1].count, 1);
}

// 14c. test_llm_metrics_by_model_and_purpose
#[test]
fn test_llm_metrics_by_model_and_purpose() {
    let conn = setup();

    // gpt-4o for conversation
    let m1 = LlmMetricsRow {
        id: "mp-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("s-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        purpose: "conversation".to_string(),
        task_type: Some("chat".to_string()),
        complexity: None,
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        estimated_cost_usd: 0.005,
        latency_ms: 2000,
        time_to_first_token_ms: None,
    };
    // gpt-4o for analysis
    let m2 = LlmMetricsRow {
        id: "mp-2".to_string(),
        purpose: "analysis".to_string(),
        estimated_cost_usd: 0.008,
        latency_ms: 3000,
        ..m1.clone()
    };
    // gpt-4o-mini for conversation
    let m3 = LlmMetricsRow {
        id: "mp-3".to_string(),
        model: "gpt-4o-mini".to_string(),
        purpose: "conversation".to_string(),
        estimated_cost_usd: 0.001,
        latency_ms: 400,
        ..m1.clone()
    };
    // gpt-4o-mini for analysis
    let m4 = LlmMetricsRow {
        id: "mp-4".to_string(),
        model: "gpt-4o-mini".to_string(),
        purpose: "analysis".to_string(),
        estimated_cost_usd: 0.0015,
        latency_ms: 500,
        ..m1.clone()
    };

    insert_llm_metrics(&conn, &m1).unwrap();
    insert_llm_metrics(&conn, &m2).unwrap();
    insert_llm_metrics(&conn, &m3).unwrap();
    insert_llm_metrics(&conn, &m4).unwrap();

    let stats = get_llm_metrics_by_model_and_purpose(&conn, "agent-1", "2020-01-01").unwrap();
    // Should have 4 entries: (gpt-4o, analysis), (gpt-4o, conversation), (gpt-4o-mini, analysis), (gpt-4o-mini, conversation)
    assert_eq!(stats.len(), 4);

    // Verify each entry has correct purpose.
    let purposes: Vec<&str> = stats.iter().map(|s| s.purpose.as_str()).collect();
    assert!(purposes.contains(&"conversation"));
    assert!(purposes.contains(&"analysis"));

    // Verify we can distinguish same model in different purposes.
    let gpt4o_conv = stats
        .iter()
        .find(|s| s.model == "gpt-4o" && s.purpose == "conversation")
        .unwrap();
    let gpt4o_anl = stats
        .iter()
        .find(|s| s.model == "gpt-4o" && s.purpose == "analysis")
        .unwrap();
    assert!((gpt4o_conv.total_cost - 0.005).abs() < 1e-9);
    assert!((gpt4o_anl.total_cost - 0.008).abs() < 1e-9);
}

// 15. test_model_pricing_upsert_and_get
#[test]
fn test_model_pricing_upsert_and_get() {
    let conn = setup();

    let pricing = ModelPricingRow {
        provider: "openai".to_string(),
        model: "gpt-4".to_string(),
        input_price_per_1m: 30.0,
        output_price_per_1m: 60.0,
        context_window: Some(128000),
    };

    upsert_model_pricing(&conn, &pricing).unwrap();

    let fetched = get_model_pricing(&conn, "openai", "gpt-4").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.provider, "openai");
    assert_eq!(fetched.model, "gpt-4");
    assert!((fetched.input_price_per_1m - 30.0).abs() < 1e-9);
    assert!((fetched.output_price_per_1m - 60.0).abs() < 1e-9);
    assert_eq!(fetched.context_window, Some(128000));
}

// 16. test_heartbeat_log_insert
#[test]
fn test_heartbeat_log_insert() {
    let conn = setup();

    let result = insert_heartbeat_log(&conn, "agent-1", "idle", Some(r#"{"action":"none"}"#));
    assert!(result.is_ok());
}

// ── delete_agent ──

#[test]
fn test_delete_agent() {
    let conn = setup();

    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: "del-1".into(),
            name: "DeleteMe".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "Doomed".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    upsert_curated_memory(
        &conn,
        &CuratedMemoryRow {
            id: "cm-del-1".into(),
            agent_id: "del-1".into(),
            category: "fact".into(),
            content: "will be deleted".into(),
            created_at: String::new(),
        },
    )
    .unwrap();

    assert!(get_agent(&conn, "del-1").unwrap().is_some());

    let deleted = delete_agent(&conn, "del-1").unwrap();
    assert!(deleted);

    assert!(get_agent(&conn, "del-1").unwrap().is_none());
    assert!(list_curated_memories(&conn, "del-1", 10000, 0)
        .unwrap()
        .0
        .is_empty());
}

#[test]
fn test_delete_agent_nonexistent() {
    let conn = setup();
    let deleted = delete_agent(&conn, "no-such-agent").unwrap();
    assert!(!deleted);
}

// ── find_agents ──

#[test]
fn test_find_agents_by_id_prefix() {
    let conn = setup();
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: "abc-12345".into(),
            name: "Alice".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "a".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: "xyz-99999".into(),
            name: "Bob".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "b".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();

    // Search by ID prefix
    let results = find_agents(&conn, "abc").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, "Alice");

    // Search by name
    let results = find_agents(&conn, "bob").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, "Bob");

    // No match
    let results = find_agents(&conn, "zzz").unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_find_agents_partial_name() {
    let conn = setup();
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: "agent-find-1".into(),
            name: "Creative Researcher".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "cr".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();

    let results = find_agents(&conn, "creative").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, "Creative Researcher");

    let results = find_agents(&conn, "researcher").unwrap();
    assert_eq!(results.len(), 1);
}

// ── Agent CRUD full cycle ──

#[test]
fn test_agent_crud_full_cycle() {
    let conn = setup();

    let agent_id = "crud-agent-1";
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: agent_id.into(),
            name: "TestAgent".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "Original Persona".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();

    let row = get_agent(&conn, agent_id).unwrap().unwrap();
    assert_eq!(row.name, "TestAgent");
    assert_eq!(row.persona_name, "Original Persona");

    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: agent_id.into(),
            name: "UpdatedAgent".into(),
            job_title: Some("Lead".into()),
            organization: None,
            image_url: None,
            persona_name: "Updated Persona".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();

    let row = get_agent(&conn, agent_id).unwrap().unwrap();
    assert_eq!(row.name, "UpdatedAgent");
    assert_eq!(row.job_title, Some("Lead".to_string()));
    assert_eq!(row.persona_name, "Updated Persona");

    // Find
    let results = find_agents(&conn, "Updated").unwrap();
    assert_eq!(results.len(), 1);

    // Delete
    let deleted = delete_agent(&conn, agent_id).unwrap();
    assert!(deleted);
    assert!(get_agent(&conn, agent_id).unwrap().is_none());

    // Find after delete
    let results = find_agents(&conn, "Updated").unwrap();
    assert!(results.is_empty());
}

// ── Discord Channel Config ──

#[test]
fn test_channel_config_upsert_and_get() {
    let conn = setup();

    let cfg = ChannelConfigRow {
        channel_id: "123456".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "general".to_string(),
        readable: true,
        writable: false,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };

    upsert_channel_config(&conn, &cfg).unwrap();

    let fetched = get_channel_config(&conn, "123456").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.channel_id, "123456");
    assert_eq!(fetched.guild_id, "guild-1");
    assert_eq!(fetched.channel_name, "general");
    assert!(fetched.readable);
    assert!(!fetched.writable);
}

#[test]
fn test_channel_config_upsert_update() {
    let conn = setup();

    let cfg = ChannelConfigRow {
        channel_id: "123456".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "general".to_string(),
        readable: true,
        writable: true,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };
    upsert_channel_config(&conn, &cfg).unwrap();

    // Update writable to false
    let cfg2 = ChannelConfigRow {
        writable: false,
        ..cfg
    };
    upsert_channel_config(&conn, &cfg2).unwrap();

    let fetched = get_channel_config(&conn, "123456").unwrap().unwrap();
    assert!(fetched.readable);
    assert!(!fetched.writable);
}

#[test]
fn test_channel_config_list_by_guild() {
    let conn = setup();

    let cfg1 = ChannelConfigRow {
        channel_id: "ch-1".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "general".to_string(),
        readable: true,
        writable: true,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };
    let cfg2 = ChannelConfigRow {
        channel_id: "ch-2".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "random".to_string(),
        readable: false,
        writable: true,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };
    let cfg3 = ChannelConfigRow {
        channel_id: "ch-3".to_string(),
        agent_id: String::new(),
        guild_id: "guild-2".to_string(),
        channel_name: "other".to_string(),
        readable: true,
        writable: true,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };

    upsert_channel_config(&conn, &cfg1).unwrap();
    upsert_channel_config(&conn, &cfg2).unwrap();
    upsert_channel_config(&conn, &cfg3).unwrap();

    let results = list_channel_configs_by_guild(&conn, "guild-1").unwrap();
    assert_eq!(results.len(), 2);

    let results2 = list_channel_configs_by_guild(&conn, "guild-2").unwrap();
    assert_eq!(results2.len(), 1);
}

#[test]
fn test_is_channel_readable_writable_defaults() {
    let conn = setup();

    // No config → defaults to true
    assert!(is_channel_readable(&conn, "unknown-ch"));
    assert!(is_channel_writable(&conn, "unknown-ch"));

    // Set readable=false
    let cfg = ChannelConfigRow {
        channel_id: "ch-blocked".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "blocked".to_string(),
        readable: false,
        writable: false,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };
    upsert_channel_config(&conn, &cfg).unwrap();

    assert!(!is_channel_readable(&conn, "ch-blocked"));
    assert!(!is_channel_writable(&conn, "ch-blocked"));
}

// ── Heartbeat Instructions ──

fn hb_agent(id: &str, heartbeat: &str) -> AgentRow {
    AgentRow {
        agent_id: id.to_string(),
        name: "N".to_string(),
        job_title: None,
        organization: None,
        image_url: None,
        persona_name: "P".to_string(),
        personality: None,
        instructions: String::new(),
        heartbeat_instructions: heartbeat.to_string(),
        model: None,
        reasoning_effort: None,
        web_search: None,
        metadata_json: None,
    }
}

fn hb_channel(channel_id: &str, agent_id: &str, heartbeat: &str) -> ChannelConfigRow {
    ChannelConfigRow {
        channel_id: channel_id.to_string(),
        agent_id: agent_id.to_string(),
        guild_id: "g1".to_string(),
        channel_name: String::new(),
        readable: true,
        writable: true,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: heartbeat.to_string(),
    }
}

/// T-1.1 / T-1.2: agents.heartbeat_instructions round-trips and patches independently.
#[test]
fn test_agent_heartbeat_instructions_roundtrip_and_patch() {
    let conn = setup();
    upsert_agent(&conn, &hb_agent("a1", "話題があるときだけ話す")).unwrap();
    let got = get_agent(&conn, "a1").unwrap().unwrap();
    assert_eq!(got.heartbeat_instructions, "話題があるときだけ話す");
    assert_eq!(got.instructions, "");

    // patch only heartbeat_instructions; other fields stay.
    let patch = AgentPatch {
        heartbeat_instructions: Some("業務連絡のみ".to_string()),
        ..Default::default()
    };
    assert!(apply_agent_patch(&conn, "a1", &patch).unwrap());
    let got = get_agent(&conn, "a1").unwrap().unwrap();
    assert_eq!(got.heartbeat_instructions, "業務連絡のみ");
    assert_eq!(got.name, "N");
    assert_eq!(got.persona_name, "P");
}

/// T-1.3: channel override round-trips.
#[test]
fn test_channel_heartbeat_instructions_roundtrip() {
    let conn = setup();
    upsert_channel_config(&conn, &hb_channel("ch1", "a1", "雑談禁止")).unwrap();
    let got = get_channel_config_for_agent(&conn, "ch1", "a1")
        .unwrap()
        .unwrap();
    assert_eq!(got.heartbeat_instructions, "雑談禁止");
}

/// T-2.1: priority channel(agent) > channel(global) > agent global.
#[test]
fn test_resolve_priority() {
    let conn = setup();
    upsert_agent(&conn, &hb_agent("a1", "AGENT")).unwrap();
    upsert_channel_config(&conn, &hb_channel("ch1", "", "GLOBAL_CH")).unwrap();
    upsert_channel_config(&conn, &hb_channel("ch1", "a1", "AGENT_CH")).unwrap();

    // channel(agent) wins and is concatenated after agent global.
    let r = resolve_heartbeat_instructions(&conn, "a1", "ch1");
    assert_eq!(r.source, "agent+channel");
    assert_eq!(r.text, "AGENT\n\nAGENT_CH");

    // remove channel(agent) override → falls back to channel(global).
    delete_channel_config_for_agent(&conn, "ch1", "a1").unwrap();
    let r = resolve_heartbeat_instructions(&conn, "a1", "ch1");
    assert_eq!(r.text, "AGENT\n\nGLOBAL_CH");

    // remove channel(global) → agent global only.
    delete_channel_config_for_agent(&conn, "ch1", "").unwrap();
    let r = resolve_heartbeat_instructions(&conn, "a1", "ch1");
    assert_eq!(r.source, "agent");
    assert_eq!(r.text, "AGENT");
}

/// T-2.2: all empty → default fallback.
#[test]
fn test_resolve_default_fallback() {
    let conn = setup();
    upsert_agent(&conn, &hb_agent("a1", "")).unwrap();
    let r = resolve_heartbeat_instructions(&conn, "a1", "ch-none");
    assert_eq!(r.source, "default");
    assert_eq!(r.text, DEFAULT_HEARTBEAT_INSTRUCTIONS);
}

/// T-2.4: clamp to max length and strip control characters.
#[test]
fn test_sanitize_clamp_and_control_chars() {
    let long = "あ".repeat(MAX_HEARTBEAT_INSTRUCTIONS_LEN + 100);
    let out = sanitize_heartbeat_instructions(&long);
    assert_eq!(out.chars().count(), MAX_HEARTBEAT_INSTRUCTIONS_LEN);

    let dirty = "ok\u{0007}line\nnext\ttab";
    let cleaned = sanitize_heartbeat_instructions(dirty);
    assert!(!cleaned.contains('\u{0007}'));
    assert!(cleaned.contains('\n'));
    assert!(cleaned.contains('\t'));
    assert_eq!(cleaned, "okline\nnext\ttab");
}

/// T-3.2: audit row records old/new/reason and is retrievable.
#[test]
fn test_heartbeat_instructions_audit_roundtrip() {
    let conn = setup();
    let audit = HeartbeatInstructionsAuditRow {
        agent_id: "a1".to_string(),
        scope: "agent".to_string(),
        channel_id: None,
        caller_identity: "owner".to_string(),
        caller_discord_id: Some("123".to_string()),
        old_value: Some("old".to_string()),
        new_value: Some("new".to_string()),
        reason: Some("オーナー依頼".to_string()),
    };
    insert_heartbeat_instructions_audit(&conn, &audit).unwrap();
    let rows = list_heartbeat_instructions_audit(&conn, "a1", 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].old_value.as_deref(), Some("old"));
    assert_eq!(rows[0].new_value.as_deref(), Some("new"));
    assert_eq!(rows[0].reason.as_deref(), Some("オーナー依頼"));
}

// ── Agent Discord Config ──

#[test]
fn test_agent_discord_config_upsert_and_get() {
    let conn = setup();

    let cfg = AgentDiscordConfigRow {
        agent_id: "agent-1".to_string(),
        bot_token: "TOKEN_ABC_12345".to_string(),
        owner_discord_id: "390123456789".to_string(),
        enabled: true,
    };

    upsert_agent_discord_config(&conn, &cfg).unwrap();

    let fetched = get_agent_discord_config(&conn, "agent-1").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.agent_id, "agent-1");
    assert_eq!(fetched.bot_token, "TOKEN_ABC_12345");
    assert_eq!(fetched.owner_discord_id, "390123456789");
    assert!(fetched.enabled);
}

#[test]
fn test_agent_discord_config_get_nonexistent() {
    let conn = setup();
    let result = get_agent_discord_config(&conn, "no-such-agent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_agent_discord_config_upsert_update() {
    let conn = setup();

    let cfg = AgentDiscordConfigRow {
        agent_id: "agent-1".to_string(),
        bot_token: "OLD_TOKEN".to_string(),
        owner_discord_id: "".to_string(),
        enabled: true,
    };
    upsert_agent_discord_config(&conn, &cfg).unwrap();

    // Update token and owner
    let cfg2 = AgentDiscordConfigRow {
        agent_id: "agent-1".to_string(),
        bot_token: "NEW_TOKEN".to_string(),
        owner_discord_id: "999888777".to_string(),
        enabled: false,
    };
    upsert_agent_discord_config(&conn, &cfg2).unwrap();

    let fetched = get_agent_discord_config(&conn, "agent-1").unwrap().unwrap();
    assert_eq!(fetched.bot_token, "NEW_TOKEN");
    assert_eq!(fetched.owner_discord_id, "999888777");
    assert!(!fetched.enabled);
}

#[test]
fn test_agent_discord_config_delete() {
    let conn = setup();

    let cfg = AgentDiscordConfigRow {
        agent_id: "agent-del".to_string(),
        bot_token: "TOKEN".to_string(),
        owner_discord_id: "".to_string(),
        enabled: true,
    };
    upsert_agent_discord_config(&conn, &cfg).unwrap();
    assert!(get_agent_discord_config(&conn, "agent-del")
        .unwrap()
        .is_some());

    let deleted = delete_agent_discord_config(&conn, "agent-del").unwrap();
    assert!(deleted);
    assert!(get_agent_discord_config(&conn, "agent-del")
        .unwrap()
        .is_none());

    // Delete nonexistent → false
    let deleted2 = delete_agent_discord_config(&conn, "agent-del").unwrap();
    assert!(!deleted2);
}

#[test]
fn test_list_enabled_agent_discord_configs() {
    let conn = setup();

    let cfg1 = AgentDiscordConfigRow {
        agent_id: "a1".to_string(),
        bot_token: "T1".to_string(),
        owner_discord_id: "".to_string(),
        enabled: true,
    };
    let cfg2 = AgentDiscordConfigRow {
        agent_id: "a2".to_string(),
        bot_token: "T2".to_string(),
        owner_discord_id: "".to_string(),
        enabled: false, // disabled
    };
    let cfg3 = AgentDiscordConfigRow {
        agent_id: "a3".to_string(),
        bot_token: "T3".to_string(),
        owner_discord_id: "owner".to_string(),
        enabled: true,
    };

    upsert_agent_discord_config(&conn, &cfg1).unwrap();
    upsert_agent_discord_config(&conn, &cfg2).unwrap();
    upsert_agent_discord_config(&conn, &cfg3).unwrap();

    let enabled = list_enabled_agent_discord_configs(&conn).unwrap();
    assert_eq!(enabled.len(), 2);

    let ids: Vec<&str> = enabled.iter().map(|c| c.agent_id.as_str()).collect();
    assert!(ids.contains(&"a1"));
    assert!(ids.contains(&"a3"));
    assert!(!ids.contains(&"a2"));
}

#[test]
fn test_set_agent_discord_config_enabled() {
    let conn = setup();

    let cfg = AgentDiscordConfigRow {
        agent_id: "agent-toggle".to_string(),
        bot_token: "TOKEN".to_string(),
        owner_discord_id: "".to_string(),
        enabled: true,
    };
    upsert_agent_discord_config(&conn, &cfg).unwrap();

    // Initially enabled
    let fetched = get_agent_discord_config(&conn, "agent-toggle")
        .unwrap()
        .unwrap();
    assert!(fetched.enabled);

    // Disable
    let updated = set_agent_discord_config_enabled(&conn, "agent-toggle", false).unwrap();
    assert!(updated);
    let fetched = get_agent_discord_config(&conn, "agent-toggle")
        .unwrap()
        .unwrap();
    assert!(!fetched.enabled);

    // Re-enable
    let updated = set_agent_discord_config_enabled(&conn, "agent-toggle", true).unwrap();
    assert!(updated);
    let fetched = get_agent_discord_config(&conn, "agent-toggle")
        .unwrap()
        .unwrap();
    assert!(fetched.enabled);

    // Nonexistent agent → false
    let updated = set_agent_discord_config_enabled(&conn, "no-such", false).unwrap();
    assert!(!updated);
}

#[test]
fn test_delete_agent_also_removes_discord_config() {
    let conn = setup();

    let agent_id = "agent-discord-del";
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: agent_id.into(),
            name: "DiscordAgent".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "d".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    upsert_agent_discord_config(
        &conn,
        &AgentDiscordConfigRow {
            agent_id: agent_id.into(),
            bot_token: "BOT_TOKEN_123".into(),
            owner_discord_id: "owner-1".into(),
            enabled: true,
        },
    )
    .unwrap();

    // Verify exists
    assert!(get_agent_discord_config(&conn, agent_id).unwrap().is_some());

    // Delete agent
    let deleted = delete_agent(&conn, agent_id).unwrap();
    assert!(deleted);

    // Discord config should also be gone
    assert!(get_agent_discord_config(&conn, agent_id).unwrap().is_none());
}

// ============================================
// Agent Webhook Config tests
// ============================================

fn sample_webhook_row(agent_id: &str) -> AgentWebhookConfigRow {
    AgentWebhookConfigRow {
        scope: "agent".into(),
        agent_id: agent_id.into(),
        tool_name: "".into(),
        kind: "subtask".into(),
        url: "https://example.com/hook".into(),
        events_json: Some(r#"["start","done"]"#.into()),
        enabled: true,
        name: Some("default hook".into()),
        created_by: Some("tester".into()),
        output_mode: "full".into(),
        max_chars: 2000,
        updated_at: String::new(),
    }
}

#[test]
fn test_agent_webhook_upsert_and_get_roundtrip() {
    let conn = setup();
    let row = sample_webhook_row("agent-1");
    upsert_agent_webhook_config(&conn, &row).unwrap();

    let fetched = get_agent_webhook_config(&conn, "agent", "agent-1", "", "subtask")
        .unwrap()
        .unwrap();
    assert_eq!(fetched.scope, "agent");
    assert_eq!(fetched.agent_id, "agent-1");
    assert_eq!(fetched.tool_name, "");
    assert_eq!(fetched.kind, "subtask");
    assert_eq!(fetched.url, "https://example.com/hook");
    assert_eq!(fetched.events_json, Some(r#"["start","done"]"#.to_string()));
    assert!(fetched.enabled);
    assert_eq!(fetched.name, Some("default hook".to_string()));
    assert_eq!(fetched.created_by, Some("tester".to_string()));
    assert_eq!(fetched.output_mode, "full");
    assert_eq!(fetched.max_chars, 2000);
    assert!(!fetched.updated_at.is_empty());
}

#[test]
fn test_agent_webhook_get_missing_returns_none() {
    let conn = setup();
    let result = get_agent_webhook_config(&conn, "agent", "nope", "", "subtask").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_agent_webhook_upsert_updates_not_duplicates() {
    let conn = setup();
    let mut row = sample_webhook_row("agent-1");
    upsert_agent_webhook_config(&conn, &row).unwrap();

    row.url = "https://example.com/updated".into();
    upsert_agent_webhook_config(&conn, &row).unwrap();

    let fetched = get_agent_webhook_config(&conn, "agent", "agent-1", "", "subtask")
        .unwrap()
        .unwrap();
    assert_eq!(fetched.url, "https://example.com/updated");

    // Only one row for this PK
    let all = list_agent_webhook_config(&conn, Some("agent-1"), true).unwrap();
    let count = all
        .iter()
        .filter(|r| {
            r.scope == "agent"
                && r.agent_id == "agent-1"
                && r.tool_name.is_empty()
                && r.kind == "subtask"
        })
        .count();
    assert_eq!(count, 1);
}

#[test]
fn test_agent_webhook_list_include_disabled_filter() {
    let conn = setup();
    let mut enabled_row = sample_webhook_row("agent-1");
    enabled_row.kind = "subtask".into();
    upsert_agent_webhook_config(&conn, &enabled_row).unwrap();

    let mut disabled_row = sample_webhook_row("agent-1");
    disabled_row.kind = "tool".into();
    disabled_row.enabled = false;
    upsert_agent_webhook_config(&conn, &disabled_row).unwrap();

    let only_enabled = list_agent_webhook_config(&conn, Some("agent-1"), false).unwrap();
    assert_eq!(only_enabled.len(), 1);
    assert_eq!(only_enabled[0].kind, "subtask");

    let with_disabled = list_agent_webhook_config(&conn, Some("agent-1"), true).unwrap();
    assert_eq!(with_disabled.len(), 2);
}

#[test]
fn test_agent_webhook_list_agent_includes_global() {
    let conn = setup();
    upsert_agent_webhook_config(&conn, &sample_webhook_row("agent-1")).unwrap();

    let mut global = sample_webhook_row("*");
    global.scope = "global".into();
    upsert_agent_webhook_config(&conn, &global).unwrap();

    upsert_agent_webhook_config(&conn, &sample_webhook_row("agent-2")).unwrap();

    let rows = list_agent_webhook_config(&conn, Some("agent-1"), true).unwrap();
    let agent_ids: Vec<&str> = rows.iter().map(|r| r.agent_id.as_str()).collect();
    assert!(agent_ids.contains(&"agent-1"));
    assert!(agent_ids.contains(&"*"));
    assert!(!agent_ids.contains(&"agent-2"));
    assert_eq!(rows.len(), 2);

    // None -> all rows
    let all = list_agent_webhook_config(&conn, None, true).unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn test_agent_webhook_distinct_pk_combos_coexist() {
    let conn = setup();

    let mut r1 = sample_webhook_row("agent-1");
    r1.kind = "subtask".into();
    let mut r2 = sample_webhook_row("agent-1");
    r2.kind = "tool".into();
    r2.tool_name = "my_tool".into();
    let mut r3 = sample_webhook_row("agent-1");
    r3.scope = "tool".into();
    r3.kind = "lifecycle".into();

    upsert_agent_webhook_config(&conn, &r1).unwrap();
    upsert_agent_webhook_config(&conn, &r2).unwrap();
    upsert_agent_webhook_config(&conn, &r3).unwrap();

    let rows = list_agent_webhook_config(&conn, Some("agent-1"), true).unwrap();
    assert_eq!(rows.len(), 3);

    assert!(
        get_agent_webhook_config(&conn, "agent", "agent-1", "", "subtask")
            .unwrap()
            .is_some()
    );
    assert!(
        get_agent_webhook_config(&conn, "agent", "agent-1", "my_tool", "tool")
            .unwrap()
            .is_some()
    );
    assert!(
        get_agent_webhook_config(&conn, "tool", "agent-1", "", "lifecycle")
            .unwrap()
            .is_some()
    );
}

// ============================================
// short_id tests (T-1.1 ~ T-1.6)
// ============================================

#[test]
fn test_next_short_id_empty_table() {
    // T-1.1: Empty table should return "t1"
    let conn = setup();
    let result = next_short_id(&conn, "a1", "t").unwrap();
    assert_eq!(result, "t1");
}

#[test]
fn test_next_short_id_sequential() {
    // T-1.2: With t1,t2,t3 existing, should return "t4"
    let conn = setup();
    for i in 1..=3 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("node-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: format!("Topic {i}"),
                summary: "test".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("t{i}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let result = next_short_id(&conn, "a1", "t").unwrap();
    assert_eq!(result, "t4");
}

#[test]
fn test_next_short_id_independent_prefix() {
    // T-1.3: t1, t2, h1 exist -> prefix="h" returns "h2"
    let conn = setup();
    for (id, prefix, num) in &[("n1", "t", 1), ("n2", "t", 2), ("n3", "h", 1)] {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("{prefix}{num}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let result = next_short_id(&conn, "a1", "h").unwrap();
    assert_eq!(result, "h2");
}

#[test]
fn test_next_short_id_independent_agent() {
    // T-1.4: agent a1 has t1-t10, agent a2 has t1 -> a2 prefix="t" returns "t2"
    let conn = setup();
    for i in 1..=10 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("a1-node-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("t{i}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    insert_index_node(
        &conn,
        &IndexNodeRow {
            id: "a2-node-1".to_string(),
            agent_id: "a2".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: String::new(),
            title: "T".to_string(),
            summary: "s".to_string(),
            start_log_id: None,
            end_log_id: None,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: Some("t1".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
    let result = next_short_id(&conn, "a2", "t").unwrap();
    assert_eq!(result, "t2");
}

#[test]
fn test_next_short_id_with_gaps() {
    // T-1.5: t1, t3, t5 exist (gaps) -> returns "t6" (MAX+1)
    let conn = setup();
    for (id, num) in &[("n1", 1), ("n2", 3), ("n3", 5)] {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("t{num}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let result = next_short_id(&conn, "a1", "t").unwrap();
    assert_eq!(result, "t6");
}

#[test]
fn test_next_short_id_all_prefixes() {
    // T-1.6: All prefix patterns return "{prefix}1" on empty table
    let conn = setup();
    for prefix in &["t", "h", "d", "w", "m", "y", "p", "r", "s"] {
        let result = next_short_id(&conn, "a1", prefix).unwrap();
        assert_eq!(result, format!("{prefix}1"), "Failed for prefix {prefix}");
    }
}

// ============================================
// backfill_short_ids tests (T-1.7 ~ T-1.9)
// ============================================

#[test]
fn test_backfill_short_ids_basic() {
    // T-1.7: 5 topics + 3 dailies with NULL short_id -> get assigned
    let conn = setup();
    for i in 1..=5 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("topic-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: format!("Topic {i}"),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: format!("2026-01-01T00:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: None,
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    for i in 1..=3 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("daily-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "daily".to_string(),
                source_type: String::new(),
                title: format!("Daily {i}"),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: format!("2026-01-01T01:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: None,
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let count = backfill_short_ids(&conn).unwrap();
    assert_eq!(count, 8);
    // Verify topics got t1-t5, dailies got d1-d3
    let node = get_index_node(&conn, "topic-1").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t1".to_string()));
    let node = get_index_node(&conn, "topic-5").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t5".to_string()));
    let node = get_index_node(&conn, "daily-1").unwrap().unwrap();
    assert_eq!(node.short_id, Some("d1".to_string()));
    let node = get_index_node(&conn, "daily-3").unwrap().unwrap();
    assert_eq!(node.short_id, Some("d3".to_string()));
}

#[test]
fn test_backfill_short_ids_skip_existing() {
    // T-1.8: t1, t2 already set, 3 NULL -> only NULL ones get t3, t4, t5
    let conn = setup();
    for i in 1..=2 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("topic-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: format!("2026-01-01T00:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("t{i}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    for i in 3..=5 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("topic-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: format!("2026-01-01T00:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: None,
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let count = backfill_short_ids(&conn).unwrap();
    assert_eq!(count, 3);
    // t1, t2 unchanged
    let node = get_index_node(&conn, "topic-1").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t1".to_string()));
    // New ones got t3, t4, t5
    let node = get_index_node(&conn, "topic-3").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t3".to_string()));
    let node = get_index_node(&conn, "topic-5").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t5".to_string()));
}

#[test]
fn test_backfill_short_ids_empty_table() {
    // T-1.9: No nodes -> 0 changes, no error
    let conn = setup();
    let count = backfill_short_ids(&conn).unwrap();
    assert_eq!(count, 0);
}

// ============================================
// T-1.10 ~ T-1.12: date_from/date_to backfill tests
// TODO: These tests require session_log data infrastructure setup.
//       Implement when session_log-based date inference is added.
// ============================================

// ============================================
// get_index_node_by_short_or_id tests (T-1.13 ~ T-1.15)
// ============================================

#[test]
fn test_get_index_node_by_short_id() {
    // T-1.13: Search by short_id "t42"
    let conn = setup();
    insert_index_node(
        &conn,
        &IndexNodeRow {
            id: "topic-agent:nostarou:main-sess_abc-1-20".to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: String::new(),
            title: "Test Topic".to_string(),
            summary: "test summary".to_string(),
            start_log_id: None,
            end_log_id: None,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: Some("t42".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
    let result = get_index_node_by_short_or_id(&conn, "a1", "t42").unwrap();
    assert!(result.is_some());
    assert_eq!(
        result.unwrap().id,
        "topic-agent:nostarou:main-sess_abc-1-20"
    );
}

#[test]
fn test_get_index_node_by_full_id() {
    // T-1.14: Search by full id
    let conn = setup();
    insert_index_node(
        &conn,
        &IndexNodeRow {
            id: "topic-agent:nostarou:main-sess_abc-1-20".to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: String::new(),
            title: "Test Topic".to_string(),
            summary: "test summary".to_string(),
            start_log_id: None,
            end_log_id: None,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: Some("t42".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
    let result =
        get_index_node_by_short_or_id(&conn, "a1", "topic-agent:nostarou:main-sess_abc-1-20")
            .unwrap();
    assert!(result.is_some());
    assert_eq!(
        result.unwrap().id,
        "topic-agent:nostarou:main-sess_abc-1-20"
    );
}

#[test]
fn test_get_index_node_by_short_id_not_found() {
    // T-1.15: Non-existent short_id returns None
    let conn = setup();
    let result = get_index_node_by_short_or_id(&conn, "a1", "t99999").unwrap();
    assert!(result.is_none());
}

/// **フル ID のフォールバック検索も agent_id でスコープされる**（#203 の一括点検）。
///
/// short_id での引きは SQL に `agent_id = ?1` があるので構造的に守られているが、
/// 見つからなかったときのフォールバック（`get_index_node`）は **agent_id を条件に
/// 持たない**ので、取得後の `node.agent_id == agent_id` 再チェックだけが境界になる。
/// ノード ID は `topic-agent:{name}:{session}-...` という予測可能な形なので、この
/// 再チェックが外れると他エージェントの非公開会話のタイトル/サマリが ID の推測だけで
/// 読める。再チェックを削っても落ちるテストが 1 件も無かったため追加する。
#[test]
fn test_get_index_node_by_full_id_is_scoped_to_agent() {
    let conn = setup();
    let node_id = "topic-agent:nostarou:secret-sess_abc-1-20";
    insert_index_node(
        &conn,
        &IndexNodeRow {
            id: node_id.to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: String::new(),
            title: "a1 の非公開トピック".to_string(),
            summary: "他エージェントに見えてはならない要約".to_string(),
            start_log_id: None,
            end_log_id: None,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: Some("t42".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();

    // 持ち主は引ける（フォールバック経路が生きていることの対照）。
    assert!(
        get_index_node_by_short_or_id(&conn, "a1", node_id)
            .unwrap()
            .is_some(),
        "持ち主はフル ID で引ける"
    );

    // 別エージェントはフル ID を知っていても引けない。
    assert!(
        get_index_node_by_short_or_id(&conn, "a2", node_id)
            .unwrap()
            .is_none(),
        "別エージェントのノードがフル ID 経由で漏れている"
    );

    // short_id も同様（こちらは SQL 側で守られている）。
    assert!(
        get_index_node_by_short_or_id(&conn, "a2", "t42")
            .unwrap()
            .is_none(),
        "別エージェントのノードが short_id 経由で漏れている"
    );
}

// ============================================
// memory_index_fts / キーワード逆引きテスト
// ============================================

fn mk_topic_node(
    id: &str,
    agent_id: &str,
    title: &str,
    summary: &str,
    keywords: &[&str],
) -> IndexNodeRow {
    IndexNodeRow {
        id: id.to_string(),
        agent_id: agent_id.to_string(),
        parent_id: None,
        node_type: "topic".to_string(),
        source_type: "session_log".to_string(),
        title: title.to_string(),
        summary: summary.to_string(),
        start_log_id: None,
        end_log_id: None,
        source_session_id: None,
        date_from: Some("2026-06-01".to_string()),
        date_to: Some("2026-06-02".to_string()),
        depth: 3,
        child_count: 0,
        token_count: 0,
        created_at: "2026-06-01T00:00:00Z".to_string(),
        updated_at: "2026-06-01T00:00:00Z".to_string(),
        short_id: Some(id.to_string()),
        keywords_json: serde_json::to_string(keywords).unwrap(),
        summary_refreshed_at: None,
    }
}

fn fts_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM memory_index_fts", [], |r| r.get(0))
        .unwrap()
}

fn nodes_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM memory_index_nodes", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn test_index_fts_consistency_through_write_paths() {
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node(
            "t1",
            "a1",
            "Discord連携",
            "botの実装",
            &["Discord", "serenity"],
        ),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &mk_topic_node("t2", "a1", "料理の話", "カレーを作った", &["カレー"]),
    )
    .unwrap();
    assert_eq!(fts_count(&conn), nodes_count(&conn));

    // キーワードでヒット
    let hits = search_index_nodes(&conn, "a1", "serenity", 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "t1");

    // summary 更新が検索に反映される
    update_index_node_summary(&conn, "t2", "料理の話", "肉じゃがを作った").unwrap();
    assert!(
        search_index_nodes(&conn, "a1", "肉じゃが", 10, None)
            .unwrap()
            .len()
            == 1
    );
    assert!(search_index_nodes(&conn, "a1", "カレーを作った", 10, None)
        .unwrap()
        .is_empty());
    assert_eq!(fts_count(&conn), nodes_count(&conn));

    // keywords 更新が検索に反映される
    update_index_node_keywords(&conn, "t2", "[\"肉じゃが\",\"じゃがいも\"]").unwrap();
    let hits = search_index_nodes(&conn, "a1", "じゃがいも", 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "t2");

    // 単体削除で FTS も消える
    delete_index_node(&conn, "t1").unwrap();
    assert!(search_index_nodes(&conn, "a1", "serenity", 10, None)
        .unwrap()
        .is_empty());
    assert_eq!(fts_count(&conn), nodes_count(&conn));

    // agent 単位 purge で FTS も消える
    delete_index_nodes_for_agent(&conn, "a1").unwrap();
    assert_eq!(fts_count(&conn), 0);
    assert_eq!(nodes_count(&conn), 0);
}

#[test]
fn test_index_fts_insert_or_ignore_keeps_existing() {
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "元タイトル", "元要約", &["元"]),
    )
    .unwrap();
    // 同一 id の再 insert は OR IGNORE で無視され、FTS も元のまま
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "新タイトル", "新要約", &["新"]),
    )
    .unwrap();
    assert_eq!(fts_count(&conn), 1);
    assert_eq!(
        search_index_nodes(&conn, "a1", "元要約", 10, None)
            .unwrap()
            .len(),
        1
    );
    assert!(search_index_nodes(&conn, "a1", "新要約", 10, None)
        .unwrap()
        .is_empty());
}

#[test]
fn test_search_index_nodes_scoping_and_filters() {
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "Rust勉強会", "所有権の話", &["Rust"]),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &mk_topic_node("t2", "a2", "Rust輪読", "他人の記憶", &["Rust"]),
    )
    .unwrap();
    let mut period = mk_topic_node("p1", "a1", "2026-05", "5月のRustまとめ", &[]);
    period.node_type = "period".to_string();
    insert_index_node(&conn, &period).unwrap();

    // agent 分離: a1 からは a2 のノードが見えない
    let hits = search_index_nodes(&conn, "a1", "Rust", 10, None).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.node_id != "t2"));

    // node_type フィルタ
    let hits = search_index_nodes(&conn, "a1", "Rust", 10, Some("period")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "p1");

    // AND で 0 件 → OR フォールバックで拾う
    let hits = search_index_nodes(&conn, "a1", "所有権 存在しない語", 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "t1");

    // 空クエリは空結果
    assert!(search_index_nodes(&conn, "a1", "   ", 10, None)
        .unwrap()
        .is_empty());
}

#[test]
fn test_list_topics_missing_keywords() {
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "キーワードなし", "s", &[]),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &mk_topic_node("t2", "a1", "キーワードあり", "s", &["kw"]),
    )
    .unwrap();
    let mut daily = mk_topic_node("d1", "a1", "daily由来", "s", &[]);
    daily.source_type = "daily_log".to_string();
    insert_index_node(&conn, &daily).unwrap();

    let missing = list_topics_missing_keywords(&conn, "a1", 10).unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].id, "t1");
}

#[test]
fn test_search_index_nodes_short_query_like_fallback() {
    // trigram は 3 文字未満の語に当たらない → LIKE フォールバックで拾う
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "AI導入の相談", "LLMの選定", &["AI"]),
    )
    .unwrap();
    let hits = search_index_nodes(&conn, "a1", "AI", 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "t1");
    // LIKE フォールバックでも agent 分離は効く
    assert!(search_index_nodes(&conn, "a2", "AI", 10, None)
        .unwrap()
        .is_empty());
}

#[test]
fn test_delete_index_node_cascades_fts_for_subtree() {
    // parent_id の ON DELETE CASCADE で子孫ノードが消えるとき、FTS も部分木ごと消える
    let conn = setup();
    let mut parent = mk_topic_node("s1", "a1", "親セッション", "親要約", &[]);
    parent.node_type = "session".to_string();
    insert_index_node(&conn, &parent).unwrap();
    let mut child = mk_topic_node("t1", "a1", "子トピック", "子要約ユニーク", &["子kw"]);
    child.parent_id = Some("s1".to_string());
    insert_index_node(&conn, &child).unwrap();
    assert_eq!(nodes_count(&conn), 2);
    assert_eq!(fts_count(&conn), 2);

    delete_index_node(&conn, "s1").unwrap();
    // CASCADE で子も消え、FTS に孤児が残らない
    assert_eq!(nodes_count(&conn), 0);
    assert_eq!(fts_count(&conn), 0);
    assert!(search_index_nodes(&conn, "a1", "子要約ユニーク", 10, None)
        .unwrap()
        .is_empty());
}

#[test]
fn test_index_write_helpers_work_inside_outer_transaction() {
    // index_builder::delete_index はトランザクション内から delete_index_nodes_for_agent
    // を呼ぶ。SAVEPOINT 方式なので外側 tx があっても動くこと（BEGIN の入れ子は不可）。
    let conn = setup();
    insert_index_node(&conn, &mk_topic_node("t1", "a1", "T", "S", &["kw"])).unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    delete_index_nodes_for_agent(&tx, "a1").unwrap();
    insert_index_node(&tx, &mk_topic_node("t2", "a1", "T2", "S2", &["kw2"])).unwrap();
    tx.commit().unwrap();
    assert_eq!(nodes_count(&conn), 1);
    assert_eq!(fts_count(&conn), 1);
}

#[test]
fn test_skill_usage_log_and_last_consolidation() {
    let conn = setup();
    // スキル利用のセッション単位記録
    insert_skill_usage(&conn, "a1", "sk1", "sess-A").unwrap();
    insert_skill_usage(&conn, "a1", "sk1", "sess-B").unwrap();
    insert_skill_usage(&conn, "a1", "sk2", "sess-A").unwrap();
    let mut sk1 = list_skill_used_sessions(&conn, "sk1", None).unwrap();
    sk1.sort();
    assert_eq!(sk1, vec!["sess-A".to_string(), "sess-B".to_string()]);
    assert_eq!(
        list_skill_used_sessions(&conn, "sk2", None).unwrap().len(),
        1
    );
    // since フィルタ（未来時刻なら0件）
    let future = "2999-01-01T00:00:00+00:00";
    assert!(list_skill_used_sessions(&conn, "sk1", Some(future))
        .unwrap()
        .is_empty());

    // last_skill_consolidation_at: 行が無ければ None、UPSERT で行を作って永続化
    assert!(get_last_skill_consolidation_at(&conn, "a1")
        .unwrap()
        .is_none());
    set_last_skill_consolidation_at(&conn, "a1", "2026-07-01T00:00:00+00:00").unwrap();
    assert_eq!(
        get_last_skill_consolidation_at(&conn, "a1")
            .unwrap()
            .as_deref(),
        Some("2026-07-01T00:00:00+00:00")
    );
    // 2回目はフィールドのみ更新
    set_last_skill_consolidation_at(&conn, "a1", "2026-07-02T00:00:00+00:00").unwrap();
    assert_eq!(
        get_last_skill_consolidation_at(&conn, "a1")
            .unwrap()
            .as_deref(),
        Some("2026-07-02T00:00:00+00:00")
    );
}

// ---- 権限の表記（列挙型, #234） ----

/// 表記ゆれが型で起こりえないこと: DB に入る文字列は列挙型からしか作れず、
/// **全 variant がケバブケース**で、読み書きが往復する。
#[test]
fn permission_spelling_cannot_drift() {
    for p in TRUSTED_USER_PERMISSIONS {
        let s = p.as_db_str();
        // アンダースコア表記は存在しない（#234 の食い違いはこれで起きた）。
        assert!(!s.contains('_'), "{s} はケバブケースでない");
        // 書いた表記はそのまま読み戻せる。
        assert_eq!(TrustedUserPermission::parse(s), Some(p));
        assert_eq!(TrustedUserPermission::from_db_str(s), p);
        // serde 表現（API の応答 / 設定側の CommandPermission と同じ規約）も同じ文字列。
        assert_eq!(serde_json::to_string(&p).unwrap(), format!("\"{s}\""));
    }
    // 表記は 3 つで全部（増えたらここが落ちる）。
    assert_eq!(
        TRUSTED_USER_PERMISSIONS.map(|p| p.as_db_str()),
        ["owner", "user", "co-agent"]
    );
}

/// 未知の表記は**入口で通らない**。かつて寛容に受け入れていた綴りも通らない。
/// 読み出しは最小権限（`user`）へ倒れる（fail-closed、行の判定は従来と同じ）。
#[test]
fn unknown_permission_spellings_are_rejected_at_the_gate() {
    for bad in [
        "co_agent", "coagent", "CoAgent", "Owner", "trusted", "", " user",
    ] {
        assert_eq!(TrustedUserPermission::parse(bad), None, "{bad:?}");
        assert_eq!(
            TrustedUserPermission::from_db_str(bad),
            TrustedUserPermission::User,
            "{bad:?}"
        );
    }
}

/// 既定は `user`（登録 API の既定と揃っていること）。
#[test]
fn permission_defaults_to_user() {
    assert_eq!(
        TrustedUserPermission::default(),
        TrustedUserPermission::User
    );
}

/// 選択肢の定義が 2 箇所に分かれてドリフトしないこと（#234）。
///
/// ダッシュボードの `TRUSTED_USER_PERMISSIONS` はこの列挙型の写しでしかない。
/// 独立した文字列配列だった頃、UI は `co-agent`・判定は `co_agent` で、
/// **ダッシュボードからの登録が黙って無効**になっていた。片方だけ変えたらここが落ちる。
#[test]
fn dashboard_permission_options_match_the_enum() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../web/src/api/trusted_users.ts"
    );
    let src = std::fs::read_to_string(path).expect("ダッシュボードの API 定義を読めること");
    let (_, rest) = src
        .split_once("export const TRUSTED_USER_PERMISSIONS = [")
        .expect("TRUSTED_USER_PERMISSIONS の定義があること");
    let (list, _) = rest.split_once(']').expect("配列が閉じていること");
    let from_dashboard: Vec<String> = list
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let from_enum: Vec<String> = TRUSTED_USER_PERMISSIONS
        .iter()
        .map(|p| p.as_db_str().to_string())
        .collect();
    assert_eq!(from_dashboard, from_enum);
}

// ============================================
// Agent Heartbeat Config（#247）
// ============================================

/// 行が無いエージェントは**無効**（fail-closed）。既定間隔は返すが有効にはしない。
#[test]
fn agent_heartbeat_defaults_to_disabled_when_unset() {
    let conn = crate::init_memory().unwrap();
    assert_eq!(get_agent_heartbeat_config(&conn, "a1").unwrap(), None);

    let r = resolve_agent_heartbeat(&conn, "a1", 1800, 300);
    assert!(!r.enabled, "設定が無いときは無効");
    assert_eq!(r.interval_secs, 1800);
    assert_eq!(r.source, "unset");
}

/// upsert は作成も更新もする。間隔 None は「運用者既定に従う」。
#[test]
fn agent_heartbeat_upsert_creates_then_updates() {
    let conn = crate::init_memory().unwrap();
    upsert_agent_heartbeat_config(
        &conn,
        &AgentHeartbeatConfigRow {
            agent_id: "a1".to_string(),
            enabled: true,
            interval_secs: None,
        },
    )
    .unwrap();
    let r = resolve_agent_heartbeat(&conn, "a1", 1800, 300);
    assert!(r.enabled);
    assert_eq!((r.interval_secs, r.source), (1800, "default"));

    upsert_agent_heartbeat_config(
        &conn,
        &AgentHeartbeatConfigRow {
            agent_id: "a1".to_string(),
            enabled: true,
            interval_secs: Some(900),
        },
    )
    .unwrap();
    let r = resolve_agent_heartbeat(&conn, "a1", 1800, 300);
    assert_eq!((r.enabled, r.interval_secs, r.source), (true, 900, "agent"));

    // 行は 1 件のまま（PK 衝突で増えない）。
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_heartbeat_config", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 1);
}

/// 無効な行は「間隔がいくつでも無効」。他エージェントの設定は混ざらない。
#[test]
fn agent_heartbeat_disabled_row_stays_disabled_and_is_per_agent() {
    let conn = crate::init_memory().unwrap();
    upsert_agent_heartbeat_config(
        &conn,
        &AgentHeartbeatConfigRow {
            agent_id: "a1".to_string(),
            enabled: false,
            interval_secs: Some(600),
        },
    )
    .unwrap();
    let r = resolve_agent_heartbeat(&conn, "a1", 1800, 300);
    assert!(!r.enabled);
    assert_eq!(r.source, "disabled");
    // 別エージェントは影響を受けない。
    assert!(!resolve_agent_heartbeat(&conn, "a2", 1800, 300).enabled);
    assert_eq!(
        resolve_agent_heartbeat(&conn, "a2", 1800, 300).source,
        "unset"
    );
}

/// `list_agents_with_heartbeat_enabled` は enabled = 1 の行だけを集合として返す。
/// 無効行・未設定は含めない。壊れた interval でも enabled なら列挙する（発火可否は
/// resolve が握る二段構え）。
#[test]
fn list_agents_with_heartbeat_enabled_returns_only_enabled_rows() {
    let conn = crate::init_memory().unwrap();
    // 未設定なら空。
    assert!(list_agents_with_heartbeat_enabled(&conn)
        .unwrap()
        .is_empty());

    // enabled = true（間隔あり）。
    upsert_agent_heartbeat_config(
        &conn,
        &AgentHeartbeatConfigRow {
            agent_id: "on-with-interval".to_string(),
            enabled: true,
            interval_secs: Some(900),
        },
    )
    .unwrap();
    // enabled = true（間隔 None = 既定に従う）。
    upsert_agent_heartbeat_config(
        &conn,
        &AgentHeartbeatConfigRow {
            agent_id: "on-default-interval".to_string(),
            enabled: true,
            interval_secs: None,
        },
    )
    .unwrap();
    // enabled = false（除外される）。
    upsert_agent_heartbeat_config(
        &conn,
        &AgentHeartbeatConfigRow {
            agent_id: "off".to_string(),
            enabled: false,
            interval_secs: Some(600),
        },
    )
    .unwrap();
    // enabled = true だが壊れた interval（それでも列挙する。発火は resolve が止める）。
    conn.execute(
        "INSERT INTO agent_heartbeat_config (agent_id, enabled, interval_secs, updated_at)
         VALUES ('on-broken-interval', 1, 0, '2026-01-01')",
        [],
    )
    .unwrap();

    let mut got = list_agents_with_heartbeat_enabled(&conn).unwrap();
    got.sort();
    assert_eq!(
        got,
        vec![
            "on-broken-interval".to_string(),
            "on-default-interval".to_string(),
            "on-with-interval".to_string(),
        ],
        "enabled = 1 の行だけ（無効・未設定は除外、壊れた間隔でも enabled なら含む）"
    );
}

/// 壊れた値（0 / 負値）は**無効**として扱う。下限未満は下限へ引き上げる。
#[test]
fn agent_heartbeat_broken_interval_disables_and_below_floor_clamps_up() {
    let conn = crate::init_memory().unwrap();
    for broken in [0i64, -1] {
        conn.execute(
            "INSERT INTO agent_heartbeat_config (agent_id, enabled, interval_secs, updated_at)
             VALUES ('broken', 1, ?1, '2026-01-01')
             ON CONFLICT(agent_id) DO UPDATE SET interval_secs = excluded.interval_secs",
            params![broken],
        )
        .unwrap();
        let r = resolve_agent_heartbeat(&conn, "broken", 1800, 300);
        assert!(!r.enabled, "壊れた間隔 {broken} は無効として扱う");
        assert_eq!(r.source, "invalid");
    }

    // 下限を後から引き上げた運用者を模す: 停止させず下限へ引き上げる。
    upsert_agent_heartbeat_config(
        &conn,
        &AgentHeartbeatConfigRow {
            agent_id: "a1".to_string(),
            enabled: true,
            interval_secs: Some(60),
        },
    )
    .unwrap();
    let r = resolve_agent_heartbeat(&conn, "a1", 1800, 300);
    assert_eq!(
        (r.enabled, r.interval_secs, r.source),
        (true, 300, "clamped")
    );

    // 下限 0（運用者が下限を外した）でも 0 秒間隔にはしない。
    let r = resolve_agent_heartbeat(&conn, "a1", 1800, 0);
    assert_eq!((r.enabled, r.interval_secs, r.source), (true, 60, "agent"));
}

// ============================================
// カテゴリ層（issue #313）
// ============================================

/// **回帰ガード**: `insert_index_node` は `INSERT OR IGNORE` なので、node_type の CHECK に
/// `'category'`/`'meta'` が無いと**エラーにならず黙って落ちる**（作ったつもりで消える）。
/// この沈黙を固定する: 挿入後に実際に読み戻せることを assert する。CHECK を狭めて
/// 退行させたら、OR IGNORE で行が入らず get_index_node が None になりこのテストが落ちる。
#[test]
fn category_and_meta_nodes_persist_through_or_ignore_insert() {
    let conn = setup();
    for (id, ntype, sid) in [("cat1", "category", "c1"), ("meta1", "meta", "g1")] {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: ntype.to_string(),
                source_type: "category".to_string(),
                title: format!("title-{id}"),
                summary: String::new(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(sid.to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
        // 沈黙が起きていないこと（OR IGNORE で握り潰されず実際に保存されている）。
        assert!(
            get_index_node(&conn, id).unwrap().is_some(),
            "{ntype} ノードが INSERT OR IGNORE で黙って落ちた（CHECK に {ntype} が無い退行）"
        );
    }
}

#[test]
fn category_seed_and_assignment_queries_roundtrip() {
    let conn = setup();
    let now = "2026-06-01T00:00:00Z";

    // 種: curated long_term/<名前> を 2 件。
    for (i, name) in ["Rustの学び", "Discord運用"].iter().enumerate() {
        upsert_curated_memory(
            &conn,
            &CuratedMemoryRow {
                id: format!("cm{i}"),
                agent_id: "a1".to_string(),
                category: format!("long_term/{name}"),
                content: "…".to_string(),
                created_at: now.to_string(),
            },
        )
        .unwrap();
    }
    let seeds = list_long_term_category_seeds(&conn, "a1").unwrap();
    assert_eq!(
        seeds,
        vec!["Discord運用".to_string(), "Rustの学び".to_string()]
    );

    // カテゴリツリーの根を確保（冪等）。
    let root = ensure_category_root(&conn, "a1", now).unwrap();
    assert_eq!(ensure_category_root(&conn, "a1", now).unwrap(), root);

    // カテゴリノードを作り、トップレベルに現れる。
    let cat = insert_category_node(&conn, "a1", &root, "Rustの学び", "", now).unwrap();
    assert!(get_category_node_by_title(&conn, "a1", "Rustの学び")
        .unwrap()
        .is_some());
    let tops = list_top_level_categories(&conn, "a1").unwrap();
    assert_eq!(tops.len(), 1);

    // topic を積み、未割当 → 割当 → sticky。
    for (id, sid) in [("t1", "t1"), ("t2", "t2")] {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: format!("topic-{id}"),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: now.to_string(),
                updated_at: now.to_string(),
                short_id: Some(sid.to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    assert_eq!(list_unassigned_topics(&conn, "a1", 10).unwrap().len(), 2);
    assert!(assign_topic_to_category(&conn, "a1", "t1", &cat.id, now).unwrap());
    // 二重割当は sticky で false（同じ入力で結果が変わらない）。
    assert!(!assign_topic_to_category(&conn, "a1", "t1", &cat.id, now).unwrap());
    assert_eq!(list_unassigned_topics(&conn, "a1", 10).unwrap().len(), 1);
    let counts = count_category_members(&conn, "a1").unwrap();
    assert_eq!(counts.get(&cat.id).copied(), Some(1));
}

// ---- タグ操作（issue #359 / #313 段階2）----

/// テスト用に topic ノードを 1 件積む（`node_type='topic'`, `source_type='session_log'`）。
fn insert_test_topic(conn: &Connection, agent_id: &str, id: &str, short_id: &str) {
    insert_index_node(
        conn,
        &IndexNodeRow {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: "session_log".to_string(),
            title: format!("topic-{id}"),
            summary: "s".to_string(),
            start_log_id: None,
            end_log_id: None,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-06-01T00:00:00Z".to_string(),
            updated_at: "2026-06-01T00:00:00Z".to_string(),
            short_id: Some(short_id.to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
}

/// #359: 1 つの topic に複数タグが付き（多対多）、付け直し・外しができる（sticky でない）。
/// 二重付与は PK 冪等。無いタグ名は新設され、既存タグは title 一致で束ねて二重作成しない。
#[test]
fn tag_topic_multi_tag_untag_retag() {
    let conn = setup();
    let now = "2026-06-01T00:00:00Z";
    insert_test_topic(&conn, "a1", "t1", "t1");

    // 2 つのタグを付ける（両方新設）。1 topic に複数タグ。
    tag_topic(
        &conn,
        "a1",
        "t1",
        &["Rust".to_string(), "設計".to_string()],
        now,
    )
    .unwrap();
    let counts = count_category_members(&conn, "a1").unwrap();
    let total: i64 = counts.values().sum();
    assert_eq!(total, 2, "1 topic に 2 タグが付く（多対多）");
    // タグは category ノードとして browse で引ける。
    assert!(get_category_node_by_title(&conn, "a1", "Rust")
        .unwrap()
        .is_some());
    assert!(get_category_node_by_title(&conn, "a1", "設計")
        .unwrap()
        .is_some());
    assert_eq!(list_top_level_categories(&conn, "a1").unwrap().len(), 2);

    // 同じタグの二重付与は冪等（PK で弾かれ、件数もノード数も増えない）。
    tag_topic(&conn, "a1", "t1", &["Rust".to_string()], now).unwrap();
    let total2: i64 = count_category_members(&conn, "a1").unwrap().values().sum();
    assert_eq!(total2, 2, "二重付与は冪等（増えない）");
    assert_eq!(list_top_level_categories(&conn, "a1").unwrap().len(), 2);

    // 外せる（member を削除。タグノード自体は残る）。
    assert!(remove_tag_member(&conn, "a1", "t1", "Rust").unwrap());
    let rust_id = get_category_node_by_title(&conn, "a1", "Rust")
        .unwrap()
        .unwrap()
        .id;
    assert_eq!(
        count_category_members(&conn, "a1")
            .unwrap()
            .get(&rust_id)
            .copied(),
        None,
        "外したタグの member は 0 件"
    );
    assert!(
        get_category_node_by_title(&conn, "a1", "Rust")
            .unwrap()
            .is_some(),
        "外してもタグノード自体は消えない"
    );
    // 二度目の外しは false（もう付いていない）。
    assert!(!remove_tag_member(&conn, "a1", "t1", "Rust").unwrap());

    // 付け直せる（sticky でない = 一期一会）。
    tag_topic(&conn, "a1", "t1", &["Rust".to_string()], now).unwrap();
    assert_eq!(
        count_category_members(&conn, "a1")
            .unwrap()
            .get(&rust_id)
            .copied(),
        Some(1),
        "外した後にまた付けられる（sticky でない）"
    );
}

/// #359: タグ新設が黙って失敗しない。`resolve_or_create_tag` は新設したノードを read-back
/// で検証してから id を返す（`insert_index_node` の `INSERT OR IGNORE` が CHECK 違反等を
/// 握り潰しても、実在しなければ Err にする / #344 の教訓）。返る id は必ず実在する。
/// 既存タグは title 一致で束ねて二重作成しない＝冪等。空白のみの名前は拒否する。
#[test]
fn resolve_or_create_tag_is_verified_and_idempotent() {
    let conn = setup();
    let now = "2026-06-01T00:00:00Z";

    // 新設: 返る id は必ず実在ノードを指す（黙って失敗＝ダングリング id を返さない）。
    let id1 = resolve_or_create_tag(&conn, "a1", "Rust", now).unwrap();
    assert!(
        get_index_node(&conn, &id1).unwrap().is_some(),
        "新設タグの id が実在ノードを指す（read-back 検証）"
    );

    // 冪等: 同名は同じ id を返し、ノードを二重に作らない。
    let id2 = resolve_or_create_tag(&conn, "a1", "Rust", now).unwrap();
    assert_eq!(id1, id2, "同名タグは同じ id（二重作成しない）");
    assert_eq!(
        list_top_level_categories(&conn, "a1").unwrap().len(),
        1,
        "ノードは 1 つだけ"
    );
    // 前後の空白は正規化される（別タグにならない）。
    let id3 = resolve_or_create_tag(&conn, "a1", "  Rust ", now).unwrap();
    assert_eq!(id1, id3, "前後空白は正規化されて同じタグ");

    // 空白のみ / 空文字は拒否（無名タグを作らない）。
    assert!(resolve_or_create_tag(&conn, "a1", "", now).is_err());
    assert!(resolve_or_create_tag(&conn, "a1", "   ", now).is_err());
    assert_eq!(
        list_top_level_categories(&conn, "a1").unwrap().len(),
        1,
        "拒否時にノードを増やさない"
    );
}

/// #359: `merge_tags` は from の member を into へ付け替え、from ノードを削除する。
/// - 付け替え先に同じ (topic, into) 行が既にあっても落ちない（OR IGNORE でスキップ）。
/// - from タグノード削除で **FTS 孤児を残さない**（`delete_index_node` の subtree CTE 経由）。
#[test]
fn merge_tags_reassigns_without_collision_and_cleans_fts() {
    let conn = setup();
    let now = "2026-06-01T00:00:00Z";
    insert_test_topic(&conn, "a1", "t1", "t1");
    insert_test_topic(&conn, "a1", "t2", "t2");

    // t1 は from と into の両方、t2 は from だけ。統合すると t1 が付け替え衝突を起こす。
    tag_topic(
        &conn,
        "a1",
        "t1",
        &["旧".to_string(), "新".to_string()],
        now,
    )
    .unwrap();
    tag_topic(&conn, "a1", "t2", &["旧".to_string()], now).unwrap();

    let from_id = get_category_node_by_title(&conn, "a1", "旧")
        .unwrap()
        .unwrap()
        .id;
    // from ノードの FTS 行が存在することを事前確認（後で孤児が残らないことを見るため）。
    let fts_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_index_fts WHERE node_id = ?1",
            params![from_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_before, 1, "from タグは FTS に載っている");

    // 統合（付け替え衝突を含む）: t1 は既に「新」を持つので OR IGNORE でスキップされ落ちない。
    let outcome = merge_tags(&conn, "a1", "旧", "新", now).unwrap();
    assert_eq!(outcome.from_category_id, from_id);

    // from ノードは消え、from の member も残らない（孤児 member 無し）。
    assert!(
        get_category_node_by_title(&conn, "a1", "旧")
            .unwrap()
            .is_none(),
        "統合後 from タグノードは削除される"
    );
    assert_eq!(
        count_category_members(&conn, "a1")
            .unwrap()
            .get(&from_id)
            .copied(),
        None,
        "from の member は残らない"
    );
    // into には t1・t2 の 2 件（t1 は元々あった 1 件のまま、衝突で二重にならない）。
    let into_id = get_category_node_by_title(&conn, "a1", "新")
        .unwrap()
        .unwrap()
        .id;
    assert_eq!(
        count_category_members(&conn, "a1")
            .unwrap()
            .get(&into_id)
            .copied(),
        Some(2),
        "into に 2 topic（衝突で二重にならない）"
    );

    // FTS 孤児が残らない: 消えた from ノードの FTS 行が無い。
    let fts_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_index_fts WHERE node_id = ?1",
            params![from_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_after, 0, "from タグ削除で FTS 孤児を残さない");

    // 自己統合（from == into）は拒否（from を消すと into も消えるため）。
    assert!(merge_tags(&conn, "a1", "新", "新", now).is_err());
    // 存在しない from の統合も拒否。
    assert!(merge_tags(&conn, "a1", "無", "新", now).is_err());
}

/// 実データに近い規模（topic 数千件）でカテゴリ層のクエリが破綻しないことを確認する。
/// LLM は使わず、割当の「頭脳」以外（未割当抽出・sticky 割当・件数集計・冪等性）を
/// 実規模で検証する。CI を重くしないため #[ignore]（`--ignored` で明示実行）。
#[test]
#[ignore]
fn category_layer_scales_to_thousands_of_topics() {
    let conn = setup();
    let now = "2026-06-01T00:00:00Z";
    let root = ensure_category_root(&conn, "a1", now).unwrap();
    // 5 カテゴリを種として用意。
    let cats: Vec<String> = (0..5)
        .map(|i| {
            insert_category_node(&conn, "a1", &root, &format!("カテゴリ{i}"), "", now)
                .unwrap()
                .id
        })
        .collect();

    // 3,000 topic を積む（session_log ツリー）。
    let n: i64 = 3000;
    insert_index_node(
        &conn,
        &IndexNodeRow {
            id: "s1".to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "session".to_string(),
            source_type: "session_log".to_string(),
            title: "S".to_string(),
            summary: String::new(),
            start_log_id: None,
            end_log_id: None,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: now.to_string(),
            updated_at: now.to_string(),
            short_id: Some("s1".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
    for i in 0..n {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("t{i}"),
                agent_id: "a1".to_string(),
                parent_id: Some("s1".to_string()),
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: format!("topic {i}"),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: format!("2026-06-01T00:00:{:02}Z", i % 60),
                updated_at: now.to_string(),
                short_id: Some(format!("t{i}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }

    // バッチ 12 件ずつ、未割当が尽きるまで sticky 割当する（LLM の代わりに round-robin）。
    let started = std::time::Instant::now();
    let mut total = 0usize;
    let mut batches = 0usize;
    loop {
        let batch = list_unassigned_topics(&conn, "a1", 12).unwrap();
        if batch.is_empty() {
            break;
        }
        for (k, t) in batch.iter().enumerate() {
            assert!(
                assign_topic_to_category(&conn, "a1", &t.id, &cats[(total + k) % 5], now).unwrap()
            );
        }
        total += batch.len();
        batches += 1;
        assert!((batches as i64) < n, "バッチが前進していない（無限ループ）");
    }
    let elapsed = started.elapsed();
    assert_eq!(total, n as usize, "全 topic が割り当てられる");
    // 未割当が尽き、再度引いても空（sticky・冪等）。
    assert!(list_unassigned_topics(&conn, "a1", 12).unwrap().is_empty());
    // 件数集計はカテゴリ数ぶんに収まる（トップレベルが 5 のまま膨らまない）。
    let counts = count_category_members(&conn, "a1").unwrap();
    assert_eq!(counts.values().sum::<i64>(), n);
    assert_eq!(counts.len(), 5);
    assert_eq!(list_top_level_categories(&conn, "a1").unwrap().len(), 5);
    // 冪等: 既割当への再割当は false（結果が変わらない）。
    assert!(!assign_topic_to_category(&conn, "a1", "t0", &cats[0], now).unwrap());
    eprintln!(
        "[scale] {n} topics assigned in {batches} batches, {} ms",
        elapsed.as_millis()
    );
}

// ============================================
// Channel Heartbeat Interval 解決（#336）
// ============================================

/// テスト用にチャンネルのハートビート間隔を仕込む。
fn seed_channel_interval(
    conn: &Connection,
    channel_id: &str,
    agent_id: &str,
    interval: Option<u64>,
) {
    upsert_channel_config(
        conn,
        &ChannelConfigRow {
            channel_id: channel_id.to_string(),
            agent_id: agent_id.to_string(),
            guild_id: "g1".to_string(),
            channel_name: String::new(),
            readable: true,
            writable: true,
            whitelisted: false,
            heartbeat_enabled: true,
            heartbeat_interval_secs: interval,
            heartbeat_instructions: String::new(),
        },
    )
    .unwrap();
}

/// チャンネルの値が最優先（channel → agent → 既定）。
#[test]
fn resolve_channel_heartbeat_prefers_channel_value() {
    let conn = crate::init_memory().unwrap();
    // agent 設定（フォールバック候補）。
    upsert_agent_heartbeat_config(
        &conn,
        &AgentHeartbeatConfigRow {
            agent_id: "a1".to_string(),
            enabled: false,
            interval_secs: Some(7200),
        },
    )
    .unwrap();

    // channel=Some(900): channel が勝つ。
    let r = resolve_channel_heartbeat_interval(&conn, "a1", Some(900), 1800, 300);
    assert_eq!((r.interval_secs, r.source), (900, "channel"));
}

/// チャンネル未設定なら agent 設定へフォールバック（enabled は問わない）。
#[test]
fn resolve_channel_heartbeat_falls_back_to_agent() {
    let conn = crate::init_memory().unwrap();
    upsert_agent_heartbeat_config(
        &conn,
        &AgentHeartbeatConfigRow {
            agent_id: "a1".to_string(),
            enabled: false, // 無効でも interval 値はフォールバックに使う
            interval_secs: Some(7200),
        },
    )
    .unwrap();

    let r = resolve_channel_heartbeat_interval(&conn, "a1", None, 1800, 300);
    assert_eq!((r.interval_secs, r.source), (7200, "agent"));
}

/// チャンネルも agent も無ければ運用者既定。
#[test]
fn resolve_channel_heartbeat_falls_back_to_default() {
    let conn = crate::init_memory().unwrap();
    let r = resolve_channel_heartbeat_interval(&conn, "a1", None, 1800, 300);
    assert_eq!((r.interval_secs, r.source), (1800, "default"));

    // agent 行はあるが interval None（既定に従う）→ 既定。
    upsert_agent_heartbeat_config(
        &conn,
        &AgentHeartbeatConfigRow {
            agent_id: "a1".to_string(),
            enabled: true,
            interval_secs: None,
        },
    )
    .unwrap();
    let r = resolve_channel_heartbeat_interval(&conn, "a1", None, 1800, 300);
    assert_eq!((r.interval_secs, r.source), (1800, "default"));
}

/// 下限はチャンネル単位でも効く（#336 決定3）。channel 値が下限未満なら引き上げる。
#[test]
fn resolve_channel_heartbeat_clamps_below_floor() {
    let conn = crate::init_memory().unwrap();
    // channel=60 < floor 300 → 300 へ引き上げ。
    let r = resolve_channel_heartbeat_interval(&conn, "a1", Some(60), 1800, 300);
    assert_eq!((r.interval_secs, r.source), (300, "clamped"));

    // agent フォールバック値が下限未満でも引き上げる。
    upsert_agent_heartbeat_config(
        &conn,
        &AgentHeartbeatConfigRow {
            agent_id: "a1".to_string(),
            enabled: false,
            interval_secs: Some(120),
        },
    )
    .unwrap();
    let r = resolve_channel_heartbeat_interval(&conn, "a1", None, 1800, 300);
    assert_eq!((r.interval_secs, r.source), (300, "clamped"));

    // 壊れた channel 値（0）は未設定扱い → agent 120 → clamp 300。
    let r = resolve_channel_heartbeat_interval(&conn, "a1", Some(0), 1800, 300);
    assert_eq!((r.interval_secs, r.source), (300, "clamped"));
}

/// 実データが失われないこと（既存の他エージェントの channel 行が混ざらない）。
#[test]
fn resolve_channel_heartbeat_is_per_agent() {
    let conn = crate::init_memory().unwrap();
    seed_channel_interval(&conn, "ch1", "a1", Some(900));
    seed_channel_interval(&conn, "ch1", "a2", Some(1200));

    // a1 の channel 行の値を渡せば a1 用に解決される（channel_interval は呼び出し側が
    // 該当行から取り出して渡す設計）。
    let r1 = resolve_channel_heartbeat_interval(&conn, "a1", Some(900), 1800, 300);
    assert_eq!(r1.interval_secs, 900);
    let r2 = resolve_channel_heartbeat_interval(&conn, "a2", Some(1200), 1800, 300);
    assert_eq!(r2.interval_secs, 1200);
}

// ============================================
// スリープ整理ラン（#313 段階3 / #361）: マーカー + worklist クエリ
// ============================================

/// テスト用に topic ノードを 1 件入れる（created_at / end_log_id / source_type を指定）。
fn seed_topic(
    conn: &Connection,
    agent_id: &str,
    id: &str,
    short: &str,
    created_at: &str,
    end_log_id: Option<i64>,
    source_type: &str,
) {
    insert_index_node(
        conn,
        &IndexNodeRow {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: source_type.to_string(),
            title: format!("題 {short}"),
            summary: "s".to_string(),
            start_log_id: None,
            end_log_id,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 3,
            child_count: 0,
            token_count: 0,
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
            short_id: Some(short.to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
}

#[test]
fn organize_marker_get_set_roundtrip_and_default_none() {
    let conn = setup();
    // config 行が無ければ None（get_memory_index_config の非永続デフォルトに引きずられない）。
    assert_eq!(get_last_organize_at(&conn, "a1").unwrap(), None);
    // set は行を作る（UPSERT）。
    set_last_organize_at(&conn, "a1", "2026-08-03T00:00:00Z").unwrap();
    assert_eq!(
        get_last_organize_at(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-03T00:00:00Z")
    );
    // 上書きは last_organize_at のみを更新し、他エージェントに漏れない。
    set_last_organize_at(&conn, "a1", "2026-08-04T00:00:00Z").unwrap();
    assert_eq!(
        get_last_organize_at(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-04T00:00:00Z")
    );
    assert_eq!(get_last_organize_at(&conn, "a2").unwrap(), None);
}

#[test]
fn organize_marker_does_not_disturb_skill_consolidation_marker() {
    let conn = setup();
    // 既存の skill 棚卸しマーカーが立っている状態で organize マーカーを刻んでも、
    // 隣の列は消えない（同じ config 行を共有するため）。
    set_last_skill_consolidation_at(&conn, "a1", "2026-07-01T00:00:00Z").unwrap();
    set_last_organize_at(&conn, "a1", "2026-08-03T00:00:00Z").unwrap();
    assert_eq!(
        get_last_skill_consolidation_at(&conn, "a1")
            .unwrap()
            .as_deref(),
        Some("2026-07-01T00:00:00Z")
    );
    assert_eq!(
        get_last_organize_at(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-03T00:00:00Z")
    );
}

#[test]
fn organize_worklist_respects_since_snapshot_limit_and_order() {
    let conn = setup();
    // スナップショット内（end_log_id <= 100）で、マーカー(2026-08-02)以降の topic を古い順に。
    seed_topic(
        &conn,
        "a1",
        "old",
        "t1",
        "2026-08-01T00:00:00Z",
        Some(10),
        "session_log",
    ); // マーカー前 → 除外
    seed_topic(
        &conn,
        "a1",
        "n1",
        "t2",
        "2026-08-03T00:00:00Z",
        Some(50),
        "session_log",
    );
    seed_topic(
        &conn,
        "a1",
        "n2",
        "t3",
        "2026-08-04T00:00:00Z",
        Some(80),
        "session_log",
    );
    seed_topic(
        &conn,
        "a1",
        "future",
        "t4",
        "2026-08-05T00:00:00Z",
        Some(200),
        "session_log",
    ); // snapshot 超過 → 除外
       // topic 以外・他エージェント・category は対象外。
    seed_topic(
        &conn,
        "a1",
        "cat",
        "c1",
        "2026-08-03T00:00:00Z",
        Some(60),
        "category",
    );
    seed_topic(
        &conn,
        "a2",
        "other",
        "o1",
        "2026-08-03T00:00:00Z",
        Some(60),
        "session_log",
    );

    let since = Some(("2026-08-02T00:00:00Z", ""));
    // 件数ゲート（下限判定用）。
    assert_eq!(count_organize_topics(&conn, "a1", since, 100).unwrap(), 2);
    // worklist は (created_at, id) 昇順で n1, n2。
    let wl = list_organize_topics(&conn, "a1", since, 100, 50).unwrap();
    let ids: Vec<&str> = wl.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["n1", "n2"]);

    // since=None なら下端制約なし（マーカー前の old も入る。future は依然 snapshot 超過で除外）。
    assert_eq!(count_organize_topics(&conn, "a1", None, 100).unwrap(), 3);
}

#[test]
fn organize_worklist_limit_leaves_remainder_for_next_time() {
    let conn = setup();
    for i in 1..=5 {
        // created_at を昇順に振る（08-03T00:0i）。
        let ts = format!("2026-08-03T00:0{i}:00Z");
        seed_topic(
            &conn,
            "a1",
            &format!("n{i}"),
            &format!("t{i}"),
            &ts,
            Some(10 + i),
            "session_log",
        );
    }
    let since = Some(("2026-08-02T00:00:00Z", ""));
    // 下限判定は全 5 件を数える。
    assert_eq!(count_organize_topics(&conn, "a1", since, 100).unwrap(), 5);
    // N=3 で切ると (created_at, id) 昇順 3 件。残りの n4/n5 はより新しいので、
    // 末尾の (created_at, id) カーソルを刻めば次回に拾える（前進のみ / 残りは次回）。
    let wl = list_organize_topics(&conn, "a1", since, 100, 3).unwrap();
    let ids: Vec<&str> = wl.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["n1", "n2", "n3"]);
    let last = wl.last().unwrap();
    let cursor = (last.created_at.as_str(), last.id.as_str());

    // 次回: カーソルを進めると残り 2 件だけが対象（重複しない）。
    let next = list_organize_topics(&conn, "a1", Some(cursor), 100, 3).unwrap();
    let next_ids: Vec<&str> = next.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(next_ids, vec!["n4", "n5"]);
}

#[test]
fn organize_worklist_includes_null_end_log_id() {
    let conn = setup();
    // end_log_id NULL（索引済みとみなす）は snapshot 内として拾う。
    seed_topic(
        &conn,
        "a1",
        "n1",
        "t1",
        "2026-08-03T00:00:00Z",
        None,
        "session_log",
    );
    assert_eq!(
        count_organize_topics(&conn, "a1", Some(("2026-08-02T00:00:00Z", "")), 5).unwrap(),
        1
    );
}

/// 回帰（blocker / PR #364 レビュー）: 索引ビルドは 1 パスの全 topic に**同一 created_at**
/// を刻む（`index_builder.rs`）。新規が N を超え、切り口が同着群の内側に落ちても、
/// `(created_at, id)` カーソルなら残余を次回に引き継いで取りこぼさないこと。
#[test]
fn organize_worklist_same_created_at_group_not_dropped_across_runs() {
    let conn = setup();
    let ts = "2026-08-03T00:00:00Z";
    // 同着 created_at の 51 件（id は昇順で相異）。本番の topic id 形に寄せる。
    for i in 0..51 {
        seed_topic(
            &conn,
            "a1",
            &format!("topic-a1-s-{i:03}"),
            &format!("t{i:03}"),
            ts,
            Some(10),
            "session_log",
        );
    }
    let since = Some(("2026-08-02T00:00:00Z", ""));
    // run1: N=50。
    let wl1 = list_organize_topics(&conn, "a1", since, 100, 50).unwrap();
    assert_eq!(wl1.len(), 50);
    // マーカー前進 = 提示した末尾の (created_at, id) カーソル。
    let last = wl1.last().unwrap();
    let cursor = (last.created_at.as_str(), last.id.as_str());
    // run2: 残り 1 件が次回に拾える（同着でも取りこぼさない）。
    let wl2 = list_organize_topics(&conn, "a1", Some(cursor), 100, 50).unwrap();
    assert_eq!(wl2.len(), 1, "同着 created_at 群の残余が取りこぼされている");
    // run1 と run2 は重複しない（カーソルより後の 1 件だけ）。
    assert!(
        wl1.iter().all(|t| t.id != wl2[0].id),
        "run1 と run2 の topic が重複している"
    );
    // count も同じカーソルで残余 1 を返す（ゲートと worklist の整合）。
    assert_eq!(
        count_organize_topics(&conn, "a1", Some(cursor), 100).unwrap(),
        1
    );
}

// ============================================
// スリープ整理ラン（#313 段階3b / #365）: 過去分の遡り消化マーカー + 降順 worklist
// ============================================

#[test]
fn organize_backlog_cursor_get_set_roundtrip_and_default_none() {
    let conn = setup();
    // 行が無ければ None。
    assert_eq!(get_organize_backlog_cursor(&conn, "a1").unwrap(), None);
    set_organize_backlog_cursor(&conn, "a1", "2026-08-03T00:00:00Z|old1").unwrap();
    assert_eq!(
        get_organize_backlog_cursor(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-03T00:00:00Z|old1")
    );
    // 上書きは当該列のみ。他エージェントに漏れない。
    set_organize_backlog_cursor(&conn, "a1", "2026-08-01T00:00:00Z|old9").unwrap();
    assert_eq!(
        get_organize_backlog_cursor(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-01T00:00:00Z|old9")
    );
    assert_eq!(get_organize_backlog_cursor(&conn, "a2").unwrap(), None);
}

#[test]
fn organize_last_run_at_get_set_roundtrip_and_default_none() {
    let conn = setup();
    assert_eq!(get_organize_last_run_at(&conn, "a1").unwrap(), None);
    set_organize_last_run_at(&conn, "a1", "2026-08-03T00:00:00Z").unwrap();
    assert_eq!(
        get_organize_last_run_at(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-03T00:00:00Z")
    );
    set_organize_last_run_at(&conn, "a1", "2026-08-04T00:00:00Z").unwrap();
    assert_eq!(
        get_organize_last_run_at(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-04T00:00:00Z")
    );
    assert_eq!(get_organize_last_run_at(&conn, "a2").unwrap(), None);
}

#[test]
fn organize_markers_three_axes_do_not_disturb_each_other() {
    let conn = setup();
    // 3 マーカー（新規位置 / 遡り位置 / throttle 刻時）+ skill 棚卸しが同じ config 行を
    // 共有しても互いに消えない。
    set_last_skill_consolidation_at(&conn, "a1", "2026-07-01T00:00:00Z").unwrap();
    set_last_organize_at(&conn, "a1", "2026-08-04T00:00:00Z|n5").unwrap();
    set_organize_backlog_cursor(&conn, "a1", "2026-06-01T00:00:00Z|old3").unwrap();
    set_organize_last_run_at(&conn, "a1", "2026-08-05T00:00:00Z").unwrap();
    assert_eq!(
        get_last_skill_consolidation_at(&conn, "a1")
            .unwrap()
            .as_deref(),
        Some("2026-07-01T00:00:00Z")
    );
    assert_eq!(
        get_last_organize_at(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-04T00:00:00Z|n5")
    );
    assert_eq!(
        get_organize_backlog_cursor(&conn, "a1").unwrap().as_deref(),
        Some("2026-06-01T00:00:00Z|old3")
    );
    assert_eq!(
        get_organize_last_run_at(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-05T00:00:00Z")
    );
}

#[test]
fn organize_backlog_respects_boundary_snapshot_and_desc_order() {
    let conn = setup();
    // 境界（遡りカーソル）= 2026-08-02。これより古い topic だけが過去分。
    seed_topic(
        &conn,
        "a1",
        "b1",
        "t1",
        "2026-08-01T00:00:00Z",
        Some(30),
        "session_log",
    );
    seed_topic(
        &conn,
        "a1",
        "b2",
        "t2",
        "2026-07-15T00:00:00Z",
        Some(20),
        "session_log",
    );
    seed_topic(
        &conn,
        "a1",
        "b3",
        "t3",
        "2026-07-01T00:00:00Z",
        Some(10),
        "session_log",
    );
    // 境界より新しい（=新規側の領域）→ 過去分に入らない。
    seed_topic(
        &conn,
        "a1",
        "recent",
        "t4",
        "2026-08-05T00:00:00Z",
        Some(40),
        "session_log",
    );
    // snapshot 超過（end_log_id > 100）→ 除外。
    seed_topic(
        &conn,
        "a1",
        "beyond",
        "t5",
        "2026-07-10T00:00:00Z",
        Some(200),
        "session_log",
    );
    // 別エージェント・category → 対象外。
    seed_topic(
        &conn,
        "a2",
        "o1",
        "o",
        "2026-07-05T00:00:00Z",
        Some(20),
        "session_log",
    );
    seed_topic(
        &conn,
        "a1",
        "cat",
        "c1",
        "2026-07-05T00:00:00Z",
        Some(20),
        "category",
    );

    let before = ("2026-08-02T00:00:00Z", "");
    // 残数（監査・先頭到達判定）= b1,b2,b3 の 3 件。
    assert_eq!(
        count_organize_backlog_topics(&conn, "a1", before, 100).unwrap(),
        3
    );
    // worklist は created_at **降順**（新しい過去分から遡る）: b1(08-01) → b2(07-15) → b3(07-01)。
    let wl = list_organize_backlog_topics(&conn, "a1", before, 100, 50).unwrap();
    let ids: Vec<&str> = wl.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["b1", "b2", "b3"]);
}

/// 回帰（#364 blocker と同型・**降順側**）: 索引ビルドは 1 パスの全 topic に同一 created_at を
/// 刻む。遡りが N を超え、切り口が同着群の内側に落ちても、`(created_at, id)` カーソル（降順）
/// なら残余を次回に引き継いで取りこぼさないこと。
#[test]
fn organize_backlog_same_created_at_group_not_dropped_descending() {
    let conn = setup();
    let ts = "2026-07-01T00:00:00Z"; // 境界より古い同着 created_at
    for i in 0..51 {
        seed_topic(
            &conn,
            "a1",
            &format!("topic-a1-s-{i:03}"),
            &format!("t{i:03}"),
            ts,
            Some(10),
            "session_log",
        );
    }
    // 境界は同着群より新しい任意時刻。id="" 付きなので created_at < 境界 の全件が対象。
    let before = ("2026-08-01T00:00:00Z", "");
    // run1: N=50（降順 = id 降順で上位 50 = 050..001）。
    let wl1 = list_organize_backlog_topics(&conn, "a1", before, 100, 50).unwrap();
    assert_eq!(wl1.len(), 50);
    // 遡りマーカー = 提示した中で最も古い（降順の末尾）の (created_at, id)。
    let oldest = wl1.last().unwrap();
    let cursor = (oldest.created_at.as_str(), oldest.id.as_str());
    // run2: 残り 1 件（同着でも取りこぼさない）。
    let wl2 = list_organize_backlog_topics(&conn, "a1", cursor, 100, 50).unwrap();
    assert_eq!(
        wl2.len(),
        1,
        "同着 created_at 群の残余が降順側で取りこぼされている"
    );
    assert!(
        wl1.iter().all(|t| t.id != wl2[0].id),
        "run1 と run2 の topic が重複している"
    );
    // count も同じカーソルで残余 1（ゲートと worklist の整合）。
    assert_eq!(
        count_organize_backlog_topics(&conn, "a1", cursor, 100).unwrap(),
        1
    );
}

#[test]
fn organize_backlog_reaches_head_returns_empty() {
    let conn = setup();
    seed_topic(
        &conn,
        "a1",
        "b1",
        "t1",
        "2026-07-01T00:00:00Z",
        Some(10),
        "session_log",
    );
    seed_topic(
        &conn,
        "a1",
        "b2",
        "t2",
        "2026-06-01T00:00:00Z",
        Some(11),
        "session_log",
    );
    // カーソルを最古（b2）ちょうどに置く: b2 は `id < ""`不成立で除外、b1 は created_at>cursor で除外。
    let before = ("2026-06-01T00:00:00Z", "");
    assert_eq!(
        count_organize_backlog_topics(&conn, "a1", before, 100).unwrap(),
        0,
        "先頭到達で残数 0"
    );
    let wl = list_organize_backlog_topics(&conn, "a1", before, 100, 50).unwrap();
    assert!(wl.is_empty(), "先頭到達で 0 件（無限に走らない）");
}

// ============================================
// 記憶の単位（宣言 / issue #379 #376 段階1）
// ============================================

/// 生ログを 1 件挿入して id を返す（created_at は now）。
fn ins_log(conn: &Connection, agent: &str, session: &str, content: &str) -> i64 {
    insert_session_log(
        conn,
        &SessionLogRow {
            id: None,
            agent_id: agent.to_string(),
            session_id: session.to_string(),
            log_type: "message".to_string(),
            content: content.to_string(),
            speaker_id: None,
            turn_number: None,
            metadata_json: None,
            created_at: None,
        },
    )
    .unwrap()
}

fn members_for(conn: &Connection, agent: &str, topic_id: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM memory_category_members WHERE agent_id = ?1 AND topic_id = ?2",
        params![agent, topic_id],
        |r| r.get(0),
    )
    .unwrap()
}

fn fts_has_node(conn: &Connection, node_id: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM memory_index_fts WHERE node_id = ?1",
        params![node_id],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

/// **設計の核**: 宣言ユニット（node_type='unit'）は time-series・タグ整理の worklist 系
/// クエリに 1 件も出ない（構造的分離）。一方 browse（get_index_tree）と search（FTS）には出る。
#[test]
fn declared_unit_excluded_from_all_worklists_but_visible_to_browse_and_search() {
    let conn = setup();
    // session_log topic を 2 件（keywords 未設定＝ backfill 対象）。
    seed_topic(
        &conn,
        "a1",
        "t1",
        "s1",
        "2026-08-01T00:00:00Z",
        Some(10),
        "session_log",
    );
    seed_topic(
        &conn,
        "a1",
        "t2",
        "s2",
        "2026-08-02T00:00:00Z",
        Some(20),
        "session_log",
    );
    // 生ログを積んで宣言範囲を作る。
    for i in 0..5 {
        ins_log(&conn, "a1", "sess", &format!("発話 {i}"));
    }
    let now = "2026-08-04T00:00:00Z";
    let unit = record_memory_unit(
        &conn,
        "a1",
        "Rust の所有権を理解した週",
        "所有権と借用の話",
        1,
        5,
        Some("2026-08-04"),
        Some("2026-08-04"),
        now,
    )
    .unwrap();
    assert_eq!(unit.node_type, "unit");
    assert_eq!(unit.source_type, "declared");
    assert_eq!(unit.short_id.as_deref(), Some("u1"));

    // --- worklist 系: 宣言ユニットは 1 件も出ない（session_log topic だけ） ---
    // (1) スリープ整理（新規側）
    assert_eq!(count_organize_topics(&conn, "a1", None, 100).unwrap(), 2);
    let wl = list_organize_topics(&conn, "a1", None, 100, 50).unwrap();
    assert_eq!(wl.len(), 2);
    assert!(wl.iter().all(|n| n.node_type == "topic"));
    // (2) スリープ整理（遡り側）
    let before = ("2027-01-01T00:00:00Z", "");
    assert_eq!(
        count_organize_backlog_topics(&conn, "a1", before, 100).unwrap(),
        2
    );
    let bl = list_organize_backlog_topics(&conn, "a1", before, 100, 50).unwrap();
    assert!(bl.iter().all(|n| n.id != unit.id));
    // (3) タグ割当 worklist（未分類 topic）
    let unassigned = list_unassigned_topics(&conn, "a1", 50).unwrap();
    assert_eq!(unassigned.len(), 2, "unit は未分類 topic に混ざらない");
    assert!(unassigned.iter().all(|n| n.id != unit.id));
    // (4) keywords バックフィル worklist（unit も keywords='[]' だが対象外）
    let missing = list_topics_missing_keywords(&conn, "a1", 50).unwrap();
    assert_eq!(missing.len(), 2);
    assert!(missing.iter().all(|n| n.id != unit.id));

    // --- browse / search: 宣言ユニットは見える（意図どおり） ---
    let tree = get_index_tree(&conn, "a1").unwrap();
    assert!(
        tree.iter()
            .any(|n| n.id == unit.id && n.node_type == "unit"),
        "browse（get_index_tree）には宣言ユニットが出る"
    );
    let hits = search_index_nodes(&conn, "a1", "所有権", 10, None).unwrap();
    assert!(
        hits.iter().any(|h| h.node_id == unit.id),
        "search（FTS）で宣言ユニットが引ける"
    );
}

/// 月次ロールアップの topic 数集計（count_topics_per_period）は宣言ユニットで水増しされない。
#[test]
fn declared_unit_does_not_inflate_rollup_topic_count() {
    let conn = setup();
    // root→period→session→topic の time-series ツリー。
    for (id, ntype, parent) in [
        ("root-a1", "root", None),
        ("p1", "period", Some("root-a1")),
        ("s1", "session", Some("p1")),
    ] {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: parent.map(String::from),
                node_type: ntype.to_string(),
                source_type: "session_log".to_string(),
                title: id.to_string(),
                summary: String::new(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-06-01T00:00:00Z".to_string(),
                updated_at: "2026-06-01T00:00:00Z".to_string(),
                short_id: Some(id.to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    insert_index_node(&conn, &mk_topic_node("tp1", "a1", "topic1", "s", &[])).unwrap();
    // tp1 を session s1 の下へ。
    conn.execute(
        "UPDATE memory_index_nodes SET parent_id = 's1' WHERE id = 'tp1'",
        [],
    )
    .unwrap();

    let before = count_topics_per_period(&conn, "a1").unwrap();
    // 宣言ユニットを足す。
    ins_log(&conn, "a1", "sess", "x");
    record_memory_unit(
        &conn,
        "a1",
        "宣言",
        "",
        1,
        1,
        None,
        None,
        "2026-08-04T00:00:00Z",
    )
    .unwrap();
    let after = count_topics_per_period(&conn, "a1").unwrap();
    assert_eq!(before, after, "宣言ユニットは period の topic 数を変えない");
    assert_eq!(after.get("p1").copied(), Some(1));
}

/// record → retract で原状復帰（宣言ノード・FTS・member が消え、生ログは不変）。
#[test]
fn record_then_retract_restores_state_and_leaves_raw_logs() {
    let conn = setup();
    for i in 0..4 {
        ins_log(&conn, "a1", "sess", &format!("log {i}"));
    }
    let logs_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_sessions", [], |r| r.get(0))
        .unwrap();

    let unit = record_memory_unit(
        &conn,
        "a1",
        "宣言タイトル",
        "要約",
        1,
        4,
        None,
        None,
        "2026-08-04T00:00:00Z",
    )
    .unwrap();
    // タグを 2 つ付ける（member 行が付く）。
    tag_topic(
        &conn,
        "a1",
        &unit.id,
        &["タグA".into(), "タグB".into()],
        "2026-08-04T00:00:00Z",
    )
    .unwrap();
    assert!(fts_has_node(&conn, &unit.id), "record 後 FTS 行あり");
    assert_eq!(members_for(&conn, "a1", &unit.id), 2, "タグ 2 件の member");

    // retract。
    let removed = retract_memory_unit(&conn, "a1", "u1").unwrap();
    assert_eq!(removed, unit.id);
    assert!(
        get_index_node(&conn, &unit.id).unwrap().is_none(),
        "宣言ノードが消える"
    );
    assert!(!fts_has_node(&conn, &unit.id), "FTS 孤児を残さない");
    assert_eq!(members_for(&conn, "a1", &unit.id), 0, "member 行が消える");
    // 生ログは不変。
    let logs_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(logs_before, logs_after, "生ログは消えない・変わらない");
}

/// retract は宣言ユニット以外（session_log topic 等）を消せない（安全ガード）。
#[test]
fn retract_refuses_non_unit_nodes() {
    let conn = setup();
    seed_topic(
        &conn,
        "a1",
        "t1",
        "s1",
        "2026-08-01T00:00:00Z",
        Some(10),
        "session_log",
    );
    let err = retract_memory_unit(&conn, "a1", "s1").unwrap_err();
    assert!(
        err.to_string().contains("宣言ユニット"),
        "session_log topic は retract できない: {err}"
    );
    assert!(
        get_index_node(&conn, "t1").unwrap().is_some(),
        "対象の topic は消えていない"
    );
}

/// エージェント間で宣言を混ぜない（a1 の unit を a2 名義で retract できない）。
#[test]
fn retract_is_agent_scoped() {
    let conn = setup();
    ins_log(&conn, "a1", "sess", "x");
    let unit = record_memory_unit(
        &conn,
        "a1",
        "a1 の宣言",
        "",
        1,
        1,
        None,
        None,
        "2026-08-04T00:00:00Z",
    )
    .unwrap();
    // a2 名義では見つからない。
    assert!(retract_memory_unit(&conn, "a2", &unit.id).is_err());
    assert!(get_index_node(&conn, &unit.id).unwrap().is_some());
}

/// read_my_history は行数キャップを守り、カーソルで続きが読める。
#[test]
fn read_my_history_respects_row_cap_and_cursor() {
    let conn = setup();
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(ins_log(&conn, "a1", "sess", &format!("発話{i}")));
    }
    let filter = HistoryFilter::Session("sess".to_string());
    let page = read_my_history(&conn, "a1", &filter, None, 2, 100_000).unwrap();
    assert_eq!(page.returned, 2);
    assert_eq!(page.range_total, 5, "範囲全体の件数は常に返す");
    assert!(page.truncated);
    assert_eq!(page.next_from_id, Some(ids[2]));
    // 続きをカーソルで読む。
    let page2 = read_my_history(&conn, "a1", &filter, page.next_from_id, 2, 100_000).unwrap();
    assert_eq!(page2.returned, 2);
    assert_eq!(page2.rows[0].id, Some(ids[2]));
    let page3 = read_my_history(&conn, "a1", &filter, page2.next_from_id, 2, 100_000).unwrap();
    assert_eq!(page3.returned, 1);
    assert!(!page3.truncated);
    assert_eq!(page3.next_from_id, None);
}

/// read_my_history は総文字数キャップを守る（ただし先頭 1 行は必ず返す＝前進保証）。
#[test]
fn read_my_history_respects_char_cap_with_progress_guarantee() {
    let conn = setup();
    // 各 30 文字の本文を 4 件。char_cap=50 なら 1 行で超えるので先頭のみ返る。
    for i in 0..4 {
        ins_log(&conn, "a1", "sess", &"あ".repeat(30));
        let _ = i;
    }
    let filter = HistoryFilter::Session("sess".to_string());
    let page = read_my_history(&conn, "a1", &filter, None, 200, 50).unwrap();
    assert_eq!(
        page.returned, 1,
        "先頭 1 行は char_cap を超えても返す（前進保証）"
    );
    assert!(page.truncated);
    assert!(page.next_from_id.is_some());
}

/// read_my_history は agent_id でスコープされる（他エージェントのログを返さない）。
#[test]
fn read_my_history_is_agent_scoped() {
    let conn = setup();
    ins_log(&conn, "a1", "shared", "a1 の発話");
    ins_log(&conn, "a2", "shared", "a2 の発話");
    let filter = HistoryFilter::Session("shared".to_string());
    let page = read_my_history(&conn, "a1", &filter, None, 200, 100_000).unwrap();
    assert_eq!(page.returned, 1);
    assert_eq!(page.rows[0].content, "a1 の発話");
}

/// survey_my_history の集計（件数・セッション数・id 範囲・種別内訳・バケット上限）。
#[test]
fn survey_my_history_aggregates_and_caps_buckets() {
    let conn = setup();
    // created_at と log_type を制御するため直接 INSERT する。
    let rows = [
        ("2026-08-01T09:00:00Z", "message", "sA"),
        ("2026-08-01T10:00:00Z", "message", "sA"),
        ("2026-08-01T11:00:00Z", "reaction", "sB"),
        ("2026-08-02T09:00:00Z", "message", "sC"),
    ];
    for (ts, lt, sess) in rows {
        conn.execute(
            "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, created_at)
             VALUES ('a1', ?1, ?2, 'x', ?3)",
            params![sess, lt, ts],
        )
        .unwrap();
    }
    let survey = survey_my_history(&conn, "a1", "day", 60).unwrap();
    assert_eq!(survey.total_logs, 4);
    assert_eq!(survey.total_sessions, 3);
    assert_eq!(survey.total_buckets, 2);
    assert_eq!(survey.buckets.len(), 2);
    assert!(!survey.truncated);
    // 新しいバケットが先頭（2026-08-02）。
    assert_eq!(survey.buckets[0].bucket, "2026-08-02");
    let day1 = survey
        .buckets
        .iter()
        .find(|b| b.bucket == "2026-08-01")
        .unwrap();
    assert_eq!(day1.log_count, 3);
    assert_eq!(day1.session_count, 2);
    assert_eq!(day1.type_counts.get("message").copied(), Some(2));
    assert_eq!(day1.type_counts.get("reaction").copied(), Some(1));
    // サイズ地図（#386）: content='x' が 3 件で 3 文字、est は 3*2/3=2。
    assert_eq!(day1.content_chars, 3);
    assert_eq!(day1.est_tokens, 2);
    // 全体の総文字数・概算トークンも返す（バケットを落としても残る）。
    assert_eq!(survey.total_content_chars, 4);
    assert_eq!(survey.total_est_tokens, 2);
    // バケット上限で古い側を落とす。
    let capped = survey_my_history(&conn, "a1", "day", 1).unwrap();
    assert_eq!(capped.buckets.len(), 1);
    assert!(capped.truncated);
    assert_eq!(capped.total_buckets, 2, "全体の総バケット数は落とさず返す");
}

// ---- 宣言ランの窓を本人が決める（#394）----

/// `nth_log_id_after` は **id の差ではなく生ログの件数**で数える（id は全エージェント共通の
/// 採番なので、1 エージェントぶんの id は疎らに飛ぶ）。件数が足りなければ最後の id へ丸める。
#[test]
fn nth_log_id_after_counts_rows_not_id_gaps() {
    let conn = setup();
    // a1 と a2 を交互に入れ、a1 の id を飛び飛びにする。
    for i in 0..10 {
        for agent in ["a1", "a2"] {
            conn.execute(
                "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, created_at)
                 VALUES (?1, 's1', 'message', ?2, '2026-08-01T00:00:00Z')",
                params![agent, format!("{agent}-{i}")],
            )
            .unwrap();
        }
    }
    let a1_ids: Vec<i64> = conn
        .prepare("SELECT id FROM memory_sessions WHERE agent_id='a1' ORDER BY id ASC")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(a1_ids.len(), 10);
    assert!(a1_ids[1] - a1_ids[0] > 1, "id が飛んでいる前提が崩れている");

    // cursor=0 から 1 件目 / 3 件目 / 10 件目。
    assert_eq!(
        nth_log_id_after(&conn, "a1", 0, 1).unwrap(),
        Some(a1_ids[0])
    );
    assert_eq!(
        nth_log_id_after(&conn, "a1", 0, 3).unwrap(),
        Some(a1_ids[2])
    );
    assert_eq!(
        nth_log_id_after(&conn, "a1", 0, 10).unwrap(),
        Some(a1_ids[9])
    );
    // 足りなければ最後（あるだけ進める）。
    assert_eq!(
        nth_log_id_after(&conn, "a1", 0, 999).unwrap(),
        Some(a1_ids[9])
    );
    // cursor より後ろだけを数える。
    assert_eq!(
        nth_log_id_after(&conn, "a1", a1_ids[4], 2).unwrap(),
        Some(a1_ids[6])
    );
    // 1 件も無ければ None（＝進める先が無い）。
    assert_eq!(nth_log_id_after(&conn, "a1", a1_ids[9], 1).unwrap(), None);
    // 0 / 負の n は 1 件目に丸める（呼び出し側の 0 除算・逆走を防ぐ）。
    assert_eq!(
        nth_log_id_after(&conn, "a1", 0, 0).unwrap(),
        Some(a1_ids[0])
    );
}

/// 窓の希望（#394）は JSON で 1 列に往復し、壊れた JSON は「希望なし」に倒れる。
#[test]
fn declare_window_pref_roundtrips_and_tolerates_garbage() {
    let conn = setup();
    assert_eq!(get_memory_declare_window(&conn, "a1").unwrap(), None);

    let pref = DeclareWindowPref {
        next_from_id: Some(23_600),
        window_size: Some(450),
        note: Some("材料が薄かったので広げる".to_string()),
        updated_at: Some("2026-08-05T00:00:00Z".to_string()),
        partial_streak: Some(2),
    };
    set_memory_declare_window(&conn, "a1", Some(&pref)).unwrap();
    assert_eq!(get_memory_declare_window(&conn, "a1").unwrap(), Some(pref));

    // 位置と理由を消して広さは残す（ランが位置を使い切ったときの形）。
    let sticky = DeclareWindowPref {
        next_from_id: None,
        window_size: Some(450),
        ..Default::default()
    };
    set_memory_declare_window(&conn, "a1", Some(&sticky)).unwrap();
    let got = get_memory_declare_window(&conn, "a1").unwrap().unwrap();
    assert_eq!(got.next_from_id, None);
    assert_eq!(got.note, None);
    assert_eq!(got.partial_streak, None);
    assert_eq!(got.window_size, Some(450));

    // 旧い形（`partial_streak` が無い JSON）も読める（列は後から足したフィールド）。
    conn.execute(
        "UPDATE agent_memory_index_config SET memory_declare_window = '{\"window_size\":450}' WHERE agent_id='a1'",
        [],
    )
    .unwrap();
    let old = get_memory_declare_window(&conn, "a1").unwrap().unwrap();
    assert_eq!(old.window_size, Some(450));
    assert_eq!(old.partial_streak, None);

    // 隣の列（宣言ランのマーカー）は触らない。
    set_memory_declare_cursor(&conn, "a1", "2026-08-05T00:00:00Z|23594").unwrap();
    set_memory_declare_window(&conn, "a1", Some(&sticky)).unwrap();
    assert_eq!(
        get_memory_declare_cursor(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-05T00:00:00Z|23594")
    );

    // NULL へ戻せる。
    set_memory_declare_window(&conn, "a1", None).unwrap();
    assert_eq!(get_memory_declare_window(&conn, "a1").unwrap(), None);

    // 壊れた JSON はエラーにせず「希望なし」（＝従来どおりの窓）に倒れる。
    conn.execute(
        "UPDATE agent_memory_index_config SET memory_declare_window = 'not json' WHERE agent_id='a1'",
        [],
    )
    .unwrap();
    assert_eq!(get_memory_declare_window(&conn, "a1").unwrap(), None);
}

/// `list_recent_session_logs_of_type`: **SQL 側**で log_type を絞る（#404 / #405）。
///
/// 呼び出し側で絞ると「生の直近 N 件を取ってから捨てる」ことになり、ツール往復の多い
/// セッションでは目的の種別が N の一部しか残らない。ここで固定するのは
/// 「limit は絞ったあとの件数」「他種別が混ざらない」「id DESC で返る」。
#[test]
fn list_recent_session_logs_of_type_filters_in_sql() {
    let conn = setup();
    let mk = |log_type: &str, content: &str| SessionLogRow {
        id: None,
        agent_id: "a1".to_string(),
        session_id: "s1".to_string(),
        log_type: log_type.to_string(),
        content: content.to_string(),
        speaker_id: Some("a1".to_string()),
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    // 発言 1 件のあとにツール往復 20 件、さらに発言 3 件。
    insert_session_log(&conn, &mk("speech", "oldest speech")).unwrap();
    for i in 0..20 {
        insert_session_log(&conn, &mk("tool_result", &format!("tool {i}"))).unwrap();
    }
    for i in 0..3 {
        insert_session_log(&conn, &mk("speech", &format!("speech {i}"))).unwrap();
    }
    // 別セッションは混ざらない。
    let mut other = mk("speech", "other session");
    other.session_id = "s2".to_string();
    insert_session_log(&conn, &other).unwrap();

    // 生の直近 10 件を取ってから絞ると、ツール往復に押し出されて古い発言が落ちる。
    let raw = list_recent_session_logs(&conn, "s1", 10).unwrap();
    assert!(
        !raw.iter().any(|l| l.content == "oldest speech"),
        "前提: 生の窓では古い発言が窓の外にある"
    );

    let speech = list_recent_session_logs_of_type(&conn, "s1", "speech", 10).unwrap();
    assert_eq!(speech.len(), 4, "limit は絞ったあとの件数");
    assert!(speech.iter().all(|l| l.log_type == "speech"));
    assert!(speech.iter().all(|l| l.session_id == "s1"));
    // id DESC（呼び出し側が reverse する前提）。
    assert_eq!(speech[0].content, "speech 2");
    assert_eq!(speech[3].content, "oldest speech");

    // limit は効く。
    let limited = list_recent_session_logs_of_type(&conn, "s1", "speech", 2).unwrap();
    assert_eq!(limited.len(), 2);
    assert_eq!(limited[0].content, "speech 2");

    // 該当が無ければ空。
    assert!(
        list_recent_session_logs_of_type(&conn, "s1", "inner_voice", 10)
            .unwrap()
            .is_empty()
    );
}
