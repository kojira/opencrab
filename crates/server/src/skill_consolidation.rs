//! スリープ時スキル棚卸し（エージェント自己 curation ループ）。
//!
//! 設計: `docs/design-sleep-skill-consolidation.md`。
//!
//! per-skill の効果は計算しない。エージェント本人が直近の経験（メモリ索引の要約 +
//! セッション単位の verify 結末 + 弱い利用ヒント）を振り返り、自分の人格で
//! keep / retire / refine / create を判断する。反映は DB 直操作。スリープの内容は
//! 2層（`agent_logs` の構造化監査 + `llm_logs` の生プロンプト/生応答）で残す。

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::llm_adapter::LlmRouterAdapter;
use crate::AppState;
use opencrab_core::LlmClient;
use opencrab_llm_types::{ChatRequest, Message};

/// 本人が返す1スキルへの判断。
#[derive(Debug, Deserialize)]
struct CurationDecision {
    /// 対象スキル名（create の場合は新規名）。
    name: String,
    /// keep | retire | refine | create。未知値は keep 扱い。
    action: String,
    /// 本人の理由。
    #[serde(default)]
    reason: String,
    /// refine/create 時の新しい説明。
    #[serde(default)]
    description: Option<String>,
    /// refine/create 時の新しい行動指針。
    #[serde(default)]
    guidance: Option<String>,
}

/// 1エージェント分のスキル棚卸しを（トリガ条件を満たせば）実行する。
/// 戻り値: 棚卸しを実際に走らせたら true。
pub async fn maybe_run_skill_consolidation(
    state: &AppState,
    agent_id: &str,
) -> anyhow::Result<bool> {
    let cfg = state.skill_consolidation.clone();
    if !cfg.enabled {
        return Ok(false);
    }

    // --- トリガ判定 ---
    let now = Utc::now();
    let last_at = {
        let conn = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        opencrab_db::queries::get_last_skill_consolidation_at(&conn, agent_id)?
    };
    let Some(last_at) = last_at else {
        // 初回遭遇: 既存履歴を「新規活動」に数えないよう now をシードして終了。
        let conn = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        opencrab_db::queries::set_last_skill_consolidation_at(&conn, agent_id, &now.to_rfc3339())?;
        tracing::debug!(
            agent_id,
            "skill consolidation: seeded last_at (first encounter)"
        );
        return Ok(false);
    };

    let elapsed = last_at
        .parse::<DateTime<Utc>>()
        .map(|dt| now.signed_duration_since(dt))
        .unwrap_or_else(|_| Duration::zero());
    if elapsed < Duration::seconds(cfg.min_interval_secs.max(0)) {
        return Ok(false); // 最短間隔フロア
    }
    let activity = {
        let conn = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        opencrab_db::queries::count_active_sessions_since(&conn, agent_id, Some(&last_at))?
    };
    let time_cap_hit = elapsed >= Duration::hours(cfg.time_cap_hours.max(1));
    let fire = activity >= cfg.trigger_new_sessions || (time_cap_hit && activity >= 1);
    if !fire {
        return Ok(false);
    }

    // --- 排他（索引ビルドと衝突しない名前空間キー） ---
    let guard = crate::memory_maintenance::try_acquire_build_slot(
        &state.index_build_inflight,
        &format!("skillcuration:{agent_id}"),
    );
    let Some(_guard) = guard else {
        return Ok(false); // 既に走っている
    };

    // --- 素材の組立（DB読み） ---
    let packet = build_review_packet(state, agent_id, &cfg, &last_at)?;
    let model = packet.model.clone();
    let active_names = packet.active_names.clone();

    // --- 人格判断（LLM 1回） ---
    let system = match packet.personality.clone().filter(|p| !p.is_empty()) {
        Some(p) => format!("あなたは {} です。\n{p}", packet.persona_name),
        None => format!("あなたは {} です。", packet.persona_name),
    };
    let user = build_prompt(&packet);
    let request = ChatRequest::new(
        model.clone(),
        vec![Message::system(system.clone()), Message::user(user.clone())],
    )
    .with_temperature(0.3)
    .with_max_tokens(2000);

    let started = std::time::Instant::now();
    let response_text = match LlmRouterAdapter::new(state.llm_router.clone())
        .chat(request)
        .await
    {
        Ok(resp) => resp.first_text().unwrap_or_default().to_string(),
        Err(e) => {
            tracing::warn!(agent_id, error = %e, "skill consolidation LLM call failed");
            // 失敗も監査に残す（last_at だけ進むと DB から追えないため, レビュー#6）。
            let conn = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
            let _ = opencrab_db::queries::insert_agent_log(
                &conn,
                &opencrab_db::queries::AgentLogRow {
                    id: uuid::Uuid::new_v4().to_string(),
                    agent_id: Some(agent_id.to_string()),
                    level: "warn".to_string(),
                    context: "sleep".to_string(),
                    message: json!({ "trigger": "activity", "error": e.to_string() }).to_string(),
                    created_at: Some(now.to_rfc3339()),
                },
            );
            drop(conn);
            // 失敗しても last_at は進める（同じ活動で無限リトライしない）
            persist_last_at(state, agent_id, &now)?;
            return Ok(false);
        }
    };
    let latency_ms = started.elapsed().as_millis() as i64;

    // --- 層2: 生プロンプト/生応答を llm_logs に明示保存 ---
    let llm_log_id = uuid::Uuid::new_v4().to_string();
    {
        let conn = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        let row = opencrab_db::queries::LlmLogRow {
            id: llm_log_id.clone(),
            agent_id: agent_id.to_string(),
            session_id: None, // スリープに user session は無い
            model: Some(model.clone()),
            prompt: format!("[system]\n{system}\n\n[user]\n{user}"),
            response: response_text.clone(),
            tool_calls: None,
            latency_ms: Some(latency_ms),
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            error_code: None,
            error_body: None,
            requested_at: Some(now.to_rfc3339()),
            trigger_message_id: None,
            is_bot_iteration: false,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            created_at: now.to_rfc3339(),
        };
        if let Err(e) = opencrab_db::queries::insert_llm_log(&conn, &row) {
            tracing::warn!(agent_id, error = %e, "failed to persist skill consolidation llm_log");
        }
    }

    // --- 応答パース & 反映（DB 直操作） ---
    let decisions = parse_decisions(&response_text);
    let applied = apply_decisions(state, agent_id, &decisions, &active_names)?;

    // --- 層1: 構造化監査を agent_logs に保存 ---
    let audit = json!({
        "trigger": if activity >= cfg.trigger_new_sessions { "activity" } else { "time_cap" },
        "activity": activity,
        "skill_curation": applied,
        "cost": { "llm_calls": 1, "latency_ms": latency_ms },
        "llm_log_ids": [llm_log_id],
    });
    {
        let conn = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        let row = opencrab_db::queries::AgentLogRow {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: Some(agent_id.to_string()),
            level: "info".to_string(),
            context: "sleep".to_string(),
            message: audit.to_string(),
            created_at: Some(now.to_rfc3339()),
        };
        if let Err(e) = opencrab_db::queries::insert_agent_log(&conn, &row) {
            tracing::warn!(agent_id, error = %e, "failed to persist skill consolidation audit log");
        }
    }

    persist_last_at(state, agent_id, &now)?;
    tracing::info!(
        agent_id,
        decisions = decisions.len(),
        "skill consolidation ran"
    );
    Ok(true)
}

fn persist_last_at(state: &AppState, agent_id: &str, now: &DateTime<Utc>) -> anyhow::Result<()> {
    let conn = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
    opencrab_db::queries::set_last_skill_consolidation_at(&conn, agent_id, &now.to_rfc3339())?;
    Ok(())
}

/// 本人に渡す振り返り素材（＋実行に必要なエージェント情報）。
struct ReviewPacket {
    model: String,
    persona_name: String,
    personality: Option<String>,
    active_names: Vec<String>,
    skills: Vec<SkillMaterial>,
    memory_summary: Option<String>,
    recent_outcomes: Vec<String>,
}

struct SkillMaterial {
    name: String,
    description: String,
    guidance: String,
    archived: bool,
    used_recently: usize,
}

fn build_review_packet(
    state: &AppState,
    agent_id: &str,
    cfg: &crate::config::SkillConsolidationConfig,
    since: &str,
) -> anyhow::Result<ReviewPacket> {
    let conn = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("db lock: {e}"))?;

    let model =
        opencrab_db::queries::effective_model_for_agent(&conn, agent_id, &state.default_model)
            .unwrap_or_else(|_| state.default_model.clone());
    let (persona_name, personality) = opencrab_db::queries::get_agent(&conn, agent_id)
        .ok()
        .flatten()
        .map(|a| (a.persona_name, a.personality))
        .unwrap_or_else(|| (agent_id.to_string(), None));

    // アクティブ（非 archived）スキル + 再検討用の archived を数件
    let active = opencrab_db::queries::list_skills(&conn, agent_id, false).unwrap_or_default();
    let all = opencrab_db::queries::list_skills_filtered(&conn, agent_id, false, true)
        .unwrap_or_default();
    let archived: Vec<_> = all
        .into_iter()
        .filter(|s| s.archived)
        .take(cfg.include_archived_in_review.max(0) as usize)
        .collect();

    let mut skills = Vec::new();
    let mut active_names = Vec::new();
    for s in active.iter().chain(archived.iter()) {
        let used = opencrab_db::queries::list_skill_used_sessions(&conn, &s.id, Some(since))
            .map(|v| v.len())
            .unwrap_or(0);
        if !s.archived {
            active_names.push(s.name.clone());
        }
        skills.push(SkillMaterial {
            name: s.name.clone(),
            description: s.description.clone(),
            guidance: s.guidance.clone(),
            archived: s.archived,
            used_recently: used,
        });
    }

    let memory_summary =
        opencrab_core::memory_index::build_memory_index_section(&conn, agent_id, "")
            .ok()
            .flatten();
    let recent_outcomes: Vec<String> =
        opencrab_db::queries::list_recent_evaluations_by_agent(&conn, agent_id, 10)
            .unwrap_or_default()
            .into_iter()
            .map(|(_sid, content)| content)
            .collect();

    Ok(ReviewPacket {
        model,
        persona_name,
        personality,
        active_names,
        skills,
        memory_summary,
        recent_outcomes,
    })
}

fn build_prompt(packet: &ReviewPacket) -> String {
    let mut skills_txt = String::new();
    for s in &packet.skills {
        let tag = if s.archived { "[引退中] " } else { "" };
        skills_txt.push_str(&format!(
            "- {tag}{name}: {desc}\n  指針: {guidance}\n  （最近この名前が出た会話: {used}件）\n",
            name = s.name,
            desc = s.description,
            guidance = s.guidance.replace('\n', " "),
            used = s.used_recently,
        ));
    }
    let memory = packet
        .memory_summary
        .as_deref()
        .unwrap_or("(まだ十分な記憶の索引がありません)");
    let outcomes = if packet.recent_outcomes.is_empty() {
        "(結末シグナルなし)".to_string()
    } else {
        packet.recent_outcomes.join("\n---\n")
    };

    format!(
        "これはあなたのスリープ（内省）時間です。最近の自分の経験を振り返り、あなた自身のスキル棚を\
         あなたの人格・価値観で棚卸ししてください。正解や平均に合わせる必要はありません。あなたらしさを\
         育てる方向で、残す/引退させる/作り直す/新しく作る を決めてください。\n\n\
         # あなたのスキル一覧\n{skills_txt}\n\
         # 最近の記憶（会話・行動の要約）\n{memory}\n\n\
         # 最近のセッションの結末（評価）\n{outcomes}\n\n\
         上記を踏まえ、各スキルへの判断を JSON 配列で出力してください。数値の平均で機械的に切るのでは\
         なく、あなた自身が「効いた/自分に合う」と感じるかで判断してください。\n\
         形式: [{{\"name\":\"スキル名\",\"action\":\"keep|retire|refine|create\",\"reason\":\"理由\",\
         \"description\":\"refine/create時のみ\",\"guidance\":\"refine/create時のみ\"}}]\n\
         変更不要なものは keep。引退中のスキルを戻したい場合は refine で復活できます。JSON のみ出力。"
    )
}

fn parse_decisions(text: &str) -> Vec<CurationDecision> {
    let cleaned = opencrab_core::llm_text::strip_code_fences(text);
    serde_json::from_str::<Vec<CurationDecision>>(cleaned).unwrap_or_else(|_| {
        // オブジェクト単体や {"decisions":[...]} 形式も許容
        serde_json::from_str::<serde_json::Value>(cleaned)
            .ok()
            .and_then(|v| v.get("decisions").cloned())
            .and_then(|d| serde_json::from_value(d).ok())
            .unwrap_or_default()
    })
}

/// 判断を DB 直操作で反映し、監査用のサマリ（skill_curation 配列）を返す。
fn apply_decisions(
    state: &AppState,
    agent_id: &str,
    decisions: &[CurationDecision],
    active_names: &[String],
) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut summary = Vec::new();
    let conn = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
    let _ = active_names; // 将来の絞り込み用（現状は全判断を監査に残す）
    for d in decisions {
        let action = d.action.to_lowercase();
        let mut effective = "kept";
        let mut skill_id: Option<String> = None;
        match action.as_str() {
            "retire" => {
                if let Ok(Some(s)) =
                    opencrab_db::queries::find_skill_by_name(&conn, agent_id, &d.name)
                {
                    if opencrab_db::queries::archive_skill(&conn, &s.id, true).is_ok() {
                        effective = "retired";
                        skill_id = Some(s.id);
                    }
                }
            }
            "refine" => {
                // archived 含めて解決。既存 situation_pattern は保持（散文消去を避ける）。
                if let Ok(Some(mut s)) =
                    opencrab_db::queries::find_skill_by_name_any(&conn, agent_id, &d.name)
                {
                    if let Some(desc) = &d.description {
                        s.description = desc.clone();
                    }
                    if let Some(g) = &d.guidance {
                        s.guidance = g.clone();
                    }
                    s.archived = false; // refine は復活も兼ねる
                    s.is_active = true;
                    let sid = s.id.clone();
                    if opencrab_db::queries::update_skill(&conn, &s).is_ok() {
                        effective = "refined";
                        skill_id = Some(sid);
                    }
                }
            }
            "create" => {
                // 同名が無ければ新規（DB-only, situation_pattern="" で actions 誤解釈を回避）
                let exists = opencrab_db::queries::find_skill_by_name_any(&conn, agent_id, &d.name)
                    .ok()
                    .flatten()
                    .is_some();
                if !exists {
                    let new_id = uuid::Uuid::new_v4().to_string();
                    let row = opencrab_db::queries::SkillRow {
                        id: new_id.clone(),
                        agent_id: agent_id.to_string(),
                        name: d.name.clone(),
                        description: d.description.clone().unwrap_or_default(),
                        situation_pattern: String::new(),
                        guidance: d.guidance.clone().unwrap_or_default(),
                        source_type: "sleep_curated".to_string(),
                        source_context: None,
                        file_path: None,
                        effectiveness: None,
                        usage_count: 0,
                        is_active: true,
                        permission: "\"agent\"".to_string(),
                        archived: false,
                        // #335: スリープ棚卸しは caller=Owner のターンで走る。None = legacy
                        // grandfather（Owner 相当）。
                        created_caller: None,
                    };
                    if opencrab_db::queries::insert_skill(&conn, &row).is_ok() {
                        effective = "created";
                        skill_id = Some(new_id);
                    }
                }
            }
            _ => {} // keep / 未知 → 変更なし
        }
        summary.push(json!({
            "skill": d.name,
            "skill_id": skill_id,
            "action": effective,
            "reason": d.reason,
        }));
    }
    Ok(summary)
}
