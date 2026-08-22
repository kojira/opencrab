use std::{env, fs, path::PathBuf, process::ExitCode};

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let Some(l1_path) = args.next().map(PathBuf::from) else {
        eprintln!("usage: baseline-l2 <l1-json> <scenario-json> <output-json>");
        return ExitCode::from(2);
    };
    let Some(scenario_path) = args.next().map(PathBuf::from) else {
        eprintln!("usage: baseline-l2 <l1-json> <scenario-json> <output-json>");
        return ExitCode::from(2);
    };
    let Some(output_path) = args.next().map(PathBuf::from) else {
        eprintln!("usage: baseline-l2 <l1-json> <scenario-json> <output-json>");
        return ExitCode::from(2);
    };
    if args.next().is_some() {
        eprintln!("usage: baseline-l2 <l1-json> <scenario-json> <output-json>");
        return ExitCode::from(2);
    }

    match opencrab_server::baseline_l2::capture(&l1_path, &scenario_path).await {
        Ok(value) => {
            let bytes = match serde_json::to_vec_pretty(&value) {
                Ok(mut bytes) => {
                    bytes.push(b'\n');
                    bytes
                }
                Err(error) => {
                    eprintln!("serialize baseline: {error}");
                    return ExitCode::FAILURE;
                }
            };
            if let Err(error) = fs::write(&output_path, bytes) {
                eprintln!("write {}: {error}", output_path.display());
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("baseline-l2 capture failed: {error}");
            ExitCode::FAILURE
        }
    }
}
