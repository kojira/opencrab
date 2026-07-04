//! メモリインデックスのアイドル時メンテナンス（キーワードバックフィル / 月次ロールアップ）。
//!
//! server の memory_maintenance ループから呼ばれる。各関数は 1 回の呼び出しで
//! LLM を最大 1 コールしか使わない（予算制御は呼び出し側の tick 設計に依存しない
//! 構造で担保する）。DB ガードは LLM 呼び出し（await）を跨いで保持しない。

use anyhow::Result;

use crate::engine::LlmClient;
use opencrab_llm_types::{ChatRequest, Message};

/// 1 バッチでキーワードを付け直す topic ノードの上限。
pub const KEYWORD_BACKFILL_BATCH: usize = 10;

/// LLM が返すバックフィル結果: short_id → keywords。
type BackfillMap = std::collections::HashMap<String, Vec<String>>;

/// keywords 未付与（`keywords_json = '[]'`）の topic ノードに、title/summary から
/// キーワードを一括抽出して付与する。処理した件数を返す（対象なしなら 0、LLM ゼロコール）。
///
/// LLM 応答に含まれなかったノードには title 由来のフォールバックを書き込む
/// （`[]` のまま残すと毎 tick 再抽出対象になり永久ループするため、必ず前進する）。
pub async fn backfill_topic_keywords(
    conn: &opencrab_db::Db,
    agent_id: &str,
    llm: &dyn LlmClient,
    model: &str,
) -> Result<usize> {
    let targets = {
        let db = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
        opencrab_db::queries::list_topics_missing_keywords(&db, agent_id, KEYWORD_BACKFILL_BATCH)?
    };
    if targets.is_empty() {
        return Ok(0);
    }

    let listing: String = targets
        .iter()
        .map(|t| {
            let sid = t.short_id.as_deref().unwrap_or(&t.id);
            format!("{sid} | {} | {}", t.title, t.summary)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "以下は過去の記憶トピックの一覧です（形式: id | タイトル | 要約）。\n\
         各トピックに検索用キーワードを 3〜8 個付けてください（人物・技術・固有名詞を優先）。\n\
         JSON形式で出力: {{\"<id>\": [\"kw1\", \"kw2\"], ...}}\n\n{listing}"
    );
    let request = ChatRequest::new(
        model.to_string(),
        vec![
            Message::system("You are a helpful assistant.".to_string()),
            Message::user(prompt),
        ],
    )
    .with_temperature(0.0)
    .with_max_tokens(1024);

    let extracted: BackfillMap = match llm.chat(request).await {
        Ok(resp) => {
            let text = resp.first_text().unwrap_or_default().to_string();
            serde_json::from_str(crate::llm_text::strip_code_fences(&text)).unwrap_or_default()
        }
        Err(e) => {
            // 障害時は何も書かずに次 tick で再試行する（ここでフォールバックを
            // 書き込むと、LLM 停止中の tick ごとに最大 10 ノードへタイトル由来の
            // 低品質キーワードが恒久確定してしまう）。
            tracing::warn!(agent_id = %agent_id, error = %e, "keyword backfill LLM call failed; will retry next tick");
            return Ok(0);
        }
    };
    if extracted.is_empty() {
        // パース不能な応答も障害扱い（フォールバックは「成功応答から個別に漏れた
        // ノード」にのみ適用する）。
        tracing::warn!(agent_id = %agent_id, "keyword backfill response unparsable; will retry next tick");
        return Ok(0);
    }

    let db = conn
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
    let mut updated = 0usize;
    for t in &targets {
        let sid = t.short_id.as_deref().unwrap_or(&t.id);
        let keywords = extracted
            .get(sid)
            .cloned()
            .filter(|v| !v.is_empty())
            // LLM 応答から漏れた分は title フォールバック（前進保証）
            .unwrap_or_else(|| vec![t.title.clone()]);
        let normalized: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            keywords
                .into_iter()
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty() && seen.insert(k.clone()))
                .take(8)
                .collect()
        };
        let keywords_json =
            serde_json::to_string(&normalized).unwrap_or_else(|_| format!("[{:?}]", t.title));
        opencrab_db::queries::update_index_node_keywords(&db, &t.id, &keywords_json)?;
        updated += 1;
    }
    Ok(updated)
}

/// LLM から返る月次ロールアップ JSON。
#[derive(Debug, serde::Deserialize)]
struct RollupSummary {
    summary: String,
    #[serde(default)]
    keywords: Vec<String>,
}

/// ロールアップ対象月に含める topic 数 / 文字数の上限（プロンプト肥大防止）。
const ROLLUP_MAX_TOPICS: usize = 60;
const ROLLUP_MAX_CHARS: usize = 8000;

/// stale な過去月の period ノードを 1 つロールアップする（1 LLM コール）。
/// ロールアップした月のタイトル（`YYYY-MM`）を返す。対象がなければ None（ゼロコール）。
///
/// 生成した月次要約は period ノードの summary に書かれ、会話へ常時注入される
/// [Memory Index] セクションの月行として表示される（保存するだけの要約にしない —
/// これがこの機能の中心要件）。
pub async fn rollup_stale_period(
    conn: &opencrab_db::Db,
    agent_id: &str,
    llm: &dyn LlmClient,
    model: &str,
    persona_name: &str,
    personality: Option<&str>,
) -> Result<Option<String>> {
    let (period, topics) = {
        let db = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
        let Some(period) = opencrab_db::queries::find_stale_period(&db, agent_id)? else {
            return Ok(None);
        };
        let topics = opencrab_db::queries::list_topics_for_period(&db, agent_id, &period.id)?;
        (period, topics)
    };
    if topics.is_empty() {
        // find_stale_period は topic の存在を条件にしているため通常到達しない
        return Ok(None);
    }

    let mut listing = String::new();
    for t in topics.iter().take(ROLLUP_MAX_TOPICS) {
        let line = format!("- {}: {}\n", t.title, t.summary);
        if listing.len() + line.len() > ROLLUP_MAX_CHARS {
            break;
        }
        listing.push_str(&line);
    }

    let persona_header = match personality.filter(|p| !p.is_empty()) {
        Some(p) => format!("あなたは {persona_name} です。\n{p}\n\n"),
        None => String::new(),
    };
    let month = &period.title;
    let prompt = format!(
        "{persona_header}以下は {month} のあなたの記憶（トピック要約の一覧）です。\n\
         この月に何があったかを一人称の記憶として300字以内でまとめてください。\n\
         重要な学び・出来事・関係性・失敗と教訓を優先してください。\n\
         JSON形式で出力: {{\"summary\": \"300字以内\", \"keywords\": [\"3〜8個\"]}}\n\n\
         トピック一覧:\n{listing}"
    );
    let system_content = match personality.filter(|p| !p.is_empty()) {
        Some(p) => format!("あなたは {persona_name} です。\n{p}"),
        None => "You are a helpful assistant.".to_string(),
    };
    let request = ChatRequest::new(
        model.to_string(),
        vec![Message::system(system_content), Message::user(prompt)],
    )
    .with_temperature(0.0)
    .with_max_tokens(400);

    let rollup = match llm.chat(request).await {
        Ok(resp) => {
            let text = resp.first_text().unwrap_or_default().to_string();
            match serde_json::from_str::<RollupSummary>(crate::llm_text::strip_code_fences(&text)) {
                Ok(r) if !r.summary.trim().is_empty() => r,
                _ => {
                    // 失敗は skip + warn（summary_refreshed_at を刻まないので次 tick で再試行）
                    tracing::warn!(agent_id = %agent_id, month = %month, "rollup response parse failed; will retry next tick");
                    return Ok(None);
                }
            }
        }
        Err(e) => {
            tracing::warn!(agent_id = %agent_id, month = %month, error = %e, "rollup LLM call failed; will retry next tick");
            return Ok(None);
        }
    };

    let keywords: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        rollup
            .keywords
            .into_iter()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty() && seen.insert(k.clone()))
            .take(8)
            .collect()
    };
    let keywords_json = serde_json::to_string(&keywords).unwrap_or_else(|_| "[]".to_string());

    {
        let db = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
        opencrab_db::queries::update_period_rollup(
            &db,
            &period.id,
            rollup.summary.trim(),
            &keywords_json,
        )?;
    }
    tracing::info!(agent_id = %agent_id, month = %month, "monthly rollup written");
    Ok(Some(period.title.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{ChatResponse, LlmClient};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingLlm {
        calls: AtomicUsize,
        response: String,
    }

    #[async_trait]
    impl LlmClient for CountingLlm {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse::text(self.response.clone()))
        }
    }

    struct FailingLlm;

    #[async_trait]
    impl LlmClient for FailingLlm {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Err(anyhow::anyhow!("provider down"))
        }
    }

    fn mk_node(
        id: &str,
        node_type: &str,
        parent: Option<&str>,
        title: &str,
        created_at: &str,
    ) -> opencrab_db::queries::IndexNodeRow {
        opencrab_db::queries::IndexNodeRow {
            id: id.to_string(),
            agent_id: "a1".to_string(),
            parent_id: parent.map(String::from),
            node_type: node_type.to_string(),
            source_type: "session_log".to_string(),
            title: title.to_string(),
            summary: format!("{title} の要約"),
            start_log_id: None,
            end_log_id: None,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
            short_id: Some(id.to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        }
    }

    /// root → period(2020-05) → session → topic ×2 のツリーを作る。
    fn seed_past_month(conn: &opencrab_db::Db) {
        let db = conn.lock().unwrap();
        opencrab_db::queries::insert_index_node(
            &db,
            &mk_node("r1", "root", None, "root", "2020-05-01T00:00:00Z"),
        )
        .unwrap();
        opencrab_db::queries::insert_index_node(
            &db,
            &mk_node(
                "p1",
                "period",
                Some("r1"),
                "2020-05",
                "2020-05-01T00:00:00Z",
            ),
        )
        .unwrap();
        opencrab_db::queries::insert_index_node(
            &db,
            &mk_node(
                "s1",
                "session",
                Some("p1"),
                "Session",
                "2020-05-01T00:00:00Z",
            ),
        )
        .unwrap();
        opencrab_db::queries::insert_index_node(
            &db,
            &mk_node(
                "t1",
                "topic",
                Some("s1"),
                "Rust入門",
                "2020-05-02T00:00:00Z",
            ),
        )
        .unwrap();
        opencrab_db::queries::insert_index_node(
            &db,
            &mk_node(
                "t2",
                "topic",
                Some("s1"),
                "Discord連携",
                "2020-05-03T00:00:00Z",
            ),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn rollup_writes_summary_and_second_tick_is_noop() {
        let conn = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        seed_past_month(&conn);
        let llm = CountingLlm {
            calls: AtomicUsize::new(0),
            response: r#"{"summary": "5月はRustとDiscord連携を学んだ月だった。", "keywords": ["Rust", "Discord"]}"#.to_string(),
        };

        let done = rollup_stale_period(&conn, "a1", &llm, "m", "テスト", None)
            .await
            .unwrap();
        assert_eq!(done.as_deref(), Some("2020-05"));
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
        {
            let db = conn.lock().unwrap();
            let period = opencrab_db::queries::get_index_node(&db, "p1")
                .unwrap()
                .unwrap();
            assert_eq!(period.summary, "5月はRustとDiscord連携を学んだ月だった。");
            assert!(period.summary_refreshed_at.is_some());
            assert_eq!(period.keywords_json, r#"["Rust","Discord"]"#);
            // FTS からも月要約で引ける
            let hits = opencrab_db::queries::search_index_nodes(
                &db,
                "a1",
                "Discord連携を学んだ",
                10,
                Some("period"),
            )
            .unwrap();
            assert_eq!(hits.len(), 1);
        }

        // 2 tick 目: stale な月が無いので LLM ゼロコール
        let done = rollup_stale_period(&conn, "a1", &llm, "m", "テスト", None)
            .await
            .unwrap();
        assert!(done.is_none());
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rollup_parse_failure_leaves_period_stale_for_retry() {
        let conn = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        seed_past_month(&conn);
        let llm = CountingLlm {
            calls: AtomicUsize::new(0),
            response: "not json".to_string(),
        };
        let done = rollup_stale_period(&conn, "a1", &llm, "m", "テスト", None)
            .await
            .unwrap();
        assert!(done.is_none());
        // summary_refreshed_at が刻まれていない = 次 tick で再試行される
        let db = conn.lock().unwrap();
        let period = opencrab_db::queries::get_index_node(&db, "p1")
            .unwrap()
            .unwrap();
        assert!(period.summary_refreshed_at.is_none());
    }

    #[tokio::test]
    async fn backfill_updates_targets_and_falls_back_for_missing() {
        let conn = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        seed_past_month(&conn);
        // LLM は t1 にだけキーワードを返す → t2 は title フォールバック
        let llm = CountingLlm {
            calls: AtomicUsize::new(0),
            response: r#"{"t1": ["Rust", "所有権"]}"#.to_string(),
        };
        let updated = backfill_topic_keywords(&conn, "a1", &llm, "m")
            .await
            .unwrap();
        assert_eq!(updated, 2);
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
        {
            let db = conn.lock().unwrap();
            let t1 = opencrab_db::queries::get_index_node(&db, "t1")
                .unwrap()
                .unwrap();
            assert_eq!(t1.keywords_json, r#"["Rust","所有権"]"#);
            let t2 = opencrab_db::queries::get_index_node(&db, "t2")
                .unwrap()
                .unwrap();
            assert_eq!(t2.keywords_json, r#"["Discord連携"]"#);
        }
        // 2 回目: 対象なし → LLM ゼロコール（フォールバックにより前進保証）
        let updated = backfill_topic_keywords(&conn, "a1", &llm, "m")
            .await
            .unwrap();
        assert_eq!(updated, 0);
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn backfill_outage_stamps_nothing_and_retries() {
        // LLM 障害時にタイトル由来フォールバックを恒久確定させない（次 tick 再試行）
        let conn = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        seed_past_month(&conn);
        let updated = backfill_topic_keywords(&conn, "a1", &FailingLlm, "m")
            .await
            .unwrap();
        assert_eq!(updated, 0);
        {
            let db = conn.lock().unwrap();
            let t1 = opencrab_db::queries::get_index_node(&db, "t1")
                .unwrap()
                .unwrap();
            assert_eq!(t1.keywords_json, "[]");
            // 対象リストに残っている = 次 tick で再試行される
            assert_eq!(
                opencrab_db::queries::list_topics_missing_keywords(&db, "a1", 10)
                    .unwrap()
                    .len(),
                2
            );
        }
        // パース不能応答も同様（何も確定しない）
        let garbage = CountingLlm {
            calls: AtomicUsize::new(0),
            response: "not json".to_string(),
        };
        let updated = backfill_topic_keywords(&conn, "a1", &garbage, "m")
            .await
            .unwrap();
        assert_eq!(updated, 0);
        let db = conn.lock().unwrap();
        assert_eq!(
            opencrab_db::queries::list_topics_missing_keywords(&db, "a1", 10)
                .unwrap()
                .len(),
            2
        );
    }
}
