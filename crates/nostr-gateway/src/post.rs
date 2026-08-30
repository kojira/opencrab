//! say → nostaro 投稿。core からの say（返信本文）を発端イベントへの e-tag reply として
//! Nostr に投稿する。inbound 専用だった第1段（row229）を、オーナー裁定 row271/272
//! 「返信/投稿は say(gateway) 一本」に合わせて投稿可能にする。
//!
//! 秘密鍵はプロセス env の `NOSTARO_SECRET_KEY`（setup が注入・#620 で nostaro が config
//! より優先で読む）から child env にだけ渡す。**config には鍵を書かない**（relays のみ）。
//! nostaro の post/reply は `--relay` を取らず config の relays を使うため、relays だけの
//! config を書いて `--config` で渡す。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::secret::SECRET_ENV;

const POST_TIMEOUT: Duration = Duration::from_secs(30);

/// dry-run で say を残す tracing target。テスト・QC がこの target で本文・種別を拾う。
pub const DRY_RUN_LOG_TARGET: &str = "opencrab_nostrgate::dry_run";

/// say の投稿結果。
#[derive(Debug, PartialEq, Eq)]
pub enum SayDelivery {
    /// 発端イベントへの e-tag reply として投稿した（単一メンション）。
    Posted,
    /// 返信先が無い（bundle/曖昧）ため新規ノート（standalone）として publish した（row292/#843:
    /// drop せず必ず出す）。
    PostedStandalone,
    /// 投稿を試みたが失敗（nostaro 非ゼロ / spawn 失敗 / timeout）。
    Failed(String),
}

/// origin から返信先 event_id（hex64）を取り出す。origin は `nostr:event:v1:{lane}:{event_id}`。
pub fn event_id_from_origin(origin: &str) -> Option<String> {
    if !origin.starts_with("nostr:event:v1:") {
        return None;
    }
    let last = origin.rsplit(':').next()?;
    if last.len() == 64 && last.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(last.to_ascii_lowercase())
    } else {
        None
    }
}

/// `nostaro --config <cfg> post -- <text>` の argv（鍵は env・argv に載せない）。
/// DI-16: say は常に新規 post（standalone）。返信は DI `reply` 操作が担う。
/// `--` でオプション終端し、`-` 始まりの text も positional として渡す。
pub fn post_argv(config_path: &Path, text: &str) -> Vec<String> {
    vec![
        "--config".to_string(),
        config_path.to_string_lossy().into_owned(),
        "post".to_string(),
        "--".to_string(),
        text.to_string(),
    ]
}

/// この instance の nostaro post 用 config パス（socket と同じディレクトリ）。
pub fn post_config_path(socket: &Path, instance_id: &str) -> PathBuf {
    socket
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("nostaro-post-{instance_id}.toml"))
}

/// relays だけの nostaro config を書く（**秘密鍵は書かない**・env 注入）。
pub fn write_relays_config(path: &Path, relays: &[String]) -> std::io::Result<()> {
    // TOML の string 配列は JSON と同じ書式（"..." とエスケープ）なので serde_json で安全に組む。
    // nostaro の config は relays と default_relays の両方が必須（欠けると TOML parse error で
    // reply が落ちる）。default_relays は relays と同じ集合を入れる。
    let arr = serde_json::to_string(relays).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(path, format!("relays = {arr}\ndefault_relays = {arr}\n"))
}

/// nsec 等の秘密を潰す（ログ用。config に鍵は載せていないが env 経由の万一の漏れを塞ぐ）。
fn redact_secrets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with("nsec1") {
            out.push_str("nsec1<redacted>");
            i += 5;
            // 続く bech32 文字列を読み飛ばす。
            while i < s.len() && (bytes[i].is_ascii_alphanumeric()) {
                i += 1;
            }
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn build_post_command(bin: &Path, argv: &[String], secret: Option<&str>) -> Command {
    let mut cmd = Command::new(bin);
    cmd.args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(s) = secret {
        cmd.env(SECRET_ENV, s);
    }
    cmd
}

/// say を配送する（DI-16: 常に新規 post = standalone）。返信は DI `reply` 操作が担うので、
/// say に reply target を暗黙設定しない。`reply_origin` は wire 互換のため受けるが使わない。
/// row292/#843 の「返信先が無い say も drop せず publish」は、常に standalone post とする
/// 本設計で完全に満たされる（drop は発生しない）。結果は観測性のため `PostedStandalone` を返す。
///
/// `dry_run=true`（QC ハーネス）のときは publish せず、本文・種別を INFO ログに全文残して
/// core へは成功 ack（`PostedStandalone`）を返す。nostaro も spawn しない。**dry_run=false は
/// 従来どおり standalone post を発行する**。
pub async fn deliver_say(
    nostaro_bin: &Path,
    config_path: &Path,
    secret: Option<&str>,
    _reply_origin: Option<String>,
    text: &str,
    dry_run: bool,
) -> SayDelivery {
    if dry_run {
        // publish せず全文をログに残す（種別は常に standalone = DI-16）。
        tracing::info!(
            target: DRY_RUN_LOG_TARGET,
            kind = "standalone",
            body = %text,
            "DRY_RUN say (not published; standalone post)"
        );
        return SayDelivery::PostedStandalone;
    }
    let argv = post_argv(config_path, text);
    let mut cmd = build_post_command(nostaro_bin, &argv, secret);
    match tokio::time::timeout(POST_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => SayDelivery::PostedStandalone,
        Ok(Ok(out)) => {
            let stderr = redact_secrets(String::from_utf8_lossy(&out.stderr).trim());
            SayDelivery::Failed(format!(
                "nostaro post exit {:?}: {stderr}",
                out.status.code()
            ))
        }
        Ok(Err(e)) => SayDelivery::Failed(format!("nostaro spawn 失敗: {e}")),
        Err(_) => SayDelivery::Failed(format!("nostaro post timeout {}s", POST_TIMEOUT.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_id_from_default_and_watch_origins() {
        let id = "aa".repeat(32);
        assert_eq!(
            event_id_from_origin(&format!("nostr:event:v1:default:{id}")).as_deref(),
            Some(id.as_str())
        );
        assert_eq!(
            event_id_from_origin(&format!("nostr:event:v1:watch:4:{id}")).as_deref(),
            Some(id.as_str())
        );
    }

    #[test]
    fn event_id_rejects_non_event_or_bad_hex() {
        assert_eq!(event_id_from_origin("web:conv:1"), None);
        assert_eq!(event_id_from_origin("nostr:event:v1:default:zz"), None);
        assert_eq!(
            event_id_from_origin("nostr:event:v1:default:not-64-hex"),
            None
        );
    }

    #[test]
    fn post_argv_terminates_options() {
        let argv = post_argv(Path::new("/x/cfg.toml"), "-hello");
        assert_eq!(
            argv,
            vec![
                "--config".to_string(),
                "/x/cfg.toml".to_string(),
                "post".to_string(),
                "--".to_string(),
                "-hello".to_string(),
            ]
        );
    }

    #[test]
    fn relays_config_has_relays_no_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.toml");
        write_relays_config(&path, &["wss://x.example".into(), "wss://r.example".into()]).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            body,
            "relays = [\"wss://x.example\",\"wss://r.example\"]\ndefault_relays = [\"wss://x.example\",\"wss://r.example\"]\n"
        );
        assert!(!body.contains("secret"), "config に鍵行を書かない");
    }

    #[test]
    fn redact_blanks_nsec() {
        let s = "failed: secret_key = \"nsec1abcdef0123\" bad";
        let r = redact_secrets(s);
        assert!(!r.contains("nsec1abcdef0123"), "{r}");
        assert!(r.contains("nsec1<redacted>"), "{r}");
    }

    #[tokio::test]
    async fn no_target_say_is_published_as_standalone_post() {
        // reply_origin=None（bundle/曖昧）は drop せず standalone post で publish（row292/#843）。
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-nostaro");
        let out_file = dir.path().join("invoked.txt");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf 'ARGV:%s\\n' \"$*\" > '{}'\nexit 0\n",
                out_file.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&script).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&script, p).unwrap();
        }
        let cfg = dir.path().join("cfg.toml");
        write_relays_config(&cfg, &["wss://x.example".into()]).unwrap();
        let got =
            deliver_say(&script, &cfg, Some("nsec1fakekey"), None, "エアリプ本文", false).await;
        assert_eq!(got, SayDelivery::PostedStandalone);
        let recorded = std::fs::read_to_string(&out_file).unwrap();
        assert!(
            recorded.contains("post"),
            "post subcommand で無い: {recorded}"
        );
        assert!(
            recorded.contains("エアリプ本文"),
            "本文が渡っていない: {recorded}"
        );
        assert!(
            !recorded.contains("reply"),
            "reply にしてはならない: {recorded}"
        );
    }

    #[tokio::test]
    async fn say_invokes_nostaro_post_with_env_secret() {
        // 実 relay 不要のモック nostaro。argv と env を控えて 0 exit する。
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-nostaro");
        let out_file = dir.path().join("invoked.txt");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n{{ printf 'ARGV:%s\\n' \"$*\"; printf 'ENV:%s\\n' \"$NOSTARO_SECRET_KEY\"; }} > '{}'\nexit 0\n",
                out_file.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&script).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&script, p).unwrap();
        }
        let cfg = dir.path().join("cfg.toml");
        write_relays_config(&cfg, &["wss://x.example".into()]).unwrap();
        // DI-16: say は常に standalone post。reply_origin を渡しても post になる（reply にしない）。
        let origin = format!("nostr:event:v1:default:{}", "bb".repeat(32));
        let got =
            deliver_say(&script, &cfg, Some("nsec1fakekey"), Some(origin), "やあ", false).await;
        assert_eq!(got, SayDelivery::PostedStandalone);
        let recorded = std::fs::read_to_string(&out_file).unwrap();
        assert!(recorded.contains("post"), "post subcommand: {recorded}");
        assert!(!recorded.contains("reply"), "reply にしない: {recorded}");
        assert!(recorded.contains("やあ"), "本文が渡っていない: {recorded}");
        assert!(
            recorded.contains("ENV:nsec1fakekey"),
            "鍵が env で渡っていない: {recorded}"
        );
        assert!(
            !recorded.contains("--relay"),
            "post に --relay は無い（config の relays を使う）: {recorded}"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_is_failed_and_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-nostaro-fail");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'boom nsec1leak0123 oops' 1>&2\nexit 3\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&script).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&script, p).unwrap();
        }
        let cfg = dir.path().join("cfg.toml");
        write_relays_config(&cfg, &["wss://x.example".into()]).unwrap();
        let id = "cc".repeat(32);
        let got = deliver_say(
            &script,
            &cfg,
            Some("nsec1fakekey"),
            Some(format!("nostr:event:v1:default:{id}")),
            "t",
            false,
        )
        .await;
        match got {
            SayDelivery::Failed(msg) => {
                assert!(msg.contains("exit"), "{msg}");
                assert!(
                    !msg.contains("nsec1leak0123"),
                    "stderr の nsec が漏れている: {msg}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dry_run_acks_without_spawning() {
        // dry_run は nostaro を spawn せず（存在しない bin/cfg でも）PostedStandalone を返す。
        let got = deliver_say(
            Path::new("/nonexistent/nostaro"),
            Path::new("/nonexistent/cfg.toml"),
            Some("nsec1fakekey"),
            Some(format!("nostr:event:v1:default:{}", "dd".repeat(32))),
            "了解、やっておくね",
            true,
        )
        .await;
        assert_eq!(got, SayDelivery::PostedStandalone, "dry_run は成功 ack");
    }
}
