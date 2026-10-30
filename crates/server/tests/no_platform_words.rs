//! Core owns sessions only. Production code under `crates/` must not name a platform or a
//! platform concept (discord / nostr / slack / voice / channel); those belong to gateways.
//!
//! Not scanned:
//! - comments;
//! - test code: `tests/` directories, files whose name contains `test`, modules declared under
//!   `#[cfg(test)]` (inline or as files), and everything after a `#[cfg(test)] mod … {`;
//! - `crates/db/src/schema/`: migrations describe past schemas and cannot be rewritten.
//!
//! Allowed words that are not platforms: in-process channels (`mpsc::channel`,
//! `unbounded_channel`, …), `inner_voice` (the agent's monologue), and the stored metadata value
//! `heartbeat_channel_echo` (existing rows carry it).

use std::path::{Path, PathBuf};

const WORDS: &[&str] = &["discord", "nostr", "slack", "voice", "channel"];
const ALLOWED: &[&str] = &[
    "unbounded_channel",
    "mpsc::channel",
    "oneshot::channel",
    "broadcast::channel",
    "watch::channel",
    "inner_voice",
    "innervoice",
    "heartbeat_channel_echo",
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if path.is_dir() {
            if name == "tests" || name == "target" || path.ends_with("db/src/schema") {
                continue;
            }
            rust_files(&path, out);
        } else if name.ends_with(".rs") && !name.contains("test") {
            out.push(path);
        }
    }
}

/// The code part of a line: everything before a `//` that is not inside a string literal.
fn code_part(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_string => i += 1,
            b'"' => in_string = !in_string,
            b'/' if !in_string && bytes.get(i + 1) == Some(&b'/') => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

/// Files and directories of modules declared under `#[cfg(test)]` (`mod name;`, optionally with
/// `#[path = "…"]`).
fn test_module_paths(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let dir = file.parent().unwrap();
        let stem = file.file_stem().unwrap().to_string_lossy().to_string();
        // `foo.rs` declares children in `foo/`; `mod.rs`/`lib.rs`/`main.rs` in their own dir.
        let child_dir = if ["mod", "lib", "main"].contains(&stem.as_str()) {
            dir.to_path_buf()
        } else {
            dir.join(&stem)
        };
        for (i, line) in lines.iter().enumerate() {
            if !line.trim_start().starts_with("#[cfg(test)]") {
                continue;
            }
            let mut j = i + 1;
            let mut explicit = None;
            while let Some(l) = lines.get(j) {
                let t = l.trim_start();
                if let Some(rest) = t.strip_prefix("#[path = \"") {
                    explicit = rest.split('"').next().map(|p| child_dir.join(p));
                    if !["mod", "lib", "main"].contains(&stem.as_str()) {
                        explicit = rest.split('"').next().map(|p| dir.join(p));
                    }
                }
                if !t.starts_with("#[") {
                    break;
                }
                j += 1;
            }
            let Some(item) = lines.get(j).map(|l| l.trim()) else {
                continue;
            };
            let name = item
                .strip_prefix("pub(crate) mod ")
                .or_else(|| item.strip_prefix("pub mod "))
                .or_else(|| item.strip_prefix("mod "))
                .and_then(|rest| rest.strip_suffix(';'));
            if let Some(name) = name {
                match explicit {
                    Some(path) => out.push(path),
                    None => {
                        out.push(child_dir.join(format!("{name}.rs")));
                        out.push(child_dir.join(name));
                    }
                }
            }
        }
    }
    out
}

#[test]
fn production_code_names_no_platform() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    rust_files(&crates, &mut files);
    let test_modules = test_module_paths(&files);
    files.retain(|file| !test_modules.iter().any(|t| file == t || file.starts_with(t)));
    let mut hits = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let mut in_block_comment = false;
        let lines: Vec<&str> = text.lines().collect();
        let mut skip = 0;
        for (number, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if skip > 0 {
                skip -= 1;
                continue;
            }
            // `#[cfg(test)]` and the attributes after it mark one test-only item.
            if trimmed.starts_with("#[cfg(test)]") {
                let mut item = number + 1;
                while lines.get(item).is_some_and(|l| l.trim_start().starts_with("#[")) {
                    item += 1;
                }
                // An inline test module runs to the end of the file in practice.
                if lines.get(item).is_some_and(|l| l.trim_end().ends_with('{')) {
                    break;
                }
                skip = item - number;
                continue;
            }
            if in_block_comment {
                in_block_comment = !trimmed.contains("*/");
                continue;
            }
            if trimmed.starts_with("/*") {
                in_block_comment = !trimmed.contains("*/");
                continue;
            }
            let mut code = code_part(line).to_ascii_lowercase();
            for allowed in ALLOWED {
                code = code.replace(allowed, "");
            }
            for word in WORDS {
                if code.contains(word) {
                    hits.push(format!("{}:{}: {}", file.display(), number + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "platform words in core production code:\n{}",
        hits.join("\n")
    );
}
