//! Nostr の応答生成経路と subtask 完了 sink（#168 / RFC #152 S3b-1）。
//!
//! Nostr は「inbound イベントへの応答」と「background subtask 完了後の resume」の
//! 2 経路で同じことをする: 会話を DB から再構築 → `run_agent_response` → セッションへ転記。
//! その共通経路を [`NostrResponder`] に置き、[`SubtaskCompletionSink`] 実装も同じ型が
//! 担う（web gateway の `WebCompletionSink` + `run_and_deliver` と同じ構造）。
//! ただし web は応答生成と sink を別モジュールに分けており、sink から生の応答生成へ
//! 到達できない（直列化の飛ばしがコンパイルエラーになる）。ここは同一モジュールなので
//! その保証が無く、`respond_serialized` 経由という規律に頼っている。
//!
//! **配送は機構が行わない（#588）**: 応答本文の公開リレーへの送信は**エージェントが
//! `nostr_post` / `nostr_reply` 等のツールで自分から行う**。`respond` は応答をセッションへ
//! 転記するだけで、代わりに publish しない（Discord が機構配送、Nostr はツール配送、という
//! transport 差はここに現れる）。これで「暗黙返信の二重投稿を防ぐ」ための `sent_flag` も
//! 不要になった（撤去済み）。
//!
//! 不変条件（RFC §6）:
//! - **二重回答しない**: `settle_completed` が「DB 永続化 → sink 発火」の順序を保証済み。
//!   resume は `build_conversation_string` で DB から会話を再構築するため、完了本文を
//!   sink で運ぶ必要がない。
//! - **per-session 直列化**: inbound と resume の応答生成をどちらも
//!   [`NostrSessionRuntime::run_serialized`] の下で走らせる。同一セッション
//!   （#323 以降は **エージェント単位**）に対して 2 本の応答生成が並行しない。

use std::sync::Arc;

use tracing::{debug, error};

use opencrab_actions::{
    CallerIdentity, RunRequest, SettleKind, SubtaskCompletionSink, SubtaskSettled,
};
use opencrab_gateway::GatewayActions;

use crate::actions::NostrGatewayActions;
use crate::cli::NostaroCli;
use crate::identity::NostrIdentityAdmin;
use crate::runner::NostrAgentRunner;
use crate::session::{NostrSessionRuntime, NOSTR_SESSION_PREFIX};

/// Nostr の応答生成 + 返信配送の実体。`SubtaskCompletionSink` も実装する。
///
/// watch ループ（inbound）と完了 sink（resume）が同じ `runtime`（session ロック +
/// registry）・同じ `cli`（送信）・同じ `admin`（identity 切替）を共有する。
pub struct NostrResponder<R: NostrAgentRunner> {
    runner: R,
    cli: NostaroCli,
    runtime: Arc<NostrSessionRuntime>,
    admin: Arc<dyn NostrIdentityAdmin>,
    agent_id: String,
}

impl<R: NostrAgentRunner> Clone for NostrResponder<R> {
    fn clone(&self) -> Self {
        Self {
            runner: self.runner.clone(),
            cli: self.cli.clone(),
            runtime: self.runtime.clone(),
            admin: self.admin.clone(),
            agent_id: self.agent_id.clone(),
        }
    }
}

impl<R: NostrAgentRunner> NostrResponder<R> {
    pub fn new(
        runner: R,
        cli: NostaroCli,
        runtime: Arc<NostrSessionRuntime>,
        admin: Arc<dyn NostrIdentityAdmin>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            runner,
            cli,
            runtime,
            admin,
            agent_id: agent_id.into(),
        }
    }

    pub fn runtime(&self) -> &Arc<NostrSessionRuntime> {
        &self.runtime
    }

    /// [`Self::respond`] を per-session ロックの下で実行する（唯一の公開入口）。
    ///
    /// inbound（watch ループ）と resume（完了 sink）が同じロックを通るので、同一
    /// セッションに対して 2 本の応答生成が並行しない = 二重投稿しない。ロック取得を
    /// 呼び出し側の責務にすると 1 箇所の忘れで不変条件が壊れるため、ここに閉じ込める。
    /// `caller` は**このターンの呼び出し元**（#319）。inbound は受信イベントの発言者から
    /// 解決した値を、resume は親 run から運ばれてきた値（`SubtaskSettled.caller`）を渡す。
    /// 呼び出し側が持っている情報をそのまま受け取るだけで、ここでは導出も昇格もしない。
    pub async fn respond_serialized(
        &self,
        session_id: &str,
        reply_target: &str,
        prompt_suffix: &str,
        trigger_message_id: Option<&str>,
        caller: CallerIdentity,
        live_inbound_scope: opencrab_actions::LiveInboundScope,
    ) -> Option<String> {
        let fut = self.respond(
            session_id,
            reply_target,
            prompt_suffix,
            trigger_message_id,
            caller,
            live_inbound_scope,
        );
        self.runtime.run_serialized(session_id, fut).await
    }

    /// 会話を DB から再構築 → `run_agent_response`（非ブロック dispatch 付き）→
    /// 応答を生成してセッションへ転記する共通経路。**配送はしない**（エージェントが
    /// `nostr_post` / `nostr_reply` 等のツールで自分から行う・#588）。
    ///
    /// 呼び出しは [`Self::respond_serialized`] 経由に限る（直列化の担保）。
    /// 返り値は生成した応答本文（沈黙 = `NO_REPLY` / 空のときは `None`）。
    async fn respond(
        &self,
        session_id: &str,
        reply_target: &str,
        prompt_suffix: &str,
        trigger_message_id: Option<&str>,
        caller: CallerIdentity,
        live_inbound_scope: opencrab_actions::LiveInboundScope,
    ) -> Option<String> {
        let agent_id = self.agent_id.as_str();
        // #352: 本ターンの caller で index を絞る。caller=Agent（外部 Nostr の受信ターンが
        // 典型）には露出許可した skill だけを見せる。同じ caller を下の RunRequest にも載せる。
        let (base_prompt, agent_name) = self.runner.build_agent_context(agent_id, &caller);
        let system_prompt = format!("{base_prompt}\n\n{prompt_suffix}");

        let budget = self.runner.context_budget_tokens(agent_id);
        let conversation = self
            .runner
            .build_conversation_string(session_id, agent_id, budget)
            .unwrap_or_default();

        // Nostr の配送は**エージェントがツール（nostr_post / nostr_reply 等）で自分から行う**。
        // 機構は代わりに送らない（#588・オーナー指示「エージェントの送信に任せればいい」）。ここで
        // 作るのはそのツール群。
        let actions: Arc<dyn GatewayActions> =
            Arc::new(NostrGatewayActions::new(self.cli.clone()).with_admin(self.admin.clone()));

        // dispatch（S3a）: registry は session 単位で共有し（cancel_subtask 到達性）、
        // sink は自分自身（完了したらまた直列化下で resume する）。
        let registry = self.runtime.registry_for(session_id);
        let sink: Arc<dyn SubtaskCompletionSink> = Arc::new(self.clone());

        // 呼び出し元（`caller`）は引数で受け取る（#319）。以前はここが
        // `CallerIdentity::Agent` 固定で、オーナーが話しかけても外部の誰かが話しかけても
        // 同じ扱いだった。その結果 OWNER_ONLY / TRUSTED_ONLY のツールが list にも
        // dispatch にも出ず、エージェントは Nostr 発のターンから**自分の設定を一切変更
        // できなかった**。Discord は同じ場面で `resolve_caller` を通して発言者を見ている。
        //
        // **ここで導出しない**のが要点。inbound は受信イベントの `pubkey` を持っている
        // 場所（`handle_event`）で解決し、resume は親 run から運ばれた値
        // （`SubtaskSettled.caller` / #298）をそのまま使う。session_id から発言者を
        // 逆算するような再構築を挟むと、セッション規約を変えた瞬間に権限判定が壊れる。
        let mut req = RunRequest::new(
            agent_id,
            agent_name,
            session_id,
            system_prompt,
            conversation,
            "nostr",
            caller,
        )
        .with_gateway_actions(actions)
        .with_dispatch(Some(registry), sink)
        .with_reply_target(reply_target)
        // #323 / B2: 走行中注入を返信中の相手に絞り、別相手の新着が reply_target と
        // 食い違う本文を公開リレーへ誤爆させない。
        .with_live_inbound_scope(live_inbound_scope);
        if let Some(id) = trigger_message_id {
            req = req.with_trigger_message_id(id);
        }

        match self.runner.run_agent_response(req).await {
            Ok(result) => {
                let reply = result.response.trim().to_string();
                if reply.is_empty() || reply == "NO_REPLY" {
                    debug!(agent_id, session_id, "nostr: agent chose silence");
                    return None;
                }
                // 最終応答テキストを Nostr のセッションへ**無条件で**転記する（会話履歴の継続性）。
                // 外界への配送はエージェントがツールで行う（機構は publish しない・#588）ので、返信先の
                // 有無にかかわらずセッションに残す（オーナー指示「返信先がなくても Nostr のセッション上に
                // 残ればいい。ツールを使ったログが会話履歴にあれば自分で投稿したかどうかも分かる」）。
                //
                // #323 / B1: 返信先ノートがある時**だけ**宛先アンカーを焼く（記録専用・公開リレーへ送る
                // 本文には混ぜない。1 セッションに複数の相手が同居するため「誰宛か」を残す）。返信先が
                // 無いターン（時刻発火のブロードキャスト等）はアンカー無しでそのまま残す。
                let recorded = if reply_target.is_empty() {
                    reply.clone()
                } else {
                    format!(
                        "{reply}\n{anchor}",
                        anchor = crate::event::outbound_reply_anchor(reply_target)
                    )
                };
                self.runner.record_outbound_reply(
                    opencrab_actions::TranscriptSource::Nostr,
                    &opencrab_actions::OutboundReplyRecord {
                        agent_id,
                        session_id,
                        channel_id: None,
                        text: &recorded,
                        context: None,
                    },
                );
                Some(reply)
            }
            Err(e) => {
                error!(agent_id, session_id, error = %e, "nostr agent run failed");
                None
            }
        }
    }
}

/// 決着理由 → system prompt の 1 文目に入る述部（「…バックグラウンド処理が{…}。」）。
///
/// 継続 resume を起こす `SettleKind::Completed` は completed / stopped_by_limit /
/// error / timeout の**どれでも**発火する（値の出所は `actions/src/subtask.rs` の
/// `exit_reason`）。一律「完了しました」と告げると失敗・タイムアウトした subtask にも
/// 「完了」と伝わり、同じ prompt 内のマーカー（`exit_reason=timeout`）と食い違う。
/// #443（HB）で入れた exit_reason → 言い回しの写像と**同型**を Nostr へ適用する（#445）。
/// HB 側は #588 single-entry で継続ターン機構ごと撤去したので、この写像は現在 Nostr sink が持つ。
///
/// 未知の値は**断定しない**（「終了しました」）。正確な値は同じ prompt 内の
/// `[subtask_completed: … exit_reason=…]` マーカーがそのまま持つので、推測を足さない。
fn settle_outcome_sentence(exit_reason: &str) -> &'static str {
    match exit_reason {
        "completed" => "完了しました",
        "stopped_by_limit" => "反復上限に達して途中で打ち切られました",
        "error" => "エラーで失敗しました",
        "timeout" => "時間切れで打ち切られました",
        _ => "終了しました",
    }
}

/// resume 時に system prompt へ足す Nostr 固有の指示を組む。
///
/// 冒頭 1 文は `exit_reason` で分岐する（#443 の同型 / #445）。「結果は」→「詳細は」も
/// 中立化した。失敗・タイムアウトでも `subtask_completed` ログには理由本文が入る。
fn resume_prompt_suffix(reply_target: &str, subtask_id: &str, exit_reason: &str) -> String {
    let outcome = settle_outcome_sentence(exit_reason);
    // 配送はエージェントがツールで行う（機構は送らない・#588）。返信先ノートがあれば返信、無ければ
    // （時刻発火のブロードキャスト等）新規投稿へ誘導する。伝える必要がなければ黙ってよい。
    let deliver = if reply_target.trim().is_empty() {
        "伝えるなら nostr_post で投稿してください（今回は返信先ノートがありません）".to_string()
    } else {
        format!("相手へ伝えるなら nostr_reply(target=\"{reply_target}\") を使ってください（target は返信先ノート）")
    };
    format!(
        "[Nostr] 依頼されていたバックグラウンド処理が{outcome}。詳細は直前の会話ログの \
         subtask_completed に入っています。{deliver}。伝える必要がなければ NO_REPLY とだけ答えてください。\
         \n[subtask_completed: subtask_id={subtask_id}, exit_reason={exit_reason}]"
    )
}

impl<R: NostrAgentRunner> SubtaskCompletionSink for NostrResponder<R> {
    fn on_subtask_settled(&self, ev: SubtaskSettled) {
        // 決着（Completed）以外（進捗通知など）で resume すると、まだ走っている run の
        // 途中で二重に応答してしまう。型の意図をここで実際に守る。
        if ev.kind != SettleKind::Completed {
            debug!(
                session_id = %ev.session_id,
                kind = ?ev.kind,
                "nostr sink: not a completion, skipping resume"
            );
            return;
        }
        // 非 Nostr の親セッション（heartbeat-* / web-* / ネストした subtask-* 等）は
        // 正常系としてスキップする（Discord / web の sink も同様に前置きで弾く）。
        if !ev.session_id.starts_with(NOSTR_SESSION_PREFIX) {
            debug!(
                session_id = %ev.session_id,
                "nostr sink: parent session is not a nostr session, skipping resume"
            );
            return;
        }
        // 継続は **session_id の一致だけ**で起こす（#588 / #440）。返信先の有無で決めない。
        //
        // 継続は「自分が投げた subtask の結果を受けて続きを話す」ことなので、セッションが一致すれば
        // 十分（撤去した `HeartbeatContinuationSink` も session_id だけで判定していた）。以前は
        // 「返信先ノートが無ければ resume しない」としていたが、その根拠（届かない応答を作って
        // 転記してしまう）は #588 で消えた: 配送はエージェントがツールで行い（機構は送らない）、
        // 転記は返信先の有無に関わらず常に行う（セッションに残す）。返信先が無いブロードキャスト
        // （時刻発火）でも subtask の成果を受けて続きを話せる（#440 が塞いだ穴を開け直さない）。
        //
        // 返信先は正規化する（前後空白を落とし、空白のみは「返信先なし」＝ブロードキャスト扱い）。
        // これで転記のアンカー要否（`respond`）と誘導文（`resume_prompt_suffix`）の判定が揃う。
        let reply_target = ev
            .reply_target
            .clone()
            .unwrap_or_default()
            .trim()
            .to_string();

        let responder = self.clone();
        let sid = ev.session_id.clone();
        // **親 run の呼び出し元をそのまま引き継ぐ**（#298 が運んでいる値 / #319）。
        // ここを `CallerIdentity::Agent` 固定にしていたため、オーナー発のターンでも
        // subtask が決着した瞬間に権限が降格していた（Discord / web の sink は既に
        // `ev.caller` を使っている）。**引き継ぐだけ**で昇格はしない。
        let caller = ev.caller.clone();
        // sink は同期関数。resume は非同期なので spawn する（web gateway と同じ。
        // ここで待つと dispatch した subtask の完了処理を塞ぐ）。
        tokio::spawn(async move {
            let suffix = resume_prompt_suffix(&reply_target, &ev.subtask_id, &ev.exit_reason);
            // inbound の応答生成と直列化する（同一セッションで二重に返信しない）。
            // resume は生きた相手の識別子を持たない（`SubtaskSettled` に相手 pubkey は
            // 載っていない）ので走行中注入は `Silent`（#323 / B2）。別相手の新着が
            // reply_target と食い違う本文を公開リレーへ誤爆させない。
            responder
                .respond_serialized(
                    &sid,
                    &reply_target,
                    &suffix,
                    None,
                    caller,
                    opencrab_actions::LiveInboundScope::Silent,
                )
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Mutex;
    use std::time::Duration;

    use opencrab_actions::SubtaskSettled;
    use opencrab_core::EngineResult;
    use opencrab_db::queries::AgentNostrConfigRow;

    use crate::session::nostr_session_id;

    /// run_agent_response の観測 1 件
    /// （session_id, reply_target, dispatch 有効か, run に載った登録簿の実体）。
    ///
    /// 4 番目は **`Arc` の同一性**を見るために保持する。「dispatch が有効か」（3 番目の
    /// bool）だけでは、別インスタンスの登録簿を渡す壊れ方を検知できない。
    /// 5 番目は **run に載った呼び出し元**（#319）。以前はここが常に `Agent` 固定で、
    /// オーナー発のターンでも OWNER_ONLY / TRUSTED_ONLY のツールが出なかった。
    type RunObservation = (
        String,
        Option<String>,
        bool,
        Option<opencrab_actions::subtask::SubtaskRegistry>,
        CallerIdentity,
        // 6 番目: 走行中注入の対象範囲（#323 / B2）。sink が respond の scope を
        // RunRequest へ配線していることを検査する（ラベル文字列で保持）。
        String,
    );
    /// 転記された応答 1 件（agent_id, session_id, text）。
    type ReplyObservation = (String, String, String);

    /// テスト用の最小 `NostrAgentRunner`。LLM も DB も使わず、応答を差し替える。
    #[derive(Clone)]
    struct FakeRunner {
        response: String,
        runs: Arc<Mutex<Vec<RunObservation>>>,
        replies: Arc<Mutex<Vec<ReplyObservation>>>,
        /// run 中の待機（直列化テスト用）。
        delay: Duration,
        inflight: Arc<AtomicUsize>,
        max_inflight: Arc<AtomicUsize>,
        /// Some のとき「モデルが inline で nostr_reply を呼んだ」ことを模して、
        /// 渡された gateway_actions を実際に実行する（sent フラグ経路の検証）。
        explicit_reply_target: Option<String>,
        /// #588 Stage 2: 1 つだけ保持し `session_locks()` は毎回この clone を返す
        /// （trait の「プロセス全体で 1 実体を共有」契約を fake でも守る）。
        session_locks: std::sync::Arc<opencrab_actions::SessionLocks>,
    }

    impl FakeRunner {
        fn new(response: &str) -> Self {
            Self {
                response: response.to_string(),
                runs: Arc::new(Mutex::new(Vec::new())),
                replies: Arc::new(Mutex::new(Vec::new())),
                delay: Duration::from_millis(0),
                inflight: Arc::new(AtomicUsize::new(0)),
                max_inflight: Arc::new(AtomicUsize::new(0)),
                explicit_reply_target: None,
                session_locks: std::sync::Arc::new(opencrab_actions::SessionLocks::new()),
            }
        }

        fn with_delay(mut self, d: Duration) -> Self {
            self.delay = d;
            self
        }

        /// 「モデルがターン中に nostr_reply を明示実行する」挙動を仕込む。
        fn with_explicit_reply(mut self, target: &str) -> Self {
            self.explicit_reply_target = Some(target.to_string());
            self
        }

        /// #588: 配送は機構が行わなくなったので、resume / inbound が「走ってセッションへ
        /// 転記された」ことは**記録**（`replies`）で観測する（旧テストが送信ログ `fake.sent()` を
        /// 同期点に使っていた箇所の置き換え）。転記本文のどれかが `needle` を含めば true。
        async fn wait_for_reply(&self, needle: &str) -> bool {
            for _ in 0..100 {
                if self
                    .replies
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r.2.contains(needle))
                {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            false
        }
    }

    #[async_trait::async_trait]
    impl opencrab_actions::AgentRuntime for FakeRunner {
        async fn run_agent_response(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
            let now = self.inflight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            self.max_inflight.fetch_max(now, AtomicOrdering::SeqCst);
            let scope_label = match &req.live_inbound_scope {
                opencrab_actions::LiveInboundScope::AllOthers => "all".to_string(),
                opencrab_actions::LiveInboundScope::OnlySpeaker(pk) => format!("only:{pk}"),
                opencrab_actions::LiveInboundScope::Silent => "silent".to_string(),
            };
            self.runs.lock().unwrap().push((
                req.session_id.clone(),
                req.reply_target.clone(),
                req.completion_sink.is_some() && req.subtask_registry.is_some(),
                req.subtask_registry.clone(),
                req.caller.clone(),
                scope_label,
            ));
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            // モデルが配送系ツールを inline 実行するケース（sent フラグを立てる経路）。
            if let (Some(target), Some(ga)) =
                (&self.explicit_reply_target, req.gateway_actions.as_ref())
            {
                let ctx = opencrab_gateway::GatewayCallContext::for_agent(&req.agent_id);
                let r = ga
                    .execute(
                        "nostr_reply",
                        &serde_json::json!({"target": target, "text": "明示送信"}),
                        &ctx,
                    )
                    .await;
                assert!(
                    r.success,
                    "fake nostaro での明示送信は成功する: {:?}",
                    r.error
                );
            }
            self.inflight.fetch_sub(1, AtomicOrdering::SeqCst);
            Ok(EngineResult {
                response: self.response.clone(),
                iterations: 1,
                tool_calls_made: 0,
                stopped_by_limit: false,
                xml_fallback_parses: 0,
            })
        }

        fn build_agent_context(
            &self,
            _agent_id: &str,
            _caller: &CallerIdentity,
        ) -> (String, String) {
            ("base prompt".to_string(), "テストくん".to_string())
        }

        fn build_conversation_string(
            &self,
            _session_id: &str,
            _agent_id: &str,
            _budget: usize,
        ) -> anyhow::Result<String> {
            Ok("conversation".to_string())
        }

        fn context_budget_tokens(&self, _agent_id: &str) -> usize {
            1000
        }

        fn has_llm_providers(&self) -> bool {
            true
        }

        fn agent_exists(&self, _agent_id: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        fn session_locks(&self) -> std::sync::Arc<opencrab_actions::SessionLocks> {
            self.session_locks.clone()
        }

        fn ensure_session(&self, _s: &str, _a: &[String], _t: &str, _m: &str, _mode: &str) {}

        fn record_outbound_reply(
            &self,
            source: opencrab_actions::TranscriptSource,
            record: &opencrab_actions::OutboundReplyRecord<'_>,
        ) {
            assert_eq!(source, opencrab_actions::TranscriptSource::Nostr);
            self.replies.lock().unwrap().push((
                record.agent_id.to_string(),
                record.session_id.to_string(),
                record.text.to_string(),
            ));
        }

        // 以下はこの sink の経路が使わない（受信転記/NO_REPLY/掃除）。
        fn record_inbound_message(
            &self,
            _source: opencrab_actions::TranscriptSource,
            _record: &opencrab_actions::InboundMessageRecord<'_>,
        ) -> bool {
            unimplemented!("nostr の fake は受信転記を使わない")
        }

        fn on_inbound_message(
            &self,
            _source: opencrab_actions::TranscriptSource,
            _agent_id: &str,
            _record: &opencrab_actions::InboundMessageRecord<'_>,
        ) {
            unimplemented!("nostr の fake は受信フックを使わない")
        }

        fn record_interaction_response(
            &self,
            _agent_id: &str,
            _session_id: &str,
            _record: &opencrab_actions::InteractionRecord<'_>,
        ) {
            unimplemented!("nostr の fake は A2UI interaction を使わない")
        }

        fn record_agent_no_reply(&self, _agent_id: &str, _session_id: &str) {
            unimplemented!("nostr の fake は NO_REPLY 記録を使わない")
        }

        fn session_theme(&self, _session_id: &str) -> Option<String> {
            unimplemented!("nostr の fake は session_theme を使わない")
        }

        fn mark_interaction_status(&self, _i: &str, _s: &str, _r: Option<&str>, _u: Option<&str>) {
            unimplemented!("nostr の fake は A2UI interaction を使わない")
        }

        fn cleanup_stale_interactions(&self) {
            unimplemented!("nostr の fake は A2UI interaction を使わない")
        }

        fn cleanup_stale_interactions_for_agent(&self, _agent_id: &str) {
            unimplemented!("nostr の fake は A2UI interaction を使わない")
        }
    }

    impl NostrAgentRunner for FakeRunner {
        /// この sink は呼び出し元を**導出しない**（受け取るだけ）。解決の配線は
        /// 受信ループ側（`manager` のテスト）と server 側の実体でテストする。
        fn resolve_nostr_caller(&self, _agent_id: &str, _author_pubkey: &str) -> CallerIdentity {
            unreachable!("応答生成経路は呼び出し元を導出しない（引数で受け取る / #319）")
        }

        fn list_enabled_nostr_configs(&self) -> Vec<AgentNostrConfigRow> {
            Vec::new()
        }

        fn get_nostr_config(&self, _agent_id: &str) -> Option<AgentNostrConfigRow> {
            None
        }

        fn set_nostr_secret_key(&self, _agent_id: &str, _secret_key: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn set_nostr_self_pubkey(&self, _agent_id: &str, _self_pubkey: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn upsert_nostr_config(&self, _cfg: &AgentNostrConfigRow) -> anyhow::Result<()> {
            Ok(())
        }

        fn set_nostr_enabled(&self, _agent_id: &str, _enabled: bool) -> anyhow::Result<()> {
            Ok(())
        }

        fn resolve_nostr_relay_target(
            &self,
            _agent_id: &str,
        ) -> Option<opencrab_actions::webhook_target::WebhookConfig> {
            // この経路（応答生成 sink）は転記に関与しないので未設定扱い。
            None
        }

        fn relay_inbound_notification(
            &self,
            _target: &opencrab_actions::webhook_target::WebhookConfig,
            _text: String,
        ) {
        }

        fn agent_workspace_root(&self, _agent_id: &str) -> Option<std::path::PathBuf> {
            None
        }
    }

    struct NoopAdmin;

    #[async_trait::async_trait]
    impl NostrIdentityAdmin for NoopAdmin {
        async fn adopt_generated_identity(
            &self,
            _agent_id: &str,
            npub: &str,
        ) -> anyhow::Result<String> {
            Ok(npub.to_string())
        }
    }

    /// 送信を観測するための fake nostaro（argv を 1 行ずつ log へ追記するスクリプト）。
    /// 実リレーへは一切繋がない。
    struct FakeNostaro {
        _dir: tempfile::TempDir,
        script: std::path::PathBuf,
        log: std::path::PathBuf,
    }

    impl FakeNostaro {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("sent.log");
            let script = crate::test_support::write_fake_nostaro(
                dir.path(),
                &format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display()),
            );
            Self {
                _dir: dir,
                script,
                log,
            }
        }

        fn cli(&self) -> NostaroCli {
            NostaroCli::new().with_binary_path(self.script.to_string_lossy().to_string())
        }

        fn sent(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    fn responder(runner: FakeRunner, cli: NostaroCli) -> NostrResponder<FakeRunner> {
        NostrResponder::new(
            runner,
            cli,
            Arc::new(NostrSessionRuntime::new()),
            Arc::new(NoopAdmin),
            "agent-sink-test",
        )
    }

    fn settled_with_caller(
        session_id: &str,
        reply_target: Option<&str>,
        caller: CallerIdentity,
    ) -> SubtaskSettled {
        SubtaskSettled {
            session_id: session_id.to_string(),
            agent_id: "agent-sink-test".to_string(),
            subtask_id: "st-1".to_string(),
            exit_reason: "completed".to_string(),
            kind: SettleKind::Completed,
            reply_target: reply_target.map(|s| s.to_string()),
            caller,
        }
    }

    /// 呼び出し元を指定しない既定（最小権限）の `settled`。
    fn settled(session_id: &str, reply_target: Option<&str>) -> SubtaskSettled {
        settled_with_caller(session_id, reply_target, CallerIdentity::Agent)
    }

    /// resume は応答を `reply_target` 宛アンカー付きでセッションへ転記する（session_id からは
    /// 復元できない宛先を記録に残す）。#588: 配送は機構が行わない（エージェントがツールで送る）ので、
    /// ここは**記録**を見る。
    #[tokio::test]
    async fn sink_records_reply_with_target_anchor() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("鍵ができました");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        r.on_subtask_settled(settled(&sid, Some("note1target")));

        assert!(
            runner.wait_for_reply("note1target").await,
            "reply_target 宛アンカー付きで転記されるべき: replies={:?}",
            runner.replies.lock().unwrap()
        );
        // 機構は publish しない（配送はエージェントのツール）。
        assert!(
            fake.sent().is_empty(),
            "機構は暗黙返信しない: {}",
            fake.sent()
        );
        // 記録には本文 + 宛先アンカーが載る。
        let replies = runner.replies.lock().unwrap();
        assert_eq!(replies.len(), 1);
        assert!(
            replies[0].2.contains("鍵ができました")
                && replies[0].2.contains("[Nostr reply target=note1target]"),
            "記録は本文 + 宛先アンカー: {}",
            replies[0].2
        );
        // resume も dispatch 有効（registry + sink）で走り、reply_target を引き継ぐ。
        let runs = runner.runs.lock().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, sid);
        assert_eq!(runs[0].1.as_deref(), Some("note1target"));
        assert!(runs[0].2, "resume も非ブロック dispatch を有効化する");
        // #323 / B2: resume は相手が不定なので走行中注入は Silent（別相手の誤爆防止）。
        assert_eq!(runs[0].5, "silent", "resume の走行中注入は Silent");
    }

    /// [#323 / B1] outbound の記録には返信先アンカーが載る（記録専用 / inbound_anchor と対称）。
    /// tool_call 行を作らない転記経路なので、これが無いと「この返信が誰宛か」を復元できない。
    /// #588: 機構は publish しないので `fake.sent()` は空（配送はエージェントのツール）。
    #[tokio::test]
    async fn outbound_record_carries_reply_target_anchor() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("返答本文");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        r.respond_serialized(
            &sid,
            "note1target",
            "suffix",
            Some("evt-1"),
            CallerIdentity::Agent,
            opencrab_actions::LiveInboundScope::OnlySpeaker("pk-peer".to_string()),
        )
        .await;

        // 記録された outbound には宛先アンカーが載る（誰宛か復元可能）。
        let replies = runner.replies.lock().unwrap();
        assert_eq!(replies.len(), 1);
        assert!(
            replies[0].2.contains("返答本文")
                && replies[0].2.contains("[Nostr reply target=note1target]"),
            "記録は本文 + 宛先アンカー: {}",
            replies[0].2
        );
        // #588: 機構は暗黙返信しない（配送はエージェントが nostr_reply 等で行う）。
        assert!(
            fake.sent().is_empty(),
            "機構は publish しない: {}",
            fake.sent()
        );
    }

    // ---- #319: 呼び出し元は導出せず、呼び出し側から受け取る ----

    /// **本丸（inbound）**: 渡された呼び出し元がそのまま run に載る。
    ///
    /// 以前はここが `CallerIdentity::Agent` 固定で、オーナー発のターンでも
    /// OWNER_ONLY / TRUSTED_ONLY のツールが list にも dispatch にも出なかった（#319）。
    /// 発言者の解決は受信イベントの `pubkey` を持つ `handle_event` の責務で、
    /// ここでは**受け取った値をそのまま使う**（session_id からの逆算はしない）。
    #[tokio::test]
    async fn inbound_turn_uses_the_caller_it_was_given() {
        for caller in [
            CallerIdentity::Owner,
            CallerIdentity::TrustedUser,
            CallerIdentity::Agent,
        ] {
            let fake = FakeNostaro::new();
            let runner = FakeRunner::new("応答");
            let r = responder(runner.clone(), fake.cli());
            let sid = nostr_session_id("agent-sink-test");

            r.respond_serialized(
                &sid,
                "note1target",
                "suffix",
                Some("evt-1"),
                caller.clone(),
                opencrab_actions::LiveInboundScope::AllOthers,
            )
            .await;

            let runs = runner.runs.lock().unwrap();
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].4, caller, "渡した呼び出し元が run に載っていない");
        }
    }

    /// **本丸（resume）**: subtask 完了 resume は親 run の呼び出し元
    /// （`SubtaskSettled.caller` / #298）を引き継ぐ。
    ///
    /// ここが `Agent` 固定だったため、オーナー発のターンでも subtask が決着した瞬間に
    /// 権限が降格していた（`report_progress` を呼ぶと自分の権限が落ちる、という自爆）。
    #[tokio::test]
    async fn resume_turn_inherits_the_parent_caller() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("完了しました");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        r.on_subtask_settled(settled_with_caller(
            &sid,
            Some("note1target"),
            CallerIdentity::Owner,
        ));
        assert!(
            runner.wait_for_reply("note1target").await,
            "resume が走ること"
        );

        let runs = runner.runs.lock().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].4,
            CallerIdentity::Owner,
            "resume で親ターンの権限が落ちている"
        );
    }

    /// 引き継ぐだけで**昇格はしない**: 親が最小権限なら resume も最小権限のまま。
    #[tokio::test]
    async fn resume_does_not_escalate_a_least_privileged_parent() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("完了しました");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        r.on_subtask_settled(settled_with_caller(
            &sid,
            Some("note1target"),
            CallerIdentity::Agent,
        ));
        assert!(runner.wait_for_reply("note1target").await);

        assert_eq!(
            runner.runs.lock().unwrap()[0].4,
            CallerIdentity::Agent,
            "resume で権限が上がった"
        );
    }

    /// #588 / #440: `reply_target` が無くても（ブロードキャストの時刻発火など）継続は起こる
    /// （判定は session_id の一致だけ）。応答はセッションへアンカー無しで転記され、publish はしない
    /// （配送はエージェントのツール）。以前は「返信先が無ければ resume しない」だったが、その根拠
    /// （届かない応答を転記してしまう）は暗黙返信の撤去で消えた。
    #[tokio::test]
    async fn resume_without_reply_target_records_but_does_not_publish() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("ブロードキャストの続き");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        r.on_subtask_settled(settled(&sid, None));
        // 空白のみも「返信先なし」扱い（正規化される）。
        r.on_subtask_settled(settled(&sid, Some("   ")));

        assert!(
            runner.wait_for_reply("ブロードキャストの続き").await,
            "返信先が無くても継続ターンが走ってセッションへ転記される: replies={:?}",
            runner.replies.lock().unwrap()
        );
        // 機構は publish しない（配送はエージェントのツール）。
        assert!(fake.sent().is_empty(), "機構は送信しない: {}", fake.sent());
        // 転記は本文のみ（返信先が無いのでアンカーは付かない）。
        let replies = runner.replies.lock().unwrap();
        assert!(
            replies
                .iter()
                .all(|r| !r.2.contains("[Nostr reply target=")),
            "返信先なしの転記にアンカーは付かない: {replies:?}"
        );
    }

    /// 非 Nostr セッションの settle は無視する（web / heartbeat のネスト等）。
    #[tokio::test]
    async fn sink_ignores_non_nostr_sessions() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("x");
        let r = responder(runner.clone(), fake.cli());

        r.on_subtask_settled(settled("web-agent-x-conv1", Some("note1target")));
        r.on_subtask_settled(settled("heartbeat-agent-x", Some("note1target")));

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(fake.sent().is_empty());
        assert!(runner.runs.lock().unwrap().is_empty());
    }

    /// NO_REPLY / 空応答なら送信しない（沈黙の尊重）。
    #[tokio::test]
    async fn no_reply_response_is_not_delivered() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("NO_REPLY");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        let out = r
            .respond_serialized(
                &sid,
                "note1target",
                "suffix",
                None,
                CallerIdentity::Agent,
                opencrab_actions::LiveInboundScope::AllOthers,
            )
            .await;
        assert!(out.is_none());
        assert!(fake.sent().is_empty());
        // 転記もしない（送っていない応答を履歴に残さない）。
        assert!(runner.replies.lock().unwrap().is_empty());
    }

    /// #588: 配送はエージェントの明示送信だけ。モデルが `nostr_reply` を実行したものが届き、
    /// 機構はそれに**加えて**送ったりしない（暗黙返信は撤去済み）。送信は 1 回だけ。
    #[tokio::test]
    async fn only_the_agents_explicit_send_is_delivered() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("本文").with_explicit_reply("note1explicit");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        let out = r
            .respond_serialized(
                &sid,
                "note1implicit",
                "suffix",
                Some("evt-1"),
                CallerIdentity::Agent,
                opencrab_actions::LiveInboundScope::OnlySpeaker("pk-peer".to_string()),
            )
            .await;
        assert_eq!(out.as_deref(), Some("本文"));

        // エージェントが送った 1 通だけ。機構は reply_target（note1implicit）へ何も送らない。
        let sent = fake.sent();
        assert!(sent.contains("note1explicit"), "明示送信が届く: {sent}");
        assert!(
            !sent.contains("note1implicit"),
            "機構は reply_target へ暗黙返信しない: {sent}"
        );
        assert_eq!(
            sent.lines().filter(|l| l.contains("reply")).count(),
            1,
            "送信はエージェントの 1 回だけ: {sent}"
        );
        // 応答本文の転記は行う（会話履歴の継続性）。
        let replies = runner.replies.lock().unwrap();
        assert_eq!(replies.len(), 1);
        // #323 / B1: 記録には宛先アンカーが焼かれる（「誰宛か」を復元できる）。
        // アンカーの target はこのターンの reply_target。
        assert!(
            replies[0].2.contains("[Nostr reply target=note1implicit]"),
            "記録に宛先アンカーが載る: {}",
            replies[0].2
        );
        // #323 / B2: respond の scope が RunRequest まで配線されている。
        let runs = runner.runs.lock().unwrap();
        assert_eq!(runs[0].5, "only:pk-peer", "走行中注入の対象範囲を配線する");
    }

    /// #588: 明示送信が無ければ**何も publish されない**が、応答はセッションへ転記される
    /// （オーナー指示: 返信先があってもツールを呼ばなければ出ない。履歴には残る）。
    #[tokio::test]
    async fn no_explicit_send_records_but_does_not_publish() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("ツールを呼ばない応答");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        r.respond_serialized(
            &sid,
            "note1implicit",
            "suffix",
            Some("evt-1"),
            CallerIdentity::Agent,
            opencrab_actions::LiveInboundScope::AllOthers,
        )
        .await;
        // 機構は publish しない。
        assert!(
            fake.sent().is_empty(),
            "ツール未使用なら何も出ない: {}",
            fake.sent()
        );
        // ただしセッションへは転記される（本文 + 宛先アンカー）。
        let replies = runner.replies.lock().unwrap();
        assert_eq!(replies.len(), 1);
        assert!(
            replies[0].2.contains("ツールを呼ばない応答")
                && replies[0].2.contains("[Nostr reply target=note1implicit]"),
            "本文 + 宛先アンカーを記録: {}",
            replies[0].2
        );
    }

    /// 同一セッションでは inbound 相当の respond と resume が直列化される。
    #[tokio::test]
    async fn resume_serializes_with_inbound_on_same_session() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("ok").with_delay(Duration::from_millis(120));
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        // inbound 相当（watch ループと同じ入口）を走らせつつ、途中で完了 sink を発火。
        let r2 = r.clone();
        let sid2 = sid.clone();
        let inbound = tokio::spawn(async move {
            r2.respond_serialized(
                &sid2,
                "note1inbound",
                "suffix",
                Some("evt-1"),
                CallerIdentity::Agent,
                opencrab_actions::LiveInboundScope::AllOthers,
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        r.on_subtask_settled(settled(&sid, Some("note1resume")));

        inbound.await.unwrap();
        assert!(
            runner.wait_for_reply("note1resume").await,
            "resume も転記される"
        );
        // 直列化されているので LLM 実行が重なることはない。
        assert_eq!(
            runner.max_inflight.load(AtomicOrdering::SeqCst),
            1,
            "同一セッションの応答生成は同時に 1 本まで（二重回答の防止）"
        );
        assert_eq!(runner.runs.lock().unwrap().len(), 2);
    }

    /// 別セッション（#323 以降は**別エージェント**）は直列化されず並行する。
    #[tokio::test]
    async fn different_sessions_are_not_serialized() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("ok").with_delay(Duration::from_millis(150));
        let r = responder(runner.clone(), fake.cli());

        r.on_subtask_settled(settled(&nostr_session_id("agent-sink-a"), Some("note1a")));
        r.on_subtask_settled(settled(&nostr_session_id("agent-sink-b"), Some("note1b")));

        assert!(runner.wait_for_reply("note1a").await);
        assert!(runner.wait_for_reply("note1b").await);
        assert!(
            runner.max_inflight.load(AtomicOrdering::SeqCst) >= 2,
            "別セッションは並行して走れる"
        );
    }

    /// dispatch した subtask は session 共有 registry に載り、`cancel_subtask` から
    /// 到達できる（別 registry を渡すと常に not found になる回帰の防止 / #169）。
    #[tokio::test]
    async fn registry_is_shared_between_inbound_and_resume() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("ok");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        let inbound_registry = r.runtime().registry_for(&sid);

        // **応答生成に実際に渡された登録簿**が、停止処理が引くものと同一 Arc であること。
        //
        // ここを `registry_for(&sid)` 同士の比較で書くと `SubtaskRegistries` の恒真式に
        // なり、`respond` 側が別インスタンスを渡す壊れ方を 1 件も検知できない（実際、
        // 旧テストは `sink.rs` の `registry_for(session_id)` を新規 DashMap に差し替えても
        // 緑のままだった / #203 の一括点検）。捕まえたいのは配線なので、`FakeRunner` が
        // 捕捉した `RunRequest` の中身を見る（`web-gateway` の
        // `run_uses_the_gateways_registry_so_cancel_can_reach_it` と同じ形）。
        //
        // inbound（watch ループの入口）と resume（完了 sink）の**両経路**を見る:
        // どちらか一方だけ配線が外れても停止が届かなくなる。
        r.respond_serialized(
            &sid,
            "note1inbound",
            "suffix",
            Some("evt-1"),
            CallerIdentity::Agent,
            opencrab_actions::LiveInboundScope::AllOthers,
        )
        .await;
        r.on_subtask_settled(settled(&sid, Some("note1resume")));
        assert!(
            runner.wait_for_reply("note1resume").await,
            "resume が走ること"
        );

        {
            let runs = runner.runs.lock().unwrap();
            assert_eq!(runs.len(), 2, "inbound と resume で 2 回走る");
            for (label, obs) in [("inbound", &runs[0]), ("resume", &runs[1])] {
                let observed = obs
                    .3
                    .as_ref()
                    .unwrap_or_else(|| panic!("{label}: run に登録簿が載っていない"));
                assert!(
                    Arc::ptr_eq(observed, &inbound_registry),
                    "{label}: 応答生成に渡した登録簿が、停止処理が引くものと別インスタンス\
                     になっている（cancel_subtask が常に not found になる）"
                );
            }
        }

        // 走行中 subtask を模して登録 → has_running が真。
        inbound_registry.insert(
            "st-live".to_string(),
            opencrab_actions::SpawnedSubtask {
                abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
                session_id: "subtask-st-live".to_string(),
                parent_session_id: sid.clone(),
                agent_id: "agent-sink-test".to_string(),
                label: "nostr_generate_key(sunny)".to_string(),
                tool_name: "spawn_subtask".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: Some("note1target".to_string()),
                caller: opencrab_actions::CallerIdentity::Agent,
                lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            },
        );
        assert!(r.runtime().has_running(&sid));

        // 同じ registry を引く `cancel_subtask`（server-neutral / #161）で停止できる。
        let db = opencrab_db::Db::memory().unwrap();
        let outcome = opencrab_actions::cancel_subtask(
            &r.runtime().registry_for(&sid),
            &db,
            None,
            None,
            "st-live",
            opencrab_actions::CallerIdentity::Agent,
            Some(&sid),
        );
        assert_eq!(outcome, opencrab_actions::CancelOutcome::Cancelled);
        assert!(!r.runtime().has_running(&sid));
    }

    /// **#445**（#443 の同型）: 完了以外の決着で「完了しました」と断言しない。
    ///
    /// resume を起こす `SettleKind::Completed` は timeout / error / stopped_by_limit でも
    /// 発火するので、一律「完了」と告げると同じ prompt のマーカー（`exit_reason=timeout`）
    /// と矛盾する。各決着の述部が入り、マーカーは生の `exit_reason` をそのまま持つことも見る。
    #[test]
    fn resume_suffix_never_claims_completion_for_unfinished_subtasks() {
        for (exit_reason, expected) in [
            ("timeout", "時間切れで打ち切られました"),
            ("error", "エラーで失敗しました"),
            ("stopped_by_limit", "反復上限に達して途中で打ち切られました"),
            // 未知の値は断定しない（`subtask.rs` が語彙を増やしても誤情報にならない）。
            ("weird_new_reason", "終了しました"),
        ] {
            let suffix = resume_prompt_suffix("note1target", "st-1", exit_reason);
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
    fn resume_suffix_states_completion_only_when_completed() {
        let suffix = resume_prompt_suffix("note1target", "st-1", "completed");
        assert!(
            suffix.contains("バックグラウンド処理が完了しました"),
            "完了は完了と伝える: {suffix}"
        );
    }

    /// #588: 返信先ノートが無い（ブロードキャストの時刻発火）resume は、`nostr_reply` の空 target
    /// ではなく `nostr_post` の新規投稿へ誘導する。
    #[test]
    fn resume_suffix_guides_to_post_when_no_reply_target() {
        let suffix = resume_prompt_suffix("", "st-1", "completed");
        assert!(
            suffix.contains("nostr_post で投稿"),
            "返信先が無ければ新規投稿へ誘導: {suffix}"
        );
        assert!(
            !suffix.contains("nostr_reply(target=\"\")"),
            "空の返信先へ返信させない: {suffix}"
        );
    }
}
