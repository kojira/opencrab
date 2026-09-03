use super::*;

/// 未インデックスのログが閾値を超えていたら、バックグラウンドでメモリインデックスを
/// 構築する（#33: 段の分解。run の応答は待たせない）。
/// スキル名が応答本文で言及される最低文字数。短い名前は他語の部分一致で
/// 誤カウントしやすいので閾値でノイズを抑える。
const MIN_SKILL_NAME_LEN_FOR_MATCH: usize = 4;

/// 応答本文に skill 名が現れているか（大文字小文字無視の部分一致）。
/// `response_lower` は呼び出し側で小文字化済みを渡す。ツール名ベースの
/// 確実な信号が server 経路に無いため、これが「実際に使った」の実用的な検出。
fn skill_mentioned(response_lower: &str, skill_name: &str) -> bool {
    let name = skill_name.trim().to_lowercase();
    name.chars().count() >= MIN_SKILL_NAME_LEN_FOR_MATCH && response_lower.contains(&name)
}

/// depth 0 の run 完了時、応答で言及された有効スキルの利用回数を +1 する。
/// 「実際に使った時だけ」カウントするための best-effort（名前言及ベース）。
pub(super) fn record_used_skills(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
    response: &str,
) {
    if response.trim().is_empty() {
        return;
    }
    let response_lower = response.to_lowercase();
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return,
    };
    let skills = opencrab_db::queries::list_skills(&conn, agent_id, true).unwrap_or_default();
    for s in &skills {
        if skill_mentioned(&response_lower, &s.name) {
            if let Err(e) = opencrab_db::queries::increment_skill_usage(&conn, &s.id) {
                tracing::warn!(skill = %s.name, error = %e, "failed to increment skill usage");
            }
            // スリープ棚卸しの弱い利用ヒント: セッション単位でも記録する（名前一致ベース）。
            if let Err(e) =
                opencrab_db::queries::insert_skill_usage(&conn, agent_id, &s.id, session_id)
            {
                tracing::warn!(skill = %s.name, error = %e, "failed to log skill usage session");
            }
        }
    }
}

pub(super) fn spawn_background_index_build(
    state: &AppState,
    agent_id: &str,
    effective_model: &str,
) {
    {
        let index_db = state.db.clone();
        let index_agent_id = agent_id.to_string();
        let index_llm_router = state.llm_router.clone();
        let index_model = effective_model.to_string();
        let inflight = state.index_build_inflight.clone();
        let (index_persona_name, index_personality) = {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::get_agent(&conn, &index_agent_id)
                .ok()
                .flatten()
                .map(|a| (a.persona_name, a.personality))
                .unwrap_or_default()
        };
        tokio::spawn(async move {
            let (unindexed, config) = {
                let Ok(conn) = index_db.lock() else { return };
                let unindexed =
                    opencrab_db::queries::get_unindexed_log_count(&conn, &index_agent_id)
                        .unwrap_or(0);
                let config = opencrab_db::queries::get_memory_index_config(&conn, &index_agent_id)
                    .unwrap_or_else(|_| opencrab_db::queries::AgentMemoryIndexConfig {
                        agent_id: index_agent_id.clone(),
                        batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
                        threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
                        updated_at: String::new(),
                    });
                (unindexed, config)
            };
            if unindexed < config.threshold {
                return;
            }
            // メンテナンスループとの二重ビルド防止（watermark 冪等が正しさの本線、
            // このフラグは同じバッチへの重複 LLM 支出を防ぐだけ）。
            let _guard = match crate::memory_maintenance::try_acquire_build_slot(
                &inflight,
                &index_agent_id,
            ) {
                Some(g) => g,
                None => {
                    tracing::debug!(agent_id = %index_agent_id, "index build already in flight; skipping post-run build");
                    return;
                }
            };
            tracing::info!(
                agent_id = %index_agent_id,
                unindexed = unindexed,
                threshold = config.threshold,
                batch_size = config.batch_size,
                "Starting background memory index build"
            );
            let llm_adapter = LlmRouterAdapter::new(index_llm_router);
            match opencrab_core::memory_index::IndexBuilder::build_incremental(
                &index_db,
                &index_agent_id,
                &llm_adapter,
                &index_model,
                config.batch_size as usize,
                &index_persona_name,
                index_personality.as_deref(),
            )
            .await
            {
                Ok(result) => {
                    tracing::info!(
                        agent_id = %index_agent_id,
                        nodes_created = result.nodes_created,
                        logs_indexed = result.logs_indexed,
                        "Background memory index build completed"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        agent_id = %index_agent_id,
                        error = %e,
                        "Background memory index build FAILED"
                    );
                }
            }
        });
    }
}

#[cfg(test)]
#[path = "tests/skill_mentioned.rs"]
mod skill_mentioned_tests;
