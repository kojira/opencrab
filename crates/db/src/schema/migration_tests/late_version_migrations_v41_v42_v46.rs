use super::super::*;
/// v41（#660）: provider rename `openai` → `hermit`。
///
/// **本番形フィクスチャ**で「片方だけ改名」「pricing 未同期」の 2 罠を固定する。
/// CI は毎回新規 DB（v41 適用済みで openai 行が無い）なので、既存 DB（v40）を模して
/// openai 行を実際に置いてからマイグレーションを走らせる。
#[test]
fn provider_rename_openai_to_hermit_migration_v41() {
    let conn = crate::init_memory().expect("init");

    // v40 相当の既存 DB を模す: version を 40 へ戻し、openai を指す行と、
    // **変わってはいけない** 対照行（別 provider / 先頭アンカーに引っかからない
    // 部分一致 / NULL）を置く。
    conn.execute_batch("PRAGMA user_version = 40;").unwrap();
    conn.execute_batch(
        "INSERT INTO agents (agent_id, name, persona_name, model) VALUES \
            ('a-openai',     'n', 'p', 'openai:claude-sonnet-4-6'), \
            ('a-openrouter', 'n', 'p', 'openrouter:openai/gpt-4o'), \
            ('a-codex',      'n', 'p', 'codex:gpt-5.6'), \
            ('a-null',       'n', 'p', NULL); \
         INSERT INTO model_pricing \
            (provider, model, input_price_per_1m, output_price_per_1m, context_window, updated_at) VALUES \
            ('openai',     'claude-sonnet-4-6', 3.0, 15.0, 200000, '2026-01-01'), \
            ('chatgpt',    'gpt-5.6',           5.0, 30.0, 305000, '2026-01-01'), \
            ('openrouter', 'openai/gpt-4o',     2.5, 10.0, 128000, '2026-01-01'); \
         INSERT INTO model_experience_notes \
            (id, agent_id, provider, model, situation, observation, created_at) VALUES \
            ('e-openai',    'a1', 'openai',    'claude-sonnet-4-6', 's', 'o', '2026-01-01'), \
            ('e-anthropic', 'a1', 'anthropic', 'claude-sonnet-4-6', 's', 'o', '2026-01-01'); \
         INSERT INTO llm_provider_overrides (provider, enabled, updated_at) VALUES \
            ('openai', 1, '2026-01-01'), \
            ('ollama', 1, '2026-01-01');",
    )
    .unwrap();

    // 起動経路（run_migrations）で v41 が届く。
    run_migrations(&conn, MIGRATIONS).expect("v41 migration");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    let model_of = |id: &str| -> Option<String> {
        conn.query_row("SELECT model FROM agents WHERE agent_id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
    };
    // 罠1（片方だけ改名）の一方: agents.model の先頭 openai: が hermit: へ。
    assert_eq!(
        model_of("a-openai").as_deref(),
        Some("hermit:claude-sonnet-4-6")
    );
    // 先頭アンカー: openrouter:openai/... は巻き込まれない（部分一致で壊さない）。
    assert_eq!(
        model_of("a-openrouter").as_deref(),
        Some("openrouter:openai/gpt-4o")
    );
    // 別 provider・NULL は不変。
    assert_eq!(model_of("a-codex").as_deref(), Some("codex:gpt-5.6"));
    assert_eq!(model_of("a-null"), None);

    // 罠2（pricing 未同期）: model_pricing の provider も同一マイグレーションで hermit へ。
    assert!(
        crate::queries::get_model_pricing(&conn, "openai", "claude-sonnet-4-6")
            .unwrap()
            .is_none(),
        "openai の pricing 行は残ってはならない"
    );
    let hermit_price = crate::queries::get_model_pricing(&conn, "hermit", "claude-sonnet-4-6")
        .unwrap()
        .expect("hermit の pricing 行が無い（context_window ゲートが未登録エラーを出す）");
    // context_window ゲートが読む値が引けること（未登録扱いにならない）。
    assert_eq!(hermit_price.context_window, Some(200000));
    // provider が「ちょうど openai」の行だけ対象。substring（openrouter）や別名（chatgpt）は不変。
    assert!(
        crate::queries::get_model_pricing(&conn, "chatgpt", "gpt-5.6")
            .unwrap()
            .is_some()
    );
    assert!(
        crate::queries::get_model_pricing(&conn, "openrouter", "openai/gpt-4o")
            .unwrap()
            .is_some(),
        "provider='openrouter' は provider='openai' の部分一致で壊れてはならない"
    );

    // model_experience_notes: openai→hermit、別 provider は不変。
    let exp_provider = |id: &str| -> Option<String> {
        conn.query_row(
            "SELECT provider FROM model_experience_notes WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(exp_provider("e-openai").as_deref(), Some("hermit"));
    assert_eq!(exp_provider("e-anthropic").as_deref(), Some("anthropic"));

    // llm_provider_overrides: openai→hermit、別 provider は不変。
    let override_exists = |provider: &str| -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM llm_provider_overrides WHERE provider = ?1",
            [provider],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    };
    assert!(
        override_exists("hermit"),
        "override が hermit へ移っていない"
    );
    assert!(
        !override_exists("openai"),
        "override に openai が残っている"
    );
    assert!(override_exists("ollama"), "無関係な override が消えた");

    // 冪等性: 再実行しても openai 行はもう無く、hermit の値は変わらない（自然 no-op）。
    conn.execute_batch("PRAGMA user_version = 40;").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v41 rerun no-op");
    assert_eq!(
        model_of("a-openai").as_deref(),
        Some("hermit:claude-sonnet-4-6")
    );
    assert!(
        crate::queries::get_model_pricing(&conn, "hermit", "claude-sonnet-4-6")
            .unwrap()
            .is_some()
    );
}

/// v42（#676）: model_pricing に max_output_tokens 列を足し、claude-opus-5 だけ 128000 に
/// バックフィルする。既存の context_window / 単価は壊さない。値を持つ行と gpt-5.6 系（NULL
/// のまま）が同居することを見る。冪等でもある。
#[test]
fn model_pricing_max_output_tokens_backfill_migration_v42() {
    let conn = crate::init_memory().expect("init");
    // v41 相当の既存 DB を模す: 列を落として version を 41 へ戻し、既存行を入れる。
    // （新規 init は既に v42 まで済み・列ありなので、明示的に前状態を作る。）
    conn.execute_batch("ALTER TABLE model_pricing DROP COLUMN max_output_tokens;")
        .unwrap();
    conn.execute_batch("PRAGMA user_version = 41;").unwrap();
    conn.execute_batch(
        "INSERT INTO model_pricing \
            (provider, model, input_price_per_1m, output_price_per_1m, context_window, updated_at) VALUES \
            ('hermit',  'claude-opus-5', 5.0, 25.0, 200000, '2026-01-01'), \
            ('chatgpt', 'gpt-5.6-sol',   5.0, 30.0, 350000, '2026-01-01');",
    )
    .unwrap();

    // 起動経路で v42 が届く。
    run_migrations(&conn, MIGRATIONS).expect("v42 migration");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 列が足され、claude-opus-5 だけ 128000 にバックフィルされる。
    assert!(column_exists(&conn, "model_pricing", "max_output_tokens").unwrap());
    let opus = crate::queries::get_model_pricing(&conn, "hermit", "claude-opus-5")
        .unwrap()
        .expect("claude-opus-5 の行");
    assert_eq!(opus.max_output_tokens, Some(128000));
    // 既存の context_window / 単価は不変。
    assert_eq!(opus.context_window, Some(200000));

    // gpt-5.6 系は公式値未確定のため触らない（NULL のまま）。
    let sol = crate::queries::get_model_pricing(&conn, "chatgpt", "gpt-5.6-sol")
        .unwrap()
        .expect("gpt-5.6-sol の行");
    assert_eq!(sol.max_output_tokens, None);

    // 冪等性: 再実行しても値は変わらない（ALTER は列存在で no-op、backfill は IS NULL で自然 no-op）。
    conn.execute_batch("PRAGMA user_version = 41;").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v42 rerun no-op");
    assert_eq!(
        crate::queries::get_model_pricing(&conn, "hermit", "claude-opus-5")
            .unwrap()
            .unwrap()
            .max_output_tokens,
        Some(128000)
    );
}

/// v46（#826-B）: 会話圧縮の派生スナップショット表。正本は触らず、既存 DB へ表だけ足す。
/// 元は #826 で v43 だったが、載せ替え v43-45 との番号衝突を避け統合時に v46 へ採番。
/// 既存 transplant DB（user_version=45）が次回起動で v46 だけを適用する経路をそのまま再現する。
#[test]
fn conversation_snapshots_migration_v46() {
    let conn = crate::init_memory().expect("init");
    conn.execute_batch("DROP TABLE IF EXISTS conversation_snapshots; PRAGMA user_version = 45;")
        .unwrap();
    assert!(!table_exists(&conn, "conversation_snapshots").unwrap());

    run_migrations(&conn, MIGRATIONS).expect("v46 migration");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(table_exists(&conn, "conversation_snapshots").unwrap());

    conn.execute_batch("PRAGMA user_version = 45;").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v46 rerun no-op");
    assert!(table_exists(&conn, "conversation_snapshots").unwrap());
}
