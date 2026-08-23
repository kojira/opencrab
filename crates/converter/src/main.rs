use opencrab_converter::migrate_in_place;
use rusqlite::Connection;
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!(
        "usage: opencrab-converter --db <opencrab.db> \
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
    let mut db = None;
    let mut config = None;
    let mut environment = None;
    let mut captured_at = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--db") => db = args.next().map(PathBuf::from),
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
    let (Some(db), Some(config), Some(environment), Some(captured_at)) =
        (db, config, environment, captured_at)
    else {
        usage();
    };
    let mut conn = Connection::open(db)?;
    let report = migrate_in_place(&mut conn, config, environment, captured_at)?;
    let rendered = report.to_pretty_json()?;
    print!("{rendered}");
    Ok(())
}
