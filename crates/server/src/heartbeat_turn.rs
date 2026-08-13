//! ハートビート 1 ターンの実行と、サブタスク決着からの**継続ターン**（#440）。
//!
//! # なぜこの器があるか
//!
//! ハートビート（以下 HB）のターンは 2 経路から起きる。
//!
//! 1. **tick**: 時間で発火する経路（中央スケジューラ `scheduler.rs` の `run_one_fire`）。
//! 2. **継続ターン**: そのターンが dispatch した非同期サブタスクが決着したときの再開。
//!
//! 両者は「文脈を組む → `run_agent_response` → 応答を配送・記録 → `heartbeat_log`」まで
//! まったく同じことをする。**#588 Stage 3 でハートビートは専用の語彙（旧 SPEAK/LEARN/IDLE）を
//! 撤去し、通常のターンへ寄せた**: 応答本文（`NO_REPLY` 以外）をそのままチャンネルへ配送し、
//! 投稿した回だけ通常の配送記録（[`opencrab_server::transcript::record_outbound_reply`]）を
//! 残す（沈黙＝`NO_REPLY`＝無配送・無記録）。以前 heartbeat が `NoopCompletionSink` で継続
//! ターンを持たなかった理由の 1 つが「sink で resume させると配送とログ記録を sink 側へ
//! 複製する必要がある」だった（#440）。ここに 1 実装だけ置き、両経路がそれを通ることで
//! その複製を構造的に無くす。
//!
//! # 二重応答をどう防ぐか
//!
//! 見送りのもう 1 つの理由が「session ロックが無いため次 tick と競合する」だった。
//! [`HeartbeatTurnRunner`] は [`SessionLocks`] を 1 つ持ち、**唯一の入口**
//! [`HeartbeatTurnRunner::run_turn`] がその下でターンを走らせる。tick も継続ターンも
//! この入口しか通れない（実体の [`HeartbeatTurnRunner::turn`] は private）ので、同一 HB
//! セッションに 2 本のターンが並行しない。Discord / web / Nostr が同じ不変条件
//! （RFC #152 §6）に使っているのと同じロック実装（`opencrab_actions::session_runtime`）。
//!
//! # 他ゲートウェイとの対応
//!
//! - Discord: `DiscordCompletionSink` → `LoopEvent::SubtaskCompleted` →
//!   `process_subtask_completed`（`crates/discord/src/message_loop.rs`）。
//! - Nostr: `NostrResponder`（`crates/nostr/src/sink.rs`）。
//! - web: `WebCompletionSink`。
//!
//! いずれも「完了本文は運ばない（`settle_completed` が親セッションログへ永続化済み）・
//! system prompt に `[subtask_completed: …]` を足す・会話は DB から組み直す」で共通。
//! ここも同じ形にしてある。HB ターンの宛先は実会話セッション（#573 Stage B で統合済み）で、
//! `settle_completed` が書いた完了本文は通常の `build_conversation_string` がそのまま拾う。
//! この依存は継続ターン（いま渡す）でも次 tick（拾い直す）でも同じで、#440 の前後で変わらない。

use std::sync::Arc;

use opencrab_actions::transcript::{AgentReplyContext, OutboundReplyRecord, TranscriptSource};
use opencrab_actions::{
    CallerIdentity, RunRequest, SessionLocks, SettleKind, SubtaskCompletionSink, SubtaskSettled,
};
use opencrab_db::queries::SessionFireTarget;
use opencrab_gateway::GatewayActions;
use opencrab_server::subtask_registries::SubtaskRegistries;
use opencrab_server::AppState;

use crate::heartbeat_delivery::{self, HeartbeatDiscordHttp};

/// ハートビート由来の実行を表すゲートウェイ名（`RunRequest.gateway`）。`RuntimeInfo` に
/// 載り、継続ターンの受け口が「HB 由来か」を見分ける目印になる。
///
/// （#573 Stage C まで HB 専用セッションの `SessionRow.mode` も兼ねていたが、専用セッション
/// 生成を撤去したため現在は run 側のゲートウェイ名としてのみ使う。）
///
/// **`speaker_id='heartbeat'`（scaffolding 行の目印）とは別概念**。あちらは
/// `opencrab_db::queries::HEARTBEAT_SPEAKER_ID`。値が同じでも用途が違うので混同しないこと。
pub(crate) const HEARTBEAT_GATEWAY: &str = "heartbeat";

/// 1 ターンの宛先。tick と継続ターンで同じものを使う。
///
/// `channel_id` が空文字ならエージェント単位 tick（channel を持たない発話 / #238）。
/// `channel_id` / `guild_id` は発話の配送先・二重記録（#425）とログ表示に使う。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeartbeatTarget {
    pub agent_id: String,
    pub session_id: String,
    pub channel_id: String,
    pub guild_id: String,
    /// 実会話（`[Channel conversation]`）セッションの ID。発火先種別から解決した
    /// `nostr-{agent}` / `discord-{agent}-{guild}-{channel}`（#508 / #404）。以前は
    /// `build_context` が `guild_id`/`channel_id` から Discord 書式を組み直していたため、
    /// Nostr（両 ID が空）では必ず解決に失敗し外の会話が 1 行も入らなかった。発火先種別を
    /// 持つ `run_one_fire` で [`opencrab_db::queries::SessionFireTarget::channel_session_id`]
    /// により解決してここへ載せる。継続ターン(#440)も `target.clone()` でこの値を引き継ぐ。
    pub channel_session_id: String,
    /// 整形済みのハートビート指示文（#501）。`build_context` がその tick の system
    /// プロンプトへ 1 度だけ載せる。以前は HB セッションログへ挿入して会話へ載せていたが、
    /// 毎 tick 積み上がるため system プロンプト注入へ移した（`scheduler::format_heartbeat_prompt`）。
    /// sink が `target.clone()` を保持するので、継続ターン(#440)にも同じ指示が伝わる。
    pub instructions_prompt: String,
    /// 指示文の由来（`heartbeat_log` の `result_json` に載る診断値）。
    pub instructions_source: &'static str,
    /// 発火元の種別（#591）。SPEAK の配送先を「試す順番」ではなくこの種別から直接決める
    /// （`heartbeat_delivery::DeliveryRoute::from_fire_target`）。scheduler が発火時の
    /// `SessionFireTarget` をそのまま載せる。以前は配送が transport 横断の試す順番で決まり、
    /// Discord チャンネルの発火が Nostr へ持って行かれていた（#591）。
    pub fire_target: opencrab_db::queries::SessionFireTarget,
}

/// ターンの発火元。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TurnOrigin {
    /// 時間発火（従来の tick）。
    Tick { tick: u64 },
    /// dispatch したサブタスクの決着による継続ターン（#440）。
    SubtaskResume {
        subtask_id: String,
        exit_reason: String,
    },
}

impl TurnOrigin {
    /// system prompt へ足す接尾。
    ///
    /// tick は `None`（指示文＝出力形式の規約行は `HeartbeatTarget::instructions_prompt` が
    /// system プロンプトへ載せる / #501）。継続ターンは Discord / Nostr と同じく
    /// `[subtask_completed: …]` マーカーを足す。**本文は載せない**: 完了本文は
    /// `settle_completed` が親セッションログへ永続化済みで、会話文字列の再構築で自然に載る
    /// （RFC §1.3）。出力形式の規約行はここには書かない — 指示文（`instructions_prompt`）が
    /// system プロンプトに常在するようになり（文脈予算に左右されない）、重複するため（#501）。
    ///
    /// 冒頭 1 文は `exit_reason` で分岐する（#443）。継続ターンを起こす
    /// `SettleKind::Completed` は completed / stopped_by_limit / error / timeout の
    /// **どれでも**発火するので、一律「完了しました」と告げると 30 分でタイムアウトした
    /// subtask にも「完了」と伝わり、同じ prompt 内のマーカー（`exit_reason=timeout`）と
    /// 食い違う。
    fn prompt_suffix(&self) -> Option<String> {
        match self {
            Self::Tick { .. } => None,
            Self::SubtaskResume {
                subtask_id,
                exit_reason,
            } => {
                let outcome = settle_outcome(exit_reason).sentence;
                Some(format!(
                "[ハートビート] 依頼していたバックグラウンド処理が{outcome}。詳細は直前の会話ログの subtask_completed に入っています。\n\
                 状況を見て、いま発話するかどうかは自分で決めてください。\n\
                 [subtask_completed: subtask_id={subtask_id}, exit_reason={exit_reason}]"
            ))
            }
        }
    }

    /// 内省メモに載る発火元ラベル。tick は移設前の文言と同一（`tick {n}`）。
    ///
    /// 継続ターンのラベルも `exit_reason` で分岐する。内省メモは curated_memory へ残って
    /// 後のターンが読むため、prompt と同じ理由で「完了」を断言できない（#443）。
    /// `completed` のときの文言は移設前と同一（`subtask {id} 完了`）。
    fn label(&self) -> String {
        match self {
            Self::Tick { tick } => format!("tick {tick}"),
            Self::SubtaskResume {
                subtask_id,
                exit_reason,
            } => format!("subtask {subtask_id} {}", settle_outcome(exit_reason).label),
        }
    }
}

/// `exit_reason` を人へ見せる言い回しへ写す（#443）。
struct SettleOutcome {
    /// system prompt の 1 文目に入る述部（「…バックグラウンド処理が{sentence}。」）。
    sentence: &'static str,
    /// 内省メモ・ログのラベルへ入る短い語。
    label: &'static str,
}

/// 決着理由 → 言い回し。値の出所は `subtask_spawn.rs` の 1 箇所
/// （`stopped_by_limit` / `completed` / `error` / `timeout`）。
///
/// 未知の値は**断定しない**（「終了しました」）。正確な値は同じ prompt 内の
/// `[subtask_completed: … exit_reason=…]` マーカーがそのまま持つので、ここで推測を足す
/// 必要がない。
fn settle_outcome(exit_reason: &str) -> SettleOutcome {
    match exit_reason {
        "completed" => SettleOutcome {
            sentence: "完了しました",
            label: "完了",
        },
        "stopped_by_limit" => SettleOutcome {
            sentence: "反復上限に達して途中で打ち切られました",
            label: "途中打ち切り",
        },
        "error" => SettleOutcome {
            sentence: "エラーで失敗しました",
            label: "失敗",
        },
        "timeout" => SettleOutcome {
            sentence: "時間切れで打ち切られました",
            label: "タイムアウト",
        },
        _ => SettleOutcome {
            sentence: "終了しました",
            label: "決着",
        },
    }
}

/// HB ターンの推論実行（本番は `process::run_agent_response`）。
///
/// `deliver_heartbeat_speech` が `&AppState` ではなく `&AgentGatewayRegistry` を受けるのと
/// 同じ理由でここも境界を切ってある: ターンの前後（文脈構築・応答記録・決定の解釈・
/// 配送）の結線を、LLM を呼ばずに単体テストできるようにするため。
#[async_trait::async_trait]
pub(crate) trait HeartbeatEngine: Send + Sync {
    async fn run(&self, req: RunRequest) -> anyhow::Result<opencrab_core::EngineResult>;
}

/// 本番の実行口。`AppState` を持つのはここだけ。
struct AppStateEngine {
    state: AppState,
}

#[async_trait::async_trait]
impl HeartbeatEngine for AppStateEngine {
    async fn run(&self, req: RunRequest) -> anyhow::Result<opencrab_core::EngineResult> {
        opencrab_server::process::run_agent_response(&self.state, req).await
    }
}

/// HB ターンの実行主体。tick ループと完了 sink が**同じ 1 つ**を共有する。
///
/// 共有が要るのは [`SessionLocks`]（同一 HB セッションの直列化）と registry
/// （`cancel_subtask` の到達性 / #169）で、どちらも「セッション単位の実行状態」。
///
/// 生成はプロセス起動時に 1 つだけ（`main.rs` が `from_state` で作り `run_scheduler` へ渡す /
/// #439・#465 の中央スケジューラ）。スケジューラはこの 1 実体を clone して**全発火・全継続
/// ターンで共有**し、hot-reload でも**張り直さない**（config 変更は scheduler の発火エントリを
/// rebuild させるだけで、runner 実体は差し替わらない）。したがって [`SessionLocks`] はプロセス
/// 全域で 1 つに保たれ、tick と継続ターンは常に同じロックを共有する。旧モデル（発火ループ 1 本
/// につき runner を 1 つ生成し、hot-reload で張り直す）にあった「張り直しの瞬間に旧世代の継続
/// ターンが新世代の tick とロックを共有しない」世代跨ぎレースは、runner が単一になった現行
/// モデルでは発生しない。
pub(crate) struct HeartbeatTurnRunner {
    db: opencrab_db::Db,
    engine: Arc<dyn HeartbeatEngine>,
    /// 発話配送の登録簿（`deliver_heartbeat_speech` の第 1 引数）。
    gateways: Arc<opencrab_actions::AgentGatewayRegistry>,
    discord_http: Arc<HeartbeatDiscordHttp>,
    /// dispatch registry。**agent 単位**で共有する（#169）。
    registries: Arc<SubtaskRegistries>,
    default_model: String,
    compaction_ratio: f64,
    locks: SessionLocks,
}

impl HeartbeatTurnRunner {
    pub(crate) fn from_state(
        state: &AppState,
        discord_http: Arc<HeartbeatDiscordHttp>,
    ) -> Arc<Self> {
        Arc::new(Self {
            db: state.db.clone(),
            engine: Arc::new(AppStateEngine {
                state: state.clone(),
            }),
            gateways: state.gateways.clone(),
            discord_http,
            registries: state.subtask_registries.clone(),
            default_model: state.default_model.clone(),
            compaction_ratio: state.compaction_ratio,
            locks: SessionLocks::new(),
        })
    }

    /// **唯一の入口**。同一 HB セッションのロック下で 1 ターン走らせる。
    ///
    /// `None` は「ターンを開始できなかった」（文脈の組み立てに失敗）。移設前の tick ループが
    /// `continue` していた経路と同じで、呼び出し側は直前の決定を保つ。
    pub(crate) async fn run_turn(
        self: &Arc<Self>,
        target: &HeartbeatTarget,
        origin: TurnOrigin,
    ) -> Option<()> {
        let fut = self.turn(target, &origin);
        self.locks.run_serialized(&target.session_id, fut).await
    }

    /// 継続ターンを起動する（呼び出し側はブロックしない）。
    ///
    /// ロックの取得は spawn した中で行う。sink は `settle_completed` の途中で同期的に
    /// 呼ばれるため、ここで待つとサブタスクの決着処理そのものが止まる（Discord / Nostr /
    /// web の sink が resume を spawn しているのと同じ理由）。
    fn spawn_turn(self: &Arc<Self>, target: HeartbeatTarget, origin: TurnOrigin) {
        let runner = Arc::clone(self);
        tokio::spawn(async move {
            runner.run_turn(&target, origin).await;
        });
    }

    /// このターンが dispatch したサブタスクの決着を受ける口。
    pub(crate) fn completion_sink(
        self: &Arc<Self>,
        target: &HeartbeatTarget,
    ) -> Arc<dyn SubtaskCompletionSink> {
        Arc::new(HeartbeatCompletionSink {
            runner: Arc::clone(self),
            target: target.clone(),
        })
    }

    /// heartbeat 経路の `RunRequest` を組む（#169 / #440 / #588 Stage 3）。
    ///
    /// 非ブロック dispatch（RFC #152 S3a）を有効化する。これにより heartbeat の tick は
    /// 長時間ツールで塞がれず、`cancel_subtask`（#161）からも停止できる。
    ///
    /// - registry: **agent 単位**で `AppState` が保持しているものを共有する。tick /
    ///   チャンネル / heartbeat ループ再起動（設定変更）を跨いで同一 Arc なので、前 tick で
    ///   dispatch した subtask を後続 tick の `cancel_subtask` が引ける（使い捨ての DashMap
    ///   では常に not found）。
    /// - sink: [`HeartbeatCompletionSink`]（#440 で `NoopCompletionSink` から変更）。決着で
    ///   HB の継続ターンを起動する。継続ターンもこの口を通るので、そこから dispatch した
    ///   サブタスクの決着も同じように継続する（Discord の resume と同じく深さ 0 の通常
    ///   ターンなので、spawn 側の既存制限がそのまま効く）。
    /// - caller: **常に `Owner`**。HB は「本人が自分の意思で動くターン」なので tick も継続
    ///   ターンも同じ人格・同じ権限で走る。`SubtaskSettled.caller` を使わないのは、registry
    ///   不整合時の fail-closed 降格（`settle_completed` の `Agent` フォールバック）が HB
    ///   だけに効いて、tick と継続ターンで見えるツールが食い違うのを避けるため。親ターン
    ///   （tick）が `Owner` なので、引き継いでも昇格にはならない。
    /// - gateway_actions / reply_target（#588 Stage 3）: **通常ターンと同じツール環境**を渡す。
    ///   発火元 transport の `GatewayActions`（稼働していれば）と、Discord ではチャンネル ID を
    ///   ツールの既定宛先として載せる。これで HB ターンでも通常ターンと同じく gateway ツールが
    ///   使える。応答本文そのものの配送は run 内では起きず（`on_response_text` を渡さない）、
    ///   ターン後に発火元種別で `deliver_heartbeat_speech` が担う（`turn`）。`spawn_subtask` は
    ///   `SystemGatewayActions` 経由で depth0 全ランに常在するため、この配線に依らず使える。
    fn run_request(
        self: &Arc<Self>,
        target: &HeartbeatTarget,
        agent_name: &str,
        system_prompt: &str,
        conversation: &str,
    ) -> RunRequest {
        let mut req = RunRequest::new(
            &target.agent_id,
            agent_name,
            &target.session_id,
            system_prompt,
            conversation,
            HEARTBEAT_GATEWAY,
            CallerIdentity::Owner,
        )
        .with_dispatch(
            Some(self.registries.registry_for(&target.agent_id)),
            self.completion_sink(target),
        );
        if let Some(ga) = self.resolve_gateway_actions(target) {
            req = req.with_gateway_actions(ga);
        }
        // Discord はチャンネル ID をツールの既定宛先にする。Nostr broadcast は返信先ノートを
        // 持たないので reply_target を付けない（付けても配送はしない・GatewayCallContext の
        // 既定宛先に載るだけ）。
        if let SessionFireTarget::DiscordChannel { .. } = &target.fire_target {
            req = req.with_reply_target(target.channel_id.clone());
        }
        req
    }

    /// 発火元 transport の `GatewayActions` を登録簿から引く（#588 Stage 3）。
    ///
    /// 稼働していなければ `None`（Discord の共有 TOML ゲートウェイは登録簿に載らないため
    /// per-agent 未稼働だと `None` になる。その場合でも `spawn_subtask` は
    /// `SystemGatewayActions` 経由で使えるので発火本体は成立する）。
    fn resolve_gateway_actions(&self, target: &HeartbeatTarget) -> Option<Arc<dyn GatewayActions>> {
        let kind = match &target.fire_target {
            SessionFireTarget::NostrBroadcast => opencrab_actions::gateway_kinds::NOSTR,
            SessionFireTarget::DiscordChannel { .. } => opencrab_actions::gateway_kinds::DISCORD,
        };
        self.gateways
            .get(kind)?
            .gateway_actions_for(&target.agent_id)
    }

    /// ターン本体。**呼び出しは [`Self::run_turn`] 経由に限る**（直列化の担保）。
    ///
    /// #588 Stage 3: **通常のターンとして走る。** 応答本文の自動配送は**発火元の種別で変わる**:
    ///
    /// - **Discord チャンネルの発火**: 応答本文（`NO_REPLY`・空 以外）をそのままチャンネルへ
    ///   自動配送し（[`Self::deliver_speech`]）、投稿した回だけ通常の配送記録を残す
    ///   （[`Self::record_posted_reply`]）。沈黙（`NO_REPLY`）は無配送・無記録。
    /// - **ブロードキャスト（Nostr）の発火**: 応答本文を**自動配送しない**（オーナー判断）。
    ///   エージェントが `nostr_post` 等のツールで自分から投稿する（通常の Nostr の動き。ツールは
    ///   `with_gateway_actions` で渡している）。本文は投稿されないので配送記録も残さない
    ///   （ツール投稿は各ツールが自分で記録する）。**理由**: Nostr は既にツール投稿が通常運用で、
    ///   エージェント指示文が「ツールで送信した後は同じ本文を返さない」という二重投稿回避の
    ///   取り決めを持つ。本 Stage でその逃げ道（旧 `IDLE`）を廃止したうえに本文まで自動配送すると
    ///   二重投稿の道が開くため、ブロードキャストは自動配送しない。
    ///
    /// 発火自体は発火元によらず `heartbeat_log` に `fired` として残す。
    async fn turn(self: &Arc<Self>, target: &HeartbeatTarget, origin: &TurnOrigin) -> Option<()> {
        let (system_prompt, agent_name, conversation) = self.build_context(target, origin)?;

        let engine_result = self
            .engine
            .run(self.run_request(target, &agent_name, &system_prompt, &conversation))
            .await;

        match engine_result {
            Ok(result) => {
                // 通常ターンと同じ NO_REPLY 判定（`message_loop.rs`）。
                let text = result.response.trim();
                let is_silent = text.is_empty() || text == "NO_REPLY";
                match heartbeat_delivery::DeliveryRoute::from_fire_target(&target.fire_target) {
                    // Discord: 応答本文をそのままチャンネルへ自動配送し、投稿した回だけ記録する。
                    heartbeat_delivery::DeliveryRoute::DiscordChannel => {
                        if is_silent {
                            tracing::debug!(
                                agent_id = %target.agent_id,
                                session_id = %target.session_id,
                                origin = %origin.label(),
                                "Heartbeat turn: NO_REPLY（沈黙・無配送・無記録）"
                            );
                        } else {
                            self.record_posted_reply(target, origin, &result, text);
                            self.deliver_speech(target, text);
                            tracing::debug!(
                                agent_id = %target.agent_id,
                                session_id = %target.session_id,
                                channel_id = %target.channel_id,
                                origin = %origin.label(),
                                "Heartbeat turn: 応答本文を Discord チャンネルへ配送"
                            );
                        }
                    }
                    // ブロードキャスト（Nostr）: 応答本文は自動配送しない（doc 参照）。配送も記録も
                    // せず、エージェントのツール投稿に委ねる。
                    heartbeat_delivery::DeliveryRoute::Broadcast => {
                        tracing::debug!(
                            agent_id = %target.agent_id,
                            session_id = %target.session_id,
                            origin = %origin.label(),
                            "Heartbeat turn: ブロードキャスト発火は応答本文を自動配送しない（ツール投稿に委ねる）"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Heartbeat agent response failed for channel {}: {e}",
                    target.channel_id
                );
            }
        }

        self.record_heartbeat_log(target, origin);
        Some(())
    }

    /// 文脈（system prompt / エージェント名 / 会話文字列）を組む。失敗したら `None`。
    fn build_context(
        &self,
        target: &HeartbeatTarget,
        origin: &TurnOrigin,
    ) -> Option<(String, String, String)> {
        let conn = self.db.lock().unwrap();
        // heartbeat は caller=Owner で走る（`run_request`）。index も同じ caller で
        // 組み立て、Owner には全 skill を見せる（#352）。
        let (base_prompt, agent_name) = opencrab_server::process::build_agent_context(
            &conn,
            &target.agent_id,
            &CallerIdentity::Owner,
        );
        // Use per-agent model from DB, fallback to global default
        let agent_model = opencrab_db::queries::effective_model_for_agent(
            &conn,
            &target.agent_id,
            &self.default_model,
        )
        .unwrap_or_else(|_| self.default_model.clone());
        let budget = opencrab_server::process::compute_context_budget(
            &conn,
            agent_model.split(':').next().unwrap_or(""),
            agent_model.split(':').nth(1).unwrap_or(""),
            self.compaction_ratio,
        );
        // #588 Stage 1: HB ターンの宛先は実会話セッション（`session_id` == `channel_session_id`・
        // #573 Stage B で統合済み）なので、専用の `build_heartbeat_conversation_string`（実会話
        // セッションを別引数で受け、両者が等値のとき `[Channel conversation]` 節を等値フィルタで
        // 常に落とす上位ラッパ）を、通常の `build_conversation_string` へ寄せる。本番では
        // `session == channel_session` のため出力はバイト単位で不変（`conversation.rs` の
        // `collapses_to_recent_conversation_when_sessions_are_equal` が担保）。専用会話関数・定数・
        // `HeartbeatTarget.channel_session_id` フィールドの撤去は #588 Stage 4（本段は呼び出しの差し替えのみ）。
        let conversation = match opencrab_server::process::build_conversation_string(
            &conn,
            &target.session_id,
            &target.agent_id,
            budget,
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(agent_id = %target.agent_id, session_id = %target.session_id, "build_conversation_string failed: {e}");
                return None;
            }
        };
        drop(conn);

        // #501: 指示文（standing instruction）は毎ターンの system プロンプトへ 1 度だけ入れる。
        // 以前は HB セッションログ経由で会話へ載せていたが、毎 tick 積まれて「同じ指示 → IDLE」
        // の対が何十回も文脈に並び挙動を歪めていた。継続ターン(#440)は加えて
        // `[subtask_completed:…]` マーカー（`prompt_suffix`）を足す。出力形式の規約行は指示文側に
        // 1 本だけ持たせ、suffix 側には置かない（重複回避）。
        let mut system_prompt = base_prompt;
        if !target.instructions_prompt.is_empty() {
            system_prompt = format!("{system_prompt}\n\n{}", target.instructions_prompt);
        }
        if let Some(suffix) = origin.prompt_suffix() {
            system_prompt = format!("{system_prompt}\n\n{suffix}");
        }
        let conversation = opencrab_server::process::prepend_runtime_context(
            &conversation,
            "ハートビート自律行動",
        );
        Some((system_prompt, agent_name, conversation))
    }

    /// `heartbeat_log` へ 1 行残す（発火ログ・#588 Stage 3）。
    ///
    /// **`decision` 列は廃止された語彙（旧 `SPEAK`/`LEARN`/`IDLE`）。** #588 Stage 3 で通常ターンへ
    /// 寄せ、その tick が何を出力したかは会話ログ（投稿した回は [`Self::record_posted_reply`]・
    /// 沈黙は無記録）が持つようになったため、この列は「もう使っていないが過去データ
    /// （既存 13,073 行）のため残す」列になった。列が `NOT NULL` なので**マイグレーションせず**、
    /// 新規行には**固定値 `fired`**（＝「この tick が発火した」ことだけを表す）を入れる。同じ経緯を
    /// 列定義側（`opencrab_db` の `schema/sql.rs` の `heartbeat_log`）にも明記してある。
    ///
    /// `result_json` は発火の所在（`channel_id`）と指示文の由来（`source`）を残す。`source` は
    /// #586（agent スコープ廃止）が入るまで「適用された指示が channel か agent か」の診断値として
    /// 意味があるため落とさない。継続ターンだけ発火元（`origin`）を添える。
    fn record_heartbeat_log(&self, target: &HeartbeatTarget, origin: &TurnOrigin) {
        let Ok(conn) = self.db.lock() else {
            return;
        };
        let mut result = serde_json::json!({
            "channel_id": target.channel_id,
            "source": target.instructions_source,
        });
        // 継続ターンだけ発火元を添える（tick 行の形は不変）。どの発火が subtask の
        // 決着由来かを後から切り分けられるようにする。
        if let TurnOrigin::SubtaskResume {
            subtask_id,
            exit_reason,
        } = origin
        {
            if let Some(obj) = result.as_object_mut() {
                obj.insert("origin".to_string(), serde_json::json!("subtask_resume"));
                obj.insert("subtask_id".to_string(), serde_json::json!(subtask_id));
                obj.insert("exit_reason".to_string(), serde_json::json!(exit_reason));
            }
        }
        // `decision` は廃止語彙の固定値（上記 doc）。
        if let Err(e) = opencrab_db::queries::insert_heartbeat_log(
            &conn,
            &target.agent_id,
            "fired",
            Some(&result.to_string()),
        ) {
            tracing::error!(agent_id = %target.agent_id, "Failed to insert heartbeat log: {}", e);
        }
    }

    /// 投稿した回の**通常の配送記録**（#588 Stage 3）。
    ///
    /// 発火元 transport から `source` を、`origin` から `context` を決め、通常ターンの応答と
    /// 同じ [`opencrab_server::transcript::record_outbound_reply`] の行を実会話セッションへ残す。
    /// HB 固有の毎回記録（旧 `record_heartbeat_response`）はこれに置き換えて撤去した。沈黙の回は
    /// 呼ばれない（無記録）。
    ///
    /// **現状は Discord 発火からのみ呼ばれる**（`turn`）。ブロードキャスト（Nostr）発火は応答本文を
    /// 自動配送しないため配送記録も残さない（ツール投稿は各ツールが記録する）。`NostrBroadcast` の
    /// arm は将来ブロードキャストを記録したくなったときのために残す（`SessionFireTarget` の
    /// 網羅性を保つ意味もある）。
    fn record_posted_reply(
        &self,
        target: &HeartbeatTarget,
        origin: &TurnOrigin,
        result: &opencrab_core::EngineResult,
        text: &str,
    ) {
        let Ok(conn) = self.db.lock() else {
            return;
        };
        let (source, channel_id, context) = match &target.fire_target {
            SessionFireTarget::DiscordChannel { .. } => {
                // 通常ターンと同じ `triggered_by`: tick は直接応答、継続ターンは subtask 完了。
                let ctx = match origin {
                    TurnOrigin::SubtaskResume { .. } => AgentReplyContext::SubtaskCompleted,
                    TurnOrigin::Tick { .. } => AgentReplyContext::Direct {
                        tool_calls_made: result.tool_calls_made,
                    },
                };
                (
                    TranscriptSource::Discord,
                    Some(target.channel_id.as_str()),
                    Some(ctx),
                )
            }
            // Nostr は通常ターンでも `context`（triggered_by）を記録しない（`OutboundReplyRecord` doc）。
            SessionFireTarget::NostrBroadcast => (TranscriptSource::Nostr, None, None),
        };
        opencrab_server::transcript::record_outbound_reply(
            &conn,
            source,
            &OutboundReplyRecord {
                agent_id: &target.agent_id,
                session_id: &target.session_id,
                channel_id,
                text,
                context,
            },
        );
    }

    /// 応答本文を Discord チャンネルへ配送する（fire-and-forget・#588 Stage 3 / #591）。
    ///
    /// **現状は Discord 発火からのみ呼ばれる**（`turn`。ブロードキャスト発火は応答本文を自動配送
    /// しない）。配送先は発火元の種別で決める（`DeliveryRoute`）。発火 tick を塞がないよう spawn
    /// する（#178 系）。送信の実体は [`heartbeat_delivery`]（#400 のハンドル解決・分割送信）を
    /// **再利用**する——これはハートビート専用ではなく「メッセージループの外からチャンネルへ
    /// 投稿する」ための送信。
    fn deliver_speech(&self, target: &HeartbeatTarget, text: &str) {
        let content = text.to_string();
        let gateways = self.gateways.clone();
        let discord_http = self.discord_http.clone();
        let agent_id = target.agent_id.clone();
        let channel_target = target.channel_id.clone();
        let route = heartbeat_delivery::DeliveryRoute::from_fire_target(&target.fire_target);
        tokio::spawn(async move {
            heartbeat_delivery::deliver_heartbeat_speech(
                &gateways,
                &discord_http,
                route,
                &agent_id,
                &channel_target,
                &content,
            )
            .await;
        });
    }
}

/// HB ターンが dispatch したサブタスクの決着を受け、継続ターンを起動する sink（#440）。
struct HeartbeatCompletionSink {
    runner: Arc<HeartbeatTurnRunner>,
    /// この sink を配線したターンの宛先。継続ターンは同じ宛先で走る。
    target: HeartbeatTarget,
}

impl SubtaskCompletionSink for HeartbeatCompletionSink {
    fn on_subtask_settled(&self, ev: SubtaskSettled) {
        let Some(origin) = resume_origin(&self.target.session_id, &ev) else {
            return;
        };
        tracing::info!(
            agent_id = %self.target.agent_id,
            session_id = %self.target.session_id,
            subtask_id = %ev.subtask_id,
            exit_reason = %ev.exit_reason,
            "heartbeat: subtask settled; starting continuation turn"
        );
        self.runner.spawn_turn(self.target.clone(), origin);
    }
}

/// 決着イベントから継続ターンの発火元を決める（純粋関数）。
///
/// `None` を返す（＝継続しない）のは 2 つ:
/// - 決着以外（進捗通知）。走行中の run の途中で二重に応答してしまう（Nostr / web と同じ判断。
///   Discord だけが `Progress` も通すのは「進捗実況」機能があるため）。
/// - 親セッションがこの sink を配線したターンのものでない。sink は
///   [`RunRequest::with_dispatch`] で**その run が dispatch した subtask にのみ**配線され、
///   決着は所有 sink の `on_subtask_settled` に届く（`subtask.rs`）。よって「この HB run の
///   subtask か」の主判定は **`ev.session_id == own_session_id`** で足りる。
///
/// **`heartbeat-` 接頭辞は見ない（#573 Stage A）。** 以前は
/// `ev.session_id.starts_with("heartbeat-")` も課していたが、これは主判定
/// （`own_session_id` 一致）に対する冗長な番人にすぎず、統合後に HB ターンが実会話
/// セッション（`nostr-…` / `discord-…`）で走ると接頭辞が消える。sink は HB run にしか
/// 配線されないため、接頭辞を外しても他ゲートウェイの決着を HB として resume することは
/// ない（同一セッションで走る Discord ターンの subtask は `DiscordCompletionSink` が所有し、
/// この sink には届かない）。
fn resume_origin(own_session_id: &str, ev: &SubtaskSettled) -> Option<TurnOrigin> {
    if ev.kind != SettleKind::Completed {
        tracing::debug!(
            session_id = %ev.session_id,
            kind = ?ev.kind,
            "heartbeat sink: not a completion, skipping continuation turn"
        );
        return None;
    }
    if ev.session_id != own_session_id {
        tracing::debug!(
            session_id = %ev.session_id,
            own_session_id = %own_session_id,
            "heartbeat sink: parent session is not this sink's own session, skipping continuation turn"
        );
        return None;
    }
    Some(TurnOrigin::SubtaskResume {
        subtask_id: ev.subtask_id.clone(),
        exit_reason: ev.exit_reason.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use opencrab_actions::AgentGatewayLifecycle;
    use opencrab_core::text_delivery::TextDelivery;
    use opencrab_gateway::{
        GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext,
    };
    use std::sync::Mutex;
    use std::time::Duration;

    const AGENT: &str = "agent-a";
    const SESSION: &str = "heartbeat-agent-a-222";
    /// scheduler が整形して target に載せる指示文（本番と同形・#588 Stage 3）。#501 でこれが
    /// system プロンプトへ入る。判別しやすいよう固有の文言（「20分ごとに巡回してね」）を混ぜてある。
    const INSTRUCTIONS: &str = "[ハートビート] 現在の会話「テスト部屋」。20分ごとに巡回してね。\nいまはハートビートの時間です。取り組むことがあれば自分の言葉で短く添えたうえで spawn_subtask で起動し、無ければ NO_REPLY とだけ答えてください。";

    fn target() -> HeartbeatTarget {
        HeartbeatTarget {
            agent_id: AGENT.to_string(),
            session_id: SESSION.to_string(),
            channel_id: "222".to_string(),
            guild_id: "111".to_string(),
            channel_session_id: "discord-agent-a-111-222".to_string(),
            instructions_prompt: INSTRUCTIONS.to_string(),
            instructions_source: "default",
            fire_target: opencrab_db::queries::SessionFireTarget::DiscordChannel {
                guild_id: "111".to_string(),
                channel_id: "222".to_string(),
            },
        }
    }

    fn settled(session_id: &str, kind: SettleKind) -> SubtaskSettled {
        SubtaskSettled {
            session_id: session_id.to_string(),
            agent_id: AGENT.to_string(),
            subtask_id: "st-1".to_string(),
            exit_reason: "completed".to_string(),
            kind,
            reply_target: None,
            caller: CallerIdentity::Agent,
        }
    }

    /// 応答を返しつつ、受け取った `RunRequest` を記録する実行口。
    ///
    /// `gate` を持たせると、記録後に許可が出るまでターンが返らない（直列化の観測に使う）。
    struct RecordingEngine {
        response: String,
        requests: Mutex<Vec<RunRequest>>,
        gate: Option<Arc<tokio::sync::Semaphore>>,
    }

    impl RecordingEngine {
        fn new(response: &str) -> Arc<Self> {
            Arc::new(Self {
                response: response.to_string(),
                requests: Mutex::new(Vec::new()),
                gate: None,
            })
        }

        /// 許可を出すまで走行し続ける実行口（`gate` と対で返す）。
        fn gated(response: &str) -> (Arc<Self>, Arc<tokio::sync::Semaphore>) {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            (
                Arc::new(Self {
                    response: response.to_string(),
                    requests: Mutex::new(Vec::new()),
                    gate: Some(gate.clone()),
                }),
                gate,
            )
        }
        fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
        fn system_prompts(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.system_prompt.clone())
                .collect()
        }
        fn conversations(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.conversation.clone())
                .collect()
        }
        fn last_request_sink(&self) -> Arc<dyn SubtaskCompletionSink> {
            self.requests
                .lock()
                .unwrap()
                .last()
                .and_then(|r| r.completion_sink.clone())
                .expect("HB の run は完了 sink を配線する")
        }
    }

    #[async_trait]
    impl HeartbeatEngine for RecordingEngine {
        async fn run(&self, req: RunRequest) -> anyhow::Result<opencrab_core::EngineResult> {
            let response = self.response.clone();
            self.requests.lock().unwrap().push(req);
            if let Some(gate) = &self.gate {
                gate.acquire().await.expect("gate closed").forget();
            }
            Ok(opencrab_core::EngineResult {
                response,
                iterations: 1,
                tool_calls_made: 0,
                stopped_by_limit: false,
                xml_fallback_parses: 0,
            })
        }
    }

    /// 送信を記録するだけの配送口（ネットワークに出ない）。`heartbeat_delivery` のテストと同型。
    struct SpyDelivery {
        calls: CallLog,
    }

    #[async_trait]
    impl TextDelivery for SpyDelivery {
        fn validate_target(&self, _target: &str) -> Result<(), String> {
            Ok(())
        }
        fn mention(&self, user_id: &str) -> String {
            format!("@{user_id}")
        }
        fn chunk_limit(&self) -> usize {
            2000
        }
        async fn send_text(&self, target: &str, text: &str) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push((target.to_string(), text.to_string()));
            Ok(())
        }
    }

    struct FakeActions {
        delivery: Arc<dyn TextDelivery>,
    }

    #[async_trait]
    impl GatewayActions for FakeActions {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![]
        }
        async fn execute(
            &self,
            _name: &str,
            _args: &serde_json::Value,
            _ctx: &GatewayCallContext,
        ) -> GatewayActionResult {
            GatewayActionResult {
                success: false,
                data: None,
                error: Some("unused in tests".to_string()),
            }
        }
        fn text_delivery(&self) -> Option<Arc<dyn TextDelivery>> {
            Some(self.delivery.clone())
        }
    }

    struct FakeGateway {
        delivery: Arc<dyn TextDelivery>,
    }

    #[async_trait]
    impl AgentGatewayLifecycle for FakeGateway {
        fn kind(&self) -> &'static str {
            opencrab_actions::gateway_kinds::NOSTR
        }
        async fn start(&self, _agent_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&self, _agent_id: &str) {}
        fn is_running(&self, agent_id: &str) -> bool {
            agent_id == AGENT
        }
        async fn restore_all(&self) {}
        async fn shutdown_all(&self) {}
        fn gateway_actions_for(&self, _agent_id: &str) -> Option<Arc<dyn GatewayActions>> {
            Some(Arc::new(FakeActions {
                delivery: self.delivery.clone(),
            }))
        }
    }

    /// 記録された送信（target, text）の共有ログ（`heartbeat_delivery` のテストと同型）。
    type CallLog = Arc<Mutex<Vec<(String, String)>>>;

    /// テスト用のランナー。DB は in-memory、推論は `RecordingEngine`、配送は spy。
    fn runner_with(
        engine: Arc<RecordingEngine>,
    ) -> (Arc<HeartbeatTurnRunner>, opencrab_db::Db, CallLog) {
        let db = opencrab_db::Db::memory().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let delivery: Arc<dyn TextDelivery> = Arc::new(SpyDelivery {
            calls: calls.clone(),
        });
        let gateways = Arc::new(opencrab_actions::AgentGatewayRegistry::new());
        gateways.register(Arc::new(FakeGateway { delivery }));
        let runner = Arc::new(HeartbeatTurnRunner {
            db: db.clone(),
            engine,
            gateways,
            discord_http: Arc::new(HeartbeatDiscordHttp::new(Arc::new(std::sync::Mutex::new(
                None,
            )))),
            registries: Arc::new(SubtaskRegistries::new()),
            default_model: "mock:test".to_string(),
            compaction_ratio: 0.5,
            locks: SessionLocks::new(),
        });
        (runner, db, calls)
    }

    /// `settle_completed` が親セッションへ書く完了本文と同じ形の行を入れる。
    fn insert_subtask_completed_log(db: &opencrab_db::Db, result_text: &str) {
        let conn = db.lock().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: AGENT.to_string(),
            session_id: SESSION.to_string(),
            log_type: "system".to_string(),
            content: serde_json::json!({
                "type": "subtask_completed",
                "subtask_id": "st-1",
                "session_id": "subtask-st-1",
                "exit_reason": "completed",
                "result": result_text,
            })
            .to_string(),
            speaker_id: None,
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&conn, &log).unwrap();
    }

    /// HB **専用セッション**へ残ったエージェント自身の発話記録（#515）。
    /// `speaker_id = agent_id` の `speech` 行だけを拾う（指示文は system プロンプト側なので
    /// ここには無い / #501）。
    fn heartbeat_speech_records(db: &opencrab_db::Db) -> Vec<String> {
        let conn = db.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT content FROM memory_sessions \
                 WHERE session_id = ?1 AND log_type = 'speech' AND speaker_id = ?2 \
                 ORDER BY id",
            )
            .unwrap();
        let rows = stmt
            .query_map([SESSION, AGENT], |r| r.get::<_, String>(0))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    fn heartbeat_log_decisions(db: &opencrab_db::Db) -> Vec<(String, String)> {
        let conn = db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT decision, COALESCE(result_json, '') FROM heartbeat_log ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// `condition` が真になるまで最大 2 秒待つ（配送・継続ターンは spawn される）。
    async fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
        for _ in 0..100 {
            if condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        condition()
    }

    // ── 継続ターンの起動判定（純粋関数） ──────────────────────────────

    /// 決着（Completed）かつ自分の HB セッションなら継続する。
    #[test]
    fn resume_origin_starts_continuation_for_own_completed_subtask() {
        assert_eq!(
            resume_origin(SESSION, &settled(SESSION, SettleKind::Completed)),
            Some(TurnOrigin::SubtaskResume {
                subtask_id: "st-1".to_string(),
                exit_reason: "completed".to_string(),
            })
        );
    }

    /// 進捗通知では継続しない（走行中の run の途中で二重に応答しない）。
    #[test]
    fn resume_origin_ignores_progress() {
        assert_eq!(
            resume_origin(SESSION, &settled(SESSION, SettleKind::Progress)),
            None
        );
    }

    /// HB 以外の親セッション（Discord など）は継続しない = 既存経路を横取りしない。
    #[test]
    fn resume_origin_ignores_non_heartbeat_parent() {
        assert_eq!(
            resume_origin(
                SESSION,
                &settled("discord-agent-a-111-222", SettleKind::Completed)
            ),
            None
        );
        // 別の HB セッション（別チャンネル）も自分のものではない。
        assert_eq!(
            resume_origin(
                SESSION,
                &settled("heartbeat-agent-a-333", SettleKind::Completed)
            ),
            None
        );
    }

    /// #573 Stage A: 判定は `own_session_id` 一致のみで、`heartbeat-` 接頭辞に依存しない。
    /// 統合後（Stage B）に HB ターンが実会話セッション（`nostr-…` / `discord-…`）で走っても、
    /// 自分のセッションの完了 subtask なら継続ターンが起きる。
    #[test]
    fn resume_origin_is_prefix_independent_for_own_session() {
        for own in ["nostr-agent-a", "discord-agent-a-111-222"] {
            assert_eq!(
                resume_origin(own, &settled(own, SettleKind::Completed)),
                Some(TurnOrigin::SubtaskResume {
                    subtask_id: "st-1".to_string(),
                    exit_reason: "completed".to_string(),
                }),
                "own_session_id={own} の自分の完了 subtask は接頭辞に関わらず継続する"
            );
            // 接頭辞非依存でも「別セッションの決着は拾わない」は保たれる（横取り防止）。
            assert_eq!(
                resume_origin(own, &settled("discord-other-9-9", SettleKind::Completed)),
                None,
                "own_session_id={own} でも別セッションの決着は継続しない"
            );
        }
    }

    /// tick の suffix は無し。継続ターンだけがマーカーを足す（本文は載せない）。
    /// #501: 出力形式の規約行は指示文（`instructions_prompt` → system プロンプト）が持ち、
    /// suffix には重複して置かない。
    #[test]
    fn prompt_suffix_only_for_continuation() {
        assert_eq!(TurnOrigin::Tick { tick: 3 }.prompt_suffix(), None);
        let suffix = TurnOrigin::SubtaskResume {
            subtask_id: "st-1".to_string(),
            exit_reason: "completed".to_string(),
        }
        .prompt_suffix()
        .unwrap();
        assert!(suffix.contains("[subtask_completed: subtask_id=st-1, exit_reason=completed]"));
        assert!(
            !suffix.contains("出力形式:"),
            "出力形式の規約行は指示文側に一本化し suffix には置かない（#501）: {suffix}"
        );
    }

    /// 決着理由ごとの継続ターン（`prompt_suffix` / `label` の分岐に使う）。
    fn resume(exit_reason: &str) -> TurnOrigin {
        TurnOrigin::SubtaskResume {
            subtask_id: "st-1".to_string(),
            exit_reason: exit_reason.to_string(),
        }
    }

    /// **#443**: 完了以外の決着で「完了しました」と断言しない。
    ///
    /// `SettleKind::Completed` は timeout / error / stopped_by_limit でも発火するので、
    /// 一律「完了」と告げると同じ prompt のマーカー（`exit_reason=timeout`）と矛盾する。
    #[test]
    fn prompt_suffix_never_claims_completion_for_unfinished_subtasks() {
        for (exit_reason, expected) in [
            ("timeout", "時間切れで打ち切られました"),
            ("error", "エラーで失敗しました"),
            ("stopped_by_limit", "反復上限に達して途中で打ち切られました"),
            // 未知の値は断定しない（`subtask_spawn.rs` が語彙を増やしても誤情報にならない）。
            ("weird_new_reason", "終了しました"),
        ] {
            let suffix = resume(exit_reason).prompt_suffix().unwrap();
            assert!(
                !suffix.contains("完了しました"),
                "exit_reason={exit_reason} で完了を断言している: {suffix}"
            );
            assert!(
                suffix.contains(expected),
                "exit_reason={exit_reason} の決着が伝わらない: {suffix}"
            );
            assert!(
                suffix.contains(&format!("exit_reason={exit_reason}]")),
                "マーカーは生の exit_reason をそのまま持つ: {suffix}"
            );
        }
    }

    /// 完了した subtask だけが「完了しました」を受け取る（従来の文言）。
    #[test]
    fn prompt_suffix_states_completion_only_when_completed() {
        let suffix = resume("completed").prompt_suffix().unwrap();
        assert!(
            suffix.contains("バックグラウンド処理が完了しました"),
            "完了は完了と伝える: {suffix}"
        );
    }

    /// 内省メモのラベルも決着理由で分かれる（curated_memory へ残り後のターンが読む）。
    #[test]
    fn origin_label_reflects_exit_reason() {
        assert_eq!(resume("timeout").label(), "subtask st-1 タイムアウト");
        assert_eq!(resume("error").label(), "subtask st-1 失敗");
        assert_eq!(
            resume("stopped_by_limit").label(),
            "subtask st-1 途中打ち切り"
        );
        assert_eq!(resume("weird_new_reason").label(), "subtask st-1 決着");
    }

    /// 内省メモの発火元ラベル。tick は移設前と同じ文言。
    #[test]
    fn origin_label_keeps_tick_wording() {
        assert_eq!(TurnOrigin::Tick { tick: 7 }.label(), "tick 7");
        assert_eq!(
            TurnOrigin::SubtaskResume {
                subtask_id: "st-1".to_string(),
                exit_reason: "completed".to_string(),
            }
            .label(),
            "subtask st-1 完了"
        );
    }

    // ── 配線（HB の run が張る sink → 継続ターン） ─────────────────────

    /// **本 issue の主目的**: HB ターンが dispatch したサブタスクが決着すると、その HB 文脈で
    /// 継続ターンが 1 回走り、サブタスクの結果が会話文脈に載る。
    ///
    /// sink は「HB の run が実際に張ったもの」（`RunRequest.completion_sink`）を取り出して
    /// 発火させる。`NoopCompletionSink` に戻せば 2 ターン目が起きず、このテストが落ちる。
    #[tokio::test]
    async fn subtask_completion_starts_a_continuation_turn_with_the_result() {
        let engine = RecordingEngine::new("NO_REPLY");
        let (runner, db, _calls) = runner_with(engine.clone());

        // 1 ターン目（tick）。ここで張られた sink が継続ターンの入口になる。
        runner
            .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
            .await;
        assert_eq!(engine.count(), 1);
        let sink = engine.last_request_sink();

        // サブタスクが決着する（本文は DB へ、sink は本文を運ばない = RFC §1.3）。
        insert_subtask_completed_log(&db, "対象 note と返信案までできた");
        sink.on_subtask_settled(settled(SESSION, SettleKind::Completed));

        assert!(
            wait_until(|| engine.count() == 2).await,
            "サブタスク決着で HB の継続ターンが走る（修正前は走らない）"
        );
        let prompts = engine.system_prompts();
        assert!(
            prompts[1].contains("[subtask_completed: subtask_id=st-1, exit_reason=completed]"),
            "継続ターンの system prompt に決着マーカーが載る: {}",
            prompts[1]
        );
        assert!(
            engine.conversations()[1].contains("対象 note と返信案までできた"),
            "サブタスクの成果が会話文脈に載る（本文は DB から再構築される）"
        );
        assert!(
            !prompts[0].contains("[subtask_completed: subtask_id="),
            "tick の system prompt は従来どおり（決着マーカーを足さない）: {}",
            prompts[0]
        );
    }

    /// #501: tick の指示文は **system プロンプト**へ入り、会話文脈には積まれない。
    #[tokio::test]
    async fn tick_puts_instructions_in_the_system_prompt_not_the_conversation() {
        let engine = RecordingEngine::new("NO_REPLY");
        let (runner, _db, _calls) = runner_with(engine.clone());

        runner
            .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
            .await;

        let sys = &engine.system_prompts()[0];
        assert!(
            sys.contains("20分ごとに巡回してね") && sys.contains("spawn_subtask"),
            "tick の指示文が system プロンプトに入っていない: {sys}"
        );
        // 会話文脈には指示文を積まない（scheduler はセッションログへ書かない / #501）。
        let conv = &engine.conversations()[0];
        assert!(
            !conv.contains("20分ごとに巡回してね"),
            "指示文が会話文脈に現れている（system へ移したはず）: {conv}"
        );
    }

    /// テスト用: 指定セッションへ他者/自分の発話を 1 行入れる。
    fn insert_speech(db: &opencrab_db::Db, session_id: &str, speaker: &str, content: &str) {
        let conn = db.lock().unwrap();
        opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: session_id.to_string(),
                log_type: "speech".to_string(),
                content: content.to_string(),
                speaker_id: Some(speaker.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    /// #588 Stage 1: HB ターンは宛先セッションを通常の `build_conversation_string` で読む。
    /// 本番では宛先＝実会話セッション（`session_id == channel_session_id`・#573 Stage B）なので、
    /// Nostr HB でも他者の直近発言がそのまま会話へ入ることを、`run_turn`→`build_context` の
    /// read 経路で担保する（#508 が直した「Nostr HB に実会話が入らない」を統合後の形で維持）。
    #[tokio::test]
    async fn nostr_heartbeat_reads_channel_conversation_from_its_session() {
        let engine = RecordingEngine::new("NO_REPLY");
        let (runner, db, _calls) = runner_with(engine.clone());

        // 本番同型: 発火セッション＝実会話セッション（nostr watch が書くリテラル）。
        let session =
            opencrab_db::queries::SessionFireTarget::NostrBroadcast.channel_session_id(AGENT);
        insert_speech(&db, &session, "npub-other", "外の人: 新機能どう？");

        let nostr_target = HeartbeatTarget {
            agent_id: AGENT.to_string(),
            session_id: session.clone(),
            channel_id: String::new(),
            guild_id: String::new(),
            channel_session_id: session,
            instructions_prompt: INSTRUCTIONS.to_string(),
            instructions_source: "default",
            fire_target: opencrab_db::queries::SessionFireTarget::NostrBroadcast,
        };

        runner
            .run_turn(&nostr_target, TurnOrigin::Tick { tick: 1 })
            .await;

        let conv = &engine.conversations()[0];
        assert!(
            conv.contains("外の人: 新機能どう？"),
            "他者の直近発言が Nostr HB の会話に含まれない: {conv}"
        );
    }

    /// #588 Stage 1: Discord HB も宛先セッション（実会話 `discord-{agent}-{guild}-{channel}`）を
    /// 通常経路で読む。実会話がそのまま入ることを同じ read 経路で担保する。
    #[tokio::test]
    async fn discord_heartbeat_reads_channel_conversation_from_its_session() {
        let engine = RecordingEngine::new("NO_REPLY");
        let (runner, db, _calls) = runner_with(engine.clone());

        insert_speech(&db, "discord-agent-a-111-222", "human-1", "会議いつ？");

        // 本番同型: 宛先セッション＝実会話セッション。
        let discord_target = HeartbeatTarget {
            session_id: "discord-agent-a-111-222".to_string(),
            channel_session_id: "discord-agent-a-111-222".to_string(),
            ..target()
        };

        runner
            .run_turn(&discord_target, TurnOrigin::Tick { tick: 1 })
            .await;

        let conv = &engine.conversations()[0];
        assert!(
            conv.contains("会議いつ？"),
            "Discord HB の実会話が入らない（統合後の read 経路が壊れた）: {conv}"
        );
    }

    /// #501 + #440: 継続ターンの system プロンプトには **指示文と決着マーカーの両方**が載り、
    /// 指示文（standing instruction）は 1 度だけ現れる（suffix が重複させない）。
    #[tokio::test]
    async fn continuation_system_prompt_has_instructions_and_marker_without_duplicate_instruction()
    {
        let engine = RecordingEngine::new("NO_REPLY");
        let (runner, db, _calls) = runner_with(engine.clone());
        insert_subtask_completed_log(&db, "調査おわり");

        runner
            .run_turn(
                &target(),
                TurnOrigin::SubtaskResume {
                    subtask_id: "st-1".to_string(),
                    exit_reason: "completed".to_string(),
                },
            )
            .await;

        let sys = &engine.system_prompts()[0];
        // 指示文（standing instruction）が載る。
        assert!(
            sys.contains("20分ごとに巡回してね"),
            "指示文が載っていない: {sys}"
        );
        // 決着マーカーも載る。
        assert!(
            sys.contains("[subtask_completed: subtask_id=st-1, exit_reason=completed]"),
            "決着マーカーが載っていない: {sys}"
        );
        // 指示文の誘導（spawn_subtask 行）は 1 度だけ（指示文側のみ。suffix には置かない）。
        assert_eq!(
            sys.matches("spawn_subtask").count(),
            1,
            "指示文の誘導が重複している（suffix が二重に載せた）: {sys}"
        );
    }

    /// #588 Stage 3: **Discord** の継続ターンも、投稿した回は tick と同じ通常の配送記録を残す
    /// （旧 `SPEAK:` 解析は撤去。応答本文がそのまま Discord チャンネルへ配送される）。
    ///
    /// Discord への実バイト送信は `heartbeat_delivery`（共有 http・#400）を再利用しており、送信の
    /// 単体観測は同モジュールのテストが担う。ここでは「継続ターンが投稿の記録経路を通る」ことと
    /// `heartbeat_log` の発火元を担保する。
    #[tokio::test]
    async fn continuation_on_discord_records_the_posted_reply() {
        let engine = RecordingEngine::new("調べ終わった。結果を共有する");
        let (runner, db, _calls) = runner_with(engine.clone());
        insert_subtask_completed_log(&db, "結果");

        // Discord 発火（`target()` は DiscordChannel）。session_id は SESSION のまま。
        let started = runner
            .run_turn(
                &target(),
                TurnOrigin::SubtaskResume {
                    subtask_id: "st-1".to_string(),
                    exit_reason: "completed".to_string(),
                },
            )
            .await;

        assert!(started.is_some(), "継続ターンが開始する");
        assert_eq!(
            heartbeat_speech_records(&db),
            vec!["調べ終わった。結果を共有する".to_string()],
            "Discord の継続ターンは投稿した本文を通常の配送記録として残す"
        );
        // heartbeat_log にも 1 行残り（decision は固定値 fired）、発火元が subtask 決着だと分かる。
        let logs = heartbeat_log_decisions(&db);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].0, "fired");
        assert!(
            logs[0].1.contains("\"origin\":\"subtask_resume\"") && logs[0].1.contains("st-1"),
            "継続ターンの heartbeat_log は発火元を残す: {}",
            logs[0].1
        );
    }

    /// #588 Stage 3: tick の `heartbeat_log` は `decision=fired` 固定で、`result_json` は
    /// `channel_id` と `source` だけ（tick 行に発火元は足さない）。
    #[tokio::test]
    async fn tick_heartbeat_log_records_fired() {
        let engine = RecordingEngine::new("NO_REPLY");
        let (runner, db, _calls) = runner_with(engine);
        runner
            .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
            .await;

        let logs = heartbeat_log_decisions(&db);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].0, "fired", "廃止語彙ではなく固定値 fired を入れる");
        let v: serde_json::Value = serde_json::from_str(&logs[0].1).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"channel_id": "222", "source": "default"}),
            "tick 行に発火元は足さない"
        );
    }

    /// #588 Stage 3: **投稿した回だけ**通常の配送記録を残す。応答本文（`NO_REPLY` 以外）が
    /// エージェント自身の言葉として実会話セッションへ `speech` で 1 行残る（旧 IDLE/LEARN の
    /// 毎回記録は撤去し、`record_outbound_reply` の通常記録に寄せた）。
    ///
    /// **変異確認**: この記録は `turn()` が `record_posted_reply` を呼ぶ箇所が担う。その呼び出しを
    /// 外すと記録が 0 件になりこのテストが赤くなる。
    #[tokio::test]
    async fn a_posted_turn_records_the_response_body_as_an_outbound_reply() {
        let engine = RecordingEngine::new("新機能の反応を見にいってくる");
        let (runner, db, _calls) = runner_with(engine);
        runner
            .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
            .await;
        assert_eq!(
            heartbeat_speech_records(&db),
            vec!["新機能の反応を見にいってくる".to_string()],
            "投稿した回は応答本文が通常の配送記録として残る"
        );
    }

    /// #588 Stage 3（オーナー判断）: **ブロードキャスト（Nostr）発火は応答本文を自動配送しない。**
    ///
    /// 応答が `NO_REPLY` **以外**（＝本来なら発話）でも、broadcast 発火では自動配送しない
    /// （エージェントが `nostr_post` 等で自分で投稿する）。本文は投稿されないので配送記録も残さない。
    /// これは二重投稿の道（旧 IDLE の安全弁を廃止したうえに本文まで自動配送する）を塞ぐ核心。
    /// `turn()` の `Broadcast` 分岐で配送・記録するよう戻すとこのテストが赤くなる。
    #[tokio::test]
    async fn broadcast_fire_does_not_auto_deliver_or_record_the_body() {
        let engine = RecordingEngine::new("新機能を告知したい");
        let (runner, db, calls) = runner_with(engine);
        let broadcast_target = HeartbeatTarget {
            channel_id: String::new(),
            fire_target: opencrab_db::queries::SessionFireTarget::NostrBroadcast,
            ..target()
        };
        let started = runner
            .run_turn(&broadcast_target, TurnOrigin::Tick { tick: 1 })
            .await;
        assert!(started.is_some(), "ターン自体は開始・完了する");

        // 外部配送は 0 件（Nostr spy に何も届かない・spawn され得るので少し待ってから見る）。
        assert!(
            !wait_until(|| !calls.lock().unwrap().is_empty()).await,
            "broadcast 発火で応答本文が自動配送された（二重投稿の道が開く）: {:?}",
            *calls.lock().unwrap()
        );
        // 本文は投稿されないので会話への配送記録も残さない。
        assert!(
            heartbeat_speech_records(&db).is_empty(),
            "broadcast 発火で配送記録が残った（本文は投稿されないはず）"
        );
        // 発火ログは 1 行残る（decision=fired）。
        let logs = heartbeat_log_decisions(&db);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].0, "fired", "発火自体は fired として残る");
    }

    /// #588 Stage 3: **Discord の沈黙（`NO_REPLY`）は無配送・無記録**（旧 IDLE の毎回記録は撤去）。
    /// 発火自体は `heartbeat_log` に `fired` として残る。
    #[tokio::test]
    async fn discord_no_reply_records_nothing() {
        let engine = RecordingEngine::new("NO_REPLY");
        let (runner, db, _calls) = runner_with(engine);
        let started = runner
            .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
            .await;
        assert!(started.is_some(), "沈黙でもターン自体は開始・完了する");

        assert!(
            heartbeat_speech_records(&db).is_empty(),
            "NO_REPLY の回に会話記録が残っている（沈黙は無記録のはず）"
        );
        let logs = heartbeat_log_decisions(&db);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].0, "fired", "沈黙でも発火自体は fired として残る");
    }

    /// 継続ターンは HB セッションの直列化ロックを通る（走行中の tick と並行しない）。
    ///
    /// 1 本目を推論の中で止めたまま 2 本目を投入し、**2 本目が推論へ入らない**ことを見る。
    /// `run_turn` から `run_serialized` を外すと 2 本目が即座に入って推論回数が 2 になり、
    /// このテストが落ちる（＝二重応答の不変条件が壊れたことを検知する）。
    #[tokio::test]
    async fn a_continuation_turn_waits_for_the_running_turn() {
        let (engine, gate) = RecordingEngine::gated("NO_REPLY");
        let (runner, _db, _calls) = runner_with(engine.clone());

        let r1 = runner.clone();
        let a =
            tokio::spawn(async move { r1.run_turn(&target(), TurnOrigin::Tick { tick: 1 }).await });
        assert!(
            wait_until(|| engine.count() == 1).await,
            "1 本目が推論に入る"
        );

        let r2 = runner.clone();
        let b = tokio::spawn(async move {
            r2.run_turn(
                &target(),
                TurnOrigin::SubtaskResume {
                    subtask_id: "st-1".to_string(),
                    exit_reason: "completed".to_string(),
                },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            engine.count(),
            1,
            "走行中のターンがあるあいだ、同一 HB セッションの 2 本目は推論へ入らない"
        );

        gate.add_permits(2);
        a.await.unwrap();
        b.await.unwrap();
        assert_eq!(engine.count(), 2, "解放後に 2 本目が走る（取りこぼさない）");
        assert!(
            !runner.locks.holds_lock_entry(SESSION),
            "待機者がいなくなったロックは回収される"
        );
    }

    /// dispatch の配線（#169 の不変条件）は継続ターンでも保たれる。
    #[tokio::test]
    async fn run_request_keeps_dispatch_wiring() {
        let engine = RecordingEngine::new("NO_REPLY");
        let (runner, _db, _calls) = runner_with(engine.clone());
        runner
            .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
            .await;

        let requests = engine.requests.lock().unwrap();
        let req = requests.last().unwrap();
        assert_eq!(req.gateway, HEARTBEAT_GATEWAY);
        assert!(
            req.subtask_registry.is_some(),
            "registry を渡さないと run 内で使い捨てが作られ cancel_subtask が not found になる"
        );
        assert!(
            req.completion_sink.is_some(),
            "sink 未配線だと全ツールが inline 実行になる（#169）"
        );
        assert!(
            matches!(req.caller, CallerIdentity::Owner),
            "HB は tick も継続ターンも Owner で走る"
        );
    }

    /// #588 Stage 3: 通常ターンと同じツール環境（gateway_actions / reply_target）を配線する。
    ///
    /// - broadcast 発火（Nostr）: `runner_with` が Nostr の `FakeGateway` を登録しているので
    ///   `gateway_actions` が載る。返信先ノートが無いので `reply_target` は付けない。
    /// - Discord 発火: チャンネル ID を `reply_target` に載せる（ツールの既定宛先）。
    ///   （テストの登録簿には Discord ゲートウェイが無いので `gateway_actions` は None になるが、
    ///   本番では per-agent Discord ゲートウェイが載る。ここでは reply_target の配線を担保する。）
    #[tokio::test]
    async fn run_request_wires_gateway_actions_and_reply_target() {
        // broadcast 発火。
        {
            let engine = RecordingEngine::new("NO_REPLY");
            let (runner, _db, _calls) = runner_with(engine.clone());
            let broadcast_target = HeartbeatTarget {
                channel_id: String::new(),
                fire_target: opencrab_db::queries::SessionFireTarget::NostrBroadcast,
                ..target()
            };
            runner
                .run_turn(&broadcast_target, TurnOrigin::Tick { tick: 1 })
                .await;
            let requests = engine.requests.lock().unwrap();
            let req = requests.last().unwrap();
            assert!(
                req.gateway_actions.is_some(),
                "broadcast 発火は稼働中 transport の gateway_actions を渡す"
            );
            assert!(
                req.reply_target.is_none(),
                "broadcast は返信先ノートを持たないので reply_target を付けない"
            );
        }
        // Discord 発火。
        {
            let engine = RecordingEngine::new("NO_REPLY");
            let (runner, _db, _calls) = runner_with(engine.clone());
            runner
                .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
                .await;
            let requests = engine.requests.lock().unwrap();
            let req = requests.last().unwrap();
            assert_eq!(
                req.reply_target.as_deref(),
                Some("222"),
                "Discord 発火はチャンネル ID を reply_target に載せる"
            );
        }
    }

    /// registry は **agent 単位**で共有される（tick / 継続ターンを跨いで同一 Arc）。
    #[tokio::test]
    async fn registry_is_shared_across_turns_per_agent() {
        let engine = RecordingEngine::new("NO_REPLY");
        let (runner, _db, _calls) = runner_with(engine.clone());

        runner
            .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
            .await;
        let other_channel = HeartbeatTarget {
            session_id: "heartbeat-agent-a-333".to_string(),
            channel_id: "333".to_string(),
            ..target()
        };
        runner
            .run_turn(&other_channel, TurnOrigin::Tick { tick: 2 })
            .await;

        let requests = engine.requests.lock().unwrap();
        let r1 = requests[0].subtask_registry.clone().unwrap();
        let r2 = requests[1].subtask_registry.clone().unwrap();
        assert!(
            Arc::ptr_eq(&r1, &r2),
            "同一エージェントのターンは同じ registry を共有する"
        );
    }
}
