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
fn harness_discovers_fixtures_by_directory_scan() {
    assert!(
        code_mentions(HARNESS, "read_dir"),
        "process harness must load fixtures by directory scan"
    );
    assert!(
        !code_mentions(HARNESS, "async fn hello_bind_said_dedup"),
        "per-fixture #[tokio::test] is the triple-registration NIT"
    );
}

#[test]
fn fixture_name_field_matches_filename() {
    let dir =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../conformance/fixtures");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.extension().and_then(|s| s.to_str()) != Some("json") || stem == "ids" {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["name"].as_str().unwrap(), stem, "{}", path.display());
    }
}
