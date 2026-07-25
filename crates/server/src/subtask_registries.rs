//! 経路横断で共有する subtask registry の保持（#169）。
//!
//! 非ブロック dispatch（RFC #152 S3a）は「dispatch した subtask を登録する registry」を
//! 必要とする。この registry は **`cancel_subtask`（#161）が引くものと同一 Arc** でなければ
//! ならない。使い捨ての `DashMap` を毎回作ると、走行中 subtask を追跡しているのに
//! `cancel_subtask` からは常に not found になり、停止できない。
//!
//! そこで登場するのがこの構造体で、キー（REST は session_id / heartbeat は agent_id）
//! ごとに registry を貸し出し、同じキーには**常に同じ Arc** を返す。`AppState` が
//! 1 つ保持し、REST ハンドラ（リクエスト毎に生成される）や heartbeat コールバック
//! （設定変更で作り直される）が跨っても registry が失われないようにする。
//!
//! エントリの GC は行わない。キーの母数は「エージェント × 会話相手」で有界であり、
//! 決着後は中身が空の `DashMap` が残るだけ（web gateway の registries も同様）。

use std::sync::Arc;

use dashmap::DashMap;

use opencrab_actions::SubtaskRegistry;

/// キー単位に `SubtaskRegistry` を貸し出す共有ストア。
///
/// キーの意味は経路が決める:
/// - REST（`POST /api/agents/{id}/messages`）: session_id（`agent-msg-{agent}-{user}`）。
///   1 セッションの走行中 subtask をまとめ、session の `status` 整合にも使う。
/// - heartbeat: agent_id。tick / チャンネルを跨いで同一 registry を共有し、
///   前 tick で dispatch した subtask を後続 tick の `cancel_subtask` から停止できる。
#[derive(Default)]
pub struct SubtaskRegistries {
    registries: DashMap<String, SubtaskRegistry>,
}

impl SubtaskRegistries {
    pub fn new() -> Self {
        Self::default()
    }

    /// キーの registry を返す（無ければ生成）。同じキーには常に同じ `Arc` を返すため、
    /// dispatcher（登録側）と `cancel_subtask`（停止側）が同一 registry を見る。
    pub fn registry_for(&self, key: &str) -> SubtaskRegistry {
        self.registries
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(DashMap::new()))
            .clone()
    }

    /// キーに走行中の subtask があるか（registry を新規生成しない読み取り）。
    ///
    /// `settle_completed` は sink 発火より前に registry から除去するため、これは
    /// 「まだ決着していない subtask が残っているか」を意味する。
    pub fn has_running(&self, key: &str) -> bool {
        self.registries
            .get(key)
            .map(|r| !r.is_empty())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同じキーには同一 Arc が返る（= dispatcher と cancel_subtask が同じ registry を見る）。
    #[tokio::test]
    async fn same_key_yields_same_registry() {
        let registries = SubtaskRegistries::new();
        let a = registries.registry_for("agent-msg-x-u1");
        let b = registries.registry_for("agent-msg-x-u1");
        assert!(
            Arc::ptr_eq(&a, &b),
            "同一キーでは同じ registry を共有しなければならない"
        );
        // 片方への登録が他方から見える。
        a.insert("st-1".to_string(), fake_subtask("agent-msg-x-u1"));
        assert!(b.contains_key("st-1"));
        b.get("st-1").unwrap().abort_handle.abort();
    }

    /// 異なるキーは独立した registry（他セッションの subtask が混ざらない）。
    #[tokio::test]
    async fn different_keys_are_isolated() {
        let registries = SubtaskRegistries::new();
        let a = registries.registry_for("agent-msg-x-u1");
        let b = registries.registry_for("agent-msg-x-u2");
        assert!(!Arc::ptr_eq(&a, &b));
        a.insert("st-1".to_string(), fake_subtask("agent-msg-x-u1"));
        assert!(!b.contains_key("st-1"));
        assert!(registries.has_running("agent-msg-x-u1"));
        assert!(!registries.has_running("agent-msg-x-u2"));
        a.get("st-1").unwrap().abort_handle.abort();
    }

    /// 未知のキーは registry を作らずに false（`status` 整合の読み取りが副作用を持たない）。
    #[test]
    fn has_running_is_false_for_unknown_key() {
        let registries = SubtaskRegistries::new();
        assert!(!registries.has_running("never-seen"));
        // 空 registry を貸しただけでは走行中にならない。
        let _ = registries.registry_for("seen");
        assert!(!registries.has_running("seen"));
    }

    /// 即完了しない fake subtask（`abort_handle` が有効なまま registry に残る）。
    fn fake_subtask(parent_session_id: &str) -> opencrab_actions::SpawnedSubtask {
        opencrab_actions::SpawnedSubtask {
            abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            session_id: "subtask-st-1".to_string(),
            parent_session_id: parent_session_id.to_string(),
            agent_id: "agent-x".to_string(),
            label: "job".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: None,
        }
    }
}
