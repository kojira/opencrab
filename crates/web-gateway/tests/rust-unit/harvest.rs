//! DESIGN-SAMPLES-NODE §1.3: 正典未定値を source から機械採取する。
//! 採取値は golden にしない。出力は追記要求であり、期待値の凍結ではない。

use std::path::PathBuf;

const CLIENT_SRC: &str = include_str!("../../src/v3/client.rs");
const HTTP_SRC: &str = include_str!("../../src/v3/http.rs");

#[derive(Debug)]
struct Harvested {
    symbol: &'static str,
    source_path: &'static str,
    value: String,
}

fn take_const_usize(src: &str, name: &str) -> Option<String> {
    let needle = format!("const {name}: usize = ");
    let rest = src.split_once(&needle)?.1;
    let token = rest.split(';').next()?.trim();
    Some(token.to_string())
}

fn take_duration(src: &str, name: &str) -> Option<String> {
    let needle = format!("const {name}: Duration = ");
    let rest = src.split_once(&needle)?.1;
    let expr = rest.split(';').next()?.trim();
    Some(expr.to_string())
}

fn take_sse_event_name(src: &str) -> Option<String> {
    src.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(".event(\"")?;
        let name = rest.strip_suffix("\")")?;
        (name == "gate_error").then(|| name.to_string())
    })
}

fn take_backoff_rule(src: &str) -> Option<String> {
    if src.contains("backoff.saturating_mul(2).min(RECONNECT_MAX)")
        && src.contains("backoff = RECONNECT_MIN")
    {
        Some("RECONNECT_MIN から saturating_mul(2)、上限 RECONNECT_MAX。成功時 RECONNECT_MIN へ reset".into())
    } else {
        None
    }
}

fn source_revision() -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok();
    out.and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "UNKNOWN".into())
}

fn harvest() -> Vec<Harvested> {
    vec![
        Harvested {
            symbol: "LIVE_QUEUE_CAP",
            source_path: "crates/web-gateway/src/v3/client.rs",
            value: take_const_usize(CLIENT_SRC, "LIVE_QUEUE_CAP").expect("LIVE_QUEUE_CAP"),
        },
        Harvested {
            symbol: "SSE event name (http.rs Event::event)",
            source_path: "crates/web-gateway/src/v3/http.rs",
            value: take_sse_event_name(HTTP_SRC).expect("gate_error event"),
        },
        Harvested {
            symbol: "SAID_TIMEOUT",
            source_path: "crates/web-gateway/src/v3/client.rs",
            value: take_duration(CLIENT_SRC, "SAID_TIMEOUT").expect("SAID_TIMEOUT"),
        },
        Harvested {
            symbol: "RECONNECT_MIN",
            source_path: "crates/web-gateway/src/v3/client.rs",
            value: take_duration(CLIENT_SRC, "RECONNECT_MIN").expect("RECONNECT_MIN"),
        },
        Harvested {
            symbol: "RECONNECT_MAX",
            source_path: "crates/web-gateway/src/v3/client.rs",
            value: take_duration(CLIENT_SRC, "RECONNECT_MAX").expect("RECONNECT_MAX"),
        },
        Harvested {
            symbol: "reconnect backoff rule",
            source_path: "crates/web-gateway/src/v3/client.rs",
            value: take_backoff_rule(CLIENT_SRC).expect("backoff rule"),
        },
    ]
}

pub fn render_report() -> String {
    let rev = source_revision();
    let rows = harvest();
    let mut out = String::new();
    out.push_str("# 正典未定値（機械採取・追記要求）\n\n");
    out.push_str("DESIGN-SAMPLES-NODE.md §1.3。承認前の Rust 値を正典または golden にしない。\n\n");
    out.push_str(&format!("- source revision: `{rev}`\n"));
    out.push_str("- 採取対象 crate: `opencrab-web-gateway`\n\n");
    out.push_str("| 項目 | symbol | 採取値 | source |\n");
    out.push_str("|---|---|---|---|\n");
    let labels = [
        "live queue 容量",
        "SSE error event 名",
        "said 待ち上限",
        "reconnect backoff 初期値",
        "reconnect backoff 上限",
        "reconnect backoff 増加則 / reset",
    ];
    for (label, row) in labels.iter().zip(rows.iter()) {
        out.push_str(&format!(
            "| {label} | `{}` | `{}` | `{}` |\n",
            row.symbol, row.value, row.source_path
        ));
    }
    out.push_str("\n## 正典追記要求\n\n");
    out.push_str("- WEBGATE: live queue の容量と overflow 境界\n");
    out.push_str("- WEBGATE: SSE error event 名と data shape\n");
    out.push_str("- WEBGATE: said 待ち timeout と HTTP/UDS 終端\n");
    out.push_str("- WEBGATE: reconnect 初期値、増加則、上限、reset 条件\n");
    out
}

#[test]
fn harvest_canon_pending_values_prints() {
    let report = render_report();
    println!("{report}");
    assert!(report.contains("LIVE_QUEUE_CAP"), "{report}");
    assert!(report.contains("SAID_TIMEOUT"), "{report}");
    assert!(report.contains("RECONNECT_MIN"), "{report}");
    assert!(report.contains("RECONNECT_MAX"), "{report}");
    let dest = PathBuf::from("/Volumes/2TB/openclaw/.claude-scratch/oc2-design/canon-pending-values.md");
    std::fs::write(&dest, &report).unwrap_or_else(|e| {
        panic!("write {}: {e}", dest.display());
    });
}
