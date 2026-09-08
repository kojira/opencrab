//! 階層型記憶インデックスの増分構築。
//!
//! LLMを使って未インデックスのセッションログを要約し、
//! ツリー構造のインデックスノードとして保存する。

use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::engine::LlmClient;
use opencrab_llm_types::{ChatRequest, Message};

/// インデックス構築結果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexBuildResult {
    pub nodes_created: usize,
    pub logs_indexed: usize,
}

/// ツリー再マージ結果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeResult {
    pub periods_processed: usize,
    pub topics_merged: usize,
    pub topics_deleted: usize,
}

/// LLMから返されるサマリーJSON
#[derive(Debug, Deserialize)]
struct LlmSummary {
    title: String,
    summary: String,
    /// 検索用キーワード（逆引き）。旧形式の応答（keywords なし）も許容する。
    #[serde(default)]
    keywords: Vec<String>,
}

/// キーワードの正規化: 空白トリム・空要素除去・重複除去・最大8個。
/// LLM 出力が空/欠落の場合は title を空白分割したフォールバックを返す
/// （恒久的に keyword-less なノードを作らない — バックフィル対象判定が
/// `keywords_json = '[]'` のため、空のまま insert すると毎 tick 再抽出対象になる）。
fn normalize_keywords(keywords: Vec<String>, fallback_title: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = keywords
        .into_iter()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty() && seen.insert(k.clone()))
        .take(8)
        .collect();
    if out.is_empty() {
        out = fallback_title
            .split_whitespace()
            .map(|s| s.to_string())
            .take(8)
            .collect();
    }
    out
}

mod filter;

use filter::is_heartbeat_noise;
#[cfg(test)]
use filter::{idle_decision_has_no_reason, is_idle_heartbeat_speech};

pub struct IndexBuilder;

impl IndexBuilder {
    /// 増分インデックス構築。未インデックスのログをLLMで要約してツリーに追加。
    pub async fn build_incremental(
        conn: &opencrab_db::Db,
        agent_id: &str,
        llm: &dyn LlmClient,
        model: &str,
        batch_size: usize,
        persona_name: &str,
        personality: Option<&str>,
    ) -> Result<IndexBuildResult> {
        // 1. ウォーターマーク取得
        let (last_indexed_id, existing_total_nodes) = {
            let db = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
            let wm = opencrab_db::queries::get_index_watermark(&db, agent_id)?;
            (
                wm.as_ref().map(|w| w.last_indexed_log_id).unwrap_or(0),
                wm.as_ref().map(|w| w.total_nodes).unwrap_or(0),
            )
        };

        // 2. 未処理ログ取得
        let logs = {
            let db = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
            opencrab_db::queries::get_unindexed_session_logs(
                &db,
                agent_id,
                last_indexed_id,
                batch_size,
            )?
        };

        if logs.is_empty() {
            return Ok(IndexBuildResult {
                nodes_created: 0,
                logs_indexed: 0,
            });
        }

        // 3. session_idでグループ化
        let mut session_groups: HashMap<String, Vec<opencrab_db::queries::SessionLogRow>> =
            HashMap::new();
        for log in &logs {
            session_groups
                .entry(log.session_id.clone())
                .or_default()
                .push(log.clone());
        }

        let now = Utc::now().to_rfc3339();
        let mut nodes_created = 0;
        let mut max_log_id = last_indexed_id;

        // 4. ルートノード確保
        let root_id = format!("root-{agent_id}");
        {
            let db = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
            if opencrab_db::queries::get_index_node(&db, &root_id)?.is_none() {
                let root = opencrab_db::queries::IndexNodeRow {
                    id: root_id.clone(),
                    agent_id: agent_id.to_string(),
                    parent_id: None,
                    node_type: "root".to_string(),
                    source_type: "session_log".to_string(),
                    title: "Memory Root".to_string(),
                    summary: "Root node for all memories".to_string(),
                    start_log_id: None,
                    end_log_id: None,
                    source_session_id: None,
                    date_from: None,
                    date_to: None,
                    depth: 0,
                    child_count: 0,
                    token_count: 0,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                    short_id: Some("r0".to_string()),
                    keywords_json: "[]".to_string(),
                    summary_refreshed_at: None,
                };
                opencrab_db::queries::insert_index_node(&db, &root)?;
                nodes_created += 1;
            }
        }

        // 5. 各セッショングループを処理
        for (session_id, session_logs) in &session_groups {
            let first_log_id = session_logs.iter().filter_map(|l| l.id).min().unwrap_or(0);
            let last_log_id = session_logs.iter().filter_map(|l| l.id).max().unwrap_or(0);
            if last_log_id > max_log_id {
                max_log_id = last_log_id;
            }

            // 「何もしなかった tick」のノイズ行（毎tickのプロンプト scaffolding と idle の
            // speech 行）を要約材料から除く。実質行が残らないグループは topic を作らずスキップ
            // する（何もしなかったハートビートを索引しない — #374）。発生源（main.rs）は静観
            // 履歴を自己文脈に使う設計のため手を入れず、索引側だけで落とす。バッチ結合で idle と
            // 実のある tick が同居するため「グループ丸ごとスキップ」ではなく、ノイズ行を除いた
            // 実質（SPEAK/LEARN の speech・tool・inner_voice 等）の有無で判定する。
            // watermark（max_log_id）は上で前進済みなので、topic を作らなくても毎 tick
            // 同じログを取り直す無限ループにはならない。
            //
            // #573 Stage A: `session_id.starts_with("heartbeat-")` のゲートを外し、
            // [`is_heartbeat_noise`] を**全セッションに無条件適用**する。統合後（Stage B）は
            // HB tick が実会話セッションに直接記録されるため、接頭辞でノイズを絞れなくなる。
            // 述語自体は既に接頭辞非依存で安全: (1) scaffolding は `speaker_id='heartbeat'`
            // （HB 経路しか書かない）で判定、(2) idle は #517 以降「中身の無い裸マーカー
            // （`IDLE` / `NO_REPLY` 等）だけ」を落とし、`IDLE: <理由>` や散文・他者発言・
            // 実のある発話は残す。過去に配送層が記録した裸 `NO_REPLY`（#899 で記録経路は撤去済み・
            // 既存行のみ残存）が材料から落ちるだけで、中身のある行は落ちない（＝索引材料は
            // 縮まない方向にのみ変わる）。
            // #425: エコー行（HB 発話の表示専用の二重記録）は topic 要約の材料に入れない。
            // 記憶材料としての HB 発話は heartbeat セッション側が担うため、索引・宣言材料は
            // この PR の前後で不変。watermark（max_log_id）は上でフィルタ前の session_logs から
            // 算出済みなので、材料が空でも前進する（エコーだけのバッチが「永遠に未索引」で
            // バッチを詰まらせない — #416 と同族の「無言で進まない」を作らない）。
            let material_logs: Vec<&opencrab_db::queries::SessionLogRow> = session_logs
                .iter()
                .filter(|l| {
                    !opencrab_db::queries::is_heartbeat_channel_echo(l.metadata_json.as_deref())
                })
                .filter(|l| !is_heartbeat_noise(l, agent_id))
                .collect();
            if material_logs.is_empty() {
                continue;
            }

            // 期間ノード（年月）を確保。
            // ラベルはログ自身のタイムスタンプから導出する。インデックス実行時刻
            // （Utc::now()）を使うと、rebuild や遅延インデックス時に過去のセッションが
            // すべて実行月のバケットへ誤分類される。
            let period_label = session_logs
                .iter()
                .filter_map(|l| l.created_at.as_deref())
                .filter(|s| s.len() >= 7 && s.is_char_boundary(7))
                .min()
                .map(|s| s[..7].to_string())
                .unwrap_or_else(|| Utc::now().format("%Y-%m").to_string());
            let period_id = format!("period-{agent_id}-{period_label}");
            {
                let db = conn
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
                if opencrab_db::queries::get_index_node(&db, &period_id)?.is_none() {
                    let period_short_id = opencrab_db::queries::next_short_id(&db, agent_id, "p")?;
                    let period = opencrab_db::queries::IndexNodeRow {
                        id: period_id.clone(),
                        agent_id: agent_id.to_string(),
                        parent_id: Some(root_id.clone()),
                        node_type: "period".to_string(),
                        source_type: "session_log".to_string(),
                        title: period_label.clone(),
                        summary: format!("Conversations from {period_label}"),
                        start_log_id: None,
                        end_log_id: None,
                        source_session_id: None,
                        date_from: None,
                        date_to: None,
                        depth: 1,
                        child_count: 0,
                        token_count: 0,
                        created_at: now.clone(),
                        updated_at: now.clone(),
                        short_id: Some(period_short_id),
                        keywords_json: "[]".to_string(),
                        summary_refreshed_at: None,
                    };
                    opencrab_db::queries::insert_index_node(&db, &period)?;
                    nodes_created += 1;
                }
            }

            // セッションノードを確保
            let session_node_id = format!("session-{agent_id}-{session_id}");
            {
                let db = conn
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
                if opencrab_db::queries::get_index_node(&db, &session_node_id)?.is_none() {
                    // セッションノードのタイトルは最初の実質ログから推測
                    // （heartbeat では idle 行を除いた material_logs を使う）。
                    let preview = material_logs
                        .first()
                        .map(|l| {
                            let chars: Vec<char> = l.content.chars().collect();
                            if chars.len() > 50 {
                                format!("{}...", chars[..50].iter().collect::<String>())
                            } else {
                                l.content.clone()
                            }
                        })
                        .unwrap_or_default();
                    let session_short_id = opencrab_db::queries::next_short_id(&db, agent_id, "s")?;
                    let session_node = opencrab_db::queries::IndexNodeRow {
                        id: session_node_id.clone(),
                        agent_id: agent_id.to_string(),
                        parent_id: Some(period_id.clone()),
                        node_type: "session".to_string(),
                        source_type: "session_log".to_string(),
                        title: format!("Session: {}", &session_id[..session_id.len().min(8)]),
                        summary: preview,
                        start_log_id: Some(first_log_id),
                        end_log_id: Some(last_log_id),
                        source_session_id: Some(session_id.clone()),
                        date_from: None,
                        date_to: None,
                        depth: 2,
                        child_count: 0,
                        token_count: 0,
                        created_at: now.clone(),
                        updated_at: now.clone(),
                        short_id: Some(session_short_id),
                        keywords_json: "[]".to_string(),
                        summary_refreshed_at: None,
                    };
                    opencrab_db::queries::insert_index_node(&db, &session_node)?;
                    nodes_created += 1;
                }
            }

            // ログテキスト連結（heartbeat では idle 行を除いた material_logs を材料にする）
            let chunk_text: String = material_logs
                .iter()
                .map(|l| {
                    let speaker = l.speaker_id.as_deref().unwrap_or("unknown");
                    format!("[{}]: {}", speaker, l.content)
                })
                .collect::<Vec<_>>()
                .join("\n");

            // トークン数の概算（文字数 / 3 が日本語の目安）
            let token_count = (chunk_text.len() / 3) as i32;

            // LLM呼び出しでサマリー生成
            tracing::debug!(
                "index_builder: LLM call start - persona_name={:?}, has_personality={}",
                persona_name,
                personality.as_ref().map(|p| !p.is_empty()).unwrap_or(false)
            );
            tracing::debug!("index_builder: personality content = {:?}", personality);
            let prompt = if let Some(p) = personality.filter(|s| !s.is_empty()) {
                format!(
                    "あなたは {persona_name} です。\n{p}\n\n以下はあなたが体験した会話のログです。\nあなた自身の記憶として、以下の観点を含めて要約してください:\n\n1. 学んだこと・技術知見（新しく知ったこと、理解が深まったこと）\n2. 判断の理由（なぜそうしたか、どういう選択肢があったか）\n3. 関係性・感情（誰と何をしたか、どう感じたか）\n4. 失敗と教訓（うまくいかなかったこと、次回への学び）\n\n一人称で書いてください。客観的なイベントログではなく、あなたの記憶として。\n\nJSON形式で出力:\n{{\"title\": \"20字以内\", \"summary\": \"200字以内\", \"keywords\": [\"あなたがこの記憶を思い出すときの手がかりになるキーワード3〜8個（人物・技術・出来事・そのとき感じたこと）\"]}}\n\nログ:\n{chunk_text}"
                )
            } else {
                format!(
                    "以下の会話のログについて、一人称視点で記憶として要約してください。\n\n1. 学んだこと・技術知見\n2. 判断の理由\n3. 関係性・感情\n4. 失敗と教訓\n\nJSON形式で出力:\n{{\"title\": \"20字以内\", \"summary\": \"200字以内\", \"keywords\": [\"あなたがこの記憶を思い出すときの手がかりになるキーワード3〜8個（人物・技術・出来事・そのとき感じたこと）\"]}}\n\nログ:\n{chunk_text}"
                )
            };

            tracing::debug!(
                "index_builder: generated prompt (first 200 chars) = {:?}",
                prompt.chars().take(200).collect::<String>()
            );
            let system_content = if let Some(p) = personality.filter(|s| !s.is_empty()) {
                format!("あなたは {persona_name} です。\n{p}")
            } else {
                "You are a helpful assistant.".to_string()
            };
            let request = ChatRequest::new(
                model.to_string(),
                vec![Message::system(system_content), Message::user(prompt)],
            )
            .with_temperature(0.0)
            .with_max_tokens(320);

            let summary = match llm.chat(request).await {
                Ok(resp) => {
                    let text = resp.first_text().unwrap_or_default().to_string();
                    // JSON部分を抽出（マークダウンコードブロック対応）
                    let json_str = crate::llm_text::strip_code_fences(&text);
                    serde_json::from_str::<LlmSummary>(json_str).unwrap_or(LlmSummary {
                        title: format!("Topic (logs {first_log_id}-{last_log_id})"),
                        summary: material_logs
                            .first()
                            .map(|l| {
                                let chars: Vec<char> = l.content.chars().collect();
                                if chars.len() > 100 {
                                    format!("{}...", chars[..100].iter().collect::<String>())
                                } else {
                                    l.content.clone()
                                }
                            })
                            .unwrap_or_default(),
                        keywords: Vec::new(),
                    })
                }
                Err(e) => {
                    // LLM 呼び出し自体が失敗したケース（JSON パース失敗ではない）。
                    // 以前は "Summary generation failed" というプレースホルダ topic を
                    // 作っていたが、中身ゼロのノードが索引・FTS に恒久的に残り続けるだけで
                    // 意味がなかった（#378）。ここでは topic を作らずスキップする。
                    // watermark（max_log_id）はループ冒頭（220-222 行）で既に前進済みなので、
                    // topic を作らなくても毎 tick 同じログを取り直す無限ループにはならない
                    // （#374 と同じ罠を回避）。ただし失敗レンジはその分二度と再要約されない
                    // ため、何が抜けたか後から分かるよう warn に範囲とエラーを残す。
                    tracing::warn!(
                        agent_id = %agent_id,
                        session_id = %session_id,
                        start_log_id = first_log_id,
                        end_log_id = last_log_id,
                        error = %e,
                        "LLM summary generation failed, skipping topic (watermark still advances)"
                    );
                    continue;
                }
            };
            let keywords = normalize_keywords(summary.keywords, &summary.title);

            // topicノード作成
            let topic_id = format!("topic-{agent_id}-{session_id}-{first_log_id}-{last_log_id}");
            let date_from = session_logs
                .iter()
                .filter_map(|l| l.created_at.as_deref())
                .filter(|s| s.len() >= 10)
                .min()
                .map(|s| s[..10].to_string());
            let date_to = session_logs
                .iter()
                .filter_map(|l| l.created_at.as_deref())
                .filter(|s| s.len() >= 10)
                .max()
                .map(|s| s[..10].to_string());
            let mut topic = opencrab_db::queries::IndexNodeRow {
                id: topic_id.clone(),
                agent_id: agent_id.to_string(),
                parent_id: Some(session_node_id.clone()),
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: summary.title,
                summary: summary.summary,
                start_log_id: Some(first_log_id),
                end_log_id: Some(last_log_id),
                source_session_id: Some(session_id.clone()),
                date_from,
                date_to,
                depth: 3,
                child_count: 0,
                token_count,
                created_at: now.clone(),
                updated_at: now.clone(),
                short_id: None,
                keywords_json: serde_json::to_string(&keywords)
                    .unwrap_or_else(|_| "[]".to_string()),
                summary_refreshed_at: None,
            };

            {
                let db = conn
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
                if opencrab_db::queries::get_index_node(&db, &topic_id)?.is_none() {
                    topic.short_id = Some(opencrab_db::queries::next_short_id(&db, agent_id, "t")?);
                    opencrab_db::queries::insert_index_node(&db, &topic)?;
                    nodes_created += 1;
                } else {
                    tracing::debug!(
                        topic_id = %topic_id,
                        "Topic node already exists, skipping insertion"
                    );
                }
            }
        }

        // 6. 子ノード数を更新
        {
            let db = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
            let all_nodes = opencrab_db::queries::get_index_tree(&db, agent_id)?;
            let mut child_counts: HashMap<String, i32> = HashMap::new();
            for node in &all_nodes {
                if let Some(ref pid) = node.parent_id {
                    *child_counts.entry(pid.clone()).or_default() += 1;
                }
            }
            for (node_id, count) in &child_counts {
                opencrab_db::queries::update_index_node_child_count(&db, node_id, *count)?;
            }
        }

        // 7. ウォーターマーク更新
        {
            let db = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
            let wm = opencrab_db::queries::WatermarkRow {
                agent_id: agent_id.to_string(),
                last_indexed_log_id: max_log_id,
                last_indexed_at: now,
                total_nodes: existing_total_nodes + nodes_created as i64,
            };
            opencrab_db::queries::upsert_index_watermark(&db, &wm)?;
        }

        Ok(IndexBuildResult {
            nodes_created,
            logs_indexed: logs.len(),
        })
    }

    /// エージェントのインデックス全体を削除する。
    pub fn delete_index(conn: &opencrab_db::Db, agent_id: &str) -> Result<()> {
        let db = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
        // ノード削除と watermark 削除を原子化する（片方だけ消えた中間状態を残さない — #41）。
        let tx = db.unchecked_transaction()?;
        opencrab_db::queries::delete_index_nodes_for_agent(&tx, agent_id)?;
        opencrab_db::queries::delete_index_watermark_for_agent(&tx, agent_id)?;
        tx.commit()?;
        Ok(())
    }

    /// インデックスをゼロから再構築する（削除 → 増分ビルド）。
    pub async fn rebuild_index(
        conn: &opencrab_db::Db,
        agent_id: &str,
        llm: &dyn LlmClient,
        model: &str,
        batch_size: usize,
        persona_name: &str,
        personality: Option<&str>,
    ) -> Result<IndexBuildResult> {
        Self::delete_index(conn, agent_id)?;
        // ビルドは LLM 呼び出しを挟むため1トランザクションにできない。失敗時は
        // 部分的に構築されたツリーを残さず空に戻す（空 = 一貫した再実行可能状態。
        // 部分ツリーが残ると次回 build_incremental が INSERT OR IGNORE で
        // 中途半端に継ぎ足してしまう — #41）。
        let result = Self::build_incremental(
            conn,
            agent_id,
            llm,
            model,
            batch_size,
            persona_name,
            personality,
        )
        .await;
        if let Err(ref e) = result {
            tracing::warn!(agent_id = %agent_id, error = %e, "index rebuild failed — cleaning partial tree back to empty");
            if let Err(cleanup_err) = Self::delete_index(conn, agent_id) {
                tracing::error!(agent_id = %agent_id, error = %cleanup_err, "failed to clean partial index tree after rebuild failure");
            }
        }
        result
    }

    /// 既存のtopicノードをperiodレベルでLLM再要約・統合する（深さ調整）。
    ///
    /// topic数が max_topics_per_period を超えていたら、LLMでまとめて再要約し統合する。
    pub async fn merge_topics(
        conn: &opencrab_db::Db,
        agent_id: &str,
        llm: &dyn LlmClient,
        model: &str,
        max_topics_per_period: usize,
        persona_name: &str,
        personality: Option<&str>,
    ) -> Result<MergeResult> {
        let now = Utc::now().to_rfc3339();
        let tree = {
            let db = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
            opencrab_db::queries::get_index_tree(&db, agent_id)?
        };

        let period_nodes: Vec<_> = tree.iter().filter(|n| n.node_type == "period").collect();

        let mut merged_count = 0usize;
        let mut deleted_count = 0usize;

        for period in &period_nodes {
            let session_ids: Vec<String> = tree
                .iter()
                .filter(|n| n.node_type == "session" && n.parent_id.as_deref() == Some(&period.id))
                .map(|n| n.id.clone())
                .collect();

            let topic_nodes: Vec<_> = tree
                .iter()
                .filter(|n| {
                    n.node_type == "topic"
                        && n.parent_id
                            .as_ref()
                            .map(|pid| session_ids.contains(pid))
                            .unwrap_or(false)
                })
                .collect();

            if topic_nodes.len() <= max_topics_per_period {
                continue;
            }

            let summaries: Vec<String> = topic_nodes
                .iter()
                .map(|t| format!("# {}\n{}", t.title, t.summary))
                .collect();
            let combined = summaries.join("\n\n");

            let prompt = if let Some(p) = personality.filter(|s| !s.is_empty()) {
                format!(
                    "あなたは {persona_name} です。\n{p}\n\n以下の複数のトピック要約を、あなた自身の記憶として1つにまとめてください。\nJSON形式で返してください: {{\"title\": \"...\", \"summary\": \"...\"}}\n\n{combined}"
                )
            } else {
                format!(
                    "以下の複数のトピック要約を1つにまとめてください。\nJSON形式で返してください: {{\"title\": \"...\", \"summary\": \"...\"}}\n\n{combined}"
                )
            };

            let system_content = if let Some(p) = personality.filter(|s| !s.is_empty()) {
                format!("あなたは {persona_name} です。\n{p}")
            } else {
                "You are a helpful assistant.".to_string()
            };
            let request = ChatRequest::new(
                model.to_string(),
                vec![Message::system(system_content), Message::user(prompt)],
            )
            .with_temperature(0.0)
            .with_max_tokens(300);

            let merged_summary = match llm.chat(request).await {
                Ok(resp) => {
                    let text = resp.first_text().unwrap_or_default().to_string();
                    let json_str = crate::llm_text::strip_code_fences(&text);
                    serde_json::from_str::<LlmSummary>(json_str).unwrap_or(LlmSummary {
                        title: format!("Merged topics for {}", period.title),
                        summary: "Merged summary".to_string(),
                        keywords: Vec::new(),
                    })
                }
                Err(_) => LlmSummary {
                    title: format!("Merged topics for {}", period.title),
                    summary: "Merge failed".to_string(),
                    keywords: Vec::new(),
                },
            };
            // マージ後の keywords: 元トピック群の和集合（上限8）。LLM の再抽出はしない
            // （要約プロンプトは title/summary のみ返す想定のままにして安く保つ）。
            let merged_keywords: Vec<String> = {
                let from_topics: Vec<String> = topic_nodes
                    .iter()
                    .flat_map(|t| {
                        serde_json::from_str::<Vec<String>>(&t.keywords_json).unwrap_or_default()
                    })
                    .collect();
                normalize_keywords(from_topics, &merged_summary.title)
            };

            let start_log = topic_nodes.iter().filter_map(|t| t.start_log_id).min();
            let end_log = topic_nodes.iter().filter_map(|t| t.end_log_id).max();
            let token_total: i32 = topic_nodes.iter().map(|t| t.token_count).sum();

            let parent_session_id = topic_nodes
                .first()
                .and_then(|t| t.parent_id.clone())
                .unwrap_or_else(|| session_ids.first().cloned().unwrap_or_default());

            {
                let db = conn
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
                for topic in &topic_nodes {
                    // 生 SQL DELETE は FTS 影テーブルに孤児を残すため禁止。
                    opencrab_db::queries::delete_index_node(&db, &topic.id)?;
                    deleted_count += 1;
                }
            }

            // date_from/date_to は元トピック群の範囲を引き継ぐ（NULL のままだと
            // [Memory Index] の現在月ブロック（date_from LIKE 前方一致）から
            // マージ後のトピックが消えてしまう）。
            let merged_date_from = topic_nodes.iter().filter_map(|t| t.date_from.clone()).min();
            let merged_date_to = topic_nodes.iter().filter_map(|t| t.date_to.clone()).max();
            let merged_id = format!("merged-topic-{agent_id}-{}", Utc::now().timestamp_millis());
            let mut merged_node = opencrab_db::queries::IndexNodeRow {
                id: merged_id,
                agent_id: agent_id.to_string(),
                parent_id: Some(parent_session_id),
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: merged_summary.title,
                summary: merged_summary.summary,
                start_log_id: start_log,
                end_log_id: end_log,
                source_session_id: None,
                date_from: merged_date_from,
                date_to: merged_date_to,
                depth: 3,
                child_count: 0,
                token_count: token_total,
                created_at: now.clone(),
                updated_at: now.clone(),
                short_id: None,
                keywords_json: serde_json::to_string(&merged_keywords)
                    .unwrap_or_else(|_| "[]".to_string()),
                summary_refreshed_at: None,
            };
            {
                let db = conn
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
                // short_id は挿入時に必ず割り当てる（None のまま挿入すると
                // 旧 backfill 頼みになり、short_id 無しの窓が生じる — #41）。
                merged_node.short_id =
                    Some(opencrab_db::queries::next_short_id(&db, agent_id, "t")?);
                opencrab_db::queries::insert_index_node(&db, &merged_node)?;
            }
            merged_count += 1;
        }

        {
            let db = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
            let all_nodes = opencrab_db::queries::get_index_tree(&db, agent_id)?;
            let mut child_counts: HashMap<String, i32> = HashMap::new();
            for node in &all_nodes {
                if let Some(ref pid) = node.parent_id {
                    *child_counts.entry(pid.clone()).or_default() += 1;
                }
            }
            for (node_id, count) in &child_counts {
                opencrab_db::queries::update_index_node_child_count(&db, node_id, *count)?;
            }
        }

        Ok(MergeResult {
            periods_processed: period_nodes.len(),
            topics_merged: merged_count,
            topics_deleted: deleted_count,
        })
    }
}

#[cfg(test)]
#[path = "index_builder/tests/mod.rs"]
mod tests;
