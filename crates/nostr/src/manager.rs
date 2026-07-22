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

use anyhow::Context;
use opencrab_actions::{CallerIdentity, RunRequest};
use opencrab_gateway::GatewayActions;

use crate::actions::NostrGatewayActions;
use crate::cli::NostaroCli;
use crate::config::NostrConfig;
use crate::event::{parse_watch_line, NostrEvent};
use crate::identity::NostrIdentityAdmin;
use crate::runner::NostrAgentRunner;

/// watch ループが握る self_pubkey の共有セル（identity 切替で更新可能）。
type SelfPubkey = Arc<RwLock<String>>;

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

    /// 内部の nostaro CLI ラッパー（gateway 起動を伴わない操作用）。
    pub fn cli(&self) -> &NostaroCli {
        &self.cli
    }

    /// vanity で新規鍵を生成する。同時実行の制限は `NostaroCli` 内のゲートで一元化
    /// （HTTP ルートも LLM ツールも同じゲートを通る）。
    pub async fn generate_key(&self, prefix: &str) -> anyhow::Result<crate::GeneratedKey> {
        self.cli.vanity(prefix).await
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
        // self_pubkey は共有セル。identity 切替（本鍵採用）時に新 pubkey へ更新できる
        // ようにする（watch は鍵非依存なのでプロセス再起動不要。セル更新だけで自己
        // スキップが新 identity に追従する）。
        let self_pubkey_cell = Arc::new(RwLock::new(self_pubkey));
        // identity 切替の実体（runner+cli+セルを capture）。ツールから呼ばれる。
        let admin: Arc<dyn NostrIdentityAdmin> = Arc::new(LoopIdentityAdmin {
            runner: runner.clone(),
            cli: cli.clone(),
            self_pubkey: self_pubkey_cell.clone(),
        });
        let handle = tokio::spawn(async move {
            run_nostr_loop(runner, cli, agent, config, self_pubkey_cell, admin).await;
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
    self_pubkey: SelfPubkey,
    admin: Arc<dyn NostrIdentityAdmin>,
) {
    let mut seen = SeenEvents::new(512);
    loop {
        match run_watch_once(
            &runner,
            &cli,
            &agent_id,
            &config,
            &self_pubkey,
            &admin,
            &mut seen,
        )
        .await
        {
            Ok(()) => warn!(agent_id, "nostr watch exited; restarting after backoff"),
            Err(e) => error!(agent_id, error = %e, "nostr watch error; restarting after backoff"),
        }
        tokio::time::sleep(WATCH_RESTART_DELAY).await;
    }
}

/// 1 回分の watch 購読（プロセス寿命ぶん）。
#[allow(clippy::too_many_arguments)]
async fn run_watch_once<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    config: &NostrConfig,
    self_pubkey: &SelfPubkey,
    admin: &Arc<dyn NostrIdentityAdmin>,
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
        // 自分の投稿はスキップ（自己返信ループ防止）。identity 切替に追従するため
        // 共有セルから毎回読む。
        if *self_pubkey.read().unwrap() == event.pubkey {
            debug!(agent_id, "nostr: skipping own event");
            continue;
        }
        // 再処理防止（replay/重複）。
        if !seen.check_and_insert(&event.id) {
            debug!(agent_id, "nostr: skipping already-processed event");
            continue;
        }
        handle_event(runner, cli, agent_id, admin, &event).await;
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
    admin: &Arc<dyn NostrIdentityAdmin>,
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

    let gw = NostrGatewayActions::new(cli.clone()).with_admin(admin.clone());
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
            if let Err(e) = cli.reply(agent_id, event.reply_target(), reply, None).await {
                warn!(agent_id, error = %e, "nostr implicit reply failed");
            }
        }
        Err(e) => error!(agent_id, error = %e, "nostr agent run failed"),
    }
}

/// watch ループが握る identity 切替の実体。runner（DB）+ cli + self_pubkey セルを capture し、
/// 生成鍵を本鍵に採用する。**watch プロセスは再起動しない**（鍵非依存）。self_pubkey セルを
/// 新 pubkey へ更新するだけで、以後の自己スキップが新 identity に追従する。
struct LoopIdentityAdmin<R: NostrAgentRunner> {
    runner: R,
    cli: NostaroCli,
    self_pubkey: SelfPubkey,
}

#[async_trait::async_trait]
impl<R: NostrAgentRunner> NostrIdentityAdmin for LoopIdentityAdmin<R> {
    async fn adopt_generated_identity(&self, agent_id: &str, npub: &str) -> anyhow::Result<String> {
        // 生成鍵（自分が作ったもの）の nsec をサーバ内から読む。存在チェックで
        // 「自分が生成した鍵のみ採用可」を担保。秘密鍵は外へ出さない。
        let nsec = NostaroCli::read_generated_key(agent_id, npub)?;
        // 既存設定（relays/filter を継承）。未設定なら採用しない。
        let row = self.runner.get_nostr_config(agent_id).ok_or_else(|| {
            anyhow::anyhow!("Nostr 未設定です。先に Nostr を設定してから本鍵を切り替えてください")
        })?;
        let config = crate::config_from_row(&row);
        let relays = config.effective_relays();
        // ロールバック用に旧状態を控える。
        let old_secret = row.secret_key.clone();
        let old_pubkey = self.self_pubkey.read().unwrap().clone();

        // 1) config.toml を新鍵で再生成（0600・アトミック）。send/pubkey はこれを読む。
        NostaroCli::materialize_config(agent_id, &nsec, &relays, None)?;

        // 2) 新 pubkey を取得。**fail-closed**: 取れないと自己スキップが旧 pubkey のまま
        //    になり、新 identity の自分の投稿を拾って自己返信無限ループ＋LLM 課金になる
        //    （起動時ガードと同じ危険）。config を旧鍵へ巻き戻して中止する。
        let new_pubkey = match self.cli.pubkey(agent_id).await {
            Ok(pk) if !pk.trim().is_empty() => pk.trim().to_string(),
            _ => {
                if let Err(re) =
                    NostaroCli::materialize_config(agent_id, &old_secret, &relays, None)
                {
                    error!(agent_id, error = %re, "nostr: identity 切替のロールバック（config復元）に失敗");
                }
                anyhow::bail!(
                    "新しい鍵の pubkey を取得できませんでした。自己返信ループ防止のため切替を中止しました（設定は元に戻しました）"
                );
            }
        };

        // 3) 自己スキップ用セルを更新（以後の自己スキップが新 identity 追従）。
        *self.self_pubkey.write().unwrap() = new_pubkey;

        // 4) DB を最後に更新。失敗したら config/セルを旧状態へ巻き戻す（DB=旧 / config=新
        //    の不整合＝再起動で勝手に切替完了する事故を防ぐ）。
        if let Err(e) = self.runner.set_nostr_secret_key(agent_id, &nsec) {
            if let Err(re) = NostaroCli::materialize_config(agent_id, &old_secret, &relays, None) {
                error!(agent_id, error = %re, "nostr: identity 切替のロールバック（config復元）に失敗");
            }
            *self.self_pubkey.write().unwrap() = old_pubkey;
            return Err(e).context("DB の本鍵更新に失敗（設定を元に戻しました）");
        }
        Ok(npub.to_string())
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
