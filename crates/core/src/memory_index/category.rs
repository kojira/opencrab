//! 記憶のカテゴリ index 層のアイドル時メンテナンス（issue #313 段階1）。
//!
//! 時系列ツリー（root→period→session→topic）はそのまま残し、内容で束ねる
//! 「カテゴリ index 層」を **sleep 中に** 追記的に被せる。対話ターンでは一切走らせない
//! （#291 の再来を避ける）。server の memory_maintenance ループから 1 tick ずつ呼ばれる。
//!
//! この段階が担うのは 2 つ:
//! 1. **種まき** (`seed_categories_from_curated`): エージェントが手書きした
//!    `long_term/<名前>` の `<名前>` を初期カテゴリとして起こす（LLM ゼロコール・冪等）。
//! 2. **割当** (`assign_unassigned_topics`): まだどのカテゴリにも属さない topic を、
//!    既存カテゴリへ割り当てる（**≤1 LLM コール**・DB ロックを await 跨ぎで保持しない・
//!    sticky で冪等）。既存カテゴリ一覧を毎回見せ temperature 0.0 で引くことで割当の
//!    ドリフト（同じ内容が毎回別カテゴリへ散る）を抑える。一度付いた割当は動かさない。
//!
//! メタ index への畳み込み（トップレベルを一定数に保つ）と、注入の `Categories:` 置換は
//! 段階2/3（別 PR）。この段階では category/meta ノードは `browse/search/retrieve` から
//! 能動的に引ける状態になる（同一テーブルなので short_id / FTS がそのまま効く）。

use anyhow::Result;
use chrono::Utc;

use crate::engine::LlmClient;
use opencrab_llm_types::{ChatRequest, Message};

/// 1 tick で割り当てる未分類 topic の上限（プロンプト肥大とコスト平準化のため）。
/// キーワードバックフィル（10）と同水準。数千件のバックログはこの粒度で毎 tick 前進する。
pub const ASSIGN_BATCH: usize = 12;

/// 割当プロンプトに載せる topic 一覧の文字数上限。
const ASSIGN_LISTING_MAX_CHARS: usize = 6000;

/// LLM 応答が既存カテゴリと結び付かなかった topic の受け皿（前進保証）。
/// これが無いと LLM に毎回無視される topic がバッチ先頭に居座り無限リトライになる。
const FALLBACK_CATEGORY: &str = "未分類";

fn persona_header(persona_name: &str, personality: Option<&str>) -> String {
    match personality.filter(|p| !p.is_empty()) {
        Some(p) => format!("あなたは {persona_name} です。\n{p}\n\n"),
        None => String::new(),
    }
}

fn persona_system(persona_name: &str, personality: Option<&str>) -> String {
    match personality.filter(|p| !p.is_empty()) {
        Some(p) => format!("あなたは {persona_name} です。\n{p}"),
        None => "You are a helpful assistant.".to_string(),
    }
}

/// 種まき: curated `long_term/<名前>` を初期カテゴリノードとして起こす（LLM ゼロコール）。
///
/// 冪等: 既に同名の category/meta ノードがあれば作らない。作った件数を返す。
/// 稼働中の `upsert_curated_memory` とは競合しない（単一接続で直列化・読み取りのみ・
/// curated 行は書き換えない）。あとから増えた `long_term/<名前>` は次 tick で追記される。
pub fn seed_categories_from_curated(conn: &opencrab_db::Db, agent_id: &str) -> Result<usize> {
    let db = conn
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
    let seeds = opencrab_db::queries::list_long_term_category_seeds(&db, agent_id)?;
    if seeds.is_empty() {
        return Ok(0);
    }
    let now = Utc::now().to_rfc3339();
    let root = opencrab_db::queries::ensure_category_root(&db, agent_id, &now)?;
    let mut created = 0usize;
    for name in seeds {
        if opencrab_db::queries::get_category_node_by_title(&db, agent_id, &name)?.is_none() {
            opencrab_db::queries::insert_category_node(&db, agent_id, &root, &name, "", &now)?;
            created += 1;
        }
    }
    Ok(created)
}

/// LLM から返る割当結果: topic short_id → カテゴリ名（既存名 or "NEW:<名前>"）。
type AssignMap = std::collections::HashMap<String, String>;

/// 割当: 未分類 topic を既存カテゴリへ割り当てる（≤1 LLM コール・sticky・冪等）。
///
/// 割り当てた件数を返す。対象が無ければ 0（ゼロコール）。LLM 障害・パース不能時も 0
/// （何も割り当てず次 tick で再試行 — 低品質な割当を恒久確定させない）。
pub async fn assign_unassigned_topics(
    conn: &opencrab_db::Db,
    agent_id: &str,
    llm: &dyn LlmClient,
    model: &str,
    persona_name: &str,
    personality: Option<&str>,
) -> Result<usize> {
    // ロックを await 跨ぎで保持しない: 収集 → (unlock) → LLM → (lock) 適用。
    let (topics, categories) = {
        let db = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
        let topics = opencrab_db::queries::list_unassigned_topics(&db, agent_id, ASSIGN_BATCH)?;
        if topics.is_empty() {
            return Ok(0);
        }
        let categories = opencrab_db::queries::list_top_level_categories(&db, agent_id)?;
        (topics, categories)
    };

    let category_names: Vec<String> = categories
        .iter()
        .filter(|c| c.node_type == "category")
        .map(|c| c.title.clone())
        .collect();

    let mut listing = String::new();
    for t in &topics {
        let sid = t.short_id.as_deref().unwrap_or(&t.id);
        let line = format!("{sid} | {} | {}\n", t.title, t.summary);
        if listing.chars().count() + line.chars().count() > ASSIGN_LISTING_MAX_CHARS {
            break;
        }
        listing.push_str(&line);
    }

    let category_block = if category_names.is_empty() {
        "（まだカテゴリはありません。内容に合う簡潔なカテゴリ名を新設してください）".to_string()
    } else {
        category_names
            .iter()
            .map(|n| format!("- {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let prompt = format!(
        "{header}以下はあなたの過去の記憶トピックです（形式: id | タイトル | 要約）。\n\
         それぞれを、あなたの記憶の「カテゴリ」に振り分けてください。\n\n\
         既存のカテゴリ:\n{categories}\n\n\
         ルール:\n\
         - **できるだけ既存のカテゴリを選ぶ**（同じ内容が毎回違うカテゴリに散らばらないように）。\n\
         - どうしても合う既存カテゴリが無いときだけ、新しいカテゴリを \"NEW:<簡潔な名前>\" の形で提案する。\n\
         - 迷ったら既存の近いカテゴリに寄せる。新設は最小限に。\n\n\
         JSON形式で、topic の id をキーに出力してください:\n\
         {{\"<id>\": \"<既存カテゴリ名 または NEW:新カテゴリ名>\", ...}}\n\n\
         トピック一覧:\n{listing}",
        header = persona_header(persona_name, personality),
        categories = category_block,
    );

    let request = ChatRequest::new(
        model.to_string(),
        vec![
            Message::system(persona_system(persona_name, personality)),
            Message::user(prompt),
        ],
    )
    .with_temperature(0.0)
    .with_max_tokens(1024);

    let assign: AssignMap = match llm.chat(request).await {
        Ok(resp) => {
            let text = resp.first_text().unwrap_or_default().to_string();
            serde_json::from_str(crate::llm_text::strip_code_fences(&text)).unwrap_or_default()
        }
        Err(e) => {
            tracing::warn!(agent_id = %agent_id, error = %e, "category assignment LLM call failed; will retry next tick");
            return Ok(0);
        }
    };
    if assign.is_empty() {
        tracing::warn!(agent_id = %agent_id, "category assignment response unparsable; will retry next tick");
        return Ok(0);
    }

    // 適用（ロック再取得・await を跨がない）。
    let db = conn
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
    let now = Utc::now().to_rfc3339();
    let root = opencrab_db::queries::ensure_category_root(&db, agent_id, &now)?;

    // タイトル→category id の解決キャッシュ（同 tick 内の新設重複を防ぐ）。
    let mut title_to_id: std::collections::HashMap<String, String> = categories
        .iter()
        .filter(|c| c.node_type == "category")
        .map(|c| (c.title.clone(), c.id.clone()))
        .collect();

    // 既存カテゴリ名の正規化マップ（LLM の表記揺れを既存へ寄せる: 前後空白/大小無視）。
    let norm = |s: &str| s.trim().to_lowercase();
    let mut normkey_to_title: std::collections::HashMap<String, String> =
        title_to_id.keys().map(|t| (norm(t), t.clone())).collect();

    // カテゴリ名 → id を解決（無ければ作成）。
    let resolve = |db: &rusqlite::Connection,
                   title_to_id: &mut std::collections::HashMap<String, String>,
                   normkey_to_title: &mut std::collections::HashMap<String, String>,
                   raw: &str|
     -> Result<Option<String>> {
        let name = raw.trim();
        if name.is_empty() {
            return Ok(None);
        }
        // 既存へ寄せる（正規化一致）。
        if let Some(existing_title) = normkey_to_title.get(&norm(name)) {
            return Ok(title_to_id.get(existing_title).cloned());
        }
        // 新設（DB 側の同名も再確認してから作る）。
        if let Some(node) = opencrab_db::queries::get_category_node_by_title(db, agent_id, name)? {
            title_to_id.insert(name.to_string(), node.id.clone());
            normkey_to_title.insert(norm(name), name.to_string());
            return Ok(Some(node.id));
        }
        let node = opencrab_db::queries::insert_category_node(db, agent_id, &root, name, "", &now)?;
        title_to_id.insert(name.to_string(), node.id.clone());
        normkey_to_title.insert(norm(name), name.to_string());
        Ok(Some(node.id))
    };

    let mut assigned = 0usize;
    let mut fallback_id: Option<String> = None;
    for t in &topics {
        let sid = t.short_id.as_deref().unwrap_or(&t.id);
        // LLM 応答から取り出す（"NEW:" 接頭辞は剥がす）。
        let target = assign
            .get(sid)
            .or_else(|| assign.get(&t.id))
            .map(|v| v.trim().trim_start_matches("NEW:").trim().to_string())
            .filter(|v| !v.is_empty());

        let cat_id = match target {
            Some(name) => resolve(&db, &mut title_to_id, &mut normkey_to_title, &name)?,
            None => None,
        };
        // 応答から漏れた topic は受け皿カテゴリへ（前進保証・成功応答のときのみ）。
        let cat_id = match cat_id {
            Some(id) => id,
            None => {
                if fallback_id.is_none() {
                    fallback_id = resolve(
                        &db,
                        &mut title_to_id,
                        &mut normkey_to_title,
                        FALLBACK_CATEGORY,
                    )?;
                }
                match &fallback_id {
                    Some(id) => id.clone(),
                    None => continue,
                }
            }
        };
        if opencrab_db::queries::assign_topic_to_category(&db, agent_id, &t.id, &cat_id, &now)? {
            assigned += 1;
        }
    }
    Ok(assigned)
}

/// 1 tick 分のカテゴリ層メンテナンス: 種まき（LLM ゼロ）→ 割当（≤1 コール）。
/// `(種まき件数, 割当件数)` を返す。
pub async fn maintain_categories(
    conn: &opencrab_db::Db,
    agent_id: &str,
    llm: &dyn LlmClient,
    model: &str,
    persona_name: &str,
    personality: Option<&str>,
) -> Result<(usize, usize)> {
    let seeded = seed_categories_from_curated(conn, agent_id)?;
    let assigned =
        assign_unassigned_topics(conn, agent_id, llm, model, persona_name, personality).await?;
    Ok((seeded, assigned))
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

    fn mk_topic(
        db: &rusqlite::Connection,
        id: &str,
        short_id: &str,
        parent: &str,
        title: &str,
        created_at: &str,
    ) {
        let node = opencrab_db::queries::IndexNodeRow {
            id: id.to_string(),
            agent_id: "a1".to_string(),
            parent_id: Some(parent.to_string()),
            node_type: "topic".to_string(),
            source_type: "session_log".to_string(),
            title: title.to_string(),
            summary: format!("{title} の要約"),
            start_log_id: None,
            end_log_id: None,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 3,
            child_count: 0,
            token_count: 0,
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
            short_id: Some(short_id.to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        };
        opencrab_db::queries::insert_index_node(db, &node).unwrap();
    }

    /// root→period→session→topic の時系列ツリーを積む（session_log）。
    fn seed_timeline(conn: &opencrab_db::Db) {
        let db = conn.lock().unwrap();
        for (id, ntype, parent, sid, title) in [
            ("root-a1", "root", None, "r0", "root"),
            ("p1", "period", Some("root-a1"), "p1", "2026-06"),
            ("s1", "session", Some("p1"), "s1", "S"),
        ] {
            let node = opencrab_db::queries::IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: parent.map(String::from),
                node_type: ntype.to_string(),
                source_type: "session_log".to_string(),
                title: title.to_string(),
                summary: String::new(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-06-01T00:00:00Z".to_string(),
                updated_at: "2026-06-01T00:00:00Z".to_string(),
                short_id: Some(sid.to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            };
            opencrab_db::queries::insert_index_node(&db, &node).unwrap();
        }
        mk_topic(
            &db,
            "t1",
            "t1",
            "s1",
            "Rustの所有権",
            "2026-06-02T00:00:00Z",
        );
        mk_topic(
            &db,
            "t2",
            "t2",
            "s1",
            "Discord連携のバグ",
            "2026-06-03T00:00:00Z",
        );
    }

    fn seed_curated(conn: &opencrab_db::Db) {
        let db = conn.lock().unwrap();
        for (i, name) in ["Rustの学び", "Discord運用"].iter().enumerate() {
            opencrab_db::queries::upsert_curated_memory(
                &db,
                &opencrab_db::queries::CuratedMemoryRow {
                    id: format!("cm{i}"),
                    agent_id: "a1".to_string(),
                    category: format!("long_term/{name}"),
                    content: "…".to_string(),
                    created_at: "2026-06-01T00:00:00Z".to_string(),
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn seeding_is_idempotent() {
        let conn = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        seed_curated(&conn);
        let n1 = seed_categories_from_curated(&conn, "a1").unwrap();
        assert_eq!(n1, 2, "手書き long_term/* の2カテゴリが起きる");
        // 2 回目は 0（同名は作らない）。
        let n2 = seed_categories_from_curated(&conn, "a1").unwrap();
        assert_eq!(n2, 0);
        let db = conn.lock().unwrap();
        let tops = opencrab_db::queries::list_top_level_categories(&db, "a1").unwrap();
        assert_eq!(tops.len(), 2);
    }

    #[tokio::test]
    async fn assignment_is_sticky_and_idempotent_and_one_call() {
        let conn = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        seed_curated(&conn);
        seed_timeline(&conn);
        seed_categories_from_curated(&conn, "a1").unwrap();

        let llm = CountingLlm {
            calls: AtomicUsize::new(0),
            response: r#"{"t1": "Rustの学び", "t2": "Discord運用"}"#.to_string(),
        };
        let assigned = assign_unassigned_topics(&conn, "a1", &llm, "m", "テスト", None)
            .await
            .unwrap();
        assert_eq!(assigned, 2);
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);

        // 2 回目: 未割当が無いので LLM ゼロコール・0 件（sticky で冪等）。
        let assigned = assign_unassigned_topics(&conn, "a1", &llm, "m", "テスト", None)
            .await
            .unwrap();
        assert_eq!(assigned, 0);
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);

        // 割当先が既存の2カテゴリに収まっている（新設していない＝ドリフト無し）。
        let db = conn.lock().unwrap();
        let counts = opencrab_db::queries::count_category_members(&db, "a1").unwrap();
        assert_eq!(counts.values().sum::<i64>(), 2);
        let tops = opencrab_db::queries::list_top_level_categories(&db, "a1").unwrap();
        assert_eq!(tops.len(), 2, "既存カテゴリのみ。新設が起きていない");
    }

    #[tokio::test]
    async fn omitted_topics_go_to_fallback_for_forward_progress() {
        let conn = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        seed_curated(&conn);
        seed_timeline(&conn);
        seed_categories_from_curated(&conn, "a1").unwrap();

        // LLM は t1 だけ返す（t2 を落とす）。t2 は受け皿へ回って前進する。
        let llm = CountingLlm {
            calls: AtomicUsize::new(0),
            response: r#"{"t1": "Rustの学び"}"#.to_string(),
        };
        let assigned = assign_unassigned_topics(&conn, "a1", &llm, "m", "テスト", None)
            .await
            .unwrap();
        assert_eq!(assigned, 2, "落とされた topic も受け皿で割り当てられる");
        let db = conn.lock().unwrap();
        // 未割当がゼロになっている（無限リトライしない）。
        let left = opencrab_db::queries::list_unassigned_topics(&db, "a1", 10).unwrap();
        assert_eq!(left.len(), 0);
    }

    #[tokio::test]
    async fn llm_failure_assigns_nothing_and_retries() {
        let conn = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        seed_curated(&conn);
        seed_timeline(&conn);
        seed_categories_from_curated(&conn, "a1").unwrap();

        // パース不能応答: 何も割り当てず、次 tick 用に未割当のまま残す。
        let llm = CountingLlm {
            calls: AtomicUsize::new(0),
            response: "not json".to_string(),
        };
        let assigned = assign_unassigned_topics(&conn, "a1", &llm, "m", "テスト", None)
            .await
            .unwrap();
        assert_eq!(assigned, 0);
        let db = conn.lock().unwrap();
        let left = opencrab_db::queries::list_unassigned_topics(&db, "a1", 10).unwrap();
        assert_eq!(left.len(), 2, "割当は確定せず未割当のまま残る");
    }
}
