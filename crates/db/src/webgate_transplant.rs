//! 既存 web セッション → `extgate-{binding_id}` への 14 store 一回移送。
//! 件数/digest 不一致は中止。V3 wire は触らない。

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

include!("webgate_transplant/store_catalog.rs");
include!("webgate_transplant/snapshot_validation.rs");
include!("webgate_transplant/transplant.rs");

#[cfg(test)]
#[path = "webgate_transplant/tests.rs"]
mod tests;
