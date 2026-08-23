use opencrab_converter::{convert, ConvertOptions, MigrationInstance};
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!(
        "usage: opencrab-converter --source <source.db> --target <target.db> [--instance-set <instances.json>]"
    );
    std::process::exit(2);
}

fn main() {
    if let Err(error) = run() {
        eprintln!("opencrab-converter: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut source = None;
    let mut target = None;
    let mut instance_set = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--source") => source = args.next().map(PathBuf::from),
            Some("--target") => target = args.next().map(PathBuf::from),
            Some("--instance-set") => instance_set = args.next().map(PathBuf::from),
            _ => usage(),
        }
    }
    let (Some(source), Some(target)) = (source, target) else {
        usage();
    };
    let instances = match instance_set {
        Some(path) => MigrationInstance::read_set(path)?,
        None => Vec::new(),
    };
    let outcome = convert(ConvertOptions {
        source,
        target,
        migration_instances: instances,
    })?;
    let rendered = outcome.report.to_pretty_json()?;
    print!("{rendered}");
    Ok(())
}
