use rusqlite::Connection;

mod baseline;
mod helpers;
mod migrations;
mod sql;
mod v37_v42;
mod v43_v47;

use baseline::migrate;
use helpers::{column_exists, table_exists, table_has_rows};
use migrations::MIGRATION_GROUPS;
use sql::{
    AGENT_HEARTBEAT_CONFIG_SQL, AGENT_MCP_CONFIG_SQL, AGENT_NOSTR_CONFIG_SQL,
    AGENT_NOSTR_RELAY_CONFIG_SQL, MEMORY_CATEGORY_MEMBERS_MM_SQL, MEMORY_CATEGORY_MEMBERS_SQL,
    PROVIDER_SETTINGS_SQL, SCHEMA_SQL, SKILL_USAGE_LOG_SQL, TASK_LEDGER_SQL, TOOL_CONTINUATION_SQL,
};
use v37_v42::{migrate_v37_session_heartbeat, migrate_v38_align_schedule_vocab};
use v43_v47::{
    migrate_v43_transplant_schema, migrate_v44_extgate, migrate_v45_nostr_bundle_state,
    migrate_v47_gateway_operations,
};

#[cfg(test)]
use v37_v42::{norm_discord_id, session_id_is_valid};
#[cfg(test)]
use v43_v47::expected_v43_user_tables;

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
const MIGRATIONS: MigrationCatalog = MigrationCatalog(MIGRATION_GROUPS);

type MigrationIter =
    std::iter::Flatten<std::iter::Copied<std::slice::Iter<'static, &'static [Migration]>>>;

/// Copyable view over the ordered migration groups.
#[derive(Clone, Copy)]
struct MigrationCatalog(&'static [&'static [Migration]]);

impl MigrationCatalog {
    fn iter(self) -> MigrationIter {
        self.0.iter().copied().flatten()
    }

    #[cfg(test)]
    fn last(self) -> Option<&'static Migration> {
        self.iter().last()
    }
}

impl IntoIterator for MigrationCatalog {
    type Item = &'static Migration;
    type IntoIter = MigrationIter;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

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
        tx.execute_batch(TOOL_CONTINUATION_SQL)?;
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
fn run_migrations<'a, I>(conn: &Connection, migrations: I) -> rusqlite::Result<()>
where
    I: IntoIterator<Item = &'a Migration>,
    I::IntoIter: Clone,
{
    let current = schema_version(conn)?;
    let migrations = migrations.into_iter();
    let latest = migrations
        .clone()
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

#[cfg(test)]
mod migration_tests;
