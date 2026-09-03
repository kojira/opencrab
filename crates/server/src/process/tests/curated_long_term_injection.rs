use super::build_agent_context;
use opencrab_actions::CallerIdentity;
use opencrab_db::queries::CuratedMemoryRow;

fn curate(conn: &rusqlite::Connection, category: &str, content: &str) {
    opencrab_db::queries::upsert_curated_memory(
        conn,
        &CuratedMemoryRow {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: "a1".to_string(),
            category: category.to_string(),
            content: content.to_string(),
            created_at: String::new(),
        },
    )
    .unwrap();
}

#[test]
fn long_term_suffixed_headings_are_injected_and_bundled_by_heading() {
    let conn = opencrab_db::init_memory().unwrap();
    // 本番と同じ形: long_term は接尾辞付きだけ（素の `long_term` 行は無い）。
    curate(&conn, "long_term/A100サーバー", "- GPU は 8 枚");
    curate(&conn, "long_term/Nostr", "- リレーは wss://…");
    // user_profile は単一の完全一致（従来から生きている経路）。
    curate(&conn, "user_profile", "owner さんは…");
    // daily_log/* は注入対象外。前方一致が別 prefix を巻き込まないことの確認。
    curate(&conn, "daily_log/2026-08-11", "きょうの日記の本文");

    let (prompt, _) = build_agent_context(&conn, "a1", &CallerIdentity::Owner);

    // long_term セクションが出て、見出しごとに束ねられ、本文も載る。
    assert!(
        prompt.contains("## Long-term Memory"),
        "Long-term Memory セクションが出ていない:\n{prompt}"
    );
    assert!(
        prompt.contains("### A100サーバー")
            && prompt.contains("- GPU は 8 枚")
            && prompt.contains("### Nostr")
            && prompt.contains("- リレーは wss://…"),
        "long_term/<見出し> が見出し付きで注入されていない:\n{prompt}"
    );
    // user_profile は従来どおり見出し無しで注入（回帰していない）。
    assert!(
        prompt.contains("## User Profile") && prompt.contains("owner さんは…"),
        "user_profile の注入が壊れている:\n{prompt}"
    );
    // daily_log は注入されない（前方一致が別カテゴリを巻き込まない）。
    assert!(
        !prompt.contains("きょうの日記の本文"),
        "daily_log が誤って注入されている:\n{prompt}"
    );
}

#[test]
fn no_long_term_data_means_no_section() {
    let conn = opencrab_db::init_memory().unwrap();
    curate(&conn, "user_profile", "profile only");

    let (prompt, _) = build_agent_context(&conn, "a1", &CallerIdentity::Owner);
    // データが無ければ空の見出しは出さない（agent_rules も同様に本番は 0 行）。
    assert!(
        !prompt.contains("## Long-term Memory"),
        "空の long_term 見出しが出ている:\n{prompt}"
    );
    assert!(
        !prompt.contains("## Agent Rules"),
        "空の agent_rules 見出しが出ている:\n{prompt}"
    );
    assert!(prompt.contains("## User Profile"));
}
