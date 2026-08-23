use opencrab_converter::{convert, ConvertOptions, NoMigrationInstances};
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!("usage: opencrab-converter --source <source.db> --target <target.db>");
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
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--source") => source = args.next().map(PathBuf::from),
            Some("--target") => target = args.next().map(PathBuf::from),
            _ => usage(),
        }
    }
    let (Some(source), Some(target)) = (source, target) else {
        usage();
    };
    let outcome = convert(ConvertOptions { source, target }, &NoMigrationInstances)?;
    let rendered = outcome.report.to_pretty_json()?;
    print!("{rendered}");
    Ok(())
}
