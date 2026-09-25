use anyhow::{bail, Context, Result};
use opencrab_gateway_migrate::{
    command::{self, ImportArgs, ProjectArgs},
    destination::Inputs,
    freeze::VerifyArgs,
};
use std::{collections::BTreeMap, env, path::PathBuf};

fn main() {
    if let Err(error) = run() {
        eprintln!("opencrab-gateway-migrate: {error:#}");
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let command = args.next().context("command required")?;
    let options = parse_options(args.collect())?;
    match command.as_str() {
        "import" => {
            let destinations = command::parse_destination_paths(
                options.get("destination").map(Vec::as_slice).unwrap_or(&[]),
            )?;
            let master_keys = command::parse_kv_paths(
                options
                    .get("master-key-file")
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            )?;
            let credential_files = command::parse_kv_paths(
                options
                    .get("credential-file")
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            )?;
            command::run_import(ImportArgs {
                core_path: &one_path(&options, "core-db")?,
                approval_path: &one_path(&options, "approval")?,
                backup_dir: &one_path(&options, "backup-dir")?,
                report_path: &one_path(&options, "report")?,
                inputs: Inputs {
                    paths: destinations,
                    master_keys,
                    credential_files,
                },
            })?;
        }
        "project-core-state" => {
            command::run_project(ProjectArgs {
                core_path: &one_path(&options, "core-db")?,
                approval_path: &one_path(&options, "approval")?,
                import_report_path: &one_path(&options, "import-report")?,
                verification_path: &one_path(&options, "verification")?,
                destination_paths: command::parse_destination_paths(
                    options.get("destination").map(Vec::as_slice).unwrap_or(&[]),
                )?,
            })?;
        }
        "verify-freeze" | "clean-legacy-state" => {
            let freeze_args = VerifyArgs {
                core_path: &one_path(&options, "core-db")?,
                approval_path: &one_path(&options, "approval")?,
                import_report_path: &one_path(&options, "import-report")?,
                verification_path: &one_path(&options, "verification")?,
                freeze_dir: &one_path(&options, "freeze-dir")?,
                freeze_manifest_path: &one_path(&options, "freeze-manifest")?,
                destination_paths: command::parse_destination_paths(
                    options.get("destination").map(Vec::as_slice).unwrap_or(&[]),
                )?,
            };
            if command == "verify-freeze" {
                opencrab_gateway_migrate::freeze::verify(freeze_args)?;
            } else {
                opencrab_gateway_migrate::cleanup::run(freeze_args)?;
            }
        }
        _ => bail!("unknown command"),
    }
    Ok(())
}

fn parse_options(args: Vec<String>) -> Result<BTreeMap<String, Vec<String>>> {
    let mut output: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut iter = args.into_iter();
    while let Some(flag) = iter.next() {
        let name = flag
            .strip_prefix("--")
            .context("options must start with --")?;
        let value = iter.next().context("option value missing")?;
        output.entry(name.into()).or_default().push(value);
    }
    Ok(output)
}
fn one_path(options: &BTreeMap<String, Vec<String>>, name: &str) -> Result<PathBuf> {
    let values = options
        .get(name)
        .with_context(|| format!("--{name} required"))?;
    if values.len() != 1 {
        bail!("--{name} must occur once")
    };
    let path = PathBuf::from(&values[0]);
    if !path.is_absolute() {
        bail!("--{name} must be absolute")
    };
    Ok(path)
}
