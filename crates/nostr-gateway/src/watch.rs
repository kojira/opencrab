//! nostaro watch 子プロセス。EOF で 5 秒後に再購読。鍵は child env だけ。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::process::Command;

use crate::config::{effective_kinds, mention_lane_filter, InstanceConfig, WatchFilter};
use crate::secret::SECRET_ENV;

pub const RESUBSCRIBE: Duration = Duration::from_secs(5);

/// 偽 watch が fixture の追記を拾いにいくポーリング間隔。連続シナリオの流し込みタイミングは
/// この粒度で観測できる。
const FAKE_WATCH_POLL: Duration = Duration::from_millis(25);

pub fn plan_watch_args(relays: &[String], filter: &WatchFilter) -> Vec<String> {
    let mut args = vec![
        "watch".to_string(),
        "--json".to_string(),
        "--match=any".to_string(),
    ];
    if let Some(npub) = &filter.npub {
        args.push(format!("--npub={npub}"));
    }
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

/// メンション車線（default lane）の nostaro argv。
/// `name` が `--keyword`、`self_pubkey` が `--npub`（p タグ対象）。hex は keyword にしない。
pub fn plan_mention_lane_args(relays: &[String], cfg: &InstanceConfig) -> Vec<String> {
    plan_watch_args(relays, &mention_lane_filter(cfg))
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

/// QC ハーネス用の「偽 watch」。`nostaro watch` を spawn せず、指定 JSONL fixture の各行を
/// `on_line` へ流す。実 nostaro の WatchEvent JSONL と同一形式（1 行 = 1 イベント）。
///
/// fixture を最後まで流したあとも戻らず、ファイルを tail-follow して**後から追記された行**を
/// 拾い続ける（実 watch が購読を保持して新着を流すのと同じ振る舞い）。これにより連続シナリオ
/// （sleep 走行中の別依頼など）を fixture 追記のタイミングで決定的に駆動できる。鍵もリレーも
/// 使わない。改行未終端の部分行は完全な行になるまで送らない。
pub async fn run_fake_watch_once(
    fixture: &Path,
    mut on_line: impl FnMut(String),
) -> anyhow::Result<()> {
    tracing::warn!(
        fixture = %fixture.display(),
        "FAKE watch active — streaming fixture instead of spawning nostaro (QC harness; not production)"
    );
    // 送信済みバイト位置。完全な行を送るたびに進める。
    let mut pos: u64 = 0;
    loop {
        match tokio::fs::File::open(fixture).await {
            Ok(mut file) => {
                let len = file.metadata().await?.len();
                if len < pos {
                    // truncate / rotate されたら先頭から読み直す。
                    pos = 0;
                }
                if len > pos {
                    file.seek(std::io::SeekFrom::Start(pos)).await?;
                    let mut reader = BufReader::new(file);
                    let mut buf = String::new();
                    loop {
                        buf.clear();
                        let n = reader.read_line(&mut buf).await?;
                        if n == 0 {
                            break; // EOF
                        }
                        if buf.ends_with('\n') {
                            pos += n as u64;
                            let line = buf.trim_end_matches(['\n', '\r']).to_string();
                            if !line.is_empty() {
                                on_line(line);
                            }
                        } else {
                            // 改行未達の部分行。今回は送らず、追記されて完全になるのを待つ。
                            break;
                        }
                    }
                }
            }
            Err(_) => {
                // fixture がまだ無い/一時的に読めない。少し待って再試行。
            }
        }
        tokio::time::sleep(FAKE_WATCH_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::SECRET_ENV;

    #[test]
    fn args_never_contain_secret() {
        let secret = "nsec1verysecretvalue";
        let args = plan_watch_args(&["wss://example.invalid".into()], &WatchFilter::default());
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
        // 子 spawn は environ を読むので env 書換テストと直列化する（#868）。
        let _env = crate::ENV_LOCK.lock().await;
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

    #[test]
    fn mention_lane_spawn_argv_has_mention_keywords() {
        let self_pk = "aa".repeat(32);
        let cfg = InstanceConfig {
            relays: vec!["wss://example.invalid".into()],
            filter: WatchFilter::default(),
            self_pubkey: self_pk.clone(),
            name: Some("crab".into()),
            watches: vec![],
            delivery_mode: None,
        };
        let args = plan_mention_lane_args(&cfg.relays, &cfg);
        assert_eq!(args[0], "watch");
        assert!(args.contains(&"--json".to_string()), "{args:?}");
        assert!(args.contains(&"--match=any".to_string()), "{args:?}");
        assert!(
            args.contains(&"--keyword=crab".to_string()),
            "name keyword missing: {args:?}"
        );
        assert!(
            args.contains(&format!("--npub={self_pk}")),
            "p-tag target missing: {args:?}"
        );
        assert!(
            !args.contains(&format!("--keyword={self_pk}")),
            "hex pubkey must not be a keyword: {args:?}"
        );
        let keyword_count = args.iter().filter(|a| a.starts_with("--keyword=")).count();
        assert_eq!(keyword_count, 1, "{args:?}");
        assert!(args.contains(&"--kind=1".to_string()), "{args:?}");
        assert!(args.contains(&"--kind=7".to_string()), "{args:?}");
        assert!(
            !args.iter().any(|a| a.starts_with("--no-mention-only")),
            "{args:?}"
        );
    }

    #[test]
    fn mention_lane_spawn_argv_never_empty_net() {
        let self_pk = "bb".repeat(32);
        let cfg = InstanceConfig {
            relays: vec!["wss://example.invalid".into()],
            filter: WatchFilter::default(),
            self_pubkey: self_pk.clone(),
            name: Some("くらぶ".into()),
            watches: vec![],
            delivery_mode: None,
        };
        let args = plan_mention_lane_args(&cfg.relays, &cfg);
        assert!(
            args.contains(&"--keyword=くらぶ".to_string()),
            "条件ゼロの空網: {args:?}"
        );
        assert!(
            args.contains(&format!("--npub={self_pk}")),
            "p-tag target missing: {args:?}"
        );
        assert!(args.contains(&"--kind=1".to_string()), "{args:?}");
        assert!(args.contains(&"--kind=7".to_string()), "{args:?}");
        assert!(
            !args.contains(&format!("--keyword={self_pk}")),
            "hex pubkey must not be a keyword: {args:?}"
        );
    }

    async fn wait_until<F: Fn() -> bool>(pred: F) -> bool {
        for _ in 0..200 {
            if pred() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        pred()
    }

    #[tokio::test]
    async fn fake_watch_streams_initial_lines_then_tails_appends() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("watch.jsonl");
        // 初期 2 行（1 行 = 1 WatchEvent JSONL）。
        std::fs::write(
            &fixture,
            "{\"id\":\"aa\",\"pubkey\":\"p1\",\"created_at\":1,\"kind\":1}\n\
             {\"id\":\"bb\",\"pubkey\":\"p2\",\"created_at\":2,\"kind\":1}\n",
        )
        .unwrap();
        let got = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = got.clone();
        let path = fixture.clone();
        let task = tokio::spawn(async move {
            let _ = run_fake_watch_once(&path, move |line| {
                sink.lock().unwrap().push(line);
            })
            .await;
        });
        // 初期 2 行が流れる。
        assert!(
            wait_until(|| got.lock().unwrap().len() == 2).await,
            "初期行が流れない: {:?}",
            got.lock().unwrap()
        );
        // 後から 1 行追記 → tail-follow で拾う（連続シナリオの流し込み口）。
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&fixture)
                .unwrap();
            writeln!(
                f,
                "{{\"id\":\"cc\",\"pubkey\":\"p3\",\"created_at\":3,\"kind\":1}}"
            )
            .unwrap();
        }
        assert!(
            wait_until(|| got.lock().unwrap().len() == 3).await,
            "追記行を拾えない: {:?}",
            got.lock().unwrap()
        );
        let lines = got.lock().unwrap().clone();
        assert!(lines[0].contains("\"id\":\"aa\""), "{lines:?}");
        assert!(lines[1].contains("\"id\":\"bb\""), "{lines:?}");
        assert!(lines[2].contains("\"id\":\"cc\""), "{lines:?}");
        task.abort();
    }

    #[tokio::test]
    async fn fake_watch_holds_partial_line_until_newline() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("watch.jsonl");
        // 改行で終わらない部分行だけ先に置く。
        std::fs::write(&fixture, "{\"id\":\"aa\",\"pubkey\":\"p1\"").unwrap();
        let got = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = got.clone();
        let path = fixture.clone();
        let task = tokio::spawn(async move {
            let _ = run_fake_watch_once(&path, move |line| {
                sink.lock().unwrap().push(line);
            })
            .await;
        });
        // 部分行は送られない。
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(got.lock().unwrap().len(), 0, "部分行を送ってはいけない");
        // 残りを追記して行を完成させる → 1 行として流れる。
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&fixture)
                .unwrap();
            writeln!(f, ",\"created_at\":1,\"kind\":1}}").unwrap();
        }
        assert!(
            wait_until(|| got.lock().unwrap().len() == 1).await,
            "完成した行が流れない"
        );
        assert!(got.lock().unwrap()[0].contains("\"created_at\":1"));
        task.abort();
    }
}
