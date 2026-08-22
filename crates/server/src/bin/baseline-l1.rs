use std::{fs, path::PathBuf};

use serde_json::json;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("baseline-l1: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or_else(|| "cannot resolve repository root".to_string())?;
    let output = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| repo.join("baseline/l1/opencrab-l1.json"));

    let routes = opencrab_server::production_route_inventory();
    let route_count = routes.len();
    let document = json!({
        "schema_version": 1,
        "scope": "opencrab-external-shape-l1",
        "http": {
            "route_count": route_count,
            "routes": routes,
            "uncollected": []
        },
        "tools": opencrab_server::baseline_l1::collect_tools()?,
        "fixed_responses": opencrab_server::baseline_l1::collect_responses().await?,
    });
    let mut bytes = serde_json::to_vec_pretty(&document)
        .map_err(|e| format!("JSON serialization failed: {e}"))?;
    bytes.push(b'\n');
    let parent = output
        .parent()
        .ok_or_else(|| format!("output has no parent: {}", output.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    fs::write(&output, bytes).map_err(|e| format!("{}: {e}", output.display()))?;
    println!("wrote {} ({} HTTP routes)", output.display(), route_count);
    Ok(())
}
