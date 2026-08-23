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
        CREATE TABLE gate_instance_revisions(
          instance_id TEXT NOT NULL,
          revision INTEGER NOT NULL,
          present INTEGER NOT NULL,
          enabled INTEGER NOT NULL,
          created_at INTEGER NOT NULL,
          config_schema_id TEXT NOT NULL,
          config_bytes BLOB NOT NULL,
          config_digest BLOB NOT NULL CHECK(length(config_digest)=32),
          secret_set_id TEXT,
          PRIMARY KEY(instance_id,revision)
        );
        CREATE TABLE secret_sets(
          secret_set_id TEXT NOT NULL PRIMARY KEY,
          revision INTEGER NOT NULL,
          scope TEXT NOT NULL,
          created_at INTEGER NOT NULL
        );
        CREATE TABLE secret_values(
          secret_set_id TEXT NOT NULL,
          name TEXT NOT NULL,
          at_rest_format TEXT NOT NULL CHECK(at_rest_format IN ('source-plaintext','enc:v1','opaque')),
          value BLOB NOT NULL,
          value_digest BLOB NOT NULL CHECK(length(value_digest)=32),
          PRIMARY KEY(secret_set_id,name)
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
        CREATE TABLE places(
          id INTEGER NOT NULL PRIMARY KEY,
          parent_id INTEGER,
          inherit_from_place_id INTEGER,
          inherit_up_to_seq INTEGER,
          policy BLOB NOT NULL,
          public_key TEXT NOT NULL UNIQUE,
          created_at INTEGER NOT NULL,
          closed_at INTEGER,
          close_reason TEXT
        );
        CREATE TABLE place_source_refs(
          place_id INTEGER NOT NULL,
          classification TEXT NOT NULL CHECK(classification IN ('live','legacy_general','child','config_only')),
          source_system TEXT NOT NULL,
          source_address TEXT NOT NULL,
          source_id BLOB NOT NULL,
          source_record_digest BLOB,
          mode TEXT,
          theme TEXT,
          phase TEXT,
          source_turn_number INTEGER,
          source_status TEXT,
          participant_public_ids TEXT,
          facilitator_subject_id INTEGER,
          source_done_count INTEGER,
          source_max_turns INTEGER,
          metadata TEXT,
          updated_at INTEGER NOT NULL,
          UNIQUE(source_system,source_address)
        );
        CREATE TABLE place_default_policies(
          default_id TEXT NOT NULL PRIMARY KEY,
          place_id INTEGER,
          kind_id TEXT NOT NULL,
          resolution TEXT NOT NULL,
          source_row BLOB,
          source_updated_at INTEGER,
          policy_schema_id TEXT,
          policy_bytes BLOB,
          policy_digest BLOB
        );
        CREATE UNIQUE INDEX one_active_place_default
          ON place_default_policies(place_id,kind_id) WHERE resolution='active';
        CREATE TABLE place_subject_policies(
          place_id INTEGER NOT NULL,
          kind_id TEXT NOT NULL,
          subject_id INTEGER NOT NULL,
          admission TEXT NOT NULL,
          readable INTEGER NOT NULL,
          writable INTEGER NOT NULL,
          whitelisted INTEGER NOT NULL,
          heartbeat_enabled INTEGER NOT NULL,
          heartbeat_interval_secs INTEGER,
          heartbeat_instructions TEXT NOT NULL,
          instructions_revision INTEGER NOT NULL,
          source_row BLOB NOT NULL,
          source_updated_at INTEGER NOT NULL,
          PRIMARY KEY(place_id,kind_id,subject_id)
        );
        CREATE TABLE memberships(
          place_id INTEGER NOT NULL,
          subject_id INTEGER NOT NULL,
          role TEXT NOT NULL CHECK(role IN ('participant','observer')),
          joined_at INTEGER NOT NULL,
          shared_seen_seq INTEGER NOT NULL,
          PRIMARY KEY(place_id,subject_id)
        );
        CREATE TABLE external_origin_scopes(
          scope_id TEXT NOT NULL PRIMARY KEY,
          kind_id TEXT NOT NULL,
          address TEXT NOT NULL,
          mode TEXT NOT NULL CHECK(mode IN ('instance','kind_address')),
          instance_id TEXT,
          binding_id TEXT,
          place_id INTEGER NOT NULL
        );
        CREATE TABLE gate_bindings(
          binding_id TEXT NOT NULL PRIMARY KEY,
          place_id INTEGER NOT NULL,
          instance_id TEXT NOT NULL,
          address TEXT NOT NULL,
          label TEXT,
          origin_scope_id TEXT NOT NULL,
          binding_metadata_schema_id TEXT NOT NULL,
          binding_metadata_bytes BLOB NOT NULL,
          binding_metadata_digest BLOB NOT NULL CHECK(length(binding_metadata_digest)=32),
          catch_up_start TEXT,
          closed_at INTEGER,
          close_reason TEXT
        );
        CREATE TABLE subject_routes(
          subject_id INTEGER NOT NULL,
          place_id INTEGER NOT NULL,
          kind_id TEXT NOT NULL,
          purpose TEXT NOT NULL,
          binding_id TEXT NOT NULL,
          PRIMARY KEY(subject_id,place_id,kind_id,purpose)
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
        CREATE TABLE events(
          place_id INTEGER NOT NULL,
          seq INTEGER NOT NULL,
          kind TEXT NOT NULL,
          author_subject_id INTEGER,
          author_external_id TEXT,
          content BLOB NOT NULL,
          reply_to_seq INTEGER,
          target_seq INTEGER,
          for_subject_id INTEGER,
          created_at INTEGER NOT NULL,
          attachments BLOB NOT NULL,
          PRIMARY KEY(place_id,seq)
        );
        CREATE TABLE private_journal(
          journal_id INTEGER NOT NULL PRIMARY KEY,
          owner_subject_id INTEGER NOT NULL,
          place_id INTEGER NOT NULL,
          anchor_seq INTEGER NOT NULL,
          content BLOB NOT NULL,
          created_at INTEGER NOT NULL,
          provenance BLOB NOT NULL
        );
        CREATE TABLE legacy_history_archive(
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
        CREATE TABLE legacy_audit_records(
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
        CREATE TABLE subject_history_sources(
          subject_id INTEGER NOT NULL,
          live_place_id INTEGER NOT NULL,
          history_place_id INTEGER NOT NULL,
          ordinal INTEGER NOT NULL,
          history_max_seq INTEGER NOT NULL,
          PRIMARY KEY(subject_id,live_place_id,history_place_id),
          UNIQUE(subject_id,live_place_id,ordinal)
        );
        CREATE TABLE interactions(
          id INTEGER NOT NULL PRIMARY KEY,
          owner_subject_id INTEGER NOT NULL,
          place_id INTEGER NOT NULL,
          binding_id TEXT,
          surface TEXT NOT NULL,
          source_address TEXT NOT NULL,
          source_message_id TEXT,
          surface_id TEXT NOT NULL,
          surface_payload TEXT NOT NULL,
          payload TEXT NOT NULL,
          owner_only INTEGER NOT NULL,
          timeout_secs INTEGER NOT NULL,
          state TEXT NOT NULL CHECK(state IN ('pending','responded','expired')),
          source_record_key TEXT,
          created_at INTEGER NOT NULL,
          updated_at INTEGER NOT NULL,
          deadline INTEGER NOT NULL
        );
        CREATE TABLE interaction_responses(
          interaction_id INTEGER NOT NULL PRIMARY KEY,
          interaction_source_key TEXT,
          response TEXT NOT NULL,
          responder_kind TEXT NOT NULL CHECK(responder_kind IN ('subject','system','unknown')),
          responder_subject_id INTEGER,
          responder_external_id TEXT NOT NULL,
          responded_at INTEGER NOT NULL
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
        CREATE TABLE schedule_source_state(
          owner_subject_id INTEGER NOT NULL,
          enabled INTEGER NOT NULL,
          raw_interval_secs INTEGER,
          source_updated_at INTEGER NOT NULL
        );
        CREATE TABLE webhook_endpoints(
          id INTEGER NOT NULL PRIMARY KEY,
          created_by TEXT,
          enabled INTEGER NOT NULL,
          endpoint TEXT NOT NULL,
          event_filter TEXT,
          kind TEXT NOT NULL,
          maximum_output_chars INTEGER NOT NULL,
          name TEXT,
          output_mode TEXT NOT NULL,
          owner_subject_id INTEGER NOT NULL,
          scope TEXT NOT NULL,
          tool_name TEXT NOT NULL,
          updated_at INTEGER NOT NULL
        );
        CREATE TABLE soul_presets(
          owner_subject_id INTEGER NOT NULL,
          name TEXT NOT NULL,
          persona_name TEXT NOT NULL,
          custom_traits TEXT,
          source_record_key TEXT,
          created_at INTEGER NOT NULL,
          updated_at INTEGER NOT NULL
        );
        CREATE TABLE llm_call_records(
          id INTEGER NOT NULL PRIMARY KEY,
          bot_iteration INTEGER NOT NULL,
          cache_creation_tokens INTEGER,
          cache_read_tokens INTEGER,
          completion_tokens INTEGER,
          created_at INTEGER,
          error_body TEXT,
          error_code TEXT,
          latency_ms INTEGER,
          model TEXT,
          model_id TEXT,
          owner_subject_id INTEGER NOT NULL,
          place_id INTEGER,
          prompt_tokens INTEGER,
          provider_id TEXT,
          request_body BLOB NOT NULL,
          requested_at INTEGER,
          response_body BLOB NOT NULL,
          source_record_key TEXT,
          tool_call_payload TEXT,
          total_tokens INTEGER,
          trigger_external_id TEXT
        );
        CREATE TABLE model_observations(
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
        CREATE TABLE tasks(
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
        CREATE TABLE offloads(
          activity_id INTEGER NOT NULL PRIMARY KEY,
          body BLOB NOT NULL,
          created_at INTEGER NOT NULL,
          place_id INTEGER NOT NULL,
          subject_id INTEGER NOT NULL,
          truncated INTEGER NOT NULL
        );
        "#,
    )?;
    Ok(())
}
