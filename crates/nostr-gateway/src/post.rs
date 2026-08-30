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

/// say の投稿結果。
#[derive(Debug, PartialEq, Eq)]
pub enum SayDelivery {
    /// e-tag reply として投稿した。
    Posted,
    /// 返信先が無い（bundle/曖昧）ため投稿しなかった。エアリプは nostr_post が担う。
    Dropped,
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

/// `nostaro --config <cfg> reply -- <target> <text>` の argv（鍵は env・argv に載せない）。
/// `--` でオプション終端し、`-` 始まりの target/text も positional として渡す。
pub fn reply_argv(config_path: &Path, target: &str, text: &str) -> Vec<String> {
    vec![
        "--config".to_string(),
        config_path.to_string_lossy().into_owned(),
        "reply".to_string(),
        "--".to_string(),
        target.to_string(),
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

fn build_reply_command(bin: &Path, argv: &[String], secret: Option<&str>) -> Command {
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

/// say を配送する。`reply_origin=None`（bundle/曖昧）は投稿しない（Dropped）。
/// `Some(origin)` は origin の event_id へ e-tag reply する。
pub async fn deliver_say(
    nostaro_bin: &Path,
    config_path: &Path,
    secret: Option<&str>,
    reply_origin: Option<String>,
    text: &str,
) -> SayDelivery {
    let Some(origin) = reply_origin else {
        // say は特定ノートへの返信専用。TL 束ね（単一返信先が無い）への自発反応は
        // エアリプ（nostr_post）が担うので、gateway からは投稿しない。
        return SayDelivery::Dropped;
    };
    let Some(target) = event_id_from_origin(&origin) else {
        return SayDelivery::Failed(format!("origin から event_id を復元できない: {origin}"));
    };
    let argv = reply_argv(config_path, &target, text);
    let mut cmd = build_reply_command(nostaro_bin, &argv, secret);
    match tokio::time::timeout(POST_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => SayDelivery::Posted,
        Ok(Ok(out)) => {
            let stderr = redact_secrets(String::from_utf8_lossy(&out.stderr).trim());
            SayDelivery::Failed(format!(
                "nostaro reply exit {:?}: {stderr}",
                out.status.code()
            ))
        }
        Ok(Err(e)) => SayDelivery::Failed(format!("nostaro spawn 失敗: {e}")),
        Err(_) => SayDelivery::Failed(format!(
            "nostaro reply timeout {}s",
            POST_TIMEOUT.as_secs()
        )),
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
    fn reply_argv_terminates_options_and_orders_target_text() {
        let argv = reply_argv(Path::new("/x/cfg.toml"), "aa", "-hello");
        assert_eq!(
            argv,
            vec![
                "--config".to_string(),
                "/x/cfg.toml".to_string(),
                "reply".to_string(),
                "--".to_string(),
                "aa".to_string(),
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
    async fn bundle_origin_say_is_dropped_not_posted() {
        // reply_origin=None は投稿せず Dropped（nostaro を spawn しない）。
        let out = deliver_say(
            Path::new("/nonexistent/nostaro"),
            Path::new("/nonexistent/cfg.toml"),
            None,
            None,
            "本文",
        )
        .await;
        assert_eq!(out, SayDelivery::Dropped);
    }

    #[tokio::test]
    async fn reply_invokes_nostaro_with_target_and_env_secret() {
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
        let id = "bb".repeat(32);
        let origin = format!("nostr:event:v1:default:{id}");
        let got = deliver_say(&script, &cfg, Some("nsec1fakekey"), Some(origin), "やあ").await;
        assert_eq!(got, SayDelivery::Posted);
        let recorded = std::fs::read_to_string(&out_file).unwrap();
        assert!(recorded.contains("reply"), "{recorded}");
        assert!(recorded.contains(&id), "target が渡っていない: {recorded}");
        assert!(recorded.contains("やあ"), "本文が渡っていない: {recorded}");
        assert!(
            recorded.contains("ENV:nsec1fakekey"),
            "鍵が env で渡っていない: {recorded}"
        );
        assert!(
            !recorded.contains("--relay"),
            "post/reply に --relay は無い（config の relays を使う）: {recorded}"
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
        )
        .await;
        match got {
            SayDelivery::Failed(msg) => {
                assert!(msg.contains("exit"), "{msg}");
                assert!(!msg.contains("nsec1leak0123"), "stderr の nsec が漏れている: {msg}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
