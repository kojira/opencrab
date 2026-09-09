//! スキーマ定義の純データ（`SCHEMA_SQL` と各テーブルの const SQL 文字列）。
//!
//! `schema.rs` から機械的に切り出した純粋な文字列定数で、文面は 1 文字も変えていない（#518）。
//! 親モジュール（`schema`）の MIGRATIONS クロージャ / `migrate()` / `initialize` から参照される。

mod tool_continuation;
pub(super) use tool_continuation::TOOL_CONTINUATION_SQL;

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
/// 畳んだ後継。`session_id` は不透明な文字列（例: `nostr-{agent}` /
/// `discord-{agent}-{guild}-{channel}` / `web-{agent}-{conversation}`・列挙は固定しない）。
/// 発火先は各 transport の descriptor が `session_id` から導くので**列に持たない**（特定
/// transport 前提の列を一般化テーブルへ持ち込まない・#628）。
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
/// `migrate_v38_align_schedule_vocab` が作る**（`last_fired_at`・`next_run_at` 撤去）。
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

/// セッションに紐づく Nostr 購読（1 セッション N 行 / 載せ替え工程 3・v43）。
///
/// `SCHEMA_SQL` 側の同名ブロックと文面を揃えること（新規 DB は SCHEMA_SQL、
/// 既存 DB は v43 で同じ形に収束する）。
pub(super) const SESSION_WATCHES_SQL: &str = "
CREATE TABLE IF NOT EXISTS session_watches (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    interval_secs INTEGER NOT NULL,
    filter_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    CHECK (interval_secs > 0)
);
CREATE INDEX IF NOT EXISTS idx_session_watches_session ON session_watches(session_id);
";

/// ツール 1 実行 = 1 行（載せ替え工程 3・v43）。
///
/// `SCHEMA_SQL` 側の同名ブロックと文面を揃えること。
pub(super) const TOOL_LOGS_SQL: &str = "
CREATE TABLE IF NOT EXISTS tool_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    session_id TEXT,
    tool_name TEXT NOT NULL,
    args_json TEXT NOT NULL,
    outcome TEXT NOT NULL,
    result_text TEXT NOT NULL DEFAULT '',
    started_at TEXT,
    created_at TEXT DEFAULT (datetime('now')),
    latency_ms INTEGER,
    iteration INTEGER,
    CHECK (outcome IN ('done', 'failed', 'refused', 'deadline', 'stopped'))
);
CREATE INDEX IF NOT EXISTS idx_tool_logs_agent ON tool_logs(agent_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_tool_logs_session ON tool_logs(session_id);
";

/// 新規インストール用のスキーマ定義（全て `CREATE ... IF NOT EXISTS`）。
///
/// 注意: baseline 済みの既存DB（`user_version >= 1`）では、この `SCHEMA_SQL` は
/// **再実行されない**。したがってここにテーブル/列を追加しただけでは既存DBには反映されない。
/// 新しいテーブル/列は、必ず対応する番号付きマイグレーションを `MIGRATIONS` にも追加して
/// 既存DBへ届けること。
mod full;

pub(super) const SCHEMA_SQL: &str = full::SCHEMA_SQL;
