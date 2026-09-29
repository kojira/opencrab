//! 逐語で残せなかった区間を、トピック要約で隙間なくつなぐ（#1049）。
//!
//! 並びは新しい順に「逐語 → 現セッションのトピック要約 → [Memory Index]（宣言ユニット・月次要約）」。
//! トピック要約は宣言ユニットか月次要約が覆う地点で止め、それより古い部分は
//! [Memory Index] に任せる（だんだん粗くなる）。

use rusqlite::Connection;

/// 橋渡しに使うトピック要約 1 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeLine {
    pub start_log_id: i64,
    pub end_log_id: i64,
    pub text: String,
    pub tokens: usize,
}

/// 橋渡し区間のヘッダ。
pub const BRIDGE_HEADER: &str =
    "[Earlier in this conversation (topic summaries; retrieve_memory_nodes(short_id) for full logs)]";

/// 現セッションのトピック要約を、宣言ユニット・月次要約が覆っていない範囲だけ集める（古い順）。
pub fn load_bridge_lines(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
) -> anyhow::Result<Vec<BridgeLine>> {
    let topics = opencrab_db::queries::get_topic_nodes_for_session(conn, agent_id, session_id)?;
    let unit_end = opencrab_db::queries::list_recent_memory_units(conn, agent_id, 1)?
        .first()
        .and_then(|u| u.end_log_id)
        .unwrap_or(0);
    let rolled_months: Vec<String> = opencrab_db::queries::list_period_nodes(conn, agent_id)?
        .into_iter()
        .filter(|p| p.summary_refreshed_at.is_some())
        .map(|p| p.title)
        .collect();
    let mut out = Vec::new();
    for t in topics {
        let (Some(start), Some(end)) = (t.start_log_id, t.end_log_id) else {
            continue;
        };
        if end <= unit_end {
            continue;
        }
        let month = t
            .date_to
            .as_deref()
            .or(t.date_from.as_deref())
            .unwrap_or("");
        if month.len() >= 7 && rolled_months.iter().any(|m| m == &month[..7]) {
            continue;
        }
        let key = t.short_id.as_deref().unwrap_or(&t.id);
        let date = t
            .date_from
            .as_deref()
            .filter(|d| d.len() >= 16)
            .map(|d| format!(" ({})", d[5..16].replace('T', " ")))
            .unwrap_or_default();
        let text = format!("- [{key}]{date} {}: {}", t.title, t.summary);
        let tokens = crate::tokens::estimate_tokens(&text) + 1;
        out.push(BridgeLine {
            start_log_id: start,
            end_log_id: end,
            text,
            tokens,
        });
    }
    Ok(out)
}

/// 逐語の最古 log id（`boundary`）より前にかかるトピック要約を、新しい順に `budget` まで取り、
/// 時系列順のブロック文字列とその token 数を返す。何も入らなければ `None`。
pub fn render_bridge(
    lines: &[BridgeLine],
    boundary: i64,
    budget: usize,
) -> Option<(String, usize)> {
    let header_tokens = crate::tokens::estimate_tokens(BRIDGE_HEADER) + 1;
    if budget <= header_tokens {
        return None;
    }
    let mut used = header_tokens;
    let mut kept: Vec<&str> = Vec::new();
    for l in lines.iter().rev().filter(|l| l.start_log_id < boundary) {
        if used + l.tokens > budget {
            break;
        }
        used += l.tokens;
        kept.push(&l.text);
    }
    if kept.is_empty() {
        return None;
    }
    kept.reverse();
    Some((format!("{BRIDGE_HEADER}\n{}", kept.join("\n")), used))
}

/// `log_id` がトピック要約で覆われているか（索引が追いついていない直近は false）。
pub fn is_covered(lines: &[BridgeLine], log_id: i64) -> bool {
    lines.last().is_some_and(|l| l.end_log_id >= log_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(start: i64, end: i64, tokens: usize) -> BridgeLine {
        BridgeLine {
            start_log_id: start,
            end_log_id: end,
            text: format!("- [t{start}]"),
            tokens,
        }
    }

    #[test]
    fn newest_topics_first_within_budget_rendered_chronologically() {
        let lines = vec![
            line(1, 9, 10),
            line(10, 19, 10),
            line(20, 29, 10),
            line(30, 39, 10),
        ];
        let budget = crate::tokens::estimate_tokens(BRIDGE_HEADER) + 1 + 20;
        let (text, _) = render_bridge(&lines, 30, budget).expect("bridge");
        assert!(
            !text.contains("[t30]"),
            "逐語側にある topic は出さない: {text}"
        );
        let a = text.find("[t10]").expect("t10");
        let b = text.find("[t20]").expect("t20");
        assert!(a < b, "時系列順: {text}");
        assert!(!text.contains("[t1]"), "予算外の古い側から落ちる: {text}");
    }
}
