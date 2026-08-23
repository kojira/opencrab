use opencrab_converter::{convert, ConvertOptions, NoMigrationInstances};
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!(
        "usage: opencrab-converter --source <source.db> --target <target.db> \
         --config <default.toml> --environment <effective.env> \
         --captured-at <utc-nanos>"
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
    let mut config = None;
    let mut environment = None;
    let mut captured_at = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--source") => source = args.next().map(PathBuf::from),
            Some("--target") => target = args.next().map(PathBuf::from),
            Some("--config") => config = args.next().map(PathBuf::from),
            Some("--environment") => environment = args.next().map(PathBuf::from),
            Some("--captured-at") => {
                captured_at = args
                    .next()
                    .and_then(|value| value.to_str().and_then(|value| value.parse::<i64>().ok()))
            }
            _ => usage(),
        }
    }
    let (Some(source), Some(target), Some(config), Some(environment), Some(captured_at)) =
        (source, target, config, environment, captured_at)
    else {
        usage();
    };
    let outcome = convert(
        ConvertOptions {
            source,
            target,
            config,
            environment,
            captured_at,
        },
        &NoMigrationInstances,
    )?;
    let rendered = outcome.report.to_pretty_json()?;
    print!("{rendered}");
    Ok(())
}
