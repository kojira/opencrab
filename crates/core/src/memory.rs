use anyhow::Result;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};
use tracing;

use opencrab_db::queries;

/// Manages curated memories and session logs for an agent.
///
/// The MemoryManager wraps a shared database connection and provides
/// high-level operations for storing, retrieving, and searching memories.
#[derive(Debug, Clone)]
pub struct MemoryManager {
    agent_id: String,
    conn: Arc<Mutex<Connection>>,
}

impl MemoryManager {
    /// Create a new MemoryManager for the given agent.
    pub fn new(agent_id: impl Into<String>, conn: Arc<Mutex<Connection>>) -> Self {
        Self {
            agent_id: agent_id.into(),
            conn,
        }
    }

    /// Get all curated memories, optionally filtered by category.
    pub fn get_curated(&self, category: Option<&str>) -> Result<Vec<CuratedMemory>> {
        let conn = self.conn.lock().unwrap();
        let rows = if let Some(cat) = category {
            queries::get_curated_memories(&conn, &self.agent_id, cat)?
        } else {
            queries::list_curated_memories(&conn, &self.agent_id)?
        };

        Ok(rows
            .into_iter()
            .map(|row| CuratedMemory {
                id: row.id,
                category: row.category,
                content: row.content,
            })
            .collect())
    }

    /// Save or update a curated memory.
    pub fn save_curated(&self, id: &str, category: &str, content: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        queries::upsert_curated_memory(
            &conn,
            &queries::CuratedMemoryRow {
                id: id.to_string(),
                agent_id: self.agent_id.clone(),
                category: category.to_string(),
                content: content.to_string(),
            },
        )?;
        tracing::debug!(agent_id = %self.agent_id, category = %category, "Saved curated memory");
        Ok(())
    }

    /// Append a log entry to the session log.
    pub fn append_session_log(
        &self,
        session_id: &str,
        log_type: &str,
        content: &str,
        speaker_id: Option<&str>,
        turn_number: Option<i32>,
        metadata: Option<serde_json::Value>,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let row_id = queries::insert_session_log(
            &conn,
            &queries::SessionLogRow {
                id: None,
                agent_id: self.agent_id.clone(),
                session_id: session_id.to_string(),
                log_type: log_type.to_string(),
                content: content.to_string(),
                speaker_id: speaker_id.map(|s| s.to_string()),
                turn_number,
                metadata_json: metadata.map(|m| serde_json::to_string(&m).unwrap_or_default()),
            },
        )?;
        tracing::debug!(
            agent_id = %self.agent_id,
            session_id = %session_id,
            log_type = %log_type,
            "Appended session log"
        );
        Ok(row_id)
    }

    /// Search session logs using full-text search.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let conn = self.conn.lock().unwrap();
        let results = queries::search_session_logs(&conn, &self.agent_id, query, limit)?;
        Ok(results
            .into_iter()
            .map(|r| SearchResult {
                id: r.id,
                session_id: r.session_id,
                log_type: r.log_type,
                content: r.content,
                created_at: r.created_at,
                score: r.score,
            })
            .collect())
    }

    /// Build a context string summarizing the agent's curated memories for LLM prompts.
    pub fn build_context(&self) -> Result<String> {
        let memories = self.get_curated(None)?;
        if memories.is_empty() {
            return Ok(String::new());
        }

        let mut ctx = String::from("## Curated Memories\n\n");
        let mut current_category = String::new();

        for mem in &memories {
            if mem.category != current_category {
                ctx.push_str(&format!("### {}\n", mem.category));
                current_category = mem.category.clone();
            }
            ctx.push_str(&format!("- {}\n", mem.content));
        }

        Ok(ctx)
    }
}

/// A curated memory entry.
#[derive(Debug, Clone)]
pub struct CuratedMemory {
    pub id: String,
    pub category: String,
    pub content: String,
}

/// A search result from session log full-text search.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub id: i64,
    pub session_id: String,
    pub log_type: String,
    pub content: String,
    pub created_at: String,
    pub score: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_mm() -> MemoryManager {
        let conn = opencrab_db::init_memory().unwrap();
        MemoryManager::new("agent-test", Arc::new(Mutex::new(conn)))
    }

    #[test]
    fn test_save_and_get_curated() {
        let mm = test_mm();
        mm.save_curated("m1", "facts", "The sky is blue").unwrap();
        let memories = mm.get_curated(Some("facts")).unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].content, "The sky is blue");
        assert_eq!(memories[0].category, "facts");
    }

    #[test]
    fn test_append_and_search_session_log() {
        let mm = test_mm();
        mm.append_session_log("s1", "message", "Rust is great", None, Some(1), None).unwrap();
        mm.append_session_log("s1", "message", "Python is fine", None, Some(2), None).unwrap();
        mm.append_session_log("s1", "message", "Java is verbose", None, Some(3), None).unwrap();

        let results = mm.search("Rust", 10).unwrap();
        assert!(!results.is_empty());
        assert!(results.iter().any(|r| r.content.contains("Rust")));
    }

    #[test]
    fn test_build_context_empty() {
        let mm = test_mm();
        let ctx = mm.build_context().unwrap();
        assert!(ctx.is_empty());
    }

    #[test]
    fn test_build_context_with_data() {
        let mm = test_mm();
        mm.save_curated("m1", "facts", "Water is wet").unwrap();
        let ctx = mm.build_context().unwrap();
        assert!(ctx.contains("Curated Memories"));
        assert!(ctx.contains("Water is wet"));
    }
}
