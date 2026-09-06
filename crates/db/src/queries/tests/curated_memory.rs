use super::*;

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

/// #428: 取り込みは long_term を `long_term/<見出し>` の 1 見出し 1 行で入れるので、
/// 完全一致 `get_curated_memories(.., "long_term")` は 0 件になる（＝注入されない事故の再現）。
/// 前方一致 `get_curated_memories_by_prefix` は素の `long_term` と `long_term/<見出し>` の
/// 両方を拾い、別 prefix（daily_log 等）は拾わない。
#[test]
fn curated_by_prefix_picks_up_suffixed_headings_but_not_other_prefixes() {
    let conn = setup();
    let mk = |id: &str, category: &str, content: &str| CuratedMemoryRow {
        id: id.to_string(),
        agent_id: "agent-1".to_string(),
        category: category.to_string(),
        content: content.to_string(),
        created_at: String::new(),
    };
    // 本番と同じ形: long_term は接尾辞付きだけ。素の long_term 行は無い。
    upsert_curated_memory(&conn, &mk("c1", "long_term/Nostr", "nostr facts")).unwrap();
    upsert_curated_memory(&conn, &mk("c2", "long_term/A100", "gpu facts")).unwrap();
    // 別 prefix・別カテゴリ・別エージェントは混ざってはいけない。
    upsert_curated_memory(&conn, &mk("c3", "daily_log/2026-08-11", "diary")).unwrap();
    upsert_curated_memory(&conn, &mk("c4", "user_profile", "profile")).unwrap();
    upsert_curated_memory(
        &conn,
        &CuratedMemoryRow {
            agent_id: "agent-2".to_string(),
            ..mk("c5", "long_term/Other", "other agent")
        },
    )
    .unwrap();

    // 事故の再現: 完全一致では 1 件も引けない。
    assert!(get_curated_memories(&conn, "agent-1", "long_term")
        .unwrap()
        .is_empty());

    // 前方一致は接尾辞付き 2 件を category 昇順で返し、daily_log や他エージェントは含まない。
    let got = get_curated_memories_by_prefix(&conn, "agent-1", "long_term").unwrap();
    let cats: Vec<&str> = got.iter().map(|m| m.category.as_str()).collect();
    assert_eq!(cats, vec!["long_term/A100", "long_term/Nostr"]);

    // 素の完全一致行が在れば、それも `prefix/…` と一緒に（昇順で先頭に）返る。
    upsert_curated_memory(&conn, &mk("c6", "long_term", "bare")).unwrap();
    let with_bare = get_curated_memories_by_prefix(&conn, "agent-1", "long_term").unwrap();
    let cats: Vec<&str> = with_bare.iter().map(|m| m.category.as_str()).collect();
    assert_eq!(cats, vec!["long_term", "long_term/A100", "long_term/Nostr"]);

    // user_profile（単一の完全一致）は前方一致でも従来どおり 1 件。
    let up = get_curated_memories_by_prefix(&conn, "agent-1", "user_profile").unwrap();
    assert_eq!(up.len(), 1);
    assert_eq!(up[0].content, "profile");
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
