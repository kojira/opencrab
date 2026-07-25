//! web gateway ランタイム（#154 第1スライス）。
//!
//! ダッシュボード（web）からエージェントと会話するための最小ゲートウェイ。
//! #152 の再層化基盤（`SubtaskCompletionSink` / `settle_completed` / 非ブロック
//! dispatch）の上に載る **sink 抽象を Discord 以外で初めて動かす検証**でもある。
//!
//! Discord 実装（`crates/discord`）との対比:
//! - Discord は完了通知を `LoopEvent`（serenity 依存のイベントループ）へ送り、
//!   ループが `spawn_serialized_on_session` で resume する。
//! - web は `LoopEvent` を持たない。`WebCompletionSink::on_subtask_settled` が
//!   直接 `tokio::spawn` して per-session ロック下で resume し、SSE で配送する。
//!   Discord 固有型（LoopEvent / serenity / DiscordReplyContext）は一切持ち込まない。
//!
//! 不変条件（RFC #152 §6）:
//! - **二重回答**: `settle_completed` が「DB 永続化 → sink 発火」の順序を保証済み。
//!   resume は `build_conversation_string` で DB から会話を再構築する。
//! - **per-session 直列化**: `run_serialized` が同一セッションの inbound / resume を
//!   1 本のロックで直列化する（割り込み二重回答の防止）。異なるセッションは並行。

use std::future::Future;
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::{broadcast, Mutex};

use opencrab_actions::{
    CallerIdentity, RunRequest, SubtaskCompletionSink, SubtaskRegistry, SubtaskSettled,
};

use crate::process;
use crate::subtask_registries::SubtaskRegistries;
use crate::AppState;

/// web セッション ID の接頭辞。sink はこれで web セッションを識別する。
pub const WEB_SESSION_PREFIX: &str = "web-";

/// SSE チャンネルの capacity（未接続時のバックログ）。
const SSE_CHANNEL_CAPACITY: usize = 256;

/// session_id 規約: `web-{agent_id}-{conversation_id}`。
pub fn web_session_id(agent_id: &str, conversation_id: &str) -> String {
    format!("{WEB_SESSION_PREFIX}{agent_id}-{conversation_id}")
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
/// - `session_locks`: per-session 直列化ロック（Discord の SessionLocks 相当）。
/// - `registries`: per-session の dispatch 用 registry（inbound / resume で共有し、
///   cancel/list から到達可能に保つ）。
/// - `channels`: per-session の SSE ファンアウト（broadcast）。
#[derive(Default)]
pub struct WebGateway {
    session_locks: DashMap<String, Arc<Mutex<()>>>,
    registries: SubtaskRegistries,
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
    fn registry_for(&self, session_id: &str) -> SubtaskRegistry {
        self.registries.registry_for(session_id)
    }

    /// このセッションに未決着の subtask が残っているか。
    ///
    /// `cancel_subtask` が引く registry と同一のものを見るので、決着後に空になることを
    /// 外から確認できる（REST 側の `SubtaskRegistries::has_running` と対称）。
    pub fn has_running(&self, session_id: &str) -> bool {
        self.registries.has_running(session_id)
    }

    fn lock_for(&self, session_id: &str) -> Arc<Mutex<()>> {
        self.session_locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// 待機者がいなければロックエントリを回収する（#39 相当のリーク防止）。
    fn release_lock_if_idle(&self, session_id: &str) {
        self.session_locks
            .remove_if(session_id, |_, lock| Arc::strong_count(lock) == 1);
    }

    /// 同一セッションのロック下で `fut` を実行する（per-session 直列化）。
    ///
    /// - 異なるセッションは並行、同一セッションは直列（inbound / resume を割り込ませない）。
    /// - inbound は呼び出し側が `.await` し、resume は `tokio::spawn` の中で `.await` する。
    ///
    /// **module-private**: 直列化を呼び出し側の責務にすると 1 箇所の忘れで不変条件が壊れる
    /// （レビューで実証: sink 側の `run_serialized` を外してもテストは全緑だった）。
    /// 外へ出す入口は [`run_and_deliver_serialized`] だけにする（Nostr の
    /// `NostrResponder::respond_serialized` と同じ構造）。
    async fn run_serialized<F, T>(&self, session_id: &str, fut: F) -> T
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

/// `SubtaskCompletionSink` の web 実装（gateway 非依存 = LoopEvent 不使用）。
///
/// subtask が settle したとき、web セッションなら per-session ロック下で親を resume し、
/// 生成メッセージを SSE で配送する。非 web の親セッション（heartbeat-* / subtask-* の
/// ネスト等）は正常系としてスキップする（Discord 実装が非 discord をスキップするのと同じ）。
#[derive(Clone)]
pub struct WebCompletionSink {
    pub state: AppState,
    pub gateway: Arc<WebGateway>,
}

impl SubtaskCompletionSink for WebCompletionSink {
    fn on_subtask_settled(&self, ev: SubtaskSettled) {
        if !ev.session_id.starts_with(WEB_SESSION_PREFIX) {
            tracing::debug!(
                session_id = %ev.session_id,
                "web sink: parent session is not a web session, skipping resume"
            );
            return;
        }
        let state = self.state.clone();
        let gateway = self.gateway.clone();
        // sink は同期関数。resume は非同期のため spawn する（Discord が LoopEvent を
        // mpsc へ送るのと役割は同じ。web はループが無いので直接 spawn する）。
        tokio::spawn(async move {
            let note = format!(
                "[subtask_completed: subtask_id={}, exit_reason={}]",
                ev.subtask_id, ev.exit_reason
            );
            let sid = ev.session_id.clone();
            let agent_id = ev.agent_id.clone();
            run_and_deliver_serialized(
                &state,
                &gateway,
                &agent_id,
                &sid,
                CallerIdentity::Agent,
                Some(&note),
                "subtask_resume",
            )
            .await;
        });
    }
}

/// [`run_and_deliver`] を per-session ロックの下で実行する（**唯一の公開入口**）。
///
/// inbound（`POST /api/agents/{id}/web/send`）と resume（`WebCompletionSink`）が同じ
/// ロックを通るので、同一セッションに対して 2 本の応答生成が並行しない = 同じ履歴から
/// 2 通の応答が SSE へ流れない。
///
/// ロック取得を呼び出し側の責務にしていた頃は、sink 側の `run_serialized` を外しても
/// テストが全緑だった（呼び忘れを検出できない構造だった）。Nostr の
/// `NostrResponder::respond_serialized` と同じく、直列化をここに閉じ込める。
pub async fn run_and_deliver_serialized(
    state: &AppState,
    gateway: &Arc<WebGateway>,
    agent_id: &str,
    session_id: &str,
    caller: CallerIdentity,
    system_prompt_suffix: Option<&str>,
    kind: &str,
) -> Option<String> {
    let fut = run_and_deliver(
        state,
        gateway,
        agent_id,
        session_id,
        caller,
        system_prompt_suffix,
        kind,
    );
    gateway.run_serialized(session_id, fut).await
}

/// 会話を DB から構築 → `run_agent_response`（非ブロック dispatch 付き）→ 応答を
/// DB 保存 ＋ SSE 配送する共通経路。inbound と subtask resume の双方が使う。
///
/// 返り値: 配送した応答本文（NO_REPLY / 空 / エラー時は None）。
///
/// 呼び出しは [`run_and_deliver_serialized`] 経由に限る（直列化の担保のため
/// module-private にしている）。
///
/// **MutexGuard を await 跨ぎで保持しない**（各 DB ロックはブロックで閉じてから await）。
async fn run_and_deliver(
    state: &AppState,
    gateway: &Arc<WebGateway>,
    agent_id: &str,
    session_id: &str,
    caller: CallerIdentity,
    system_prompt_suffix: Option<&str>,
    kind: &str,
) -> Option<String> {
    // 1. system prompt（+ resume 時は [subtask_completed] 注入）。
    let (mut system_prompt, agent_name) = {
        let conn = state.db.lock().unwrap();
        process::build_agent_context(&conn, agent_id)
    };
    if let Some(suffix) = system_prompt_suffix {
        system_prompt = format!("{system_prompt}\n\n{suffix}");
    }

    // 2. 会話文字列（DB から再構築 = 二重回答不変の要）。
    let conversation = {
        let conn = state.db.lock().unwrap();
        let eff =
            opencrab_db::queries::effective_model_for_agent(&conn, agent_id, &state.default_model)
                .unwrap_or_else(|_| state.default_model.clone());
        let (prov, mdl) = process::split_llm_model_spec(&eff);
        let budget = process::compute_context_budget(&conn, prov, mdl, state.compaction_ratio);
        match process::build_conversation_string(&conn, session_id, agent_id, budget) {
            Ok(raw) => process::prepend_runtime_context(&raw, "web_conversation"),
            Err(e) => {
                tracing::error!(session_id = %session_id, "web run: build_conversation_string failed: {e}");
                return None;
            }
        }
    };

    // 3. 非ブロック dispatch（S3a）を有効化して実行。sink / registry は session 共有。
    let registry = gateway.registry_for(session_id);
    let sink: Arc<dyn SubtaskCompletionSink> = Arc::new(WebCompletionSink {
        state: state.clone(),
        gateway: gateway.clone(),
    });
    let run_req = RunRequest::new(
        agent_id,
        &agent_name,
        session_id,
        &system_prompt,
        &conversation,
        "web",
        caller,
    )
    .with_dispatch(Some(registry), sink);

    let result = process::run_agent_response(state, run_req).await;

    // 4. 応答の保存 ＋ SSE 配送。
    match result {
        Ok(er) if !er.response.trim().is_empty() && er.response.trim() != "NO_REPLY" => {
            {
                let conn = state.db.lock().unwrap();
                crate::transcript::record_rest_agent_reply(
                    &conn,
                    agent_id,
                    session_id,
                    &er.response,
                    er.iterations,
                    er.tool_calls_made,
                );
            }
            gateway.publish(
                session_id,
                &WebEvent {
                    kind: kind.to_string(),
                    agent_id: agent_id.to_string(),
                    content: er.response.clone(),
                },
            );
            Some(er.response)
        }
        Ok(_) => None, // NO_REPLY / 空: 配送しない。
        Err(e) => {
            tracing::error!(agent_id = %agent_id, session_id = %session_id, error = format!("{e:#}"), "web run: agent response failed");
            gateway.publish(
                session_id,
                &WebEvent {
                    kind: "error".to_string(),
                    agent_id: agent_id.to_string(),
                    content: format!("(error: {e})"),
                },
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn session_id_follows_convention() {
        assert_eq!(web_session_id("agent-a", "conv-1"), "web-agent-a-conv-1");
        assert!(web_session_id("x", "y").starts_with(WEB_SESSION_PREFIX));
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
            gw.session_locks.get(&sid).is_none(),
            "待機者がいないロックは回収される"
        );
    }
}
