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
    /// 平文の `secret_key` 行が実際に残っていて**除去した**永続 config の件数
    /// （既に鍵行が無い config は数えない＝冪等）。
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

        // 1b) 永続 config に**平文の secret_key 行が実際に残っているときだけ**その行を
        //     落とす（relays/default_relays/blossom は保つ）。既に鍵行が無ければ何もしない
        //     ＝冪等（2 回目以降の起動で無条件書き込み＆誤カウントをしない）。
        match opencrab_nostr::NostaroCli::agent_config_path(&row.agent_id) {
            Ok(path) => match strip_secret_key_line_from_config(&path) {
                Ok(true) => report.configs_regenerated += 1,
                Ok(false) => {}
                Err(e) => {
                    warn!(agent_id = %row.agent_id, error = %e, "#620 migration: config の鍵行除去に失敗")
                }
            },
            Err(e) => {
                warn!(agent_id = %row.agent_id, error = %e, "#620 migration: config パス解決に失敗")
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

/// 永続 config から `secret_key = "..."` 行を落とす（relays/default_relays/blossom は保つ）。
///
/// **平文の鍵行が実際に在るときだけ**書き直して `true` を返す。config が無い / 既に鍵行が
/// 無ければ**何もせず** `false`（＝冪等。2 回目以降の起動で無条件に書き込まない）。
fn strip_secret_key_line_from_config(path: &Path) -> std::io::Result<bool> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        // config 未生成＝落とす鍵行も無い。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let has_secret_key = content
        .lines()
        .any(|l| l.trim_start().starts_with("secret_key"));
    if !has_secret_key {
        return Ok(false);
    }
    // secret_key 行だけを除去し、他（relays/default_relays/blossom 等）は保つ。
    let mut stripped: String = content
        .lines()
        .filter(|l| !l.trim_start().starts_with("secret_key"))
        .collect::<Vec<_>>()
        .join("\n");
    stripped.push('\n');
    write_secret_file_0600(path, &stripped)?;
    Ok(true)
}

/// #620: 形式は正しいが**中身が違う**マスターキー（別環境の貼り間違え等）を起動時に捕まえる。
///
/// 既存の暗号文（DB の `enc:` 本鍵）を**1 本試し復号**する。復号できなければ理由を返す
/// （呼び出し側が既存のバナー経路で大きく知らせ、Nostr を起動しない）。暗号文がまだ 1 本も
/// 無ければ（初回・全平文）判定不能なので `None`（通す）。DB を引けないときも `None`（別経路で
/// 扱う）。**新しい設定は足さない**。
pub fn master_key_mismatch_reason(db: &Db, master_key: &MasterKey) -> Option<String> {
    let rows = match db.lock() {
        Ok(conn) => opencrab_db::queries::list_all_agent_nostr_configs(&conn).ok()?,
        Err(_) => return None,
    };
    for row in &rows {
        if secret_box::is_encrypted(&row.secret_key) {
            // 既存の暗号文を 1 本見つけた。これが復号できるかで一致を判定する。
            return match secret_box::decrypt(&row.secret_key, master_key) {
                Ok(_) => None, // 復号成功＝キー一致
                Err(_) => Some(
                    "マスターキーが既存の暗号文と一致しません（別環境のキーを貼り間違えた可能性）"
                        .to_string(),
                ),
            };
        }
    }
    None // 暗号文がまだ無い（初回）＝判定不能なので通す
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(seed: u8) -> MasterKey {
        std::sync::Arc::new(zeroize::Zeroizing::new([seed; 32]))
    }

    fn plaintext_row(
        agent_id: &str,
        secret_key: &str,
    ) -> opencrab_db::queries::AgentNostrConfigRow {
        opencrab_db::queries::AgentNostrConfigRow {
            agent_id: agent_id.to_string(),
            secret_key: secret_key.to_string(),
            relays_json: r#"["wss://relay.test"]"#.to_string(),
            filter_json: "{}".to_string(),
            enabled: false,
        }
    }

    /// #620 指摘1: config の鍵行除去は**平文の鍵行が在るときだけ**書き、冪等。
    #[test]
    fn strip_secret_key_line_is_idempotent_and_preserves_relays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "secret_key = \"nsec1plain\"\nrelays = [\"wss://a\"]\ndefault_relays = [\"wss://a\"]\nblossom_server = \"https://b\"\n",
        )
        .unwrap();

        // 1 回目: 鍵行が在るので除去して true。
        assert!(strip_secret_key_line_from_config(&path).unwrap());
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("secret_key"), "鍵行が残った: {after}");
        assert!(!after.contains("nsec1plain"));
        // relays/default_relays/blossom は保つ。
        assert!(after.contains("relays = [\"wss://a\"]"));
        assert!(after.contains("default_relays = [\"wss://a\"]"));
        assert!(after.contains("blossom_server = \"https://b\""));

        // 2 回目: 鍵行が無いので何もしない（false）。
        assert!(!strip_secret_key_line_from_config(&path).unwrap());
        // 未生成の config も false（NotFound）。
        assert!(!strip_secret_key_line_from_config(&dir.path().join("nope.toml")).unwrap());
    }

    /// #620 指摘1: 2 回目の起動で移行が **no-op**（`changed_anything()` が false）になる。
    /// config regen は CWD の `data/agents` を触るため、ここでは config を持たない agent id を
    /// 使い（1b は NotFound で false）、DB 本鍵と生成鍵ファイルの冪等を固定する。
    #[test]
    fn migration_second_run_is_noop() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let key = mk(7);
        let agent = "mig-idem-agent-zzz";

        // 平文の DB 本鍵。
        {
            let c = db.lock().unwrap();
            opencrab_db::queries::upsert_agent_nostr_config(
                &c,
                &plaintext_row(agent, "nsec1plain"),
            )
            .unwrap();
        }
        // 平文の生成鍵ファイル（temp agents_dir 配下）。
        let agents_dir = tempfile::tempdir().unwrap();
        let gen_dir = agents_dir
            .path()
            .join(agent)
            .join("nostr")
            .join("generated-keys");
        std::fs::create_dir_all(&gen_dir).unwrap();
        std::fs::write(gen_dir.join("npub1x.nsec"), "nsec1genplain").unwrap();
        // 旧 from-config（削除対象）。
        std::fs::write(
            gen_dir.join("npub1x.config.toml"),
            "secret_key = \"nsec1x\"",
        )
        .unwrap();

        // 1 回目: 何か移行される。
        let r1 = migrate_nostr_secrets_at_rest(&db, &key, agents_dir.path());
        assert!(r1.changed_anything(), "1 回目は移行が起きるはず: {r1:?}");
        assert_eq!(r1.db_keys_encrypted, 1);
        assert_eq!(r1.generated_keys_encrypted, 1);
        assert_eq!(r1.from_configs_deleted, 1);

        // DB 本鍵・生成鍵ファイルが暗号文になっている。
        {
            let c = db.lock().unwrap();
            let row = opencrab_db::queries::get_agent_nostr_config(&c, agent)
                .unwrap()
                .unwrap();
            assert!(secret_box::is_encrypted(&row.secret_key));
        }
        let gen = std::fs::read_to_string(gen_dir.join("npub1x.nsec")).unwrap();
        assert!(secret_box::is_encrypted(gen.trim()));

        // 2 回目: 対象が無いので **no-op**。
        let r2 = migrate_nostr_secrets_at_rest(&db, &key, agents_dir.path());
        assert!(
            !r2.changed_anything(),
            "2 回目は何も移行してはいけない（冪等）: {r2:?}"
        );
    }

    /// #620 指摘3: 形式は正しいが**中身が違う**マスターキーを、既存の暗号文の試し復号で捕まえる。
    #[test]
    fn master_key_mismatch_is_detected() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let key_a = mk(1);
        let key_b = mk(2);
        let agent = "mismatch-agent";

        // まず平文で入れ、key_a で暗号化（DB 本鍵だけ・config は触らない別 agent id）。
        {
            let c = db.lock().unwrap();
            opencrab_db::queries::upsert_agent_nostr_config(
                &c,
                &plaintext_row(agent, "nsec1plain"),
            )
            .unwrap();
            let enc = secret_box::encrypt(b"nsec1plain", &key_a).unwrap();
            opencrab_db::queries::set_agent_nostr_config_secret_key(&c, agent, &enc).unwrap();
        }

        // 正しいキーは None（一致）。
        assert!(master_key_mismatch_reason(&db, &key_a).is_none());
        // 取り違えたキーは Some（不一致）。
        assert!(master_key_mismatch_reason(&db, &key_b).is_some());
    }

    /// 暗号文がまだ 1 本も無い（初回・全平文）なら判定不能なので通す（None）。
    #[test]
    fn master_key_mismatch_none_when_no_ciphertext_yet() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        {
            let c = db.lock().unwrap();
            opencrab_db::queries::upsert_agent_nostr_config(
                &c,
                &plaintext_row("fresh-agent", "nsec1stillplain"),
            )
            .unwrap();
        }
        assert!(master_key_mismatch_reason(&db, &mk(9)).is_none());
    }
}
