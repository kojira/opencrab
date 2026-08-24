//! present && enabled の discord instance を列挙する。secret 値は出さない。

use opencrab_store::discord_launch_decisions_read_only;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(flag) = args.next() else {
        eprintln!("usage: opencrab-discord-launch-list --db <path>");
        std::process::exit(1);
    };
    let Some(path) = args.next() else {
        eprintln!("usage: opencrab-discord-launch-list --db <path>");
        std::process::exit(1);
    };
    if flag != "--db" || args.next().is_some() {
        eprintln!("usage: opencrab-discord-launch-list --db <path>");
        std::process::exit(1);
    }

    let rows = match discord_launch_decisions_read_only(path) {
        Ok(rows) => rows,
        Err(_) => {
            eprintln!("discord launch list unavailable");
            std::process::exit(1);
        }
    };
    for row in rows {
        if row.start {
            println!("{}", row.instance_id);
        } else if row.label.starts_with("shared:") {
            eprintln!(
                "instance {} label={} shared instance requires §8 multi-agent routing - not yet implemented, tracked in #793",
                row.instance_id, row.label
            );
        } else {
            eprintln!(
                "instance {} label={} token 未設定につき未起動",
                row.instance_id, row.label
            );
        }
    }
}
