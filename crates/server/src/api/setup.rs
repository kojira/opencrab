//! オンボーディング（運営者の初回セットアップ）用 API。
//!
//! - `GET  /api/setup/status` — 4 ステップ（LLM プロバイダ / エージェント /
//!   Discord 接続 / チャンネル whitelist）の進捗を集約して返す。ダッシュボードの
//!   セットアップウィザードと Home のチェックリストが同じ形を読む。
//! - `POST /api/agents/{id}/skills/seed-standard` — `skills/*.skill.md` を読み、
//!   frontmatter をパースして標準スキルをそのエージェントにシードする。新規
//!   エージェントを作った直後に呼ぶことで「作ってすぐ使える」状態にする。
//!
//! 判定はすべて既存クエリの再利用で、config.toml を手編集せずダッシュボードだけで
//! 稼働エージェントを立てられる（= 設定ゼロでも動く）ことをゴールにする。

use std::path::PathBuf;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::json;

use crate::AppState;

/// 標準スキルの置き場所。既定は cwd 直下の `skills/`。デプロイ環境で場所が
/// 違う場合は環境変数 `OPENCRAB_SKILLS_DIR` で上書きできる。
fn skills_dir() -> PathBuf {
    std::env::var("OPENCRAB_SKILLS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("skills"))
}

/// GET /api/setup/status — オンボーディング進捗の集約。
///
/// 各ステップは `done`（完了したか）と補助情報（件数など）を持つ。`complete` は
/// 全ステップ完了、`next_step` は未完の最初のステップ（全完了なら null）。
pub async fn get_setup_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    // --- LLM プロバイダ: 現在のルーターに使えるプロバイダが 1 つ以上あるか ---
    // ルーターは TOML + DB オーバーライドから構築済み。登録がある = 実際に
    // LLM を呼べる状態。既定（hermit-shell 等）だけでも done になる。
    let router = state.llm_router.get();
    let active_providers = router.provider_names();
    let llm_done = !active_providers.is_empty();
    let llm_detail = active_providers.join(", ");

    // 以降は DB 参照。ロックは 1 度だけ取ってまとめて読む。
    let (agent_count, discord_configured, discord_enabled, channel_count) = {
        let conn = state.db.lock().unwrap();
        let agent_ids = opencrab_db::queries::list_agent_ids(&conn).unwrap_or_default();
        let agent_count = agent_ids.len();

        // per-agent Discord: bot_token が入っていれば「設定済み」。enabled は別途集計。
        let mut discord_configured = 0usize;
        let mut discord_enabled = 0usize;
        for aid in &agent_ids {
            if let Ok(Some(cfg)) = opencrab_db::queries::get_agent_discord_config(&conn, aid) {
                if !cfg.bot_token.trim().is_empty() {
                    discord_configured += 1;
                    if cfg.enabled {
                        discord_enabled += 1;
                    }
                }
            }
        }

        let channel_count = opencrab_db::queries::list_whitelisted_channels(&conn)
            .map(|v| v.len())
            .unwrap_or(0);

        (
            agent_count,
            discord_configured,
            discord_enabled,
            channel_count,
        )
    };

    let agent_done = agent_count > 0;
    let discord_done = discord_configured > 0;
    let channel_done = channel_count > 0;

    // 未完の最初のステップ（ウィザードの初期フォーカス先）。
    let next_step = if !llm_done {
        Some("llm_provider")
    } else if !agent_done {
        Some("agent")
    } else if !discord_done {
        Some("discord")
    } else if !channel_done {
        Some("channel")
    } else {
        None
    };
    let complete = next_step.is_none();

    Json(json!({
        "steps": {
            "llm_provider": { "done": llm_done, "detail": llm_detail, "count": active_providers.len() },
            "agent":        { "done": agent_done, "count": agent_count },
            "discord":      { "done": discord_done, "count": discord_configured, "enabled": discord_enabled },
            "channel":      { "done": channel_done, "count": channel_count },
        },
        "complete": complete,
        "next_step": next_step,
    }))
}

/// frontmatter からパースした標準スキル。
struct ParsedSkill {
    name: String,
    description: String,
    /// DB permission 文字列（例: `"agent"` を JSON クオートした `"\"agent\""`）。
    permission_db: String,
    /// このスキルが使うアクション名。
    actions: Vec<String>,
    /// frontmatter 以降の本文（ガイダンス）。
    body: String,
}

/// `skills/*.skill.md` の YAML frontmatter + 本文を最小限パースする。
///
/// 対応するのは書き込み側（skill_management.rs）が出力する範囲:
/// `name` / `description`（クオート可）/ `permission` / `actions`（ブロックリスト）。
/// 読み取り側パーサはこれまで存在しなかったため新規実装。serde_yaml は依存に
/// 無いので、この限定フォーマット向けの行ベースパーサで済ませる。
fn parse_skill_md(content: &str) -> Option<ParsedSkill> {
    let content = content.trim_start_matches('\u{feff}'); // BOM 除去
    let mut lines = content.lines();

    // 先頭の空行を読み飛ばし、最初の非空行が `---` であることを要求する。
    let opened = loop {
        match lines.next() {
            Some(l) if l.trim().is_empty() => continue,
            Some(l) => break l.trim() == "---",
            None => return None,
        }
    };
    if !opened {
        return None;
    }

    // 閉じの `---` までを frontmatter として集める。
    let mut fm_lines = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        fm_lines.push(line);
    }
    if !closed {
        return None;
    }
    // 残りが本文。
    let body = lines.collect::<Vec<_>>().join("\n").trim().to_string();

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut permission: Option<String> = None;
    let mut actions: Vec<String> = Vec::new();
    let mut in_actions = false;

    for line in fm_lines {
        // インデント行は actions リストの項目候補。
        if line.starts_with([' ', '\t']) {
            let t = line.trim();
            if in_actions {
                if let Some(item) = t.strip_prefix("- ") {
                    let v = item.trim().trim_matches('"').trim().to_string();
                    if !v.is_empty() {
                        actions.push(v);
                    }
                }
            }
            continue;
        }
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        in_actions = false;
        let Some((k, v)) = t.split_once(':') else {
            continue;
        };
        let key = k.trim();
        let val = v.trim().trim_matches('"').trim().to_string();
        match key {
            "name" => name = Some(val),
            "description" => description = Some(val),
            "permission" => permission = Some(val),
            "actions" => in_actions = true,
            _ => {}
        }
    }

    let name = name.filter(|s| !s.is_empty())?;
    let perm = match permission.as_deref().unwrap_or("agent") {
        "owner" => "owner",
        "co_agent" | "co-agent" | "coagent" => "co_agent",
        _ => "agent",
    };

    Some(ParsedSkill {
        name,
        description: description.unwrap_or_default(),
        permission_db: format!("\"{perm}\""),
        actions,
        body,
    })
}

/// POST /api/agents/{id}/skills/seed-standard — 標準スキルをシードする。
///
/// `skills/*.skill.md` を全て読み、同名スキルが既に無いものだけを挿入する（冪等）。
/// ウィザードのエージェント作成ステップ完了後に自動で呼ばれる想定。
pub async fn seed_standard_skills(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // エージェント存在チェック（存在しない ID にシードしても無意味）。
    {
        let conn = state.db.lock().unwrap();
        let exists = opencrab_db::queries::get_agent(&conn, &id)
            .map_err(internal)?
            .is_some();
        if !exists {
            return Err((StatusCode::NOT_FOUND, format!("agent not found: {id}")));
        }
    }

    let dir = skills_dir();
    let entries = std::fs::read_dir(&dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("スキルディレクトリを読めません（{}）: {e}", dir.display()),
        )
    })?;

    let mut seeded: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        // *.skill.md のみ対象。README.md 等は無視。
        let is_skill_md = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(".skill.md"))
            .unwrap_or(false);
        if !is_skill_md {
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let Some(parsed) = parse_skill_md(&content) else {
            errors.push(format!(
                "{}: frontmatter を解釈できませんでした",
                path.display()
            ));
            continue;
        };

        let conn = state.db.lock().unwrap();
        // 同名スキルがあればスキップ（冪等）。
        match opencrab_db::queries::find_skill_by_name(&conn, &id, &parsed.name) {
            Ok(Some(_)) => {
                skipped.push(parsed.name);
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                errors.push(format!("{}: {e}", parsed.name));
                continue;
            }
        }

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let row = opencrab_db::queries::SkillRow {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: id.clone(),
            name: parsed.name.clone(),
            description: parsed.description,
            // actions は situation_pattern に JSON 配列で格納する（core::skill の
            // row_to_skill がこのフィールドから actions を復元するため）。
            situation_pattern: serde_json::to_string(&parsed.actions)
                .unwrap_or_else(|_| "[]".into()),
            guidance: parsed.body,
            source_type: "standard".to_string(),
            source_context: None,
            file_path: Some(format!("skills/{file_name}")),
            effectiveness: None,
            usage_count: 0,
            is_active: true,
            permission: parsed.permission_db,
            archived: false,
        };
        match opencrab_db::queries::insert_skill(&conn, &row) {
            Ok(()) => seeded.push(parsed.name),
            Err(e) => errors.push(format!("{}: {e}", parsed.name)),
        }
    }

    Ok(Json(json!({
        "seeded": seeded,
        "skipped": skipped,
        "errors": errors,
        "seeded_count": seeded.len(),
    })))
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let md = "---\nname: autonomous\ndescription: \"自律モード - 説明\"\nversion: 1\npermission: agent\nactions:\n  - send_speech\n  - declare_done\n---\n\n# 本文\n\nガイダンス本体。\n";
        let p = parse_skill_md(md).expect("should parse");
        assert_eq!(p.name, "autonomous");
        assert_eq!(p.description, "自律モード - 説明");
        assert_eq!(p.permission_db, "\"agent\"");
        assert_eq!(p.actions, vec!["send_speech", "declare_done"]);
        assert!(p.body.starts_with("# 本文"));
        assert!(p.body.contains("ガイダンス本体"));
    }

    #[test]
    fn owner_permission_normalized() {
        let md = "---\nname: x\ndescription: d\npermission: owner\n---\nbody";
        let p = parse_skill_md(md).unwrap();
        assert_eq!(p.permission_db, "\"owner\"");
    }

    #[test]
    fn missing_frontmatter_returns_none() {
        assert!(parse_skill_md("# just markdown\n\nno frontmatter").is_none());
    }

    #[test]
    fn unclosed_frontmatter_returns_none() {
        assert!(parse_skill_md("---\nname: x\nno close").is_none());
    }

    #[test]
    fn defaults_permission_to_agent_when_absent() {
        let md = "---\nname: y\ndescription: d\n---\nbody";
        let p = parse_skill_md(md).unwrap();
        assert_eq!(p.permission_db, "\"agent\"");
        assert!(p.actions.is_empty());
    }
}
