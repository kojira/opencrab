//! セッション単位の直列化ランタイム（#190 S1 / 方針は #191）。
//!
//! ゲートウェイ（web / Nostr）は「1 セッション = 1 会話」の単位で応答生成を直列化する
//! 必要がある。inbound の応答生成と subtask 完了 resume の応答生成が同一セッションで
//! 並行すると、両者が同じ会話履歴から独立に返信を組み立てて**二重投稿**になる
//! （RFC #152 §6 の不変条件違反）。
//!
//! この直列化は web（`WebGateway`）と Nostr（`NostrSessionRuntime`）で文字レベルまで
//! ほぼ同一のコードとして二重に実装されていた。差は可視性と、web が SSE 配信チャンネルを
//! 同じ構造体に同居させていた点だけだった。同じ不変条件を守るコードが 2 箇所にあると
//! 片方だけ直る（= 片方だけ壊れる）ため、ここ 1 つに寄せる。
//!
//! 併せて dispatch 用の登録簿（[`SubtaskRegistries`]）も保持する。inbound と resume が
//! **同一 Arc の registry** を共有することが `cancel_subtask`（#161）が走行中 subtask に
//! 到達できる条件であり、直列化と同じ「セッション単位の実行状態」だからである。
//!
//! ここに置く理由（依存方向）: 依存は server → nostr であり、Nostr 側から server の型は
//! 参照できない。registry 本体（`SubtaskRegistry`）と同じ gateway 非依存層に置くことで、
//! どのゲートウェイからも同じ 1 実装を使える。
//!
//! Discord（`spawn_serialized_on_session`）は「spawn 込みで戻り値なし」という別形のため
//! 今回は統合しない（別 issue）。

use std::future::Future;
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;

use crate::subtask::SubtaskRegistry;
use crate::subtask_registries::SubtaskRegistries;

/// セッション単位の直列化ロックと dispatch 登録簿を持つ共有ランタイム。
///
/// ゲートウェイはプロセス全体で 1 つ（`Arc<SessionRuntime>`）保持し、inbound 経路と
/// 完了 sink が同じ Arc を共有する。ロックの粒度は session_id なので、別の相手・別の
/// エージェント・別の会話は従来どおり並行する。
#[derive(Default)]
pub struct SessionRuntime {
    /// session_id → 直列化ロック。アイドルになったエントリは回収する（下記参照）。
    session_locks: DashMap<String, Arc<Mutex<()>>>,
    registries: SubtaskRegistries,
}

impl SessionRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// セッションの dispatch registry を取得する（無ければ生成し、inbound/resume で共有）。
    ///
    /// 同じ session_id には常に同じ `Arc` を返す。これが `cancel_subtask` の到達条件。
    pub fn registry_for(&self, session_id: &str) -> SubtaskRegistry {
        self.registries.registry_for(session_id)
    }

    /// セッションに走行中（未決着）の dispatch subtask があるか。
    ///
    /// `settle_completed` は sink 発火より前に registry から除去するため、決着後は false。
    pub fn has_running(&self, session_id: &str) -> bool {
        self.registries.has_running(session_id)
    }

    /// セッションのロックエントリが残っているか（回収の観測点）。
    ///
    /// 走行中は true、待機者がいなくなれば false。`session_locks` を private に保ったまま
    /// 「アイドルなロックが回収される」ことを外側の層からも検証できるようにするための
    /// 読み取り専用アクセサ（エントリを新規生成しない）。
    pub fn holds_lock_entry(&self, session_id: &str) -> bool {
        self.session_locks.contains_key(session_id)
    }

    fn lock_for(&self, session_id: &str) -> Arc<Mutex<()>> {
        self.session_locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// 待機者がいなければロックエントリを回収する（#39 相当のリーク防止）。
    ///
    /// マップ内の 1 本だけが残っている（`strong_count == 1`）なら誰も待っていない。
    /// `remove_if` はシャードロック下で判定するため、判定と削除の間に新しい待機者が
    /// 割り込むことはない。
    fn release_lock_if_idle(&self, session_id: &str) {
        self.session_locks
            .remove_if(session_id, |_, lock| Arc::strong_count(lock) == 1);
    }

    /// 同一セッションのロック下で `fut` を実行する（per-session 直列化）。
    ///
    /// - 同一セッションは直列（inbound と subtask 完了 resume を割り込ませない）。
    /// - 異なるセッション（別の相手 / 別エージェント / 別会話）は並行。
    ///
    /// 呼び出し側の注意: ゲートウェイ層はこれを**直接公開しない**。生の応答生成を
    /// private に閉じ、直列化込みの入口だけを公開する（web は `respond` モジュールに
    /// 応答生成、兄弟の `sink` モジュールに完了受け口を置いて直呼びをコンパイル
    /// エラーにしている / Nostr は `NostrResponder::respond_serialized`）。直列化を
    /// 呼び出し側の責務にすると 1 箇所の呼び忘れで不変条件が壊れ、テストでは検出できない。
    pub async fn run_serialized<F, T>(&self, session_id: &str, fut: F) -> T
    where
        F: Future<Output = T>,
    {
        let lock = self.lock_for(session_id);
        let guard = lock.lock().await;
        let out = fut.await;
        drop(guard);
        drop(lock);
        self.release_lock_if_idle(session_id);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// 同一セッションの registry は同じ Arc（dispatcher と `cancel_subtask` が同じものを見る）。
    #[test]
    fn registry_is_shared_per_session() {
        let rt = SessionRuntime::new();
        let a = rt.registry_for("s-1");
        let b = rt.registry_for("s-1");
        assert!(Arc::ptr_eq(&a, &b));
        // 別セッションは独立。
        let c = rt.registry_for("s-2");
        assert!(!Arc::ptr_eq(&a, &c));
    }

    /// 未知のセッションは走行中でない（読み取りが registry を作らない）。
    #[test]
    fn has_running_is_false_without_dispatch() {
        let rt = SessionRuntime::new();
        assert!(!rt.has_running("s-none"));
        let _ = rt.registry_for("s-none");
        assert!(!rt.has_running("s-none"));
    }

    /// per-session 直列化: 同一セッションの並行実行は同時実行数 1 になる。
    #[tokio::test]
    async fn same_session_serializes() {
        let rt = Arc::new(SessionRuntime::new());
        let inflight = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let rt = rt.clone();
            let inflight = inflight.clone();
            let max_concurrent = max_concurrent.clone();
            handles.push(tokio::spawn(async move {
                rt.run_serialized("s-serial", async move {
                    let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    inflight.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "同一セッションは直列でなければならない（二重応答の防止）"
        );
    }

    /// 異なるセッションは互いをブロックしない。
    #[tokio::test]
    async fn different_sessions_run_concurrently() {
        let rt = Arc::new(SessionRuntime::new());
        let rt1 = rt.clone();
        let block = tokio::spawn(async move {
            rt1.run_serialized("s-blocking", async {
                tokio::time::sleep(Duration::from_millis(200)).await;
            })
            .await;
        });
        tokio::time::sleep(Duration::from_millis(10)).await;

        tokio::time::timeout(
            Duration::from_millis(100),
            rt.run_serialized("s-free", async {}),
        )
        .await
        .expect("別セッションは他セッションのロックで待たされてはならない");

        block.abort();
    }

    /// 待機者のいないロックエントリは回収される（リーク防止）。
    #[tokio::test]
    async fn idle_lock_is_reclaimed() {
        let rt = SessionRuntime::new();
        rt.run_serialized("s-reclaim", async {}).await;
        assert!(
            !rt.holds_lock_entry("s-reclaim"),
            "待機者がいないロックは回収される"
        );
        assert!(rt.session_locks.is_empty());
    }

    /// 走行中はロックエントリが残る（回収が早すぎないことの裏取り）。
    #[tokio::test]
    async fn lock_entry_exists_while_running() {
        let rt = Arc::new(SessionRuntime::new());
        let rt1 = rt.clone();
        let running = tokio::spawn(async move {
            rt1.run_serialized("s-running", async {
                tokio::time::sleep(Duration::from_millis(200)).await;
            })
            .await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(rt.holds_lock_entry("s-running"));
        running.abort();
    }
}
