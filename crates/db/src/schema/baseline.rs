use rusqlite::Connection;

use super::helpers::{column_exists, table_exists};

/// FROZEN — schema version 1 baseline。
///
/// ⚠️ **警告: ここへ ALTER / 列追加を足しても既存 DB には一切効かない。**
/// この関数は `initialize` から `user_version < BASELINE_VERSION`（＝新規 DB / 版管理
/// 導入前の DB）のときしか呼ばれない。本番など既に版がスタンプ済みの DB は
/// `run_migrations` しか通らないため、ここへ書いた変更は永久に no-op になる
/// （#347 でこの罠を踏み、本番の全スキル SELECT が `no such column` で落ちた。#349）。
/// **既存 DB に効かせる変更は必ず version 2 以降の番号付き `MIGRATIONS` エントリへ**
/// 追加すること。ここは version 1 として確定した履歴であり、`backfill_short_ids` 呼び出しや
/// `migrate_soul_identity_to_agents` 含めて凍結する。
///
/// 既存テーブルへのマイグレーション（カラム追加など）。
pub(super) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
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

    // 旧 soul の JSON 列（social_style_json / thinking_style_json）は、以前ここで
    // 「dead code 削除」として DROP していた。だが soul は後段の
    // `migrate_soul_identity_to_agents` で**テーブルごと** DROP されるため、この個別 DROP は
    // 常にその直後の全体 DROP に呑まれる冗長操作でしかない。しかも thinking_style_json は
    // 自由記述 `description` を含み、ここで先に落とすと集約時の退避（#480）が拾えなくなる。
    // よって個別 DROP は撤去し、全 JSON 列の退避は集約側に一本化する（列は soul ごと消える）。

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
        crate::queries::backfill_short_ids(conn)
            .map_err(|e| rusqlite::Error::InvalidParameterName(format!("{e}")))?;
    }
    // short_id の partial index は SCHEMA_SQL ではなく **ここ** で張る（列確定後・#475）。
    // fresh DB は SCHEMA_SQL 側で列を持つので上の分岐は skip されるが、この index は
    // 新規 DB でも旧 DB でも必ず必要なので分岐の外で冪等に張る（IF NOT EXISTS）。
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_index_nodes_short_id ON memory_index_nodes(agent_id, short_id) WHERE short_id IS NOT NULL",
    )?;

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

        // soul.personality カラム追加（#480）。soul が `personality_json` しか持たない最初期
        // （2026-02・b6a145e）世代の DB は `personality` 列を持たず、後段の
        // `migrate_soul_identity_to_agents` が `SELECT ... s.personality ... FROM soul` で
        // `no such column: s.personality` を投げて起動不能になる。集約前に列を用意して塞ぐ。
        // 旧 `personality_json`（構造化 JSON）は agents.personality（自由記述 TEXT・NULL 可）に
        // 意味的対応が無いため移送せず NULL のままにする（起動の担保が目的・#478 と同じ発想）。
        let has_personality: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('soul') WHERE name='personality'")?
            .query_row([], |row| row.get::<_, i64>(0))
            .map(|c| c > 0)
            .unwrap_or(false);
        if !has_personality {
            conn.execute_batch("ALTER TABLE soul ADD COLUMN personality TEXT")?;
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

    // #480: 上の集約は soul から persona_name / personality / instructions しか agents に
    // 写さない。残る JSON 列（social_style_json / personality_json=Big Five /
    // thinking_style_json / custom_traits_json）は直後の DROP TABLE soul で失われる。
    // thinking_style_json は自由記述 `description` を、custom_traits_json は利用者任意の JSON を
    // 含み得るため、「意図して設定した値を勝手に破棄しない」原則（#456）に反する。
    // → DROP 前に、存在する JSON 列を **agents.metadata_json.legacy_soul** へ入れ子で退避する。
    //
    // 頑健性: 世代により存在する列が違う（8b2b2b8 以降の soul は JSON 列を一切持たない）ため
    // 実在する列だけを動的に組み立てる。列値が不正 JSON / NULL でも起動を止めないよう
    // `json_valid` で分岐し（不正 JSON はエラーになる `json()` を避けて生文字列で保持）、
    // 既存の metadata_json（identity 由来）が入っている経路も壊さない（valid ならそこへ挿す・
    // 不正でも `_original_metadata` に退避してから legacy_soul を足す）。
    let legacy_cols = [
        "social_style_json",
        "personality_json",
        "thinking_style_json",
        "custom_traits_json",
    ];
    let present: Vec<&str> = legacy_cols
        .into_iter()
        .filter(|c| column_exists(conn, "soul", c).unwrap_or(false))
        .collect();
    if !present.is_empty() {
        let obj_fields = present
            .iter()
            .map(|c| format!("'{c}', CASE WHEN json_valid(s.{c}) THEN json(s.{c}) ELSE s.{c} END"))
            .collect::<Vec<_>>()
            .join(", ");
        // 不正 JSON を含む行数を先に数える（起動は止めず、warn で可視化するため）。
        // 「壊れていた」= 実在列のいずれかが非 NULL かつ `json_valid` でない行。
        let broken_pred = present
            .iter()
            .map(|c| format!("(s.{c} IS NOT NULL AND NOT json_valid(s.{c}))"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let broken_json_rows: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM soul s WHERE {broken_pred}"),
            [],
            |r| r.get(0),
        )?;

        let sql = format!(
            "UPDATE agents
             SET metadata_json = json_set(
                 CASE
                     WHEN metadata_json IS NULL THEN '{{}}'
                     WHEN json_valid(metadata_json) THEN metadata_json
                     ELSE json_object('_original_metadata', metadata_json)
                 END,
                 '$.legacy_soul',
                 json((SELECT json_object({obj_fields}) FROM soul s WHERE s.agent_id = agents.agent_id))
             )
             WHERE agent_id IN (SELECT agent_id FROM soul)"
        );
        let salvaged_rows = conn.execute(&sql, [])?;

        // 移行の可視化（#480）: この世代の DB は今まで起動できず、利用者は何が起きるか分からない。
        // 黙って通すと「何か消えたかも」と疑うことすらできないため、退避したことと件数を残す。
        if salvaged_rows > 0 {
            tracing::info!(
                salvaged_rows,
                "旧 soul の付随データ（JSON 列）を agents.metadata_json の legacy_soul へ退避した"
            );
        }
        if broken_json_rows > 0 {
            tracing::warn!(
                broken_json_rows,
                "旧 soul の付随データに不正 JSON が含まれ、構造化せず生文字列として退避した（起動は継続）"
            );
        }
    }

    conn.execute_batch("DROP TABLE IF EXISTS soul; DROP TABLE IF EXISTS identity;")?;
    Ok(())
}
