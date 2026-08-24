use crate::Result;
use rusqlite::Connection;

/// Migration-owned tables. `subject_profiles` / `subject_runtime_configs` /
/// `private_journal` は store SCHEMA にもある（runtime writer）。IF NOT EXISTS。
pub(crate) fn create_migration_owned_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS subject_profiles(
          subject_id INTEGER NOT NULL,
          revision INTEGER NOT NULL,
          persona_name TEXT NOT NULL,
          persona TEXT,
          instructions TEXT NOT NULL,
          default_heartbeat_instructions TEXT NOT NULL,
          job_title TEXT,
          organization TEXT,
          image_url TEXT,
          metadata TEXT,
          updated_at INTEGER NOT NULL,
          PRIMARY KEY(subject_id,revision)
        );
        CREATE TABLE IF NOT EXISTS subject_runtime_configs(
          subject_id INTEGER NOT NULL,
          revision INTEGER NOT NULL,
          created_at INTEGER NOT NULL,
          model_alias TEXT,
          reasoning_effort TEXT,
          web_search_enabled INTEGER,
          history_policy TEXT NOT NULL,
          output_policy TEXT NOT NULL,
          model_route_id TEXT,
          source_config BLOB,
          PRIMARY KEY(subject_id,revision)
        );
        CREATE TABLE IF NOT EXISTS grant_actions(
          grant_set_revision INTEGER NOT NULL,
          grant_set_subject_id INTEGER NOT NULL,
          principal_subject_id INTEGER NOT NULL,
          action TEXT NOT NULL,
          PRIMARY KEY(grant_set_subject_id,grant_set_revision,principal_subject_id,action)
        );
        CREATE TABLE IF NOT EXISTS private_journal(
          journal_id INTEGER NOT NULL PRIMARY KEY,
          owner_subject_id INTEGER NOT NULL,
          place_id INTEGER NOT NULL,
          anchor_seq INTEGER NOT NULL,
          content BLOB NOT NULL,
          created_at INTEGER NOT NULL,
          provenance BLOB NOT NULL
        );
        CREATE TABLE IF NOT EXISTS legacy_history_archive(
          source_db_digest BLOB NOT NULL,
          source_row_id INTEGER NOT NULL,
          source_agent_id BLOB NOT NULL,
          source_session_id BLOB NOT NULL,
          log_kind TEXT NOT NULL,
          content BLOB NOT NULL,
          speaker_source_id BLOB,
          source_turn_number INTEGER,
          metadata BLOB,
          created_at INTEGER NOT NULL,
          created_at_source BLOB NOT NULL,
          metadata_source BLOB NOT NULL,
          row_digest BLOB NOT NULL,
          owner_subject_id INTEGER NOT NULL,
          proposed_place_id INTEGER,
          owner_decision_revision TEXT,
          PRIMARY KEY(source_db_digest,source_row_id)
        );
        CREATE TABLE IF NOT EXISTS legacy_audit_records(
          source_db_digest BLOB NOT NULL,
          source_row_id INTEGER NOT NULL,
          audit_kind TEXT NOT NULL,
          owner_subject_id INTEGER NOT NULL,
          place_id INTEGER,
          activity_id INTEGER,
          caller_discord_id TEXT,
          caller_identity TEXT NOT NULL,
          content BLOB NOT NULL,
          created_at INTEGER NOT NULL,
          metadata BLOB,
          new_value TEXT,
          old_value TEXT,
          provenance BLOB NOT NULL,
          reason TEXT,
          scope TEXT NOT NULL,
          source_channel_id TEXT,
          PRIMARY KEY(source_db_digest,source_row_id,audit_kind)
        );
        CREATE TABLE IF NOT EXISTS subject_history_sources(
          subject_id INTEGER NOT NULL,
          live_place_id INTEGER NOT NULL,
          history_place_id INTEGER NOT NULL,
          ordinal INTEGER NOT NULL,
          history_max_seq INTEGER NOT NULL,
          PRIMARY KEY(subject_id,live_place_id,history_place_id),
          UNIQUE(subject_id,live_place_id,ordinal)
        );
        CREATE TABLE IF NOT EXISTS migration_provenance(
          target_entity TEXT NOT NULL,
          target_key BLOB NOT NULL,
          source_database_digest BLOB NOT NULL CHECK(length(source_database_digest)=32),
          source_locator TEXT NOT NULL CHECK(source_locator <> ''),
          source_key BLOB NOT NULL,
          source_row_digest BLOB NOT NULL CHECK(length(source_row_digest)=32),
          PRIMARY KEY(target_entity,target_key,source_locator,source_key)
        );
        CREATE TABLE IF NOT EXISTS schedule_source_state(
          owner_subject_id INTEGER NOT NULL,
          enabled INTEGER NOT NULL,
          raw_interval_secs INTEGER,
          source_updated_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS model_observations(
          id INTEGER NOT NULL PRIMARY KEY,
          created_at INTEGER NOT NULL,
          model TEXT,
          model_id TEXT,
          observation TEXT NOT NULL,
          owner_subject_id INTEGER NOT NULL,
          provider TEXT,
          provider_id TEXT,
          recommendation TEXT,
          situation TEXT NOT NULL,
          source_record_key TEXT,
          tags_json TEXT
        );
        CREATE TABLE IF NOT EXISTS tasks(
          id INTEGER NOT NULL PRIMARY KEY,
          contract TEXT,
          created_at INTEGER NOT NULL,
          goal TEXT NOT NULL,
          owner_subject_id INTEGER NOT NULL,
          place_id INTEGER NOT NULL,
          restart_count INTEGER NOT NULL,
          source_record_key INTEGER,
          state TEXT NOT NULL,
          updated_at INTEGER NOT NULL
        );
        "#,
    )?;
    Ok(())
}
