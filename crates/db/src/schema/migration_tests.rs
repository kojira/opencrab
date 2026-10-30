use super::*;
use rusqlite::Connection;

include!("tests/initialize.rs");
include!("tests/runner_contracts.rs");
include!("tests/production_v62_to_latest.rs");
include!("tests/gate_admin_drop_migration_v64.rs");
include!("tests/sessions_only_migration_v65.rs");
