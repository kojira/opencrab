//! Read-only viewer for persisted tool execution logs (#787).
//!
//! 稼働中の core を所有せず読み取り専用で開く。list / show のみ（stats は作らない）。
//!
//! usage:
//!   opencrab-tool-logs --db <path> list [--agent <subject>] [--limit N]  ログ一覧（新しい順・既定 N=20）
//!   opencrab-tool-logs --db <path> show <id>                             1 件の全文（args/result）

use opencrab_store::{
    list_tool_logs_read_only, recent_tool_logs_read_only, tool_log_read_only, ToolLogRow,
};

fn usage() -> ! {
    eprintln!(
        "usage:\n  \
         opencrab-tool-logs --db <path> list [--agent <subject>] [--limit N]\n  \
         opencrab-tool-logs --db <path> show <id>"
    );
    std::process::exit(1);
}

fn opt(rest: &[String], key: &str) -> Option<String> {
    rest.iter()
        .position(|a| a == key)
        .and_then(|i| rest.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 || args[0] != "--db" {
        usage();
    }
    let db = &args[1];
    let cmd = &args[2];
    let rest = &args[3..];

    match cmd.as_str() {
        "list" => {
            let limit = opt(rest, "--limit")
                .and_then(|s| s.parse().ok())
                .unwrap_or(20);
            let rows = match opt(rest, "--agent") {
                Some(agent) => list_tool_logs_read_only(db, &agent, limit),
                None => recent_tool_logs_read_only(db, limit),
            };
            let rows = match rows {
                Ok(rows) => rows,
                Err(e) => {
                    eprintln!("tool logs unavailable: {e}");
                    std::process::exit(1);
                }
            };
            if rows.is_empty() {
                println!("(no tool logs)");
                return;
            }
            for r in rows {
                println!(
                    "{id} {ts} agent={agent} session={session} tool={tool} outcome={outcome} latency={lat}ms",
                    id = r.id,
                    ts = r.started_at.as_deref().unwrap_or(&r.created_at),
                    agent = r.agent_id,
                    session = r.session_id.as_deref().unwrap_or("-"),
                    tool = r.tool_name,
                    outcome = r.outcome,
                    lat = r.latency_ms.map(|l| l.to_string()).unwrap_or_else(|| "-".into()),
                );
            }
        }
        "show" => {
            let Some(id) = rest.first() else { usage() };
            if id.starts_with("--") {
                usage();
            }
            let Ok(id) = id.parse::<i64>() else { usage() };
            let row = match tool_log_read_only(db, id) {
                Ok(Some(row)) => row,
                Ok(None) => {
                    eprintln!("no tool log with id={id}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("tool logs unavailable: {e}");
                    std::process::exit(1);
                }
            };
            print_detail(&row);
        }
        _ => usage(),
    }
}

fn print_detail(r: &ToolLogRow) {
    let dash = |o: &Option<i64>| o.map(|v| v.to_string()).unwrap_or_else(|| "-".into());
    println!("id            {}", r.id);
    println!("started_at    {}", r.started_at.as_deref().unwrap_or("-"));
    println!("created_at    {}", r.created_at);
    println!("agent_id      {}", r.agent_id);
    println!("session_id    {}", r.session_id.as_deref().unwrap_or("-"));
    if let Some(t) = &r.turn_record_id {
        println!("turn_record   {t}");
    }
    if let Some(a) = &r.activity_id {
        println!("activity      {a}");
    }
    println!("iteration     {}", dash(&r.iteration));
    println!("tool_name     {}", r.tool_name);
    println!("outcome       {}", r.outcome);
    println!("latency_ms    {}", dash(&r.latency_ms));
    println!("\n--- args ---\n{}", pretty(&r.args_json));
    println!("\n--- result ---\n{}", r.result_text);
}

/// JSON を読みやすく整形する。壊れていても逐語で出す（近いものへ寄せない・原文を失わない）。
fn pretty(s: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| s.to_string()),
        Err(_) => s.to_string(),
    }
}
