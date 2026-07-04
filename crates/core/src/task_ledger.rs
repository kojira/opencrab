//! タスク台帳のプロンプトセクションビルダ。
//!
//! 前向きワーキング状態（goal / 契約 / 進捗 / 決定）を DB から読み出し、
//! 会話文字列の先頭へ前置する `[Task Ledger]` セクションを組み立てる。
//! context 圧縮やプロセス再起動をまたいでも、エージェントは毎 run この
//! セクション経由で作業状態を再ロードできる。
//!
//! 動的な状態は system prompt（1h ephemeral キャッシュ付き）ではなく
//! 会話側に入れる。呼び出し元は server の `build_conversation_string` と
//! discord の subtask engine。

use anyhow::Result;
use opencrab_db::queries;
use rusqlite::Connection;

use crate::llm_text::truncate_chars;

/// セクションに含める直近進捗エントリの件数上限。
pub const LEDGER_RECENT_ENTRIES: usize = 10;

/// 1エントリの描画上限（chars）。超過分は切り詰める。
const LEDGER_ENTRY_MAX_CHARS: usize = 500;

/// goal / contract の描画上限（chars）。action 層の入力上限より広い防衛的キャップ。
const LEDGER_FIELD_MAX_CHARS: usize = 4000;

/// RFC3339 タイムスタンプを表示用の `MM-DD HH:MM` に短縮する。
fn short_timestamp(rfc3339: &str) -> String {
    // "2026-07-02T09:41:23.123+00:00" -> "07-02 09:41"
    // get() で UTF-8 境界を安全に扱う（不正な値でも panic しない）
    match rfc3339.get(5..16) {
        Some(s) => s.replace('T', " "),
        None => rfc3339.to_string(),
    }
}

/// セッションの active タスクから `[Task Ledger]` セクションを組み立てる。
/// active タスクが無ければ `Ok(None)`。
pub fn build_ledger_section(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
) -> Result<Option<String>> {
    let Some(task) = queries::get_active_task_for_session(conn, agent_id, session_id)? else {
        return Ok(None);
    };

    let recent = queries::list_recent_task_progress(conn, task.id, LEDGER_RECENT_ENTRIES)?;
    // COUNT はエントリが上限に達した時（= 切り詰めが起こり得る時）だけ払う
    let total = if recent.len() == LEDGER_RECENT_ENTRIES {
        queries::count_task_progress(conn, task.id)?
    } else {
        recent.len() as i64
    };

    let mut out = String::new();
    out.push_str("[Task Ledger]\n");
    out.push_str(&format!(
        "Active task #{} (opened {}, updated {})\n",
        task.id,
        short_timestamp(&task.created_at),
        short_timestamp(&task.updated_at),
    ));
    out.push_str(&format!(
        "Goal: {}\n",
        truncate_chars(&task.goal, LEDGER_FIELD_MAX_CHARS)
    ));
    match task.contract.as_deref().filter(|c| !c.trim().is_empty()) {
        Some(contract) => out.push_str(&format!(
            "Contract (done when): {}\n",
            truncate_chars(contract, LEDGER_FIELD_MAX_CHARS)
        )),
        None => out.push_str(
            "Contract (done when): (not agreed yet — negotiate acceptance criteria, then update_task_contract)\n",
        ),
    }

    if recent.is_empty() {
        out.push_str("Progress: (no entries yet — record steps with record_task_progress)\n");
    } else {
        if total > recent.len() as i64 {
            out.push_str(&format!(
                "Progress (last {} of {} — use get_task for full history):\n",
                recent.len(),
                total
            ));
        } else {
            out.push_str("Progress:\n");
        }
        for entry in &recent {
            out.push_str(&format!(
                "- [{} {}] {}\n",
                entry.kind,
                short_timestamp(&entry.created_at),
                truncate_chars(&entry.content, LEDGER_ENTRY_MAX_CHARS),
            ));
        }
    }

    out.push_str(
        "This ledger is your persistent working state (survives compaction/restart). \
         Record steps with record_task_progress; close with close_task when the contract is met.",
    );
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        opencrab_db::init_memory().expect("init")
    }

    #[test]
    fn no_active_task_returns_none() {
        let conn = setup();
        assert!(build_ledger_section(&conn, "a1", "s1").unwrap().is_none());
    }

    #[test]
    fn short_timestamp_drops_t_and_never_panics() {
        assert_eq!(short_timestamp("2026-07-02T09:41:23+00:00"), "07-02 09:41");
        assert_eq!(short_timestamp("short"), "short");
        // 先頭16バイト内にマルチバイト文字があっても panic しない
        assert_eq!(
            short_timestamp("2026年07月02日T09:41"),
            "2026年07月02日T09:41"
        );
    }

    #[test]
    fn renders_task_with_truncated_progress() {
        let conn = setup();
        let id = queries::insert_task_ledger(&conn, "a1", "s1", "ship the feature", None).unwrap();
        for i in 1..=12 {
            queries::insert_task_progress(&conn, id, "progress", &format!("step {i}")).unwrap();
        }
        queries::insert_task_progress(&conn, id, "decision", "chose SQLite because no human edits")
            .unwrap();

        let section = build_ledger_section(&conn, "a1", "s1").unwrap().unwrap();
        assert!(section.starts_with("[Task Ledger]"));
        assert!(section.contains(&format!("Active task #{id}")));
        assert!(section.contains("Goal: ship the feature"));
        // contract 未合意のフォールバック行
        assert!(section.contains("(not agreed yet"));
        // 13件中直近10件
        assert!(section.contains("last 10 of 13"));
        assert!(!section.contains("step 3\n"), "old entries must be dropped");
        // 時系列順: step 12 の後に decision が来る
        let pos_step12 = section.find("step 12").unwrap();
        let pos_decision = section.find("[decision").unwrap();
        assert!(pos_step12 < pos_decision);
    }

    #[test]
    fn renders_contract_and_short_progress() {
        let conn = setup();
        let id = queries::insert_task_ledger(&conn, "a1", "s1", "goal", Some("all tests green"))
            .unwrap();
        queries::insert_task_progress(&conn, id, "blocker", &"x".repeat(600)).unwrap();

        let section = build_ledger_section(&conn, "a1", "s1").unwrap().unwrap();
        assert!(section.contains("Contract (done when): all tests green"));
        // 「last N of M」表記なし（全件収まる）
        assert!(section.contains("Progress:\n"));
        // 500 chars + 省略記号で切り詰め
        assert!(section.contains(&format!("{}…", "x".repeat(500))));
        assert!(!section.contains(&"x".repeat(501)));
    }
}
