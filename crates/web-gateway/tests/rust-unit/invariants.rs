//! process harness と fixture 集合の rust-unit 検査。conformance 合格数には入れない。

const HARNESS: &str = include_str!("../conformance.rs");

fn code_mentions(src: &str, needle: &str) -> bool {
    src.lines().any(|line| {
        let t = line.trim_start();
        if t.starts_with("//") {
            return false;
        }
        t.contains(needle)
    })
}

#[test]
fn harness_does_not_import_sut_internals() {
    assert!(
        !code_mentions(HARNESS, "opencrab_web_gateway"),
        "process harness must not import the SUT crate"
    );
    assert!(!code_mentions(HARNESS, "InstanceClient"));
    assert!(!code_mentions(HARNESS, "parse_frame_bytes"));
    assert!(!code_mentions(HARNESS, "router("));
}

#[test]
fn fixture_files_are_the_shared_set() {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../conformance/fixtures");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.ok()?.file_name().into_string().ok()?;
            name.strip_suffix(".json")
                .filter(|n| *n != "ids")
                .map(str::to_string)
        })
        .collect();
    names.sort();
    let expected = [
        "activity",
        "bind-conflict",
        "disconnect-unacked",
        "frame-duplicate",
        "frame-too-large",
        "hello-bind-said-dedup",
        "http-post-and-routes",
        "reconnect",
        "say-three-results",
    ];
    assert_eq!(names, expected);
}
