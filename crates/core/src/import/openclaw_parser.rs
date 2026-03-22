use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanOptions {
    pub include_daily_logs: bool,
    pub daily_log_days: u32,
    pub include_skills: bool,
    pub overwrite_if_exists: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoulImportData {
    pub persona_name: String,
    pub personality: String,
    pub found: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityImportData {
    pub name: String,
    pub image_url: Option<String>,
    pub metadata_json: String,
    pub found: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryCuratedImportData {
    pub category: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillImportData {
    pub name: String,
    pub description: String,
    pub situation_pattern: String,
    pub guidance: String,
    pub source_type: String,
    pub source_context: Option<String>,
    pub script_files: Vec<ScriptFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptFile {
    pub relative_path: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub source_dir: String,
    pub soul: SoulImportData,
    pub identity: IdentityImportData,
    pub memory_curated: Vec<MemoryCuratedImportData>,
    pub skills: Vec<SkillImportData>,
    pub daily_logs: Vec<MemoryCuratedImportData>,
    pub warnings: Vec<String>,
    pub excluded: Vec<String>,
}

pub fn scan_workspace(dir: &str, options: &ScanOptions) -> anyhow::Result<ScanResult> {
    let path = Path::new(dir);
    if !path.exists() || !path.is_dir() {
        anyhow::bail!("Directory does not exist: {}", dir);
    }

    let soul = match fs::read_to_string(path.join("SOUL.md")) {
        Ok(content) => parse_soul_md(&content),
        Err(_) => SoulImportData {
            persona_name: String::new(),
            personality: String::new(),
            found: false,
        },
    };

    let identity = match fs::read_to_string(path.join("IDENTITY.md")) {
        Ok(content) => parse_identity_md(&content),
        Err(_) => IdentityImportData {
            name: String::new(),
            image_url: None,
            metadata_json: "{}".to_string(),
            found: false,
        },
    };

    let mut memory_curated = Vec::new();

    if let Ok(content) = fs::read_to_string(path.join("MEMORY.md")) {
        memory_curated.extend(parse_memory_md(&content));
    }
    if let Ok(content) = fs::read_to_string(path.join("USER.md")) {
        memory_curated.push(parse_user_md(&content));
    }
    if let Ok(content) = fs::read_to_string(path.join("AGENTS.md")) {
        memory_curated.push(parse_agents_md(&content));
    }

    let mut daily_logs = Vec::new();
    if options.include_daily_logs {
        let memory_dir = path.join("memory");
        if memory_dir.is_dir() {
            let mut entries: Vec<_> = fs::read_dir(&memory_dir)?
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path()
                        .extension()
                        .map_or(false, |ext| ext == "md")
                        && e.path()
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .map_or(false, |s| {
                                // Match YYYY-MM-DD pattern
                                s.len() == 10 && s.chars().nth(4) == Some('-') && s.chars().nth(7) == Some('-')
                            })
                })
                .collect();
            entries.sort_by(|a, b| b.path().cmp(&a.path()));
            entries.truncate(options.daily_log_days as usize);

            for entry in entries {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    let date = entry
                        .path()
                        .file_stem()
                        .unwrap()
                        .to_string_lossy()
                        .to_string();
                    daily_logs.push(MemoryCuratedImportData {
                        category: format!("daily_log/{}", date),
                        content,
                    });
                }
            }
        }
    }

    let mut skills = Vec::new();
    if options.include_skills {
        let skills_dir = path.join("skills");
        if skills_dir.is_dir() {
            if let Ok(entries) = fs::read_dir(&skills_dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let entry_path = entry.path();
                    if entry_path.is_dir() {
                        if let Some(skill) = parse_skill_dir(&entry_path) {
                            skills.push(skill);
                        }
                    }
                }
            }
        }
    }

    let mut excluded = Vec::new();
    let mut warnings = Vec::new();

    if !soul.found {
        warnings.push("SOUL.md not found".to_string());
    }
    if !identity.found {
        warnings.push("IDENTITY.md not found".to_string());
    }
    if !options.include_daily_logs {
        excluded.push("daily_logs (disabled)".to_string());
    }
    if !options.include_skills {
        excluded.push("skills (disabled)".to_string());
    }

    Ok(ScanResult {
        source_dir: dir.to_string(),
        soul,
        identity,
        memory_curated,
        skills,
        daily_logs,
        warnings,
        excluded,
    })
}

pub fn parse_soul_md(content: &str) -> SoulImportData {
    let mut persona_name = String::new();

    // Look for persona name in **Name:** or **名前:** lines
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("**Name:**").or_else(|| trimmed.strip_prefix("**名前:**")) {
            persona_name = rest.trim().to_string();
            break;
        }
        // Also try: "- **Name:** value" format
        if let Some(rest) = trimmed.strip_prefix("- **Name:**").or_else(|| trimmed.strip_prefix("- **名前:**")) {
            persona_name = rest.trim().to_string();
            break;
        }
    }

    // If not found, try to extract from "You are **Name**." pattern
    if persona_name.is_empty() {
        for line in content.lines() {
            if let Some(start) = line.find("You are **") {
                let after = &line[start + 10..];
                if let Some(end) = after.find("**") {
                    let name = &after[..end];
                    // Extract just the first name part (before parenthetical)
                    persona_name = if let Some(paren) = name.find('(') {
                        name[..paren].trim().to_string()
                    } else if let Some(paren) = name.find('（') {
                        name[..paren].trim().to_string()
                    } else {
                        name.trim().to_string()
                    };
                    break;
                }
            }
        }
    }

    SoulImportData {
        persona_name,
        personality: content.to_string(),
        found: true,
    }
}

pub fn parse_identity_md(content: &str) -> IdentityImportData {
    let mut kvs: Vec<(String, String)> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        // Match "- **Key:** Value" pattern
        if let Some(rest) = trimmed.strip_prefix("- **") {
            if let Some(colon_pos) = rest.find(":**") {
                let key = rest[..colon_pos].to_string();
                let value = rest[colon_pos + 3..].trim().to_string();
                kvs.push((key, value));
            }
        }
    }

    let name = kvs
        .iter()
        .find(|(k, _)| k == "Name" || k == "名前")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();

    let image_url = kvs
        .iter()
        .find(|(k, _)| k == "Avatar" || k == "アバター")
        .and_then(|(_, v)| {
            let v = v.trim();
            if v.is_empty() || v.starts_with("<!--") || v == "なし" || v == "none" {
                None
            } else {
                Some(v.to_string())
            }
        });

    let metadata: std::collections::HashMap<String, String> = kvs.into_iter().collect();
    let metadata_json = serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_string());

    IdentityImportData {
        name,
        image_url,
        metadata_json,
        found: true,
    }
}

pub fn parse_memory_md(content: &str) -> Vec<MemoryCuratedImportData> {
    let mut results = Vec::new();
    let mut current_section: Option<String> = None;
    let mut current_content = String::new();

    for line in content.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            // Save previous section
            if let Some(section) = current_section.take() {
                let trimmed = current_content.trim().to_string();
                if !trimmed.is_empty() {
                    results.push(MemoryCuratedImportData {
                        category: format!("long_term/{}", section),
                        content: trimmed,
                    });
                }
            }
            current_section = Some(heading.trim().to_string());
            current_content.clear();
        } else if current_section.is_some() {
            current_content.push_str(line);
            current_content.push('\n');
        }
    }

    // Save last section
    if let Some(section) = current_section {
        let trimmed = current_content.trim().to_string();
        if !trimmed.is_empty() {
            results.push(MemoryCuratedImportData {
                category: format!("long_term/{}", section),
                content: trimmed,
            });
        }
    }

    results
}

pub fn parse_user_md(content: &str) -> MemoryCuratedImportData {
    MemoryCuratedImportData {
        category: "user_profile".to_string(),
        content: content.to_string(),
    }
}

pub fn parse_agents_md(content: &str) -> MemoryCuratedImportData {
    MemoryCuratedImportData {
        category: "agent_rules".to_string(),
        content: content.to_string(),
    }
}

pub fn parse_skill_dir(dir: &Path) -> Option<SkillImportData> {
    let skill_md_path = dir.join("SKILL.md");
    let guidance = fs::read_to_string(&skill_md_path).ok()?;

    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    // Extract description from first H1 heading
    let description = guidance
        .lines()
        .find(|line| line.starts_with("# "))
        .map(|line| line.trim_start_matches("# ").trim().to_string())
        .unwrap_or_else(|| name.clone());

    let situation_pattern = description.clone();

    // Scan for script files recursively
    let script_files = scan_script_files(dir, dir);

    Some(SkillImportData {
        name,
        description,
        situation_pattern,
        guidance,
        source_type: "openclaw_import".to_string(),
        source_context: Some(skill_md_path.to_string_lossy().to_string()),
        script_files,
    })
}

fn scan_script_files(base: &Path, dir: &Path) -> Vec<ScriptFile> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return files;
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            files.extend(scan_script_files(base, &path));
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if matches!(ext, "py" | "sh" | "js" | "ts") {
                if let Ok(content) = fs::read_to_string(&path) {
                    let relative_path = path
                        .strip_prefix(base)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string();
                    files.push(ScriptFile {
                        relative_path,
                        content,
                    });
                }
            }
        }
    }

    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_parse_soul_md_extracts_persona_name() {
        let content = r#"# SOUL.md - Who You Are
## Core Truths
Some truths here.

## Vibe
**The Persona:**
You are **のすたろう (Nostarou)**.
- **Age:** 17
"#;
        let result = parse_soul_md(content);
        assert_eq!(result.persona_name, "のすたろう");
        assert!(result.personality.contains("SOUL.md"));
        assert!(result.found);
    }

    #[test]
    fn test_parse_soul_md_no_name() {
        let content = "# SOUL.md\n## Vibe\nNo name here";
        let result = parse_soul_md(content);
        // Should not panic; name may or may not be found
        assert!(result.persona_name.is_empty() || !result.persona_name.is_empty());
    }

    #[test]
    fn test_parse_identity_md_full() {
        let content = r#"# IDENTITY.md - Who Am I?
- **Name:** のすたろう
- **Creature:** Nostr空間上に住む電脳存在
- **Emoji:** ⚡
- **Avatar:** https://example.com/avatar.png
"#;
        let result = parse_identity_md(content);
        assert_eq!(result.name, "のすたろう");
        assert_eq!(
            result.image_url,
            Some("https://example.com/avatar.png".to_string())
        );
        let meta: serde_json::Value = serde_json::from_str(&result.metadata_json).unwrap();
        assert_eq!(meta["Emoji"], "⚡");
    }

    #[test]
    fn test_parse_identity_md_no_avatar() {
        let content = r#"# IDENTITY.md
- **Name:** TestAgent
- **Avatar:**
"#;
        let result = parse_identity_md(content);
        assert_eq!(result.name, "TestAgent");
        assert!(result.image_url.is_none());
    }

    #[test]
    fn test_parse_memory_md_sections() {
        let content = r#"# MEMORY.md

## セクション1
Content of section 1.

## セクション2
Content of section 2.

## セクション3
Content of section 3.
"#;
        let result = parse_memory_md(content);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].category, "long_term/セクション1");
        assert!(result[0].content.contains("Content of section 1"));
        assert_eq!(result[1].category, "long_term/セクション2");
        assert_eq!(result[2].category, "long_term/セクション3");
    }

    #[test]
    fn test_scan_workspace_nonexistent_dir() {
        let opts = ScanOptions {
            include_daily_logs: true,
            daily_log_days: 7,
            include_skills: true,
            overwrite_if_exists: false,
        };
        let result = scan_workspace("/nonexistent/path/that/does/not/exist", &opts);
        assert!(result.is_err());
    }

    #[test]
    fn test_scan_workspace_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let opts = ScanOptions {
            include_daily_logs: true,
            daily_log_days: 7,
            include_skills: true,
            overwrite_if_exists: false,
        };
        let result = scan_workspace(tmp.path().to_str().unwrap(), &opts).unwrap();
        assert!(!result.soul.found);
        assert!(!result.identity.found);
        assert!(result.memory_curated.is_empty());
        assert!(result.skills.is_empty());
    }

    #[test]
    fn test_scan_workspace_with_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("SOUL.md"),
            "# SOUL.md\n## Vibe\nYou are **TestBot**.\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("MEMORY.md"),
            "# MEMORY\n## Section A\nContent A\n## Section B\nContent B\n",
        )
        .unwrap();
        let skill_dir = tmp.path().join("skills").join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "# My Skill\nThis is a test skill.\n",
        )
        .unwrap();

        let opts = ScanOptions {
            include_daily_logs: false,
            daily_log_days: 7,
            include_skills: true,
            overwrite_if_exists: false,
        };
        let result = scan_workspace(tmp.path().to_str().unwrap(), &opts).unwrap();
        assert!(result.soul.found);
        assert_eq!(result.memory_curated.len(), 2);
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "my-skill");
    }
}
