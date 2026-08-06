//! 会話コンテキストへ常時注入する `[Memory Index]` セクションのビルダ。
//!
//! 時間解像度のグラデーションで長期記憶を見せる: 過去の月は月次要約 1 行
//! （rollup_stale_period が生成した period.summary）、その下に本人が宣言した
//! 記憶の単位（`node_type='unit'` / #403）、宣言が届いていない直近は topic 粒度。
//! 全行に short_id が付き、`retrieve_memory_nodes` で原文へ、
//! `search_memory_index` でキーワード逆引きへ接続する。
//!
//! topic は機械の切り方、unit は本人の切り方なので、**同じ期間を二重に見せない**:
//! 生ログ id 範囲が unit と重なる topic は topic 行から落とす（#403）。
//!
//! 台帳（task_ledger）と同じく「動的な状態は system prompt ではなく会話側」
//! （system は 1h キャッシュされるため）。レンダリングは決定的で、現在時刻等の
//! 揮発値を含めない — インデックスが変わらない限りバイト単位で安定し、
//! プロンプトキャッシュのプレフィックス安定性を壊さない。

use anyhow::Result;
use rusqlite::Connection;

use crate::llm_text::truncate_chars;

/// セクション全体の文字数上限。ブロック別予算の合計（2750 + 1500 + 600 = 4850）+
/// ヘッダ・畳み行の最大 308 chars を上回る値。
/// 注意: 日本語はおよそ 0.7 tokens/char なので、フルサイズで **最大 ~3.6k tokens**
/// になる（英語なら ~1.3k）。小さいコンテキスト予算での圧迫は注入側
/// （build_conversation_string）が予算比ガードで防ぐ — ガードはセクションが
/// 予算の 1/4 を超えると**セクションごと落とす**ので、この値を上げるときは
/// 「フルサイズ × 4 < 想定予算」を必ず確認する（既定予算 50k tokens に対し
/// 3.6k × 4 = 14.4k で通る）。
pub const MEMORY_INDEX_MAX_CHARS: usize = 5200;
/// 月行ブロックの文字数予算。月次要約がこのセクションの中心なので大半を割く。
/// 超過時は古い月から落とし、`…and N older months` の 1 行に畳む。
const MONTH_BLOCK_MAX_CHARS: usize = 2750;
/// 宣言ユニット行ブロックの文字数予算（古いユニットから落とす）。
/// タイトル長は本番実測で p50=32 / p90=42 chars、行頭 `- [uNN] MM-DD ` が 15 chars
/// なので p50 行 ≒ 47 chars。[`MEMORY_INDEX_MAX_UNITS`] = 30 行 × 47 ≒ 1410 で、
/// 通常は**件数上限が先に効く**。この予算は異常に長いタイトルが続いたときの帽子。
const UNIT_BLOCK_MAX_CHARS: usize = 1500;
/// 現在月 topic ブロックの文字数予算（古い topic から落とす）。
const TOPIC_BLOCK_MAX_CHARS: usize = 600;
/// 表示する月数の上限（それより古い月は件数のみ表示）。
pub const MEMORY_INDEX_MAX_MONTHS: usize = 12;
/// 宣言ユニット行の上限。本番実測でユニットは agent ごとに 57 / 69 / 208 件あり
/// 全件は載らない。30 件は、宣言の粒度が実測でおよそ 3 日 = 1 ユニットなので
/// **直近 3 か月ぶん**にあたり、月行（過去 12 か月の月次要約）と現在月 topic の
/// 間を埋める。これより古いユニットは畳み行で件数だけ知らせ、`browse_memory_index`
/// / `search_memory_index` から引ける（FTS には全件載っている）。
pub const MEMORY_INDEX_MAX_UNITS: usize = 30;
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
/// - unit 行: 本人が宣言した記憶の単位（新しい順、`MEMORY_INDEX_MAX_UNITS` 件まで /
///   #403）。エージェント単位（セッション横断・生涯スコープ）で、月をまたぐ。
///   溢れたぶんは畳み行で件数だけ知らせる。
/// - topic 行: 現在月の topic のうち **unit が覆っていないもの**（新しい順、
///   `MEMORY_INDEX_MAX_TOPICS` 件まで）。現セッション由来は除外 — 現セッションの
///   topic はコンパクション時の [Past context summary] が担当し、short_id を
///   二重に出さない。
/// - `source_type='daily_log'` のノードはこのセクションには出さない
///   （search_memory_index からは引ける）。
///
/// 宣言ユニットが 0 件のエージェントでは unit ブロックが丸ごと出ず、topic の
/// 除外も効かないため、出力は #403 以前とバイト単位で同一になる。
pub fn build_memory_index_section(
    conn: &Connection,
    agent_id: &str,
    current_session_id: &str,
) -> Result<Option<String>> {
    let periods = opencrab_db::queries::list_period_nodes(conn, agent_id)?;
    // 宣言ユニットは period ツリーとは独立（parent は declared root）なので、
    // period が無くてもユニットだけで記憶を見せられる。
    let units =
        opencrab_db::queries::list_recent_memory_units(conn, agent_id, MEMORY_INDEX_MAX_UNITS)?;
    if periods.is_empty() && units.is_empty() {
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

    // unit が覆う範囲（生ログ id）の topic は出さない: topic は機械の切り方、
    // unit は本人の切り方で、同じ期間を二重に見せないため。覆っていない範囲
    // （宣言が届いていない直近 / 読んだが宣言しなかった飛び）は従来どおり出る。
    let topics = match &current_month {
        Some(month) => opencrab_db::queries::list_undeclared_topic_nodes_for_month(
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
    if past_periods.is_empty() && topics.is_empty() && units.is_empty() {
        return Ok(None);
    }
    // 総数は畳み行のためだけに要る。上限に届いていなければ取得済みの件数が総数なので
    // COUNT を打たない（会話のたびに走るビルダなのでクエリを増やさない）。
    let unit_total = if units.len() < MEMORY_INDEX_MAX_UNITS {
        units.len()
    } else {
        opencrab_db::queries::count_memory_units(conn, agent_id)?
    };
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
    // 表示は時系列（古い→新しい）: 会話ログの流れと揃う。副次効果として、
    // 表示月数が上限内の間は月の追加がセクション下側の変更になり、プロンプト
    // キャッシュのプレフィックスが安定する（畳み行が出る >12 か月の定常状態では
    // 畳み行の件数が先頭側で更新されるため、この恩恵は月替わり時のみ弱まる）。
    month_lines.reverse();

    // unit 行。リストは新しい順（生ログ位置の降順）なので、予算超過で落ちるのは
    // 古い側 = **新しい記憶が残る**。畳み行の件数は「表示できなかった総数」。
    let mut unit_lines: Vec<String> = Vec::new();
    for u in &units {
        let sid = u.short_id.as_deref().unwrap_or(&u.id);
        let date = u
            .date_from
            .as_deref()
            .and_then(|d| d.get(5..10))
            .unwrap_or("");
        unit_lines.push(format!("- [{sid}] {date} {}", u.title));
    }
    let mut unit_lines = take_within_budget(unit_lines, UNIT_BLOCK_MAX_CHARS);
    let hidden_units = unit_total.saturating_sub(unit_lines.len());
    // 表示は月行・topic 行と同じく時系列（古い→新しい）。
    unit_lines.reverse();

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
        // 畳んだ月は最も古い側 = 先頭に置く。
        // （月行 1 本は最大 ~310 chars < 予算 2750 なので、past_periods が
        // 非空なら month_lines も必ず非空 — 「畳み行だけ」の状態は起きない）
        if hidden_months > 0 {
            out.push_str(&format!(
                "  …and {hidden_months} older months (browse_memory_index)\n"
            ));
        }
        for l in &month_lines {
            out.push_str(l);
            out.push('\n');
        }
    }
    if !unit_lines.is_empty() {
        out.push_str("Declared memories (your own cuts):\n");
        // 畳んだユニットは最も古い側 = 先頭に置く（月行と同じ並べ方）。
        if hidden_units > 0 {
            out.push_str(&format!(
                "  …and {hidden_units} older declared memories (browse_memory_index)\n"
            ));
        }
        for l in &unit_lines {
            out.push_str(l);
            out.push('\n');
        }
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

    // ---- #403: 宣言ユニットの注入 ----

    /// 生ログ id 範囲を持つ現在月 topic を 1 本足す。
    fn seed_topic_with_range(conn: &Connection, id: &str, day: &str, from: i64, to: i64) {
        let mut t = mk_node(
            id,
            "topic",
            Some("s1"),
            &format!("機械の切り方 {id}"),
            Some("other-session"),
            Some(&format!("2026-06-{day}")),
        );
        t.start_log_id = Some(from);
        t.end_log_id = Some(to);
        opencrab_db::queries::insert_index_node(conn, &t).unwrap();
    }

    fn declare(conn: &Connection, title: &str, from: i64, to: i64, day: &str) {
        opencrab_db::queries::record_memory_unit(
            conn,
            "a1",
            title,
            "",
            from,
            to,
            Some(&format!("2026-06-{day}T00:00:00Z")),
            Some(&format!("2026-06-{day}T00:00:00Z")),
            "2026-06-30T00:00:00Z",
        )
        .unwrap();
    }

    /// 宣言ユニットが 0 件なら出力は #403 以前とバイト単位で同一。
    /// （この期待値は origin/main（06db727）の実行結果をそのまま貼ったもの）
    #[test]
    fn without_units_output_is_byte_identical_to_pre_403() {
        let conn = opencrab_db::init_memory().unwrap();
        seed(&conn);
        let section = build_memory_index_section(&conn, "a1", "current-session")
            .unwrap()
            .unwrap();
        assert_eq!(
            section,
            "[Memory Index] (your long-term memory; retrieve_memory_nodes(short_id) for full logs, search_memory_index(query) to search)\n\
             Months:\n\
             - [p1] 2026-04 (0 topics): 4月はDiscord連携に集中した。\n\
             - [p2] 2026-05 (0 topics): (summary pending)\n\
             This month's topics (other sessions):\n\
             - [t1] 06-10 他セッションの話"
        );
        assert!(!section.contains("Declared memories"));
    }

    /// unit は出る / unit が覆う範囲の topic は出ない / 覆っていない範囲は出る。
    #[test]
    fn units_render_and_suppress_only_the_topics_they_cover() {
        let conn = opencrab_db::init_memory().unwrap();
        seed(&conn);
        // 現在月の topic 3 本: 覆われる / 覆われない / id 範囲を持たない
        seed_topic_with_range(&conn, "tc", "13", 100, 199);
        seed_topic_with_range(&conn, "tu", "14", 300, 399);
        // t1（seed 産）は start/end とも NULL
        declare(&conn, "本人の切り方: 覆う", 100, 250, "13");

        let section = build_memory_index_section(&conn, "a1", "current-session")
            .unwrap()
            .unwrap();
        // unit が short_id 付きで出る
        assert!(
            section.contains("Declared memories (your own cuts):"),
            "{section}"
        );
        assert!(
            section.contains("[u1] 06-13 本人の切り方: 覆う"),
            "{section}"
        );
        // 覆われた topic は二重に出さない
        assert!(!section.contains("[tc]"), "covered topic must be dropped");
        // 覆っていない範囲の topic は従来どおり出る
        assert!(section.contains("[tu]"), "uncovered topic must remain");
        // id 範囲を持たない topic は判定不能 = 落とさない（材料を失わない側に倒す）
        assert!(
            section.contains("[t1]"),
            "topic without log ids must remain"
        );
        // 位置: 月行 → unit 行 → topic 行
        let pos_month = section.find("Months:").unwrap();
        let pos_unit = section.find("Declared memories").unwrap();
        let pos_topic = section.find("This month's topics").unwrap();
        assert!(pos_month < pos_unit && pos_unit < pos_topic, "{section}");
    }

    /// 宣言に飛びがある（読んだが宣言しなかった範囲）場合、その範囲の topic は残る。
    #[test]
    fn gap_between_units_keeps_its_topic() {
        let conn = opencrab_db::init_memory().unwrap();
        seed(&conn);
        seed_topic_with_range(&conn, "tgap", "15", 200, 299);
        declare(&conn, "前半", 100, 199, "13");
        declare(&conn, "後半", 300, 399, "16");

        let section = build_memory_index_section(&conn, "a1", "current-session")
            .unwrap()
            .unwrap();
        assert!(
            section.contains("[tgap]"),
            "topic in an undeclared gap must remain: {section}"
        );
    }

    /// 件数上限: 新しい方が残り、古い方が畳まれる。表示は古い→新しい。
    /// **この向きが逆になったら落ちる**テスト（切り詰めの向きの固定）。
    #[test]
    fn unit_cap_keeps_newest_and_renders_oldest_first() {
        let conn = opencrab_db::init_memory().unwrap();
        seed(&conn);
        let n = MEMORY_INDEX_MAX_UNITS + 5;
        for i in 1..=n {
            // start_log_id が大きいほど新しい。i=1 が最古、i=n が最新。
            declare(
                &conn,
                &format!("宣言{i}"),
                i as i64 * 10,
                i as i64 * 10 + 5,
                "13",
            );
        }

        let section = build_memory_index_section(&conn, "a1", "current-session")
            .unwrap()
            .unwrap();
        // 最新 MEMORY_INDEX_MAX_UNITS 件だけが残る（新しい方が生き残る）
        assert!(
            section.contains(&format!("宣言{n}")),
            "newest unit must survive: {section}"
        );
        assert!(
            !section.contains("宣言1 "),
            "oldest unit must be dropped: {section}"
        );
        let rendered = section.lines().filter(|l| l.starts_with("- [u")).count();
        assert_eq!(rendered, MEMORY_INDEX_MAX_UNITS);
        // 溢れたぶんは件数だけ知らせる
        assert!(
            section.contains("…and 5 older declared memories (browse_memory_index)"),
            "{section}"
        );
        // 表示は古い→新しい（月行・topic 行と同じ）
        let pos_old = section
            .find(&format!("宣言{}", n - MEMORY_INDEX_MAX_UNITS + 1))
            .unwrap();
        let pos_new = section.find(&format!("宣言{n}")).unwrap();
        assert!(
            pos_old < pos_new,
            "units must render oldest-first: {section}"
        );
        // 畳み行は最も古い側 = ブロック先頭
        let pos_fold = section.find("older declared memories").unwrap();
        assert!(pos_fold < pos_old, "{section}");
    }

    /// 長すぎるタイトルが続いても文字予算でセクション全体の上限を超えない。
    #[test]
    fn unit_block_respects_char_budget() {
        let conn = opencrab_db::init_memory().unwrap();
        seed(&conn);
        for i in 1..=MEMORY_INDEX_MAX_UNITS {
            declare(
                &conn,
                &format!("宣言{i}-{}", "長".repeat(200)),
                i as i64 * 10,
                i as i64 * 10 + 5,
                "13",
            );
        }
        let section = build_memory_index_section(&conn, "a1", "current-session")
            .unwrap()
            .unwrap();
        assert!(section.chars().count() <= MEMORY_INDEX_MAX_CHARS);
        let rendered = section.lines().filter(|l| l.starts_with("- [u")).count();
        assert!(
            rendered < MEMORY_INDEX_MAX_UNITS,
            "char budget must bind before the count cap here"
        );
        // 落ちるのは古い側
        assert!(section.contains(&format!("宣言{MEMORY_INDEX_MAX_UNITS}-")));
        assert!(!section.contains("宣言1-"));
    }

    /// period が 1 つも無くてもユニットだけで記憶を見せる。
    #[test]
    fn units_alone_render_without_periods() {
        let conn = opencrab_db::init_memory().unwrap();
        declare(&conn, "ユニットだけ", 1, 2, "13");
        let section = build_memory_index_section(&conn, "a1", "s")
            .unwrap()
            .unwrap();
        assert!(section.contains("[u1] 06-13 ユニットだけ"), "{section}");
        assert!(!section.contains("Months:"));
    }
}
