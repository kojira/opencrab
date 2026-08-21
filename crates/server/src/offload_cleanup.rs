//! 退避ファイル（`workspace/tmp/`）の掃除ループ（#711）。
//!
//! ツール結果が inline 上限を超えると `tool_result_log::offload_to_workspace` が本文を
//! `<workspace_root>/tmp/{session}-{tool_call_id}.{ext}` へ書き出す（書くのはこの 1 経路
//! だけ）。しかし**消す実装がコードのどこにも無く**、退避ファイルは無限に増える
//! （実測 206MB / 2,636 個、+44 個/日）。このモジュールが唯一の掃除経路。
//!
//! # 何を消すのか（読み手の失効から導く保持軸）
//!
//! 退避ファイルを読むのは **LLM エージェント自身だけ**で、しかも退避 notice（パスを運ぶ
//! 平文）が会話の再構成窓に残っている間に、自分の意思で `ws_read`/`execute_shell` を叩いた
//! ときに限る。ファイルは write-once なので **mtime = 生成時刻**。生成から `retention_days`
//! 経過したファイルは、そのセッションがアイドルか、窓が多数ターン進んで notice が既に落ちて
//! いるかのいずれかで、実質もう読まれない。よって **mtime による期限落とし一本**で掃除する。
//! atime は relatime/noatime で信頼できないので使わない。
//!
//! # 絶対の安全則（過去に 1.4TB を失った事故がある）
//!
//! 削除は必ず **2 段階**にする。
//! 1. **列挙と提示（消さない）**: `tmp/` 直下の通常ファイルを 1 つずつ `read_dir` し、
//!    `mtime <= cutoff` のものだけを**実パスの明示リスト**へ積む。件数・合計バイト・パスを
//!    info ログへ出す（= コードでの dry run）。
//! 2. **個別削除**: 積んだ**各実パスに 1 回ずつ `remove_file`**。グロブ展開・`remove_dir_all`・
//!    ワイルドカードは一切使わない（設計で構造的に不可能にする）。
//!
//! ディレクトリ自体・サブディレクトリ（再帰しない）・マーカー/隠しファイル（`.` 始まり）
//! には触れない。DB（`memory_sessions`/`session_logs`）にも触れない。対象は `tmp/` の
//! 通常ファイルのみ。
//!
//! # フォールバック禁止（fail-loudly）
//!
//! `metadata()` / `remove_file()` の失敗を**黙ってスキップして成功扱いにしない**。
//! `CleanupReport.errors` に積んで warn ログへ出す（1 個の失敗で全体は止めない）。
//! 列挙〜削除の間に消えていた（`NotFound`）ケースも握り潰さず `vanished` として数える。
//! ただし `tmp/` ディレクトリ自体が存在しない（退避実績が無いエージェント）のは失敗では
//! なく「掃除対象ゼロ」なので、それは静かにスキップする（失敗の握り潰しではない）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use opencrab_core::workspace::resolve_agent_workspace;
use opencrab_db::Db;

/// 掃除 1 回ぶんの結果（ログ / テスト用）。
#[derive(Debug, Default)]
pub struct CleanupReport {
    /// 実際に削除できたファイル数。
    pub deleted: usize,
    /// 削除で解放したバイト数（列挙時に見えたサイズの合計）。
    pub freed_bytes: u64,
    /// 列挙後・削除前に既に消えていた（`NotFound`）数。握り潰さず可視化するための計上。
    pub vanished: usize,
    /// `metadata()` / `remove_file()` / `read_dir()` などで失敗したもの（`パス: 理由`）。
    /// 失敗しても他ファイルの掃除は続けるが、各失敗は必ずここへ残す。
    pub errors: Vec<String>,
}

impl CleanupReport {
    pub fn did_anything(&self) -> bool {
        self.deleted > 0 || self.vanished > 0 || !self.errors.is_empty()
    }
}

/// 前回 tick を成功させた時刻を残すマーカー名（`marker_dir` 直下）。
///
/// **なぜ永続化するか**: 素朴に `loop { sleep(interval); … }` にするとタイマの起点が
/// プロセス起動時刻になる。このワークスペースは再起動が頻繁で、`interval`（既定 86400 秒）
/// より短い間隔で再起動すると tick が一度も来ず、掃除が黙って達成されない。マーカーに前回
/// 実行時刻を書いておき**経過時間**で発火判定すれば、再起動をまたいでも「前回から interval
/// 経過 / 一度も走っていない」なら発火する（`llm_log_archive` と同型）。
const LAST_RUN_MARKER: &str = ".offload_cleanup_last_run";

/// 起動直後のブートストームを避けるための初回判定までの待ち。判定自体は経過時間ベース
/// なので、この待ちの後すぐ（マーカーが無い / 古ければ）初回 tick が走る。
const STARTUP_DELAY: Duration = Duration::from_secs(180);

/// 「まだ発火時刻でない」ときの再判定間隔の上限。再起動直後にマーカー起点で速やかに
/// 発火できるよう、長くても 1 時間で一度は判定に戻る。
const POLL_CAP: Duration = Duration::from_secs(3600);

fn last_run_path(marker_dir: &Path) -> PathBuf {
    marker_dir.join(LAST_RUN_MARKER)
}

/// マーカーから前回実行時刻を読む。無い / 壊れていれば `None`（= 一度も走っていない扱い）。
fn read_last_run(path: &Path) -> Option<DateTime<Utc>> {
    let s = std::fs::read_to_string(path).ok()?;
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// 前回実行時刻を書き込む（原子性は不要: 次回はここを読むだけ、壊れていれば None 扱い）。
fn write_last_run(path: &Path, when: DateTime<Utc>) -> Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, when.to_rfc3339())
        .with_context(|| format!("write last-run marker: {}", path.display()))?;
    Ok(())
}

/// 発火すべきか。**マーカーが無ければ常に発火**（一度も走っていない = すぐ回す）。
/// あれば「now - 前回 >= interval」で判定する。プロセス起動時刻には依らないので
/// 再起動をまたいでも成り立つ。
fn is_due(last_run: Option<DateTime<Utc>>, now: DateTime<Utc>, interval: chrono::Duration) -> bool {
    match last_run {
        None => true,
        Some(prev) => now.signed_duration_since(prev) >= interval,
    }
}

/// 発火時刻なら 1 回ぶん掃除し、成功したらマーカーを更新する（同期・テスト可能）。
///
/// - まだ発火時刻でなければ `Ok(None)`（マーカーは触らない）。
/// - `cleanup_all` が Err を返したら（DB ロック失敗など）マーカーを更新しない（次回また再試行）。
/// - ファイル単位の失敗は `report.errors` に積むだけで tick 自体は成功とみなし、マーカーを更新する。
fn run_tick_if_due(
    db: &Db,
    workspace_template: &str,
    marker_dir: &Path,
    retention_days: i64,
    interval: chrono::Duration,
    now: DateTime<Utc>,
) -> Result<Option<CleanupReport>> {
    let marker = last_run_path(marker_dir);
    if !is_due(read_last_run(&marker), now, interval) {
        return Ok(None);
    }
    let report = cleanup_all(db, workspace_template, retention_days, now)?;
    write_last_run(&marker, now)?;
    Ok(Some(report))
}

/// 掃除ループを起動する（プロセス生存期間の常駐タスク）。
///
/// 日次程度で十分なので `interval_secs` は最低 3600 秒に丸める。同期 DB とディレクトリ走査を
/// 触るので `spawn_blocking` に載せ、async ランタイムを塞がない。発火判定は永続化した
/// マーカー起点の経過時間で行うので、**再起動を頻繁に挟んでも発火する**（`LAST_RUN_MARKER`）。
pub fn spawn_offload_cleanup_loop(
    db: Db,
    workspace_template: String,
    marker_dir: PathBuf,
    retention_days: i64,
    interval_secs: u64,
) {
    tokio::spawn(async move {
        let interval_secs = interval_secs.max(3600);
        let interval = Duration::from_secs(interval_secs);
        let chrono_interval = chrono::Duration::seconds(interval_secs as i64);
        tracing::info!(
            interval_secs,
            retention_days,
            workspace_template = %workspace_template,
            marker_dir = %marker_dir.display(),
            "offload cleanup loop started"
        );
        // 起動直後の集中 I/O を避けるため少しだけ待ってから初回判定に入る。
        tokio::time::sleep(STARTUP_DELAY).await;
        loop {
            let db = db.clone();
            let template = workspace_template.clone();
            let mdir = marker_dir.clone();
            let res = tokio::task::spawn_blocking(move || {
                run_tick_if_due(
                    &db,
                    &template,
                    &mdir,
                    retention_days,
                    chrono_interval,
                    Utc::now(),
                )
            })
            .await;
            match res {
                Ok(Ok(Some(report))) => {
                    // 動いたことを必ず観測できるよう、対象ゼロでも INFO を出す。
                    tracing::info!(
                        deleted = report.deleted,
                        freed_bytes = report.freed_bytes,
                        vanished = report.vanished,
                        errors = report.errors.len(),
                        "offload cleanup tick ran"
                    );
                    for e in &report.errors {
                        tracing::warn!(detail = %e, "offload cleanup file failed (not deleted)");
                    }
                    tokio::time::sleep(interval).await;
                }
                Ok(Ok(None)) => {
                    // まだ発火時刻でない。上限つきで待って再判定（再起動直後の取りこぼし防止）。
                    tokio::time::sleep(interval.min(POLL_CAP)).await;
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %format!("{e:#}"), "offload cleanup tick failed");
                    tokio::time::sleep(interval).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "offload cleanup task join failed");
                    tokio::time::sleep(interval).await;
                }
            }
        }
    });
}

/// 全エージェントの `tmp/` を 1 回ぶん掃除する（同期・`now` 注入可能）。
///
/// DB ロック失敗（= エージェント一覧が引けない）だけは `Err`（この tick 全体を再試行）。
/// 個々のエージェント / ファイルの失敗は `report.errors` に積んで先へ進む。
pub fn cleanup_all(
    db: &Db,
    workspace_template: &str,
    retention_days: i64,
    now: DateTime<Utc>,
) -> Result<CleanupReport> {
    let agent_ids: Vec<String> = {
        let conn = db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
        // メンテナンスループと同じ列挙口（全エージェントの agent_id）。`find_agents(conn,"")`
        // ではなく purpose-built なこちらを使う（名前は要らず id だけ欲しいため）。
        opencrab_db::queries::list_agent_ids(&conn)?
    };

    let mut report = CleanupReport::default();
    for agent_id in agent_ids {
        // テンプレート展開は既存 resolver に一本化（agent_id の検証込み）。無効な agent_id は
        // 握り潰さず errors に積んで次へ。
        let workspace = match resolve_agent_workspace(workspace_template, &agent_id) {
            Ok(ws) => ws,
            Err(e) => {
                report
                    .errors
                    .push(format!("agent {agent_id}: resolve workspace: {e:#}"));
                continue;
            }
        };
        let tmp = workspace.join("tmp");
        cleanup_tmp_dir(&tmp, retention_days, now, &mut report);
    }
    Ok(report)
}

/// 1 つの `tmp/` ディレクトリを掃除する（2 段階・DB 非依存・掃除ロジックの本体）。
///
/// Stage 1 で `mtime <= cutoff` の**通常ファイル**の実パスだけを列挙し、Stage 2 でその
/// 各パスに `remove_file` を 1 回ずつ掛ける。結果は `report` に加算する。
fn cleanup_tmp_dir(
    dir: &Path,
    retention_days: i64,
    now: DateTime<Utc>,
    report: &mut CleanupReport,
) {
    let cutoff = now - chrono::Duration::days(retention_days);

    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        // tmp/ 自体が無い = このエージェントは退避実績が無い → 掃除対象ゼロ。失敗の握り潰し
        // ではないので静かに戻る（他の失敗と違い、可視化する事象ではない）。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            report
                .errors
                .push(format!("{}: read_dir: {e}", dir.display()));
            return;
        }
    };

    // Stage 1 — 列挙（実パスの明示リスト。ここでは消さない）。
    let mut victims: Vec<(PathBuf, u64)> = Vec::new();
    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                report
                    .errors
                    .push(format!("{}: read entry: {e}", dir.display()));
                continue;
            }
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // マーカー / 隠しファイル（`.` 始まり）は対象外。
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        // `DirEntry::metadata` はシンボリックリンクを**辿らない**（= symlink 自身の情報）。
        // よって symlink は is_file が false になり下で除外される（リンク越しの削除をしない）。
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                report
                    .errors
                    .push(format!("{}: metadata: {e}", path.display()));
                continue;
            }
        };
        // サブディレクトリには再帰せず触れない。通常ファイル以外（dir/symlink/socket 等）も除外。
        if !md.is_file() {
            continue;
        }
        let modified = match md.modified() {
            Ok(t) => t,
            Err(e) => {
                report
                    .errors
                    .push(format!("{}: mtime: {e}", path.display()));
                continue;
            }
        };
        let mtime: DateTime<Utc> = modified.into();
        // 生成から retention_days 経過（mtime <= cutoff）したものだけを候補にする。
        // 生成から retention_days 経過（mtime <= cutoff）したものだけを候補にする。
        if mtime <= cutoff {
            victims.push((path, md.len()));
        }
    }

    if victims.is_empty() {
        return;
    }

    // dry run = 展開結果（実パス）の提示。多いときはサンプルだけ出す（全件はパス列で膨らむため）。
    let total_bytes: u64 = victims.iter().map(|(_, sz)| *sz).sum();
    const SAMPLE: usize = 10;
    let sample: Vec<String> = victims
        .iter()
        .take(SAMPLE)
        .map(|(p, _)| p.display().to_string())
        .collect();
    tracing::info!(
        dir = %dir.display(),
        count = victims.len(),
        total_bytes,
        sample = ?sample,
        "offload cleanup: deleting expired files"
    );

    // Stage 2 — 個別削除（各実パスに 1 回ずつ remove_file。グロブ・再帰・ワイルドカードは無し）。
    for (path, size) in victims {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                report.deleted += 1;
                report.freed_bytes += size;
            }
            // 列挙〜削除の間に消えていた。ゴール（消えていること）は達成だが握り潰さず計上する。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                report.vanished += 1;
            }
            Err(e) => {
                report
                    .errors
                    .push(format!("{}: remove_file: {e}", path.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ファイルを作り、mtime を明示的に `mtime` へ設定する（`now` 注入とセットで決定的な
    /// 期限テストにする）。`File::set_modified` は Rust 1.75+ で安定。
    fn write_file_with_mtime(path: &Path, contents: &[u8], mtime: DateTime<Utc>) {
        std::fs::write(path, contents).unwrap();
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(std::time::SystemTime::from(mtime)).unwrap();
        f.sync_all().unwrap();
    }

    fn now() -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).unwrap()
    }

    fn days_ago(d: i64) -> DateTime<Utc> {
        now() - chrono::Duration::days(d)
    }

    // 最重要 1: 保持期間より古いものは消し、期間内のものは残す（age 述語の固定）。
    #[test]
    fn deletes_expired_keeps_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("s1-old.txt");
        let recent = tmp.path().join("s2-recent.txt");
        write_file_with_mtime(&old, b"0123456789", days_ago(10)); // 10 日前 → 消す
        write_file_with_mtime(&recent, b"keep me", days_ago(1)); // 1 日前 → 残す

        let mut report = CleanupReport::default();
        cleanup_tmp_dir(tmp.path(), 7, now(), &mut report);

        assert_eq!(report.deleted, 1);
        assert_eq!(report.freed_bytes, 10);
        assert!(report.errors.is_empty());
        assert!(!old.exists(), "10 日前のファイルは消えるべき");
        assert!(recent.exists(), "1 日前のファイルは残すべき");
    }

    // 最重要 2: 保持期間内のファイルは 1 つも消さない（変異確認の主対象）。
    #[test]
    fn keeps_all_files_within_retention() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("s1-a.txt");
        let b = tmp.path().join("s2-b.json");
        write_file_with_mtime(&a, b"aaa", days_ago(3));
        write_file_with_mtime(&b, b"bbb", days_ago(6)); // ぎりぎり保持内（<7日）

        let mut report = CleanupReport::default();
        cleanup_tmp_dir(tmp.path(), 7, now(), &mut report);

        assert_eq!(report.deleted, 0, "保持期間内は 1 つも消さない");
        assert_eq!(report.freed_bytes, 0);
        assert!(a.exists() && b.exists());
    }

    // 境界: ちょうど retention_days 前（mtime == cutoff）は消す側に含める（`<= cutoff`）。
    #[test]
    fn boundary_exactly_retention_is_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("s1-boundary.txt");
        write_file_with_mtime(&f, b"x", days_ago(7));

        let mut report = CleanupReport::default();
        cleanup_tmp_dir(tmp.path(), 7, now(), &mut report);
        assert_eq!(report.deleted, 1);
        assert!(!f.exists());
    }

    // マーカー・隠しファイル・サブディレクトリには触れない（古くても保護される）。
    #[test]
    fn skips_marker_hidden_and_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join(LAST_RUN_MARKER);
        let hidden = tmp.path().join(".secret");
        let subdir = tmp.path().join("nested");
        let regular = tmp.path().join("s1-old.txt");
        write_file_with_mtime(&marker, b"2020-01-01T00:00:00+00:00", days_ago(100));
        write_file_with_mtime(&hidden, b"h", days_ago(100));
        std::fs::create_dir(&subdir).unwrap();
        // サブディレクトリ内に古いファイルがあっても再帰しないので消えない。
        write_file_with_mtime(&subdir.join("inner.txt"), b"i", days_ago(100));
        write_file_with_mtime(&regular, b"r", days_ago(100));

        let mut report = CleanupReport::default();
        cleanup_tmp_dir(tmp.path(), 7, now(), &mut report);

        assert_eq!(report.deleted, 1, "通常ファイルだけ消える");
        assert!(marker.exists(), "マーカーは保護");
        assert!(hidden.exists(), "隠しファイルは保護");
        assert!(subdir.exists(), "サブディレクトリは保護");
        assert!(subdir.join("inner.txt").exists(), "再帰しない");
        assert!(!regular.exists());
    }

    // tmp/ が存在しない（退避実績なし）のは失敗ではない = errors に積まない。
    #[test]
    fn missing_tmp_dir_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let mut report = CleanupReport::default();
        cleanup_tmp_dir(&missing, 7, now(), &mut report);
        assert_eq!(report.deleted, 0);
        assert!(report.errors.is_empty(), "存在しない tmp はエラーにしない");
        assert!(!report.did_anything());
    }

    // fail-loudly: remove_file が失敗したら握り潰さず errors に積む（成功扱いにしない）。
    // 親ディレクトリを読み取り専用にして remove_file を EACCES にする（unix）。
    #[cfg(unix)]
    #[test]
    fn remove_failure_is_collected_not_swallowed() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("s1-old.txt");
        write_file_with_mtime(&f, b"x", days_ago(30));

        // 親ディレクトリを r-x のみに（削除には親への書き込みが要る → 失敗する）。
        let orig = std::fs::metadata(tmp.path()).unwrap().permissions();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

        let mut report = CleanupReport::default();
        cleanup_tmp_dir(tmp.path(), 7, now(), &mut report);

        // 後始末（tempdir が消せるよう権限を戻す）。
        std::fs::set_permissions(tmp.path(), orig).unwrap();

        assert_eq!(report.deleted, 0, "消せていない");
        assert_eq!(report.errors.len(), 1, "失敗は必ず記録される");
        assert!(report.errors[0].contains("remove_file"));
        assert!(f.exists(), "ファイルは残っている");
    }

    // is_due: マーカー起点の経過時間で判定する（プロセス起動時刻に依らない）。
    #[test]
    fn is_due_fires_on_first_run_and_after_interval() {
        let day = chrono::Duration::seconds(86400);
        assert!(is_due(None, now(), day), "マーカー無しは即発火");
        assert!(
            !is_due(Some(now() - chrono::Duration::hours(1)), now(), day),
            "直近に走っていれば発火しない"
        );
        assert!(
            is_due(Some(now() - chrono::Duration::hours(25)), now(), day),
            "interval 超で発火"
        );
    }

    #[test]
    fn last_run_marker_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = last_run_path(dir.path());
        assert!(read_last_run(&path).is_none());
        write_last_run(&path, now()).unwrap();
        assert_eq!(read_last_run(&path), Some(now()));
    }

    // DB からエージェントを列挙し、各 tmp を掃除する end-to-end（enumeration 経路）。
    #[test]
    fn cleanup_all_enumerates_agents_and_cleans_each_tmp() {
        // in-memory DB に最小のエージェントを 2 体入れる（NOT NULL: name, persona_name）。
        let conn = opencrab_db::init_memory().unwrap();
        for id in ["agent-a", "agent-b"] {
            conn.execute(
                "INSERT INTO agents (agent_id, name, persona_name) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, id, id],
            )
            .unwrap();
        }
        let db = Db::from_connection(conn);

        // workspace テンプレートを一時ディレクトリ配下に向ける。
        let base = tempfile::tempdir().unwrap();
        let template = format!("{}/{{agent_id}}/workspace", base.path().to_string_lossy());

        // 各エージェントの tmp に古い/新しいファイルを 1 つずつ。
        for id in ["agent-a", "agent-b"] {
            let tmp = base.path().join(id).join("workspace").join("tmp");
            std::fs::create_dir_all(&tmp).unwrap();
            write_file_with_mtime(&tmp.join("old.txt"), b"12345", days_ago(30));
            write_file_with_mtime(&tmp.join("new.txt"), b"keep", days_ago(1));
        }

        let report = cleanup_all(&db, &template, 7, now()).unwrap();

        assert_eq!(report.deleted, 2, "2 エージェント × 古い 1 つ");
        assert_eq!(report.freed_bytes, 10);
        assert!(report.errors.is_empty());
        for id in ["agent-a", "agent-b"] {
            let tmp = base.path().join(id).join("workspace").join("tmp");
            assert!(!tmp.join("old.txt").exists());
            assert!(tmp.join("new.txt").exists());
        }
    }

    // run_tick_if_due: 初回は即発火してマーカーを書く。直後の再起動では発火しない。
    #[test]
    fn tick_fires_first_then_respects_marker() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = Db::from_connection(conn); // エージェント 0 でも tick は成立する
        let marker_dir = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let template = format!("{}/{{agent_id}}/workspace", base.path().to_string_lossy());
        let day = chrono::Duration::seconds(86400);

        let r1 = run_tick_if_due(&db, &template, marker_dir.path(), 7, day, now()).unwrap();
        assert!(r1.is_some(), "初回はマーカー無しで即発火");
        assert!(read_last_run(&last_run_path(marker_dir.path())).is_some());

        let r2 = run_tick_if_due(
            &db,
            &template,
            marker_dir.path(),
            7,
            day,
            now() + chrono::Duration::minutes(1),
        )
        .unwrap();
        assert!(r2.is_none(), "直近に走ったばかりなら発火しない");

        let r3 = run_tick_if_due(
            &db,
            &template,
            marker_dir.path(),
            7,
            day,
            now() + chrono::Duration::hours(25),
        )
        .unwrap();
        assert!(r3.is_some(), "interval 経過後は再起動をまたいで発火");
    }
}
