use crate::Result;
use rusqlite::Connection;

pub(crate) fn create_phase1_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys=ON;
        CREATE TABLE gate_instances(
          instance_id TEXT NOT NULL PRIMARY KEY,
          kind_id TEXT NOT NULL CHECK(kind_id IN ('discord','nostr','web','rest')),
          label TEXT NOT NULL,
          owner_subject_id INTEGER,
          active_revision INTEGER NOT NULL,
          lifecycle TEXT NOT NULL CHECK(lifecycle IN ('stopped','starting','running','stopping'))
        );
        CREATE TABLE subjects(
          id INTEGER NOT NULL PRIMARY KEY,
          kind TEXT NOT NULL CHECK(kind IN ('human','agent')),
          public_id TEXT NOT NULL UNIQUE,
          display_name TEXT NOT NULL,
          created_at INTEGER NOT NULL,
          CHECK(
            (public_id <> '' AND instr(public_id,':')=0
             AND public_id NOT GLOB '*[^A-Za-z0-9_-]*')
            OR (substr(public_id,1,17)='external:discord:' AND length(public_id)>17
                AND substr(public_id,18) NOT GLOB '*[^A-Za-z0-9_-]*')
            OR (substr(public_id,1,15)='external:nostr:' AND length(public_id)>15
                AND substr(public_id,16) NOT GLOB '*[^A-Za-z0-9_-]*')
            OR (substr(public_id,1,13)='external:web:' AND length(public_id)>13
                AND substr(public_id,14) NOT GLOB '*[^A-Za-z0-9_-]*')
            OR (substr(public_id,1,14)='external:rest:' AND length(public_id)>14
                AND substr(public_id,15) NOT GLOB '*[^A-Za-z0-9_-]*')
          )
        );
        CREATE TABLE subject_profiles(
          subject_id INTEGER NOT NULL,
          revision INTEGER NOT NULL,
          persona_name TEXT NOT NULL,
          persona TEXT NOT NULL,
          instructions TEXT NOT NULL,
          default_heartbeat_instructions TEXT NOT NULL,
          job_title TEXT,
          organization TEXT,
          image_url TEXT,
          metadata TEXT,
          updated_at INTEGER NOT NULL,
          PRIMARY KEY(subject_id,revision)
        );
        CREATE TABLE subject_runtime_configs(
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
        CREATE TABLE gate_subject_identities(
          instance_id TEXT NOT NULL,
          external_id TEXT NOT NULL,
          subject_id INTEGER NOT NULL,
          display_name TEXT,
          PRIMARY KEY(instance_id,external_id)
        );
        CREATE TABLE grant_sets(
          agent_subject_id INTEGER NOT NULL,
          revision INTEGER NOT NULL,
          created_at INTEGER NOT NULL,
          PRIMARY KEY(agent_subject_id,revision)
        );
        CREATE TABLE agent_grants(
          grant_set_revision INTEGER NOT NULL,
          grant_set_subject_id INTEGER NOT NULL,
          principal_subject_id INTEGER NOT NULL,
          role TEXT NOT NULL CHECK(role IN ('owner','owner_equivalent','trusted','agent')),
          scope TEXT NOT NULL,
          PRIMARY KEY(grant_set_subject_id,grant_set_revision,principal_subject_id,role,scope)
        );
        CREATE TABLE grant_actions(
          grant_set_revision INTEGER NOT NULL,
          grant_set_subject_id INTEGER NOT NULL,
          principal_subject_id INTEGER NOT NULL,
          action TEXT NOT NULL,
          PRIMARY KEY(grant_set_subject_id,grant_set_revision,principal_subject_id,action)
        );
        CREATE TABLE grant_source_provenance(
          agent_subject_id INTEGER NOT NULL,
          principal_subject_id INTEGER NOT NULL,
          gate_kind TEXT,
          external_id TEXT NOT NULL,
          source_permission TEXT,
          source_allowed_actions TEXT,
          source_record_key TEXT,
          created_by TEXT NOT NULL,
          created_at INTEGER NOT NULL
        );
        CREATE TABLE migration_provenance(
          target_entity TEXT NOT NULL,
          target_key BLOB NOT NULL,
          source_database_digest BLOB NOT NULL CHECK(length(source_database_digest)=32),
          source_locator TEXT NOT NULL CHECK(source_locator <> ''),
          source_key BLOB NOT NULL,
          source_row_digest BLOB NOT NULL CHECK(length(source_row_digest)=32),
          PRIMARY KEY(target_entity,target_key,source_locator,source_key)
        );
        CREATE TABLE legacy_unowned_source_rows(
          source_db TEXT NOT NULL,
          source_table TEXT NOT NULL,
          source_key BLOB NOT NULL,
          row_values BLOB NOT NULL,
          reason TEXT NOT NULL CHECK(reason <> ''),
          PRIMARY KEY(source_db,source_table,source_key)
        );
        "#,
    )?;
    Ok(())
}
