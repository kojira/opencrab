//! ハートビート 1 ターンの実行と、サブタスク決着からの**継続ターン**（#440）。
//!
//! # なぜこの器があるか
//!
//! ハートビート（以下 HB）のターンは 2 経路から起きる。
//!
//! 1. **tick**: 時間で発火する経路（中央スケジューラ `scheduler.rs` の `run_one_fire`）。
//! 2. **継続ターン**: そのターンが dispatch した非同期サブタスクが決着したときの再開。
//!
//! 両者は「文脈を組む → `run_agent_response` → 応答を記録 → `SPEAK/LEARN/IDLE` を解く →
//! `heartbeat_log` → 発話を配送」までまったく同じことをする。以前 heartbeat が
//! `NoopCompletionSink` で継続ターンを持たなかった理由の 1 つが「sink で resume させると
//! `SPEAK:` パースと heartbeat ログ記録を sink 側へ複製する必要がある」だった（#440）。
//! ここに 1 実装だけ置き、両経路がそれを通ることでその複製を構造的に無くす。
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
//! ここも同じ形にしてある。**ハートビート専用セッション側のログは種別で絞らない**
//! （実会話セクションだけが `speech` に絞られる / #404）ので、`settle_completed` が書いた
//! 完了本文は `build_heartbeat_conversation_string` がそのまま拾う。この依存は継続ターン
//! （いま渡す）でも次 tick（拾い直す）でも同じで、#440 の前後で変わらない。

use std::sync::Arc;

use opencrab_actions::{
    CallerIdentity, RunRequest, SessionLocks, SettleKind, SubtaskCompletionSink, SubtaskSettled,
};
use opencrab_core::heartbeat::HeartbeatDecision;
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
    ) -> Option<HeartbeatDecision> {
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

    /// heartbeat 経路の `RunRequest` を組む（#169 / #440）。
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
    fn run_request(
        self: &Arc<Self>,
        target: &HeartbeatTarget,
        agent_name: &str,
        system_prompt: &str,
        conversation: &str,
    ) -> RunRequest {
        RunRequest::new(
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
        )
    }

    /// ターン本体。**呼び出しは [`Self::run_turn`] 経由に限る**（直列化の担保）。
    async fn turn(
        self: &Arc<Self>,
        target: &HeartbeatTarget,
        origin: &TurnOrigin,
    ) -> Option<HeartbeatDecision> {
        let (system_prompt, agent_name, conversation) = self.build_context(target, origin)?;

        let engine_result = self
            .engine
            .run(self.run_request(target, &agent_name, &system_prompt, &conversation))
            .await;

        let decision = match engine_result {
            Ok(result) => {
                // #573 Stage B: 決定を先に解き、正規化した本文を実会話セッション
                // （target.session_id は Stage B で = channel_session_id）へ第一級で 1 行記録する。
                // - SPEAK: parse_heartbeat_decision が返すクリーン本文（マーカー・思考行前置を
                //   除去済み）を記録する（#425 エコーが記録していた形）。生の `SPEAK:` を残すと
                //   モデルが自分の履歴で probe 形式を見て通常返信にマーカーを漏らすリスクがある。
                // - IDLE / LEARN: #515 の記録形（応答テキストそのまま = `IDLE: 理由` / `LEARN: …`）
                //   を踏襲する。新しい書式は発明しない。
                // R-simple: 配信可否に関わらず無条件で記録する（#515 の理念。配信失敗した SPEAK も
                //   本人の言葉として残す。Nostr は delivered=false だが記録は残す）。
                let decision = crate::parse_heartbeat_decision(&result.response);
                let record_text: String = match &decision {
                    HeartbeatDecision::Speak(content) => content.clone(),
                    _ => result.response.trim().to_string(),
                };
                {
                    let conn = self.db.lock().unwrap();
                    let log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: target.agent_id.clone(),
                        session_id: target.session_id.clone(),
                        log_type: "speech".to_string(),
                        content: record_text,
                        speaker_id: Some(target.agent_id.clone()),
                        turn_number: None,
                        metadata_json: None,
                        created_at: None,
                    };
                    if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
                        tracing::error!(agent_id = %target.agent_id, "Failed to insert heartbeat response log: {e}");
                    }
                }
                decision
            }
            Err(e) => {
                tracing::warn!(
                    "Heartbeat agent response failed for channel {}: {e}",
                    target.channel_id
                );
                HeartbeatDecision::Idle
            }
        };

        // 移設前は tick ループ側にあった 1 行（`Heartbeat tick result`）。継続ターンも
        // 同じ 1 行で観測できるよう、発火元をラベルで添えてここへ移した。
        tracing::debug!(
            agent_id = %target.agent_id,
            session_id = %target.session_id,
            channel_id = %target.channel_id,
            origin = %origin.label(),
            decision = %decision,
            "Heartbeat turn result"
        );

        self.record_heartbeat_log(target, origin, &decision);
        self.apply_decision(target, origin, &decision);
        Some(decision)
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
        // #404: ハートビート専用セッションの履歴だけでは、**同じチャンネルで実際に
        // 交わされた会話が 1 行も見えない**（見えるのは自分の過去のハートビートだけ）。
        // 実会話セッションを解決して文脈へ入れる。専用セッションへの分離自体
        // （`SPEAK:` パースとハートビートログを実会話と混ぜない）は維持し、実会話は
        // **読むだけ**。
        // #508: 実会話セッションは発火先種別から解決済み（`run_one_fire` が
        // `SessionFireTarget::channel_session_id` で解く）。以前はここで guild_id/channel_id
        // から Discord 書式を組み直していたため、Nostr（両 ID が空）では必ず解決に失敗し外の
        // 会話が 1 行も入らなかった。target が持つ解決済み ID をそのまま読む。
        let conversation = match opencrab_server::process::build_heartbeat_conversation_string(
            &conn,
            &target.session_id,
            Some(target.channel_session_id.as_str()),
            &target.agent_id,
            budget,
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(agent_id = %target.agent_id, session_id = %target.session_id, "build_heartbeat_conversation_string failed: {e}");
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

    /// `heartbeat_log` へ 1 行残す。tick の `result_json` は移設前と同じキー集合。
    fn record_heartbeat_log(
        &self,
        target: &HeartbeatTarget,
        origin: &TurnOrigin,
        decision: &HeartbeatDecision,
    ) {
        let Ok(conn) = self.db.lock() else {
            return;
        };
        let decision_str = match decision {
            HeartbeatDecision::Idle => "idle",
            HeartbeatDecision::Speak(_) => "speak",
            HeartbeatDecision::Learn => "learn",
            HeartbeatDecision::ManageSkills { .. } => "manage_skills",
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
        if let Err(e) = opencrab_db::queries::insert_heartbeat_log(
            &conn,
            &target.agent_id,
            decision_str,
            Some(&result.to_string()),
        ) {
            tracing::error!(agent_id = %target.agent_id, "Failed to insert heartbeat log: {}", e);
        }
    }

    /// Speak / Learn の後続処理（配送・内省メモ）。どちらも fire-and-forget。
    fn apply_decision(
        &self,
        target: &HeartbeatTarget,
        origin: &TurnOrigin,
        decision: &HeartbeatDecision,
    ) {
        match decision {
            HeartbeatDecision::Speak(content) => {
                // 発話出口（段階3 PR-A / #246）。まず登録簿（`state.gateways`）の
                // 非 Discord transport を試し、配れなければ既存の Discord 共有 http
                // 経路へ落ちる。Discord の挙動はバイト単位で不変（詳細は
                // `heartbeat_delivery` モジュール doc）。fire-and-forget で発火 tick を
                // 塞がない（#178 系）。
                let content = content.clone();
                let gateways = self.gateways.clone();
                let discord_http = self.discord_http.clone();
                let agent_id_log = target.agent_id.clone();
                let ch_id_str = target.channel_id.clone();
                tokio::spawn(async move {
                    // 配送のみ（fire-and-forget）。実会話セッションへの記録は #573 Stage B で
                    // turn() の経路1（正規化記録）に一元化したため、#425 エコー
                    // （record_heartbeat_channel_echo）の呼び出しは撤去した。delivered 戻り値は
                    // 使わない（R-simple: 配信可否に関わらず記録済み）。
                    heartbeat_delivery::deliver_heartbeat_speech(
                        &gateways,
                        &discord_http,
                        &agent_id_log,
                        &ch_id_str,
                        &content,
                    )
                    .await;
                });
            }
            HeartbeatDecision::Learn => {
                let db = self.db.clone();
                let agent_id_log = target.agent_id.clone();
                let ch_id_str = target.channel_id.clone();
                let origin_label = origin.label();
                tokio::spawn(async move {
                    if let Ok(conn) = db.lock() {
                        let memory = opencrab_db::queries::CuratedMemoryRow {
                            id: uuid::Uuid::new_v4().to_string(),
                            agent_id: agent_id_log.clone(),
                            category: "reflection".to_string(),
                            content: format!(
                                "ハートビート内省 ({}, channel {}): 静かに自己を振り返る。",
                                origin_label, ch_id_str
                            ),
                            created_at: String::new(),
                        };
                        if let Err(e) = opencrab_db::queries::upsert_curated_memory(&conn, &memory)
                        {
                            tracing::error!(agent_id = %agent_id_log, "Heartbeat reflect_and_learn failed: {e}");
                        } else {
                            tracing::info!(agent_id = %agent_id_log, channel_id = %ch_id_str, "Heartbeat reflect_and_learn: saved at {}", origin_label);
                        }
                    }
                });
            }
            HeartbeatDecision::Idle => {}
            HeartbeatDecision::ManageSkills { .. } => {}
        }
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
    /// scheduler が整形して target に載せる指示文（本番と同形）。#501 でこれが system
    /// プロンプトへ入る。判別しやすいよう固有の文言を混ぜてある。
    const INSTRUCTIONS: &str = "[ハートビート] 現在の会話「テスト部屋」。20分ごとに巡回してね。\n出力形式: SPEAK/LEARN/IDLE のいずれか。SPEAKの場合のみ 'SPEAK: <メッセージ>' の形式で一言。";

    fn target() -> HeartbeatTarget {
        HeartbeatTarget {
            agent_id: AGENT.to_string(),
            session_id: SESSION.to_string(),
            channel_id: "222".to_string(),
            guild_id: "111".to_string(),
            channel_session_id: "discord-agent-a-111-222".to_string(),
            instructions_prompt: INSTRUCTIONS.to_string(),
            instructions_source: "default",
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
        let engine = RecordingEngine::new("IDLE");
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
        let engine = RecordingEngine::new("IDLE");
        let (runner, _db, _calls) = runner_with(engine.clone());

        runner
            .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
            .await;

        let sys = &engine.system_prompts()[0];
        assert!(
            sys.contains("20分ごとに巡回してね") && sys.contains("出力形式: SPEAK/LEARN/IDLE"),
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

    /// #508: Nostr のハートビートに `[Channel conversation]` が入り、他者の直近発言が含まれる
    /// こと。**発火先種別（`NostrBroadcast`）から解決した** channel session を target に載せ、
    /// `build_context` がそれを読む経路を丸ごと踏む。
    ///
    /// **恒真回避**: 実会話は本番の nostr watch が書く**リテラル** `nostr-{agent}` へ seed し、
    /// target の `channel_session_id` は解決経路（`SessionFireTarget::channel_session_id`）から
    /// 取る。両者は解決が正しいときだけ一致する。解決を旧 Discord 専用（Nostr で空/None 相当）へ
    /// 戻すと target が別セッションを指し、実会話が 1 行も入らず assertion が赤くなる。
    #[tokio::test]
    async fn nostr_heartbeat_gets_channel_conversation_from_fire_target() {
        let engine = RecordingEngine::new("IDLE");
        let (runner, db, _calls) = runner_with(engine.clone());

        // 本番の nostr watch が書く実会話セッション（リテラル）へ他者発言を入れる。
        insert_speech(
            &db,
            &format!("nostr-{AGENT}"),
            "npub-other",
            "外の人: 新機能どう？",
        );

        // 発火先種別からの解決経路（run_one_fire と同じ呼び出し）で target を組む。
        let nostr_target = HeartbeatTarget {
            agent_id: AGENT.to_string(),
            session_id: "heartbeat-agent-a-".to_string(),
            channel_id: String::new(),
            guild_id: String::new(),
            channel_session_id: opencrab_db::queries::SessionFireTarget::NostrBroadcast
                .channel_session_id(AGENT),
            instructions_prompt: INSTRUCTIONS.to_string(),
            instructions_source: "default",
        };

        runner
            .run_turn(&nostr_target, TurnOrigin::Tick { tick: 1 })
            .await;

        let conv = &engine.conversations()[0];
        assert!(
            conv.contains("[Channel conversation]"),
            "Nostr HB に実会話セクションが入っていない（#508 の直したい欠落）: {conv}"
        );
        assert!(
            conv.contains("外の人: 新機能どう？"),
            "他者の直近発言が Nostr HB の会話に含まれない: {conv}"
        );
    }

    /// #508: Discord の解決は不変 — `discord-{agent}-{guild}-{channel}` の実会話がそのまま入る。
    /// Nostr 対応で Discord 経路の挙動が変わっていないことを同じ read 経路で担保する。
    #[tokio::test]
    async fn discord_heartbeat_channel_conversation_unchanged() {
        let engine = RecordingEngine::new("IDLE");
        let (runner, db, _calls) = runner_with(engine.clone());

        // target() は channel 222 / guild 111 → discord-agent-a-111-222。
        insert_speech(&db, "discord-agent-a-111-222", "human-1", "会議いつ？");

        runner
            .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
            .await;

        let conv = &engine.conversations()[0];
        assert!(
            conv.contains("[Channel conversation]") && conv.contains("会議いつ？"),
            "Discord HB の実会話が入らない（既存経路が壊れた）: {conv}"
        );
    }

    /// #501 + #440: 継続ターンの system プロンプトには **指示文と決着マーカーの両方**が載り、
    /// 出力形式の規約行は 1 度だけ現れる（重複しない）。
    #[tokio::test]
    async fn continuation_system_prompt_has_instructions_and_marker_without_duplicate_format_line()
    {
        let engine = RecordingEngine::new("IDLE");
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
        // 出力形式の規約行は 1 度だけ（指示文側のみ。suffix には置かない）。
        assert_eq!(
            sys.matches("出力形式: SPEAK/LEARN/IDLE").count(),
            1,
            "出力形式の規約行が重複している: {sys}"
        );
    }

    /// 継続ターンの `SPEAK:` は tick とまったく同じ配送に乗る。
    #[tokio::test]
    async fn continuation_speak_goes_through_the_existing_delivery() {
        let engine = RecordingEngine::new("SPEAK: 新着に返信した");
        let (runner, db, calls) = runner_with(engine.clone());
        insert_subtask_completed_log(&db, "結果");

        let decision = runner
            .run_turn(
                &target(),
                TurnOrigin::SubtaskResume {
                    subtask_id: "st-1".to_string(),
                    exit_reason: "completed".to_string(),
                },
            )
            .await;

        assert!(matches!(decision, Some(HeartbeatDecision::Speak(ref c)) if c == "新着に返信した"));
        assert!(
            wait_until(|| !calls.lock().unwrap().is_empty()).await,
            "継続ターンの発話が既存の配送出口（deliver_heartbeat_speech）に乗る"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("222".to_string(), "新着に返信した".to_string())],
            "宛先も本文も tick の発話と同じ扱い"
        );
        // heartbeat_log にも 1 行残り、発火元が subtask 決着だと分かる。
        let logs = heartbeat_log_decisions(&db);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].0, "speak");
        assert!(
            logs[0].1.contains("\"origin\":\"subtask_resume\"") && logs[0].1.contains("st-1"),
            "継続ターンの heartbeat_log は発火元を残す: {}",
            logs[0].1
        );
    }

    /// tick の `heartbeat_log` の形は移設前のまま（`channel_id` と `source` だけ）。
    #[tokio::test]
    async fn tick_heartbeat_log_shape_is_unchanged() {
        let engine = RecordingEngine::new("IDLE");
        let (runner, db, _calls) = runner_with(engine);
        runner
            .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
            .await;

        let logs = heartbeat_log_decisions(&db);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].0, "idle");
        let v: serde_json::Value = serde_json::from_str(&logs[0].1).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"channel_id": "222", "source": "default"}),
            "tick 行に発火元は足さない"
        );
    }

    /// #515 / #573 Stage B: **ターン結果を記録として残す**。SPEAK / LEARN / IDLE のどれでも、
    /// エージェント自身の言葉が（Stage B では実会話）セッションへ `speech` として 1 行残る。
    /// IDLE / LEARN は #515 の形（応答テキストそのまま = `IDLE: 理由` / `LEARN: …`）。
    /// **SPEAK だけは Stage B でマーカーを剥いだクリーン本文**を記録する（生の `SPEAK:` を
    /// 履歴に残すと通常返信に probe マーカーが漏れるため）。
    ///
    /// **変異確認**: この記録は `turn()` の `speech` 挿入（正規化した本文を書く箇所）が担う。
    /// その挿入を外すと記録が 0 件になり、3 ケースとも赤くなる。
    #[tokio::test]
    async fn every_decision_leaves_the_agents_own_words_as_a_record() {
        // IDLE + 理由。理由が記録へそのまま残る。
        {
            let engine = RecordingEngine::new("IDLE: TL に新しい話題が無い");
            let (runner, db, _calls) = runner_with(engine);
            runner
                .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
                .await;
            let records = heartbeat_speech_records(&db);
            assert_eq!(records, vec!["IDLE: TL に新しい話題が無い".to_string()]);
        }
        // SPEAK。#573 Stage B: マーカーを剥いだクリーン本文が記録に残る（`SPEAK:` は付かない）。
        {
            let engine = RecordingEngine::new("SPEAK: 新着に返信した");
            let (runner, db, _calls) = runner_with(engine);
            runner
                .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
                .await;
            assert_eq!(
                heartbeat_speech_records(&db),
                vec!["新着に返信した".to_string()]
            );
        }
        // LEARN。何をしたか（内省した）が記録に残る。
        {
            let engine = RecordingEngine::new("LEARN: 巡回の気づきをメモした");
            let (runner, db, _calls) = runner_with(engine);
            runner
                .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
                .await;
            assert_eq!(
                heartbeat_speech_records(&db),
                vec!["LEARN: 巡回の気づきをメモした".to_string()]
            );
        }
    }

    /// #515: 理由が無い素の `IDLE`（LLM が規約を守らない場合）でも壊れない。決定は Idle、
    /// 記録は空にならず「IDLE」1 語がそのまま残る（記録機構は応答をそのまま書くだけ）。
    #[tokio::test]
    async fn bare_idle_without_reason_still_records() {
        let engine = RecordingEngine::new("IDLE");
        let (runner, db, _calls) = runner_with(engine);
        let decision = runner
            .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
            .await;
        assert!(matches!(decision, Some(HeartbeatDecision::Idle)));
        assert_eq!(heartbeat_speech_records(&db), vec!["IDLE".to_string()]);
    }

    /// #515（SPEAK 側の非対称の是正）: **IDLE の理由文に `SPEAK:` が紛れても外部配送しない**。
    ///
    /// 決定が Idle になること（parse）だけでなく、**配送出口（spy）が 1 度も呼ばれない**ことを
    /// ターンごと通しで見る。旧実装だと理由の右側が発話として `deliver_heartbeat_speech` に乗り、
    /// 取り消せない外部投稿になる。`parse_heartbeat_decision` の SPEAK 除外を外すと spy に
    /// 送信が入り、このテストが赤くなる（＝外部誤投稿の検知）。
    #[tokio::test]
    async fn idle_reason_mentioning_speak_is_never_delivered() {
        let engine = RecordingEngine::new("IDLE: 今は SPEAK: するほどの話題がない");
        let (runner, db, calls) = runner_with(engine);

        let decision = runner
            .run_turn(&target(), TurnOrigin::Tick { tick: 1 })
            .await;

        assert!(
            matches!(decision, Some(HeartbeatDecision::Idle)),
            "IDLE の理由に SPEAK: が入っても決定は Idle"
        );
        // 配送は spawn され得るので、少し待ってからでも 1 件も無いことを見る。
        assert!(
            !wait_until(|| !calls.lock().unwrap().is_empty()).await,
            "IDLE の理由文が外部チャンネルへ配送された（取り消せない誤投稿）: {:?}",
            *calls.lock().unwrap()
        );
        // 記録には理由がそのまま残る（記録は応答をそのまま書く）。
        assert_eq!(
            heartbeat_speech_records(&db),
            vec!["IDLE: 今は SPEAK: するほどの話題がない".to_string()]
        );
    }

    /// 継続ターンは HB セッションの直列化ロックを通る（走行中の tick と並行しない）。
    ///
    /// 1 本目を推論の中で止めたまま 2 本目を投入し、**2 本目が推論へ入らない**ことを見る。
    /// `run_turn` から `run_serialized` を外すと 2 本目が即座に入って推論回数が 2 になり、
    /// このテストが落ちる（＝二重応答の不変条件が壊れたことを検知する）。
    #[tokio::test]
    async fn a_continuation_turn_waits_for_the_running_turn() {
        let (engine, gate) = RecordingEngine::gated("IDLE");
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
        let engine = RecordingEngine::new("IDLE");
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

    /// registry は **agent 単位**で共有される（tick / 継続ターンを跨いで同一 Arc）。
    #[tokio::test]
    async fn registry_is_shared_across_turns_per_agent() {
        let engine = RecordingEngine::new("IDLE");
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
