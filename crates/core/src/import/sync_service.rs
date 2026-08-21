//! OpenClaw -> OpenCrab 増分同期サービス
//!
//! MEMORY.md のセクション単位の差分検知と daily_log ファイルの新規追加を担当する。

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use opencrab_db::queries::{
    get_latest_sync_at, get_sync_state, list_sync_states, upsert_curated_memory, upsert_sync_state,
    CuratedMemoryRow, SyncStateRow,
};

// ============================================
// オプション
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncOptions {
    pub include_daily_logs: bool,
    pub daily_log_days: u32,
    pub force_resync: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            include_daily_logs: true,
            daily_log_days: 30,
            force_resync: false,
        }
    }
}

// ============================================
// ステータス確認結果
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionDetail {
    pub section: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMdChanges {
    pub status: String,
    pub sections_total: usize,
    pub sections_new: usize,
    pub sections_updated: usize,
    pub sections_unchanged: usize,
    pub details: Vec<SectionDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyLogChanges {
    pub total_files: usize,
    pub new_files: usize,
    pub already_synced: usize,
    pub new_file_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStatusResult {
    pub agent_id: String,
    pub source_dir: String,
    pub last_sync_at: Option<String>,
    pub memory_md_changes: MemoryMdChanges,
    pub daily_log_changes: DailyLogChanges,
    pub has_changes: bool,
}

// ============================================
// 同期実行結果
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResult {
    pub agent_id: String,
    pub synced_at: String,
    pub memory_md_upserted: usize,
    pub memory_md_skipped: usize,
    pub daily_logs_imported: usize,
    pub daily_logs_skipped: usize,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

// ============================================
// ハッシュ計算
// ============================================

fn hash_content(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn hash_section(heading: &str, body: &str) -> String {
    let combined = format!("{}|{}", heading, body);
    hash_content(&combined)
}

// ============================================
// MEMORY.md パース
// ============================================

struct Section {
    heading: String,
    body: String,
}

fn parse_memory_sections(content: &str) -> Vec<Section> {
    let mut sections = Vec::new();
    let mut current_heading: Option<String> = None;
    let mut current_body = Vec::new();

    for line in content.lines() {
        if let Some(stripped) = line.strip_prefix("## ") {
            if let Some(heading) = current_heading.take() {
                sections.push(Section {
                    heading,
                    body: current_body.join("\n"),
                });
                current_body.clear();
            }
            current_heading = Some(stripped.trim().to_string());
        } else if current_heading.is_some() {
            current_body.push(line.to_string());
        }
    }

    if let Some(heading) = current_heading {
        sections.push(Section {
            heading,
            body: current_body.join("\n"),
        });
    }

    sections
}

// ============================================
// source_dir 検証
// ============================================

fn validate_source_dir(source_dir: &str) -> Result<PathBuf> {
    let path = Path::new(source_dir);
    if !path.is_absolute() {
        anyhow::bail!("source_dir must be an absolute path: {}", source_dir);
    }
    if !path.exists() {
        anyhow::bail!("source_dir does not exist: {}", source_dir);
    }
    if !path.is_dir() {
        anyhow::bail!("source_dir is not a directory: {}", source_dir);
    }
    Ok(path.canonicalize()?)
}

// ============================================
// daily_log ファイル列挙
// ============================================

fn list_daily_log_files(source_path: &Path, days: u32) -> Result<Vec<String>> {
    let memory_dir = source_path.join("memory");
    if !memory_dir.exists() {
        return Ok(vec![]);
    }

    let cutoff = if days > 0 {
        let today = chrono::Utc::now().date_naive();
        Some(today - chrono::Duration::days(days as i64))
    } else {
        None
    };

    let mut files = Vec::new();
    for entry in std::fs::read_dir(&memory_dir)? {
        let entry = entry?;
        let file_name = entry.file_name().to_string_lossy().to_string();

        if !file_name.ends_with(".md") {
            continue;
        }

        let stem = &file_name[..file_name.len() - 3];
        let Ok(date) = chrono::NaiveDate::parse_from_str(stem, "%Y-%m-%d") else {
            continue;
        };

        if let Some(cutoff_date) = cutoff {
            if date < cutoff_date {
                continue;
            }
        }

        files.push(format!("memory/{}", file_name));
    }

    files.sort();
    Ok(files)
}

// ============================================
// 同期状態チェック（プレビュー）
// ============================================

pub fn check_sync_status(
    conn: &Connection,
    agent_id: &str,
    source_dir: &str,
    options: &SyncOptions,
) -> Result<SyncStatusResult> {
    let source_path = validate_source_dir(source_dir)?;
    let source_dir_canonical = source_path.to_string_lossy().to_string();

    let last_sync_at = get_latest_sync_at(conn, agent_id)?;

    let memory_md_path = source_path.join("MEMORY.md");
    let memory_md_changes = if memory_md_path.exists() {
        let content = std::fs::read_to_string(&memory_md_path)?;
        let sections = parse_memory_sections(&content);
        let mut details = Vec::new();
        let mut sections_new = 0;
        let mut sections_updated = 0;
        let mut sections_unchanged = 0;

        for section in &sections {
            let file_name = format!("MEMORY.md::{}", section.heading);
            let new_hash = hash_section(&section.heading, &section.body);

            let state = get_sync_state(conn, agent_id, &source_dir_canonical, &file_name)?;
            let (status, prev_hash) = match state {
                None => {
                    sections_new += 1;
                    ("new".to_string(), None)
                }
                Some(s) if s.content_hash != new_hash => {
                    sections_updated += 1;
                    ("updated".to_string(), Some(s.content_hash))
                }
                Some(_) => {
                    sections_unchanged += 1;
                    ("unchanged".to_string(), None)
                }
            };

            details.push(SectionDetail {
                section: section.heading.clone(),
                status,
                prev_hash,
                new_hash: Some(new_hash),
            });
        }

        let status = if sections_new + sections_updated > 0 {
            "changed"
        } else {
            "unchanged"
        };

        MemoryMdChanges {
            status: status.to_string(),
            sections_total: sections.len(),
            sections_new,
            sections_updated,
            sections_unchanged,
            details,
        }
    } else {
        MemoryMdChanges {
            status: "not_found".to_string(),
            sections_total: 0,
            sections_new: 0,
            sections_updated: 0,
            sections_unchanged: 0,
            details: vec![],
        }
    };

    let daily_log_changes = if options.include_daily_logs {
        let files = list_daily_log_files(&source_path, options.daily_log_days)?;
        let mut new_files = Vec::new();
        let mut already_synced = 0;

        for file_name in &files {
            let state = get_sync_state(conn, agent_id, &source_dir_canonical, file_name)?;
            if state.is_none() {
                new_files.push(file_name.clone());
            } else if !options.force_resync {
                let full_path = source_path.join(file_name);
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    let hash = hash_content(&content);
                    if state.as_ref().map(|s| s.content_hash.as_str()) == Some(hash.as_str()) {
                        already_synced += 1;
                    } else {
                        new_files.push(file_name.clone());
                    }
                } else {
                    already_synced += 1;
                }
            } else {
                new_files.push(file_name.clone());
            }
        }

        DailyLogChanges {
            total_files: files.len(),
            new_files: new_files.len(),
            already_synced,
            new_file_names: new_files,
        }
    } else {
        DailyLogChanges {
            total_files: 0,
            new_files: 0,
            already_synced: 0,
            new_file_names: vec![],
        }
    };

    let has_changes = memory_md_changes.sections_new + memory_md_changes.sections_updated > 0
        || daily_log_changes.new_files > 0;

    Ok(SyncStatusResult {
        agent_id: agent_id.to_string(),
        source_dir: source_dir_canonical,
        last_sync_at,
        memory_md_changes,
        daily_log_changes,
        has_changes,
    })
}

// ============================================
// 同期実行
// ============================================

const MAX_FILE_SIZE: u64 = 1024 * 1024;

pub fn execute_sync(
    conn: &Connection,
    agent_id: &str,
    source_dir: &str,
    options: &SyncOptions,
) -> Result<SyncResult> {
    let source_path = validate_source_dir(source_dir)?;
    let source_dir_canonical = source_path.to_string_lossy().to_string();
    let synced_at = Utc::now().to_rfc3339();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    let mut memory_md_upserted = 0;
    let mut memory_md_skipped = 0;
    let mut daily_logs_imported = 0;
    let mut daily_logs_skipped = 0;

    let memory_md_path = source_path.join("MEMORY.md");
    if memory_md_path.exists() {
        let metadata = std::fs::metadata(&memory_md_path)?;
        if metadata.len() > MAX_FILE_SIZE {
            warnings.push(format!(
                "MEMORY.md is too large ({}B > 1MB), skipping",
                metadata.len()
            ));
        } else {
            let content = std::fs::read_to_string(&memory_md_path)?;
            let sections = parse_memory_sections(&content);

            for section in sections {
                let file_name = format!("MEMORY.md::{}", section.heading);
                let new_hash = hash_section(&section.heading, &section.body);

                let state = get_sync_state(conn, agent_id, &source_dir_canonical, &file_name)?;
                let should_sync = match &state {
                    None => true,
                    Some(s) => options.force_resync || s.content_hash != new_hash,
                };

                if should_sync {
                    let category = format!("long_term/{}", section.heading);
                    let row = CuratedMemoryRow {
                        id: Uuid::new_v4().to_string(),
                        agent_id: agent_id.to_string(),
                        category,
                        content: section.body.trim().to_string(),
                        created_at: String::new(),
                    };
                    match upsert_curated_memory(conn, &row) {
                        Ok(_) => {
                            let sync_row = SyncStateRow {
                                id: Uuid::new_v4().to_string(),
                                agent_id: agent_id.to_string(),
                                source_dir: source_dir_canonical.clone(),
                                file_type: "memory_md".to_string(),
                                file_name,
                                content_hash: new_hash,
                                synced_at: synced_at.clone(),
                                created_at: synced_at.clone(),
                            };
                            upsert_sync_state(conn, &sync_row)?;
                            memory_md_upserted += 1;
                        }
                        Err(e) => {
                            errors.push(format!(
                                "Failed to upsert section {}: {}",
                                section.heading, e
                            ));
                        }
                    }
                } else {
                    memory_md_skipped += 1;
                }
            }
        }
    }

    if options.include_daily_logs {
        let files = list_daily_log_files(&source_path, options.daily_log_days)?;

        for file_name in files {
            let full_path = source_path.join(&file_name);

            let metadata = match std::fs::metadata(&full_path) {
                Ok(m) => m,
                Err(e) => {
                    warnings.push(format!("Cannot stat {}: {}", file_name, e));
                    continue;
                }
            };
            if metadata.len() > MAX_FILE_SIZE {
                warnings.push(format!(
                    "{} is too large ({}B > 1MB), skipping",
                    file_name,
                    metadata.len()
                ));
                daily_logs_skipped += 1;
                continue;
            }

            let content = match std::fs::read_to_string(&full_path) {
                Ok(c) => c,
                Err(e) => {
                    warnings.push(format!("Cannot read {}: {}", file_name, e));
                    continue;
                }
            };
            let new_hash = hash_content(&content);

            let state = get_sync_state(conn, agent_id, &source_dir_canonical, &file_name)?;
            let should_sync = match &state {
                None => true,
                Some(s) => options.force_resync || s.content_hash != new_hash,
            };

            if should_sync {
                let date_part = file_name
                    .strip_prefix("memory/")
                    .and_then(|s| s.strip_suffix(".md"))
                    .unwrap_or(&file_name);
                let category = format!("daily_log/{}", date_part);

                let row = CuratedMemoryRow {
                    id: Uuid::new_v4().to_string(),
                    agent_id: agent_id.to_string(),
                    category,
                    content,
                    created_at: String::new(),
                };
                match upsert_curated_memory(conn, &row) {
                    Ok(_) => {
                        let sync_row = SyncStateRow {
                            id: Uuid::new_v4().to_string(),
                            agent_id: agent_id.to_string(),
                            source_dir: source_dir_canonical.clone(),
                            file_type: "daily_log".to_string(),
                            file_name,
                            content_hash: new_hash,
                            synced_at: synced_at.clone(),
                            created_at: synced_at.clone(),
                        };
                        upsert_sync_state(conn, &sync_row)?;
                        daily_logs_imported += 1;
                    }
                    Err(e) => {
                        errors.push(format!("Failed to import {}: {}", file_name, e));
                    }
                }
            } else {
                daily_logs_skipped += 1;
            }
        }
    }

    Ok(SyncResult {
        agent_id: agent_id.to_string(),
        synced_at,
        memory_md_upserted,
        memory_md_skipped,
        daily_logs_imported,
        daily_logs_skipped,
        warnings,
        errors,
    })
}

// ============================================
// 同期履歴
// ============================================

pub fn get_sync_history(
    conn: &Connection,
    agent_id: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<SyncStateRow>, i64)> {
    list_sync_states(conn, agent_id, limit, offset)
}

// ============================================
// テスト
// ============================================

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use tempfile::TempDir;

    fn test_conn() -> rusqlite::Connection {
        opencrab_db::init_memory().unwrap()
    }

    fn make_workspace(dir: &TempDir) -> String {
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        dir.path().to_string_lossy().to_string()
    }

    fn write_file(dir: &TempDir, path: &str, content: &str) {
        let full_path = dir.path().join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full_path, content).unwrap();
    }

    #[test]
    fn test_sync_skips_unchanged_files() {
        let conn = test_conn();
        let dir = TempDir::new().unwrap();
        let source_dir = make_workspace(&dir);

        write_file(&dir, "MEMORY.md", "## テスト\ntest content\n");
        write_file(&dir, "memory/2026-01-01.md", "day1 content");

        let opts = SyncOptions::default();

        let result = execute_sync(&conn, "agent-1", &source_dir, &opts).unwrap();
        assert_eq!(result.memory_md_upserted, 1);
        assert_eq!(result.daily_logs_imported, 0);

        let result2 = execute_sync(&conn, "agent-1", &source_dir, &opts).unwrap();
        assert_eq!(result2.memory_md_upserted, 0);
        assert_eq!(result2.memory_md_skipped, 1);
    }

    #[test]
    fn test_sync_updates_changed_sections() {
        let conn = test_conn();
        let dir = TempDir::new().unwrap();
        let source_dir = make_workspace(&dir);

        write_file(&dir, "MEMORY.md", "## セクションA\n初回内容\n");

        let opts = SyncOptions::default();
        let result1 = execute_sync(&conn, "agent-2", &source_dir, &opts).unwrap();
        assert_eq!(result1.memory_md_upserted, 1);

        write_file(&dir, "MEMORY.md", "## セクションA\n変更後の内容\n");
        let result2 = execute_sync(&conn, "agent-2", &source_dir, &opts).unwrap();
        assert_eq!(result2.memory_md_upserted, 1);
        assert_eq!(result2.memory_md_skipped, 0);
    }

    #[test]
    fn test_sync_adds_new_daily_logs_only() {
        let conn = test_conn();
        let dir = TempDir::new().unwrap();
        let source_dir = make_workspace(&dir);

        let today = chrono::Utc::now().date_naive();
        let file_name = format!("memory/{}.md", today.format("%Y-%m-%d"));
        write_file(&dir, &file_name, "today log");

        let opts = SyncOptions::default();
        let result1 = execute_sync(&conn, "agent-3", &source_dir, &opts).unwrap();
        assert_eq!(result1.daily_logs_imported, 1);

        let result2 = execute_sync(&conn, "agent-3", &source_dir, &opts).unwrap();
        assert_eq!(result2.daily_logs_imported, 0);
        assert_eq!(result2.daily_logs_skipped, 1);
    }

    #[test]
    fn test_sync_is_idempotent() {
        let conn = test_conn();
        let dir = TempDir::new().unwrap();
        let source_dir = make_workspace(&dir);

        write_file(&dir, "MEMORY.md", "## 冪等テスト\nコンテンツ\n");

        let opts = SyncOptions::default();

        for _ in 0..3 {
            let result = execute_sync(&conn, "agent-4", &source_dir, &opts).unwrap();
            assert!(result.errors.is_empty());
        }

        let (memories, _) =
            opencrab_db::queries::list_curated_memories(&conn, "agent-4", 100, 0).unwrap();
        let categories: Vec<_> = memories.iter().map(|m| m.category.as_str()).collect();
        let unique_cats: HashSet<_> = categories.iter().copied().collect();
        assert_eq!(
            categories.len(),
            unique_cats.len(),
            "重複カテゴリがある: {:?}",
            categories
        );
    }

    #[test]
    fn test_sync_does_not_delete_removed_sections() {
        let conn = test_conn();
        let dir = TempDir::new().unwrap();
        let source_dir = make_workspace(&dir);

        write_file(
            &dir,
            "MEMORY.md",
            "## 残るセクション\n内容\n## 削除されるセクション\n内容2\n",
        );

        let opts = SyncOptions::default();
        execute_sync(&conn, "agent-5", &source_dir, &opts).unwrap();

        write_file(&dir, "MEMORY.md", "## 残るセクション\n内容\n");
        execute_sync(&conn, "agent-5", &source_dir, &opts).unwrap();

        let (memories, _) =
            opencrab_db::queries::list_curated_memories(&conn, "agent-5", 100, 0).unwrap();
        assert_eq!(memories.len(), 2, "削除されたセクションも残っているはず");
    }

    #[test]
    fn test_check_sync_status() {
        let conn = test_conn();
        let dir = TempDir::new().unwrap();
        let source_dir = make_workspace(&dir);

        write_file(&dir, "MEMORY.md", "## ステータステスト\nコンテンツ\n");

        let opts = SyncOptions::default();
        let status = check_sync_status(&conn, "agent-6", &source_dir, &opts).unwrap();
        assert!(status.has_changes);
        assert_eq!(status.memory_md_changes.sections_new, 1);

        execute_sync(&conn, "agent-6", &source_dir, &opts).unwrap();
        let status2 = check_sync_status(&conn, "agent-6", &source_dir, &opts).unwrap();
        assert!(!status2.has_changes);
    }
}
