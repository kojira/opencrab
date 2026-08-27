//! nostaro watch 子プロセス。EOF で 5 秒後に再購読。鍵は child env だけ。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::config::{effective_kinds, WatchFilter};
use crate::secret::SECRET_ENV;

pub const RESUBSCRIBE: Duration = Duration::from_secs(5);

pub fn plan_watch_args(relays: &[String], filter: &WatchFilter) -> Vec<String> {
    let mut args = vec![
        "watch".to_string(),
        "--json".to_string(),
        "--match=any".to_string(),
    ];
    for relay in relays {
        args.push(format!("--relay={relay}"));
    }
    for author in &filter.authors {
        args.push(format!("--author={author}"));
    }
    for keyword in &filter.keywords {
        args.push(format!("--keyword={keyword}"));
    }
    for kind in effective_kinds(filter) {
        args.push(format!("--kind={kind}"));
    }
    args
}

pub fn build_watch_command(
    bin: &Path,
    relays: &[String],
    filter: &WatchFilter,
    secret: Option<&str>,
) -> Command {
    let args = plan_watch_args(relays, filter);
    tracing::info!(program = %bin.display(), ?args, "spawn nostaro watch");
    let mut cmd = Command::new(bin);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(secret) = secret {
        cmd.env(SECRET_ENV, secret);
    }
    cmd
}

pub async fn run_watch_once(
    bin: &Path,
    relays: &[String],
    filter: &WatchFilter,
    secret: Option<&str>,
    mut on_line: impl FnMut(String),
) -> anyhow::Result<()> {
    let mut child = build_watch_command(bin, relays, filter, secret)
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn nostaro watch: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("nostaro watch produced no stdout handle"))?;
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        on_line(line);
    }
    let _ = child.wait().await;
    Ok(())
}

pub async fn run_watch_loop(
    bin: PathBuf,
    relays: Vec<String>,
    filter: WatchFilter,
    secret: Option<Arc<String>>,
    resubscribe: Duration,
    mut on_line: impl FnMut(String),
) {
    loop {
        let secret_ref = secret.as_deref().map(String::as_str);
        if let Err(e) = run_watch_once(&bin, &relays, &filter, secret_ref, &mut on_line).await {
            tracing::error!(error = %e, "watch child failed");
        }
        tokio::time::sleep(resubscribe).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::SECRET_ENV;

    #[test]
    fn args_never_contain_secret() {
        let secret = "nsec1verysecretvalue";
        let args = plan_watch_args(
            &["wss://example.invalid".into()],
            &WatchFilter::default(),
        );
        assert!(args.iter().all(|a| !a.contains(secret)));
        assert!(args.iter().all(|a| !a.contains("NOSTARO")));
        assert_eq!(args[0], "watch");
        assert!(args.contains(&"--json".to_string()));
        assert!(args.contains(&"--match=any".to_string()));
        assert!(args.iter().any(|a| a == "--kind=1"));
        let rendered = format!("{args:?}");
        assert!(!rendered.contains(secret));
    }

    #[tokio::test]
    async fn child_receives_env_secret_not_argv() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-nostaro");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'ARGV:%s\\n' \"$*\"\nprintf 'ENV:%s\\n' \"$NOSTARO_SECRET_KEY\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&script).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&script, p).unwrap();
        }
        let secret = "nsec1childonly";
        let mut child = build_watch_command(
            &script,
            &["wss://example.invalid".into()],
            &WatchFilter::default(),
            Some(secret),
        )
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let mut argv_line = String::new();
        let mut env_line = String::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            if line.starts_with("ARGV:") {
                argv_line = line;
            } else if line.starts_with("ENV:") {
                env_line = line;
            }
        }
        let _ = child.wait().await;
        assert!(!argv_line.contains(secret), "{argv_line}");
        assert_eq!(env_line, format!("ENV:{secret}"));
        assert!(std::env::var(SECRET_ENV).is_err());
    }
}
