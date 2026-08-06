//! Claude Code のセッションログを 1 プロジェクト分だけ `memory_sessions` へ取り込む
//! コマンド（#413 段階1）。
//!
//! 選別・マッピング・重複防止の中身は [`opencrab_core::import::claude_code`] にあり、
//! ここは「読む・書く・数える」だけを持つ。
//!
//! ```text
//! cargo run --bin opencrab-import-claude-code -- \
//!     --db <path> \
//!     --project-dir ~/.claude/projects/<project> \
//!     --agent-id <agent_id> \
//!     [--workspace-root <path>] [--dry-run]
//! ```
//!
//! - `--db` は**必須**。渡さないと何も起きない。
//! - `--project-dir` は **1 プロジェクト**（`~/.claude/projects/` 直下の 1 ディレクトリ）。
//!   複数プロジェクトを 1 エージェントへ入れない — 材料が混ざると単位も混ざる。
//!   プロジェクトが違えば `--agent-id` も分ける。
//! - `--dry-run` は 1 行も書かずに集計だけ出す。
//!
//! 読み取りは `.jsonl` のみ（**元ログには一切書かない**）。
//!
//! # 取り込み先を本番 DB にしない
//!
//! **`config/default.toml` を読まない。** DB パスは `--db` からしか来ないので、config の
//! `database.path`（＝稼働中の本番 DB）が既定として紛れ込む余地が構造的に無い。
//! 空の新規ファイルを渡せばスキーマごと作る（[`opencrab_db::schema::initialize`]）ので、
//! **既存エージェントのデータが 1 行も無い DB でそのまま動く**。
//!
//! さらに、**指定した DB に他のエージェントの生ログが 1 行でもあれば中止する**。
//! オーナー判断（#413）で移住先は稼働中の 3 体とは別インスタンス（別 DB・別ポート・
//! ゲートウェイ無効・ハートビート無効）と決まっており、理由は 4 つ:
//!
//! 1. **デバッグとの干渉**: 本番 DB へ read-only クエリを投げるたびに、自分の記憶を触る
//!    ことになる。過去ログを読んでデバッグすると混乱して手が止まる実例がある。
//! 2. **再起動との干渉**: 同じインスタンスに記憶ランがあると停止のたびに中断される
//!    （宣言ランのバックログ消化は 5.5 時間かかった）。
//! 3. **記憶の分離**: 「記憶をエージェント間で同期しない」を DB レベルで担保できる。
//! 4. **事故の隔離**: 取り込みや実験で壊しても稼働中の 3 体に影響しない。
//!
//! 3 は**この経路だけの都合ではない**ので、運用の申し合わせではなくコマンドの不変条件に
//! してある。
//!
//! # 退避先
//!
//! thinking の全文を置くワークスペースは、既定で `--db` の隣（`<db の親>/agents/
//! <agent_id>/workspace`）。別インスタンスの既定レイアウト（`data/opencrab.db` と
//! `data/agents/{agent_id}/workspace`）と一致するので、段階2 でそのまま拾える。
//! `--workspace-root` で明示もできる。
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
    db: String,
    workspace_root: Option<PathBuf>,
    dry_run: bool,
}

fn usage() -> &'static str {
    "usage: opencrab-import-claude-code --db <path> --project-dir <dir> --agent-id <id> \
     [--workspace-root <path>] [--dry-run]"
}

fn parse_args() -> Result<Args> {
    let mut project_dir: Option<PathBuf> = None;
    let mut agent_id: Option<String> = None;
    let mut db: Option<String> = None;
    let mut workspace_root: Option<PathBuf> = None;
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
            "--workspace-root" => workspace_root = Some(PathBuf::from(want("--workspace-root")?)),
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
        // 既定にしない（config の `database.path` へフォールバックしない）。稼働中の
        // 本番 DB が黙って書き込み先になる経路をそもそも作らない。
        db: db.with_context(|| format!("--db is required\n{}", usage()))?,
        workspace_root,
        dry_run,
    })
}

/// thinking の退避先。`--workspace-root` が無ければ `--db` の隣に置く。
///
/// 別インスタンスの既定レイアウト（`data/opencrab.db` と
/// `data/agents/{agent_id}/workspace`）と一致する形。cwd にも config にも依存しない。
fn default_workspace_root(db_path: &str, agent_id: &str) -> Result<PathBuf> {
    // agent_id はディレクトリ名になる。`../` 入りを弾く（#48 と同じ検証を通す）。
    opencrab_core::workspace::validate_agent_id(agent_id)?;
    let base = Path::new(db_path).parent().unwrap_or(Path::new("."));
    Ok(base.join("agents").join(agent_id).join("workspace"))
}

/// 既存行から重複防止の鍵と thinking 連番のカーソルを復元し、
/// **他所が入れた行がこのエージェントに既にあるか**も同時に見る。
#[derive(Debug)]
struct Existing {
    keys: HashSet<(String, String, usize)>,
    next_thinking_index: usize,
    /// この取り込み以外が入れた行の件数（誤爆検出用）。
    foreign_rows: i64,
}

/// この DB に**他のエージェント**の生ログが何行あるか。
///
/// 0 でなければ稼働中のインスタンスの DB（か、その写し）を渡している。取り込み先は
/// 別インスタンスの DB でなければならないので、その場合は中止する。
fn other_agent_rows(conn: &Connection, agent_id: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM memory_sessions WHERE agent_id != ?1",
        [agent_id],
        |r| r.get(0),
    )?)
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

/// テーブルが存在するか。`sqlite_master` を照会するだけで **DB を書き換えない**。
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

/// 取り込み先が共有 DB でないことを確かめ、既存の取り込み状態を返す。**照会のみで
/// DB を書き換えない**。
///
/// - 他エージェントの行があれば中止（1 DB = 1 エージェント）。
/// - 同じ `agent_id` に別経路が入れた行があれば中止（id の打ち間違え）。
fn guard_not_shared_database(conn: &Connection, agent_id: &str, db_path: &str) -> Result<Existing> {
    // 稼働中のインスタンスの DB を渡していないか。他のエージェントの記憶がある DB は
    // 取り込み先にしない（デバッグ・再起動と干渉し、記憶の分離が DB レベルで崩れる）。
    let others = other_agent_rows(conn, agent_id)?;
    if others > 0 {
        bail!(
            "{db_path} already holds {others} session log rows for other agents; \
             refusing to import into a shared database. Claude Code material belongs in a \
             separate instance (its own db / port / gateways off / heartbeat off) so that \
             debugging and restarts do not touch these memories. Point --db at a new file."
        );
    }

    let existing = load_existing(conn, agent_id)?;

    // 同じ agent_id に別経路が入れた行がある場合も止める（id の打ち間違え）。
    if existing.foreign_rows > 0 {
        bail!(
            "agent {agent_id} already has {} session log rows from other sources; \
             refusing to mix Claude Code material into an existing agent's memory \
             (use a fresh agent id — 1 project = 1 agent)",
            existing.foreign_rows
        );
    }

    Ok(existing)
}

/// 取り込み前の安全確認とスキーマ準備。**中止する場合は DB を一切書き換えない**
/// （`user_version` も行も不変）。main から切り出してテスト可能にしてある。
///
/// 中止ガードは**スキーマのマイグレーションより前**に評価する。`--db` を稼働中の
/// 本番へ打ち間違えたとき、このコマンドがサーバより新しいスキーマ版だと、中止する前に
/// `schema::initialize` が `user_version` やテーブルを本番へ書いてしまうため。
///
/// - `memory_sessions` が既にある DB … initialize より先にガードを通し、通過したときだけ
///   initialize する（隔離 DB のスキーマ更新は正当なので塞がない）。
/// - `memory_sessions` が無い新規/空の DB … ガードの照会対象そのものが無いので、従来どおり
///   initialize でスキーマを作ってから評価する（新規なので必ず 0 件）。
fn prepare_database(conn: &Connection, agent_id: &str, db_path: &str) -> Result<Existing> {
    if table_exists(conn, "memory_sessions")? {
        let existing = guard_not_shared_database(conn, agent_id, db_path)?;
        opencrab_db::schema::initialize(conn).with_context(|| format!("init schema: {db_path}"))?;
        Ok(existing)
    } else {
        // 空の新規ファイルでもそのまま動くようにスキーマを作る。段階2 の別インスタンスへ
        // そのまま渡せる形にする。
        opencrab_db::schema::initialize(conn).with_context(|| format!("init schema: {db_path}"))?;
        guard_not_shared_database(conn, agent_id, db_path)
    }
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
    let args = parse_args()?;
    let db_path = args.db.clone();
    let workspace_root = match args.workspace_root.clone() {
        Some(root) => root,
        None => default_workspace_root(&db_path, &args.agent_id)?,
    };

    println!("project:   {}", args.project_dir.display());
    println!("agent:     {}", args.agent_id);
    println!(
        "db:        {db_path}{}",
        if args.dry_run { " (dry-run)" } else { "" }
    );
    println!("workspace: {}", workspace_root.display());

    // 生ログは開く前に存在を確かめる（打ち間違えたパスで「0 件でした」と言わない）。
    if !args.project_dir.is_dir() {
        bail!("not a directory: {}", args.project_dir.display());
    }

    if let Some(parent) = Path::new(&db_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let conn = Connection::open(&db_path).with_context(|| format!("open DB: {db_path}"))?;
    // 安全ガードを通してからスキーマを準備する。中止する経路では DB を 1 バイトも
    // 書き換えない（[`prepare_database`] の不変条件）。
    let existing = prepare_database(&conn, &args.agent_id, &db_path)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_row(conn: &Connection, agent_id: &str) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.to_string(),
                session_id: "s".to_string(),
                log_type: "speech".to_string(),
                content: "x".to_string(),
                speaker_id: Some(agent_id.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    /// 空の DB では他エージェントの行は 0（＝取り込みが通る）。稼働中の DB を模して
    /// 別エージェントの行を 1 つ足すと検出される（＝中止する）。
    #[test]
    fn a_shared_database_is_detected() {
        let conn = opencrab_db::init_memory().unwrap();
        assert_eq!(other_agent_rows(&conn, "cc-agent").unwrap(), 0);

        // 自分の行は「他エージェント」に数えない（再取り込みで止まらない）。
        insert_row(&conn, "cc-agent");
        assert_eq!(other_agent_rows(&conn, "cc-agent").unwrap(), 0);

        insert_row(&conn, "already-running-agent");
        assert_eq!(other_agent_rows(&conn, "cc-agent").unwrap(), 1);
    }

    /// 全テーブルの行数スナップショット（中止経路で 1 行も動いていないことの照合用）。
    fn table_row_counts(conn: &Connection) -> std::collections::BTreeMap<String, i64> {
        let names: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master \
                     WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            rows
        };
        names
            .into_iter()
            .map(|n| {
                let c: i64 = conn
                    .query_row(&format!("SELECT COUNT(*) FROM \"{n}\""), [], |r| r.get(0))
                    .unwrap();
                (n, c)
            })
            .collect()
    }

    /// 中止する経路では DB を **1 バイトも書き換えない**（マイグレーションも走らせない）。
    /// bin がサーバより新しいスキーマ版でも、`--db` の打ち間違え（他エージェントの行がある
    /// 共有 DB）では `user_version` も全テーブルの行数も動かない、という不変条件の回帰。
    #[test]
    fn aborting_on_a_shared_database_writes_nothing() {
        let conn = opencrab_db::init_memory().unwrap();
        // 他エージェントの行がある＝共有 DB（稼働中インスタンス）を模す。
        insert_row(&conn, "already-running-agent");

        // サーバが 1 つ古いスキーマ版であることを模す（bin の方が新しく、initialize が
        // マイグレーションを書き得る状況）。人為的に user_version を 1 つ下げる。
        let current: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        let lowered = current - 1;
        conn.execute_batch(&format!("PRAGMA user_version = {lowered}"))
            .unwrap();

        let before = table_row_counts(&conn);

        // 取り込み準備は中止する（memory_sessions が既にあるので initialize より前に
        // ガードが走る）。
        let err = prepare_database(&conn, "cc-agent", "/tmp/does-not-matter.db").unwrap_err();
        assert!(
            err.to_string().contains("shared database"),
            "想定した中止理由でない: {err}"
        );

        // 中止したので initialize は走っていない: user_version も全テーブル行数も不変。
        let after_version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after_version, lowered,
            "中止したのに user_version が動いた（マイグレーションが本番へ書かれた）"
        );
        assert_eq!(
            table_row_counts(&conn),
            before,
            "中止したのにテーブルの行数が変わった"
        );
    }

    /// 退避先は `--db` の隣。別インスタンスの既定レイアウト
    /// （`data/opencrab.db` ＋ `data/agents/{agent_id}/workspace`）と一致する。
    #[test]
    fn workspace_defaults_next_to_the_database() {
        assert_eq!(
            default_workspace_root("/srv/cc/data/opencrab.db", "cc-agent").unwrap(),
            PathBuf::from("/srv/cc/data/agents/cc-agent/workspace")
        );
        // 親の無いパスでも壊れない。
        assert_eq!(
            default_workspace_root("opencrab.db", "cc-agent").unwrap(),
            PathBuf::from("agents/cc-agent/workspace")
        );
    }

    /// agent_id はディレクトリ名になる。パス区切り入りは弾く。
    #[test]
    fn workspace_root_rejects_path_traversal_in_the_agent_id() {
        assert!(default_workspace_root("/srv/cc/data/opencrab.db", "../../etc").is_err());
    }
}
