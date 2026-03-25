//! システム全体のログラッパー。

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "debug" => Some(LogLevel::Debug),
            "info" => Some(LogLevel::Info),
            "warn" => Some(LogLevel::Warn),
            "error" => Some(LogLevel::Error),
            _ => None,
        }
    }
}

static GLOBAL_LOG_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Info as u8);

pub fn get_log_level() -> LogLevel {
    match GLOBAL_LOG_LEVEL.load(Ordering::Relaxed) {
        0 => LogLevel::Debug,
        1 => LogLevel::Info,
        2 => LogLevel::Warn,
        _ => LogLevel::Error,
    }
}

pub fn set_log_level(level: LogLevel) {
    GLOBAL_LOG_LEVEL.store(level as u8, Ordering::Relaxed);
    tracing::info!("Log level changed to: {}", level.as_str());
}

pub fn agent_log(
    db: &Arc<Mutex<rusqlite::Connection>>,
    agent_id: Option<&str>,
    level: LogLevel,
    context: &str,
    message: &str,
) {
    if level < get_log_level() {
        return;
    }
    match level {
        LogLevel::Debug => tracing::debug!(agent_id = ?agent_id, context = %context, "{}", message),
        LogLevel::Info => tracing::info!(agent_id = ?agent_id, context = %context, "{}", message),
        LogLevel::Warn => tracing::warn!(agent_id = ?agent_id, context = %context, "{}", message),
        LogLevel::Error => tracing::error!(agent_id = ?agent_id, context = %context, "{}", message),
    }
    if let Ok(conn) = db.lock() {
        let row = opencrab_db::queries::AgentLogRow {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: agent_id.map(String::from),
            level: level.as_str().to_string(),
            context: context.to_string(),
            message: message.to_string(),
            created_at: Some(chrono::Utc::now().to_rfc3339()),
        };
        let _ = opencrab_db::queries::insert_agent_log(&conn, &row);
    }
}
