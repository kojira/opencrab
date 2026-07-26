//! Nostr ゲートウェイの per-session ランタイム（#168 / RFC #152 S3b-1）。
//!
//! Nostr には従来 session ロックが無く、watch ループが `handle_event` を逐次
//! `await` するだけだった。それでも「1 ループ = 1 直列」だったため inbound 同士は
//! 衝突しなかったが、非ブロック dispatch（S3a）を配線すると **subtask 完了 resume が
//! ループ外の spawn から走る**ため、同一セッションで inbound の応答生成と resume の
//! 応答生成が並行しうる。両者が同じ会話から独立に返信を組み立てると二重投稿になる
//! （RFC §6 の不変条件違反）。
//!
//! そこで per-session ロックを置く。ロックの粒度は **session = エージェント × 相手
//! pubkey** なので、別の相手との会話や別エージェントは従来どおり並行する。
//!
//! 直列化ロジックそのものは web gateway と文字レベルでほぼ同一だったため、#190 S1 で
//! gateway 非依存層の [`SessionRuntime`] へ 1 つに寄せた。ここに残すのは **Nostr 固有の
//! 語彙**（session_id の規約と接頭辞）だけで、`NostrSessionRuntime` はその下位層型の
//! 別名である（呼び出し側の型名・API は不変）。

use opencrab_actions::SessionRuntime;

/// Nostr セッション ID の接頭辞。sink はこれで Nostr セッションを識別する。
pub const NOSTR_SESSION_PREFIX: &str = "nostr-";

/// session_id 規約: `nostr-{agent_id}-{author_pubkey}`（1 相手 = 1 会話）。
pub fn nostr_session_id(agent_id: &str, author_pubkey: &str) -> String {
    format!("{NOSTR_SESSION_PREFIX}{agent_id}-{author_pubkey}")
}

/// Nostr ゲートウェイが全エージェント横断で 1 つ持つ per-session ランタイム。
///
/// `NostrGatewayManager` が `Arc` で保持し、watch ループと完了 sink が同じ Arc を共有する。
/// 実体は gateway 非依存層の [`SessionRuntime`]（per-session 直列化 + dispatch 登録簿）。
/// 直列化を Nostr 側に複製しないことで、web と挙動がずれない。
pub type NostrSessionRuntime = SessionRuntime;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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

    /// Nostr の session_id でも直列化ロックが取得・回収される（下位層への委譲の配線確認）。
    ///
    /// 直列 / 並行そのものの検証は下位層
    /// （`opencrab_actions::session_runtime` のテスト）が持つ。
    #[tokio::test]
    async fn idle_lock_is_reclaimed() {
        let rt = NostrSessionRuntime::new();
        let sid = nostr_session_id("a1", "reclaim");
        rt.run_serialized(&sid, async {}).await;
        assert!(!rt.holds_lock_entry(&sid));
    }
}
