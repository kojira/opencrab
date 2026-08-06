//! Claude Code のセッションログを 1 プロジェクト分だけ `memory_sessions` へ取り込む
//! コマンド（#413 段階1）。
//!
//! 選別・マッピング・重複防止の中身は [`opencrab_core::import::claude_code`] にあり、
//! ここは「読む・書く・数える」だけを持つ。
//!
//! ```text
//! cargo run --bin opencrab-import-claude-code -- \
//!     --project-dir ~/.claude/projects/<project> \
//!     --agent-id <agent_id> \
//!     [--db <path>] [--config <path>] [--dry-run]
//! ```
//!
//! - `--project-dir` は **1 プロジェクト**（`~/.claude/projects/` 直下の 1 ディレクトリ）。
//!   複数プロジェクトを 1 エージェントへ入れない — 材料が混ざると単位も混ざる。
//!   プロジェクトが違えば `--agent-id` も分ける。
//! - `--db` を渡すと `config` の DB ではなくそのファイルへ書く。**本番 DB のコピーで
//!   検証する**ための逃げ道で、稼働中の DB を触らずに全体を通せる。
//! - `--dry-run` は 1 行も書かずに集計だけ出す。
//!
//! 読み取りは `.jsonl` のみ（**元ログには一切書かない**）。
//!
//! エージェントの**登録**（`agents` 行の作成）・宣言ランの実行・文脈への注入は段階2 で、
//! ここではやらない。このコマンドは生ログを置くところまで。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use opencrab_core::import::claude_code::{self, PlannedRow};
use rusqlite::Connection;

struct Args {
    project_dir: PathBuf,
    agent_id: String,
    db: Option<String>,
    config: String,
    dry_run: bool,
}

fn usage() -> &'static str {
    "usage: opencrab-import-claude-code --project-dir <dir> --agent-id <id> \
     [--db <path>] [--config <path>] [--dry-run]"
}

fn parse_args() -> Result<Args> {
    let mut project_dir: Option<PathBuf> = None;
    let mut agent_id: Option<String> = None;
    let mut db: Option<String> = None;
    let mut config = "config/default.toml".to_string();
    let mut dry_run = false;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut want = |name: &str| -> Result<String> {
            it.next()
                .with_context(|| format!("{name} takes a value\n{}", usage()))
        };
        match arg.as_str() {
            "--project-dir" => project_dir = Some(PathBuf::from(want("--project-dir")?)),
            "--agent-id" => agent_id = Some(want("--agent-id")?),
            "--db" => db = Some(want("--db")?),
            "--config" => config = want("--config")?,
            "--dry-run" => dry_run = true,
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}\n{}", usage()),
        }
    }

    Ok(Args {
        project_dir: project_dir
            .with_context(|| format!("--project-dir is required\n{}", usage()))?,
        agent_id: agent_id.with_context(|| format!("--agent-id is required\n{}", usage()))?,
        db,
        config,
        dry_run,
    })
}

/// 既存行から重複防止の鍵と thinking 連番のカーソルを復元し、
/// **他所が入れた行がこのエージェントに既にあるか**も同時に見る。
struct Existing {
    keys: HashSet<(String, String, usize)>,
    next_thinking_index: usize,
    /// この取り込み以外が入れた行の件数（誤爆検出用）。
    foreign_rows: i64,
}

fn load_existing(conn: &Connection, agent_id: &str) -> Result<Existing> {
    let mut stmt =
        conn.prepare("SELECT session_id, metadata_json FROM memory_sessions WHERE agent_id = ?1")?;
    let rows: Vec<(String, Option<String>)> = stmt
        .query_map([agent_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let keys = claude_code::imported_keys_from_metadata(
        rows.iter().map(|(s, m)| (s.as_str(), m.as_deref())),
    );
    let next_thinking_index =
        claude_code::next_thinking_index(rows.iter().filter_map(|(_, m)| m.as_deref()));
    let foreign_rows = rows
        .iter()
        .filter(|(_, m)| {
            let source = m
                .as_deref()
                .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                .and_then(|v| {
                    v.get("source")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                });
            source.as_deref() != Some(claude_code::SOURCE_TAG)
        })
        .count() as i64;

    Ok(Existing {
        keys,
        next_thinking_index,
        foreign_rows,
    })
}

/// thinking の全文をワークスペースへ退避する。生ログの印が指す先。
fn offload_thinking(workspace_root: &Path, rows: &[PlannedRow]) -> Result<usize> {
    let dir = workspace_root.join(claude_code::THINKING_DIR);
    let mut written = 0usize;
    for row in rows {
        let (Some(body), Some(rel)) = (&row.thinking_body, row.thinking_rel_path()) else {
            continue;
        };
        if written == 0 {
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let path = workspace_root.join(&rel);
        std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
        written += 1;
    }
    Ok(written)
}

fn human(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1}{}", UNITS[i])
}

fn print_stats(stats: &claude_code::ScanStats) {
    println!(
        "\nscanned: {} files / {} lines ({} unparsable)",
        stats.files, stats.lines, stats.unparsable_lines
    );
    if !stats.cwds.is_empty() {
        println!("cwd values seen: {}", stats.cwds.len());
    }

    println!("\nkept:");
    for (kind, s) in &stats.kept {
        println!("  {kind:<28} {:>8} {:>10}", s.count, human(s.bytes));
    }
    println!(
        "  {:<28} {:>8} {:>10}",
        "(total)",
        stats.kept_rows(),
        human(stats.kept_bytes())
    );

    println!("\ndropped:");
    for (kind, s) in &stats.dropped {
        println!("  {kind:<28} {:>8} {:>10}", s.count, human(s.bytes));
    }
    println!(
        "  {:<28} {:>8} {:>10}",
        "(total)",
        stats.dropped.values().map(|s| s.count).sum::<usize>(),
        human(stats.dropped_bytes())
    );
    if stats.undatable_rows > 0 {
        println!("\n{} rows had no usable timestamp", stats.undatable_rows);
    }
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let args = parse_args()?;

    let cfg = opencrab_server::config::load_config(&args.config)
        .with_context(|| format!("failed to load {}", args.config))?;
    let db_path = args.db.clone().unwrap_or_else(|| cfg.database.path.clone());

    println!("project: {}", args.project_dir.display());
    println!("agent:   {}", args.agent_id);
    println!(
        "db:      {db_path}{}",
        if args.dry_run { " (dry-run)" } else { "" }
    );

    // 生ログは開く前に存在を確かめる（打ち間違えたパスで「0 件でした」と言わない）。
    if !args.project_dir.is_dir() {
        bail!("not a directory: {}", args.project_dir.display());
    }

    let conn = Connection::open(&db_path).with_context(|| format!("open DB: {db_path}"))?;
    let existing = load_existing(&conn, &args.agent_id)?;

    // 既存エージェントへの誤爆を止める。この取り込み以外が入れた行が 1 行でもあれば、
    // それは動いているエージェントの記憶であって、材料を混ぜてよい相手ではない。
    if existing.foreign_rows > 0 {
        bail!(
            "agent {} already has {} session log rows from other sources; \
             refusing to mix Claude Code material into an existing agent's memory \
             (use a fresh agent id — 1 project = 1 agent)",
            args.agent_id,
            existing.foreign_rows
        );
    }

    let scan = claude_code::scan_project_dir(&args.project_dir, existing.next_thinking_index)?;
    print_stats(&scan.stats);

    let planned = scan.rows.len();
    let rows = claude_code::filter_already_imported(scan.rows, &existing.keys);
    println!(
        "\nrows to insert: {} (already imported: {})",
        rows.len(),
        planned - rows.len()
    );

    if args.dry_run {
        println!("dry-run: nothing written.");
        return Ok(());
    }

    let workspace_root = opencrab_core::workspace::resolve_agent_workspace(
        &cfg.agent.workspace_path,
        &args.agent_id,
    )?;
    std::fs::create_dir_all(&workspace_root)
        .with_context(|| format!("create {}", workspace_root.display()))?;
    let offloaded = offload_thinking(&workspace_root, &rows)?;

    // 1 トランザクションで入れる。途中で落ちた取り込みが半端な範囲を残すと、
    // 宣言ランのカーソルがその途中を跨いでしまう。
    let tx = conn.unchecked_transaction()?;
    for row in &rows {
        opencrab_db::queries::insert_session_log_at(
            &tx,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: args.agent_id.clone(),
                session_id: row.session_id.clone(),
                log_type: row.log_type.clone(),
                content: row.content.clone(),
                speaker_id: Some(row.speaker.speaker_id(&args.agent_id)),
                turn_number: None,
                metadata_json: Some(row.metadata_json()),
                created_at: None,
            },
            &row.created_at,
        )?;
    }
    tx.commit()?;

    println!(
        "inserted {} rows; wrote {} thinking bodies under {}",
        rows.len(),
        offloaded,
        workspace_root.join(claude_code::THINKING_DIR).display()
    );
    if let (Some(first), Some(last)) = (rows.first(), rows.last()) {
        println!("range: {} .. {}", first.created_at, last.created_at);
    }
    Ok(())
}
