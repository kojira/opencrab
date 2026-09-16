pub mod args;
pub mod config;
pub mod jsonl;
pub mod repl;
pub mod runtime;

use args::Mode;
use runtime::Frontend;

pub fn resolve_mode(
    mode: Mode,
    stdin_terminal: bool,
    stdout_terminal: bool,
) -> Result<Frontend, String> {
    match mode {
        Mode::Auto if stdin_terminal && stdout_terminal => Ok(Frontend::Repl),
        Mode::Auto => Ok(Frontend::Jsonl),
        Mode::Repl if stdin_terminal && stdout_terminal => Ok(Frontend::Repl),
        Mode::Repl => Err("repl mode requires terminal stdin and stdout".into()),
        Mode::Jsonl => Ok(Frontend::Jsonl),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_requires_both_terminals_for_repl() {
        assert_eq!(resolve_mode(Mode::Auto, true, true), Ok(Frontend::Repl));
        assert_eq!(resolve_mode(Mode::Auto, true, false), Ok(Frontend::Jsonl));
        assert_eq!(resolve_mode(Mode::Auto, false, true), Ok(Frontend::Jsonl));
    }

    #[test]
    fn explicit_repl_fails_without_two_terminals() {
        assert!(resolve_mode(Mode::Repl, false, true).is_err());
        assert_eq!(resolve_mode(Mode::Jsonl, true, true), Ok(Frontend::Jsonl));
    }
}
