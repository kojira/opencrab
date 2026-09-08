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

use opencrab_actions::{CallerIdentity, RunRequest, SubtaskCompletionSink, SubtaskSettled};
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

        // #665: 会話履歴の構築（DB からターンの文脈を組む）。この後 run_agent_response（engine）へ渡す。
        // ここが重い/詰まると LLM リクエスト前で止まる（llm_logs に行が出ない宙吊りの前段）。入と出を出す。
        let budget =
            match self
                .runner
                .context_budget_tokens(agent_id, session_id, &system_prompt, "")
            {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(
                        agent_id,
                        session_id,
                        error_name = e.name(),
                        "{name}: {e}",
                        name = e.name()
                    );
                    return None;
                }
            };
        debug!(
            agent_id,
            session_id,
            stage = "context_build",
            "turn: 文脈構築 開始（入）"
        );
        let conversation = self
            .runner
            .build_conversation_string(session_id, agent_id, budget, &system_prompt, "")
            .unwrap_or_default();
        debug!(
            agent_id,
            session_id,
            conversation_len = conversation.len(),
            stage = "context_build",
            "turn: 文脈構築 完了（出）"
        );

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
                // 第一柱: NO_REPLY 終端解釈で前段のみを転記する。後続ゴミは破棄し
                // （非空なら破棄ログ）、前段が空なら沈黙。#890 §11: 続けて末尾 CONTINUE も
                // 剥がす（NO_REPLY→CONTINUE を 1 経路で確定）。
                let Some(reply) = opencrab_actions::visible_speech_after_markers(
                    &result.response,
                    opencrab_actions::DeliveryContext {
                        session_id,
                        agent_id,
                        origin: "nostr",
                    },
                )
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()) else {
                    debug!(agent_id, session_id, "nostr: agent chose silence");
                    return None;
                };
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
                // #665: 応答をセッションへ転記する（Nostr は外界配送せずここに残す）。ターン末尾の
                // DB 書き込み段。入と出で挟み、転記で詰まる形（DB ロック待ち等）も切り分けられるようにする。
                debug!(
                    agent_id,
                    session_id,
                    stage = "record_reply",
                    "turn: 応答転記 開始（入）"
                );
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
                debug!(
                    agent_id,
                    session_id,
                    stage = "record_reply",
                    "turn: 応答転記 完了（出）"
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
    fn session_prefix(&self) -> &'static str {
        NOSTR_SESSION_PREFIX
    }
    /// 進捗では継続しない（まだ走っている run の途中で二重に応答してしまう）。転送するのは
    /// Discord だけ（#638）。
    fn forwards_progress(&self) -> bool {
        false
    }
    fn deliver_continuation(&self, ev: SubtaskSettled) {
        // kind の検査も親セッションの検査も `dispatch_settled`（#638）が済ませている。
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
#[path = "sink/tests/mod.rs"]
mod tests;
