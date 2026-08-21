//! メモリインデックスのクエリ層（#518 で区画ごとにサブモジュールへ分割）。
//!
//! 区画: 階層ツリー(`tree`) / 月次ロールアップ(`rollup`) / エージェント別設定(`settings`) /
//! ノード検索(`search`) / カテゴリ層(`category`) / タグ操作(`tags`) / 宣言ユニット(`units`) /
//! 凝縮コア(`cores`)。公開パスは従来どおり `pub use` で `queries::*` へフラット化される
//! （分割前と同じ名前で解決する）。

#[allow(unused_imports)]
use super::*;

mod category;
mod cores;
mod rollup;
mod search;
mod settings;
mod tags;
mod tree;
mod units;

pub use category::*;
pub use cores::*;
pub use rollup::*;
pub use search::*;
pub use settings::*;
pub use tags::*;
pub use tree::*;
pub use units::*;

#[cfg(test)]
mod undeclared_topic_tests {
    //! #410: `list_undeclared_topic_nodes_for_month` の O(topic×unit) 相関 NOT EXISTS を
    //! 「先に unit の MAX(end_log_id) を 1 回引いて OR で短絡する」形に書き換えた。
    //! ここで固定するのは **結果集合が旧クエリと 1 行も変わらないこと**（最適化は
    //! 走査量だけを減らし、返す行は不変）。OR 短絡で分岐が増えたので、左辺で早期確定した
    //! topic（短絡した側）と、左辺が偽で NOT EXISTS まで回った topic（回った側）の両方を通す。
    use super::*;
    use rusqlite::{params, Connection};

    const MONTH: &str = "2026-01";

    fn topic(
        id: &str,
        start: Option<i64>,
        end: Option<i64>,
        sess: Option<&str>,
        created: &str,
    ) -> IndexNodeRow {
        IndexNodeRow {
            id: id.to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: "session_log".to_string(),
            title: id.to_string(),
            summary: "s".to_string(),
            start_log_id: start,
            end_log_id: end,
            source_session_id: sess.map(str::to_string),
            date_from: Some("2026-01-15".to_string()),
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: created.to_string(),
            updated_at: created.to_string(),
            short_id: None,
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        }
    }

    fn unit(id: &str, start: Option<i64>, end: Option<i64>) -> IndexNodeRow {
        IndexNodeRow {
            id: id.to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "unit".to_string(),
            source_type: "session_log".to_string(),
            title: id.to_string(),
            summary: "s".to_string(),
            start_log_id: start,
            end_log_id: end,
            source_session_id: None,
            date_from: Some("2026-01-15".to_string()),
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: None,
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        }
    }

    /// 書き換え前の素の相関 NOT EXISTS 版（黄金の実装）。本番関数の結果はこれと
    /// 全ケース一致しなければならない。**このクエリは変更してはいけない**（比較基準）。
    fn naive_undeclared(
        conn: &Connection,
        agent_id: &str,
        month_prefix: &str,
        exclude_session_id: &str,
        limit: usize,
    ) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT t.id FROM memory_index_nodes t
                 WHERE t.agent_id = ?1 AND t.node_type = 'topic' AND t.source_type = 'session_log'
                   AND t.date_from LIKE ?2 || '%'
                   AND (t.source_session_id IS NULL OR t.source_session_id != ?3)
                   AND NOT EXISTS (
                     SELECT 1 FROM memory_index_nodes u
                     WHERE u.agent_id = t.agent_id AND u.node_type = 'unit'
                       AND u.start_log_id <= t.end_log_id AND u.end_log_id >= t.start_log_id
                   )
                 ORDER BY t.created_at DESC LIMIT ?4",
            )
            .unwrap();
        let rows = stmt
            .query_map(
                params![agent_id, month_prefix, exclude_session_id, limit as i64],
                |r| r.get::<_, String>(0),
            )
            .unwrap();
        rows.collect::<std::result::Result<_, _>>().unwrap()
    }

    fn prod_ids(conn: &Connection) -> Vec<String> {
        list_undeclared_topic_nodes_for_month(conn, "a1", MONTH, "sess-excluded", 100)
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect()
    }

    /// 各シナリオのノードを流し込み、本番クエリと素の NOT EXISTS 版が **同じ id 列**を
    /// 返すことを確認する。境界ケース（topic 0 件 / unit 0 件 / end_log_id NULL / 全 topic が
    /// max_end より後）を 1 つずつ独立した DB で回す。
    #[test]
    fn rewrite_is_equivalent_to_naive_not_exists() {
        // (name, topics, units)
        let scenarios: Vec<(&str, Vec<IndexNodeRow>, Vec<IndexNodeRow>)> = vec![
            // 1) topic が 1 件も無い → 両方空
            ("no topics", vec![], vec![unit("u1", Some(1), Some(10))]),
            // 2) unit が 1 件も無い → MAX(end) が NULL。全 topic が未宣言（左辺は NULL で
            //    偽、NOT EXISTS が真）。両方とも全件返す。
            (
                "no units",
                vec![
                    topic("t-a", Some(1), Some(5), None, "2026-01-02T00:00:00Z"),
                    topic("t-b", Some(6), Some(9), None, "2026-01-03T00:00:00Z"),
                ],
                vec![],
            ),
            // 3) 全 topic が全 unit の end より後 → 左辺（短絡側）で全件確定。
            (
                "all topics after max_end (short-circuit side)",
                vec![
                    topic("t-a", Some(100), Some(110), None, "2026-01-02T00:00:00Z"),
                    topic("t-b", Some(111), Some(120), None, "2026-01-03T00:00:00Z"),
                ],
                vec![
                    unit("u1", Some(1), Some(50)),
                    unit("u2", Some(51), Some(99)),
                ],
            ),
            // 4) 混在: 覆われた topic は除外、隙間の topic は残る（NOT EXISTS まで回る側）、
            //    max_end より後の topic は短絡側で残る。
            (
                "mixed: covered / gap / after",
                vec![
                    topic("t-covered", Some(2), Some(4), None, "2026-01-02T00:00:00Z"),
                    topic(
                        "t-boundary",
                        Some(50),
                        Some(50),
                        None,
                        "2026-01-03T00:00:00Z",
                    ),
                    topic("t-gap", Some(31), Some(39), None, "2026-01-04T00:00:00Z"),
                    topic(
                        "t-after",
                        Some(200),
                        Some(210),
                        None,
                        "2026-01-05T00:00:00Z",
                    ),
                    // start_log_id == MAX(end)=99。u2 の端に重なるので naive では **除外**。
                    // 左辺は `> `（`>=` ではない）ので短絡せず NOT EXISTS で除外に落ちる。
                    // これで `>` → `>=` への変異を殺す。
                    topic(
                        "t-at-max",
                        Some(99),
                        Some(105),
                        None,
                        "2026-01-06T00:00:00Z",
                    ),
                ],
                vec![
                    unit("u1", Some(1), Some(30)),
                    unit("u2", Some(40), Some(99)),
                ],
            ),
            // 5) topic の end_log_id が NULL → overlap 比較が NULL になり NOT EXISTS 真。
            //    左辺の start>MAX は真になり得る（start が MAX 超のとき短絡）／偽のとき回る。
            (
                "topic end_log_id NULL",
                vec![
                    topic("t-nullend-lo", Some(5), None, None, "2026-01-02T00:00:00Z"),
                    topic(
                        "t-nullend-hi",
                        Some(500),
                        None,
                        None,
                        "2026-01-03T00:00:00Z",
                    ),
                    topic("t-nullstart", None, Some(5), None, "2026-01-04T00:00:00Z"),
                    topic("t-nullboth", None, None, None, "2026-01-05T00:00:00Z"),
                ],
                vec![unit("u1", Some(1), Some(100))],
            ),
            // 6) unit 側に NULL 端 → MAX は非 NULL 端だけ拾う。NULL 端 unit は overlap で
            //    決してマッチしない（両クエリで同じく無視される）。
            (
                "unit has NULL endpoints",
                vec![
                    topic("t-a", Some(2), Some(4), None, "2026-01-02T00:00:00Z"),
                    topic("t-b", Some(60), Some(70), None, "2026-01-03T00:00:00Z"),
                    topic("t-c", Some(200), Some(210), None, "2026-01-04T00:00:00Z"),
                ],
                vec![
                    unit("u-nullend", Some(1), None),
                    unit("u-nullstart", None, Some(500)),
                    unit("u-real", Some(1), Some(50)),
                ],
            ),
        ];

        for (name, topics, units) in scenarios {
            let conn = crate::init_memory().unwrap();
            for t in &topics {
                insert_index_node(&conn, t).unwrap();
            }
            for u in &units {
                insert_index_node(&conn, u).unwrap();
            }
            let got = prod_ids(&conn);
            let want = naive_undeclared(&conn, "a1", MONTH, "sess-excluded", 100);
            assert_eq!(got, want, "scenario `{name}`: prod != naive");
        }
    }

    /// 短絡した側（左辺 `start_log_id > MAX(end)` が真）を明示的に通す: unit があっても
    /// それより新しい topic は必ず返る。
    #[test]
    fn short_circuit_side_includes_topics_newer_than_all_units() {
        let conn = crate::init_memory().unwrap();
        insert_index_node(&conn, &unit("u1", Some(1), Some(100))).unwrap();
        insert_index_node(
            &conn,
            &topic("t-new", Some(101), Some(120), None, "2026-01-02T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(prod_ids(&conn), vec!["t-new".to_string()]);
    }

    /// 回った側（左辺が偽で NOT EXISTS を評価）を明示的に通す: max_end 以下の範囲で、
    /// 覆われた topic は除外・隙間の topic は残る。
    #[test]
    fn correlated_side_excludes_covered_keeps_gap() {
        let conn = crate::init_memory().unwrap();
        insert_index_node(&conn, &unit("u1", Some(1), Some(30))).unwrap();
        insert_index_node(&conn, &unit("u2", Some(40), Some(100))).unwrap();
        insert_index_node(
            &conn,
            &topic("t-covered", Some(5), Some(10), None, "2026-01-02T00:00:00Z"),
        )
        .unwrap();
        insert_index_node(
            &conn,
            &topic("t-gap", Some(31), Some(39), None, "2026-01-03T00:00:00Z"),
        )
        .unwrap();
        // どちらも start_log_id(5,31) <= MAX(end)=100 なので左辺は偽 → NOT EXISTS まで回る。
        assert_eq!(prod_ids(&conn), vec!["t-gap".to_string()]);
    }

    /// unit が 0 件（MAX(end) = NULL）でも全 topic が返る（早期 return しない）。
    #[test]
    fn no_units_returns_all_topics() {
        let conn = crate::init_memory().unwrap();
        insert_index_node(
            &conn,
            &topic("t-a", Some(1), Some(5), None, "2026-01-02T00:00:00Z"),
        )
        .unwrap();
        insert_index_node(
            &conn,
            &topic("t-b", Some(6), Some(9), None, "2026-01-03T00:00:00Z"),
        )
        .unwrap();
        // created_at DESC 順
        assert_eq!(prod_ids(&conn), vec!["t-b".to_string(), "t-a".to_string()]);
    }

    /// topic が 0 件なら空。
    #[test]
    fn no_topics_returns_empty() {
        let conn = crate::init_memory().unwrap();
        insert_index_node(&conn, &unit("u1", Some(1), Some(10))).unwrap();
        assert!(prod_ids(&conn).is_empty());
    }

    /// #410 の効果計測用（既定では走らせない）。本番規模のシード DB を作り、旧
    /// （素の相関 NOT EXISTS）と新（MAX 短絡）の実測と EXPLAIN QUERY PLAN を印字する。
    /// 実行: `cargo test -p opencrab-db bench_undeclared -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_undeclared_old_vs_new() {
        use std::time::Instant;

        let conn = crate::init_memory().unwrap();
        // 本番 memory_index_nodes は約 8,400 行。ここでは同月・session_log の topic を
        // 主体に据え、宣言済み unit が古い側を覆い、新しい topic は未宣言（start > max_end）
        // という実運用の分布を作る。
        let n_units: i64 = 400; // 宣言ユニット（古い側を覆う）
        let n_topics_declared: i64 = 4000; // max_end 以下（覆われ or 隙間 → NOT EXISTS を通る）
        let n_topics_new: i64 = 4000; // max_end より後（短絡側で早期確定）

        // unit: log id 1..=(n_units*10) を 10 幅で連続に覆う（max_end = n_units*10）。
        let max_declared = n_units * 10;
        for i in 0..n_units {
            let s = i * 10 + 1;
            let e = s + 9;
            insert_index_node(&conn, &unit(&format!("u{i}"), Some(s), Some(e))).unwrap();
        }
        // 宣言域内の topic（覆われた / 隙間まちまち）。
        for i in 0..n_topics_declared {
            let s = i % max_declared + 1;
            let e = s + 3;
            insert_index_node(
                &conn,
                &topic(
                    &format!("td{i}"),
                    Some(s),
                    Some(e),
                    None,
                    &format!("2026-01-10T00:{:02}:{:02}Z", (i / 60) % 60, i % 60),
                ),
            )
            .unwrap();
        }
        // 宣言域より後（未宣言）の topic。
        for i in 0..n_topics_new {
            let s = max_declared + i * 5 + 1;
            let e = s + 4;
            insert_index_node(
                &conn,
                &topic(
                    &format!("tn{i}"),
                    Some(s),
                    Some(e),
                    None,
                    &format!("2026-01-20T00:{:02}:{:02}Z", (i / 60) % 60, i % 60),
                ),
            )
            .unwrap();
        }

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_index_nodes", [], |r| r.get(0))
            .unwrap();
        eprintln!(
            "--- seed: total rows={total}, units={n_units}, topics(declared)={n_topics_declared}, topics(new)={n_topics_new}, max_end={max_declared}"
        );

        // 結果が一致することも同時に確認（規模ありでの同値）。
        let got = prod_ids(&conn);
        let want = naive_undeclared(&conn, "a1", MONTH, "sess-excluded", 100);
        assert_eq!(got.len(), want.len(), "bench: prod/naive 件数不一致");
        assert_eq!(got, want, "bench: prod/naive 集合不一致");
        eprintln!("--- result rows (both) = {}", got.len());

        let new_sql = "SELECT t.id FROM memory_index_nodes t
             WHERE t.agent_id = ?1 AND t.node_type = 'topic' AND t.source_type = 'session_log'
               AND t.date_from LIKE ?2 || '%'
               AND (t.source_session_id IS NULL OR t.source_session_id != ?3)
               AND (
                 t.start_log_id > (SELECT MAX(u.end_log_id) FROM memory_index_nodes u
                                   WHERE u.agent_id = ?1 AND u.node_type = 'unit')
                 OR NOT EXISTS (SELECT 1 FROM memory_index_nodes u
                   WHERE u.agent_id = ?1 AND u.node_type = 'unit'
                     AND u.start_log_id <= t.end_log_id AND u.end_log_id >= t.start_log_id)
               )
             ORDER BY t.created_at DESC LIMIT ?4";
        let old_sql = "SELECT t.id FROM memory_index_nodes t
             WHERE t.agent_id = ?1 AND t.node_type = 'topic' AND t.source_type = 'session_log'
               AND t.date_from LIKE ?2 || '%'
               AND (t.source_session_id IS NULL OR t.source_session_id != ?3)
               AND NOT EXISTS (SELECT 1 FROM memory_index_nodes u
                 WHERE u.agent_id = t.agent_id AND u.node_type = 'unit'
                   AND u.start_log_id <= t.end_log_id AND u.end_log_id >= t.start_log_id)
             ORDER BY t.created_at DESC LIMIT ?4";

        let explain = |label: &str, sql: &str| {
            let eqp = format!("EXPLAIN QUERY PLAN {sql}");
            let mut stmt = conn.prepare(&eqp).unwrap();
            let rows: Vec<String> = stmt
                .query_map(params!["a1", MONTH, "sess-excluded", 100i64], |r| {
                    r.get::<_, String>(3)
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            eprintln!("--- EXPLAIN QUERY PLAN [{label}]");
            for line in rows {
                eprintln!("    {line}");
            }
        };
        explain("OLD (correlated NOT EXISTS)", old_sql);
        explain("NEW (MAX short-circuit + NOT EXISTS)", new_sql);

        let run = |sql: &str| -> u128 {
            let mut stmt = conn.prepare(sql).unwrap();
            let iters = 20;
            let start = Instant::now();
            for _ in 0..iters {
                let rows = stmt
                    .query_map(params!["a1", MONTH, "sess-excluded", 100i64], |r| {
                        r.get::<_, String>(0)
                    })
                    .unwrap();
                let _: Vec<String> = rows.map(|r| r.unwrap()).collect();
            }
            start.elapsed().as_micros() / iters
        };
        // ウォームアップ
        run(old_sql);
        run(new_sql);
        let old_us = run(old_sql);
        let new_us = run(new_sql);
        eprintln!("--- timing (avg over 20 iters): OLD={old_us}us  NEW={new_us}us");
    }
}
