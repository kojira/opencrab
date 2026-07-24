//! Gateway 非依存の subtask 抽象（RFC #152 案A / S0）。
//!
//! この段（S0）では「まだ誰も使わない」抽象を追加するだけであり、既存の
//! Discord 実装（`crates/discord` の `SpawnedSubtask` / `SubtaskRegistry` /
//! `LoopEvent` 経由の完了通知）とは配線しない。S1 で Discord 側をこの抽象へ
//! 載せ替える。
//!
//! 設計の核（RFC §1.3・§3.1）:
//! - 完了通知に本文（result）は運搬しない。本文は既に session_logs（DB）へ
//!   永続化済みで、再注入は `build_conversation_string` が DB から会話を
//!   再構築する。sink に必要なのは「親セッションのエージェントを resume せよ」
//!   という**軽量トリガ**だけ。
//! - Discord 固有型（`WebhookConfig` / `DeliveryBatch` / serenity 等）は
//!   ここには一切入れない。webhook 系は S1 で discord 側の随伴構造へ分離する。

use std::sync::Arc;

use dashmap::DashMap;
use tokio::task::AbortHandle;

/// subtask が settle（決着）したときの種別。
///
/// progress の二重定義を避け、完了と進捗の責務を `exit_reason` 文字列ではなく
/// 型で分ける（RFC レビュー指摘 P2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleKind {
    /// subtask 本体が終了した（completed / error / timeout / stopped_by_limit 等、
    /// 詳細は `SubtaskSettled::exit_reason`）。
    Completed,
    /// 走行中の中間進捗通知。
    Progress,
}

/// subtask の settle を親セッションへ通知するための最小ペイロード。
///
/// 本文（result）は運搬しない（RFC §1.3）。返信ルーティング（`reply_target`）も
/// 載せない — ランタイムが settle 時に registry から引いて sink へ渡す
/// （RFC §3.1(4)）。ここには resume 判断に要る最小情報だけを持つ。
#[derive(Debug, Clone)]
pub struct SubtaskSettled {
    /// 親セッション ID（resume 対象）。
    pub session_id: String,
    /// 親セッションのエージェント ID。
    pub agent_id: String,
    /// settle した subtask の ID。
    pub subtask_id: String,
    /// 決着理由（completed / error / timeout / stopped_by_limit など）。
    /// 種別は `kind` が持つ（progress の二重定義回避）。
    pub exit_reason: String,
    /// 決着の種別（完了 or 進捗）。
    pub kind: SettleKind,
}

/// subtask 完了通知の抽象（`LoopEvent` 直依存を置換する）。
///
/// ランタイムは `Arc<dyn SubtaskCompletionSink>` を保持し、**DB 永続化の後に**
/// `on_subtask_settled` を呼ぶだけで、`LoopEvent` を知らない。sink 実装が
/// 「resume ＋ その gateway の配送口」を担う（Discord=`send_to_channel` /
/// Nostr=`reply` / REST=保存して取得 / heartbeat=次 tick 拾い or 保存）。
pub trait SubtaskCompletionSink: Send + Sync {
    /// 親セッションのエージェントを resume して subtask 結果を会話へ再注入する
    /// トリガ。本文は DB 永続化済みのため運搬しない（RFC §1.3）。
    fn on_subtask_settled(&self, ev: SubtaskSettled);
}

/// registry が追跡する走行中 subtask のエントリ（gateway 非依存版）。
///
/// 既存 `opencrab_discord::SpawnedSubtask` と同型だが、Discord 固有の
/// webhook フィールド（`WebhookConfig` / `DeliveryBatch`）は持たない。
/// 返信ルーティングは gateway 不透明な `reply_target` として spawn 時に捕捉する
/// （RFC §3.1(4)、Nostr で session_id から導出できない問題への対処）。
#[derive(Clone)]
pub struct SpawnedSubtask {
    /// subtask 本体タスクの abort ハンドル（cancel / kill_on_drop 用）。
    pub abort_handle: AbortHandle,
    /// subtask 自身のセッション ID。
    pub session_id: String,
    /// この subtask を spawn した親セッション ID（resume 対象）。
    pub parent_session_id: String,
    /// 実行エージェント ID。
    pub agent_id: String,
    /// 人間可読ラベル（list / cancel での識別用）。
    pub label: String,
    /// 起動時刻（duration 算出用）。monotonic な `Instant` を用いる。
    pub started_at: std::time::Instant,
    /// gateway 不透明な返信ルーティング token（spawn 時に捕捉）。
    /// settle 時にランタイムが registry から引いて sink へ渡す。
    /// `None` なら返信配送しない。
    pub reply_target: Option<String>,
}

/// アクティブな subtask を subtask_id で引く registry（gateway 非依存版）。
///
/// 現 `opencrab_discord::SubtaskRegistry` と同型だが gateway 非依存。
pub type SubtaskRegistry = Arc<DashMap<String, SpawnedSubtask>>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `SubtaskCompletionSink` の最小フェイク実装。受け取った settle を記録する。
    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<SubtaskSettled>>,
    }

    impl SubtaskCompletionSink for RecordingSink {
        fn on_subtask_settled(&self, ev: SubtaskSettled) {
            self.events.lock().unwrap().push(ev);
        }
    }

    #[test]
    fn sink_receives_settled_event() {
        let sink: Arc<dyn SubtaskCompletionSink> = Arc::new(RecordingSink::default());
        sink.on_subtask_settled(SubtaskSettled {
            session_id: "discord-123".to_string(),
            agent_id: "agent-a".to_string(),
            subtask_id: "sub-1".to_string(),
            exit_reason: "completed".to_string(),
            kind: SettleKind::Completed,
        });

        // downcast せずに検証するため、具象型で1つ生成しても振る舞いを確認できる。
        let recording = RecordingSink::default();
        recording.on_subtask_settled(SubtaskSettled {
            session_id: "nostr-abc".to_string(),
            agent_id: "agent-b".to_string(),
            subtask_id: "sub-2".to_string(),
            exit_reason: "progress".to_string(),
            kind: SettleKind::Progress,
        });
        let events = recording.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].subtask_id, "sub-2");
        assert_eq!(events[0].kind, SettleKind::Progress);
    }

    #[tokio::test]
    async fn registry_holds_spawned_subtask() {
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let handle = tokio::spawn(async {
            // 即完了せず abort_handle を有効に保つ。
            std::future::pending::<()>().await;
        })
        .abort_handle();

        let entry = SpawnedSubtask {
            abort_handle: handle,
            session_id: "sub-session-1".to_string(),
            parent_session_id: "discord-123".to_string(),
            agent_id: "agent-a".to_string(),
            label: "compile the report".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: Some("channel:456".to_string()),
        };
        registry.insert("sub-1".to_string(), entry);

        assert_eq!(registry.len(), 1);
        let got = registry.get("sub-1").unwrap();
        assert_eq!(got.parent_session_id, "discord-123");
        assert_eq!(got.reply_target.as_deref(), Some("channel:456"));

        // abort して registry から除去（cancel 相当）。
        got.abort_handle.abort();
        drop(got);
        registry.remove("sub-1");
        assert!(registry.is_empty());
    }
}
