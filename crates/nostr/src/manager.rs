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
        self.stop_agent_gateway(agent_id).await;

        // nsec を含む config を 0600 で書き出す。
        NostaroCli::materialize_config(agent_id, secret_key, &config.effective_relays(), None)?;

        // 自分の pubkey（best-effort: 取れなくても続行するが自己フィルタは効かない）。
        let self_pubkey = match self.cli.pubkey(agent_id).await {
            Ok(pk) => Some(pk.trim().to_string()).filter(|s| !s.is_empty()),
            Err(e) => {
                warn!(agent_id, error = %e, "nostr: could not resolve own pubkey; self-reply filter disabled");
                None
            }
        };

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

/// watch ループ本体。watch が落ちてもバックオフして再購読する（abort されるまで）。
async fn run_nostr_loop<R: NostrAgentRunner>(
    runner: R,
    cli: NostaroCli,
    agent_id: String,
    config: NostrConfig,
    self_pubkey: Option<String>,
) {
    loop {
        match run_watch_once(&runner, &cli, &agent_id, &config, self_pubkey.as_deref()).await {
            Ok(()) => {
                warn!(agent_id, "nostr watch exited; restarting after backoff");
            }
            Err(e) => {
                error!(agent_id, error = %e, "nostr watch error; restarting after backoff");
            }
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
    self_pubkey: Option<&str>,
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
        if self_pubkey.is_some_and(|pk| pk == event.pubkey) {
            debug!(agent_id, "nostr: skipping own event");
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

    let actions: Arc<dyn GatewayActions> = Arc::new(NostrGatewayActions::new(cli.clone()));
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
            // 明示的に nostr_reply を呼んでいない場合の暗黙返信。
            match cli.reply(agent_id, event.reply_target(), reply).await {
                Ok(_) => runner.record_nostr_agent_reply(agent_id, &session_id, reply),
                Err(e) => warn!(agent_id, error = %e, "nostr implicit reply failed"),
            }
        }
        Err(e) => error!(agent_id, error = %e, "nostr agent run failed"),
    }
}
