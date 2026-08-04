//! メモリインデックスのアイドル時メンテナンスループ。
//!
//! 単一の常駐タスクが一定間隔で DB の**全エージェント**を巡回し、各エージェントに
//! ついて ①増分ビルド（アイドルゲート付き）②キーワードバックフィル（≤1 LLM コール）
//! ③月次ロールアップ（≤1 LLM コール）を行う。何もすることがない tick は SQL 数本で
//! 終わり、LLM コールはゼロ。
//!
//! ハートビートに紐づけないのは、heartbeat_enabled が既定 false かつチャンネル
//! ホワイトリスト依存のため（そこに載せると大半の構成でロールアップが一度も走らない）。
//! 全エージェントを毎 tick 列挙するのは、per-agent ゲートウェイのみのエージェントや
//! ランタイムに作成されたエージェントも漏らさないため。

use std::sync::Arc;

use dashmap::DashMap;

use crate::llm_adapter::LlmRouterAdapter;
use crate::AppState;

/// エージェント単位のインデックスビルド in-flight フラグ。
///
/// post-run の `spawn_background_index_build` とメンテナンス tick が同じバッチを
/// 同時にビルドして LLM 支出を二重にしないための排他。watermark の冪等性が
/// 正しさの本線であり、このフラグは費用最適化。
pub type IndexBuildInflight = Arc<DashMap<String, ()>>;

/// ビルドスロットの RAII ガード（drop で解放）。
pub struct BuildSlotGuard {
    inflight: IndexBuildInflight,
    agent_id: String,
}

impl Drop for BuildSlotGuard {
    fn drop(&mut self) {
        self.inflight.remove(&self.agent_id);
    }
}

/// ビルドスロットの取得を試みる。既に他方がビルド中なら None。
pub fn try_acquire_build_slot(
    inflight: &IndexBuildInflight,
    agent_id: &str,
) -> Option<BuildSlotGuard> {
    use dashmap::mapref::entry::Entry;
    match inflight.entry(agent_id.to_string()) {
        Entry::Occupied(_) => None,
        Entry::Vacant(v) => {
            v.insert(());
            Some(BuildSlotGuard {
                inflight: inflight.clone(),
                agent_id: agent_id.to_string(),
            })
        }
    }
}

/// 1 tick の実施内容（ログ/テスト用）。
#[derive(Debug, Default, PartialEq)]
pub struct MaintenanceReport {
    pub logs_indexed: usize,
    pub keywords_backfilled: usize,
    pub rolled_up_month: Option<String>,
    pub skill_consolidated: bool,
    /// カテゴリ層（#313）: 種まきで新設したカテゴリ数。
    pub categories_seeded: usize,
    /// カテゴリ層（#313）: 既存カテゴリへ割り当てた topic 数。
    pub topics_categorized: usize,
    /// 整理ラン（#313 段階3 / #361）を実際に起動したか。既定オフ・ゲート未達では false。
    pub organized: bool,
    /// 宣言ラン（#384 / #376 段階2）を実際に起動したか。既定オフ・ゲート未達では false。
    pub declared: bool,
}

impl MaintenanceReport {
    pub fn did_anything(&self) -> bool {
        self.logs_indexed > 0
            || self.keywords_backfilled > 0
            || self.rolled_up_month.is_some()
            || self.skill_consolidated
            || self.categories_seeded > 0
            || self.topics_categorized > 0
            || self.organized
            || self.declared
    }
}

/// 増分ビルドのアイドルゲート: 最新の未処理ログがこの分数より古ければ
/// 「会話が一段落した」とみなし、閾値未満でもビルドしてよい。
const IDLE_GATE_MINUTES: i64 = 10;

/// メンテナンスループを起動する（プロセス生存期間の常駐タスク）。
pub fn spawn_memory_maintenance_loop(state: AppState, interval_secs: u64) {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(interval_secs.max(60));
        tracing::info!(
            interval_secs = interval.as_secs(),
            "memory maintenance loop started"
        );
        loop {
            tokio::time::sleep(interval).await;

            // 毎 tick 全エージェントを列挙（ランタイム作成にも追従）
            let agent_ids: Vec<String> = {
                let Ok(conn) = state.db.lock() else { continue };
                opencrab_db::queries::list_agent_ids(&conn).unwrap_or_default()
            };
            for agent_id in agent_ids {
                match run_maintenance_tick(&state, &agent_id).await {
                    Ok(report) if report.did_anything() => {
                        tracing::info!(
                            agent_id = %agent_id,
                            logs_indexed = report.logs_indexed,
                            keywords_backfilled = report.keywords_backfilled,
                            rolled_up_month = ?report.rolled_up_month,
                            categories_seeded = report.categories_seeded,
                            topics_categorized = report.topics_categorized,
                            organized = report.organized,
                            declared = report.declared,
                            "memory maintenance tick"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(agent_id = %agent_id, error = %e, "memory maintenance tick failed");
                    }
                }
            }
        }
    });
}

/// 1 エージェント分のメンテナンス tick。
///
/// LLM コール数の上限: 増分ビルド ≤ バッチ内セッション群数（post-run トリガーと
/// 同プロファイル）+ バックフィル 1 + ロールアップ 1。何も無ければゼロ。
pub async fn run_maintenance_tick(
    state: &AppState,
    agent_id: &str,
) -> anyhow::Result<MaintenanceReport> {
    let mut report = MaintenanceReport::default();

    let (effective_model, persona_name, personality, unindexed, newest_log_at, config) = {
        let conn = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
        let model =
            opencrab_db::queries::effective_model_for_agent(&conn, agent_id, &state.default_model)
                .unwrap_or_else(|_| state.default_model.clone());
        let (persona_name, personality) = opencrab_db::queries::get_agent(&conn, agent_id)
            .ok()
            .flatten()
            .map(|a| (a.persona_name, a.personality))
            .unwrap_or_default();
        let (unindexed, newest_log_at) =
            opencrab_db::queries::get_unindexed_stats(&conn, agent_id)?;
        let config =
            opencrab_db::queries::get_memory_index_config(&conn, agent_id).unwrap_or_else(|_| {
                opencrab_db::queries::AgentMemoryIndexConfig {
                    agent_id: agent_id.to_string(),
                    batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
                    threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
                    updated_at: String::new(),
                }
            });
        (
            model,
            persona_name,
            personality,
            unindexed,
            newest_log_at,
            config,
        )
    };
    let llm = LlmRouterAdapter::new(state.llm_router.clone());

    // ① 増分ビルド。閾値未満でも「最新の未処理ログが十分古い = 会話が一段落」なら
    // 拾う（post-run トリガーは閾値未満の残りを永遠に取りこぼすため）。
    let idle = newest_log_at
        .as_deref()
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .map(|t| chrono::Utc::now().signed_duration_since(t).num_minutes() >= IDLE_GATE_MINUTES)
        .unwrap_or(false);
    if unindexed > 0 && (unindexed >= config.threshold || idle) {
        if let Some(_guard) = try_acquire_build_slot(&state.index_build_inflight, agent_id) {
            match opencrab_core::memory_index::IndexBuilder::build_incremental(
                &state.db,
                agent_id,
                &llm,
                &effective_model,
                config.batch_size as usize,
                &persona_name,
                personality.as_deref(),
            )
            .await
            {
                Ok(r) => report.logs_indexed = r.logs_indexed,
                Err(e) => {
                    tracing::warn!(agent_id = %agent_id, error = %e, "maintenance incremental build failed");
                }
            }
        }
    }

    // ② キーワードバックフィル（≤1 コール、対象が無ければゼロコール）。
    // キーワード抽出も人格を通す（方針: 人格のベクトルを最大限反映する）。
    match opencrab_core::memory_index::maintenance::backfill_topic_keywords(
        &state.db,
        agent_id,
        &llm,
        &effective_model,
        &persona_name,
        personality.as_deref(),
    )
    .await
    {
        Ok(n) => report.keywords_backfilled = n,
        Err(e) => {
            tracing::warn!(agent_id = %agent_id, error = %e, "keyword backfill failed");
        }
    }

    // ③ 月次ロールアップ（≤1 コール、stale な過去月が無ければゼロコール）。
    // 生成された月次要約は [Memory Index] セクションの月行として会話に常時出る。
    match opencrab_core::memory_index::maintenance::rollup_stale_period(
        &state.db,
        agent_id,
        &llm,
        &effective_model,
        &persona_name,
        personality.as_deref(),
    )
    .await
    {
        Ok(month) => report.rolled_up_month = month,
        Err(e) => {
            tracing::warn!(agent_id = %agent_id, error = %e, "monthly rollup failed");
        }
    }

    // ④ カテゴリ層（issue #313）: 種まき（LLM ゼロコール）＋ 未分類 topic の割当（≤1 コール）。
    // sleep 中にのみ整理する（対話ターンでは走らせない = #291 の再来を避ける）。既存の
    // rollup と同じく「1 tick 1 LLM コール / ロックを await 跨ぎで保持しない / sticky で冪等」。
    // category/meta ノードは同一テーブルなので browse/search/retrieve から能動的に引ける。
    //
    // #345: #313 の方針が「エージェント自身に整理させる（一期一会）」へ変わり、いまの
    // 単一ラベル・sticky・12件ずつの割当は作り直しになるため、作り直す前提の処理へ LLM
    // 費用を払い続けないよう、config で丸ごと止められるようにする（既定オフ）。
    // `skill_consolidation` と同じく LLM を消費する自律処理なので同じ opt-in の流儀に揃える。
    // 機能そのものは #313 で作り直す際に参照するので残す（既存データ・スキーマは触らない）。
    if state.category_maintenance.enabled {
        match opencrab_core::memory_index::category::maintain_categories(
            &state.db,
            agent_id,
            &llm,
            &effective_model,
            &persona_name,
            personality.as_deref(),
        )
        .await
        {
            Ok((seeded, assigned)) => {
                report.categories_seeded = seeded;
                report.topics_categorized = assigned;
            }
            Err(e) => {
                tracing::warn!(agent_id = %agent_id, error = %e, "category maintenance failed");
            }
        }
    }

    // ⑤ スキル棚卸し（自己 curation）。メモリ統合の後に走らせる（設計: 統合→振り返り）。
    // 既定は無効（config skill_consolidation.enabled）。トリガ未達なら即 return。
    match crate::skill_consolidation::maybe_run_skill_consolidation(state, agent_id).await {
        Ok(ran) => report.skill_consolidated = ran,
        Err(e) => {
            tracing::warn!(agent_id = %agent_id, error = %e, "skill consolidation failed");
        }
    }

    // ⑥ 宣言ラン（#384 / #376 段階2）。本人が別セッション・本人の人格で自分の生ログを俯瞰し、
    // 「どこからどこまでが一つの記憶か」を宣言する。宣言（何が 1 つの記憶か）はタグ付け（どう
    // 分類するか）の一段下＝先なので、⑦のタグ整理ランより**前**に置く（設計 #376: 宣言 → タグ）。
    // **既定オフ**（config memory_declare.enabled）。ゲート未達ならゼロコールで即 return。
    // 対話ターンでは走らせない（呼び出し元はこの sleep ループのみ / #291）。
    match crate::memory_declare::maybe_run_memory_declare(state, agent_id).await {
        Ok(ran) => report.declared = ran,
        Err(e) => {
            tracing::warn!(agent_id = %agent_id, error = %e, "memory declare failed");
        }
    }

    // ⑦ エージェンティック整理ラン（#313 段階3 / #361）。①〜③で索引を確定させた**後**に、
    // 本人が別セッション・本人の人格で自分の記憶（topic）にタグを付けて整理する。
    // **既定オフ**（config memory_organize.enabled）。ゲート（日次 + 下限）未達なら
    // ゼロコールで即 return。対話ターンでは走らせない（呼び出し元はこの sleep ループのみ / #291）。
    match crate::memory_organize::maybe_run_memory_organize(state, agent_id).await {
        Ok(ran) => report.organized = ran,
        Err(e) => {
            tracing::warn!(agent_id = %agent_id, error = %e, "memory organize failed");
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_slot_is_exclusive_and_released_on_drop() {
        let inflight: IndexBuildInflight = Arc::new(DashMap::new());
        let g1 = try_acquire_build_slot(&inflight, "a1");
        assert!(g1.is_some());
        // 同一エージェントの二重取得は不可、他エージェントは可
        assert!(try_acquire_build_slot(&inflight, "a1").is_none());
        assert!(try_acquire_build_slot(&inflight, "a2").is_some());
        drop(g1);
        // drop で解放される
        assert!(try_acquire_build_slot(&inflight, "a1").is_some());
    }
}
