//! フォローリスト（kind:3）を admission のフォロイー集合へ反映する（#698 の gateway 側実装）。
//!
//! gateway store の固定 access に、自分のフォローリストを定期取得して足す。取得に失敗したら
//! 前回値を保持する（全通しにも空にもしない）。core は関与しない。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::process::Command;

use crate::config::AccessConfig;
use crate::map::normalize_author_id;
use crate::secret::SECRET_ENV;

/// 旧 Nostr manager（#698）と同じ更新間隔。
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(300);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// admission が読む access。フォローリスト更新で差し替わる。
pub type SharedAccess = Arc<RwLock<AccessConfig>>;

/// store 由来の固定 access に、取得したフォロイーを足した access を作る。
pub fn merge_followees(base: &AccessConfig, fetched: &BTreeSet<String>) -> AccessConfig {
    let mut merged = base.clone();
    let mut set: BTreeSet<String> = base
        .followees
        .iter()
        .filter_map(|k| normalize_author_id(k))
        .collect();
    set.extend(fetched.iter().cloned());
    merged.followees = set.into_iter().collect();
    merged
}

/// `nostaro following --out-format json` の `{"users":[{"hex",..}]}` を hex 集合にする。
pub fn parse_following_json(raw: &str) -> anyhow::Result<BTreeSet<String>> {
    let v: serde_json::Value = serde_json::from_str(raw)?;
    let users = v
        .get("users")
        .and_then(|u| u.as_array())
        .ok_or_else(|| anyhow::anyhow!("following JSON has no users array"))?;
    Ok(users
        .iter()
        .filter_map(|u| u.get("hex").and_then(|h| h.as_str()))
        .filter_map(normalize_author_id)
        .collect())
}

pub fn following_out_path(post_config: &Path, instance_id: &str) -> PathBuf {
    post_config
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("nostaro-following-{instance_id}.json"))
}

/// 自分のフォローリストを nostaro で取得する。読むだけで署名はしない。
/// nostaro はクライアント生成に鍵を要するので、watch と同じく env でだけ渡す。
pub async fn fetch_following(
    nostaro_bin: &Path,
    config_path: &Path,
    secret: Option<&str>,
    self_pubkey: &str,
    out_path: &Path,
) -> anyhow::Result<BTreeSet<String>> {
    let _ = std::fs::remove_file(out_path);
    let mut cmd = Command::new(nostaro_bin);
    cmd.arg("--config")
        .arg(config_path)
        .arg("following")
        .arg(self_pubkey)
        .arg(format!("--out={}", out_path.display()))
        .arg("--out-format=json")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(s) = secret {
        cmd.env(SECRET_ENV, s);
    }
    let status = tokio::time::timeout(FETCH_TIMEOUT, cmd.status())
        .await
        .map_err(|_| anyhow::anyhow!("nostaro following timed out"))??;
    if !status.success() {
        anyhow::bail!("nostaro following exited with {:?}", status.code());
    }
    let raw = std::fs::read_to_string(out_path)?;
    let _ = std::fs::remove_file(out_path);
    parse_following_json(&raw)
}

/// 1 回取得して反映する。失敗なら前回値を保持して warn。
pub async fn refresh_once(
    shared: &SharedAccess,
    base: &AccessConfig,
    nostaro_bin: &Path,
    config_path: &Path,
    secret: Option<&str>,
    self_pubkey: &str,
    out_path: &Path,
) {
    match fetch_following(nostaro_bin, config_path, secret, self_pubkey, out_path).await {
        Ok(fetched) => {
            let next = merge_followees(base, &fetched);
            let n = next.followees.len();
            *shared.write().unwrap_or_else(|e| e.into_inner()) = next;
            tracing::info!(followees = n, "follow list applied to admission");
        }
        Err(e) => {
            tracing::warn!(error = %e, "follow list refresh failed; keeping previous followees");
        }
    }
}

/// 起動直後に 1 回、その後 `REFRESH_INTERVAL` ごとに更新し続ける。
pub fn spawn_refresher(
    shared: SharedAccess,
    base: AccessConfig,
    nostaro_bin: PathBuf,
    config_path: PathBuf,
    secret: Option<Arc<String>>,
    self_pubkey: String,
    out_path: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(REFRESH_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            refresh_once(
                &shared,
                &base,
                &nostaro_bin,
                &config_path,
                secret.as_ref().map(|s| s.as_str()),
                &self_pubkey,
                &out_path,
            )
            .await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::admit;
    use crate::map::WatchEvent;
    use opencrab_gate_client::wire::SaidCaller;

    fn event(author: &str) -> WatchEvent {
        WatchEvent {
            id: "aa".repeat(32),
            pubkey: author.into(),
            npub: None,
            note_id: None,
            created_at: 1,
            kind: 1,
            content: "のすたろう".into(),
            tags: vec![],
        }
    }

    fn fake_nostaro(dir: &Path, body: &str) -> PathBuf {
        let script = dir.join("fake-nostaro");
        std::fs::write(&script, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&script).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&script, p).unwrap();
        script
    }

    /// 本番で起きた取りこぼし: owner だけの store access ではフォロー中の作者が落ちる。
    /// フォローリスト反映後は Agent として admission を通る。失敗時は前回値を保持する。
    #[tokio::test]
    async fn followed_author_is_admitted_after_refresh_and_kept_on_failure() {
        let _env = crate::ENV_LOCK.lock().await;
        let self_key = "11".repeat(32);
        let owner = "22".repeat(32);
        let followed = "33".repeat(32);
        let base = AccessConfig {
            owner: vec![owner.clone()],
            ..Default::default()
        };
        let shared: SharedAccess = Arc::new(RwLock::new(base.clone()));
        assert_eq!(
            admit(&event(&followed), &self_key, &shared.read().unwrap()),
            None
        );

        let dir = tempfile::tempdir().unwrap();
        // 引数を検証し、--out=<path> へ following JSON を書く偽 nostaro。
        let ok = fake_nostaro(
            dir.path(),
            &format!(
                "#!/bin/sh\n[ \"$3\" = following ] && [ \"$4\" = {self_key} ] && [ \"$6\" = --out-format=json ] || exit 9\n\
                 out=${{5#--out=}}\nprintf '{{\"count\":1,\"users\":[{{\"hex\":\"{followed}\",\"npub\":\"x\"}}]}}' > \"$out\"\n"
            ),
        );
        let out = dir.path().join("following.json");
        let cfg = dir.path().join("cfg.toml");
        refresh_once(&shared, &base, &ok, &cfg, None, &self_key, &out).await;
        assert_eq!(
            admit(&event(&followed), &self_key, &shared.read().unwrap()),
            Some(SaidCaller::Agent)
        );
        assert_eq!(
            admit(&event(&owner), &self_key, &shared.read().unwrap()),
            Some(SaidCaller::Owner)
        );

        let failing = fake_nostaro(dir.path(), "#!/bin/sh\nexit 1\n");
        refresh_once(&shared, &base, &failing, &cfg, None, &self_key, &out).await;
        assert_eq!(
            admit(&event(&followed), &self_key, &shared.read().unwrap()),
            Some(SaidCaller::Agent),
            "a failed refresh must keep the previous followees"
        );
    }

    #[test]
    fn parse_rejects_broken_json_and_skips_bad_entries() {
        assert!(parse_following_json("{").is_err());
        assert!(parse_following_json("{\"count\":0}").is_err());
        let ok = format!(
            "{{\"users\":[{{\"hex\":\"{}\"}},{{\"npub\":\"npub1x\"}},{{\"hex\":\"zz\"}}]}}",
            "AB".repeat(32)
        );
        let set = parse_following_json(&ok).unwrap();
        assert_eq!(set.into_iter().collect::<Vec<_>>(), vec!["ab".repeat(32)]);
    }
}
