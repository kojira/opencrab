//! Per-agent Nostr sub-gateway マネージャ + watch ループ。
//!
//! Discord の `DiscordGatewayManager` と同型。エージェント毎に nostaro の `watch --json`
//! を spawn し、JSONL イベントを読んで `run_agent_response` → 返信する。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use opencrab_actions::{CallerIdentity, RunRequest};
use opencrab_gateway::GatewayActions;

use crate::actions::NostrGatewayActions;
use crate::cli::NostaroCli;
use crate::config::NostrConfig;
use crate::event::{parse_watch_line, NostrEvent};
use crate::runner::NostrAgentRunner;

/// watch が落ちたときの再接続バックオフ。
const WATCH_RESTART_DELAY: Duration = Duration::from_secs(5);

pub struct NostrGatewayManager<R: NostrAgentRunner> {
    // std RwLock: is_running を同期メソッドにするため。ガードは await を跨がない。
    gateways: RwLock<HashMap<String, JoinHandle<()>>>,
    runner: R,
    cli: NostaroCli,
}

impl<R: NostrAgentRunner> NostrGatewayManager<R> {
    pub fn new(runner: R) -> Self {
        Self {
            gateways: RwLock::new(HashMap::new()),
            runner,
            cli: NostaroCli::new(),
        }
    }

    pub fn with_cli(mut self, cli: NostaroCli) -> Self {
        self.cli = cli;
        self
    }

    /// 内部の nostaro CLI ラッパー（鍵生成など gateway 起動を伴わない操作用）。
    pub fn cli(&self) -> &NostaroCli {
        &self.cli
    }

    /// エージェントの Nostr ゲートウェイを起動する。
    ///
    /// 秘密鍵/リレーから per-agent config.toml を materialize（0600）し、自分の pubkey を
    /// 取得（自己返信ループ防止）して watch ループを spawn する。
    pub async fn start_agent_gateway(
        &self,
        agent_id: &str,
        secret_key: &str,
        config: NostrConfig,
    ) -> anyhow::Result<()> {
        // 全ノート購読（author も keyword も無い）は洪水/資源浪費になるため、
        // ここ（単一チョークポイント）で拒否する（PUT enabled=false→/start バイパス封じ）。
        if config.filter_is_unbounded() {
            anyhow::bail!(
                "Nostr フィルタが空です。author か keyword を最低1つ指定してください（全ノート購読は不可）"
            );
        }

        self.stop_agent_gateway(agent_id).await;

        // nsec を含む config を 0600 で書き出す。
        NostaroCli::materialize_config(agent_id, secret_key, &config.effective_relays(), None)?;

        // 自分の pubkey は自己返信ループ防止に必須。取得できなければ **起動しない**
        // （fail-closed: 自己フィルタ無しで走ると keyword フィルタ時に自分の返信を
        // 拾って無限ループ＋LLM 支出になる）。
        let self_pubkey = self
            .cli
            .pubkey(agent_id)
            .await
            .map(|pk| pk.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "自分の pubkey を取得できませんでした（`nostaro pubkey` 必須）。\
                     自己返信ループ防止のため起動を中止します"
                )
            })?;

        let runner = self.runner.clone();
        let cli = self.cli.clone();
        let agent = agent_id.to_string();
        let handle = tokio::spawn(async move {
            run_nostr_loop(runner, cli, agent, config, self_pubkey).await;
        });

        self.gateways
            .write()
            .unwrap()
            .insert(agent_id.to_string(), handle);
        info!(agent_id, "Per-agent Nostr gateway started");
        Ok(())
    }

    pub async fn stop_agent_gateway(&self, agent_id: &str) {
        let handle = self.gateways.write().unwrap().remove(agent_id);
        if let Some(handle) = handle {
            // abort でループ frame を drop → 子 nostaro は kill_on_drop で kill される。
            handle.abort();
            info!(agent_id, "Per-agent Nostr gateway stopped");
        }
    }

    pub fn is_running(&self, agent_id: &str) -> bool {
        self.gateways
            .read()
            .unwrap()
            .get(agent_id)
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }

    /// enabled な設定を DB から復元して起動する。
    pub async fn restore_from_db(&self) {
        for cfg in self.runner.list_enabled_nostr_configs() {
            let config = crate::config_from_row(&cfg);
            if let Err(e) = self
                .start_agent_gateway(&cfg.agent_id, &cfg.secret_key, config)
                .await
            {
                error!(agent_id = %cfg.agent_id, error = %e, "Failed to restore Nostr gateway");
            }
        }
    }

    pub async fn shutdown_all(&self) {
        let handles: Vec<(String, JoinHandle<()>)> =
            self.gateways.write().unwrap().drain().collect();
        for (agent_id, handle) in handles {
            handle.abort();
            info!(agent_id, "Nostr gateway stopped (shutdown_all)");
        }
    }
}

/// 処理済み event.id の bounded FIFO セット（watch 再購読時の再処理 = 二重返信を防ぐ）。
struct SeenEvents {
    order: std::collections::VecDeque<String>,
    set: std::collections::HashSet<String>,
    cap: usize,
}

impl SeenEvents {
    fn new(cap: usize) -> Self {
        Self {
            order: std::collections::VecDeque::new(),
            set: std::collections::HashSet::new(),
            cap,
        }
    }

    /// 新規なら true を返して記録。既知なら false。
    fn check_and_insert(&mut self, id: &str) -> bool {
        if self.set.contains(id) {
            return false;
        }
        self.set.insert(id.to_string());
        self.order.push_back(id.to_string());
        if self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

/// watch ループ本体。watch が落ちてもバックオフして再購読する（abort されるまで）。
/// dedup セットはループ寿命で保持する（再購読時の replay を跨いで効かせる）。
async fn run_nostr_loop<R: NostrAgentRunner>(
    runner: R,
    cli: NostaroCli,
    agent_id: String,
    config: NostrConfig,
    self_pubkey: String,
) {
    let mut seen = SeenEvents::new(512);
    loop {
        match run_watch_once(&runner, &cli, &agent_id, &config, &self_pubkey, &mut seen).await {
            Ok(()) => warn!(agent_id, "nostr watch exited; restarting after backoff"),
            Err(e) => error!(agent_id, error = %e, "nostr watch error; restarting after backoff"),
        }
        tokio::time::sleep(WATCH_RESTART_DELAY).await;
    }
}

/// 1 回分の watch 購読（プロセス寿命ぶん）。
async fn run_watch_once<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    config: &NostrConfig,
    self_pubkey: &str,
    seen: &mut SeenEvents,
) -> anyhow::Result<()> {
    let mut cmd = cli.build_watch_command(agent_id, config)?;
    let mut child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!("failed to spawn `nostaro watch` (is nostaro installed?): {e}")
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("nostaro watch produced no stdout handle"))?;
    let mut lines = BufReader::new(stdout).lines();

    info!(agent_id, relays = ?config.effective_relays(), "nostr watch subscribed");
    while let Some(line) = lines.next_line().await? {
        let Some(event) = parse_watch_line(&line) else {
            continue;
        };
        // 自分の投稿はスキップ（自己返信ループ防止）。
        if self_pubkey == event.pubkey {
            debug!(agent_id, "nostr: skipping own event");
            continue;
        }
        // 再処理防止（replay/重複）。
        if !seen.check_and_insert(&event.id) {
            debug!(agent_id, "nostr: skipping already-processed event");
            continue;
        }
        handle_event(runner, cli, agent_id, &event).await;
    }
    // stdout EOF → プロセス終了を回収。
    let _ = child.wait().await;
    Ok(())
}

/// 受信イベント1件を処理する（セッション記録 → エージェント実行 → 返信）。
async fn handle_event<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    event: &NostrEvent,
) {
    // author 単位のセッション（1 相手 = 1 会話）。
    let session_id = format!("nostr-{agent_id}-{}", event.pubkey);

    let (base_prompt, agent_name) = runner.build_agent_context(agent_id);
    let system_prompt = format!(
        "{base_prompt}\n\n[Nostr] {author} さんの投稿への応答です。返信するなら \
         nostr_reply(target=\"{target}\") を使ってください（target は返信先ノート）。\
         返信不要なら NO_REPLY とだけ答えてください。",
        author = event.author_label(),
        target = event.reply_target(),
    );

    runner.ensure_session(&session_id, &[agent_id.to_string()], "Nostr", "{}");
    runner.record_nostr_user_message(
        &session_id,
        &event.pubkey,
        &event.author_label(),
        &event.content,
    );

    let budget = runner.context_budget_tokens(agent_id);
    let conversation = runner
        .build_conversation_string(&session_id, agent_id, budget)
        .unwrap_or_default();

    let gw = NostrGatewayActions::new(cli.clone());
    let sent = gw.sent_flag();
    let actions: Arc<dyn GatewayActions> = Arc::new(gw);
    let req = RunRequest::new(
        agent_id,
        agent_name,
        session_id.clone(),
        system_prompt,
        conversation,
        "nostr",
        // Nostr の投稿者は外部ユーザー。最小権限（Agent）で扱う。
        CallerIdentity::Agent,
    )
    .with_gateway_actions(actions)
    .with_trigger_message_id(event.id.clone());

    match runner.run_agent_response(req).await {
        Ok(result) => {
            let reply = result.response.trim();
            if reply.is_empty() || reply == "NO_REPLY" {
                debug!(agent_id, "nostr: agent chose silence");
                return;
            }
            // 最終応答テキストを転記（会話履歴の継続性）。
            runner.record_nostr_agent_reply(agent_id, &session_id, reply);
            // モデルが既に nostr_* で送信していれば暗黙返信しない（二重送信防止）。
            if sent.load(std::sync::atomic::Ordering::SeqCst) {
                debug!(
                    agent_id,
                    "nostr: explicit send already occurred; skipping implicit reply"
                );
                return;
            }
            if let Err(e) = cli.reply(agent_id, event.reply_target(), reply).await {
                warn!(agent_id, error = %e, "nostr implicit reply failed");
            }
        }
        Err(e) => error!(agent_id, error = %e, "nostr agent run failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::SeenEvents;

    #[test]
    fn test_seen_events_dedup_and_eviction() {
        let mut seen = SeenEvents::new(2);
        assert!(seen.check_and_insert("a")); // 新規
        assert!(!seen.check_and_insert("a")); // 既知
        assert!(seen.check_and_insert("b"));
        // cap=2 を超えると最古（a）を追い出す。
        assert!(seen.check_and_insert("c"));
        // a は追い出されたので再び新規扱い（replay 耐性は cap ぶん）。
        assert!(seen.check_and_insert("a"));
        // b はまだ保持（直近）… ではなく a 追加で b が最古になり追い出される可能性。
        // 少なくとも直近の c は既知のまま。
        assert!(!seen.check_and_insert("c"));
    }
}
