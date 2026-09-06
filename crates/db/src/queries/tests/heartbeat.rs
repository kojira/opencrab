use super::*;

// 16. test_heartbeat_log_insert
#[test]
fn test_heartbeat_log_insert() {
    let conn = setup();

    let result = insert_heartbeat_log(&conn, "agent-1", "idle", Some(r#"{"action":"none"}"#));
    assert!(result.is_ok());
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
/// チャンネル指示があればエージェント指示に**上書き**する（連結しない・#583）。
#[test]
fn test_resolve_priority() {
    let conn = setup();
    upsert_agent(&conn, &hb_agent("a1", "AGENT")).unwrap();
    upsert_channel_config(&conn, &hb_channel("ch1", "", "GLOBAL_CH")).unwrap();
    upsert_channel_config(&conn, &hb_channel("ch1", "a1", "AGENT_CH")).unwrap();

    // channel(agent) wins and overrides the agent global entirely.
    let r = resolve_heartbeat_instructions(&conn, "a1", "ch1");
    assert_eq!(r.source, "channel");
    assert_eq!(r.text, "AGENT_CH");

    // remove channel(agent) override → falls back to channel(global), still overriding agent.
    delete_channel_config_for_agent(&conn, "ch1", "a1").unwrap();
    let r = resolve_heartbeat_instructions(&conn, "a1", "ch1");
    assert_eq!(r.source, "channel");
    assert_eq!(r.text, "GLOBAL_CH");

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
