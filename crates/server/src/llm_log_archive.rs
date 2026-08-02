//! 古い `llm_logs` を zip へ書き出して DB から外す（#337）。
//!
//! `llm_logs` は 1 行に「プロンプト全文 + 応答」を丸ごと保存するため肥大しやすく
//! （実測で DB の 97% / 3.3GB）、再起動前の DB バックアップを重くしている。デバッグ用に
//! 直近は残しつつ、保持期間より古い**月**を丸ごと JSONL に書き出して zip 圧縮し、
//! DB からその行を外す。
//!
//! # 絶対の安全則（オーナー方針「勝手に LLM ログ消そうとしないで？」）
//!
//! **書き出し → 検証 → 削除** の順を崩さない。各月について
//! 1. 対象行を DB から読み出す（`list_llm_logs_for_month`）。
//! 2. JSONL を zip に書く（`.tmp` に書いてから最終名へ rename = 原子的差し替え）。
//! 3. 書いた zip を**読み直して**行数と id 集合が一致することを検証する。
//! 4. 検証を通ったときだけ、**読み出した id だけ**を単一トランザクションで削除する。
//!
//! 2 か 3 で失敗したら `?` で早期 return し、その月は**消さない**。月の述語ではなく
//! id 指定で消すので、アーカイブ後に挿入された同月の新しい行を巻き込まない。冪等：
//! 2 回流しても、削除済みの月は行が無いので何もしない。書いた後・削除前に落ちても、
//! 次回は行がまだ DB にあるので zip を上書きし直して削除する（内容は決定的）。
//!
//! # 対象月の選び方
//!
//! **月末がカットオフ（now - retention_days）より前**の月だけを対象にする。すなわち
//! カットオフをまたぐ「境界の月」は丸ごと残す。行単位で 30 日ちょうどに切らないのは、
//! 部分月ファイルの追記や再書き出しを避けて冪等性を単純に保つため。実際の保持は
//! 最大 1 か月ぶん長くなりうるが、目的（直近をデバッグ用に残す）には十分。
//!
//! `memory_sessions`（会話ログ = 記憶の本体）には手を出さない。対象は `llm_logs` のみ。
//!
//! # VACUUM はしない
//!
//! 行を消しても SQLite のファイルは縮まない（空き領域として再利用される）。3.4GB の
//! `VACUUM` は長時間 DB をロックするため稼働中に走らせるべきでなく、この自動ループでは
//! 一切実行しない。空き回収は運用者が手動コマンド（`opencrab-vacuum` バイナリ）で
//! サーバ停止中に行う。

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};

use opencrab_db::queries::LlmLogRow;
use opencrab_db::Db;

/// 1 か月ぶんのアーカイブ結果。
#[derive(Debug, Clone)]
pub struct MonthArchived {
    pub month: String,
    pub archived_rows: usize,
    pub deleted_rows: usize,
    pub path: PathBuf,
}

/// アーカイブ 1 回ぶんの結果（ログ / テスト用）。
#[derive(Debug, Default)]
pub struct ArchiveReport {
    /// 書き出して削除まで完了した月。
    pub months: Vec<MonthArchived>,
    /// 月単位で失敗したもの（`月: 理由`）。失敗した月は削除していない。
    pub errors: Vec<String>,
}

impl ArchiveReport {
    pub fn did_anything(&self) -> bool {
        !self.months.is_empty() || !self.errors.is_empty()
    }
    pub fn total_archived(&self) -> usize {
        self.months.iter().map(|m| m.archived_rows).sum()
    }
    pub fn total_deleted(&self) -> usize {
        self.months.iter().map(|m| m.deleted_rows).sum()
    }
}

/// 前回 tick を成功させた時刻を残すマーカー名（archive_dir 直下）。
///
/// **なぜ永続化するか**: 素朴に `loop { sleep(interval); … }` にすると、タイマは
/// プロセス起動時刻を起点にする。このワークスペースは再起動が頻繁で、`interval`
/// （既定 86400 秒）より短い間隔で再起動すると tick が一度も来ず、DB 縮小が黙って
/// 達成されない。マーカーに前回実行時刻を書いておき、**経過時間**で発火判定すれば
/// 再起動をまたいでも「前回から interval 経った / 一度も走っていない」なら発火する。
const LAST_RUN_MARKER: &str = ".llm_log_archive_last_run";

/// 起動直後のブートストームを避けるための初回判定までの待ち。判定自体は経過時間ベース
/// なので、この待ちの後すぐ（マーカーが無い / 古ければ）初回 tick が走る。
const STARTUP_DELAY: Duration = Duration::from_secs(180);

/// 「まだ発火時刻でない」ときの再判定間隔の上限。再起動直後にマーカー起点で速やかに
/// 発火できるよう、長くても 1 時間で一度は判定に戻る。
const POLL_CAP: Duration = Duration::from_secs(3600);

fn last_run_path(archive_dir: &Path) -> PathBuf {
    archive_dir.join(LAST_RUN_MARKER)
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

/// 発火時刻なら 1 回ぶんアーカイブし、成功したらマーカーを更新する（同期・テスト可能）。
///
/// - まだ発火時刻でなければ `Ok(None)`（マーカーは触らない）。
/// - `archive_llm_logs` が Err を返したらマーカーを更新しない（次回また発火して再試行）。
/// - 月単位の失敗は `report.errors` に積むだけで tick 自体は成功とみなし、マーカーを更新する
///   （失敗した月は次の interval でまた対象になる）。
fn run_tick_if_due(
    db: &Db,
    archive_dir: &Path,
    retention_days: i64,
    interval: chrono::Duration,
    now: DateTime<Utc>,
) -> Result<Option<ArchiveReport>> {
    let marker = last_run_path(archive_dir);
    if !is_due(read_last_run(&marker), now, interval) {
        return Ok(None);
    }
    let report = archive_llm_logs(db, archive_dir, retention_days, now)?;
    write_last_run(&marker, now)?;
    Ok(Some(report))
}

/// アーカイブループを起動する（プロセス生存期間の常駐タスク）。
///
/// 日次程度で十分なので `interval_secs` は最低 3600 秒に丸める。重い I/O と同期 DB を
/// 触るので `spawn_blocking` に載せ、async ランタイムを塞がない。発火判定は永続化した
/// マーカー起点の経過時間で行うので、**再起動を頻繁に挟んでも発火する**（`LAST_RUN_MARKER`）。
pub fn spawn_llm_log_archive_loop(
    db: Db,
    archive_dir: PathBuf,
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
            archive_dir = %archive_dir.display(),
            "llm_log archive loop started"
        );
        // 起動直後の集中 I/O を避けるため少しだけ待ってから初回判定に入る。
        tokio::time::sleep(STARTUP_DELAY).await;
        loop {
            let db = db.clone();
            let dir = archive_dir.clone();
            let res = tokio::task::spawn_blocking(move || {
                run_tick_if_due(&db, &dir, retention_days, chrono_interval, Utc::now())
            })
            .await;
            match res {
                Ok(Ok(Some(report))) => {
                    // 動いたことを必ず観測できるよう、対象ゼロでも INFO を出す。
                    tracing::info!(
                        months = report.months.len(),
                        archived_rows = report.total_archived(),
                        deleted_rows = report.total_deleted(),
                        errors = report.errors.len(),
                        "llm_log archive tick ran"
                    );
                    for e in &report.errors {
                        tracing::warn!(detail = %e, "llm_log archive month failed (not deleted)");
                    }
                    tokio::time::sleep(interval).await;
                }
                Ok(Ok(None)) => {
                    // まだ発火時刻でない。上限つきで待って再判定（再起動直後の取りこぼし防止）。
                    tokio::time::sleep(interval.min(POLL_CAP)).await;
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %format!("{e:#}"), "llm_log archive tick failed");
                    tokio::time::sleep(interval).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "llm_log archive task join failed");
                    tokio::time::sleep(interval).await;
                }
            }
        }
    });
}

/// 保持期間より古い月を書き出して DB から外す（同期）。
///
/// `now` は注入可能（テスト用）。ディレクトリ作成に失敗した場合は**何も削除せず**
/// Err を返す。個々の月の失敗は `report.errors` に積んで次の月へ進む（その月は消さない）。
pub fn archive_llm_logs(
    db: &Db,
    archive_dir: &Path,
    retention_days: i64,
    now: DateTime<Utc>,
) -> Result<ArchiveReport> {
    // 出力先を用意（失敗したら 1 行も消さずに Err）。
    std::fs::create_dir_all(archive_dir)
        .with_context(|| format!("failed to create archive dir: {}", archive_dir.display()))?;

    let months = {
        let conn = db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
        opencrab_db::queries::list_llm_log_months(&conn)?
    };

    let mut report = ArchiveReport::default();
    for (month, _count) in months {
        if !month_is_eligible(&month, retention_days, now) {
            continue;
        }
        match archive_one_month(db, archive_dir, &month) {
            Ok(Some(outcome)) => report.months.push(outcome),
            Ok(None) => {} // 対象行が無かった（他ループとの競合等）
            Err(e) => report.errors.push(format!("{month}: {e:#}")),
        }
    }
    Ok(report)
}

/// 月 `YYYY-MM` が「月末 < カットオフ」を満たすか。
///
/// 月内の全行は「翌月 1 日 00:00 UTC」より前。これがカットオフ（now - retention_days）
/// 以下なら、その月の全行はカットオフより古い = 対象。パースできない月名は対象外。
fn month_is_eligible(month: &str, retention_days: i64, now: DateTime<Utc>) -> bool {
    let Some(first_of_next) = first_of_next_month(month) else {
        return false;
    };
    let cutoff = now - chrono::Duration::days(retention_days);
    first_of_next <= cutoff
}

/// `YYYY-MM` → 翌月 1 日 00:00:00 UTC。
fn first_of_next_month(month: &str) -> Option<DateTime<Utc>> {
    let (y, m) = month.split_once('-')?;
    let year: i32 = y.parse().ok()?;
    let mon: u32 = m.parse().ok()?;
    if !(1..=12).contains(&mon) {
        return None;
    }
    let (ny, nm) = if mon == 12 {
        (year + 1, 1)
    } else {
        (year, mon + 1)
    };
    let date = NaiveDate::from_ymd_opt(ny, nm, 1)?;
    let naive = date.and_hms_opt(0, 0, 0)?;
    Some(Utc.from_utc_datetime(&naive))
}

/// 1 か月を書き出して検証し、成功したら削除する。対象行ゼロなら `Ok(None)`。
///
/// 書き出し（`write_jsonl_zip`）か検証（`verify_zip`）が失敗すると `?` で早期 return し、
/// **削除に到達しない**。これが「書き出せていないログを消さない」の実装本体。
fn archive_one_month(db: &Db, dir: &Path, month: &str) -> Result<Option<MonthArchived>> {
    archive_one_month_with(db, dir, month, write_jsonl_zip)
}

/// `archive_one_month` の本体。書き出し関数を注入できるようにして、テストで
/// **検証を失敗させたときにフル経路で削除へ到達しない**ことを固定できるようにする。
fn archive_one_month_with(
    db: &Db,
    dir: &Path,
    month: &str,
    write_fn: impl Fn(&Path, &str, &[LlmLogRow]) -> Result<()>,
) -> Result<Option<MonthArchived>> {
    // ① 対象行を確定（DB ロックは読み出しの間だけ保持）。
    let rows = {
        let conn = db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
        opencrab_db::queries::list_llm_logs_for_month(&conn, month)?
    };
    if rows.is_empty() {
        return Ok(None);
    }

    let inner_name = format!("llm_logs-{month}.jsonl");
    let canonical = dir.join(format!("llm_logs-{month}.jsonl.zip"));
    // 既に同月の zip があるなら**上書きしない**（#341 提案2）。通常は「空になった古い月へ
    // 遅延 insert / クロック巻き戻しで 1 行だけ入る」希少ケースで、素朴に上書きすると
    // その 1 行だけの zip が過去にアーカイブ済みの行を持つ旧 zip を潰し、「消えて zip も無い」
    // 唯一の残存経路になる。別名で書くことで旧 zip を保全する（クラッシュ復旧の再書き出しも
    // 別名になるだけで、内容は決定的・削除は id 指定なので安全）。
    let final_path = if canonical.exists() {
        let stamp = Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
        dir.join(format!("llm_logs-{month}-{stamp}.jsonl.zip"))
    } else {
        canonical
    };
    let final_name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("llm_logs-{month}.jsonl.zip"));
    let tmp_path = dir.join(format!("{final_name}.tmp"));

    // ② tmp に書き出す（I/O 失敗はここで Err → 削除しない）。書き出しの中で zip 本体を
    //    fsync するので、page cache に残ったまま消える窓を塞ぐ。
    write_fn(&tmp_path, &inner_name, &rows)
        .with_context(|| format!("failed to write archive for {month}"))?;

    // ③ 書いた tmp を読み直して検証（不一致は Err → 削除しない）。
    verify_zip(&tmp_path, &inner_name, &rows)
        .with_context(|| format!("verification failed for {month}"))?;

    // rename は原子的（同一ディレクトリ）。tmp と最終の内容は同一なので tmp の検証で足りる。
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("failed to finalize archive for {month}"))?;

    // rename を永続化するため親ディレクトリも fsync する（電源断/カーネルパニックで
    // 「DB の削除は永続・zip は page cache のまま消失」の窓を塞ぐ）。ディレクトリ fsync は
    // 一部の FS/プラットフォームで非対応なので**ベストエフォート**（失敗しても削除は止めない:
    // zip 本体は既に fsync 済みで、最悪でも rename の耐久性が従来どおりに戻るだけ）。
    if let Err(e) = fsync_dir(dir) {
        tracing::warn!(
            error = %format!("{e:#}"),
            dir = %dir.display(),
            "failed to fsync archive dir (continuing; rename durability unchanged)"
        );
    }

    // ④ 検証済みの id だけを単一トランザクションで削除。
    let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    let deleted = {
        let mut conn = db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
        opencrab_db::queries::delete_llm_logs_by_ids(&mut conn, &ids)?
    };

    Ok(Some(MonthArchived {
        month: month.to_string(),
        archived_rows: rows.len(),
        deleted_rows: deleted,
        path: final_path,
    }))
}

/// 行を JSONL（1 行 = 1 行の完全な JSON）にして zip（deflate）へ書き出す。
///
/// `LlmLogRow` の `Serialize` をそのまま使うので、各行は**そのまま復元できる**。
fn write_jsonl_zip(path: &Path, inner_name: &str, rows: &[LlmLogRow]) -> Result<()> {
    let file = std::fs::File::create(path)
        .with_context(|| format!("create zip file: {}", path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file(inner_name, opts)?;
    for row in rows {
        let line = serde_json::to_string(row)?;
        zip.write_all(line.as_bytes())?;
        zip.write_all(b"\n")?;
    }
    // `finish` は内側の `File` を返す。削除する前に zip 本体を fsync して、page cache に
    // 残ったまま電源断で消える窓を塞ぐ（親ディレクトリの fsync は rename 後に別途行う）。
    let file = zip.finish()?;
    file.sync_all()
        .with_context(|| format!("fsync zip file: {}", path.display()))?;
    Ok(())
}

/// ディレクトリを fsync する（`rename` の耐久性確保）。unix では dir fd を fsync する。
/// 呼び出し側はこれをベストエフォート扱いする（失敗しても削除は止めない）。
fn fsync_dir(dir: &Path) -> Result<()> {
    let f = std::fs::File::open(dir)
        .with_context(|| format!("open dir for fsync: {}", dir.display()))?;
    f.sync_all()
        .with_context(|| format!("fsync dir: {}", dir.display()))?;
    Ok(())
}

/// 書いた zip を読み直し、行数と id 集合が DB 由来の `rows` と一致することを検証する。
///
/// 各行が `LlmLogRow` として parse できることも同時に確かめる（壊れた行を検出）。
fn verify_zip(path: &Path, inner_name: &str, rows: &[LlmLogRow]) -> Result<()> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("open zip for verify: {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut entry = archive
        .by_name(inner_name)
        .with_context(|| format!("archive missing entry {inner_name}"))?;
    let mut content = String::new();
    entry.read_to_string(&mut content)?;

    let parsed: Vec<LlmLogRow> = content
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()
        .context("archive line is not a valid LlmLogRow")?;

    if parsed.len() != rows.len() {
        bail!(
            "row count mismatch: zip has {}, db had {}",
            parsed.len(),
            rows.len()
        );
    }
    let db_ids: HashSet<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    let zip_ids: HashSet<&str> = parsed.iter().map(|r| r.id.as_str()).collect();
    if db_ids != zip_ids {
        bail!("id set mismatch between zip and db");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_row(id: &str, ts: &str) -> LlmLogRow {
        LlmLogRow {
            id: id.to_string(),
            agent_id: "a1".to_string(),
            session_id: Some("s1".to_string()),
            model: Some("m".to_string()),
            prompt: format!("prompt-{id}"),
            response: format!("response-{id}"),
            tool_calls: None,
            latency_ms: Some(10),
            prompt_tokens: Some(1),
            completion_tokens: Some(2),
            total_tokens: Some(3),
            error_code: None,
            error_body: None,
            requested_at: Some(ts.to_string()),
            trigger_message_id: None,
            is_bot_iteration: false,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            created_at: ts.to_string(),
        }
    }

    fn mk_db(rows: &[LlmLogRow]) -> Db {
        let conn = opencrab_db::init_memory().unwrap();
        for r in rows {
            opencrab_db::queries::insert_llm_log(&conn, r).unwrap();
        }
        Db::from_connection(conn)
    }

    fn count_rows(db: &Db) -> i64 {
        let conn = db.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM llm_logs", [], |r| r.get(0))
            .unwrap()
    }

    // now = 2026-08-02。retention 30 日 → カットオフ 2026-07-03。
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 2, 12, 0, 0).unwrap()
    }

    #[test]
    fn eligibility_keeps_boundary_and_recent_months() {
        // 2026-06 は月末 6/30 < 7/03 → 対象。2026-07 は月末 7/31 > 7/03 → 残す。
        assert!(month_is_eligible("2026-05", 30, now()));
        assert!(month_is_eligible("2026-06", 30, now()));
        assert!(!month_is_eligible("2026-07", 30, now()));
        assert!(!month_is_eligible("2026-08", 30, now()));
        assert!(!month_is_eligible("garbage", 30, now()));
    }

    #[test]
    fn old_month_is_written_and_deleted_recent_kept() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![
            mk_row("old-a", "2026-05-10T10:00:00+00:00"),
            mk_row("old-b", "2026-05-20T10:00:00+00:00"),
            mk_row("recent", "2026-07-20T10:00:00+00:00"),
        ];
        let db = mk_db(&rows);

        let report = archive_llm_logs(&db, dir.path(), 30, now()).unwrap();

        // 古い 5 月だけ書き出して削除、直近 7 月は残る。
        assert_eq!(report.months.len(), 1);
        assert_eq!(report.months[0].month, "2026-05");
        assert_eq!(report.months[0].archived_rows, 2);
        assert_eq!(report.months[0].deleted_rows, 2);
        assert!(report.errors.is_empty());
        assert_eq!(count_rows(&db), 1); // recent が残る

        let zip = dir.path().join("llm_logs-2026-05.jsonl.zip");
        assert!(zip.exists());
        assert!(!dir.path().join("llm_logs-2026-05.jsonl.zip.tmp").exists());
    }

    #[test]
    fn rows_are_restorable_from_zip() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![
            mk_row("r1", "2026-05-10T10:00:00+00:00"),
            mk_row("r2", "2026-05-11T09:30:00+00:00"),
        ];
        let db = mk_db(&rows);
        archive_llm_logs(&db, dir.path(), 30, now()).unwrap();

        // zip → JSONL → LlmLogRow に完全復元できる。
        let file = std::fs::File::open(dir.path().join("llm_logs-2026-05.jsonl.zip")).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut entry = archive.by_name("llm_logs-2026-05.jsonl").unwrap();
        let mut content = String::new();
        entry.read_to_string(&mut content).unwrap();
        let restored: Vec<LlmLogRow> = content
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].id, "r1");
        assert_eq!(restored[0].prompt, "prompt-r1");
        assert_eq!(restored[1].id, "r2");
        assert_eq!(restored[1].response, "response-r2");
    }

    #[test]
    fn idempotent_second_run_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![mk_row("old-a", "2026-05-10T10:00:00+00:00")];
        let db = mk_db(&rows);

        let r1 = archive_llm_logs(&db, dir.path(), 30, now()).unwrap();
        assert_eq!(r1.total_deleted(), 1);
        assert_eq!(count_rows(&db), 0);

        // 2 回目: 対象行がもう無いので何もしない（二重削除しない・壊れない）。
        let r2 = archive_llm_logs(&db, dir.path(), 30, now()).unwrap();
        assert!(r2.months.is_empty());
        assert!(r2.errors.is_empty());
        assert_eq!(count_rows(&db), 0);
    }

    // 最重要: 書き出しに失敗したら 1 行も消さない。
    //
    // tmp zip の書き出し先に**ディレクトリ**を先置きして `File::create` を失敗させる。
    // 実際の書き出し経路を通して失敗を起こし、DB 行が残ることを固定する。
    #[test]
    fn write_failure_does_not_delete_any_row() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![
            mk_row("old-a", "2026-05-10T10:00:00+00:00"),
            mk_row("old-b", "2026-05-20T10:00:00+00:00"),
        ];
        let db = mk_db(&rows);

        // 2026-05 の tmp 出力名と同名のディレクトリを作って File::create を失敗させる。
        std::fs::create_dir(dir.path().join("llm_logs-2026-05.jsonl.zip.tmp")).unwrap();

        let report = archive_llm_logs(&db, dir.path(), 30, now()).unwrap();

        // 月は失敗として記録され、行は 1 つも消えていない。
        assert!(report.months.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].starts_with("2026-05:"));
        assert_eq!(count_rows(&db), 2);
    }

    // 検証（読み直し）で不一致なら消さない、を verify 単体で固定する。
    #[test]
    fn verify_rejects_row_count_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.jsonl.zip");
        let written = vec![mk_row("r1", "2026-05-10T10:00:00+00:00")];
        write_jsonl_zip(&path, "x.jsonl", &written).unwrap();

        // DB 側が 2 行あった想定 → zip は 1 行なので不一致で Err。
        let expected = vec![
            mk_row("r1", "2026-05-10T10:00:00+00:00"),
            mk_row("r2", "2026-05-11T10:00:00+00:00"),
        ];
        assert!(verify_zip(&path, "x.jsonl", &expected).is_err());
    }

    // 最重要（#341-3）: 検証を強制失敗させたとき、フル経路（archive_one_month）で
    // **削除に到達しない**ことを固定する。書き出し関数を差し替え、DB は 2 行なのに
    // zip には 1 行しか書かせず、verify を不一致で落とす。
    #[test]
    fn verify_failure_deletes_nothing_full_path() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![
            mk_row("old-a", "2026-05-10T10:00:00+00:00"),
            mk_row("old-b", "2026-05-20T10:00:00+00:00"),
        ];
        let db = mk_db(&rows);

        // 検証を必ず失敗させる書き出し（渡された行のうち先頭 1 行だけ書く）。
        let bad_writer =
            |path: &Path, inner: &str, rows: &[LlmLogRow]| write_jsonl_zip(path, inner, &rows[..1]);
        let res = archive_one_month_with(&db, dir.path(), "2026-05", bad_writer);

        assert!(res.is_err(), "verify 不一致で Err になるはず");
        // フル経路で削除に到達していない（2 行のまま）。
        assert_eq!(count_rows(&db), 2);
        // verify で弾かれるので rename されず、最終 zip は存在しない。
        assert!(!dir.path().join("llm_logs-2026-05.jsonl.zip").exists());
    }

    // #341-1: 再起動をまたいでも「プロセス開始時刻」ではなくマーカー起点で発火する。
    #[test]
    fn is_due_fires_on_first_run_and_after_interval() {
        let day = chrono::Duration::seconds(86400);
        // マーカー無し（= 一度も走っていない）は即発火。
        assert!(is_due(None, now(), day));
        // 直近に走っていれば発火しない。
        assert!(!is_due(
            Some(now() - chrono::Duration::hours(1)),
            now(),
            day
        ));
        // interval を過ぎていれば発火。
        assert!(is_due(
            Some(now() - chrono::Duration::hours(25)),
            now(),
            day
        ));
    }

    #[test]
    fn restart_still_fires_via_persisted_marker() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![mk_row("old-a", "2026-05-10T10:00:00+00:00")];
        let db = mk_db(&rows);
        let day = chrono::Duration::seconds(86400);

        // 1) 初回起動（マーカー無し）: interval を待たずに即発火して書き出し・削除。
        let r1 = run_tick_if_due(&db, dir.path(), 30, day, now()).unwrap();
        assert!(r1.is_some(), "初回はマーカーが無いので即発火するはず");
        assert_eq!(count_rows(&db), 0);
        assert!(read_last_run(&last_run_path(dir.path())).is_some());

        // 2) すぐ再起動（1 分後）: 直近に走ったので発火しない（= 再起動連打でも二重実行しない）。
        let r2 = run_tick_if_due(
            &db,
            dir.path(),
            30,
            day,
            now() + chrono::Duration::minutes(1),
        )
        .unwrap();
        assert!(r2.is_none(), "直近に走ったばかりなら発火しない");

        // 3) interval 経過後の再起動: マーカー起点で判定して再び発火する。
        let r3 = run_tick_if_due(
            &db,
            dir.path(),
            30,
            day,
            now() + chrono::Duration::hours(25),
        )
        .unwrap();
        assert!(
            r3.is_some(),
            "前回から interval 経てば再起動をまたいでも発火する"
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

    // #341 提案2: 空になった古い月へ遅延 insert された 1 行が、過去にアーカイブ済みの
    // 旧 zip を上書きして「消えて zip も無い」状態を作らないことを固定する。
    #[test]
    fn stray_late_insert_does_not_overwrite_existing_zip() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![
            mk_row("a", "2026-05-10T10:00:00+00:00"),
            mk_row("b", "2026-05-20T10:00:00+00:00"),
        ];
        let db = mk_db(&rows);

        // 1 回目: 2026-05 を書き出して削除。canonical zip ができる。
        let r1 = archive_llm_logs(&db, dir.path(), 30, now()).unwrap();
        assert_eq!(r1.total_deleted(), 2);
        let canonical = dir.path().join("llm_logs-2026-05.jsonl.zip");
        assert!(canonical.exists());

        // 空になった 2026-05 へ遅延 insert（バックフィル / クロック巻き戻し相当）で 1 行。
        {
            let conn = db.lock().unwrap();
            opencrab_db::queries::insert_llm_log(
                &conn,
                &mk_row("stray", "2026-05-15T10:00:00+00:00"),
            )
            .unwrap();
        }

        // 2 回目: stray を書き出して削除するが、canonical zip は上書きしない。
        let r2 = archive_llm_logs(&db, dir.path(), 30, now()).unwrap();
        assert_eq!(r2.total_deleted(), 1);
        assert_eq!(count_rows(&db), 0);

        // canonical zip は元の 2 行（a, b）のまま = 上書きされていない。
        let file = std::fs::File::open(&canonical).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut entry = archive.by_name("llm_logs-2026-05.jsonl").unwrap();
        let mut content = String::new();
        entry.read_to_string(&mut content).unwrap();
        let restored: Vec<LlmLogRow> = content
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(restored.len(), 2);
        let ids: HashSet<&str> = restored.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains("a") && ids.contains("b"));

        // stray は別名の zip に保存されている（旧 zip とは別ファイル）。
        let extra: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("llm_logs-2026-05-") && n.ends_with(".jsonl.zip"))
            .collect();
        assert_eq!(extra.len(), 1, "stray 用の別名 zip が 1 つできる");
    }
}
