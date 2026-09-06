use super::*;

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
