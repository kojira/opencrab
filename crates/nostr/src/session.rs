//! Nostr ゲートウェイの per-session ランタイム（#168 / RFC #152 S3b-1）。
//!
//! Nostr には従来 session ロックが無く、watch ループが `handle_event` を逐次
//! `await` するだけだった。それでも「1 ループ = 1 直列」だったため inbound 同士は
//! 衝突しなかったが、非ブロック dispatch（S3a）を配線すると **subtask 完了 resume が
//! ループ外の spawn から走る**ため、同一セッションで inbound の応答生成と resume の
//! 応答生成が並行しうる。両者が同じ会話から独立に返信を組み立てると二重投稿になる
//! （RFC §6 の不変条件違反）。
//!
//! そこで per-session ロックを置く。ロックの粒度は **session = エージェント**
//! （#323）なので、別エージェントは従来どおり並行し、同じエージェントの応答は
//! 相手が誰であれ 1 本ずつになる。
//!
//! 直列化ロジックそのものは web gateway と文字レベルでほぼ同一だったため、#190 S1 で
//! gateway 非依存層の [`SessionRuntime`] へ 1 つに寄せた。ここに残すのは **Nostr 固有の
//! 語彙**（session_id の規約と接頭辞）だけで、`NostrSessionRuntime` はその下位層型の
//! 別名である（呼び出し側の型名・API は不変）。

use opencrab_actions::SessionRuntime;

/// Nostr セッション ID の接頭辞。sink はこれで Nostr セッションを識別する。
pub const NOSTR_SESSION_PREFIX: &str = "nostr-";

/// session_id 規約: `nostr-{agent_id}`（**1 エージェント = 1 会話** / #323）。
///
/// 以前は `nostr-{agent_id}-{author_pubkey}`（1 相手 = 1 会話）だった。Discord の
/// 「チャンネル = 会話 = セッション」という 1 対 1 の対応を Nostr に持ち込んだもの
/// だが、Nostr のスレッドは**多人数**なので前提が合っておらず、
///
/// - 会話が相手ごとに割れ、エージェントは「自分がさっき誰に何を言ったか」を跨いで
///   見られない（同じ内容を繰り返す / 自分の発言と食い違うことを言う）
/// - 直列化の鍵（`SessionRuntime` の session_id）が会話単位とずれ、同一エージェントの
///   応答が相手ごとに並行して走る
///
/// という 2 つの症状が出ていた（#323）。エージェント単位に寄せると両方消える。
///
/// **発言者の区別は session ではなく転記の `speaker_id`（相手の pubkey）が担う。**
/// 会話文字列は `[{speaker_id}]:` 形式で出るので、1 本に混ざっても誰の発言かは
/// 失われない。相手 pubkey を session_id から復元できないことも意図どおりで、
/// 必要な経路は発生源（受信イベント）から明示的に運ぶ。
///
/// # 既存セッションの扱い
///
/// 旧規約で溜まった `nostr-{agent_id}-{author_pubkey}` の行は **そのまま残す**
/// （DB マイグレーションは行わない）。統合しない理由:
///
/// - **不可逆になる**。統合は session_id の付け替えなので行は失わないが、
///   エージェント自身の発言（outbound）は宛先を持たないため、一度混ぜると
///   「どの相手との会話だったか」を復元できない。戻せない変更を、戻せる変更
///   （バイナリの差し戻しだけで済む）と一緒にしない。
/// - **時系列で辿る手段は失われない**。旧セッションの行は `created_at` ごと
///   そのまま残り、会話ログ検索（`search_session_logs`）は session ではなく
///   agent スコープなので、旧い Nostr のやり取りは引き続き引ける。
/// - 実際に溜まっている量が小さく、1 本化した新セッションは数ターンで文脈が育つ。
///
/// 後から統合したくなったら、この関数の規約に合わせて session_id を付け替える
/// マイグレーションを足せばよい（新しい概念は要らない）。
pub fn nostr_session_id(agent_id: &str) -> String {
    format!("{NOSTR_SESSION_PREFIX}{agent_id}")
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

    /// [#323] session_id は agent 単位（相手 pubkey は入らない）。
    #[test]
    fn session_id_follows_convention() {
        assert_eq!(nostr_session_id("a1"), "nostr-a1");
        assert!(nostr_session_id("a1").starts_with(NOSTR_SESSION_PREFIX));
    }

    /// 同一セッションの registry は同じ Arc（dispatcher と cancel_subtask が同じものを見る）。
    #[test]
    fn registry_is_shared_per_session() {
        let rt = NostrSessionRuntime::new();
        let sid = nostr_session_id("a1");
        let a = rt.registry_for(&sid);
        let b = rt.registry_for(&sid);
        assert!(Arc::ptr_eq(&a, &b));
        // 別エージェントは独立（session が独立する単位は agent / #323）。
        let c = rt.registry_for(&nostr_session_id("a2"));
        assert!(!Arc::ptr_eq(&a, &c));
    }

    /// Nostr の session_id でも直列化ロックが取得・回収される（下位層への委譲の配線確認）。
    ///
    /// 直列 / 並行そのものの検証は下位層
    /// （`opencrab_actions::session_runtime` のテスト）が持つ。
    #[tokio::test]
    async fn idle_lock_is_reclaimed() {
        let rt = NostrSessionRuntime::new();
        let sid = nostr_session_id("a1-reclaim");
        rt.run_serialized(&sid, async {}).await;
        assert!(!rt.holds_lock_entry(&sid));
    }
}
