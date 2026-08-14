//! #620: Nostr の at-rest 秘密（DB 本鍵・生成鍵ファイル・永続 config）を平文→暗号文へ
//! 移行する。**起動時に 1 回・冪等**（`enc:v1:` 済みはスキップ）。マスターキーが在るときだけ
//! 呼ぶ（＝ Nostr サブシステムを起動する構成）。暗号化対象が無ければ何もしない。
//!
//! 削除（平文 from-config）は**列挙してログに出してから、別ループで個別に削除**する
//! （glob 展開を使わない）。

use std::path::{Path, PathBuf};

use opencrab_core::secret_box;
use opencrab_db::Db;
use opencrab_nostr::MasterKey;
use tracing::{error, info, warn};

/// 移行結果のサマリ（起動ログに出す）。
#[derive(Debug, Default, Clone, Copy)]
pub struct MigrationReport {
    /// 平文だった DB 本鍵を暗号化した件数。
    pub db_keys_encrypted: usize,
    /// 鍵行を落として再生成した永続 config の件数。
    pub configs_regenerated: usize,
    /// 平文だった生成鍵ファイルを暗号化した件数。
    pub generated_keys_encrypted: usize,
    /// 削除した平文 from-config（`*.config.toml`）の件数。
    pub from_configs_deleted: usize,
}

impl MigrationReport {
    /// 何か変更したか（ログの出し分け用）。
    pub fn changed_anything(&self) -> bool {
        self.db_keys_encrypted > 0
            || self.configs_regenerated > 0
            || self.generated_keys_encrypted > 0
            || self.from_configs_deleted > 0
    }
}

/// 起動時の at-rest 移行本体。`agents_dir` は `data/agents`（各 `<id>/nostr/...` の親）。
pub fn migrate_nostr_secrets_at_rest(
    db: &Db,
    master_key: &MasterKey,
    agents_dir: &Path,
) -> MigrationReport {
    let mut report = MigrationReport::default();

    // 1) DB 本鍵の暗号化 + 永続 config の鍵行なし再生成。
    let rows = match db.lock() {
        Ok(conn) => opencrab_db::queries::list_all_agent_nostr_configs(&conn).unwrap_or_default(),
        Err(e) => {
            error!(error = %e, "#620 migration: DB ロック取得に失敗（移行を見送る）");
            Vec::new()
        }
    };
    for row in &rows {
        // 1a) DB 本鍵が平文なら暗号化して書き戻す（空はスキップ・`enc:` はスキップ）。
        let sk = row.secret_key.trim();
        if !sk.is_empty() && !secret_box::is_encrypted(sk) {
            match secret_box::encrypt(sk.as_bytes(), master_key) {
                Ok(enc) => match db.lock() {
                    Ok(conn) => {
                        match opencrab_db::queries::set_agent_nostr_config_secret_key(
                            &conn,
                            &row.agent_id,
                            &enc,
                        ) {
                            Ok(_) => report.db_keys_encrypted += 1,
                            Err(e) => {
                                warn!(agent_id = %row.agent_id, error = %e, "#620 migration: DB 本鍵の暗号化書き戻しに失敗")
                            }
                        }
                    }
                    Err(e) => {
                        warn!(agent_id = %row.agent_id, error = %e, "#620 migration: DB ロック取得に失敗（本鍵）")
                    }
                },
                Err(e) => {
                    error!(agent_id = %row.agent_id, error = %e, "#620 migration: 本鍵の暗号化に失敗")
                }
            }
        }

        // 1b) 永続 config を鍵行なしで再生成（relays を DB から継承）。既に鍵行が無ければ
        //     no-op 相当（同じ内容で上書きするだけ）。
        let cfg = opencrab_nostr::config_from_row(row);
        match opencrab_nostr::NostaroCli::materialize_config(
            &row.agent_id,
            &cfg.effective_relays(),
            None,
        ) {
            Ok(_) => report.configs_regenerated += 1,
            Err(e) => {
                warn!(agent_id = %row.agent_id, error = %e, "#620 migration: config 再生成に失敗")
            }
        }
    }

    // 2) 生成鍵ファイルの暗号化 + 平文 from-config の削除（FS 走査）。
    migrate_generated_key_files(agents_dir, master_key, &mut report);

    report
}

/// `data/agents/*/nostr/generated-keys/` を走査し、平文 `*.nsec` を暗号化、平文 `*.config.toml`
/// （旧 from-config）を削除する。
fn migrate_generated_key_files(
    agents_dir: &Path,
    master_key: &MasterKey,
    report: &mut MigrationReport,
) {
    let agents = match std::fs::read_dir(agents_dir) {
        Ok(e) => e,
        // agents ディレクトリが無い＝移行対象なし。
        Err(_) => return,
    };
    for agent in agents.flatten() {
        let gen_dir = agent.path().join("nostr").join("generated-keys");
        let entries = match std::fs::read_dir(&gen_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        // 削除対象（from-config）は**先に列挙**してから、別ループで個別に削除する。
        let mut to_delete: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            // 通常ファイルのみ（ディレクトリ / symlink は触らない）。
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".config.toml") {
                // 旧 from-config（平文 secret_key を含みうる）。削除対象に積む。
                to_delete.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) == Some("nsec") {
                // 平文なら暗号化して**アトミックに置換**（旧平文は残らない）。
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let c = content.trim();
                    if !c.is_empty() && !secret_box::is_encrypted(c) {
                        match secret_box::encrypt(c.as_bytes(), master_key) {
                            Ok(enc) => {
                                if write_secret_file_0600(&path, &enc).is_ok() {
                                    report.generated_keys_encrypted += 1;
                                } else {
                                    warn!(path = %path.display(), "#620 migration: 生成鍵の暗号化置換に失敗");
                                }
                            }
                            Err(e) => {
                                error!(path = %path.display(), error = %e, "#620 migration: 生成鍵の暗号化に失敗")
                            }
                        }
                    }
                }
            }
        }
        // 列挙表示 → 別処理で削除（glob 展開を使わず個別に消す）。
        for p in &to_delete {
            info!(path = %p.display(), "#620 migration: 平文 from-config を削除対象に列挙");
        }
        for p in to_delete {
            match std::fs::remove_file(&p) {
                Ok(_) => report.from_configs_deleted += 1,
                Err(e) => {
                    warn!(path = %p.display(), error = %e, "#620 migration: from-config の削除に失敗")
                }
            }
        }
    }
}

/// 秘密（暗号文）を 0600 でアトミックに（tmp→rename）書く。cli.rs の同名ヘルパと同型だが
/// crate 越しには呼べないので移行用に持つ。
fn write_secret_file_0600(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("migrate.tmp.{}", std::process::id()));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all().ok();
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp, contents)?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}
