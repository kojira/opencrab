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

/// heartbeat セッションの speech 行が「エージェント自身の idle（静観）応答」かどうかを判定する。
///
/// 判定は**過去データ**（旧 `SPEAK:` / `LEARN` / `IDLE` の語彙で残った heartbeat speech 行）を
/// メモリ索引から除外するためのもの: 応答本文に非空の `SPEAK:` があれば発話、`LEARN` を含めば
/// 学習として残す。**#588 Stage 3 でハートビートの専用語彙（旧 `HeartbeatDecision`）は撤去され、
/// 新しい行はこの形式で書かれない**（通常の配送記録になる）ので、この判定は既存の履歴のための
/// もの。マーカー集合を列挙せず「全大文字トークン + 中身の有無」で見るため、語彙撤去後の新しい
/// 記録に対しても安全側（＝実ありとして残す）に働く。
///
/// #517: 残り（SPEAK/LEARN でない）は従来「一律 idle」としていたが、#515 で当時の HB は IDLE の
/// 記録が「`IDLE: <なぜ見送ったか>`」という**本人の言葉の理由**を持つようになっていた（過去データ）。
/// 理由つきの記録は材料として意味があるので落とさない。**中身があるか**で判定する:
/// 先頭の全大文字マーカー（`IDLE` / `NO_REPLY` 等 = `[A-Z_]+` と続く任意の `:`）を剥いで
/// なお非空の本文が残れば「実あり」として残し、剥いだ後が空（無内容の `IDLE` 等・
/// pre-#515 の 2,991 件）だけを idle ノイズとして除外する。
///
/// この変更は**除外を狭める方向のみ**（新 idle 集合 ⊆ 旧 idle 集合）。SPEAK/LEARN の
/// 扱いは不変。マーカー集合を列挙せず「全大文字トークン + 中身の有無」で見るので、
/// 生成タイトルの揺れや将来のマーカー追加に強い。
///
/// **話者ガード**: idle として捨ててよいのは `speaker_id == agent_id`、つまり
/// エージェント自身の応答行に限る。本番のハートビートセッションは実測で
/// 単一話者（全 speech が自分）だが、万一「他者の発言」が混ざっても、それは
/// 相手の言葉という実質なので idle 扱いにせず材料に残す（本文が SPEAK:/LEARN を
/// 含まなくても落とさない）。
///
/// 対象は `speech` 行のみ。それ以外の log_type は常に「実あり」として残す
/// （呼び出し側で heartbeat セッションに限定する）。
fn is_idle_heartbeat_speech(log: &opencrab_db::queries::SessionLogRow, agent_id: &str) -> bool {
    if log.log_type != "speech" {
        return false;
    }
    // 自分以外の発話（他者の言葉）は idle 扱いにしない = 材料に残す。
    if log.speaker_id.as_deref() != Some(agent_id) {
        return false;
    }
    let text = log.content.trim();
    // SPEAK: の後に非空の内容があれば発話 = 実あり。
    if let Some(rest) = text
        .lines()
        .find(|l| l.contains("SPEAK:"))
        .and_then(|l| l.split_once("SPEAK:").map(|x| x.1))
    {
        if !rest.trim().is_empty() {
            return false;
        }
    }
    // LEARN を含めば学習 = 実あり。
    if text.to_uppercase().contains("LEARN") {
        return false;
    }
    // #517: 先頭の全大文字マーカーを剥いで中身が残れば実あり（`IDLE: <理由>` を残す）。
    // 無内容の裸マーカー（`IDLE` 等）だけが idle ノイズ。
    idle_decision_has_no_reason(text)
}

/// 先頭の全大文字決定マーカー（`IDLE` / `NO_REPLY` 等 = `[A-Z_]+`）と続く任意の `:` を
/// 剥いで、残りが空白のみか（＝理由本文が無い裸マーカー）を返す（#517）。
///
/// マーカーが無い本文（CJK 等で始まる散文）は剥がすものが無く、非空なら `false`。
/// これにより「中身があるか」で idle を判定し、`IDLE: <理由>`・改行後に本文が続く
/// `IDLE\n\n…`・マーカー無しの散文はすべて残し、無内容の `IDLE` / `NO_REPLY` /
/// 空文字だけを true（＝ idle ノイズ）とする。
fn idle_decision_has_no_reason(text: &str) -> bool {
    let t = text.trim();
    // 先頭の連続する [A-Z_] をマーカーとして数える（ASCII 大文字とアンダースコアのみ）。
    let marker_len = t
        .bytes()
        .take_while(|b| b.is_ascii_uppercase() || *b == b'_')
        .count();
    // マーカーが実在するときだけ剥ぐ（無ければ本文全体をそのまま見る）。
    let rest = &t[marker_len..];
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    rest.trim().is_empty()
}

/// 「何もしていない tick」を構成するノイズ行かどうか（HB 由来ノイズの判定）。
///
/// #573 Stage A で呼び出し側の `heartbeat-` 接頭辞ゲートを外し、全セッションへ無条件適用
/// するようになった。述語は元から接頭辞非依存で、下記 2 種は HB 経路しか生まない目印
/// （`speaker_id='heartbeat'`）と中身の有無で判定するため、実会話セッションの中身のある
/// 行を落とすことはない（[`is_idle_heartbeat_speech`] 参照）。
///
/// 除外対象は 2 種類:
/// 1. 毎 tick 注入されるハートビートのプロンプト scaffolding
///    （`log_type='system'` かつ `speaker_id='heartbeat'`, `main.rs` の 357-388 行）。
///    これは記憶ではなく毎回同一の指示文で、除かないと全 heartbeat バッチに必ず
///    残ってしまい「実質ログが残らない」バッチが存在しなくなる。
/// 2. idle（静観）の speech 行（[`is_idle_heartbeat_speech`]）。
///
/// 逆に `tool_call` / `tool_result` / `inner_voice` / 実のある speech（SPEAK/LEARN）/
/// 他者の発言は実際の活動・実質なので材料に残す。これらが 1 件も残らないバッチだけを
/// 「純idle」として topic 化しない（#374）。
fn is_heartbeat_noise(log: &opencrab_db::queries::SessionLogRow, agent_id: &str) -> bool {
    if log.log_type == "system"
        && log.speaker_id.as_deref() == Some(opencrab_db::queries::HEARTBEAT_SPEAKER_ID)
    {
        return true;
    }
    is_idle_heartbeat_speech(log, agent_id)
}

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
            // 実のある発話は残す。実会話セッションに既にある裸 `NO_REPLY`（`record_agent_no_reply`
            // が記録）が材料から落ちるだけで、中身のある行は落ちない（＝索引材料は縮まない方向に
            // のみ変わる）。
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
mod tests {
    use super::*;
    use crate::engine::{ChatRequest, ChatResponse, LlmClient};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct MockLlm;

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse::text(
                r#"{"title": "テストトピック", "summary": "テスト要約です。"}"#.to_string(),
            ))
        }
    }

    struct RecordingMockLlm {
        last_request: Arc<Mutex<Option<ChatRequest>>>,
    }

    #[async_trait]
    impl LlmClient for RecordingMockLlm {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
            *self.last_request.lock().unwrap() = Some(req);
            Ok(ChatResponse::text(
                r#"{"title": "テストトピック", "summary": "テスト要約です。"}"#.to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn test_build_incremental_empty() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        let result =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert_eq!(result.nodes_created, 0);
        assert_eq!(result.logs_indexed, 0);
    }

    #[tokio::test]
    async fn test_build_incremental_with_logs() {
        let db_conn = opencrab_db::init_memory().unwrap();
        // Insert some test logs
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Hello, this is a test message about Rust programming.".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let log2 = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Yes, Rust is great for systems programming.".to_string(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: Some(2),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log2).unwrap();

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        let result =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();

        // root + period + session + topic = 4 nodes
        assert_eq!(result.nodes_created, 4);
        assert_eq!(result.logs_indexed, 2);

        // Verify watermark
        let db = conn.lock().unwrap();
        let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
            .unwrap()
            .unwrap();
        assert_eq!(wm.last_indexed_log_id, 2);
        assert_eq!(wm.total_nodes, 4);

        // Verify tree structure
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        assert_eq!(tree.len(), 4);
        assert!(tree.iter().any(|n| n.node_type == "root"));
        assert!(tree.iter().any(|n| n.node_type == "period"));
        assert!(tree.iter().any(|n| n.node_type == "session"));
        assert!(tree.iter().any(|n| n.node_type == "topic"));

        // Topic node should have LLM-generated title
        let topic = tree.iter().find(|n| n.node_type == "topic").unwrap();
        assert_eq!(topic.title, "テストトピック");
        assert_eq!(topic.summary, "テスト要約です。");
        // keywords 無しの旧形式応答 → title フォールバック（空にはしない）
        assert_eq!(topic.keywords_json, r#"["テストトピック"]"#);
    }

    struct KeywordMockLlm;

    #[async_trait]
    impl LlmClient for KeywordMockLlm {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse::text(
                r#"{"title": "Rust勉強会", "summary": "所有権を学んだ。", "keywords": ["Rust", "所有権", " Rust ", ""]}"#
                    .to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn test_topic_keywords_from_llm_normalized_and_searchable() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Rust ownership discussion".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let conn = opencrab_db::Db::from_connection(db_conn);
        IndexBuilder::build_incremental(&conn, "agent-1", &KeywordMockLlm, "m", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        let topic = tree.iter().find(|n| n.node_type == "topic").unwrap();
        // トリム・空要素除去・重複除去される
        assert_eq!(topic.keywords_json, r#"["Rust","所有権"]"#);
        // FTS 逆引きでキーワードから引ける
        let hits =
            opencrab_db::queries::search_index_nodes(&db, "agent-1", "所有権", 10, None).unwrap();
        assert!(hits.iter().any(|h| h.node_id == topic.id));
    }

    #[tokio::test]
    async fn test_merge_topics_leaves_no_fts_orphans() {
        let db_conn = opencrab_db::init_memory().unwrap();
        // 同一月に 3 セッション分のログ → topic 3 個
        for s in 1..=3 {
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "agent-1".to_string(),
                session_id: format!("session-{s}"),
                log_type: "message".to_string(),
                content: format!("unique-marker-{s} content"),
                speaker_id: Some("user-1".to_string()),
                turn_number: Some(1),
                metadata_json: None,
                created_at: None,
            };
            opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();
        }
        let conn = opencrab_db::Db::from_connection(db_conn);
        IndexBuilder::build_incremental(&conn, "agent-1", &KeywordMockLlm, "m", 50, "", None)
            .await
            .unwrap();

        let merge = IndexBuilder::merge_topics(&conn, "agent-1", &KeywordMockLlm, "m", 1, "", None)
            .await
            .unwrap();
        assert!(merge.topics_deleted >= 2);

        let db = conn.lock().unwrap();
        // FTS 行数 = ノード行数（孤児なし）
        let fts: i64 = db
            .query_row("SELECT COUNT(*) FROM memory_index_fts", [], |r| r.get(0))
            .unwrap();
        let nodes: i64 = db
            .query_row("SELECT COUNT(*) FROM memory_index_nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts, nodes);
        // マージノードは元トピックの keywords を引き継ぐ
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        let merged = tree
            .iter()
            .find(|n| n.id.starts_with("merged-topic-"))
            .unwrap();
        assert!(merged.keywords_json.contains("Rust"));
    }

    /// T-2.1: ペルソナ情報が要約プロンプトに含まれる
    #[tokio::test]
    async fn test_persona_prompt_contains_persona_info() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Hello, this is a test message.".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let conn = opencrab_db::Db::from_connection(db_conn);
        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };

        let _result = IndexBuilder::build_incremental(
            &conn,
            "agent-1",
            &llm,
            "test-model",
            50,
            "エージェントC",
            Some("17歳のオタク高校生"),
        )
        .await
        .unwrap();

        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = request.messages[1].text_content().unwrap_or("");
        assert!(
            prompt.contains("エージェントC"),
            "プロンプトにペルソナ名が含まれるべき"
        );
        assert!(
            prompt.contains("17歳のオタク高校生"),
            "プロンプトにpersonalityが含まれるべき"
        );
    }

    /// T-2.2: 注目ポイント4軸がプロンプトに含まれる
    #[tokio::test]
    async fn test_persona_prompt_contains_four_axes() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Test message for four axes check.".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let conn = opencrab_db::Db::from_connection(db_conn);
        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };

        let _result = IndexBuilder::build_incremental(
            &conn,
            "agent-1",
            &llm,
            "test-model",
            50,
            "テスト",
            Some("テスト用ペルソナ"),
        )
        .await
        .unwrap();

        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = request.messages[1].text_content().unwrap_or("");
        assert!(
            prompt.contains("学んだこと") || prompt.contains("技術知見"),
            "技術知見軸が含まれるべき"
        );
        assert!(
            prompt.contains("判断の理由") || prompt.contains("判断"),
            "判断軸が含まれるべき"
        );
        assert!(
            prompt.contains("関係性") || prompt.contains("感情"),
            "関係性・感情軸が含まれるべき"
        );
        assert!(
            prompt.contains("失敗") || prompt.contains("教訓"),
            "失敗・教訓軸が含まれるべき"
        );
    }

    /// T-2.5: ペルソナが空でもエラーにならずデフォルト一人称で要約される
    #[tokio::test]
    async fn test_persona_empty_uses_default() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Test message for empty persona.".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let conn = opencrab_db::Db::from_connection(db_conn);
        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };

        let result =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();

        assert!(result.nodes_created > 0, "ノードが生成されるべき");

        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = request.messages[1].text_content().unwrap_or("");
        // Default prompt should still use 一人称
        assert!(
            prompt.contains("一人称"),
            "デフォルトプロンプトに一人称が含まれるべき"
        );
    }

    #[tokio::test]
    async fn test_build_incremental_idempotent() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Test message".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // First build
        let r1 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert!(r1.nodes_created > 0);

        // Second build should create no new nodes (no new logs)
        let r2 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert_eq!(r2.nodes_created, 0);
        assert_eq!(r2.logs_indexed, 0);
    }

    /// LLM 呼び出しそのものが失敗するモック（#378）。
    struct FailingLlm;

    #[async_trait]
    impl LlmClient for FailingLlm {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Err(anyhow::anyhow!("simulated provider failure"))
        }
    }

    /// JSON ではない応答を返すモック（要約経路の `Ok` だがパース失敗ケース）。
    struct InvalidJsonMockLlm;

    #[async_trait]
    impl LlmClient for InvalidJsonMockLlm {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse::text(
                "これはJSONではない普通の文章です".to_string(),
            ))
        }
    }

    /// #378: LLM 呼び出しが Err のときはプレースホルダ topic を作らずスキップする。
    /// ただし watermark は前進させ、同じログを毎 tick 取り直す無限ループ（#374 の罠）を防ぐ。
    #[tokio::test]
    async fn test_llm_error_skips_topic_but_advances_watermark() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 2);
        let conn = opencrab_db::Db::from_connection(db_conn);

        let result = IndexBuilder::build_incremental(
            &conn,
            "agent-1",
            &FailingLlm,
            "test-model",
            50,
            "",
            None,
        )
        .await
        .unwrap();

        // logs は取得されている（スキップされたのは topic 生成だけ）
        assert_eq!(result.logs_indexed, 2);

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        // topic ノードは 1 件も作られない（"Summary generation failed" プレースホルダを作らない）
        assert!(
            !tree.iter().any(|n| n.node_type == "topic"),
            "LLM エラー時に topic を作ってはならない"
        );
        assert!(
            !tree
                .iter()
                .any(|n| n.summary == "Summary generation failed"),
            "'Summary generation failed' プレースホルダを作ってはならない"
        );

        // watermark は最終ログ ID まで前進している（再ビルドで再取得しない）
        let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
            .unwrap()
            .unwrap();
        assert_eq!(
            wm.last_indexed_log_id, 2,
            "watermark は失敗レンジを追い越して前進すべき"
        );
        drop(db);

        // 再ビルドしても新しいログは取得されない（毎 tick 再取得ループにならない）
        let r2 = IndexBuilder::build_incremental(
            &conn,
            "agent-1",
            &FailingLlm,
            "test-model",
            50,
            "",
            None,
        )
        .await
        .unwrap();
        assert_eq!(r2.logs_indexed, 0, "watermark 前進済みなので再取得しない");
    }

    /// #378: JSON パース失敗（応答は返っている）ケースは従来どおり topic を作る。
    /// 先頭ログ本文の先頭 100 字を summary に使う既存挙動を変えない。
    #[tokio::test]
    async fn test_invalid_json_still_creates_topic() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let content = "Rust の所有権について議論した内容のログ".to_string();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: content.clone(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();
        let conn = opencrab_db::Db::from_connection(db_conn);

        IndexBuilder::build_incremental(&conn, "agent-1", &InvalidJsonMockLlm, "m", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        let topic = tree
            .iter()
            .find(|n| n.node_type == "topic")
            .expect("パース失敗時も topic は作られるべき");
        // 100 字未満なので先頭ログ本文がそのまま summary になる
        assert_eq!(topic.summary, content);
        assert_ne!(topic.summary, "Summary generation failed");
        assert_eq!(topic.title, "Topic (logs 1-1)");
    }

    /// ヘルパー: 指定セッションにN件のログを投入
    fn insert_logs(conn: &rusqlite::Connection, agent_id: &str, session_id: &str, count: usize) {
        for i in 0..count {
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                log_type: "message".to_string(),
                content: format!("Message {i} in session {session_id}"),
                speaker_id: Some(if i % 2 == 0 {
                    "user-1".to_string()
                } else {
                    agent_id.to_string()
                }),
                turn_number: Some(i as i32),
                metadata_json: None,
                created_at: None,
            };
            opencrab_db::queries::insert_session_log(conn, &log).unwrap();
        }
    }

    /// 複数セッションにまたがるログ — 各セッションが別のsession/topicノードになるか
    #[tokio::test]
    async fn test_multiple_sessions() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-a", 5);
        insert_logs(&db_conn, "agent-1", "session-b", 3);
        insert_logs(&db_conn, "agent-1", "session-c", 4);

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        let result =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();

        assert_eq!(result.logs_indexed, 12); // 5+3+4

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();

        // root(1) + period(1) + session(3) + topic(3) = 8
        assert_eq!(tree.len(), 8);
        let sessions: Vec<_> = tree.iter().filter(|n| n.node_type == "session").collect();
        assert_eq!(sessions.len(), 3);
        let topics: Vec<_> = tree.iter().filter(|n| n.node_type == "topic").collect();
        assert_eq!(topics.len(), 3);

        // 各topicは異なるsource_session_idを持つ
        let mut topic_sessions: Vec<_> = topics
            .iter()
            .filter_map(|n| n.source_session_id.clone())
            .collect();
        topic_sessions.sort();
        assert_eq!(topic_sessions, vec!["session-a", "session-b", "session-c"]);
    }

    /// バッチサイズ超過 — batch_sizeで切られ、残りは次回ビルドで処理される
    #[tokio::test]
    async fn test_batch_size_limit() {
        let db_conn = opencrab_db::init_memory().unwrap();
        // 30件投入、batch_size=10で実行
        insert_logs(&db_conn, "agent-1", "session-1", 15);
        insert_logs(&db_conn, "agent-1", "session-2", 15);

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // batch_size=10: 最初の10件のみ処理される
        let r1 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 10, "", None)
                .await
                .unwrap();
        assert_eq!(r1.logs_indexed, 10);

        // ウォーターマークは10件目まで進んでいる
        {
            let db = conn.lock().unwrap();
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(wm.last_indexed_log_id, 10);
        }

        // 2回目: 次の10件
        let r2 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 10, "", None)
                .await
                .unwrap();
        assert_eq!(r2.logs_indexed, 10);

        // 3回目: 残り10件
        let r3 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 10, "", None)
                .await
                .unwrap();
        assert_eq!(r3.logs_indexed, 10);

        // 4回目: もう残りなし
        let r4 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 10, "", None)
                .await
                .unwrap();
        assert_eq!(r4.logs_indexed, 0);
        assert_eq!(r4.nodes_created, 0);

        // 最終的にウォーターマークは30件目
        {
            let db = conn.lock().unwrap();
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(wm.last_indexed_log_id, 30);
        }
    }

    /// 増分ビルド — 初回ビルド後に新ログ追加、再ビルドで新ノードのみ追加
    #[tokio::test]
    async fn test_incremental_after_new_logs() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // 初回ビルド
        let r1 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert_eq!(r1.logs_indexed, 5);
        let first_node_count = r1.nodes_created;

        // 新しいセッションにログ追加
        {
            let db = conn.lock().unwrap();
            insert_logs(&db, "agent-1", "session-2", 3);
        }

        // 2回目ビルド — 新ログのみ処理、session-2のノードが追加される
        let r2 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert_eq!(r2.logs_indexed, 3);
        // session + topic = 2 new nodes (root/periodは既存を再利用)
        assert_eq!(r2.nodes_created, 2);

        // ツリー全体を検証
        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        assert_eq!(tree.len(), first_node_count + 2); // 初回4 + session+topic=2
        let sessions: Vec<_> = tree.iter().filter(|n| n.node_type == "session").collect();
        assert_eq!(sessions.len(), 2);

        // ウォーターマークが最終ログまで進んでいる
        let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
            .unwrap()
            .unwrap();
        assert_eq!(wm.last_indexed_log_id, 8); // 5+3
    }

    /// 大量ログ（1セッション100件） — 全件が1つのtopicノードにまとまる
    #[tokio::test]
    async fn test_large_single_session() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-big", 100);

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        let result =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 200, "", None)
                .await
                .unwrap();

        assert_eq!(result.logs_indexed, 100);

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        // root(1) + period(1) + session(1) + topic(1) = 4
        assert_eq!(tree.len(), 4);

        let topic = tree.iter().find(|n| n.node_type == "topic").unwrap();
        assert_eq!(topic.start_log_id, Some(1));
        assert_eq!(topic.end_log_id, Some(100));
        assert!(topic.token_count > 0);

        // child_countが正しく更新されている
        let root = tree.iter().find(|n| n.node_type == "root").unwrap();
        assert_eq!(root.child_count, 1); // period
        let session = tree.iter().find(|n| n.node_type == "session").unwrap();
        assert_eq!(session.child_count, 1); // topic
    }

    /// 異なるエージェントのログが混在 — agent_idでフィルタされる
    #[tokio::test]
    async fn test_agent_isolation() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);
        insert_logs(&db_conn, "agent-2", "session-2", 8);

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // agent-1のみビルド
        let r1 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert_eq!(r1.logs_indexed, 5);

        // agent-2のみビルド
        let r2 =
            IndexBuilder::build_incremental(&conn, "agent-2", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert_eq!(r2.logs_indexed, 8);

        // 各エージェントのツリーが独立
        let db = conn.lock().unwrap();
        let tree1 = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        let tree2 = opencrab_db::queries::get_index_tree(&db, "agent-2").unwrap();
        assert_eq!(tree1.len(), 4); // root+period+session+topic
        assert_eq!(tree2.len(), 4);
        // ノードIDが重複しない
        let ids1: Vec<_> = tree1.iter().map(|n| &n.id).collect();
        let ids2: Vec<_> = tree2.iter().map(|n| &n.id).collect();
        for id in &ids1 {
            assert!(!ids2.contains(id));
        }
    }

    // ================================================================
    // delete_index / rebuild_index / merge_topics テスト
    // ================================================================

    /// delete_index: 全ノードとウォーターマークが削除されるか
    #[tokio::test]
    async fn test_delete_index() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // まずインデックスを構築
        let r = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        assert!(r.nodes_created > 0);

        // 削除前にツリーとウォーターマークが存在することを確認
        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            assert!(!tree.is_empty(), "削除前はノードが存在するはず");
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1").unwrap();
            assert!(wm.is_some(), "削除前はウォーターマークが存在するはず");
        }

        // 削除実行
        IndexBuilder::delete_index(&conn, "agent-1").unwrap();

        // 削除後: ツリーもウォーターマークも空になるはず
        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            assert!(tree.is_empty(), "削除後はノードが0件になるはず");
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1").unwrap();
            assert!(wm.is_none(), "削除後はウォーターマークがNoneになるはず");
        }
    }

    /// delete_index: 他のエージェントのデータは影響を受けない
    #[tokio::test]
    async fn test_delete_index_isolation() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);
        insert_logs(&db_conn, "agent-2", "session-2", 5);
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        IndexBuilder::build_incremental(&conn, "agent-2", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        // agent-1のみ削除
        IndexBuilder::delete_index(&conn, "agent-1").unwrap();

        // agent-1は空、agent-2は無事
        {
            let db = conn.lock().unwrap();
            let tree1 = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            let tree2 = opencrab_db::queries::get_index_tree(&db, "agent-2").unwrap();
            assert!(tree1.is_empty(), "agent-1のノードは削除済みのはず");
            assert!(!tree2.is_empty(), "agent-2のノードは無事なはず");
            let wm1 = opencrab_db::queries::get_index_watermark(&db, "agent-1").unwrap();
            let wm2 = opencrab_db::queries::get_index_watermark(&db, "agent-2").unwrap();
            assert!(wm1.is_none(), "agent-1のウォーターマークは削除済みのはず");
            assert!(wm2.is_some(), "agent-2のウォーターマークは無事なはず");
        }
    }

    /// rebuild_index: 削除 → 再構築でウォーターマークがリセットされ、全ログが再インデックスされるか
    #[tokio::test]
    async fn test_rebuild_index() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);
        insert_logs(&db_conn, "agent-1", "session-2", 3);
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // 初回ビルド
        let r1 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        let first_tree_len = {
            let db = conn.lock().unwrap();
            opencrab_db::queries::get_index_tree(&db, "agent-1")
                .unwrap()
                .len()
        };
        assert!(r1.nodes_created > 0);

        // 再構築
        let r2 = IndexBuilder::rebuild_index(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        // 再構築後は全ログが再インデックスされる
        assert_eq!(r2.logs_indexed, 8, "8件全ログが再インデックスされるはず");
        assert!(r2.nodes_created > 0, "ノードが再作成されるはず");

        // ウォーターマークが最新の状態
        {
            let db = conn.lock().unwrap();
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(
                wm.last_indexed_log_id, 8,
                "ウォーターマークが最終ログIDを指すはず"
            );

            // ツリーが再構築されている
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            assert_eq!(tree.len(), first_tree_len, "再構築後もツリー構造が同じはず");
        }
    }

    /// rebuild_index: 空のインデックスからも再構築できる
    #[tokio::test]
    async fn test_rebuild_index_from_empty() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 3);
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // ビルドせずに再構築（初回rebuild）
        let r = IndexBuilder::rebuild_index(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        assert_eq!(r.logs_indexed, 3);
        assert_eq!(r.nodes_created, 4); // root+period+session+topic
    }

    /// merge_topics: topicが閾値以下なら変化なし
    #[tokio::test]
    async fn test_merge_topics_no_merge_needed() {
        let db_conn = opencrab_db::init_memory().unwrap();
        // 2セッション = 2topicノード
        insert_logs(&db_conn, "agent-1", "session-a", 3);
        insert_logs(&db_conn, "agent-1", "session-b", 3);
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        let tree_before = {
            let db = conn.lock().unwrap();
            opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap()
        };

        // max_topics_per_period=5 なので2topicは閾値以下
        let result = IndexBuilder::merge_topics(&conn, "agent-1", &llm, "test-model", 5, "", None)
            .await
            .unwrap();

        assert_eq!(result.topics_merged, 0, "マージ不要なのでmergedは0");
        assert_eq!(result.topics_deleted, 0, "削除も0");

        // ツリーは変化なし
        let tree_after = {
            let db = conn.lock().unwrap();
            opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap()
        };
        assert_eq!(
            tree_before.len(),
            tree_after.len(),
            "マージなしでツリー長は変化しない"
        );
    }

    /// merge_topics: topicが閾値超過でマージされ、要約が統合されるか
    #[tokio::test]
    async fn test_merge_topics_triggers_merge() {
        let db_conn = opencrab_db::init_memory().unwrap();
        // 4セッション = 4topicノード（1periodの下）
        insert_logs(&db_conn, "agent-1", "session-a", 2);
        insert_logs(&db_conn, "agent-1", "session-b", 2);
        insert_logs(&db_conn, "agent-1", "session-c", 2);
        insert_logs(&db_conn, "agent-1", "session-d", 2);
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        // max_topics_per_period=2 → 4topics > 2 なのでマージ発動
        let result = IndexBuilder::merge_topics(&conn, "agent-1", &llm, "test-model", 2, "", None)
            .await
            .unwrap();

        assert_eq!(result.periods_processed, 1, "1つのperiodが処理されるはず");
        assert!(
            result.topics_merged >= 1,
            "少なくとも1回のマージが実行されるはず"
        );
        assert!(
            result.topics_deleted >= 3,
            "旧topicは削除されるはず（4 - 1 = 3）"
        );

        // マージ後: 4topicが統合されて1topicになる
        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        let topics: Vec<_> = tree.iter().filter(|n| n.node_type == "topic").collect();
        // マージで4→1になる（merged-topicが1つ）
        assert_eq!(topics.len(), 1, "4topicがマージされて1topicになるはず");
        // マージされたtopicはMockLlmのタイトルを持つ
        assert_eq!(
            topics[0].title, "テストトピック",
            "LLM生成タイトルを持つはず"
        );

        // child_countが正しく更新されている
        let session_nodes: Vec<_> = tree.iter().filter(|n| n.node_type == "session").collect();
        // マージ後、統合topicの親sessionのchild_countが更新されているはず
        let _ = session_nodes; // child_count検証は親ID依存のため省略
    }

    /// merge_topics: マージ後にrebuild_indexしても整合性が保たれる
    #[tokio::test]
    async fn test_merge_then_rebuild() {
        let db_conn = opencrab_db::init_memory().unwrap();
        for i in 0..5 {
            insert_logs(&db_conn, "agent-1", &format!("session-{i}"), 3);
        }
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        // まずマージ
        let merge_result =
            IndexBuilder::merge_topics(&conn, "agent-1", &llm, "test-model", 2, "", None)
                .await
                .unwrap();
        assert!(merge_result.topics_merged > 0);

        // その後rebuildすると完全にリフレッシュされる
        let rebuild_result =
            IndexBuilder::rebuild_index(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert_eq!(
            rebuild_result.logs_indexed, 15,
            "5セッション×3ログ=15件が再インデックス"
        );

        // orphanなし、child_count整合
        let db = conn.lock().unwrap();
        let metrics =
            crate::memory_index::graph_query::IndexQualityMetrics::compute(&db, "agent-1").unwrap();
        assert_eq!(metrics.orphan_count, 0);
        assert_eq!(metrics.child_count_mismatch, 0);
        assert_eq!(metrics.log_coverage, 1.0);
    }

    /// ヘルパー: 指定セッションにログを created_at 付きで投入
    /// insert_session_log は常に Utc::now() を使うため、INSERT 後に UPDATE で上書きする
    fn insert_log_with_date(
        conn: &rusqlite::Connection,
        agent_id: &str,
        session_id: &str,
        turn: i32,
        content: &str,
        created_at: &str,
    ) {
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "message".to_string(),
            content: content.to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(turn),
            metadata_json: None,
            created_at: Some(created_at.to_string()),
        };
        let row_id = opencrab_db::queries::insert_session_log(conn, &log).unwrap();
        conn.execute(
            "UPDATE memory_sessions SET created_at = ?1 WHERE id = ?2",
            rusqlite::params![created_at, row_id],
        )
        .unwrap();
    }

    /// T-1.10: 同一日のログ → date_from == date_to == その日
    #[tokio::test]
    async fn test_build_date_same_day() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        {
            let c = conn.lock().unwrap();
            insert_log_with_date(
                &c,
                "agent-d",
                "sess-d",
                0,
                "Morning msg",
                "2026-04-01 09:00:00",
            );
            insert_log_with_date(
                &c,
                "agent-d",
                "sess-d",
                1,
                "Afternoon msg",
                "2026-04-01 15:00:00",
            );
        }

        let result =
            IndexBuilder::build_incremental(&conn, "agent-d", &llm, "test-model", 2, "", None)
                .await
                .unwrap();
        assert!(result.nodes_created > 0);

        let c = conn.lock().unwrap();
        let topics =
            opencrab_db::queries::get_topic_nodes_for_session(&c, "agent-d", "sess-d").unwrap();
        assert!(!topics.is_empty());
        let t = &topics[0];
        assert_eq!(t.date_from.as_deref(), Some("2026-04-01"));
        assert_eq!(t.date_to.as_deref(), Some("2026-04-01"));
    }

    /// T-1.11: 複数日にまたがるログ → date_from < date_to
    #[tokio::test]
    async fn test_build_date_multi_day() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        {
            let c = conn.lock().unwrap();
            insert_log_with_date(
                &c,
                "agent-m",
                "sess-m",
                0,
                "Day 1 msg",
                "2026-04-01 10:00:00",
            );
            insert_log_with_date(
                &c,
                "agent-m",
                "sess-m",
                1,
                "Day 3 msg",
                "2026-04-03 14:00:00",
            );
        }

        let result =
            IndexBuilder::build_incremental(&conn, "agent-m", &llm, "test-model", 2, "", None)
                .await
                .unwrap();
        assert!(result.nodes_created > 0);

        let c = conn.lock().unwrap();
        let topics =
            opencrab_db::queries::get_topic_nodes_for_session(&c, "agent-m", "sess-m").unwrap();
        assert!(!topics.is_empty());
        let t = &topics[0];
        assert_eq!(t.date_from.as_deref(), Some("2026-04-01"));
        assert_eq!(t.date_to.as_deref(), Some("2026-04-03"));
    }

    /// T-1.12: created_at が短い/空文字のログ → date_from/date_to は None（パニックしない）
    /// memory_sessions.created_at は NOT NULL なので NULL にはできないが、
    /// 空文字や短い文字列でも s[..10] スライスでパニックしないことを検証する
    #[tokio::test]
    async fn test_build_date_null_created_at() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        {
            let c = conn.lock().unwrap();
            // Use the existing insert_logs helper which sets created_at to now()
            insert_logs(&c, "agent-n", "sess-n", 3);
            // Set created_at to empty string to simulate missing/invalid date
            c.execute_batch(
                "UPDATE memory_sessions SET created_at = '' WHERE agent_id = 'agent-n'",
            )
            .unwrap();
        }

        let result =
            IndexBuilder::build_incremental(&conn, "agent-n", &llm, "test-model", 3, "", None)
                .await
                .unwrap();
        // Should not panic, nodes should still be created
        assert!(result.nodes_created > 0);

        let c = conn.lock().unwrap();
        let topics =
            opencrab_db::queries::get_topic_nodes_for_session(&c, "agent-n", "sess-n").unwrap();
        assert!(!topics.is_empty());
        let t = &topics[0];
        // date_from/date_to should be None when all logs have empty created_at
        assert_eq!(t.date_from, None);
        assert_eq!(t.date_to, None);
    }

    // ================================================================
    // #374: 何もしなかったハートビート（idle）を topic 化しない
    // ================================================================

    /// heartbeat セッションに任意の log_type / speaker の行を投入するヘルパー。
    fn insert_hb_row(
        conn: &rusqlite::Connection,
        agent_id: &str,
        session_id: &str,
        log_type: &str,
        speaker: &str,
        content: &str,
    ) {
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: log_type.to_string(),
            content: content.to_string(),
            speaker_id: Some(speaker.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(conn, &log).unwrap();
    }

    /// heartbeat セッションに agent の speech 行を投入するヘルパー。
    fn insert_heartbeat_speech(
        conn: &rusqlite::Connection,
        agent_id: &str,
        session_id: &str,
        content: &str,
    ) {
        insert_hb_row(conn, agent_id, session_id, "speech", agent_id, content);
    }

    /// 毎 tick 注入される heartbeat のプロンプト scaffolding 行を投入するヘルパー。
    fn insert_heartbeat_prompt(conn: &rusqlite::Connection, agent_id: &str, session_id: &str) {
        insert_hb_row(
            conn,
            agent_id,
            session_id,
            "system",
            "heartbeat",
            "[ハートビート] 現在の会話「x」。出力形式: SPEAK/LEARN/IDLE のいずれか。",
        );
    }

    /// idle 判定が過去データ（旧 SPEAK/LEARN/IDLE 語彙）の分類と一致することを直接確認する。
    #[test]
    fn test_is_idle_heartbeat_speech_classification() {
        // speaker はデフォルトで自分（"a"）。
        let mk = |content: &str, log_type: &str| opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "a".to_string(),
            session_id: "heartbeat-a-c".to_string(),
            log_type: log_type.to_string(),
            content: content.to_string(),
            speaker_id: Some("a".to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        // SPEAK: も LEARN も無い → idle
        assert!(is_idle_heartbeat_speech(&mk("IDLE", "speech"), "a"));
        assert!(is_idle_heartbeat_speech(&mk("  IDLE  ", "speech"), "a"));
        // SPEAK: の後に非空 → 実あり
        assert!(!is_idle_heartbeat_speech(
            &mk("SPEAK: hello", "speech"),
            "a"
        ));
        assert!(!is_idle_heartbeat_speech(
            &mk("考えた結果\nSPEAK: みんなおはよう", "speech"),
            "a"
        ));
        // SPEAK: が空 → idle（main.rs と同基準）
        assert!(is_idle_heartbeat_speech(&mk("SPEAK:", "speech"), "a"));
        assert!(is_idle_heartbeat_speech(&mk("SPEAK:   ", "speech"), "a"));
        // LEARN を含む → 実あり
        assert!(!is_idle_heartbeat_speech(&mk("LEARN", "speech"), "a"));
        assert!(!is_idle_heartbeat_speech(
            &mk("learn something new", "speech"),
            "a"
        ));
        // speech 以外の log_type は常に実あり扱い（heartbeat の応答は speech のみ）
        assert!(!is_idle_heartbeat_speech(&mk("IDLE", "system"), "a"));
        // 話者ガード: 他者の発言は本文が idle 風でも idle 扱いにしない（材料に残す）
        assert!(!is_idle_heartbeat_speech(
            &mk("IDLE", "speech"),
            "other-agent"
        ));
    }

    /// #517: #515 以降、IDLE の記録は「`IDLE: <なぜ見送ったか>`」と本人の言葉の理由を持つ。
    /// 理由つきは材料として意味があるので**索引に残す**。無内容の裸マーカーだけを除外する。
    /// サンプルは本番 DB（`?mode=ro`）の実データから採取。
    #[test]
    fn idle_speech_keeps_reasoned_records_517() {
        let mk = |content: &str| opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "a".to_string(),
            session_id: "heartbeat-a-c".to_string(),
            log_type: "speech".to_string(),
            content: content.to_string(),
            speaker_id: Some("a".to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };

        // 無内容の裸マーカー・空文字は従来どおり idle（除外）— 回帰ガード。
        assert!(is_idle_heartbeat_speech(&mk("IDLE"), "a"));
        assert!(is_idle_heartbeat_speech(&mk("  IDLE  "), "a"));
        assert!(is_idle_heartbeat_speech(&mk("IDLE:"), "a"));
        assert!(is_idle_heartbeat_speech(&mk("IDLE:   "), "a"));
        assert!(
            is_idle_heartbeat_speech(&mk("NO_REPLY"), "a"),
            "裸の決定マーカーは除外"
        );
        assert!(is_idle_heartbeat_speech(&mk(""), "a"), "空文字は除外");

        // #515 の理由つき IDLE（本番実サンプル）は実ありとして残す。
        for s in [
            "IDLE: 自分自身を直す開発ツールの再帰ネタで、TLへ自然に混ざった。",
            "IDLE: まだ30分経っていない。静かに待つ。",
            "IDLE: TLの新着を絞り込み中。",
            // 改行後に本文が続く形（マーカー行の後に散文）も残す。
            "IDLE\n\n特に話題もないしのんびりしてるよ〜☀️",
        ] {
            assert!(
                !is_idle_heartbeat_speech(&mk(s), "a"),
                "理由つき IDLE を落としている: {s}"
            );
        }

        // マーカー無しの散文（本番実サンプル）も中身があるので残す。
        assert!(!is_idle_heartbeat_speech(&mk("確認中だよ〜。"), "a"));

        // SPEAK/LEARN の扱いは不変。
        assert!(!is_idle_heartbeat_speech(&mk("SPEAK: おはよう"), "a"));
        assert!(is_idle_heartbeat_speech(&mk("SPEAK:"), "a"));
        assert!(!is_idle_heartbeat_speech(&mk("LEARN"), "a"));
    }

    /// #517: 理由判定ヘルパの単体。先頭全大文字マーカー＋任意の `:` を剥いだ残りの有無で決める。
    #[test]
    fn idle_decision_has_no_reason_unit_517() {
        // 裸マーカー / 空 → 理由なし（true）。
        assert!(idle_decision_has_no_reason("IDLE"));
        assert!(idle_decision_has_no_reason("IDLE:"));
        assert!(idle_decision_has_no_reason("NO_REPLY"));
        assert!(idle_decision_has_no_reason("  IDLE  "));
        assert!(idle_decision_has_no_reason(""));
        // 理由つき / 散文 → 理由あり（false）。
        assert!(!idle_decision_has_no_reason("IDLE: 理由"));
        assert!(!idle_decision_has_no_reason("IDLE:理由"));
        assert!(!idle_decision_has_no_reason("IDLE\n\n本文"));
        assert!(!idle_decision_has_no_reason("確認中だよ〜。"));
    }

    /// is_heartbeat_noise: プロンプト scaffolding と idle speech を除き、
    /// tool/inner_voice/実ある speech は残す。
    #[test]
    fn test_is_heartbeat_noise_classification() {
        let mk =
            |content: &str, log_type: &str, speaker: &str| opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "a".to_string(),
                session_id: "heartbeat-a-c".to_string(),
                log_type: log_type.to_string(),
                content: content.to_string(),
                speaker_id: Some(speaker.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            };
        // 毎tick のプロンプト scaffolding（system + speaker=heartbeat）→ ノイズ
        assert!(is_heartbeat_noise(
            &mk("[ハートビート] ...", "system", "heartbeat"),
            "a"
        ));
        // idle speech（自分）→ ノイズ
        assert!(is_heartbeat_noise(&mk("IDLE", "speech", "a"), "a"));
        // 実のある speech → 残す
        assert!(!is_heartbeat_noise(&mk("SPEAK: hi", "speech", "a"), "a"));
        assert!(!is_heartbeat_noise(&mk("LEARN x", "speech", "a"), "a"));
        // tool / inner_voice は実活動 → 残す
        assert!(!is_heartbeat_noise(&mk("call foo", "tool_call", "a"), "a"));
        assert!(!is_heartbeat_noise(&mk("result", "tool_result", "a"), "a"));
        assert!(!is_heartbeat_noise(
            &mk("考えている", "inner_voice", "a"),
            "a"
        ));
        // system でも speaker が heartbeat 以外なら残す
        assert!(!is_heartbeat_noise(&mk("なにか", "system", "a"), "a"));
        // 他者の発言は本文が idle 風でも残す（相手の言葉を落とさない）
        assert!(!is_heartbeat_noise(&mk("IDLE", "speech", "other"), "a"));
    }

    /// 純idle だけの heartbeat グループ（毎tick のプロンプト + idle speech のみ）
    /// → topic を作らず、watermark は前進する。
    #[tokio::test]
    async fn test_heartbeat_idle_only_no_topic_but_watermark_advances() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let sid = "heartbeat-agent-1-chan-1";
        // 3 tick 分。各 tick は「プロンプト行 + idle の応答」の 2 行。
        for _ in 0..3 {
            insert_heartbeat_prompt(&db_conn, "agent-1", sid);
            insert_heartbeat_speech(&db_conn, "agent-1", sid, "IDLE");
        }
        let conn = opencrab_db::Db::from_connection(db_conn);

        let result = IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();
        // 6 行は処理対象として消費される（logs_indexed は取得件数）
        assert_eq!(result.logs_indexed, 6);

        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            // 実質行が無いので topic も session も作られない（root のみ）
            assert!(
                tree.iter().all(|n| n.node_type != "topic"),
                "純idle なら topic は作られない"
            );
            assert!(
                tree.iter().all(|n| n.node_type != "session"),
                "純idle なら session ノードも作られない"
            );
            // watermark は最終ログ ID まで前進している
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(
                wm.last_indexed_log_id, 6,
                "topic を作らなくても watermark は前進する"
            );
        }

        // 再ビルドで同じログを取り直さない（無限ループにならない）
        let r2 = IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();
        assert_eq!(r2.logs_indexed, 0, "watermark 前進により再取得しない");
    }

    /// decision が idle でも tool/inner_voice の実活動があれば topic は作られる。
    #[tokio::test]
    async fn test_heartbeat_idle_decision_with_tool_activity_creates_topic() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let sid = "heartbeat-agent-1-chan-1";
        insert_heartbeat_prompt(&db_conn, "agent-1", sid);
        insert_hb_row(
            &db_conn,
            "agent-1",
            sid,
            "inner_voice",
            "agent-1",
            "状況を確認しよう",
        );
        insert_hb_row(
            &db_conn,
            "agent-1",
            sid,
            "tool_call",
            "agent-1",
            "search(x)",
        );
        insert_hb_row(
            &db_conn,
            "agent-1",
            sid,
            "tool_result",
            "agent-1",
            "found y",
        );
        insert_heartbeat_speech(&db_conn, "agent-1", sid, "IDLE");
        let conn = opencrab_db::Db::from_connection(db_conn);

        IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        assert!(
            tree.iter().any(|n| n.node_type == "topic"),
            "tool/inner_voice の実活動があれば topic は作られる"
        );
    }

    /// 話者ガード: heartbeat セッションに他者の発言が混ざっていたら、自分は idle でも
    /// topic を作り、他者の言葉を要約材料に残す（相手の言葉を落とさない）。
    #[tokio::test]
    async fn test_heartbeat_others_speech_is_kept() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let sid = "heartbeat-agent-1-chan-1";
        // 自分は静観、しかし相手が話しかけてきた（本文は SPEAK:/LEARN を含まない実質発言）
        insert_heartbeat_prompt(&db_conn, "agent-1", sid);
        insert_heartbeat_speech(&db_conn, "agent-1", sid, "IDLE");
        insert_hb_row(
            &db_conn,
            "agent-1",
            sid,
            "speech",
            "someone-else",
            "ねえ、これどう思う？相手からの実質発言marker",
        );
        let conn = opencrab_db::Db::from_connection(db_conn);

        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };
        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "m", 50, "", None)
            .await
            .unwrap();

        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            assert!(
                tree.iter().any(|n| n.node_type == "topic"),
                "他者の発言があれば topic は作られる"
            );
        }
        // 他者の発言は要約材料に含まれ、自分の idle 行は除かれる
        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = request.messages[1].text_content().unwrap_or("").to_string();
        assert!(
            prompt.contains("相手からの実質発言marker"),
            "他者の発言は材料に残る"
        );
    }

    /// idle と実のある tick が混在する heartbeat グループ → topic は作られ、
    /// 要約の材料から idle 行が除かれている。被覆範囲は全ログを跨ぐ。
    #[tokio::test]
    async fn test_heartbeat_mixed_creates_topic_excluding_idle_material() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let sid = "heartbeat-agent-1-chan-1";
        // #517: idle 行は実在形の裸マーカー（`IDLE`）を使う。以前は "IDLE静観marker" と
        // マーカーに内容を直結した合成文字列だったが、#517 で「中身があるか」判定に変えた
        // 結果その形は理由つき扱いで残る（意図どおり）。無内容の裸 idle が材料から除かれる
        // ことをここで担保する（理由つき idle を残すことは unit テストが担保）。
        insert_heartbeat_speech(&db_conn, "agent-1", sid, "IDLE");
        insert_heartbeat_speech(&db_conn, "agent-1", sid, "SPEAK: 実のある発言unique");
        insert_heartbeat_speech(&db_conn, "agent-1", sid, "IDLE");
        let conn = opencrab_db::Db::from_connection(db_conn);

        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };
        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "m", 50, "", None)
            .await
            .unwrap();

        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            let topic = tree
                .iter()
                .find(|n| n.node_type == "topic")
                .expect("実のある行があれば topic は作られる");
            // 被覆範囲は idle を含む全ログを跨ぐ（watermark/カバレッジ維持）
            assert_eq!(topic.start_log_id, Some(1));
            assert_eq!(topic.end_log_id, Some(3));
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(wm.last_indexed_log_id, 3);
        }

        // LLM に渡した要約材料から idle 行が除かれ、実のある行だけが含まれる
        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = request.messages[1].text_content().unwrap_or("").to_string();
        assert!(
            prompt.contains("実のある発言unique"),
            "実のある行は材料に含まれる"
        );
        assert!(
            !prompt.contains("IDLE"),
            "無内容の裸 idle 行は要約材料から除かれる"
        );
    }

    /// heartbeat の LEARN 行は実ありとして残り、topic が作られる。
    #[tokio::test]
    async fn test_heartbeat_learn_row_is_substantive() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let sid = "heartbeat-agent-1-chan-1";
        insert_heartbeat_speech(&db_conn, "agent-1", sid, "IDLE");
        insert_heartbeat_speech(&db_conn, "agent-1", sid, "LEARN 新しい知見を得た");
        let conn = opencrab_db::Db::from_connection(db_conn);

        IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        assert!(
            tree.iter().any(|n| n.node_type == "topic"),
            "LEARN 行があれば topic は作られる"
        );
    }

    /// 通常（非 heartbeat）セッションは idle フィルタの対象外。本文が "IDLE" でも
    /// 従来どおり topic が作られる。
    #[tokio::test]
    async fn test_non_heartbeat_session_bare_idle_marker_is_filtered_573() {
        // #573 Stage A: idle ノイズ除外は接頭辞ゲートを外し全セッションへ適用する。
        // 通常セッション（`heartbeat-` で始まらない）でも、中身の無い裸マーカーだけの
        // バッチは topic を作らない（実会話セッションの裸 `NO_REPLY` が材料を汚さない）。
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_heartbeat_speech(&db_conn, "agent-1", "discord-agent-1-g-c", "NO_REPLY");
        let conn = opencrab_db::Db::from_connection(db_conn);

        IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        assert!(
            !tree.iter().any(|n| n.node_type == "topic"),
            "通常セッションでも裸マーカーのみのバッチは topic を作らない"
        );
    }

    /// #573 Stage A: 接頭辞ゲートを外しても**中身のある発話は落とさない**（過剰フィルタ
    /// でないことの包含確認）。通常セッションに実のある speech があれば topic が作られる。
    #[tokio::test]
    async fn test_non_heartbeat_session_substantive_speech_still_indexed_573() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_heartbeat_speech(
            &db_conn,
            "agent-1",
            "discord-agent-1-g-c",
            "IDLE: 相手が寝る前の挨拶をしていたので静かに見送った。",
        );
        let conn = opencrab_db::Db::from_connection(db_conn);

        IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        assert!(
            tree.iter().any(|n| n.node_type == "topic"),
            "中身のある IDLE 理由（#517）は通常セッションでも材料に残り topic 化する"
        );
    }

    /// #425 表示専用エコー行を投入するヘルパー。実会話（discord）セッションに、
    /// 印つき speech として入れる。
    fn insert_echo(conn: &rusqlite::Connection, agent_id: &str, session_id: &str, content: &str) {
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "speech".to_string(),
            content: content.to_string(),
            speaker_id: Some(agent_id.to_string()),
            turn_number: None,
            metadata_json: Some(opencrab_db::queries::HEARTBEAT_CHANNEL_ECHO_METADATA.to_string()),
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(conn, &log).unwrap();
    }

    /// #425: エコー行だけのバッチ（実会話セッションに印つき行しか無い）→ topic を作らず、
    /// watermark は前進する。エコー行が「永遠に未索引」で残ってバッチを詰まらせない
    /// （#416 と同族の「無言で進まない」を作らない）。再ビルドで同じ行を取り直さない。
    #[tokio::test]
    async fn test_heartbeat_echo_only_no_topic_but_watermark_advances() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let sid = "discord-agent-1-111-222";
        insert_echo(&db_conn, "agent-1", sid, "エコー発話1");
        insert_echo(&db_conn, "agent-1", sid, "エコー発話2");
        insert_echo(&db_conn, "agent-1", sid, "エコー発話3");
        let conn = opencrab_db::Db::from_connection(db_conn);

        let result = IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();
        // 3 行は取得（消費）される。
        assert_eq!(result.logs_indexed, 3);

        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            assert!(
                tree.iter().all(|n| n.node_type != "topic"),
                "エコーだけなら topic は作られない（記憶材料に入れない）"
            );
            assert!(
                tree.iter().all(|n| n.node_type != "session"),
                "エコーだけなら session ノードも作られない"
            );
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(
                wm.last_indexed_log_id, 3,
                "topic を作らなくても watermark は前進する（バッチが詰まらない）"
            );
        }

        // 再ビルドで同じログを取り直さない（無限ループにならない）。
        let r2 = IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();
        assert_eq!(r2.logs_indexed, 0, "watermark 前進により再取得しない");
    }

    /// #425: 実会話セッションに実発言とエコーが混在 → topic は作られるが、要約材料には
    /// エコー行が入らない（記憶索引・宣言材料はこの PR の前後で不変）。被覆範囲は
    /// エコーを含む全ログを跨ぐ（watermark 維持）。
    #[tokio::test]
    async fn test_heartbeat_echo_excluded_from_index_material() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let sid = "discord-agent-1-111-222";
        // 他者の実発言（材料に残る）→ 本人の HB エコー（材料から除く）。
        insert_hb_row(
            &db_conn,
            "agent-1",
            sid,
            "speech",
            "someone-else",
            "他者の実発言substantivemarker",
        );
        insert_echo(&db_conn, "agent-1", sid, "本人のHBエコーechomarker");
        let conn = opencrab_db::Db::from_connection(db_conn);

        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };
        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "m", 50, "", None)
            .await
            .unwrap();

        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            let topic = tree
                .iter()
                .find(|n| n.node_type == "topic")
                .expect("他者の実発言があれば topic は作られる");
            // 被覆範囲はエコーを含む全ログを跨ぐ（カバレッジ・watermark 維持）。
            assert_eq!(topic.start_log_id, Some(1));
            assert_eq!(topic.end_log_id, Some(2));
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(wm.last_indexed_log_id, 2);
        }

        // LLM に渡した要約材料からエコー行が除かれ、他者の実発言だけが含まれる。
        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = request.messages[1].text_content().unwrap_or("").to_string();
        assert!(
            prompt.contains("substantivemarker"),
            "他者の実発言は材料に残る"
        );
        assert!(
            !prompt.contains("echomarker"),
            "エコー行は要約材料から除かれる（記憶索引に入れない）"
        );
    }
}
