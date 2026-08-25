//! web ゲートウェイの共有ランタイム（SSE 配信チャンネル + per-session 直列化）。

use std::future::Future;

use dashmap::DashMap;
use tokio::sync::broadcast;

use opencrab_actions::{CallerIdentity, SessionRuntime, SubtaskRegistry};

/// web セッション ID の接頭辞。sink はこれで web セッションを識別する。
pub const WEB_SESSION_PREFIX: &str = "web-";

/// web 会話セッションのテーマ（session 行の `theme` と実行時コンテキストの topic）。
pub const WEB_SESSION_THEME: &str = "web_conversation";

/// SSE チャンネルの capacity（未接続時のバックログ）。
///
/// crate 内に見せているのは、`http` の取りこぼし（`Lagged`）テストが「capacity を
/// 超える数を publish する」ために必要だから（数値をテスト側へ写すと capacity を
/// 変えたときにテストが黙って無意味になる）。
pub(crate) const SSE_CHANNEL_CAPACITY: usize = 256;

/// session_id 規約: `web-{agent_id}-{conversation_id}`。
pub fn web_session_id(agent_id: &str, conversation_id: &str) -> String {
    format!("{WEB_SESSION_PREFIX}{agent_id}-{conversation_id}")
}

/// 呼び出し元種別の表示名（HTTP レスポンスの `caller_type`）。
///
/// 権限判定そのものは core の [`accept_inbound`](opencrab_actions::accept_inbound) が行う。
/// ここはその結果をレスポンス用の文字列にするだけの純関数。
pub fn caller_type_label(caller: &CallerIdentity) -> &'static str {
    match caller {
        CallerIdentity::CoAgent { .. } => "co_agent",
        CallerIdentity::TrustedUser => "trusted_user",
        CallerIdentity::Owner => "owner",
        _ => "agent",
    }
}

/// SSE で配送するエージェント発話イベント。
///
/// `kind` は配送元の種別: `direct`（inbound への応答） / `subtask_resume`
/// （subtask 完了 resume の応答） / `error`。
#[derive(Debug, Clone, serde::Serialize)]
pub struct WebEvent {
    pub kind: String,
    pub agent_id: String,
    pub content: String,
}

/// web gateway の共有ランタイム。`AppState` が `Arc<WebGateway>` として保持する。
///
/// - `runtime`: per-session 直列化ロック ＋ dispatch 用 registry。gateway 非依存層の
///   [`SessionRuntime`] に委譲する（Nostr と同一実装 / #190 S1）。inbound と resume が
///   同じロック・同じ registry を通ることで、二重回答の防止と cancel の到達性を保つ。
/// - `channels`: per-session の SSE ファンアウト（broadcast）。**web 固有**なのでここに残す
///   （Nostr は返信を relay へ送るため配信チャンネルを持たない）。
#[derive(Default)]
pub struct WebGateway {
    runtime: SessionRuntime,
    channels: DashMap<String, broadcast::Sender<String>>,
}

impl WebGateway {
    pub fn new() -> Self {
        Self::default()
    }

    /// セッションの SSE チャンネルを購読する（無ければ生成）。
    pub fn subscribe(&self, session_id: &str) -> broadcast::Receiver<String> {
        self.channels
            .entry(session_id.to_string())
            .or_insert_with(|| broadcast::channel(SSE_CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// イベントを SSE へ publish する（best-effort。購読者がいなくても DB には残る）。
    pub fn publish(&self, session_id: &str, event: &WebEvent) {
        let Some(sender) = self.channels.get(session_id) else {
            return;
        };
        let payload = match serde_json::to_string(event) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(session_id = %session_id, "web publish serialize failed: {e}");
                return;
            }
        };
        // 購読者がいない場合の Err は無視する（保存は別経路で済んでいる）。
        let _ = sender.send(payload);
    }

    /// セッションの dispatch registry を取得する（無ければ生成し、inbound/resume で共有）。
    pub(crate) fn registry_for(&self, session_id: &str) -> SubtaskRegistry {
        self.runtime.registry_for(session_id)
    }

    /// このセッションに未決着の subtask が残っているか。
    ///
    /// `cancel_subtask` が引く registry と同一のものを見るので、決着後に空になることを
    /// 外から確認できる。
    pub fn has_running(&self, session_id: &str) -> bool {
        self.runtime.has_running(session_id)
    }

    /// セッションのロックエントリが残っているか（回収の観測点。テスト用）。
    #[cfg(test)]
    fn holds_lock_entry(&self, session_id: &str) -> bool {
        self.runtime.holds_lock_entry(session_id)
    }

    /// 同一セッションのロック下で `fut` を実行する（per-session 直列化）。
    ///
    /// - 異なるセッションは並行、同一セッションは直列（inbound / resume を割り込ませない）。
    /// - inbound は呼び出し側が `.await` し、resume は `tokio::spawn` の中で `.await` する。
    ///
    /// ロック保持・アイドル回収の実装は [`SessionRuntime::run_serialized`]（gateway 非依存層）
    /// にあり、ここはそれへの薄いラッパである。
    ///
    /// **crate-private**: 直列化を crate の外の呼び出し側の責務にすると 1 箇所の忘れで
    /// 不変条件が壊れる（レビューで実証: sink 側の `run_serialized` を外してもテストは
    /// 全緑だった）。外へ出す入口は
    /// [`run_and_deliver_serialized`](crate::run_and_deliver_serialized) だけにする。
    pub(crate) async fn run_serialized<F, T>(&self, session_id: &str, fut: F) -> T
    where
        F: Future<Output = T>,
    {
        self.runtime.run_serialized(session_id, fut).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn session_id_follows_convention() {
        assert_eq!(web_session_id("agent-a", "conv-1"), "web-agent-a-conv-1");
        assert!(web_session_id("x", "y").starts_with(WEB_SESSION_PREFIX));
    }

    /// `caller_type` の語彙は HTTP レスポンスの契約（ダッシュボードが読む）。
    #[test]
    fn caller_type_labels_are_stable() {
        assert_eq!(
            caller_type_label(&CallerIdentity::CoAgent {
                agent_id: "a".to_string()
            }),
            "co_agent"
        );
        assert_eq!(
            caller_type_label(&CallerIdentity::TrustedUser),
            "trusted_user"
        );
        assert_eq!(caller_type_label(&CallerIdentity::Owner), "owner");
        assert_eq!(caller_type_label(&CallerIdentity::Agent), "agent");
    }

    /// SSE 配送: publish したイベントが購読者へ届く（保存＋live push の live 側）。
    #[tokio::test]
    async fn publish_reaches_subscriber() {
        let gw = WebGateway::new();
        let sid = web_session_id("a", "c1");
        let mut rx = gw.subscribe(&sid);
        gw.publish(
            &sid,
            &WebEvent {
                kind: "direct".to_string(),
                agent_id: "a".to_string(),
                content: "hello".to_string(),
            },
        );
        let payload = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("recv timed out")
            .expect("channel closed");
        assert!(payload.contains("\"kind\":\"direct\""));
        assert!(payload.contains("hello"));
    }

    /// 購読者がいなくても publish が panic しない（best-effort、DB 保存は別経路）。
    #[tokio::test]
    async fn publish_without_subscriber_is_noop() {
        let gw = WebGateway::new();
        // subscribe せずに publish しても落ちない（チャンネル未生成）。
        gw.publish(
            "web-a-none",
            &WebEvent {
                kind: "direct".to_string(),
                agent_id: "a".to_string(),
                content: "x".to_string(),
            },
        );
    }

    /// per-session 直列化: 同一セッションの run_serialized は直列化される。
    #[tokio::test]
    async fn same_session_serializes() {
        let gw = Arc::new(WebGateway::new());
        let counter = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let sid = web_session_id("a", "serial");

        let mut handles = Vec::new();
        for _ in 0..5 {
            let gw = gw.clone();
            let counter = counter.clone();
            let max_concurrent = max_concurrent.clone();
            let sid = sid.clone();
            handles.push(tokio::spawn(async move {
                gw.run_serialized(&sid, async move {
                    let now = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    counter.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // 同一セッションなので同時実行数は常に 1。
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }

    /// per-session 直列化: 異なるセッションは並行して走る。
    #[tokio::test]
    async fn different_sessions_run_concurrently() {
        let gw = Arc::new(WebGateway::new());
        let started = Arc::new(AtomicUsize::new(0));

        let gw1 = gw.clone();
        let started1 = started.clone();
        let block = tokio::spawn(async move {
            gw1.run_serialized(&web_session_id("a", "blocking"), async move {
                started1.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(200)).await;
            })
            .await;
        });

        // 別セッションは block を待たずに実行できる。
        tokio::time::timeout(
            Duration::from_millis(100),
            gw.run_serialized(&web_session_id("a", "free"), async {}),
        )
        .await
        .expect("different session must not be blocked by another session's lock");

        block.abort();
    }

    /// idle になったロックエントリは回収される（リーク防止）。
    #[tokio::test]
    async fn idle_lock_is_reclaimed() {
        let gw = WebGateway::new();
        let sid = web_session_id("a", "reclaim");
        gw.run_serialized(&sid, async {}).await;
        assert!(
            !gw.holds_lock_entry(&sid),
            "待機者がいないロックは回収される"
        );
    }
}
