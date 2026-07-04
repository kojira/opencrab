//! 会話コンテキストへ常時注入する `[Memory Index]` セクションのビルダ。
//!
//! 時間解像度のグラデーションで長期記憶を見せる: 過去の月は月次要約 1 行
//! （rollup_stale_period が生成した period.summary）、現在月は topic 粒度。
//! 全行に short_id が付き、`retrieve_memory_nodes` で原文へ、
//! `search_memory_index` でキーワード逆引きへ接続する。
//!
//! 台帳（task_ledger）と同じく「動的な状態は system prompt ではなく会話側」
//! （system は 1h キャッシュされるため）。レンダリングは決定的で、現在時刻等の
//! 揮発値を含めない — インデックスが変わらない限りバイト単位で安定し、
//! プロンプトキャッシュのプレフィックス安定性を壊さない。

use anyhow::Result;
use rusqlite::Connection;

use crate::llm_text::truncate_chars;

/// セクション全体の文字数上限。ブロック別予算の合計 + ヘッダでこの値を超えない。
/// 注意: 日本語はおよそ 0.7 tokens/char なので、フルサイズで **最大 ~2.5k tokens**
/// になる（英語なら ~1k）。小さいコンテキスト予算での圧迫は注入側
/// （build_conversation_string）が予算比ガードで防ぐ。
pub const MEMORY_INDEX_MAX_CHARS: usize = 3600;
/// 月行ブロックの文字数予算。月次要約がこのセクションの中心なので大半を割く。
/// 超過時は古い月から落とし、`…and N older months` の 1 行に畳む。
const MONTH_BLOCK_MAX_CHARS: usize = 2750;
/// 現在月 topic ブロックの文字数予算（古い topic から落とす）。
const TOPIC_BLOCK_MAX_CHARS: usize = 600;
/// 表示する月数の上限（それより古い月は件数のみ表示）。
pub const MEMORY_INDEX_MAX_MONTHS: usize = 12;
/// 現在月の topic 行の上限。
pub const MEMORY_INDEX_MAX_TOPICS: usize = 15;
/// 月次要約の描画上限（chars）。
const PERIOD_SUMMARY_MAX_CHARS: usize = 250;

/// 先頭から文字数予算内に収まる行だけを取る（行リストは新しい順 = 古い側が落ちる）。
fn take_within_budget(lines: Vec<String>, budget_chars: usize) -> Vec<String> {
    let mut used = 0usize;
    let mut out = Vec::new();
    for line in lines {
        let cost = line.chars().count() + 1; // 改行分
        if used + cost > budget_chars {
            break;
        }
        used += cost;
        out.push(line);
    }
    out
}

/// `[Memory Index]` セクションを組み立てる。インデックスが空なら `Ok(None)`。
///
/// - 月行: 現在月**以外**の全 period（新しい順、`MEMORY_INDEX_MAX_MONTHS` 件まで）。
///   未ロールアップの月は `(summary pending)` として行は出す（初日から形が安定し、
///   ロールアップが進むほど中身が濃くなる）。
/// - topic 行: 現在月の topic（新しい順、`MEMORY_INDEX_MAX_TOPICS` 件まで）。
///   現セッション由来は除外 — 現セッションの topic はコンパクション時の
///   [Past context summary] が担当し、short_id を二重に出さない。
/// - `source_type='daily_log'` のノードはこのセクションには出さない
///   （search_memory_index からは引ける）。
pub fn build_memory_index_section(
    conn: &Connection,
    agent_id: &str,
    current_session_id: &str,
) -> Result<Option<String>> {
    let periods = opencrab_db::queries::list_period_nodes(conn, agent_id)?;
    if periods.is_empty() {
        return Ok(None);
    }
    // 「現在月」はノード側の最新月とする（クロック非依存でレンダリングが決定的。
    // 最新月 = まだ増え続けている月なので topic 粒度で見せる）。ただし最新月が
    // 既にロールアップ済みなら、それは暦上の過去月（エージェントが月を跨いで
    // 非アクティブだった場合）なので月行として出し、topic ブロックは省く —
    // 生成済みの月次要約を埋もれさせない。
    let current_month = periods
        .first()
        .filter(|p| p.summary_refreshed_at.is_none())
        .map(|p| p.title.clone());

    let topics = match &current_month {
        Some(month) => opencrab_db::queries::list_topic_nodes_for_month(
            conn,
            agent_id,
            month,
            current_session_id,
            MEMORY_INDEX_MAX_TOPICS,
        )?,
        None => Vec::new(),
    };
    let past_periods: Vec<_> = periods
        .iter()
        .filter(|p| Some(&p.title) != current_month.as_ref())
        .collect();
    if past_periods.is_empty() && topics.is_empty() {
        return Ok(None);
    }
    let topic_counts = opencrab_db::queries::count_topics_per_period(conn, agent_id)?;

    let mut month_lines: Vec<String> = Vec::new();
    for p in past_periods.iter().take(MEMORY_INDEX_MAX_MONTHS) {
        let sid = p.short_id.as_deref().unwrap_or(&p.id);
        let summary = if p.summary_refreshed_at.is_some() {
            truncate_chars(&p.summary, PERIOD_SUMMARY_MAX_CHARS)
        } else {
            "(summary pending)".to_string()
        };
        // child_count は直下の session 数なので topic 総数は別クエリで引く
        let n_topics = topic_counts.get(&p.id).copied().unwrap_or(0);
        month_lines.push(format!(
            "- [{sid}] {} ({n_topics} topics): {summary}",
            p.title
        ));
    }
    // ブロック別予算で古い側から刈る（月と topic が互いを締め出さない）。
    // リストは新しい順なので take_within_budget は新しい記憶を優先して残す。
    let mut month_lines = take_within_budget(month_lines, MONTH_BLOCK_MAX_CHARS);
    let hidden_months = past_periods.len().saturating_sub(month_lines.len());
    // 表示は時系列（古い→新しい）: 会話ログの流れと揃い、月の追加が常に
    // セクション下側の変更になるためプロンプトキャッシュのプレフィックスも安定する。
    month_lines.reverse();

    let mut topic_lines: Vec<String> = Vec::new();
    for t in &topics {
        let sid = t.short_id.as_deref().unwrap_or(&t.id);
        let date = t
            .date_from
            .as_deref()
            .and_then(|d| d.get(5..10))
            .unwrap_or("");
        topic_lines.push(format!("- [{sid}] {date} {}", t.title));
    }
    let mut topic_lines = take_within_budget(topic_lines, TOPIC_BLOCK_MAX_CHARS);
    topic_lines.reverse();

    let mut out = String::new();
    out.push_str(
        "[Memory Index] (your long-term memory; retrieve_memory_nodes(short_id) for full logs, search_memory_index(query) to search)\n",
    );
    if !month_lines.is_empty() {
        out.push_str("Months:\n");
        // 畳んだ月は最も古い側 = 先頭に置く
        if hidden_months > 0 {
            out.push_str(&format!(
                "  …and {hidden_months} older months (browse_memory_index)\n"
            ));
        }
        for l in &month_lines {
            out.push_str(l);
            out.push('\n');
        }
    } else if hidden_months > 0 {
        out.push_str(&format!(
            "  …and {hidden_months} older months (browse_memory_index)\n"
        ));
    }
    if !topic_lines.is_empty() {
        out.push_str("This month's topics (other sessions):\n");
        for l in &topic_lines {
            out.push_str(l);
            out.push('\n');
        }
    }

    Ok(Some(out.trim_end().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_node(
        id: &str,
        node_type: &str,
        parent: Option<&str>,
        title: &str,
        source_session_id: Option<&str>,
        date_from: Option<&str>,
    ) -> opencrab_db::queries::IndexNodeRow {
        opencrab_db::queries::IndexNodeRow {
            id: id.to_string(),
            agent_id: "a1".to_string(),
            parent_id: parent.map(String::from),
            node_type: node_type.to_string(),
            source_type: "session_log".to_string(),
            title: title.to_string(),
            summary: format!("{title} summary"),
            start_log_id: None,
            end_log_id: None,
            source_session_id: source_session_id.map(String::from),
            date_from: date_from.map(String::from),
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: format!("{}T00:00:00Z", date_from.unwrap_or("2026-01-01")),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: Some(id.to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        }
    }

    fn seed(conn: &Connection) {
        use opencrab_db::queries::*;
        insert_index_node(conn, &mk_node("r1", "root", None, "root", None, None)).unwrap();
        // 2 か月分の period + 現在月（最新月 = 2026-06）
        insert_index_node(
            conn,
            &mk_node("p1", "period", Some("r1"), "2026-04", None, None),
        )
        .unwrap();
        insert_index_node(
            conn,
            &mk_node("p2", "period", Some("r1"), "2026-05", None, None),
        )
        .unwrap();
        insert_index_node(
            conn,
            &mk_node("p3", "period", Some("r1"), "2026-06", None, None),
        )
        .unwrap();
        // 2026-04 はロールアップ済み
        update_period_rollup(conn, "p1", "4月はDiscord連携に集中した。", "[\"Discord\"]").unwrap();
        // 現在月のセッションと topic（他セッション + 現セッション + daily_log）
        insert_index_node(conn, &mk_node("s1", "session", Some("p3"), "S", None, None)).unwrap();
        insert_index_node(
            conn,
            &mk_node(
                "t1",
                "topic",
                Some("s1"),
                "他セッションの話",
                Some("other-session"),
                Some("2026-06-10"),
            ),
        )
        .unwrap();
        insert_index_node(
            conn,
            &mk_node(
                "t2",
                "topic",
                Some("s1"),
                "現セッションの話",
                Some("current-session"),
                Some("2026-06-11"),
            ),
        )
        .unwrap();
        let mut daily = mk_node("d1", "topic", None, "daily由来", None, Some("2026-06-12"));
        daily.source_type = "daily_log".to_string();
        insert_index_node(conn, &daily).unwrap();
    }

    #[test]
    fn empty_index_returns_none() {
        let conn = opencrab_db::init_memory().unwrap();
        assert!(build_memory_index_section(&conn, "a1", "s")
            .unwrap()
            .is_none());
    }

    #[test]
    fn renders_month_gradient_and_excludes_current_session_and_daily() {
        let conn = opencrab_db::init_memory().unwrap();
        seed(&conn);
        let section = build_memory_index_section(&conn, "a1", "current-session")
            .unwrap()
            .unwrap();
        assert!(section.starts_with("[Memory Index]"));
        // 月行: 時系列（古い 2026-04 が上）、現在月 2026-06 は月行に出ない
        let pos_may = section.find("[p2] 2026-05").unwrap();
        let pos_apr = section.find("[p1] 2026-04").unwrap();
        assert!(pos_apr < pos_may);
        assert!(!section.contains("[p3] 2026-06"));
        // ロールアップ済みは要約、未ロールアップは pending
        assert!(section.contains("4月はDiscord連携に集中した。"));
        assert!(section.contains("[p2] 2026-05 (0 topics): (summary pending)"));
        // 現在月 topic: 他セッションのみ、現セッションと daily_log は出ない
        assert!(section.contains("[t1] 06-10 他セッションの話"));
        assert!(!section.contains("現セッションの話"));
        assert!(!section.contains("daily由来"));
        // 全行に short_id
        assert!(section.contains("[t1]") && section.contains("[p1]"));
    }

    #[test]
    fn rolled_up_newest_month_renders_as_month_line() {
        // 月を跨いで非アクティブだったエージェント: 最新 period が既にロールアップ
        // 済みなら、それは暦上の過去月 — topic 粒度ではなく月行として要約を見せる
        let conn = opencrab_db::init_memory().unwrap();
        use opencrab_db::queries::*;
        insert_index_node(&conn, &mk_node("r1", "root", None, "root", None, None)).unwrap();
        insert_index_node(
            &conn,
            &mk_node("p1", "period", Some("r1"), "2026-03", None, None),
        )
        .unwrap();
        update_period_rollup(&conn, "p1", "3月の月次要約。", "[]").unwrap();
        insert_index_node(
            &conn,
            &mk_node("s1", "session", Some("p1"), "S", None, None),
        )
        .unwrap();
        insert_index_node(
            &conn,
            &mk_node(
                "t1",
                "topic",
                Some("s1"),
                "3月の話",
                Some("other"),
                Some("2026-03-05"),
            ),
        )
        .unwrap();

        let section = build_memory_index_section(&conn, "a1", "s")
            .unwrap()
            .unwrap();
        assert!(section.contains("[p1] 2026-03 (1 topics): 3月の月次要約。"));
        assert!(!section.contains("This month's topics"));
    }

    #[test]
    fn char_cap_drops_topics_before_months() {
        let conn = opencrab_db::init_memory().unwrap();
        use opencrab_db::queries::*;
        insert_index_node(&conn, &mk_node("r1", "root", None, "root", None, None)).unwrap();
        // 12 か月分の長い月次要約（各行は 250 chars に切り詰められる）
        for i in 0..12 {
            let id = format!("p{i}");
            let title = format!("2025-{:02}", i + 1);
            insert_index_node(
                &conn,
                &mk_node(&id, "period", Some("r1"), &title, None, None),
            )
            .unwrap();
            update_period_rollup(&conn, &id, &"長い要約。".repeat(60), "[]").unwrap();
        }
        // 現在月（最新）+ 長いタイトルの topic 群で上限を超過させる
        insert_index_node(
            &conn,
            &mk_node("pc", "period", Some("r1"), "2026-01", None, None),
        )
        .unwrap();
        insert_index_node(
            &conn,
            &mk_node("sc", "session", Some("pc"), "S", None, None),
        )
        .unwrap();
        for i in 0..10 {
            let id = format!("t{i}");
            let title = format!("topic-{i}-{}", "x".repeat(90));
            insert_index_node(
                &conn,
                &mk_node(
                    &id,
                    "topic",
                    Some("sc"),
                    &title,
                    Some("other"),
                    Some(&format!("2026-01-{:02}", i + 1)),
                ),
            )
            .unwrap();
        }

        let section = build_memory_index_section(&conn, "a1", "s")
            .unwrap()
            .unwrap();
        assert!(section.chars().count() <= MEMORY_INDEX_MAX_CHARS);
        // 月ブロック: 新しい月は残り、予算超過分は古い月から畳まれて件数表示になる
        assert!(section.contains("[p11] 2025-12"));
        assert!(!section.contains("[p0] 2025-01"));
        assert!(section.contains("older months (browse_memory_index)"));
        // topic ブロック: 月と独立の予算を持ち、新しい topic は必ず残る
        assert!(section.contains("[t9]"), "newest topic must survive");
        assert!(!section.contains("[t0]"), "oldest topic must be dropped");
        // 表示は時系列: 畳み行が最上部、残存する最古の月 → 最新の月の順、
        // topic も古い→新しい
        let pos_fold = section.find("older months").unwrap();
        let pos_oldest_kept = section.find("[p2] 2025-03").unwrap_or(usize::MAX - 1);
        let pos_dec = section.find("[p11] 2025-12").unwrap();
        assert!(pos_fold < pos_dec);
        assert!(pos_oldest_kept < pos_dec || pos_oldest_kept == usize::MAX - 1);
        let pos_t5 = section.find("[t5]").unwrap();
        let pos_t9 = section.find("[t9]").unwrap();
        assert!(pos_t5 < pos_t9, "topics must render oldest-first");
    }
}
