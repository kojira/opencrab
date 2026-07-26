//! ピアレビュー**返信の回収**（LOOPS 原則 II: 自己採点させるな — #49 phase 2 / #58）。
//!
//! 依頼の**送信側**（定義・引数検査・レビュアー解決・メッセージ組み立て・分割送信・
//! 台帳記録）は #157 S7 で gateway 非依存層（`crates/server/src/peer_review.rs`）へ
//! 移設済み。Discord に残るのは配送口（`super::text_delivery::DiscordTextDelivery`）と、
//! **このファイルの返信回収**だけ。
//!
//! 回収を移していないのは意図的: `record_peer_review_reply` は Discord の受信ループ
//! 1 箇所（`crate::message_loop`）からしか呼ばれず、**汎用の受信フック点は #156 S4 の
//! 担当**だから。フック点ができたらここも汎用層へ移せる。
//!
//! レビュアー側の応答規約（`[Peer Review Request]` には NO_REPLY せず `[Peer Review]` で
//! 応答する）は system prompt（server/process.rs build_agent_context）に定義されている。

use tracing::warn;

use opencrab_core::llm_text::truncate_chars;
use opencrab_gateway::PEER_REVIEW_REPLY_MARKER;

/// `[Peer Review]` 返信のパース結果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PeerReviewVerdict {
    /// 0.0-1.0 に clamp 済み。抽出できなければ None。
    pub score: Option<f64>,
    pub gaps: Vec<String>,
    pub summary: String,
}

/// text 中で `[Peer Review]` marker が行頭（markdown 装飾は許容）に現れる位置を返す。
///
/// debounce がレビュアーの前置きと verdict を1メッセージに結合することがあるため、
/// 先頭だけでなく各行の行頭を見る。行の途中の言及（レビュー対象の diff 等）は無視する。
fn find_reply_marker(text: &str) -> Option<usize> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let stripped = line.trim_start_matches(|c: char| {
            c.is_whitespace() || c == '*' || c == '_' || c == '#' || c == '>'
        });
        if stripped.starts_with(PEER_REVIEW_REPLY_MARKER) {
            return Some(offset + (line.len() - stripped.len()));
        }
        offset += line.len();
    }
    None
}

/// `[Peer Review]` を行頭に含むメッセージから score / gaps / summary を抽出する。
///
/// レビュアーは LLM なので形式ゆれに寛容にパースする（フィールド欠落でも Some を返す）。
/// marker を行頭に含まないメッセージは None。
pub(crate) fn parse_peer_review_reply(text: &str) -> Option<PeerReviewVerdict> {
    let marker_pos = find_reply_marker(text)?;
    let body = &text[marker_pos + PEER_REVIEW_REPLY_MARKER.len()..];
    let lower = body.to_ascii_lowercase();

    // フィールドキーはコロン必須で照合する（"no gaps found" のような本文中の
    // 単語をフィールド開始と誤認して gaps を捏造しないため）
    // score: の後の最初の数値（"0.8", "0.8/1.0", "0.8 (…)" 等の先頭数値を拾う）
    let score = lower.find("score:").and_then(|pos| {
        let after = &body[pos + "score:".len()..];
        let after = after.trim_start();
        let num: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        num.parse::<f64>().ok().map(|v| v.clamp(0.0, 1.0))
    });

    // gaps: から summary:（または末尾）まで
    let gaps = match lower.find("gaps:") {
        Some(pos) => {
            let after = &body[pos + "gaps:".len()..];
            let after = after.trim_start_matches(' ');
            let end = after
                .to_ascii_lowercase()
                .find("summary:")
                .unwrap_or(after.len());
            // インライン形式（"Gaps: none, Summary: ..."）の区切りカンマ等を落とす
            let strip = |s: &str| {
                s.trim_matches(|c: char| c.is_whitespace() || c == ',' || c == ';')
                    .to_string()
            };
            let section = strip(&after[..end]);
            if section.eq_ignore_ascii_case("none") || section.is_empty() {
                Vec::new()
            } else {
                // "- x" 行のリスト、または改行区切りのインライン
                let items: Vec<String> = section
                    .lines()
                    .map(|l| strip(l.trim().trim_start_matches('-')))
                    .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("none"))
                    .collect();
                items
            }
        }
        None => Vec::new(),
    };

    // summary: の後（無ければ本文先頭 200 chars をフォールバック）
    let summary = match lower.find("summary:") {
        Some(pos) => {
            let after = &body[pos + "summary:".len()..];
            after.trim().to_string()
        }
        None => truncate_chars(body.trim(), 200),
    };

    Some(PeerReviewVerdict {
        score,
        gaps,
        summary,
    })
}

/// パース済み verdict をタスク台帳の progress 文字列に整形する。
pub(crate) fn format_peer_review_progress(verdict: &PeerReviewVerdict, reviewer: &str) -> String {
    let score = verdict
        .score
        .map(|s| format!("{s:.2}"))
        .unwrap_or_else(|| "n/a".to_string());
    let gaps = if verdict.gaps.is_empty() {
        "none".to_string()
    } else {
        truncate_chars(&verdict.gaps.join("; "), 800)
    };
    format!(
        "[peer review] score {score} (from {reviewer}): {}; gaps: {gaps}",
        truncate_chars(&verdict.summary, 300),
    )
}

/// active タスクに「未回収のレビュー依頼」があるか判定する。
///
/// 直近の進捗を新しい順に見て、`[peer review]`（受領記録）より後に
/// `[peer review requested]` があれば未回収。これにより:
/// - 依頼していないタスクには第三者間のレビューが記録されない
///   （同一チャンネルの別 bot 同士のレビューを誤記録しない）
/// - 1依頼につき1件だけ記録される（同文の連投は2件目以降スキップ）
fn has_outstanding_review_request(conn: &rusqlite::Connection, task_id: i64) -> bool {
    let recent =
        opencrab_db::queries::list_recent_task_progress(conn, task_id, 30).unwrap_or_default();
    for entry in recent.iter().rev() {
        if entry.content.starts_with("[peer review requested]") {
            return true;
        }
        if entry.content.starts_with("[peer review]") {
            return false;
        }
    }
    false
}

/// 受信した `[Peer Review]` 返信を requester の active タスクへ自動記録する（#58）。
///
/// ゲート（すべて満たす場合のみ記録）:
/// 1. marker が行頭にある
/// 2. 送信者がこのエージェントの登録済み co_agent（第三者・未信頼の偽 verdict を排除）
/// 3. active タスクに未回収のレビュー依頼がある（依頼していないレビューを誤記録しない）
///
/// 記録は追加処理: メッセージ自体はこの後通常どおり LLM にも流れる（会話には speech として残る）。
/// session_logs には重ねて記録しない（二重描画を避ける。台帳経由で次ターンの [Task Ledger] に出る）。
/// 記録した場合 true を返す。
pub(crate) fn record_peer_review_reply(
    db: &opencrab_db::Db,
    agent_id: &str,
    session_id: &str,
    sender_id: &str,
    sender_name: &str,
    text: &str,
) -> bool {
    let Some(verdict) = parse_peer_review_reply(text) else {
        return false;
    };
    let Ok(conn) = db.lock() else {
        warn!("record_peer_review_reply: DB lock failed, review not recorded");
        return false;
    };
    // 送信者ゲート: 登録済み co_agent のみ
    let is_co_agent = opencrab_db::queries::get_trusted_user(&conn, sender_id, agent_id)
        .map(|u| u.permission == "co_agent")
        .unwrap_or(false);
    if !is_co_agent {
        tracing::debug!(
            agent_id = %agent_id,
            sender_id = %sender_id,
            "peer review reply from non-co_agent sender — skipping auto-record"
        );
        return false;
    }
    let Some(task) = opencrab_db::queries::get_active_task_for_session(&conn, agent_id, session_id)
        .ok()
        .flatten()
    else {
        tracing::debug!(
            agent_id = %agent_id,
            session_id = %session_id,
            "peer review reply received but no active task — skipping auto-record"
        );
        return false;
    };
    if !has_outstanding_review_request(&conn, task.id) {
        tracing::debug!(
            agent_id = %agent_id,
            task_id = task.id,
            "peer review reply but no outstanding request on active task — skipping auto-record"
        );
        return false;
    }
    let content = format_peer_review_progress(&verdict, sender_name);
    match opencrab_db::queries::insert_task_progress(&conn, task.id, "progress", &content) {
        Ok(_) => {
            tracing::info!(
                agent_id = %agent_id,
                task_id = task.id,
                score = ?verdict.score,
                reviewer = %sender_name,
                "peer review reply auto-recorded to task ledger"
            );
            true
        }
        Err(e) => {
            warn!("record_peer_review_reply: ledger record failed: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 依頼の**送信側**のテスト（宛先解決 4 / ヘッダ組み立て 4 / レビュアー解決 1）は
    // #157 S7 で server 側（`crates/server/src/peer_review.rs`）へ移設済み。ここに残るのは
    // **返信の回収**（解析・整形・3 つのゲート）のテストだけ。

    #[test]
    fn parse_reply_full_form() {
        let v = parse_peer_review_reply(
            "[Peer Review] score: 0.75\ngaps:\n- tests not run\n- no error handling\nsummary: solid but unverified",
        )
        .unwrap();
        assert_eq!(v.score, Some(0.75));
        assert_eq!(v.gaps, vec!["tests not run", "no error handling"]);
        assert_eq!(v.summary, "solid but unverified");
    }

    #[test]
    fn parse_reply_inline_and_variants() {
        // インライン gaps、score にスラッシュ形式、大文字
        let v = parse_peer_review_reply(
            "  [Peer Review] Score: 0.9/1.0, Gaps: none, Summary: looks good",
        )
        .unwrap();
        assert_eq!(v.score, Some(0.9));
        assert!(v.gaps.is_empty());
        assert_eq!(v.summary, "looks good");

        // score 欠落 → None、summary 欠落 → 本文フォールバック
        let v = parse_peer_review_reply("[Peer Review] this looks fine to me").unwrap();
        assert_eq!(v.score, None);
        assert!(v.summary.contains("looks fine"));

        // 1.0 超は clamp
        let v = parse_peer_review_reply("[Peer Review] score: 8.5 summary: s").unwrap();
        assert_eq!(v.score, Some(1.0));
    }

    #[test]
    fn parse_reply_rejects_non_marker() {
        assert!(parse_peer_review_reply("just chatting about [Peer Review] stuff").is_none());
        assert!(parse_peer_review_reply("[Peer Review Request] from a").is_none());
    }

    #[test]
    fn parse_reply_finds_marker_after_preamble_and_markdown() {
        // debounce がレビュアーの前置きと verdict を結合するケース
        let v = parse_peer_review_reply(
            "Looking at it now.\n[Peer Review] score: 0.8, gaps: none, summary: fine",
        )
        .unwrap();
        assert_eq!(v.score, Some(0.8));

        // markdown 装飾付き行頭
        let v = parse_peer_review_reply("**[Peer Review]** score: 0.5 summary: hm").unwrap();
        assert_eq!(v.score, Some(0.5));

        // 行の途中の言及は依然として無視
        assert!(parse_peer_review_reply("the diff mentions [Peer Review] in prose").is_none());

        // 本文中の "gaps" という単語をフィールド開始と誤認しない（コロン必須）
        let v = parse_peer_review_reply("[Peer Review] score: 1.0 summary: no gaps found").unwrap();
        assert!(v.gaps.is_empty());
        assert_eq!(v.summary, "no gaps found");
    }

    #[test]
    fn format_progress_bounds_and_labels() {
        let v = PeerReviewVerdict {
            score: Some(0.4),
            gaps: vec!["a".repeat(600), "b".to_string()],
            summary: "needs work".to_string(),
        };
        let s = format_peer_review_progress(&v, "crab-b");
        assert!(s.starts_with("[peer review] score 0.40 (from crab-b): needs work"));
        assert!(s.chars().count() < 1300, "progress entry must stay bounded");

        let v = PeerReviewVerdict {
            score: None,
            gaps: vec![],
            summary: "s".to_string(),
        };
        assert!(format_peer_review_progress(&v, "r").contains("score n/a"));
    }

    #[test]
    fn record_reply_gates_and_writes() {
        let db = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        let task_id = {
            let conn = db.lock().unwrap();
            // 送信者 "42" をこのエージェントの co_agent として登録
            opencrab_db::queries::add_trusted_user(
                &conn,
                "row-1",
                "a1",
                "42",
                "co_agent",
                "owner",
                "2026-01-01",
                "Crab B",
            )
            .unwrap();
            let task_id =
                opencrab_db::queries::insert_task_ledger(&conn, "a1", "s1", "goal", None).unwrap();
            // 未回収のレビュー依頼を記録
            opencrab_db::queries::insert_task_progress(
                &conn,
                task_id,
                "progress",
                "[peer review requested] posted to channel 1 (1 parts)",
            )
            .unwrap();
            task_id
        };
        let reply = "[Peer Review] score: 0.6\ngaps:\n- missing tests\nsummary: incomplete";

        // marker 無し → 記録しない
        assert!(!record_peer_review_reply(
            &db, "a1", "s1", "42", "crab-b", "hello"
        ));
        // 未登録送信者（co_agent でない）→ 記録しない
        assert!(!record_peer_review_reply(
            &db, "a1", "s1", "99", "stranger", reply
        ));
        // active タスクの無いセッション → 記録しない
        assert!(!record_peer_review_reply(
            &db, "a1", "other", "42", "crab-b", reply
        ));
        // 正常系
        assert!(record_peer_review_reply(
            &db, "a1", "s1", "42", "crab-b", reply
        ));
        // 依頼が回収済みになったので、追加の返信は記録しない（1依頼1記録）
        assert!(!record_peer_review_reply(
            &db, "a1", "s1", "42", "crab-b", reply
        ));

        let conn = db.lock().unwrap();
        let progress = opencrab_db::queries::list_recent_task_progress(&conn, task_id, 10).unwrap();
        assert_eq!(progress.len(), 2); // requested + received
        assert!(progress[1]
            .content
            .contains("[peer review] score 0.60 (from crab-b)"));
        assert!(progress[1].content.contains("missing tests"));
    }
}
