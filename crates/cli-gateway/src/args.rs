use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Repl,
    Jsonl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionArg {
    New(String),
    Existing(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub placement: PathBuf,
    pub agent: String,
    pub session: SessionArg,
    pub mode: Mode,
    pub connect_timeout_secs: u64,
}

impl Args {
    pub fn parse<I, S>(values: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut it = values.into_iter().map(Into::into);
        let _program = it.next();
        let mut placement = None;
        let mut agent = None;
        let mut session = None;
        let mut mode = Mode::Auto;
        let mut mode_seen = false;
        let mut timeout = 10;
        let mut timeout_seen = false;
        while let Some(flag) = it.next() {
            let value = match flag.as_str() {
                "--placement"
                | "--agent"
                | "--new"
                | "--session"
                | "--mode"
                | "--connect-timeout-secs" => it
                    .next()
                    .ok_or_else(|| format!("missing value for {flag}"))?,
                "-h" | "--help" => return Err(usage().to_string()),
                _ => return Err(format!("unknown argument: {flag}\n{}", usage())),
            };
            match flag.as_str() {
                "--placement" if placement.is_none() => placement = Some(PathBuf::from(value)),
                "--agent" if agent.is_none() => agent = nonempty("agent", value).map(Some)?,
                "--new" if session.is_none() => {
                    session = nonempty("new session name", value)
                        .map(SessionArg::New)
                        .map(Some)?
                }
                "--session" if session.is_none() => {
                    session = nonempty("session address", value)
                        .map(SessionArg::Existing)
                        .map(Some)?
                }
                "--mode" if !mode_seen => {
                    mode_seen = true;
                    mode = match value.as_str() {
                        "auto" => Mode::Auto,
                        "repl" => Mode::Repl,
                        "jsonl" => Mode::Jsonl,
                        _ => return Err("mode must be auto, repl, or jsonl".into()),
                    }
                }
                "--connect-timeout-secs" if !timeout_seen => {
                    timeout_seen = true;
                    timeout = value
                        .parse::<u64>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| "connect timeout must be a positive integer".to_string())?;
                }
                _ => return Err(format!("argument supplied more than once: {flag}")),
            }
        }
        Ok(Self {
            placement: placement.ok_or_else(|| "--placement is required".to_string())?,
            agent: agent.ok_or_else(|| "--agent is required".to_string())?,
            session: session
                .ok_or_else(|| "exactly one of --new and --session is required".to_string())?,
            mode,
            connect_timeout_secs: timeout,
        })
    }
}

fn nonempty(name: &str, value: String) -> Result<String, String> {
    if value.is_empty() {
        Err(format!("{name} must be nonempty"))
    } else {
        Ok(value)
    }
}

pub fn usage() -> &'static str {
    "usage: opencrab-cli-gateway --placement FILE --agent ID (--new NAME | --session ADDRESS) [--mode auto|repl|jsonl] [--connect-timeout-secs N]"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Vec<&'static str> {
        vec!["bin", "--placement", "/tmp/p.json", "--agent", "agent-a"]
    }

    #[test]
    fn parses_exact_new_selection() {
        let mut argv = base();
        argv.extend(["--new", "chat", "--mode", "jsonl"]);
        let args = Args::parse(argv).unwrap();
        assert_eq!(args.session, SessionArg::New("chat".into()));
        assert_eq!(args.mode, Mode::Jsonl);
        assert_eq!(args.connect_timeout_secs, 10);
    }

    #[test]
    fn requires_one_session_selector() {
        assert!(Args::parse(base()).unwrap_err().contains("exactly one"));
        let mut argv = base();
        argv.extend(["--new", "a", "--session", "extgate-b"]);
        assert!(Args::parse(argv).unwrap_err().contains("more than once"));
    }

    #[test]
    fn rejects_unknown_zero_timeout_and_duplicate_options() {
        let mut argv = base();
        argv.extend(["--session", "x", "--wat"]);
        assert!(Args::parse(argv).is_err());
        let mut argv = base();
        argv.extend(["--session", "x", "--connect-timeout-secs", "0"]);
        assert!(Args::parse(argv).is_err());
        let mut argv = base();
        argv.extend(["--session", "x", "--mode", "jsonl", "--mode", "auto"]);
        assert!(Args::parse(argv).unwrap_err().contains("more than once"));
    }
}
