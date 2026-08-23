//! Read-only viewer for persisted LLM logs (#766).
//!
//! 挙動調査の第一手はここ——「そのターンで何を送り・何が返り・どのツールが呼ばれたか」を DB から
//! 直接引く。稼働中の core を所有せず読み取り専用で開くので、観測が権威（gate epoch 等）を触らない
//! （AGREED §2.11 補足）。列は本体 opencrab の llm_logs と同一。将来のダッシュボード（旧 API 互換）は
//! 同じ読み口（`list_llm_logs_read_only` / `llm_logs_stats_read_only`）を土台にできる。
//!
//! usage:
//!   opencrab-llm-logs --db <path> list [--agent <subject>] [--limit N]  ログ一覧（新しい順・既定 N=20）
//!   opencrab-llm-logs --db <path> show <id>                             1 件の全文（prompt/response/tool_calls）
//!   opencrab-llm-logs --db <path> stats --agent <subject> [--days N]    日次統計（既定 N=30）

use opencrab_store::{
    list_llm_logs_read_only, llm_log_read_only, llm_logs_stats_read_only,
    recent_llm_logs_read_only, LlmLogRow,
};

fn usage() -> ! {
    eprintln!(
        "usage:\n  \
         opencrab-llm-logs --db <path> list [--agent <subject>] [--limit N]\n  \
         opencrab-llm-logs --db <path> show <id>\n  \
         opencrab-llm-logs --db <path> stats --agent <subject> [--days N]"
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
                Some(agent) => list_llm_logs_read_only(db, &agent, limit),
                None => recent_llm_logs_read_only(db, limit),
            };
            let rows = match rows {
                Ok(rows) => rows,
                Err(e) => {
                    eprintln!("llm logs unavailable: {e}");
                    std::process::exit(1);
                }
            };
            if rows.is_empty() {
                println!("(no llm logs)");
                return;
            }
            for r in rows {
                let tools = r.tool_calls.as_deref().map(tool_names).unwrap_or_default();
                let tools = if tools.is_empty() {
                    String::new()
                } else {
                    format!(" tools=[{tools}]")
                };
                let err = r
                    .error_code
                    .as_deref()
                    .map(|c| format!(" error={c}"))
                    .unwrap_or_default();
                println!(
                    "{id} {ts} agent={agent} session={session} iter={iter} model={model} latency={lat}ms{tools}{err}",
                    id = r.id,
                    ts = r.requested_at.as_deref().unwrap_or(&r.created_at),
                    agent = r.agent_id,
                    session = r.session_id.as_deref().unwrap_or("-"),
                    iter = r.iteration.map(|i| i.to_string()).unwrap_or_else(|| "-".into()),
                    model = r.model.as_deref().unwrap_or("-"),
                    lat = r.latency_ms.map(|l| l.to_string()).unwrap_or_else(|| "-".into()),
                );
            }
        }
        "show" => {
            let Some(id) = rest.first() else { usage() };
            if id.starts_with("--") {
                usage();
            }
            let row = match llm_log_read_only(db, id) {
                Ok(Some(row)) => row,
                Ok(None) => {
                    eprintln!("no llm log with id={id}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("llm logs unavailable: {e}");
                    std::process::exit(1);
                }
            };
            print_detail(&row);
        }
        "stats" => {
            let Some(agent) = opt(rest, "--agent") else {
                usage()
            };
            let days = opt(rest, "--days")
                .and_then(|s| s.parse().ok())
                .unwrap_or(30);
            let rows = match llm_logs_stats_read_only(db, &agent, days) {
                Ok(rows) => rows,
                Err(e) => {
                    eprintln!("llm logs unavailable: {e}");
                    std::process::exit(1);
                }
            };
            if rows.is_empty() {
                println!("(no stats)");
                return;
            }
            println!("date        calls  total_tok  prompt_tok  compl_tok  avg_latency  errors");
            for d in rows {
                println!(
                    "{date}  {count:>5}  {total:>9}  {prompt:>10}  {compl:>9}  {lat:>9.1}ms  {err:>6}",
                    date = d.date,
                    count = d.count,
                    total = d.total_tokens,
                    prompt = d.prompt_tokens,
                    compl = d.completion_tokens,
                    lat = d.avg_latency_ms,
                    err = d.error_count,
                );
            }
        }
        _ => usage(),
    }
}

fn print_detail(r: &LlmLogRow) {
    let dash = |o: &Option<i64>| o.map(|v| v.to_string()).unwrap_or_else(|| "-".into());
    println!("id            {}", r.id);
    println!("requested_at  {}", r.requested_at.as_deref().unwrap_or("-"));
    println!("created_at    {}", r.created_at);
    println!("agent_id      {}", r.agent_id);
    println!("session_id    {}", r.session_id.as_deref().unwrap_or("-"));
    if let Some(t) = &r.turn_record_id {
        println!("turn_record   {t}");
    }
    println!("iteration     {}", dash(&r.iteration));
    println!("is_bot_iter   {}", r.is_bot_iteration);
    println!("model         {}", r.model.as_deref().unwrap_or("-"));
    println!("latency_ms    {}", dash(&r.latency_ms));
    if let Some(tm) = &r.trigger_message_id {
        println!("trigger_msg   {tm}");
    }
    println!(
        "tokens        prompt={} completion={} total={} cache_read={} cache_creation={}",
        dash(&r.prompt_tokens),
        dash(&r.completion_tokens),
        dash(&r.total_tokens),
        dash(&r.cache_read_tokens),
        dash(&r.cache_creation_tokens),
    );
    if let Some(code) = &r.error_code {
        println!("error_code    {code}");
    }
    if let Some(body) = &r.error_body {
        println!("error_body    {body}");
    }
    println!("\n--- prompt (sent) ---\n{}", pretty(&r.prompt));
    println!("\n--- response (returned) ---\n{}", pretty(&r.response));
    match &r.tool_calls {
        Some(tc) => println!("\n--- tool_calls ---\n{}", pretty(tc)),
        None => println!("\n--- tool_calls ---\n(none)"),
    }
}

/// JSON を読みやすく整形する。壊れていても逐語で出す（近いものへ寄せない・原文を失わない）。
fn pretty(s: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| s.to_string()),
        Err(_) => s.to_string(),
    }
}

/// tool_calls JSON から呼ばれた道具名だけをカンマ区切りで抜く（一覧の要約用）。
fn tool_names(s: &str) -> String {
    let Ok(serde_json::Value::Array(calls)) = serde_json::from_str::<serde_json::Value>(s) else {
        return String::new();
    };
    calls
        .iter()
        .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
        .collect::<Vec<_>>()
        .join(",")
}
