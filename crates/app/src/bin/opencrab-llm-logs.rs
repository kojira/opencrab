//! Read-only viewer for persisted LLM logs (#766).
//!
//! 挙動調査の第一手はここ——「そのターンで何を送り・何が返り・どのツールが呼ばれたか」を DB から
//! 直接引く。稼働中の core を所有せず読み取り専用で開くので、観測が権威（gate epoch 等）を触らない。
//! 将来のダッシュボードはこの読み口（`recent_llm_logs_read_only` / `llm_log_read_only`）を土台にする。
//!
//! usage:
//!   opencrab-llm-logs --db <path> list [--limit N]   直近のログを新しい順に一覧（既定 N=20）
//!   opencrab-llm-logs --db <path> show <id>          1 件の全文（request / response / tool_calls）

use opencrab_store::{llm_log_read_only, recent_llm_logs_read_only, LlmLogRow};

fn usage() -> ! {
    eprintln!(
        "usage:\n  \
         opencrab-llm-logs --db <path> list [--limit N]\n  \
         opencrab-llm-logs --db <path> show <id>"
    );
    std::process::exit(1);
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
            let mut limit: i64 = 20;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--limit" => {
                        let Some(v) = rest.get(i + 1).and_then(|s| s.parse::<i64>().ok()) else {
                            usage();
                        };
                        limit = v;
                        i += 2;
                    }
                    _ => usage(),
                }
            }
            let rows = match recent_llm_logs_read_only(db, limit) {
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
                println!(
                    "#{id} {ts} turn={turn} subject={subject} iter={iter} model={model} outcome={outcome} latency={lat}ms{tools}",
                    id = r.id,
                    ts = fmt_utc(r.requested_at),
                    turn = r.turn_record_id,
                    subject = r.subject,
                    iter = r.iteration,
                    model = r.model,
                    outcome = r.outcome,
                    lat = r.latency_ms,
                );
            }
        }
        "show" => {
            let Some(id) = rest.first().and_then(|s| s.parse::<i64>().ok()) else {
                usage();
            };
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
        _ => usage(),
    }
}

fn print_detail(r: &LlmLogRow) {
    println!("id            {}", r.id);
    println!(
        "requested_at  {} ({} ns)",
        fmt_utc(r.requested_at),
        r.requested_at
    );
    println!("turn_record   {}", r.turn_record_id);
    println!("place         {}", r.place);
    println!("subject       {}", r.subject);
    println!("iteration     {}", r.iteration);
    println!("model         {}", r.model);
    println!("outcome       {}", r.outcome);
    println!("latency_ms    {}", r.latency_ms);
    if let Some(e) = &r.error_detail {
        println!("error_detail  {e}");
    }
    println!("\n--- request (sent) ---\n{}", pretty(&r.request));
    match &r.response {
        Some(resp) => println!("\n--- response (returned) ---\n{}", pretty(resp)),
        None => println!("\n--- response (returned) ---\n(none)"),
    }
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

/// unix epoch nanos → `YYYY-MM-DD HH:MM:SS UTC`（chrono を足さず、civil-from-days で組む）。
fn fmt_utc(nanos: i64) -> String {
    if nanos <= 0 {
        return "-".to_string();
    }
    let secs = nanos / 1_000_000_000;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC")
}

/// Howard Hinnant の civil_from_days（epoch=1970-01-01 からの日数 → 年月日）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
