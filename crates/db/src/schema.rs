use rusqlite::Connection;

/// スキーマ初期化
pub fn initialize(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA_SQL)?;
    migrate(conn)?;
    Ok(())
}

/// 既存テーブルへのマイグレーション（カラム追加など）
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
        conn.execute_batch("ALTER TABLE skills ADD COLUMN permission TEXT NOT NULL DEFAULT '\"agent\"'")?;
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
        conn.execute_batch("ALTER TABLE discord_channel_config ADD COLUMN whitelisted INTEGER NOT NULL DEFAULT 0")?;
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
        conn.execute_batch("ALTER TABLE discord_channel_config ADD COLUMN heartbeat_interval_secs INTEGER")?;
    }

    // agent_memory_index_config テーブル作成（既存DBへの対応）
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_memory_index_config (
            agent_id TEXT PRIMARY KEY,
            batch_size INTEGER NOT NULL DEFAULT 50,
            threshold INTEGER NOT NULL DEFAULT 20,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        )"
    )?;

    // soul.custom_traits_json → soul.personality rename
    let has_custom_traits: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('soul') WHERE name='custom_traits_json'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if has_custom_traits {
        conn.execute_batch("ALTER TABLE soul RENAME COLUMN custom_traits_json TO personality")?;
    }

    // soul.personality_json → drop
    let has_personality_json: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('soul') WHERE name='personality_json'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if has_personality_json {
        conn.execute_batch("ALTER TABLE soul DROP COLUMN personality_json")?;
    }

    // llm_logs テーブル作成（既存DBへの対応）
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS llm_logs (
            id TEXT PRIMARY KEY,
            agent_id TEXT NOT NULL,
            session_id TEXT,
            model TEXT,
            prompt TEXT NOT NULL,
            response TEXT NOT NULL,
            tool_calls TEXT,
            created_at TEXT DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_llm_logs_agent ON llm_logs(agent_id);
        CREATE INDEX IF NOT EXISTS idx_llm_logs_created ON llm_logs(agent_id, created_at DESC);"
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

    Ok(())
}

const SCHEMA_SQL: &str = r#"
-- ============================================
-- SOUL: ペルソナの核心
-- ============================================
CREATE TABLE IF NOT EXISTS soul (
    agent_id TEXT PRIMARY KEY,
    persona_name TEXT NOT NULL,
    social_style_json TEXT NOT NULL DEFAULT '{}',
    thinking_style_json TEXT NOT NULL DEFAULT '{}',
    personality TEXT,
    updated_at TEXT NOT NULL
);

-- ============================================
-- IDENTITY: 役割・立場
-- ============================================
CREATE TABLE IF NOT EXISTS identity (
    agent_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    job_title TEXT,
    organization TEXT,
    image_url TEXT,
    metadata_json TEXT,
    updated_at TEXT NOT NULL
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
    guild_id TEXT NOT NULL,
    channel_name TEXT NOT NULL DEFAULT '',
    readable INTEGER NOT NULL DEFAULT 1,
    writable INTEGER NOT NULL DEFAULT 1,
    whitelisted INTEGER NOT NULL DEFAULT 0,
    heartbeat_enabled INTEGER NOT NULL DEFAULT 1,
    heartbeat_interval_secs INTEGER,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (channel_id)
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
-- 記憶インデックス: 階層ツリーノード
-- ============================================
CREATE TABLE IF NOT EXISTS memory_index_nodes (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    parent_id TEXT,
    node_type TEXT NOT NULL,
    title TEXT NOT NULL,
    summary TEXT NOT NULL,
    start_log_id INTEGER,
    end_log_id INTEGER,
    source_session_id TEXT,
    depth INTEGER NOT NULL DEFAULT 0,
    child_count INTEGER NOT NULL DEFAULT 0,
    token_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mem_idx_agent ON memory_index_nodes(agent_id);
CREATE INDEX IF NOT EXISTS idx_mem_idx_parent ON memory_index_nodes(agent_id, parent_id);
CREATE INDEX IF NOT EXISTS idx_mem_idx_type ON memory_index_nodes(agent_id, node_type);

-- ============================================
-- 記憶インデックス: ウォーターマーク（進捗管理）
-- ============================================
CREATE TABLE IF NOT EXISTS memory_index_watermark (
    agent_id TEXT PRIMARY KEY,
    last_indexed_log_id INTEGER NOT NULL DEFAULT 0,
    last_indexed_at TEXT NOT NULL,
    total_nodes INTEGER NOT NULL DEFAULT 0
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
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

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
    prompt TEXT NOT NULL,
    response TEXT NOT NULL,
    tool_calls TEXT,
    created_at TEXT DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_llm_logs_agent ON llm_logs(agent_id);
CREATE INDEX IF NOT EXISTS idx_llm_logs_created ON llm_logs(agent_id, created_at DESC);
"#;
