//! process-local live registry。startup は空。DB から復元しない。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use opencrab_db::Db;
use tokio::net::unix::OwnedWriteHalf;

use crate::bearer::OperatorToken;
use crate::error::{ErrorCode, GateError};

/// hello 済みで未 close の接続。
pub struct LiveEntry {
    pub identity: u64,
    pub revision: u64,
    pub writer: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    pub acknowledged: HashSet<String>,
    pub pending: HashMap<String, Pending>,
}

pub enum Pending {
    Bind {
        binding_id: String,
        started: Instant,
    },
    Say {
        delivery_id: String,
    },
}

impl Pending {
    pub fn is_bind(&self) -> bool {
        matches!(self, Self::Bind { .. })
    }

    pub fn is_say(&self) -> bool {
        matches!(self, Self::Say { .. })
    }

    pub fn binding_id(&self) -> Option<&str> {
        match self {
            Self::Bind { binding_id, .. } => Some(binding_id),
            Self::Say { .. } => None,
        }
    }

    pub fn delivery_id(&self) -> Option<&str> {
        match self {
            Self::Say { delivery_id } => Some(delivery_id),
            Self::Bind { .. } => None,
        }
    }
}

#[derive(Default)]
pub struct Registry {
    live: HashMap<String, LiveEntry>,
}

impl Registry {
    pub fn get(&self, instance_id: &str) -> Option<&LiveEntry> {
        self.live.get(instance_id)
    }

    pub fn get_mut(&mut self, instance_id: &str) -> Option<&mut LiveEntry> {
        self.live.get_mut(instance_id)
    }

    pub fn is_live(&self, instance_id: &str) -> bool {
        self.live.contains_key(instance_id)
    }

    pub fn insert(&mut self, instance_id: String, entry: LiveEntry) {
        self.live.insert(instance_id, entry);
    }

    /// 同じ `connection_identity` の entry だけ消す。
    pub fn remove_if_identity(&mut self, instance_id: &str, identity: u64) -> Option<LiveEntry> {
        match self.live.get(instance_id) {
            Some(e) if e.identity == identity => self.live.remove(instance_id),
            _ => None,
        }
    }

    pub fn pending_say_ids(&self, instance_id: &str) -> Vec<String> {
        self.live
            .get(instance_id)
            .map(|e| {
                e.pending
                    .values()
                    .filter_map(|p| p.delivery_id().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// conformance 用の計測と failure injection。本番は読まない。
#[derive(Default)]
pub struct GateProbe {
    pub accept_inbound_count: AtomicUsize,
    pub start_session_turn_count: AtomicUsize,
    pub lookup_resolve_count: AtomicUsize,
    pub lookup_dm_any_count: AtomicUsize,
    pub lookup_dm_count: AtomicUsize,
    pub lookup_wl_count: AtomicUsize,
    pub fail_reply_log: AtomicBool,
    pub fail_delivery_insert: AtomicBool,
    pub fail_say_write: AtomicBool,
    pub whitelist_override: Mutex<Option<bool>>,
}

pub struct ExtgateState {
    pub db: Db,
    pub registry: Mutex<Registry>,
    pub token: OperatorToken,
    pub halt: AtomicBool,
    next_identity: AtomicU64,
    pub probe: GateProbe,
}

impl ExtgateState {
    pub fn new(db: Db, token: OperatorToken) -> Self {
        Self {
            db,
            registry: Mutex::new(Registry::default()),
            token,
            halt: AtomicBool::new(false),
            next_identity: AtomicU64::new(1),
            probe: GateProbe::default(),
        }
    }

    pub fn lock_registry(&self) -> Result<MutexGuard<'_, Registry>, GateError> {
        match self.registry.lock() {
            Ok(g) => Ok(g),
            Err(_) => {
                self.halt.store(true, std::sync::atomic::Ordering::SeqCst);
                Err(GateError::new(ErrorCode::StoreError))
            }
        }
    }

    pub fn alloc_identity(&self) -> u64 {
        self.next_identity
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn halt(&self) {
        self.halt.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_halted(&self) -> bool {
        self.halt.load(std::sync::atomic::Ordering::SeqCst)
    }
}
