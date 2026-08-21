//! Per-agent Discord Bot gateway manager.
//!
//! Each agent can have its own Discord Bot token, managed independently.
//! `DiscordGatewayManager` handles lifecycle (start/stop) for all per-agent gateways.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::task::JoinHandle;
use tracing::{error, info};

use crate::gateway::DiscordGateway;

use crate::AgentRunner;
use opencrab_actions::subtask::SubtaskRegistry;
use opencrab_actions::TimedFireRouter;
use opencrab_core::a2ui::PendingInteractionRegistry;

struct AgentGatewayEntry {
    gateway: Arc<DiscordGateway>,
    handle: JoinHandle<()>,
}

/// per-agent ゲートウェイを起動する**前**の owner 前処理: 正規化して、未設定なら警告する。
///
/// owner は入口で正規化する。DB に前後空白付きで保存された既存行でも、
/// 「DM は通るのに owner 専用 UI だけ無言で拒否される」半端な状態を作らない
/// （下位の form/modal 側は生比較のまま。判定述語の共通化は #174）。
///
/// per-agent 経路は共有（TOML）ゲートウェイ側の起動警告に載らないので、ここでも
/// owner 未設定を知らせる（復元経路 `restore_from_db` も通る）。
///
/// `start_agent_gateway` 本体は `DiscordGateway::start()` で実ネットワークに出るため
/// そのままではテストできない。ネットワークに触らない前処理だけをこの関数に切り出し、
/// 戻り値（正規化済み owner）を呼び出し側に使わせることで、警告と正規化の両方を
/// 単体テストで押さえる。
///
/// `#[deny(dead_code)]` は「この関数が呼ばれ続けること」を保証するための保険。
/// 将来 `start_agent_gateway` をリファクタして呼び出しを落とすと、警告ではなく
/// コンパイルエラーになる（CI は警告では落ちないため、警告では歯止めにならない）。
/// 呼び出しが消えると owner の入口正規化も消え、レガシー空白付きの行で
/// 「DM は通るのに owner 専用 UI だけ無言で拒否」が復活してしまう。
#[deny(dead_code)]
fn prepare_owner_for_gateway(agent_id: &str, owner_discord_id: &str) -> String {
    let owner_discord_id = owner_discord_id.trim();
    crate::owner_warning::warn_if_agent_gateway_owner_unset(agent_id, owner_discord_id);
    owner_discord_id.to_string()
}

pub struct DiscordGatewayManager<T: AgentRunner> {
    // std RwLock（tokio ではない）: is_running を同期メソッドにするため。
    // ガードを await 跨ぎで保持しないこと（各メソッドでスコープを閉じる）。
    gateways: RwLock<HashMap<String, AgentGatewayEntry>>,
    state: T,
    // #588 TimedFire / #603: per-agent ゲートウェイのループを、この体の Discord 受け口として
    // `timed_fire_router` へ登録するための共有 Arc（scheduler が発火時に per-agent 優先で引く）。
    // **必須**（`new` の引数）にしてあるので配線し忘れが起きえない（#602 で忘れて本番が止まった。
    // Option + builder だと呼び忘れてもコンパイルが通ってしまう）。
    timed_fire_router: Arc<TimedFireRouter>,
}

impl<T: AgentRunner> DiscordGatewayManager<T> {
    /// `timed_fire_router` は**必須**。scheduler の時刻発火をこのマネージャの per-agent ループへ
    /// 届けるための受け口レジストリで、渡さないと発火が届かない（#603）。型で強制することで
    /// #602 のような「配線し忘れて黙って全 skip」を再発させない。
    pub fn new(state: T, timed_fire_router: Arc<TimedFireRouter>) -> Self {
        Self {
            gateways: RwLock::new(HashMap::new()),
            state,
            timed_fire_router,
        }
    }

    /// Start a per-agent Discord gateway with the given token.
    pub async fn start_agent_gateway(
        &self,
        agent_id: &str,
        token: &str,
        owner_discord_id: &str,
    ) -> anyhow::Result<()> {
        // 起動前の owner 前処理（正規化 + 未設定警告）。テストは下の `tests` モジュール。
        let owner_normalized = prepare_owner_for_gateway(agent_id, owner_discord_id);
        let owner_discord_id = owner_normalized.as_str();

        // Stop existing gateway for this agent if running.
        self.stop_agent_gateway(agent_id).await;

        let pending_interaction_registry: PendingInteractionRegistry =
            Arc::new(dashmap::DashMap::new());
        let form_modal_resolver = Some(crate::form_modal::form_modal_resolver(
            pending_interaction_registry.clone(),
        ));
        let gateway = Arc::new(DiscordGateway::with_form_modal_resolver(
            token,
            form_modal_resolver,
        ));
        gateway.start().await?;

        // #489: この bot 自身の Discord user id を co_agent 逆引き表へ書き戻す。
        //
        // 出所は **bot_token で認証した自分自身**（`get_current_user` = `GET /users/@me`）。
        // 受信メッセージの author からは決して書かない（外部が「user_id ↔ agent UUID」を
        // 仕込めると任意ユーザーが co_agent に化ける）。best-effort: 取得や書き込みに失敗しても
        // 起動は続ける。書けなければ `bot_user_id` は空のままで、co_agent 判定は fail-closed
        // （逆引き不可 → Agent 権限）に倒れるだけで安全側。
        //
        // ※ `DbGuard` は `!Send` なので、ネットワーク待ち（await）は先に済ませ、DB ロックは
        //    await を挟まない同期ブロックに閉じる。
        match gateway.http().get_current_user().await {
            Ok(user) => {
                let bot_user_id = user.id.get().to_string();
                match self.state.db().lock() {
                    Ok(conn) => {
                        if let Err(e) = opencrab_db::queries::set_agent_discord_bot_user_id(
                            &conn,
                            agent_id,
                            &bot_user_id,
                        ) {
                            error!(agent_id = %agent_id, error = %e, "#489: bot_user_id の書き戻しに失敗（co_agent 逆引きは fail-closed のまま）");
                        }
                    }
                    Err(e) => {
                        error!(agent_id = %agent_id, error = %e, "#489: DB ロック取得に失敗し bot_user_id を書き戻せず（co_agent 逆引きは fail-closed のまま）");
                    }
                }
            }
            Err(e) => {
                error!(agent_id = %agent_id, error = %e, "#489: 自分の Discord user id を取得できず co_agent 逆引き表を更新できなかった（co_agent は fail-closed のまま）");
            }
        }

        // auto-dispatch の登録簿。停止（`cancel_subtask`）は gateway 非依存層の実装が
        // 同じ Arc を run 経由（`RunRequest::with_dispatch`）で受け取るため、この
        // registry はループへ渡すだけでよい（#157 S2 で gateway_actions からは外した）。
        let subtask_registry_for_loop: SubtaskRegistry = Arc::new(dashmap::DashMap::new());

        // Create event channel for A2UI and other async events
        let (event_tx, event_rx) = crate::message_loop::create_event_channel();

        // #588 TimedFire: このループをこの体の per-agent Discord 受け口として登録する。
        // scheduler は発火時に per-agent 優先で引く（#400 と同型）ので、以降この体の時刻発火は
        // 自分のボットで出るこのループへ届く。停止時（`stop_agent_gateway`）に解除する。
        // #603: router は必須（`new` の引数）なので「未配線で登録できない」経路は型で消えた。
        self.timed_fire_router.register_per_agent(
            opencrab_actions::gateway_kinds::DISCORD,
            agent_id,
            Arc::new(crate::message_loop::DiscordTimedFireSink {
                event_tx: event_tx.clone(),
            }),
        );
        // #601: 登録が起きたことを起動時に 1 行残す（型で強制した後も、運用で「実際に登録された」
        // ことがログで見えるほうが良い・統括判断）。
        info!(
            agent_id = %agent_id,
            transport = "discord",
            "timed-fire: 受け口を登録（per-agent Discord loop）"
        );

        // このゲートウェイの保留対話は上で作り直した登録簿（＝空）にしか無いので、
        // 前回稼働分の `pending` 行は再開できない。期限切れとして明示的に閉じる。
        // ただし**このエージェント分だけ**にする: per-agent ゲートウェイは実行中にも
        // 再起動されるため（ダッシュボード操作）、全件を閉じると同時に動いている別
        // エージェントの生きた保留対話まで落ちる（#196）。
        self.state.cleanup_stale_interactions_for_agent(agent_id);

        let gateway_actions: Arc<dyn opencrab_gateway::GatewayActions> = Arc::new(
            crate::DiscordGatewayActions::new(
                gateway.http().clone(),
                self.state.db().clone(),
                self.state.workspace_base().to_string(),
                None,
            )
            .with_a2ui(pending_interaction_registry.clone(), event_tx.clone())
            .with_owner_discord_id(owner_discord_id),
        );

        let loop_state = self.state.clone();
        let loop_gateway = gateway.clone();
        let agent_ids = vec![agent_id.to_string()];
        let owner = owner_discord_id.to_string();

        let handle = tokio::spawn(async move {
            crate::run_discord_loop(
                loop_gateway,
                loop_state,
                agent_ids,
                gateway_actions,
                owner,
                Some(pending_interaction_registry),
                Some((event_tx, event_rx)),
                // per-agent ゲートウェイは enabled な設定から起動される側なので
                // 専用設定スキップは無効（true にすると自分自身を skip してしまう）。
                false,
                // VC 対話 v1 は共有（TOML）ゲートウェイのみ対応。per-agent 側は未配線。
                None,
                subtask_registry_for_loop,
            )
            .await;
        });

        {
            let mut gateways = self.gateways.write().unwrap();
            gateways.insert(agent_id.to_string(), AgentGatewayEntry { gateway, handle });
        }

        info!(agent_id = %agent_id, "Per-agent Discord gateway started");
        Ok(())
    }

    /// Stop a per-agent Discord gateway.
    pub async fn stop_agent_gateway(&self, agent_id: &str) {
        let entry = {
            let mut gateways = self.gateways.write().unwrap();
            gateways.remove(agent_id)
        };

        if let Some(entry) = entry {
            entry.gateway.shutdown().await;
            entry.handle.abort();
            // #588 TimedFire: 死んだループへ発火が消えないよう受け口を解除する（以降この体の
            // 時刻発火は共有ゲートウェイへ落ちる・#400 の動的フォールバック）。#603: router は必須。
            self.timed_fire_router
                .unregister_per_agent(opencrab_actions::gateway_kinds::DISCORD, agent_id);
            info!(agent_id = %agent_id, "Per-agent Discord gateway stopped");
        }
    }

    /// Check if a per-agent gateway is running.
    ///
    /// 同期メソッド: 共有ゲートウェイのメッセージループが per-message で
    /// 「専用ゲートウェイが実際に稼働しているか」を判定するのに使う（#40）。
    pub fn is_running(&self, agent_id: &str) -> bool {
        let gateways = self.gateways.read().unwrap();
        gateways
            .get(agent_id)
            .map(|e| !e.handle.is_finished())
            .unwrap_or(false)
    }

    /// Get the HTTP client for a per-agent gateway.
    pub fn get_http_for_agent(&self, agent_id: &str) -> Option<Arc<serenity::http::Http>> {
        let gateways = self.gateways.read().unwrap();
        gateways.get(agent_id).map(|e| e.gateway.http().clone())
    }

    /// Restore all enabled agent Discord configs from DB and start their gateways.
    pub async fn restore_from_db(&self) {
        for cfg in self.state.list_enabled_discord_configs() {
            if let Err(e) = self
                .start_agent_gateway(&cfg.agent_id, &cfg.bot_token, &cfg.owner_discord_id)
                .await
            {
                error!(
                    agent_id = %cfg.agent_id,
                    error = %e,
                    "Failed to restore per-agent Discord gateway"
                );
            }
        }
    }

    /// Shutdown all per-agent gateways.
    pub async fn shutdown_all(&self) {
        let entries: Vec<(String, AgentGatewayEntry)> = {
            let mut gateways = self.gateways.write().unwrap();
            gateways.drain().collect()
        };

        for (agent_id, entry) in entries {
            entry.gateway.shutdown().await;
            entry.handle.abort();
            info!(agent_id = %agent_id, "Per-agent Discord gateway stopped (shutdown_all)");
        }
    }
}

/// エージェント単位ライフサイクルの共通契約（#191 段階2）。
///
/// 既存の具象メソッドへ委譲するだけで、挙動は変えない。契約側の `start` は資格情報を
/// 引数に取らない（transport ごとに形が違うため）ので、ここで DB の設定行を読む。
///
/// **owner の入口正規化はここでは行わない。** `start_agent_gateway` の中の
/// `prepare_owner_for_gateway` がそのまま担う（トレイト経由でも生の呼び出しでも
/// 同じ 1 箇所を通る）。ここで trim を重ねると「正規化の置き場所が 2 つある」形になり、
/// 片方だけ消えたときに `DM は通るのに owner 専用 UI だけ無言で拒否` が復活する。
///
/// ## 起動条件のガード（#191 段階2 PR3）
///
/// `start` は [`gateway_will_start`] で「有効フラグが立っていて、かつトークンが空白で
/// ない」ことを確認してから起動する。**これは REST ハンドラ（`PATCH /discord`）が
/// 呼び出しの手前で行っていた判定を、述語ごとそのまま持ち上げたもの**で、引数も同じ
/// （同一の DB 行の `enabled` と `bot_token`）。呼び出し側の規約にすると呼び出し口が
/// 増えるたびに忘れうるので、型の内側に閉じて「忘れても安全」にする。
///
/// [`gateway_will_start`]: crate::gateway_will_start
#[async_trait::async_trait]
impl<T: AgentRunner> opencrab_actions::AgentGatewayLifecycle for DiscordGatewayManager<T> {
    fn kind(&self) -> &'static str {
        opencrab_actions::gateway_kinds::DISCORD
    }

    async fn start(&self, agent_id: &str) -> anyhow::Result<()> {
        let cfg = self
            .state
            .get_discord_config(agent_id)
            .ok_or_else(|| anyhow::anyhow!("Discord 設定がありません（agent_id={agent_id}）"))?;
        // 起動条件のガード。`gateway_will_start` は共有ゲートウェイの起動判定・owner 未設定
        // 警告と同じ述語で、条件が 1 箇所に閉じている（空白だけのトークンは「無し」扱い）。
        if !crate::gateway_will_start(cfg.enabled, &cfg.bot_token) {
            return Err(opencrab_actions::StartDeclined::err(
                opencrab_actions::gateway_kinds::DISCORD,
                agent_id,
                format!(
                    "enabled={} / bot_token={}",
                    cfg.enabled,
                    if cfg.bot_token.trim().is_empty() {
                        "空"
                    } else {
                        "あり"
                    }
                ),
            ));
        }
        // enabled フラグの**書き換え**は呼び出し側の責務（DB の方針でありライフサイクル
        // ではない）。ここで見るのは読み取りだけ。
        self.start_agent_gateway(agent_id, &cfg.bot_token, &cfg.owner_discord_id)
            .await
    }

    async fn stop(&self, agent_id: &str) {
        self.stop_agent_gateway(agent_id).await;
    }

    fn is_running(&self, agent_id: &str) -> bool {
        DiscordGatewayManager::is_running(self, agent_id)
    }

    async fn restore_all(&self) {
        self.restore_from_db().await;
    }

    async fn shutdown_all(&self) {
        DiscordGatewayManager::shutdown_all(self).await;
    }

    /// 稼働中の per-agent ゲートウェイの HTTP クライアントからツール実行の実体を組む
    /// （capability / #191 段階2 PR4）。稼働していなければ `None`。
    ///
    /// A2UI の描画面と owner を**付けない**のは意図的な差で、
    /// 意図的な差である: それらは受信ループが持つ per-connection の状態
    /// （`start_agent_gateway` が作る保留対話の登録簿・イベント送信口）に紐づいており、
    /// 接続の外から組み直すと**別の登録簿**を指してしまう。
    fn gateway_actions_for(
        &self,
        agent_id: &str,
    ) -> Option<Arc<dyn opencrab_gateway::GatewayActions>> {
        let http = self.get_http_for_agent(agent_id)?;
        Some(Arc::new(crate::DiscordGatewayActions::new(
            http,
            self.state.db().clone(),
            self.state.workspace_base().to_string(),
            None,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::prepare_owner_for_gateway;
    use crate::owner_warning::capture::captured_logs;

    /// 起動経路が owner を正規化して渡す（DB のレガシー行が空白付きでも同じ）。
    #[test]
    fn start_path_normalizes_owner() {
        assert_eq!(
            prepare_owner_for_gateway("crab", "  123456789012345678\n"),
            "123456789012345678"
        );
        assert_eq!(prepare_owner_for_gateway("crab", "   "), "");
    }

    /// 起動経路そのものが owner 未設定の警告を出す。
    ///
    /// `owner_warning` の純関数テストだけでは「呼ばれているか」を保証できない。
    /// ここでは起動前処理を実際に呼び、warn イベントが出ることを確認する。
    #[test]
    fn start_path_warns_when_owner_is_unset() {
        for owner in ["", " ", " \t\n"] {
            let logs = captured_logs(|| {
                prepare_owner_for_gateway("agent-under-test", owner);
            });
            assert!(logs.contains("WARN"), "warn レベルで出ること: {logs}");
            assert!(
                logs.contains("empty owner_discord_id"),
                "owner={owner:?} で本文が出ること: {logs}"
            );
            assert!(
                logs.contains("agent-under-test"),
                "どのエージェントか分かること: {logs}"
            );
        }
    }

    /// owner 設定済みなら起動経路は黙る（「常に出ている警告」を作らない）。
    #[test]
    fn start_path_is_silent_when_owner_is_set() {
        let logs = captured_logs(|| {
            prepare_owner_for_gateway("crab", "  123456789012345678  ");
        });
        assert!(logs.trim().is_empty(), "余計な警告を出さないこと: {logs}");
    }
}
