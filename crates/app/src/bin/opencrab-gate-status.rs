//! Read-only canonical gate readiness helper.

use opencrab_store::gate_status_read_only;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(flag) = args.next() else {
        eprintln!("usage: opencrab-gate-status --db <path>");
        std::process::exit(1);
    };
    let Some(path) = args.next() else {
        eprintln!("usage: opencrab-gate-status --db <path>");
        std::process::exit(1);
    };
    if flag != "--db" || args.next().is_some() {
        eprintln!("usage: opencrab-gate-status --db <path>");
        std::process::exit(1);
    }

    let rows = match gate_status_read_only(path) {
        Ok(rows) => rows,
        Err(_) => {
            eprintln!("running (degraded): status unavailable");
            std::process::exit(1);
        }
    };
    let enabled: Vec<_> = rows.into_iter().filter(|row| row.enabled).collect();
    if enabled.is_empty() {
        println!("running (healthy)");
        return;
    }

    let mut degraded = false;
    for row in enabled {
        let code = if row.connection_epoch.is_none() {
            "no_connection"
        } else if row.connection_revision != Some(row.revision) {
            "revision_mismatch"
        } else if row.connection_state.as_deref() == Some("active") && row.lifecycle == "running" {
            "active"
        } else if row.lifecycle == "starting" {
            "starting"
        } else {
            row.connection_state.as_deref().unwrap_or("no_connection")
        };
        if code != "active" {
            degraded = true;
        }
        println!(
            "instance={} revision={} state={code}",
            row.instance_id, row.revision
        );
    }
    if degraded {
        eprintln!("running (degraded)");
        std::process::exit(1);
    }
    println!("running (healthy)");
}
