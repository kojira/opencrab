use rusqlite::Connection;

/// スキーマのバージョン管理は `PRAGMA user_version` で行う。
///
/// 既存の冪等 `migrate()`（version 1 baseline）を凍結し、以降のスキーマ変更は
/// [`MIGRATIONS`] に番号付きで追加する。既存DBは全て `user_version = 0` なので、
/// 初回起動では baseline（`SCHEMA_SQL` + `migrate()`）を従来どおり適用してから
/// version 1 をスタンプする。以降の起動では baseline をスキップし、番号付き
/// マイグレーションのうち未適用のものだけを実行する。
const BASELINE_VERSION: i64 = 1;

/// 番号付きマイグレーション1件。
struct Migration {
    version: i64,
    #[allow(dead_code)]
    description: &'static str,
    up: fn(&Connection) -> rusqlite::Result<()>,
}

/// version 2 以降のスキーマ変更をここに追記する（version は厳密増加）。
///
/// 重要な運用ルール:
/// - `migrate()`（version 1 baseline）へは今後**追記しない**。新しい変更はここへ。
/// - `SCHEMA_SQL`（新規インストール用）にテーブル/列を足したら、既存DB
///   （baseline済み＝`SCHEMA_SQL` を再実行しない）にも届くよう、**必ず対応する
///   番号付きマイグレーションもここへ追加する**こと。忘れると既存DBだけ列が欠ける。
/// - 各 `up` は自身のトランザクション内で実行される（`run_migrations` 参照）。
///   `journal_mode`/`VACUUM` 等の非トランザクショナルな操作は `up` 内で行わない。
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 2,
        description: "task ledger: goal/contract/progress (issue #50)",
        up: |conn| conn.execute_batch(TASK_LEDGER_SQL),
    },
    Migration {
        version: 3,
        description: "trusted_discord_users.display_name (peer reviewer roster, issue #57)",
        // 新規DB は SCHEMA_SQL 側で列を持つため、column_exists でガードして冪等にする
        up: |conn| {
            if !column_exists(conn, "trusted_discord_users", "display_name")? {
                conn.execute_batch(
                    "ALTER TABLE trusted_discord_users ADD COLUMN display_name TEXT NOT NULL DEFAULT ''",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 4,
        description: "agent_sessions backfill from sessions.participant_ids_json (issue #37)",
        // participant の関係を agent_sessions テーブルに昇格する（#37）。
        // 既存 sessions の JSON 配列から backfill。壊れた JSON / 非文字列要素は
        // 行単位で skip（sessions 側の表示は participant_ids_json を読み続けるため
        // 情報は失われない）。INSERT OR IGNORE で再実行にも冪等。
        up: |conn| {
            let mut stmt = conn.prepare("SELECT id, participant_ids_json FROM sessions")?;
            let rows: Vec<(String, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            for (session_id, participants_json) in rows {
                let Ok(serde_json::Value::Array(ids)) =
                    serde_json::from_str::<serde_json::Value>(&participants_json)
                else {
                    // 壊れた JSON は skip（マイグレーション全体は落とさない）
                    continue;
                };
                for id in ids {
                    if let Some(agent_id) = id.as_str() {
                        conn.execute(
                            "INSERT OR IGNORE INTO agent_sessions (agent_id, session_id) VALUES (?1, ?2)",
                            rusqlite::params![agent_id, session_id],
                        )?;
                    }
                }
            }
            Ok(())
        },
    },
    Migration {
        version: 5,
        description: "memory_index_nodes: FK(parent_id, CASCADE) + CHECK(node_type) (issue #41)",
        // テーブル再構築（SQLite は既存テーブルへの FK/CHECK 追加不可）。
        // メモリインデックスは session_logs から再構築可能な派生データなので、
        // 整合しない行（不正 node_type）はコピー対象から外し、孤児 parent_id は
        // NULL に落とす（次回 rebuild で正しいツリーに戻る）。
        // FK 検査はトランザクション内で切り替え可能な defer_foreign_keys で
        // commit 時まで遅延させる（コピー順序に依存しない）。
        up: |conn| {
            // 冪等ガード: 新規DBは SCHEMA_SQL 側で FK/CHECK を持つ。
            let has_fk: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_list('memory_index_nodes')",
                [],
                |r| r.get(0),
            )?;
            if has_fk > 0 {
                return Ok(());
            }
            conn.execute_batch("PRAGMA defer_foreign_keys = ON")?;
            conn.execute_batch(
                "CREATE TABLE memory_index_nodes_new (
                    id TEXT PRIMARY KEY,
                    agent_id TEXT NOT NULL,
                    parent_id TEXT REFERENCES memory_index_nodes_new(id) ON DELETE CASCADE,
                    node_type TEXT NOT NULL CHECK (node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly')),
                    source_type TEXT NOT NULL DEFAULT 'session_log',
                    title TEXT NOT NULL,
                    summary TEXT NOT NULL,
                    start_log_id INTEGER,
                    end_log_id INTEGER,
                    source_session_id TEXT,
                    date_from TEXT,
                    date_to TEXT,
                    depth INTEGER NOT NULL DEFAULT 0,
                    child_count INTEGER NOT NULL DEFAULT 0,
                    token_count INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    short_id TEXT
                );
                INSERT INTO memory_index_nodes_new
                    (id, agent_id, parent_id, node_type, source_type, title, summary,
                     start_log_id, end_log_id, source_session_id, date_from, date_to,
                     depth, child_count, token_count, created_at, updated_at, short_id)
                    SELECT id, agent_id, parent_id, node_type, source_type, title, summary,
                           start_log_id, end_log_id, source_session_id, date_from, date_to,
                           depth, child_count, token_count, created_at, updated_at, short_id
                    FROM memory_index_nodes
                    WHERE node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly');
                UPDATE memory_index_nodes_new SET parent_id = NULL
                    WHERE parent_id IS NOT NULL
                      AND parent_id NOT IN (SELECT id FROM memory_index_nodes_new);
                DROP TABLE memory_index_nodes;
                ALTER TABLE memory_index_nodes_new RENAME TO memory_index_nodes;
                CREATE INDEX IF NOT EXISTS idx_mem_idx_agent ON memory_index_nodes(agent_id);
                CREATE INDEX IF NOT EXISTS idx_mem_idx_parent ON memory_index_nodes(agent_id, parent_id);
                CREATE INDEX IF NOT EXISTS idx_mem_idx_type ON memory_index_nodes(agent_id, node_type);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_index_nodes_short_id ON memory_index_nodes(agent_id, short_id) WHERE short_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_memory_index_nodes_source_type ON memory_index_nodes(agent_id, source_type);",
            )?;
            Ok(())
        },
    },
    Migration {
        version: 6,
        description: "task_ledger.restart_count (loop restart v1, issue #52)",
        // 新規/既存DBとも ALTER で追加する（SCHEMA_SQL 側の task_ledger ブロックは
        // TASK_LEDGER_SQL との文面パリティ制約があるため変更しない）。
        up: |conn| {
            if !column_exists(conn, "task_ledger", "restart_count")? {
                conn.execute_batch(
                    "ALTER TABLE task_ledger ADD COLUMN restart_count INTEGER NOT NULL DEFAULT 0",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 7,
        description: "memory index: keywords + rollup timestamp + node FTS (reverse lookup)",
        // キーワード逆引きと月次ロールアップの土台。新規DBも SCHEMA_SQL は触らず
        // ここで ALTER する（v6 前例）。FTS バックフィルは影テーブルが空のときだけ
        // 実行するので冪等。
        up: |conn| {
            if !column_exists(conn, "memory_index_nodes", "keywords_json")? {
                conn.execute_batch(
                    "ALTER TABLE memory_index_nodes ADD COLUMN keywords_json TEXT NOT NULL DEFAULT '[]'",
                )?;
            }
            if !column_exists(conn, "memory_index_nodes", "summary_refreshed_at")? {
                conn.execute_batch(
                    "ALTER TABLE memory_index_nodes ADD COLUMN summary_refreshed_at TEXT",
                )?;
            }
            // tokenize=trigram: 日本語は空白で区切られないため、既定の unicode61 だと
            // 文全体が 1 トークンになり部分語で当たらない。trigram は 3 文字以上の
            // 部分文字列マッチを可能にする（2 文字以下はクエリ層の LIKE フォールバック）。
            conn.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS memory_index_fts USING fts5(
                    title, summary, keywords,
                    node_id UNINDEXED, agent_id UNINDEXED, node_type UNINDEXED, source_type UNINDEXED,
                    tokenize='trigram')",
            )?;
            let fts_rows: i64 =
                conn.query_row("SELECT COUNT(*) FROM memory_index_fts", [], |r| r.get(0))?;
            if fts_rows == 0 {
                conn.execute_batch(
                    "INSERT INTO memory_index_fts (title, summary, keywords, node_id, agent_id, node_type, source_type)
                     SELECT title, summary, '', id, agent_id, node_type, source_type FROM memory_index_nodes",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 8,
        description: "provider settings overrides (dashboard-managed LLM/voice providers)",
        up: |conn| conn.execute_batch(PROVIDER_SETTINGS_SQL),
    },
    Migration {
        version: 9,
        description: "llm_provider_overrides.reasoning_effort (dashboard-editable thinking level)",
        up: |conn| {
            if !column_exists(conn, "llm_provider_overrides", "reasoning_effort")? {
                conn.execute_batch(
                    "ALTER TABLE llm_provider_overrides ADD COLUMN reasoning_effort TEXT",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 10,
        description: "agents.reasoning_effort (per-agent thinking level)",
        up: |conn| {
            if !column_exists(conn, "agents", "reasoning_effort")? {
                conn.execute_batch("ALTER TABLE agents ADD COLUMN reasoning_effort TEXT")?;
            }
            Ok(())
        },
    },
    Migration {
        version: 11,
        description: "sleep skill consolidation: skill_usage_log + agent_memory_index_config.last_skill_consolidation_at",
        up: |conn| {
            conn.execute_batch(SKILL_USAGE_LOG_SQL)?;
            // 棚卸しの最終実行時刻。SQLite の ADD COLUMN DEFAULT は定数のみで
            // datetime('now') を使えないため NULL 許容で追加し、初回シード/実行後に
            // 明示 UPSERT で now を刻む（design-sleep-skill-consolidation.md §5/§8.3）。
            if !column_exists(conn, "agent_memory_index_config", "last_skill_consolidation_at")? {
                conn.execute_batch(
                    "ALTER TABLE agent_memory_index_config ADD COLUMN last_skill_consolidation_at TEXT",
                )?;
            }
            Ok(())
        },
    },
];

/// スキル利用のセッション単位記録（スリープ棚卸しの弱い利用ヒント用）。
/// 注入時ではなく「利用が検出された時」に記録する（名前一致ベース, ノイズあり）。
const SKILL_USAGE_LOG_SQL: &str = "
CREATE TABLE IF NOT EXISTS skill_usage_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_skill_usage_log_skill ON skill_usage_log(skill_id);
CREATE INDEX IF NOT EXISTS idx_skill_usage_log_session ON skill_usage_log(session_id);
";

/// ダッシュボードから編集する LLM/voice プロバイダー設定のオーバーライド。
/// TOML を土台に、行/フィールドが存在するものだけ上書きする。
const PROVIDER_SETTINGS_SQL: &str = "
CREATE TABLE IF NOT EXISTS llm_provider_overrides (
    provider TEXT PRIMARY KEY,
    enabled INTEGER,
    api_key TEXT,
    base_url TEXT,
    default_model TEXT,
    reasoning_effort TEXT,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS voice_config_override (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    config_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
";

/// version 2: タスク台帳。
///
/// `SCHEMA_SQL` 末尾の同名ブロックと**文面を完全一致**させること
/// （`task_ledger_schema_parity` テストが sqlite_master の SQL 文字列で比較する）。
const TASK_LEDGER_SQL: &str = r#"
-- ============================================
-- TASK LEDGER: 前向きワーキング状態（goal/契約/進捗/決定）
-- ============================================
CREATE TABLE IF NOT EXISTS task_ledger (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    goal TEXT NOT NULL,
    contract TEXT,
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'done', 'abandoned')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_ledger_session
    ON task_ledger(agent_id, session_id, status);
CREATE UNIQUE INDEX IF NOT EXISTS idx_task_ledger_one_active
    ON task_ledger(agent_id, session_id) WHERE status = 'active';

CREATE TABLE IF NOT EXISTS task_progress (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES task_ledger(id) ON DELETE CASCADE,
    kind TEXT NOT NULL DEFAULT 'progress'
        CHECK (kind IN ('progress', 'decision', 'blocker')),
    content TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_progress_task ON task_progress(task_id);
"#;

/// このバイナリが知る最新スキーマバージョン。
#[cfg(test)]
fn latest_version() -> i64 {
    MIGRATIONS
        .last()
        .map(|m| m.version)
        .unwrap_or(BASELINE_VERSION)
}

/// 現在のスキーマバージョン（`PRAGMA user_version`）を読み取る。
fn schema_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

/// スキーマ初期化。
///
/// - `user_version < BASELINE_VERSION`（新規DB / バージョン管理導入前の既存DB）の場合、
///   `SCHEMA_SQL` + 凍結された `migrate()` を**1トランザクション**で適用し、version 1 を
///   スタンプする。破壊的なテーブルリビルドや DROP を含むため一括ロールバック可能にする
///   （途中失敗すれば version は 0 のままで、次回起動でクリーンに再試行される）。
/// - その後、`MIGRATIONS` のうち未適用の番号付きマイグレーションを**各自のトランザクション**で
///   適用する（途中失敗時は直前まで確定・再開可能）。
pub fn initialize(conn: &Connection) -> rusqlite::Result<()> {
    let current = schema_version(conn)?;
    if current < BASELINE_VERSION {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(SCHEMA_SQL)?;
        migrate(&tx)?;
        tx.execute_batch(&format!("PRAGMA user_version = {BASELINE_VERSION}"))?;
        tx.commit()?;
    }
    run_migrations(conn, MIGRATIONS)?;
    Ok(())
}

/// 番号付きマイグレーションを順に適用する。
///
/// `current` より大きい version の各マイグレーションを、それぞれ独自の
/// トランザクション内で実行し、成功後に同一トランザクション内で `user_version` を
/// スタンプする。`current` が既知の最新版より新しい（＝より新しいバイナリで作られたDBを
/// 古いバイナリで開いた）場合は、破壊的な誤動作を避けるため明示的にエラーにする。
fn run_migrations(conn: &Connection, migrations: &[Migration]) -> rusqlite::Result<()> {
    let current = schema_version(conn)?;
    let latest = migrations
        .last()
        .map(|m| m.version)
        .unwrap_or(BASELINE_VERSION);
    if current > latest {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some(format!(
                "database schema version {current} is newer than this binary supports ({latest}); please upgrade the application"
            )),
        ));
    }
    for m in migrations {
        if m.version > current {
            let tx = conn.unchecked_transaction()?;
            (m.up)(&tx)?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", m.version))?;
            tx.commit()?;
        }
    }
    Ok(())
}

/// FROZEN — schema version 1 baseline。
///
/// この関数へは**今後追記しないこと**。スキーマ変更は version 2 以降の番号付き
/// [`MIGRATIONS`] エントリとして追加する。ここは version 1 として確定した履歴であり、
/// `backfill_short_ids` 呼び出しや `migrate_soul_identity_to_agents` 含めて凍結する。
///
/// 既存テーブルへのマイグレーション（カラム追加など）。
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    // sessions.metadata_json カラム追加（既存DBへの対応）
    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name='metadata_json'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE sessions ADD COLUMN metadata_json TEXT")?;
    }

    // skills.permission カラム追加（既存DBへの対応）
    let has_permission_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('skills') WHERE name='permission'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_permission_col {
        conn.execute_batch(
            "ALTER TABLE skills ADD COLUMN permission TEXT NOT NULL DEFAULT '\"agent\"'",
        )?;
    }

    // skills.archived カラム追加（スキルアーカイブ機能）
    let has_archived_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('skills') WHERE name='archived'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_archived_col {
        conn.execute_batch("ALTER TABLE skills ADD COLUMN archived INTEGER NOT NULL DEFAULT 0")?;
    }

    // discord_channel_config.whitelisted カラム追加
    let has_whitelisted_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('discord_channel_config') WHERE name='whitelisted'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_whitelisted_col {
        conn.execute_batch(
            "ALTER TABLE discord_channel_config ADD COLUMN whitelisted INTEGER NOT NULL DEFAULT 0",
        )?;
    }

    // discord_channel_config.heartbeat_enabled カラム追加
    let has_heartbeat_enabled_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('discord_channel_config') WHERE name='heartbeat_enabled'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_heartbeat_enabled_col {
        conn.execute_batch("ALTER TABLE discord_channel_config ADD COLUMN heartbeat_enabled INTEGER NOT NULL DEFAULT 1")?;
    }

    // discord_channel_config.heartbeat_interval_secs カラム追加
    let has_hb_interval: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('discord_channel_config') WHERE name='heartbeat_interval_secs'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_hb_interval {
        conn.execute_batch(
            "ALTER TABLE discord_channel_config ADD COLUMN heartbeat_interval_secs INTEGER",
        )?;
    }

    // discord_channel_config: agent_idカラム追加 + PKを(channel_id, agent_id)に変更
    let has_agent_id_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('discord_channel_config') WHERE name='agent_id'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_agent_id_col {
        // テーブル再作成でPKを(channel_id, agent_id)に変更
        conn.execute_batch("
            CREATE TABLE IF NOT EXISTS discord_channel_config_new (
                channel_id TEXT NOT NULL,
                agent_id TEXT NOT NULL DEFAULT '',
                guild_id TEXT NOT NULL,
                channel_name TEXT NOT NULL DEFAULT '',
                readable INTEGER NOT NULL DEFAULT 1,
                writable INTEGER NOT NULL DEFAULT 1,
                whitelisted INTEGER NOT NULL DEFAULT 0,
                heartbeat_enabled INTEGER NOT NULL DEFAULT 1,
                heartbeat_interval_secs INTEGER,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (channel_id, agent_id)
            );
            INSERT INTO discord_channel_config_new
                (channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, updated_at)
            SELECT channel_id, '', guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, updated_at
            FROM discord_channel_config;
            DROP TABLE discord_channel_config;
            ALTER TABLE discord_channel_config_new RENAME TO discord_channel_config;
            CREATE INDEX IF NOT EXISTS idx_discord_channel_guild ON discord_channel_config(guild_id);
            CREATE INDEX IF NOT EXISTS idx_discord_channel_agent ON discord_channel_config(agent_id);
        ")?;
    }
    // agent_idカラムが存在する場合もインデックスを保証する（新規DB・マイグレーション済みDB共通）
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_discord_channel_agent ON discord_channel_config(agent_id)",
    )?;

    // agents.heartbeat_instructions カラム追加（ハートビート専用指示）
    let has_agent_hb_instr: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('agents') WHERE name='heartbeat_instructions'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_agent_hb_instr {
        conn.execute_batch(
            "ALTER TABLE agents ADD COLUMN heartbeat_instructions TEXT NOT NULL DEFAULT ''",
        )?;
    }

    // discord_channel_config.heartbeat_instructions カラム追加（チャンネル単位の上書き）
    let has_channel_hb_instr: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('discord_channel_config') WHERE name='heartbeat_instructions'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_channel_hb_instr {
        conn.execute_batch(
            "ALTER TABLE discord_channel_config ADD COLUMN heartbeat_instructions TEXT NOT NULL DEFAULT ''",
        )?;
    }

    // heartbeat_instructions_audit テーブル作成（指示改変の監査ログ）
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS heartbeat_instructions_audit (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id TEXT NOT NULL,
            scope TEXT NOT NULL,
            channel_id TEXT,
            caller_identity TEXT NOT NULL,
            caller_discord_id TEXT,
            old_value TEXT,
            new_value TEXT,
            reason TEXT,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_heartbeat_instr_audit_agent
            ON heartbeat_instructions_audit(agent_id, created_at DESC);",
    )?;

    // agent_memory_index_config テーブル作成（既存DBへの対応）
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_memory_index_config (
            agent_id TEXT PRIMARY KEY,
            batch_size INTEGER NOT NULL DEFAULT 50,
            threshold INTEGER NOT NULL DEFAULT 20,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )?;

    // 旧 soul テーブル向けマイグレーション（新規DBでは soul 不存在のためスキップ）
    if table_exists(conn, "soul")? {
        // soul.social_style_json カラムDROP（dead code削除）
        let has_social_style: bool = conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('soul') WHERE name='social_style_json'",
            )?
            .query_row([], |row| row.get::<_, i64>(0))
            .map(|c| c > 0)
            .unwrap_or(false);
        if has_social_style {
            conn.execute_batch("ALTER TABLE soul DROP COLUMN social_style_json")?;
        }

        // soul.thinking_style_json カラムDROP（dead code削除）
        let has_thinking_style: bool = conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('soul') WHERE name='thinking_style_json'",
            )?
            .query_row([], |row| row.get::<_, i64>(0))
            .map(|c| c > 0)
            .unwrap_or(false);
        if has_thinking_style {
            conn.execute_batch("ALTER TABLE soul DROP COLUMN thinking_style_json")?;
        }
    }

    // llm_logs テーブル作成（既存DBへの対応）
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS llm_logs (
            id TEXT PRIMARY KEY,
            agent_id TEXT NOT NULL,
            session_id TEXT,
            model TEXT,
            prompt TEXT NOT NULL DEFAULT '',
            response TEXT NOT NULL DEFAULT '',
            tool_calls TEXT,
            latency_ms INTEGER,
            prompt_tokens INTEGER,
            completion_tokens INTEGER,
            total_tokens INTEGER,
            error_code TEXT,
            error_body TEXT,
            requested_at TEXT,
            created_at TEXT DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_llm_logs_agent ON llm_logs(agent_id);
        CREATE INDEX IF NOT EXISTS idx_llm_logs_created ON llm_logs(agent_id, created_at DESC);",
    )?;

    // llm_logs 新カラム追加（既存DBへの対応）
    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='latency_ms'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN latency_ms INTEGER")?;
    }

    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='prompt_tokens'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN prompt_tokens INTEGER")?;
    }

    let has_col: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='completion_tokens'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN completion_tokens INTEGER")?;
    }

    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='total_tokens'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN total_tokens INTEGER")?;
    }

    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='error_code'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN error_code TEXT")?;
    }

    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='error_body'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN error_body TEXT")?;
    }

    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='requested_at'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN requested_at TEXT")?;
    }
    // After the requested_at column is added (or confirmed to exist), create the index.
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_llm_logs_requested ON llm_logs(agent_id, requested_at DESC)")?;

    // llm_logs.trigger_message_id カラム追加
    let has_trigger: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='trigger_message_id'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_trigger {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN trigger_message_id TEXT")?;
    }

    // llm_logs.is_bot_iteration カラム追加
    let has_bot_iter: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='is_bot_iteration'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_bot_iter {
        conn.execute_batch(
            "ALTER TABLE llm_logs ADD COLUMN is_bot_iteration INTEGER NOT NULL DEFAULT 0",
        )?;
    }

    // llm_logs.cache_read_tokens カラム追加
    let has_cache_read: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='cache_read_tokens'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_cache_read {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN cache_read_tokens INTEGER")?;
    }

    // llm_logs.cache_creation_tokens カラム追加
    let has_cache_creation: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='cache_creation_tokens'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_cache_creation {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN cache_creation_tokens INTEGER")?;
    }

    let has_source_type_col: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('memory_index_nodes') WHERE name='source_type'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_source_type_col {
        conn.execute_batch(
            "ALTER TABLE memory_index_nodes ADD COLUMN source_type TEXT NOT NULL DEFAULT 'session_log'",
        )?;
    }

    let has_date_from_col: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('memory_index_nodes') WHERE name='date_from'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_date_from_col {
        conn.execute_batch("ALTER TABLE memory_index_nodes ADD COLUMN date_from TEXT")?;
    }

    let has_date_to_col: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('memory_index_nodes') WHERE name='date_to'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_date_to_col {
        conn.execute_batch("ALTER TABLE memory_index_nodes ADD COLUMN date_to TEXT")?;
    }

    let has_short_id_col: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('memory_index_nodes') WHERE name='short_id'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_short_id_col {
        conn.execute_batch("ALTER TABLE memory_index_nodes ADD COLUMN short_id TEXT")?;
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_index_nodes_short_id ON memory_index_nodes(agent_id, short_id) WHERE short_id IS NOT NULL",
        )?;
        crate::queries::backfill_short_ids(conn)
            .map_err(|e| rusqlite::Error::InvalidParameterName(format!("{e}")))?;
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS daily_log_index_watermark (
            agent_id TEXT NOT NULL PRIMARY KEY,
            last_indexed_date TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )",
    )?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_memory_index_nodes_source_type
         ON memory_index_nodes (agent_id, source_type)",
    )?;

    // skills.skill_type カラムDROP（v2: executableタイプ廃止）
    let has_skill_type_drop: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('skills') WHERE name='skill_type'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if has_skill_type_drop {
        conn.execute_batch("ALTER TABLE skills DROP COLUMN skill_type")?;
    }

    // skills.code カラムDROP（v2: executableタイプ廃止）
    let has_code_drop: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('skills') WHERE name='code'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if has_code_drop {
        conn.execute_batch("ALTER TABLE skills DROP COLUMN code")?;
    }

    // memory_curated.created_at カラム追加
    let has_curated_created_at: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('memory_curated') WHERE name='created_at'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_curated_created_at {
        conn.execute_batch(
            "ALTER TABLE memory_curated ADD COLUMN created_at TEXT NOT NULL DEFAULT ''",
        )?;
    }

    if table_exists(conn, "soul")? {
        // soul.instructions カラム追加（操作ルール・AGENTS.md相当）
        let has_instructions: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('soul') WHERE name='instructions'")?
            .query_row([], |row| row.get::<_, i64>(0))
            .map(|c| c > 0)
            .unwrap_or(false);
        if !has_instructions {
            conn.execute_batch(
                "ALTER TABLE soul ADD COLUMN instructions TEXT NOT NULL DEFAULT ''",
            )?;
        }
    }

    // import_sync_state テーブル作成
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS import_sync_state (
            id TEXT PRIMARY KEY,
            agent_id TEXT NOT NULL,
            source_dir TEXT NOT NULL,
            file_type TEXT NOT NULL,
            file_name TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            synced_at TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_import_sync_state_key
            ON import_sync_state(agent_id, source_dir, file_name);
        CREATE INDEX IF NOT EXISTS idx_import_sync_state_agent
            ON import_sync_state(agent_id);",
    )?;

    // memory_curated の (agent_id, category) UNIQUE INDEX 追加
    // 既存の重複レコードをself-joinで削除してからインデックスを作成
    // SQLite特有の制限: サブクエリ内でLIMITが使えないのでself-joinを使う
    conn.execute_batch(
        "DELETE FROM memory_curated
         WHERE id IN (
             SELECT mc1.id FROM memory_curated mc1
             INNER JOIN memory_curated mc2 ON mc1.agent_id = mc2.agent_id
                 AND mc1.category = mc2.category
                 AND mc1.updated_at < mc2.updated_at
         );
         CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_curated_agent_category
             ON memory_curated(agent_id, category);",
    )?;

    // agent_logs テーブル作成（既存DBへの対応）
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_logs (
            id TEXT PRIMARY KEY,
            agent_id TEXT,
            level TEXT NOT NULL,
            context TEXT NOT NULL,
            message TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_agent_logs_agent ON agent_logs(agent_id, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_agent_logs_level ON agent_logs(level, created_at DESC);",
    )?;

    // soul + identity → agents 集約（既存DBのみ。soul テーブルがあればデータ移行して DROP）
    migrate_soul_identity_to_agents(conn)?;

    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// テーブルに指定カラムが存在するか判定する（将来の番号付きマイグレーション用ヘルパー）。
///
/// version 1 baseline (`migrate`) 内の約30箇所のインライン `pragma_table_info` プローブは
/// 凍結のためリファクタしないが、version 2 以降の `Migration::up` ではこのヘルパーを使う。
#[allow(dead_code)]
fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
        [table, column],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 旧 soul / identity を agents に統合し、旧テーブルを削除する。
fn migrate_soul_identity_to_agents(conn: &Connection) -> rusqlite::Result<()> {
    if !table_exists(conn, "soul")? {
        return Ok(());
    }

    // agents は SCHEMA で CREATE IF NOT EXISTS 済み。空または未作成のどちらでもよい。
    conn.execute_batch(
        r#"
        INSERT INTO agents (agent_id, name, job_title, organization, image_url, persona_name, personality, instructions, model, metadata_json, created_at, updated_at)
        SELECT
            s.agent_id,
            i.name,
            i.job_title,
            i.organization,
            i.image_url,
            s.persona_name,
            s.personality,
            s.instructions,
            NULL,
            i.metadata_json,
            datetime('now'),
            CASE WHEN s.updated_at >= i.updated_at THEN s.updated_at ELSE i.updated_at END
        FROM soul s
        INNER JOIN identity i ON s.agent_id = i.agent_id
        WHERE NOT EXISTS (SELECT 1 FROM agents a WHERE a.agent_id = s.agent_id);

        INSERT INTO agents (agent_id, name, job_title, organization, image_url, persona_name, personality, instructions, model, metadata_json, created_at, updated_at)
        SELECT
            s.agent_id,
            s.persona_name,
            NULL,
            NULL,
            NULL,
            s.persona_name,
            s.personality,
            s.instructions,
            NULL,
            NULL,
            datetime('now'),
            s.updated_at
        FROM soul s
        WHERE NOT EXISTS (SELECT 1 FROM identity i WHERE i.agent_id = s.agent_id)
          AND NOT EXISTS (SELECT 1 FROM agents a WHERE a.agent_id = s.agent_id);

        INSERT INTO agents (agent_id, name, job_title, organization, image_url, persona_name, personality, instructions, model, metadata_json, created_at, updated_at)
        SELECT
            i.agent_id,
            i.name,
            i.job_title,
            i.organization,
            i.image_url,
            i.name,
            NULL,
            '',
            NULL,
            i.metadata_json,
            datetime('now'),
            i.updated_at
        FROM identity i
        WHERE NOT EXISTS (SELECT 1 FROM soul s WHERE s.agent_id = i.agent_id)
          AND NOT EXISTS (SELECT 1 FROM agents a WHERE a.agent_id = i.agent_id);
        "#,
    )?;

    conn.execute_batch("DROP TABLE IF EXISTS soul; DROP TABLE IF EXISTS identity;")?;
    Ok(())
}

/// 新規インストール用のスキーマ定義（全て `CREATE ... IF NOT EXISTS`）。
///
/// 注意: baseline 済みの既存DB（`user_version >= 1`）では、この `SCHEMA_SQL` は
/// **再実行されない**。したがってここにテーブル/列を追加しただけでは既存DBには反映されない。
/// 新しいテーブル/列は、必ず対応する番号付きマイグレーションを [`MIGRATIONS`] にも追加して
/// 既存DBへ届けること。
const SCHEMA_SQL: &str = r#"
-- ============================================
-- AGENTS: soul + identity 統合 + エージェント別モデル
-- ============================================
CREATE TABLE IF NOT EXISTS agents (
    agent_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    job_title TEXT,
    organization TEXT,
    image_url TEXT,
    persona_name TEXT NOT NULL,
    personality TEXT,
    instructions TEXT NOT NULL DEFAULT '',
    heartbeat_instructions TEXT NOT NULL DEFAULT '',
    model TEXT,
    reasoning_effort TEXT,
    metadata_json TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- ============================================
-- MEMORY: キュレーション記憶
-- ============================================
CREATE TABLE IF NOT EXISTS memory_curated (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    category TEXT NOT NULL,
    content TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_memory_curated_agent ON memory_curated(agent_id);
CREATE INDEX IF NOT EXISTS idx_memory_curated_category ON memory_curated(agent_id, category);

-- ============================================
-- MEMORY: セッションログ
-- ============================================
CREATE TABLE IF NOT EXISTS memory_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    log_type TEXT NOT NULL,
    content TEXT NOT NULL,
    speaker_id TEXT,
    turn_number INTEGER,
    metadata_json TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_memory_sessions_agent ON memory_sessions(agent_id);
CREATE INDEX IF NOT EXISTS idx_memory_sessions_session ON memory_sessions(agent_id, session_id);

-- ============================================
-- MEMORY: FTS5全文検索
-- ============================================
CREATE VIRTUAL TABLE IF NOT EXISTS memory_sessions_fts USING fts5(
    content,
    agent_id UNINDEXED,
    session_id UNINDEXED,
    log_type UNINDEXED
);

-- ============================================
-- Skills: スキル管理
-- ============================================
CREATE TABLE IF NOT EXISTS skills (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    situation_pattern TEXT NOT NULL,
    guidance TEXT NOT NULL,
    source_type TEXT NOT NULL DEFAULT 'standard',
    source_context TEXT,
    file_path TEXT,
    effectiveness REAL,
    usage_count INTEGER NOT NULL DEFAULT 0,
    is_active INTEGER NOT NULL DEFAULT 1,
    permission TEXT NOT NULL DEFAULT '"agent"',
    archived INTEGER NOT NULL DEFAULT 0,
    last_used_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_skills_agent ON skills(agent_id);
CREATE INDEX IF NOT EXISTS idx_skills_active ON skills(agent_id, is_active);

-- ============================================
-- Impressions: 心象
-- ============================================
CREATE TABLE IF NOT EXISTS impressions (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    target_name TEXT NOT NULL,
    personality TEXT DEFAULT '',
    communication_style TEXT DEFAULT '',
    recent_behavior TEXT DEFAULT '',
    agreement TEXT DEFAULT '中立',
    notes TEXT DEFAULT '',
    last_updated_turn INTEGER DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(agent_id, session_id, target_id)
);
CREATE INDEX IF NOT EXISTS idx_impressions_session ON impressions(agent_id, session_id);

-- ============================================
-- LLM利用メトリクス
-- ============================================
CREATE TABLE IF NOT EXISTS llm_usage_metrics (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    session_id TEXT,
    timestamp TEXT NOT NULL,

    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    purpose TEXT NOT NULL,
    task_type TEXT,
    complexity TEXT,

    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    total_tokens INTEGER NOT NULL,
    estimated_cost_usd REAL NOT NULL,

    latency_ms INTEGER NOT NULL,
    time_to_first_token_ms INTEGER,

    quality_score REAL,
    self_evaluation TEXT,
    task_success INTEGER,
    would_use_again INTEGER,
    better_model_suggestion TEXT,

    tags TEXT,

    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_llm_metrics_agent ON llm_usage_metrics(agent_id);
CREATE INDEX IF NOT EXISTS idx_llm_metrics_model ON llm_usage_metrics(model);
CREATE INDEX IF NOT EXISTS idx_llm_metrics_timestamp ON llm_usage_metrics(timestamp);

-- ============================================
-- モデル経験ノート: エージェントが自由に書く定性的な知見
-- ============================================
CREATE TABLE IF NOT EXISTS model_experience_notes (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    provider TEXT,
    model TEXT,
    situation TEXT NOT NULL,
    observation TEXT NOT NULL,
    recommendation TEXT,
    tags TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_model_exp_agent ON model_experience_notes(agent_id);
CREATE INDEX IF NOT EXISTS idx_model_exp_model ON model_experience_notes(agent_id, provider, model);

-- ============================================
-- モデル価格情報
-- ============================================
CREATE TABLE IF NOT EXISTS model_pricing (
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    input_price_per_1m REAL NOT NULL,
    output_price_per_1m REAL NOT NULL,
    context_window INTEGER,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (provider, model)
);

-- ============================================
-- ハートビートログ
-- ============================================
CREATE TABLE IF NOT EXISTS heartbeat_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    decision TEXT NOT NULL,
    result_json TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_heartbeat_agent ON heartbeat_log(agent_id);

-- ============================================
-- ハートビート指示の監査ログ
-- ============================================
CREATE TABLE IF NOT EXISTS heartbeat_instructions_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    scope TEXT NOT NULL,
    channel_id TEXT,
    caller_identity TEXT NOT NULL,
    caller_discord_id TEXT,
    old_value TEXT,
    new_value TEXT,
    reason TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_heartbeat_instr_audit_agent
    ON heartbeat_instructions_audit(agent_id, created_at DESC);

-- ============================================
-- セッション状態
-- ============================================
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    mode TEXT NOT NULL DEFAULT 'facilitated',
    theme TEXT NOT NULL,
    phase TEXT NOT NULL DEFAULT 'divergent',
    turn_number INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'active',
    participant_ids_json TEXT NOT NULL DEFAULT '[]',
    facilitator_id TEXT,
    done_count INTEGER NOT NULL DEFAULT 0,
    max_turns INTEGER,
    metadata_json TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- ============================================
-- エージェントのセッション参加状態
-- ============================================
CREATE TABLE IF NOT EXISTS agent_sessions (
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    last_speech_at TEXT,
    done_declared INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (agent_id, session_id)
);

-- ============================================
-- Discordチャンネル設定
-- ============================================
CREATE TABLE IF NOT EXISTS discord_channel_config (
    channel_id TEXT NOT NULL,
    agent_id TEXT NOT NULL DEFAULT '',
    guild_id TEXT NOT NULL,
    channel_name TEXT NOT NULL DEFAULT '',
    readable INTEGER NOT NULL DEFAULT 1,
    writable INTEGER NOT NULL DEFAULT 1,
    whitelisted INTEGER NOT NULL DEFAULT 0,
    heartbeat_enabled INTEGER NOT NULL DEFAULT 1,
    heartbeat_interval_secs INTEGER,
    heartbeat_instructions TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL,
    PRIMARY KEY (channel_id, agent_id)
);
CREATE INDEX IF NOT EXISTS idx_discord_channel_guild ON discord_channel_config(guild_id);

-- ============================================
-- ペルソナプリセット
-- ============================================
CREATE TABLE IF NOT EXISTS soul_presets (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    preset_name TEXT NOT NULL,
    persona_name TEXT NOT NULL,
    custom_traits_json TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_soul_presets_agent ON soul_presets(agent_id);

-- ============================================
-- エージェント別Discord Bot設定
-- ============================================
CREATE TABLE IF NOT EXISTS agent_discord_config (
    agent_id TEXT PRIMARY KEY,
    bot_token TEXT NOT NULL,
    owner_discord_id TEXT NOT NULL DEFAULT '',
    enabled INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL
);

-- ============================================
-- Agent Webhook Config (subtask/tool/lifecycle webhook defaults)
-- ============================================
CREATE TABLE IF NOT EXISTS agent_webhook_config (
    scope        TEXT NOT NULL DEFAULT 'agent',
    agent_id     TEXT NOT NULL,
    tool_name    TEXT NOT NULL DEFAULT '',
    kind         TEXT NOT NULL DEFAULT 'subtask',
    url          TEXT NOT NULL,
    events_json  TEXT,
    enabled      INTEGER NOT NULL DEFAULT 1,
    name         TEXT,
    created_by   TEXT,
    output_mode  TEXT NOT NULL DEFAULT 'summary',
    max_chars    INTEGER NOT NULL DEFAULT 1500,
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (scope, agent_id, tool_name, kind)
);
CREATE INDEX IF NOT EXISTS idx_agent_webhook_agent ON agent_webhook_config(agent_id);

-- ============================================
-- 記憶インデックス: 階層ツリーノード
-- ============================================
CREATE TABLE IF NOT EXISTS memory_index_nodes (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    parent_id TEXT REFERENCES memory_index_nodes(id) ON DELETE CASCADE,
    node_type TEXT NOT NULL CHECK (node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly')),
    source_type TEXT NOT NULL DEFAULT 'session_log',
    title TEXT NOT NULL,
    summary TEXT NOT NULL,
    start_log_id INTEGER,
    end_log_id INTEGER,
    source_session_id TEXT,
    date_from TEXT,
    date_to TEXT,
    depth INTEGER NOT NULL DEFAULT 0,
    child_count INTEGER NOT NULL DEFAULT 0,
    token_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    short_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_mem_idx_agent ON memory_index_nodes(agent_id);
CREATE INDEX IF NOT EXISTS idx_mem_idx_parent ON memory_index_nodes(agent_id, parent_id);
CREATE INDEX IF NOT EXISTS idx_mem_idx_type ON memory_index_nodes(agent_id, node_type);
CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_index_nodes_short_id ON memory_index_nodes(agent_id, short_id) WHERE short_id IS NOT NULL;

-- ============================================
-- 記憶インデックス: ウォーターマーク（進捗管理）
-- ============================================
CREATE TABLE IF NOT EXISTS memory_index_watermark (
    agent_id TEXT PRIMARY KEY,
    last_indexed_log_id INTEGER NOT NULL DEFAULT 0,
    last_indexed_at TEXT NOT NULL,
    total_nodes INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS daily_log_index_watermark (
    agent_id TEXT NOT NULL PRIMARY KEY,
    last_indexed_date TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- ============================================
-- Co-Agent信頼関係
-- ============================================
CREATE TABLE IF NOT EXISTS trusted_co_agents (
    id           TEXT PRIMARY KEY,
    agent_id     TEXT NOT NULL,
    co_agent_id  TEXT NOT NULL,
    allowed_actions TEXT,
    created_by   TEXT NOT NULL,
    created_at   DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (agent_id, co_agent_id)
);
CREATE INDEX IF NOT EXISTS idx_trusted_co_agents_agent ON trusted_co_agents(agent_id);

-- ============================================
-- 信頼済みDiscordユーザー
-- ============================================
CREATE TABLE IF NOT EXISTS trusted_discord_users (
  id TEXT PRIMARY KEY,
  discord_user_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  permission TEXT NOT NULL DEFAULT 'user',
  created_by TEXT NOT NULL DEFAULT 'owner',
  created_at TEXT NOT NULL,
  display_name TEXT NOT NULL DEFAULT '',
  UNIQUE (discord_user_id, agent_id)
);
CREATE INDEX IF NOT EXISTS idx_trusted_discord_users_agent ON trusted_discord_users(agent_id);

-- ============================================
-- エージェント別メモリインデックス設定
-- ============================================
CREATE TABLE IF NOT EXISTS agent_memory_index_config (
    agent_id TEXT PRIMARY KEY,
    batch_size INTEGER NOT NULL DEFAULT 50,
    threshold INTEGER NOT NULL DEFAULT 20,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_skill_consolidation_at TEXT
);

-- スキル利用のセッション単位記録（スリープ棚卸しの弱い利用ヒント用）
CREATE TABLE IF NOT EXISTS skill_usage_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_skill_usage_log_skill ON skill_usage_log(skill_id);
CREATE INDEX IF NOT EXISTS idx_skill_usage_log_session ON skill_usage_log(session_id);

-- ============================================
-- エージェント別許可コマンド（動的追加）
-- ============================================
CREATE TABLE IF NOT EXISTS agent_allowed_commands (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    command TEXT NOT NULL,
    added_by TEXT NOT NULL DEFAULT 'owner',
    added_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (agent_id, command)
);
CREATE INDEX IF NOT EXISTS idx_agent_allowed_commands_agent ON agent_allowed_commands(agent_id);

-- ============================================
-- LLM入出力ログ
-- ============================================
CREATE TABLE IF NOT EXISTS llm_logs (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    session_id TEXT,
    model TEXT,
    prompt TEXT NOT NULL DEFAULT '',
    response TEXT NOT NULL DEFAULT '',
    tool_calls TEXT,
    latency_ms INTEGER,
    prompt_tokens INTEGER,
    completion_tokens INTEGER,
    total_tokens INTEGER,
    error_code TEXT,
    error_body TEXT,
    requested_at TEXT,
    trigger_message_id TEXT,
    is_bot_iteration INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER,
    cache_creation_tokens INTEGER,
    created_at TEXT DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_llm_logs_agent ON llm_logs(agent_id);
CREATE INDEX IF NOT EXISTS idx_llm_logs_created ON llm_logs(agent_id, created_at DESC);

-- ============================================
-- インポート同期状態
-- ============================================
CREATE TABLE IF NOT EXISTS import_sync_state (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    source_dir TEXT NOT NULL,
    file_type TEXT NOT NULL,
    file_name TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    synced_at TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_import_sync_state_key
    ON import_sync_state(agent_id, source_dir, file_name);
CREATE INDEX IF NOT EXISTS idx_import_sync_state_agent
    ON import_sync_state(agent_id);

-- ============================================
-- エージェントログ
-- ============================================
CREATE TABLE IF NOT EXISTS agent_logs (
    id TEXT PRIMARY KEY,
    agent_id TEXT,
    level TEXT NOT NULL,
    context TEXT NOT NULL,
    message TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_agent_logs_agent ON agent_logs(agent_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_agent_logs_level ON agent_logs(level, created_at DESC);

-- ============================================
-- A2UI: 保留中のインタラクション
-- ============================================
CREATE TABLE IF NOT EXISTS pending_interactions (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    message_id TEXT,
    platform TEXT NOT NULL DEFAULT 'discord',
    surface_id TEXT NOT NULL,
    a2ui_components_json TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    response_json TEXT,
    responder_id TEXT,
    owner_only INTEGER NOT NULL DEFAULT 1,
    timeout_secs INTEGER NOT NULL DEFAULT 300,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    responded_at TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_pending_interactions_agent
    ON pending_interactions(agent_id, status);
CREATE INDEX IF NOT EXISTS idx_pending_interactions_session
    ON pending_interactions(session_id, status);
CREATE INDEX IF NOT EXISTS idx_pending_interactions_surface
    ON pending_interactions(surface_id);

-- ============================================
-- TASK LEDGER: 前向きワーキング状態（goal/契約/進捗/決定）
-- ============================================
CREATE TABLE IF NOT EXISTS task_ledger (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    goal TEXT NOT NULL,
    contract TEXT,
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'done', 'abandoned')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_ledger_session
    ON task_ledger(agent_id, session_id, status);
CREATE UNIQUE INDEX IF NOT EXISTS idx_task_ledger_one_active
    ON task_ledger(agent_id, session_id) WHERE status = 'active';

CREATE TABLE IF NOT EXISTS task_progress (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES task_ledger(id) ON DELETE CASCADE,
    kind TEXT NOT NULL DEFAULT 'progress'
        CHECK (kind IN ('progress', 'decision', 'blocker')),
    content TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_progress_task ON task_progress(task_id);
"#;

#[cfg(test)]
mod migration_tests {
    use super::*;
    use rusqlite::Connection;

    /// A. バージョン管理導入前の旧DBを模して、baseline が再適用され version 1 に
    /// スタンプされることを検証する。
    #[test]
    fn baseline_reconciles_pre_versioning_db() {
        let conn = crate::init_memory().expect("init");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // 旧DBを模す: version を 0 に戻し、baseline が再追加する列を落とす。
        conn.execute_batch("PRAGMA user_version = 0").unwrap();
        conn.execute_batch("ALTER TABLE skills DROP COLUMN archived")
            .unwrap();
        assert!(!column_exists(&conn, "skills", "archived").unwrap());

        // 再初期化で baseline + 番号付きマイグレーションが走り、列が復活し最新版にスタンプされる。
        initialize(&conn).expect("re-initialize");
        assert!(column_exists(&conn, "skills", "archived").unwrap());
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
    }

    /// B. 冪等性: baseline 済みDBで initialize を再実行しても破壊的再構築は走らず、
    /// 既存データが保持される。
    #[test]
    fn initialize_is_idempotent_and_non_destructive() {
        let conn = crate::init_memory().expect("init");
        conn.execute_batch(
            "INSERT INTO agents (agent_id, name, persona_name) VALUES ('sentinel', 'n', 'p')",
        )
        .unwrap();

        initialize(&conn).expect("second initialize");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agents WHERE agent_id = 'sentinel'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "sentinel row must survive (baseline not re-run)");
    }

    fn create_marker(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch("CREATE TABLE test_marker (id INTEGER)")
    }

    /// C. 番号付きマイグレーションのランナー: 未適用の version 2 を適用し、
    /// 再実行では no-op になる（version gate）。
    #[test]
    fn run_migrations_applies_and_then_skips() {
        let conn = crate::init_memory().expect("init");
        // 実 MIGRATIONS 適用済み（= 最新版）なので、fake v2 が未適用となる状態に戻す。
        conn.execute_batch("PRAGMA user_version = 1").unwrap();
        let fake = &[Migration {
            version: 2,
            description: "add test_marker",
            up: create_marker,
        }];

        run_migrations(&conn, fake).expect("apply v2");
        assert!(table_exists(&conn, "test_marker").unwrap());
        assert_eq!(schema_version(&conn).unwrap(), 2);

        // 再実行は no-op（既に version 2）。
        run_migrations(&conn, fake).expect("re-run no-op");
        assert_eq!(schema_version(&conn).unwrap(), 2);
    }

    fn fail_after_marker(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch("CREATE TABLE test_marker (id INTEGER)")?;
        Err(rusqlite::Error::InvalidQuery)
    }

    /// D. マイグレーション失敗時は、その up の変更と version スタンプが
    /// トランザクションごとロールバックされる。
    #[test]
    fn failed_migration_rolls_back_and_leaves_version() {
        let conn = crate::init_memory().expect("init");
        // 実 MIGRATIONS 適用済みなので、fake v2 が適用対象となる状態に戻す。
        conn.execute_batch("PRAGMA user_version = 1").unwrap();
        let fake = &[Migration {
            version: 2,
            description: "fails",
            up: fail_after_marker,
        }];

        let result = run_migrations(&conn, fake);
        assert!(result.is_err());
        assert!(!table_exists(&conn, "test_marker").unwrap());
        assert_eq!(schema_version(&conn).unwrap(), BASELINE_VERSION);
    }

    /// E. 実 MIGRATIONS の version は厳密増加・全て baseline より大きい・重複なし。
    #[test]
    fn agent_sessions_backfill_migration_v4() {
        let conn = crate::init_memory().expect("init");
        // v3 状態に戻し、v4 の backfill 対象となる sessions 行を用意する
        // （うち1件は壊れた JSON — skip され、他の行は影響を受けないこと）。
        conn.execute_batch("PRAGMA user_version = 3").unwrap();
        conn.execute_batch("DELETE FROM agent_sessions").unwrap();
        conn.execute_batch(
            "INSERT INTO sessions (id, mode, theme, phase, turn_number, status, participant_ids_json, done_count, created_at, updated_at)
             VALUES ('s1', 'discord', 't', 'active', 0, 'active', '[\"a1\",\"a2\"]', 0, '2026-01-01', '2026-01-01'),
                    ('s2', 'discord', 't', 'active', 0, 'active', 'not-json', 0, '2026-01-01', '2026-01-01'),
                    ('s3', 'discord', 't', 'active', 0, 'active', '[\"a1\"]', 0, '2026-01-01', '2026-01-01')",
        )
        .unwrap();

        run_migrations(&conn, MIGRATIONS).expect("v4 migration");

        let participants = crate::queries::list_session_participants(&conn, "s1").unwrap();
        assert_eq!(participants, vec!["a1".to_string(), "a2".to_string()]);
        assert!(crate::queries::list_session_participants(&conn, "s2")
            .unwrap()
            .is_empty());
        assert_eq!(
            crate::queries::count_sessions_for_agent(&conn, "a1").unwrap(),
            2
        );
        // 部分一致の誤マッチが無いこと（旧 LIKE 実装は "a" が "a1" にもマッチした）
        assert_eq!(
            crate::queries::count_sessions_for_agent(&conn, "a").unwrap(),
            0
        );
    }

    #[test]
    fn memory_index_fk_cascade_and_check_enforced() {
        let conn = crate::init_memory().expect("init");
        conn.execute_batch(
            "INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, title, summary, created_at, updated_at)
             VALUES ('r', 'a1', NULL, 'root', 't', 's', '2026-01-01', '2026-01-01'),
                    ('c', 'a1', 'r', 'topic', 't', 's', '2026-01-01', '2026-01-01')",
        )
        .unwrap();
        // CHECK: 不正 node_type は拒否
        assert!(conn
            .execute_batch(
                "INSERT INTO memory_index_nodes (id, agent_id, node_type, title, summary, created_at, updated_at)
                 VALUES ('x', 'a1', 'bogus', 't', 's', '2026-01-01', '2026-01-01')",
            )
            .is_err());
        // FK: 存在しない親は拒否
        assert!(conn
            .execute_batch(
                "INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, title, summary, created_at, updated_at)
                 VALUES ('y', 'a1', 'nope', 'topic', 't', 's', '2026-01-01', '2026-01-01')",
            )
            .is_err());
        // CASCADE: 親削除で子も消える
        conn.execute("DELETE FROM memory_index_nodes WHERE id = 'r'", [])
            .unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_index_nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn memory_index_rebuild_migration_v5_upgrades_legacy_table() {
        let conn = crate::init_memory().expect("init");
        // v4 時点の旧テーブル形（FK/CHECK なし）を再現する
        conn.execute_batch("PRAGMA user_version = 4").unwrap();
        conn.execute_batch(
            "DROP TABLE memory_index_nodes;
             CREATE TABLE memory_index_nodes (
                id TEXT PRIMARY KEY, agent_id TEXT NOT NULL, parent_id TEXT,
                node_type TEXT NOT NULL, source_type TEXT NOT NULL DEFAULT 'session_log',
                title TEXT NOT NULL, summary TEXT NOT NULL,
                start_log_id INTEGER, end_log_id INTEGER, source_session_id TEXT,
                date_from TEXT, date_to TEXT,
                depth INTEGER NOT NULL DEFAULT 0, child_count INTEGER NOT NULL DEFAULT 0,
                token_count INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, short_id TEXT
             );
             INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, title, summary, created_at, updated_at)
             VALUES ('r', 'a1', NULL, 'root', 't', 's', '2026-01-01', '2026-01-01'),
                    ('c', 'a1', 'r', 'topic', 't', 's', '2026-01-01', '2026-01-01'),
                    ('orphan', 'a1', 'ghost', 'topic', 't', 's', '2026-01-01', '2026-01-01'),
                    ('junk', 'a1', NULL, 'bogus_type', 't', 's', '2026-01-01', '2026-01-01')",
        )
        .unwrap();

        run_migrations(&conn, MIGRATIONS).expect("v5 migration");

        // FK が付与され、正常行は保存、孤児は parent NULL 化、不正 node_type は除外
        let fk_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_list('memory_index_nodes')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fk_count, 1);
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM memory_index_nodes ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            ids,
            vec!["c".to_string(), "orphan".to_string(), "r".to_string()]
        );
        let orphan_parent: Option<String> = conn
            .query_row(
                "SELECT parent_id FROM memory_index_nodes WHERE id = 'orphan'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphan_parent, None);
    }

    #[test]
    fn task_ledger_restart_count_migration_v6() {
        // 新規DB: init 直後から列がある（v6 適用済み）。
        let conn = crate::init_memory().expect("init");
        assert!(column_exists(&conn, "task_ledger", "restart_count").unwrap());

        // 既存DB（v5 時点 = 列なし）からの upgrade。
        conn.execute_batch(
            "DROP TABLE task_progress; DROP TABLE task_ledger; PRAGMA user_version = 1",
        )
        .unwrap();
        conn.execute_batch(TASK_LEDGER_SQL).unwrap();
        conn.execute_batch("PRAGMA user_version = 5").unwrap();
        conn.execute_batch(
            "INSERT INTO task_ledger (agent_id, session_id, goal, status, created_at, updated_at)
             VALUES ('a1', 's1', 'g', 'active', '2026-01-01', '2026-01-01')",
        )
        .unwrap();
        assert!(!column_exists(&conn, "task_ledger", "restart_count").unwrap());

        run_migrations(&conn, MIGRATIONS).expect("v6 migration");
        assert!(column_exists(&conn, "task_ledger", "restart_count").unwrap());
        // 既存行は DEFAULT 0 で読める
        let count: i64 = conn
            .query_row(
                "SELECT restart_count FROM task_ledger WHERE agent_id = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
    }

    #[test]
    fn memory_index_keywords_migration_v7() {
        // 新規DB: init 直後から列と FTS 表がある。
        let conn = crate::init_memory().expect("init");
        assert!(column_exists(&conn, "memory_index_nodes", "keywords_json").unwrap());
        assert!(column_exists(&conn, "memory_index_nodes", "summary_refreshed_at").unwrap());
        assert!(table_exists(&conn, "memory_index_fts").unwrap());

        // 既存DB（v6 時点 = 列も FTS も無い）からの upgrade。
        conn.execute_batch(
            "DROP TABLE memory_index_fts;
             DROP TABLE memory_index_nodes;
             CREATE TABLE memory_index_nodes (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                parent_id TEXT REFERENCES memory_index_nodes(id) ON DELETE CASCADE,
                node_type TEXT NOT NULL CHECK (node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly')),
                source_type TEXT NOT NULL DEFAULT 'session_log',
                title TEXT NOT NULL, summary TEXT NOT NULL,
                start_log_id INTEGER, end_log_id INTEGER, source_session_id TEXT,
                date_from TEXT, date_to TEXT,
                depth INTEGER NOT NULL DEFAULT 0, child_count INTEGER NOT NULL DEFAULT 0,
                token_count INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, short_id TEXT
             );
             INSERT INTO memory_index_nodes (id, agent_id, node_type, title, summary, created_at, updated_at)
             VALUES ('t-legacy', 'a1', 'topic', '旧トピック', '旧要約テキスト', '2026-01-01', '2026-01-01');
             PRAGMA user_version = 6;",
        )
        .unwrap();
        assert!(!column_exists(&conn, "memory_index_nodes", "keywords_json").unwrap());

        run_migrations(&conn, MIGRATIONS).expect("v7 migration");
        assert!(column_exists(&conn, "memory_index_nodes", "keywords_json").unwrap());
        assert!(column_exists(&conn, "memory_index_nodes", "summary_refreshed_at").unwrap());
        // 既存行が FTS にバックフィルされ、trigram で部分一致検索できる
        let fts_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_index_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_rows, 1);
        let hits = crate::queries::search_index_nodes(&conn, "a1", "旧要約", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node_id, "t-legacy");
        // keywords_json は DEFAULT '[]' で読める
        assert_eq!(hits[0].keywords_json, "[]");
        // 再実行してもバックフィルは重複しない
        conn.execute_batch("PRAGMA user_version = 6").unwrap();
        run_migrations(&conn, MIGRATIONS).expect("v7 rerun");
        let fts_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_index_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_rows, 1);
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
    }

    #[test]
    fn migrations_versions_are_strictly_increasing() {
        let mut prev = BASELINE_VERSION;
        for m in MIGRATIONS {
            assert!(
                m.version > prev,
                "migration versions must be strictly increasing and > baseline"
            );
            prev = m.version;
        }
    }

    /// G. v1 DB が v2 マイグレーションでタスク台帳テーブルを獲得する。
    #[test]
    fn task_ledger_migration_upgrades_v1_db() {
        let conn = crate::init_memory().expect("init");
        // v1 相当の既存DBを模す: タスク台帳を落として version 1 に戻す。
        conn.execute_batch(
            "DROP TABLE task_progress; DROP TABLE task_ledger; PRAGMA user_version = 1",
        )
        .unwrap();
        assert!(!table_exists(&conn, "task_ledger").unwrap());

        initialize(&conn).expect("upgrade v1 -> latest");
        assert!(table_exists(&conn, "task_ledger").unwrap());
        assert!(table_exists(&conn, "task_progress").unwrap());
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
    }

    /// v3: display_name 列の付与。v2 相当 DB（列なし）からのアップグレードと、
    /// 新規 DB（SCHEMA_SQL 由来で列あり）での冪等性の両方を確認する。
    #[test]
    fn display_name_migration_upgrades_v2_db() {
        let conn = crate::init_memory().expect("init");
        // 新規 DB には既に列がある（SCHEMA_SQL 由来）
        assert!(column_exists(&conn, "trusted_discord_users", "display_name").unwrap());

        // v2 相当の既存 DB を模す: 列なしのテーブルに作り直して version 2 に戻す
        conn.execute_batch(
            "DROP TABLE trusted_discord_users;
             CREATE TABLE trusted_discord_users (
               id TEXT PRIMARY KEY,
               discord_user_id TEXT NOT NULL,
               agent_id TEXT NOT NULL,
               permission TEXT NOT NULL DEFAULT 'user',
               created_by TEXT NOT NULL DEFAULT 'owner',
               created_at TEXT NOT NULL,
               UNIQUE (discord_user_id, agent_id)
             );
             PRAGMA user_version = 2",
        )
        .unwrap();
        assert!(!column_exists(&conn, "trusted_discord_users", "display_name").unwrap());

        initialize(&conn).expect("upgrade v2 -> v3");
        assert!(column_exists(&conn, "trusted_discord_users", "display_name").unwrap());
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // 再実行しても冪等
        initialize(&conn).expect("idempotent");
    }

    /// H. SCHEMA_SQL 側と TASK_LEDGER_SQL 側で生成されるテーブル定義が一致する
    /// （両所への二重記載がドリフトしていないことの検証）。
    #[test]
    fn task_ledger_schema_parity() {
        let dump = |conn: &Connection| -> Vec<String> {
            conn.prepare(
                "SELECT sql FROM sqlite_master
                 WHERE name IN ('task_ledger','task_progress',
                                'idx_task_ledger_session','idx_task_ledger_one_active',
                                'idx_task_progress_task')
                 ORDER BY name",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
        };

        // 新規DB: SCHEMA_SQL 由来（baseline 時点でテーブルが出来ており、v2 は no-op）。
        let fresh = crate::init_memory().expect("fresh");
        // 既存DB: baseline 後に v2 マイグレーション由来で作成。
        let migrated = crate::init_memory().expect("migrated");
        migrated
            .execute_batch(
                "DROP TABLE task_progress; DROP TABLE task_ledger; PRAGMA user_version = 1",
            )
            .unwrap();
        initialize(&migrated).expect("re-migrate");

        assert_eq!(dump(&fresh), dump(&migrated));
        assert_eq!(dump(&fresh).len(), 5, "expected 2 tables + 3 indexes");
    }

    /// F. ダウングレードガード: DB が既知の最新版より新しい場合はエラーにする。
    #[test]
    fn downgrade_is_rejected() {
        let conn = crate::init_memory().expect("init");
        conn.execute_batch("PRAGMA user_version = 999").unwrap();
        let fake = &[Migration {
            version: 2,
            description: "v2",
            up: create_marker,
        }];
        let result = run_migrations(&conn, fake);
        assert!(result.is_err(), "newer-than-supported DB must be rejected");
        assert!(!table_exists(&conn, "test_marker").unwrap());
    }
}
