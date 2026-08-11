//! スキーマ定義の純データ（`SCHEMA_SQL` と各テーブルの const SQL 文字列）。
//!
//! `schema.rs` から機械的に切り出した純粋な文字列定数で、文面は 1 文字も変えていない（#518）。
//! 親モジュール（`schema`）の MIGRATIONS クロージャ / `migrate()` / `initialize` から参照される。

/// カテゴリ層メンバー表 — **v23 当時の形**（topic ↔ category の参照, issue #313）。
///
/// PK は `(agent_id, topic_id)` = 1 topic 高々 1 category（sticky）。**これは v23 が
/// 作った履歴の形**であり、v26（#358）で多対多 PK（[`MEMORY_CATEGORY_MEMBERS_MM_SQL`]）
/// へ作り直す。最終形（新規 DB の SCHEMA_SQL / 既存 DB の v26 収束先）は多対多の方。
/// この const は v23 マイグレーション専用として残す（凍結された履歴の再現）。FK は
/// 張らない（追記的・可逆を優先: category/meta を切り戻しで消しても member 行が残る
/// だけで害が無い）。
pub(super) const MEMORY_CATEGORY_MEMBERS_SQL: &str = "
CREATE TABLE IF NOT EXISTS memory_category_members (
    agent_id TEXT NOT NULL,
    topic_id TEXT NOT NULL,
    category_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (agent_id, topic_id)
);
CREATE INDEX IF NOT EXISTS idx_memory_category_members_cat ON memory_category_members(agent_id, category_id);
";

/// カテゴリ層メンバー表 — **多対多 PK の最終形**（issue #358 / v26）。
///
/// PK を `(agent_id, topic_id, category_id)` にして 1 topic に複数の category を付けられる
/// ようにする。SQLite は PK 変更＝テーブル再構築なので DROP+CREATE で作り直す（v26 の時点で
/// 旧行は白紙化対象なので保全しない）。**SCHEMA_SQL 内の同名ブロックと文面を揃えること**
/// （新規 DB は SCHEMA_SQL、既存 DB は v26 で同じ形に収束する）。FK は張らない（v23 と同方針）。
pub(super) const MEMORY_CATEGORY_MEMBERS_MM_SQL: &str = "
DROP TABLE IF EXISTS memory_category_members;
CREATE TABLE memory_category_members (
    agent_id TEXT NOT NULL,
    topic_id TEXT NOT NULL,
    category_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (agent_id, topic_id, category_id)
);
CREATE INDEX IF NOT EXISTS idx_memory_category_members_cat ON memory_category_members(agent_id, category_id);
";

/// per-agent の Nostr sub-gateway 設定。秘密鍵はエージェント毎に隔離（鍵の共有防止）。
pub(super) const AGENT_NOSTR_CONFIG_SQL: &str = "
CREATE TABLE IF NOT EXISTS agent_nostr_config (
    agent_id TEXT PRIMARY KEY,
    secret_key TEXT NOT NULL,
    relays_json TEXT NOT NULL DEFAULT '[]',
    filter_json TEXT NOT NULL DEFAULT '{}',
    enabled INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);
";

/// per-agent の Nostr 受信転記先設定（issue #252 段階 A）。
///
/// エージェントが Nostr で受け取った自分宛の受信を、エージェント単位で設定した 1 つの
/// Discord チャンネル（webhook）へ転記するための宛先。
///
/// - `enabled`: 既定 **0（無効）**。行を作っただけでは転記しない（fail-closed / #240 と同じ轍を
///   踏まない）。行が無いエージェントも無効として扱う（上位の解決が fail-closed）。
/// - `webhook_url`: 転記先の webhook URL。NULL / 空なら転記しない。URL の妥当性検証は
///   db 層では行わず、`opencrab_actions::webhook_target::resolve_nostr_relay_webhook` が担う
///   （db クレートは Discord/webhook の語彙に依存しない）。
pub(super) const AGENT_NOSTR_RELAY_CONFIG_SQL: &str = "
CREATE TABLE IF NOT EXISTS agent_nostr_relay_config (
    agent_id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL DEFAULT 0,
    webhook_url TEXT,
    updated_at TEXT NOT NULL
);
";

/// per-agent のハートビート設定（#247）。**エージェント自身が触れる唯一の自律実行設定**。
///
/// - `enabled`: 既定 **0（無効）**。設定を作っただけで自律実行が始まらないようにする（#240）。
/// - `interval_secs`: NULL = 運用者の既定（設定ファイルの `[agent] heartbeat_interval_secs`）
///   に従う。値の下限は設定ファイル（`[agent] heartbeat_min_interval_secs`）で運用者が決め、
///   書き込み口（`set_my_heartbeat`）が下限より短い要求を**拒否**する。DB 側に CHECK は
///   置かない（下限は運用者が変えられる値なので、スキーマに焼き付けると変更のたびに
///   マイグレーションが要る）。
///
/// 行が無い / 壊れているときは**無効**として扱う（`queries::resolve_agent_heartbeat`）。
pub(super) const AGENT_HEARTBEAT_CONFIG_SQL: &str = "
CREATE TABLE IF NOT EXISTS agent_heartbeat_config (
    agent_id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL DEFAULT 0,
    interval_secs INTEGER,
    updated_at TEXT NOT NULL
);
";

/// セッション単位のハートビート設定（統合スケジューラ / #439 × #456 の PR1）。
///
/// agent スコープ（`agent_heartbeat_config`）と channel スコープ
/// （`discord_channel_config.heartbeat_*`）の二本立てを **セッション単位の 1 テーブル**へ
/// 畳んだ後継。`session_id` は `nostr-{agent}` / `discord-{agent}-{guild}-{channel}`。
/// 発火先（Nostr broadcast / Discord channel）は `session_id` 接頭辞から導くので**列に
/// 持たない**（Discord 前提の列を一般化テーブルへ持ち込まない）。
///
/// 既定は**無効**（`enabled INTEGER NOT NULL DEFAULT 0` / fail-closed・#240）。
/// `interval_secs` は生値（`NULL` = 運用者既定）。`anchor_at`/`last_fired_at` は rfc3339 の
/// 壁時計（永続アンカー・#439）。**この PR では発火経路はまだ切り替えない**（PR2）。
///
/// **`SCHEMA_SQL` 側の同名ブロックと文面を一致させること**（新規 DB は SCHEMA_SQL 経由・
/// 既存 DB は v37 マイグレーション経由で同じ形に収束する）。
pub(super) const SESSION_HEARTBEAT_CONFIG_SQL: &str = "
CREATE TABLE IF NOT EXISTS session_heartbeat_config (
    agent_id      TEXT NOT NULL,
    session_id    TEXT NOT NULL,
    enabled       INTEGER NOT NULL DEFAULT 0,
    interval_secs INTEGER,
    anchor_at     TEXT,
    last_fired_at TEXT,
    updated_at    TEXT NOT NULL,
    PRIMARY KEY (agent_id, session_id)
);
";

/// per-agent 定時実行（#455 の PR1 スキーマ）。cron / `@every` をセッション時刻源へ載せる。
///
/// 既定は**無効**（fail-closed・#240）。`session_id` は注入先の一本化されたセッション
/// （Nostr agent は `nostr-{agent}`）。`next_run_at` は計算結果キャッシュで真実は再計算。
/// jitter は列を作らない（設計 §9・非採用）。**発火（scheduler 配線）は PR4** で、この PR は
/// 表の新設のみ（既存挙動は 1 バイトも変わらない＝積むものが無い）。
///
/// **⚠️ これは v37 の凍結履歴（旧列名 `last_run_at` / `next_run_at`）。書き換えない。**
/// PR4（#455）で語彙を heartbeat に揃えたため、**最終形は v38 の
/// [`migrate_v38_align_schedule_vocab`] が作る**（`last_fired_at`・`next_run_at` 撤去）。
/// 新規 DB の最終形は `SCHEMA_SQL` 側（そちらは新列名）。この定数を変えると v37 の履歴が
/// ずれる（既存 DB は v37 でこの形を経由してから v38 で収束する）。
pub(super) const AGENT_SCHEDULES_SQL: &str = "
CREATE TABLE IF NOT EXISTS agent_schedules (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id     TEXT NOT NULL,
    session_id   TEXT NOT NULL,
    cron_expr    TEXT NOT NULL,
    timezone     TEXT NOT NULL DEFAULT 'Asia/Tokyo',
    message      TEXT NOT NULL,
    enabled      INTEGER NOT NULL DEFAULT 0,
    anchor_at    TEXT,
    last_run_at  TEXT,
    next_run_at  TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_agent_schedules_agent ON agent_schedules(agent_id);
";

/// per-agent の MCP サーバ設定。1 エージェント × 複数サーバ（主キー (agent_id, name)）。
pub(super) const AGENT_MCP_CONFIG_SQL: &str = "
CREATE TABLE IF NOT EXISTS agent_mcp_config (
    agent_id TEXT NOT NULL,
    name TEXT NOT NULL,
    command TEXT NOT NULL,
    args_json TEXT NOT NULL DEFAULT '[]',
    env_json TEXT NOT NULL DEFAULT '{}',
    trusted_only INTEGER NOT NULL DEFAULT 1,
    enabled INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (agent_id, name)
);
";

/// スキル利用のセッション単位記録（スリープ棚卸しの弱い利用ヒント用）。
/// 注入時ではなく「利用が検出された時」に記録する（名前一致ベース, ノイズあり）。
pub(super) const SKILL_USAGE_LOG_SQL: &str = "
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
pub(super) const PROVIDER_SETTINGS_SQL: &str = "
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
pub(super) const TASK_LEDGER_SQL: &str = r#"
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

/// 新規インストール用のスキーマ定義（全て `CREATE ... IF NOT EXISTS`）。
///
/// 注意: baseline 済みの既存DB（`user_version >= 1`）では、この `SCHEMA_SQL` は
/// **再実行されない**。したがってここにテーブル/列を追加しただけでは既存DBには反映されない。
/// 新しいテーブル/列は、必ず対応する番号付きマイグレーションを [`MIGRATIONS`] にも追加して
/// 既存DBへ届けること。
pub(super) const SCHEMA_SQL: &str = r#"
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
    web_search INTEGER,
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
-- #546: session_id を先頭に引くクエリ用（list_recent_session_logs_of_type /
-- list_recent_user_speech_logs / list_recent_session_logs 等）。上の
-- idx_memory_sessions_session は (agent_id, session_id) で**先頭が agent_id** のため、
-- 共有チャンネルセッション（session_id に agent を含まず複数 agent が同居 / #404・#508）を
-- agent_id 無しで引くこれらは全表 SCAN だった。id を 3 列目に含め、(session_id, log_type)
-- 検索と `ORDER BY id ... LIMIT` を 1 本で満たす。既存 DB へは migration v39 で届ける。
CREATE INDEX IF NOT EXISTS idx_memory_sessions_session_type ON memory_sessions(session_id, log_type, id);

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
    -- 作成時の caller の trust class（'owner' / 'trusted' / 'agent'）。NULL = この列より
    -- 前に作られた既存スキル（legacy grandfather = Owner 相当扱い）。read_skill が
    -- 「強いターンが弱いスキルを借りる」confused deputy を塞ぐために参照する（#335）。
    created_caller TEXT,
    -- caller=Agent のターン（＝素の Agent 権限で走る run。外部 Nostr の受信ターンが
    -- 典型例だが、判定軸は transport ではなく **caller=Agent** である）に、この skill を
    -- index（system prompt）へ出し read_skill の本文を渡してよいか。既定 0 = 見せない
    -- （fail-closed）。オーナーがダッシュボード（REST）で少数だけ 1 に切り替える。
    -- Owner / CoAgent / TrustedUser の見え方には影響しない（従来どおり全部見える）。issue #352。
    agent_visible INTEGER NOT NULL DEFAULT 0,
    last_used_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_skills_agent ON skills(agent_id);
CREATE INDEX IF NOT EXISTS idx_skills_active ON skills(agent_id, is_active);

-- ============================================
-- Impressions: 心象
-- ============================================
-- スコープは **agent × target**（#314）。同じ相手なら Discord でも Nostr でも
-- 同じ 1 行を更新・参照する（「同じ人は同じ人」）。`session_id` は
-- 「**最後に更新されたセッション**」の記録として残す（時系列の辿り先）。
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
    UNIQUE(agent_id, target_id)
);

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
    -- この bot 自身の Discord user id（#489）。**発言者識別子 → agent UUID の逆引き**に使う
    -- （co_agent 判定）。書くのは gateway 起動時の**自分自身の認証済み接続**（`get_current_user`）
    -- だけで、config 構造体・upsert・REST 設定 API のどれからも書けない（外部が仕込めない）。
    -- 既定の空文字は「未接続 = 逆引き不可 = fail-closed」を意味する。
    bot_user_id TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL
);

-- ============================================
-- エージェント別 Nostr sub-gateway 設定（秘密鍵は per-agent 隔離）
-- ============================================
CREATE TABLE IF NOT EXISTS agent_nostr_config (
    agent_id TEXT PRIMARY KEY,
    secret_key TEXT NOT NULL,
    relays_json TEXT NOT NULL DEFAULT '[]',
    filter_json TEXT NOT NULL DEFAULT '{}',
    enabled INTEGER NOT NULL DEFAULT 0,
    -- Nostr 経路のオーナー識別子（#319）。`agent_discord_config.owner_discord_id` の
    -- Nostr 版。**64 桁小文字 hex に正規化して保存**し、既定の空文字は「オーナー未設定
    -- ＝誰もオーナーにならない」を意味する（fail-closed）。
    owner_pubkey TEXT NOT NULL DEFAULT '',
    -- この agent 自身の Nostr pubkey（#489。64 桁小文字 hex）。**発言者 pubkey → agent UUID の
    -- 逆引き**に使う（co_agent 判定）。書くのは gateway 起動時に自分の secret_key から導出した
    -- pubkey（`nostaro pubkey`）と identity 切替時の新 pubkey **だけ**で、config 構造体・upsert・
    -- REST 設定 API のどれからも書けない。既定の空文字は「未接続 = 逆引き不可 = fail-closed」。
    self_pubkey TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL
);

-- ============================================
-- エージェント別 Nostr 受信 → Discord 転記先（issue #252 段階 A）
-- 既定は無効（行があっても enabled=0 なら転記しない / fail-closed）
-- ============================================
CREATE TABLE IF NOT EXISTS agent_nostr_relay_config (
    agent_id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL DEFAULT 0,
    webhook_url TEXT,
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
    node_type TEXT NOT NULL CHECK (node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly','category','meta','unit')),
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
-- `idx_memory_index_nodes_short_id`（short_id 列参照）は **ここに置かない**。migrate() が張る。
-- 不変条件: SCHEMA_SQL には「migrate() が後から ALTER で足す列」を参照する index を置かないこと。
-- 理由: baseline 経路（initialize / user_version<1）は fresh でない旧 DB にも SCHEMA_SQL を流す。
-- `CREATE TABLE IF NOT EXISTS` は既存表を skip するので、旧 DB では列が増えないまま index が走り
-- 「no such column」で落ちる（#475/#476）。列を保証する migrate() 側で張れば旧 DB でも安全（#475）。

-- ============================================
-- 記憶インデックス: カテゴリ層メンバー（topic ↔ category の参照, issue #313）
-- ============================================
-- 時系列ツリー（root→period→session→topic）の parent_id を壊さないため、topic の
-- カテゴリ所属は parent 軸ではなく**参照**で持つ。PK は `(agent_id, topic_id, category_id)`
-- の多対多（issue #358）。1 topic は複数の関心にまたがるのでタグは複数付けられる。
-- node への FK は張らない（category/meta を切り戻しで消しても member 行が残るだけで
-- 害が無く、join 側で解決する。追記的・可逆を優先）。
-- ※ 既存 DB は v26 で同じ形に収束する（[`MEMORY_CATEGORY_MEMBERS_MM_SQL`] と文面一致）。
CREATE TABLE IF NOT EXISTS memory_category_members (
    agent_id TEXT NOT NULL,
    topic_id TEXT NOT NULL,
    category_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (agent_id, topic_id, category_id)
);
CREATE INDEX IF NOT EXISTS idx_memory_category_members_cat ON memory_category_members(agent_id, category_id);

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
-- 信頼済みユーザー（経路ごとの識別子空間）
-- ============================================
-- 旧名は `trusted_discord_users` / `discord_user_id`。Discord 以外の経路（web / rest）も
-- 同じ表を使うので #159 (v17) で改名した。旧DBは v17 の RENAME で追従する。
CREATE TABLE IF NOT EXISTS trusted_users (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  permission TEXT NOT NULL DEFAULT 'user',
  created_by TEXT NOT NULL DEFAULT 'owner',
  created_at TEXT NOT NULL,
  display_name TEXT NOT NULL DEFAULT '',
  -- その識別子が「どの経路のものか」（#214）。列追加前の行は全て Discord の
  -- 識別子空間なので DEFAULT 'discord'（`pending_interactions.platform` の前例に倣う）。
  -- 一意制約は (user_id, agent_id) のまま据え置く（作り直しは非可逆なので #159 の最終段）。
  platform TEXT NOT NULL DEFAULT 'discord',
  UNIQUE (user_id, agent_id)
);
CREATE INDEX IF NOT EXISTS idx_trusted_users_agent ON trusted_users(agent_id);

-- ============================================
-- エージェント別メモリインデックス設定
-- ============================================
CREATE TABLE IF NOT EXISTS agent_memory_index_config (
    agent_id TEXT PRIMARY KEY,
    batch_size INTEGER NOT NULL DEFAULT 50,
    threshold INTEGER NOT NULL DEFAULT 20,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_skill_consolidation_at TEXT,
    last_organize_at TEXT,
    organize_backlog_cursor TEXT,
    organize_last_run_at TEXT,
    memory_declare_cursor TEXT,
    memory_declare_window TEXT,
    memory_condense_cursor TEXT
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
-- AGENT INBOX: 外部イベント受信箱（webhook intake / issue #454）
-- ============================================
-- 外部 source（第一号: omoikane）の出来事を受け取り、heartbeat 外の専用ループが
-- 消化するまで積んでおく。処理は `processed_at` を刻んで記録する（NULL = 未処理）。
-- webhook は at-most-once なので、停止中に落ちたイベントは catch-up ポーリングが
-- 補充する。同じ出来事が webhook と catch-up の両方から来ても二重に積まないよう、
-- source アダプタが払い出す `dedup_key`（source 内で一意。例: コメント id）に UNIQUE を
-- 張り、投入は `INSERT OR IGNORE` で行う。
CREATE TABLE IF NOT EXISTS agent_inbox (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    source TEXT NOT NULL,
    event_type TEXT NOT NULL,
    dedup_key TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    received_at TEXT NOT NULL DEFAULT (datetime('now')),
    processed_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_inbox_dedup
    ON agent_inbox(source, dedup_key);
CREATE INDEX IF NOT EXISTS idx_agent_inbox_unprocessed
    ON agent_inbox(agent_id, processed_at);

-- ============================================
-- SESSION HEARTBEAT CONFIG: セッション単位ハートビート（統合スケジューラ / #439 × #456）
-- ============================================
-- agent/channel 二本立てを畳んだ後継。発火先は session_id 接頭辞から導くので列に持たない。
-- 既定は無効（fail-closed / #240）。この PR では発火経路はまだ切り替えない（PR2）。
-- ※ SESSION_HEARTBEAT_CONFIG_SQL 定数と文面を一致させること。
CREATE TABLE IF NOT EXISTS session_heartbeat_config (
    agent_id      TEXT NOT NULL,
    session_id    TEXT NOT NULL,
    enabled       INTEGER NOT NULL DEFAULT 0,
    interval_secs INTEGER,
    anchor_at     TEXT,
    last_fired_at TEXT,
    updated_at    TEXT NOT NULL,
    PRIMARY KEY (agent_id, session_id)
);

-- ============================================
-- AGENT SCHEDULES: per-agent 定時実行（#455）
-- ============================================
-- cron / @every をセッション時刻源へ載せる。既定は無効（fail-closed / #240）。
-- 発火（scheduler 配線）は PR4。この PR は表の新設のみ。
-- ※ 語彙は heartbeat に揃える（last_fired_at・next は照会時算出でキャッシュ列なし / v38・#455）。
--   AGENT_SCHEDULES_SQL 定数は v37 の凍結履歴（旧列名）なので、ここは定数と一致しない。
--   既存 DB は v38 の ALTER で同じ最終形へ収束する（parity テストで固定）。
CREATE TABLE IF NOT EXISTS agent_schedules (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id      TEXT NOT NULL,
    session_id    TEXT NOT NULL,
    cron_expr     TEXT NOT NULL,
    timezone      TEXT NOT NULL DEFAULT 'Asia/Tokyo',
    message       TEXT NOT NULL,
    enabled       INTEGER NOT NULL DEFAULT 0,
    anchor_at     TEXT,
    last_fired_at TEXT,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_agent_schedules_agent ON agent_schedules(agent_id);

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
