use super::*;
use rusqlite::Connection;

include!("tests/support.rs");
include!("tests/access_gate_migrations_v44_v45.rs");
include!("tests/baseline_compatibility.rs");
include!("tests/early_version_migrations.rs");
include!("tests/impressions_migration_v21.rs");
include!("tests/late_version_migrations_v41_v42_v46.rs");
include!("tests/legacy_foundation_migrations.rs");
include!("tests/maintenance_cleanup_migration_v33.rs");
include!("tests/memory_index_migrations.rs");
include!("tests/nostr_skills_config_migrations.rs");
include!("tests/routing_migration_v32.rs");
include!("tests/runner_contracts.rs");
include!("tests/schedule_migrations_v37_v38.rs");
include!("tests/schema_contracts.rs");
include!("tests/transplant_migration_v43.rs");
include!("tests/v47_copy_rehearsal.rs");
