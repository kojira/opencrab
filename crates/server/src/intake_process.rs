//! 受信箱（`agent_inbox`）の消化ループ（webhook intake / issue #454）。
//!
//! # なぜ heartbeat と別ループか
//!
//! heartbeat のループ群は「グローバル有効 or opt-in 済みエージェントが居る」ときしか張られない
//! （`make_heartbeat_callback` を回す `heartbeat_loop`）。inbox 消化をそこへ相乗りさせると、
//! webhook 対象エージェントの heartbeat が無効なとき **inbox が黙って消化されない**（silent
//! no-op）。それを避けるため常時起動の専用ループにする。
//!
//! # コスト制御（受け入れ基準）
//!
//! 「inbox 空の tick では LLM 呼び出しが発生しない」を満たすため、まず未処理行を持つ
//! エージェントだけを [`agents_with_unprocessed_inbox`] で絞り、**未処理が 1 件も無ければ
//! turn を起こさない**（DB クエリ 1 本で終わる）。
//!
//! # turn の実体
//!
//! 既存の [`HeartbeatTurnRunner`] を再利用する（直列化ロック・dispatch・発話配送・SPEAK 解釈を
//! 流用）。未処理イベントを agent-scoped の HB セッションへ system ログとして差し込んで**から**
//! turn を起こす。差し込み時点で「配送」は完了とみなして processed を刻む（turn が失敗しても
//! イベントは会話ログに残り、次の tick 以降の文脈に載る）。

use std::sync::Arc;
use std::time::Duration;

use opencrab_db::queries::{
    agents_with_unprocessed_inbox, insert_session_log, list_unprocessed_inbox,
    mark_inbox_processed, AgentInboxRow, SessionLogRow,
};
use opencrab_server::AppState;

use crate::heartbeat_turn::{HeartbeatTarget, HeartbeatTurnRunner, TurnOrigin};

/// 1 エージェントから 1 tick で消化する未処理イベントの上限（バッチ）。
const INBOX_BATCH_LIMIT: i64 = 20;

/// 1 イベントの payload を prompt に載せるときの最大文字数（文脈予算の暴発を防ぐ）。
const PAYLOAD_PREVIEW_CHARS: usize = 4000;

/// ループの下限間隔（秒）。設定値はそのまま保持し、ここで床を効かせる（既存ループと同流儀）。
const MIN_INTERVAL_SECS: u64 = 10;

/// 受信箱消化ループを起動する（常時。source アダプタや heartbeat 設定に依存しない）。
pub fn spawn_intake_process_loop(state: AppState, runner: Arc<HeartbeatTurnRunner>) {
    let interval_secs = state.intake.process_interval_secs.max(MIN_INTERVAL_SECS);
    tokio::spawn(async move {
        let interval = Duration::from_secs(interval_secs);
        tracing::info!(interval_secs, "intake process loop started");
        loop {
            process_all_inboxes(&state, &runner).await;
            tokio::time::sleep(interval).await;
        }
    });
}

/// 未処理を持つエージェントだけを順に消化する。空なら turn を一切起こさない。
async fn process_all_inboxes(state: &AppState, runner: &Arc<HeartbeatTurnRunner>) {
    let agent_ids = {
        let Ok(conn) = state.db.lock() else {
            return;
        };
        match agents_with_unprocessed_inbox(&conn) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "intake process: 未処理エージェントの走査に失敗");
                return;
            }
        }
    };
    for stored_agent_id in agent_ids {
        process_agent_inbox(state, runner, &stored_agent_id).await;
    }
}

/// 1 エージェント分を消化する。
///
/// `stored_agent_id` は受信時に保存した値（config のルート値 = 名前 or UUID）。turn は
/// heartbeat と同じく解決した UUID で走らせる（名前→UUID は `resolve_agent_id`）。
async fn process_agent_inbox(
    state: &AppState,
    runner: &Arc<HeartbeatTurnRunner>,
    stored_agent_id: &str,
) {
    // (a) 未処理を取得（短いロック）。
    let rows = {
        let Ok(conn) = state.db.lock() else {
            return;
        };
        match list_unprocessed_inbox(&conn, stored_agent_id, INBOX_BATCH_LIMIT) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(agent_id = %stored_agent_id, error = %e, "intake process: 取得失敗");
                return;
            }
        }
    };
    if rows.is_empty() {
        return; // 直前に他所が処理した等。turn は起こさない。
    }

    // (b) 名前→UUID 解決（短いロック / heartbeat と同じ）。
    let resolved_agent_id = {
        let Ok(conn) = state.db.lock() else {
            return;
        };
        crate::resolve_agent_id(&conn, stored_agent_id)
    };

    // (c) agent-scoped の HB セッション（channel_id=""）。内部でロックするので (a)(b) の
    //     ロックは既に落ちている。
    let session_id = crate::get_or_create_heartbeat_session(&state.db, &resolved_agent_id, "");

    // (d) イベントを system ログへ差し込み、processed を刻む（短いロック）。
    //     差し込み = 配送完了とみなす。turn が失敗してもイベントは会話ログに残る。
    {
        let Ok(conn) = state.db.lock() else {
            return;
        };
        let content = build_inbox_prompt(&rows);
        let log = SessionLogRow {
            id: None,
            agent_id: resolved_agent_id.clone(),
            session_id: session_id.clone(),
            log_type: "system".to_string(),
            content,
            speaker_id: Some("intake".to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        if let Err(e) = insert_session_log(&conn, &log) {
            // 差し込めなければ processed を刻まない（次 tick で再試行）。
            tracing::warn!(agent_id = %resolved_agent_id, error = %e, "intake process: セッションログ差し込み失敗");
            return;
        }
        for r in &rows {
            if let Err(e) = mark_inbox_processed(&conn, &r.id) {
                tracing::warn!(agent_id = %resolved_agent_id, inbox_id = %r.id, error = %e, "intake process: processed マーク失敗");
            }
        }
    }

    // (e) turn（ロック無し・await）。失敗（None = 文脈組み立て失敗）はイベントを会話へ
    //     残したまま握る。SubtaskResume と同じく直前の決定を保つ意味は無いので戻り値は捨てる。
    let target = HeartbeatTarget {
        agent_id: resolved_agent_id,
        session_id,
        channel_id: String::new(),
        guild_id: String::new(),
        instructions_source: "intake",
    };
    runner
        .run_turn(&target, TurnOrigin::InboxDelivery { count: rows.len() })
        .await;
}

/// 未処理イベント群を 1 つの system prompt にまとめる。会話文字列の再構築で載る本文。
fn build_inbox_prompt(rows: &[AgentInboxRow]) -> String {
    let mut s = format!(
        "[受信箱] 未処理の外部イベントが {} 件届いています:\n",
        rows.len()
    );
    for (i, r) in rows.iter().enumerate() {
        let payload = truncate_chars(&r.payload_json, PAYLOAD_PREVIEW_CHARS);
        s.push_str(&format!(
            "\n{}. [{}/{}] (received_at={})\n{}\n",
            i + 1,
            r.source,
            r.event_type,
            r.received_at,
            payload
        ));
    }
    s
}

/// 文字数（char 単位）で切り詰める。マルチバイト境界を割らない。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…（{}文字を省略）", s.chars().count() - max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, source: &str, ev: &str, payload: &str) -> AgentInboxRow {
        AgentInboxRow {
            id: id.to_string(),
            agent_id: "scout".to_string(),
            source: source.to_string(),
            event_type: ev.to_string(),
            dedup_key: format!("{ev}:{id}"),
            payload_json: payload.to_string(),
            received_at: "2026-08-09T00:00:00Z".to_string(),
            processed_at: None,
        }
    }

    #[test]
    fn prompt_lists_all_events_with_source_and_type() {
        let rows = vec![
            row(
                "1",
                "omoikane",
                "comment.created",
                "{\"id\":1,\"text\":\"hi\"}",
            ),
            row("2", "omoikane", "chat.message", "{\"id\":2}"),
        ];
        let p = build_inbox_prompt(&rows);
        assert!(p.contains("2 件"));
        assert!(p.contains("omoikane/comment.created"));
        assert!(p.contains("omoikane/chat.message"));
        assert!(p.contains("\"text\":\"hi\""));
    }

    #[test]
    fn truncate_respects_char_boundary_and_marks_omission() {
        let long = "あ".repeat(5000);
        let out = truncate_chars(&long, PAYLOAD_PREVIEW_CHARS);
        // char 数で切る（バイトではない）。省略マーカーが付く。
        assert!(out.chars().count() < 5000);
        assert!(out.contains("文字を省略"));
        // 短い入力はそのまま。
        assert_eq!(truncate_chars("short", 100), "short");
    }
}
