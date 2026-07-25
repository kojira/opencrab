//! Nostr ゲートウェイの per-session ランタイム（#168 / RFC #152 S3b-1）。
//!
//! Nostr には従来 session ロックが無く、watch ループが `handle_event` を逐次
//! `await` するだけだった。それでも「1 ループ = 1 直列」だったため inbound 同士は
//! 衝突しなかったが、非ブロック dispatch（S3a）を配線すると **subtask 完了 resume が
//! ループ外の spawn から走る**ため、同一セッションで inbound の応答生成と resume の
//! 応答生成が並行しうる。両者が同じ会話から独立に返信を組み立てると二重投稿になる
//! （RFC §6 の不変条件違反）。
//!
//! そこで web gateway（`WebGateway::run_serialized`）と同じ per-session ロックを
//! Nostr にも置く。ロックの粒度は **session = エージェント × 相手 pubkey** なので、
//! 別の相手との会話や別エージェントは従来どおり並行する。
//!
//! `registries` は dispatch した subtask を追跡する registry を session 単位で貸す。
//! inbound と resume で同一 Arc を共有することが `cancel_subtask`（#161）が到達できる
//! 条件（別 registry を渡すと常に not found になる）。

use std::future::Future;
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;

use opencrab_actions::{SubtaskRegistries, SubtaskRegistry};

/// Nostr セッション ID の接頭辞。sink はこれで Nostr セッションを識別する。
pub const NOSTR_SESSION_PREFIX: &str = "nostr-";

/// session_id 規約: `nostr-{agent_id}-{author_pubkey}`（1 相手 = 1 会話）。
pub fn nostr_session_id(agent_id: &str, author_pubkey: &str) -> String {
    format!("{NOSTR_SESSION_PREFIX}{agent_id}-{author_pubkey}")
}

/// Nostr ゲートウェイが全エージェント横断で 1 つ持つ per-session ランタイム。
///
/// `NostrGatewayManager` が保持し、watch ループと完了 sink が同じ Arc を共有する。
#[derive(Default)]
pub struct NostrSessionRuntime {
    session_locks: DashMap<String, Arc<Mutex<()>>>,
    registries: SubtaskRegistries,
}

impl NostrSessionRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// セッションの dispatch registry を取得する（無ければ生成し、inbound/resume で共有）。
    pub fn registry_for(&self, session_id: &str) -> SubtaskRegistry {
        self.registries.registry_for(session_id)
    }

    /// セッションに走行中（未決着）の dispatch subtask があるか。
    pub fn has_running(&self, session_id: &str) -> bool {
        self.registries.has_running(session_id)
    }

    fn lock_for(&self, session_id: &str) -> Arc<Mutex<()>> {
        self.session_locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// 待機者がいなければロックエントリを回収する（リーク防止）。
    fn release_lock_if_idle(&self, session_id: &str) {
        self.session_locks
            .remove_if(session_id, |_, lock| Arc::strong_count(lock) == 1);
    }

    /// 同一セッションのロック下で `fut` を実行する（per-session 直列化）。
    ///
    /// - 同一セッションは直列（inbound と subtask 完了 resume を割り込ませない）。
    /// - 異なるセッション（別の相手 / 別エージェント）は並行。
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

    #[test]
    fn session_id_follows_convention() {
        assert_eq!(nostr_session_id("a1", "deadbeef"), "nostr-a1-deadbeef");
        assert!(nostr_session_id("a1", "x").starts_with(NOSTR_SESSION_PREFIX));
    }

    /// 同一セッションの registry は同じ Arc（dispatcher と cancel_subtask が同じものを見る）。
    #[test]
    fn registry_is_shared_per_session() {
        let rt = NostrSessionRuntime::new();
        let sid = nostr_session_id("a1", "pk1");
        let a = rt.registry_for(&sid);
        let b = rt.registry_for(&sid);
        assert!(Arc::ptr_eq(&a, &b));
        // 別セッションは独立。
        let c = rt.registry_for(&nostr_session_id("a1", "pk2"));
        assert!(!Arc::ptr_eq(&a, &c));
    }

    /// per-session 直列化: 同一セッションの並行実行は同時実行数 1 になる。
    #[tokio::test]
    async fn same_session_serializes() {
        let rt = Arc::new(NostrSessionRuntime::new());
        let inflight = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let sid = nostr_session_id("a1", "serial");

        let mut handles = Vec::new();
        for _ in 0..5 {
            let rt = rt.clone();
            let inflight = inflight.clone();
            let max_concurrent = max_concurrent.clone();
            let sid = sid.clone();
            handles.push(tokio::spawn(async move {
                rt.run_serialized(&sid, async move {
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
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }

    /// 異なるセッション（別の相手）は互いをブロックしない。
    #[tokio::test]
    async fn different_sessions_run_concurrently() {
        let rt = Arc::new(NostrSessionRuntime::new());
        let rt1 = rt.clone();
        let block = tokio::spawn(async move {
            rt1.run_serialized(&nostr_session_id("a1", "blocking"), async {
                tokio::time::sleep(Duration::from_millis(200)).await;
            })
            .await;
        });
        tokio::time::sleep(Duration::from_millis(10)).await;

        tokio::time::timeout(
            Duration::from_millis(100),
            rt.run_serialized(&nostr_session_id("a1", "free"), async {}),
        )
        .await
        .expect("別セッションは他セッションのロックで待たされてはならない");

        block.abort();
    }

    /// 待機者のいないロックエントリは回収される（リーク防止）。
    #[tokio::test]
    async fn idle_lock_is_reclaimed() {
        let rt = NostrSessionRuntime::new();
        let sid = nostr_session_id("a1", "reclaim");
        rt.run_serialized(&sid, async {}).await;
        assert!(rt.session_locks.get(&sid).is_none());
    }
}
