use super::*;
use rusqlite::Connection;

include!("migration_tests/support.rs");
include!("migration_tests/access_gate_migrations_v44_v45.rs");
include!("migration_tests/baseline_compatibility.rs");
include!("migration_tests/early_version_migrations.rs");
include!("migration_tests/impressions_migration_v21.rs");
include!("migration_tests/late_version_migrations_v41_v42_v46.rs");
include!("migration_tests/legacy_foundation_migrations.rs");
include!("migration_tests/maintenance_cleanup_migration_v33.rs");
include!("migration_tests/memory_index_migrations.rs");
include!("migration_tests/nostr_skills_config_migrations.rs");
include!("migration_tests/routing_migration_v32.rs");
include!("migration_tests/runner_contracts.rs");
include!("migration_tests/schedule_migrations_v37_v38.rs");
include!("migration_tests/schema_contracts.rs");
include!("migration_tests/transplant_migration_v43.rs");
