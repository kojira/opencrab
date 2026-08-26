//! §2.2: 禁止 production 参照 0、server から gateway crate 依存 0。

use std::fs;
use std::path::{Path, PathBuf};

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn walk_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("tests") {
                continue;
            }
            walk_rs(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn forbidden_production_references_are_zero() {
    let src = crate_root().join("src");
    let mut files = Vec::new();
    walk_rs(&src, &mut files);
    let patterns = [
        "send_web_message",
        "web_stream",
        "send_owner_instruction",
        "send_mentor_instruction",
        "fn send_message",
        "WebCompletionSink",
        "WebTimedFireSink",
        "WEB_SESSION_PREFIX",
        "opencrab_web_gateway",
        "opencrab-web-gateway",
    ];
    let mut hits = Vec::new();
    for path in &files {
        let text = fs::read_to_string(path).unwrap();
        for pat in patterns {
            if text.contains(pat) {
                hits.push(format!("{}: {pat}", path.display()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "forbidden production references:\n{}",
        hits.join("\n")
    );
}

#[test]
fn server_cargo_has_no_web_gateway_dependency() {
    let toml = fs::read_to_string(crate_root().join("Cargo.toml")).unwrap();
    assert!(
        !toml.contains("opencrab-web-gateway"),
        "server Cargo.toml still depends on opencrab-web-gateway"
    );
    assert!(
        !toml.contains("web = "),
        "server still has a web feature"
    );
}
