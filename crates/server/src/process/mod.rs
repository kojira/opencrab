//! エージェントのメッセージ処理に関する共通ロジック。
//!
//! REST API (`api/sessions.rs`) と Discordゲートウェイ (`discord.rs`) の
//! 両方から利用される。

use std::sync::Arc;

use tracing::Instrument;

use opencrab_core::LlmCallLog;
use opencrab_llm::pricing::PricingRegistry;

use crate::llm_adapter::{LlmRouterAdapter, MetricsContext};
use crate::AppState;

mod budget;
mod callbacks;
mod live_inbound;
mod loop_restart;
mod prompt;
mod skills;
mod wiring;

pub use budget::{
    build_conversation_string, build_conversation_string_with_memory_index,
    build_conversation_string_with_waters, check_agent_model_change, compute_context_budget,
    core_functions_tokens, ensure_functions_within_cap, ensure_model_context_window_registered,
    ensure_model_max_output_tokens_registered, ensure_request_functions_budget,
    ensure_startup_budget_inputs, include_memory_index, measure_functions_tokens,
    model_context_window_missing_message, normalize_model_spec, resolve_agent_request_envelope,
    resolve_model_max_output_tokens, resolve_water_levels, split_llm_model_spec,
    ContextBudgetEnvelope, ContextBudgetError, ContextBudgetPolicy, MemoryIndexDecision,
    RequestEnvelopeArgs, DEFAULT_MEMORY_INDEX_TOKEN_CAP,
};
pub use live_inbound::{prepend_runtime_context, prepend_runtime_context_discord};
pub use prompt::{build_agent_context, build_more_tools_index};
#[allow(unused_imports)]
pub(crate) use wiring::{
    build_turn_executor, effective_allowed_commands, resolve_run_tools_config, TurnExecutorWiring,
};

use budget::{format_single_log, spawn_background_turn_end_snapshot, typed_exceeds_input_budget};
use callbacks::{
    merge_image_urls, set_llm_log_callback, set_run_notifier_callbacks, set_turn_log_callbacks,
};
use live_inbound::{SessionLiveInbound, SubtaskSteerInbound};
use loop_restart::prepare_loop_restart;
use skills::{record_used_skills, spawn_background_index_build};

/// ピアレビュアーのロスターセクションを組み立てる。
///
/// trusted_users の permission='co-agent' 行（選定ロジックは
/// `queries::list_co_agent_reviewers` に一元化 — reviewer 解決側と共有）。
/// ロスターは変更頻度が低いので system prompt 配置で問題ない（毎 run DB から再構築される）。
///
/// 経路も reviewer 解決と同じ [`crate::peer_review::REVIEWER_PLATFORM`]（#159）。返信を
/// 受理できない経路の相手を載せると、指名はできるが回収されない依頼になる。
///
/// **表示名だけを出す**（#158 S2）。共有プロンプトは transport 非依存でなければならず、
/// メンション記法（`<@id>`）の組み立ては transport 側の責務。reviewer の解決は
/// 「表示名優先・登録済みのみ」（`resolve_reviewer`）なので表示名で引ける。
/// 表示名が空の行は名前で指名できないため載せない（モデルに識別子を推測させない）。
///
/// #920: Peer Review 節の撤去に伴い prompt 組み立てからは外した（呼び出し元は消えたが、
/// 関数本体の撤去は #921 で行う。tests がまだ本関数を直接検証している）。
#[allow(dead_code)]
fn peer_reviewers_section(conn: &rusqlite::Connection, agent_id: &str) -> String {
    let reviewers: Vec<String> = opencrab_db::queries::list_co_agent_reviewers(
        conn,
        crate::peer_review::REVIEWER_PLATFORM,
        agent_id,
    )
    .unwrap_or_default()
    .into_iter()
    .filter(|u| !u.display_name.is_empty())
    .map(|u| format!("- {}", u.display_name))
    .collect();
    if reviewers.is_empty() {
        String::new()
    } else {
        format!(
            "\nYour registered peer reviewers (pass their display name as `reviewer`):\n{}\n",
            reviewers.join("\n")
        )
    }
}

#[cfg(test)]
#[path = "tests/peer_reviewers_section.rs"]
mod peer_reviewers_section_tests;

/// 実行対象の agent 行が `agents` に存在しないときのエラー（#632）。
///
/// `run_agent_response` は**サーバ側の全ターン実行が通る唯一のチョークポイント**
/// （REST `agents_messages`、scheduler / intake / sleep / subtask、そして web も
/// production では `AppState::run_agent_response` 経由でここを通る）。
/// エージェント別テーブルには FK 制約が無く、存在しない agent_id でも per-agent 設定が
/// 既定に落ちたまま「動いてしまう」。ここで 1 度だけ弾けば、入口ごとにチェックを
/// 手でコピーする必要がなくなり、将来の入口も自動的に閉じる。
///
/// HTTP ハンドラはこのエラーを `downcast_ref` して 404 に写像する。
#[derive(Debug, Clone)]
pub struct AgentNotFound {
    pub agent_id: String,
}

impl std::fmt::Display for AgentNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "agent not found: {}", self.agent_id)
    }
}

impl std::error::Error for AgentNotFound {}

/// エージェントにメッセージを処理させ、応答テキストを返す。
///
/// SkillEngine + BridgedExecutor + LlmRouterAdapter のフルパイプラインを実行する。
/// 実行要求は `RunRequest`（#33: 13位置引数の置き換え）で受ける。
///
/// **#632: 実行対象の agent 行が無ければ、何も実行せず [`AgentNotFound`] を返す。**
/// これがサーバ側ターン実行の単一チョークポイントである（詳細は [`AgentNotFound`]）。
pub async fn run_agent_response(
    state: &AppState,
    req: opencrab_actions::RunRequest,
) -> anyhow::Result<opencrab_core::EngineResult> {
    let agent_id = req.agent_id.as_str();
    let agent_name = req.agent_name.as_str();
    let session_id = req.session_id.as_str();
    let system_prompt = req.system_prompt.as_str();
    let conversation = req.conversation.as_str();
    let gateway = req.gateway.as_str();
    let depth = req.depth;

    // #665: この run を貫く相関 ID と span。全 gateway（Discord/Nostr/web/時刻発火）がこの
    // 単一チョークポイントを通るので、ここで採番すればターン内の LLM/ツール往復（engine 内の debug）が
    // 同じ turn_id で束ねられ、llm_logs の行とも突き合わせられる。span は下の engine 実行 future に
    // `.instrument` して engine 側の各 debug 行へ agent_id / session_id / turn_id を継承させる（純可視化・
    // 制御には使わない）。run_agent_response 自身の行は await を跨ぐ span enter を避けて明示フィールドで出す。
    let turn_id = opencrab_actions::new_turn_id();
    let turn_span = tracing::info_span!(
        "turn",
        agent_id = %agent_id,
        session_id = %session_id,
        transport = %gateway,
        depth,
        turn_id = %turn_id,
    );

    // #632: 存在しないエージェントではターンを起こさない（サーバ側の単一チョークポイント）。
    // 以降の workspace 作成・LLM 実行・ツール実行の手前で弾く。行が無いと per-agent 設定が
    // 全部既定に落ちるのに動いてしまい、タイプミスに気づけない。
    {
        let conn = state.db.lock().unwrap();
        if opencrab_db::queries::get_agent(&conn, agent_id)?.is_none() {
            return Err(AgentNotFound {
                agent_id: agent_id.to_string(),
            }
            .into());
        }
    }

    // #665: ターン実行の入り。ここから下の文脈準備 → engine 実行までを 1 本のターンとして追う。
    tracing::debug!(
        agent_id = %agent_id,
        session_id = %session_id,
        transport = %gateway,
        depth,
        turn_id = %turn_id,
        stage = "run",
        "turn: ターン実行 開始（入）"
    );
    // #665: 「終了」ログを**構造的に必ず**出す Drop ガード。以降の setup 段には workspace 解決などの
    // `?` early-return が挟まる。末尾 1 箇所だと `?` や panic で抜けたとき終了ログが出ず、「入って止まった」と
    // 「エラーで抜けた」が区別できない。スコープ離脱で必ず 1 行出す。`outcome` は正常経路で結果に応じて
    // 上書きし、既定は "aborted"（終了ログ到達前の `?`/panic で抜けた）。純可視化・制御フローには影響しない。
    let mut turn_end = TurnEndLog {
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        turn_id: turn_id.clone(),
        outcome: "aborted",
    };

    // Build workspace path for this agent.
    let ws_path =
        opencrab_core::workspace::resolve_agent_workspace(&state.workspace_base, agent_id)?;
    std::fs::create_dir_all(&ws_path).ok();
    let workspace = opencrab_core::workspace::Workspace::from_root(std::path::Path::new(&ws_path))?;

    let effective_model = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::effective_model_for_agent(&conn, agent_id, &state.default_model)
            .unwrap_or_else(|_| state.default_model.clone())
    };
    // per-agent の推論（thinking）強度。空/未設定なら None（プロバイダー既定に従う）。
    let agent_reasoning_effort = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::effective_reasoning_effort_for_agent(&conn, agent_id).unwrap_or(None)
    };
    // per-agent の本文URL読取り（provider native web_search / url_context）。既定は無効。
    let agent_web_search = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::web_search_enabled_for_agent(&conn, agent_id).unwrap_or(false)
    };

    // Create BridgedExecutor with ActionContext.
    let last_metrics_id = Arc::new(std::sync::Mutex::new(None));
    let model_override = Arc::new(std::sync::Mutex::new(None));
    // depth >= 1 の再入実行は sub-engine（`spawn_subtask` が起動したサブタスク）。
    // メトリクスの purpose ラベルは旧 Discord 実装（`execute_spawn_subtask`）と同じく
    // "subtask" にする（#175 S4）。
    let current_purpose = Arc::new(std::sync::Mutex::new(
        if depth == 0 {
            "conversation"
        } else {
            "subtask"
        }
        .to_string(),
    ));

    let runtime_info = opencrab_actions::RuntimeInfo {
        default_model: state.default_model.clone(),
        active_model: Some(effective_model.clone()),
        available_providers: state
            .llm_router
            .get()
            .provider_names()
            .into_iter()
            .map(String::from)
            .collect(),
        gateway: gateway.to_string(),
    };

    // dispatch した subtask にも同じ呼び出し元を載せる（#298）ので、ここでは複製する。
    let run_caller = req.caller.clone();
    let ctx = opencrab_actions::ActionContext {
        caller: req.caller,
        agent_id: agent_id.to_string(),
        agent_name: agent_name.to_string(),
        session_id: Some(session_id.to_string()),
        db: state.db.clone(),
        workspace: Arc::new(workspace),
        last_metrics_id: last_metrics_id.clone(),
        model_override: model_override.clone(),
        current_purpose: current_purpose.clone(),
        runtime_info: Arc::new(std::sync::Mutex::new(runtime_info)),
    };
    // 走行中 subtask の共有 registry を **1 度だけ**解決する。`SystemGatewayActions`
    // （cancel_subtask / report_progress）と自動 dispatcher、そして `spawn_subtask`
    // （#175 S4）が同一 Arc を見ることで「停止の到達性」が保たれる。呼び出し側が
    // registry を渡さなかった場合も、この run 内では全員が同じフレッシュな registry を
    // 共有する（以前は dispatcher だけがフレッシュ生成し、cancel が not found になった）。
    let subtask_registry: opencrab_actions::SubtaskRegistry = req
        .subtask_registry
        .clone()
        .unwrap_or_else(|| std::sync::Arc::new(dashmap::DashMap::new()));

    let executor = build_turn_executor(
        state,
        TurnExecutorWiring {
            context: ctx,
            depth,
            gateway_actions: req.gateway_actions.clone(),
            subtask_registry: subtask_registry.clone(),
            completion_sink: req.completion_sink.clone(),
            subtask_starts: req.subtask_starts.clone(),
            reply_target: req.reply_target.clone(),
            tool_allowlist: req.tool_allowlist.clone(),
        },
        |caller_is_trusted| {
            state.mcp_manager.as_ref().map(|manager| {
                Arc::new(manager.provider_for(agent_id, caller_is_trusted))
                    as Arc<dyn opencrab_gateway::GatewayActions>
            })
        },
    );
    // ツール/コマンド活動を webhook へ実況する sink を挿す。
    //
    // - サブタスク走行（`run_notifier` あり）は、その run 専用の配送ワーカーを共有する
    //   sink を通知実装から受け取る（lifecycle と tool_call_* の順序が 1 本の worker で
    //   保たれる）。
    // - それ以外（depth0 / メインターン）は activity family のデフォルト宛先から
    //   factory で組む。activity 行が無ければ factory は None を返し、配送 worker も
    //   起動しない（best-effort）。無効/不正なデフォルトは sink 側で診断を残し、黙って
    //   fall through しない。
    let run_notifier = req.run_notifier.clone();
    let notifier_tool_sink = run_notifier.as_ref().and_then(|n| n.tool_event_sink());
    #[cfg(feature = "discord")]
    let tool_event_sink = notifier_tool_sink
        .or_else(|| opencrab_discord::spawn_activity_tool_event_sink(state.db.clone(), agent_id));
    #[cfg(not(feature = "discord"))]
    let tool_event_sink = notifier_tool_sink;
    let executor = match tool_event_sink {
        Some(sink) => executor.with_tool_event_sink(sink),
        None => executor,
    };

    // §2.7 案B: 「## More tools」静的 index を effective − 投影 から導出し system prompt へ後付け。
    // executor が具体型（`BridgedExecutor`）のうちに計算する（この直後に Arc<dyn> へ包む）。
    // lane（Nostr）・owner-only は effective に現れるかで自動的に出し分き、build_agent_context の
    // 契約は変えない。
    let more_tools_index = build_more_tools_index(&executor);
    let system_prompt_owned;
    let system_prompt = if more_tools_index.is_empty() {
        system_prompt
    } else {
        system_prompt_owned = format!("{system_prompt}{more_tools_index}");
        system_prompt_owned.as_str()
    };

    // Create LlmRouterAdapter with metrics recording.
    let metrics_ctx = MetricsContext {
        db: state.db.clone(),
        agent_id: agent_id.to_string(),
        session_id: Some(session_id.to_string()),
        pricing: PricingRegistry::default(),
        last_metrics_id: last_metrics_id.clone(),
        current_purpose: current_purpose.clone(),
    };
    let llm_client = LlmRouterAdapter::new(state.llm_router.clone())
        .with_metrics(metrics_ctx)
        .with_agent_id(agent_id);

    // Main engine: 30 iterations max. Sub-engines: unlimited (timeout-controlled).
    let max_iterations = if depth == 0 { 30 } else { usize::MAX };

    // tool_result の退避先（サイズ上限超過分）。inline 経路のログ callback と
    // dispatch 経路（`SubtaskToolDispatcher`）が同じ root を使う。
    let tool_result_workspace =
        opencrab_core::workspace::resolve_agent_workspace(&state.workspace_base, agent_id)?;

    // 合成 executor を 1 つの Arc にまとめ、engine（inline 実行）と dispatcher
    // （background 実行 = RFC #152 S3a 非ブロック）で共有する。dispatch した単一
    // ツールは同じ合成 executor（SystemGatewayActions を含む＝nostr_generate_key
    // 等 server ツール到達可）で実行される（S2 到達性の実経路化）。
    let executor: std::sync::Arc<dyn opencrab_core::ActionExecutor> = std::sync::Arc::new(executor);
    let mut engine = opencrab_core::SkillEngine::new(
        Box::new(llm_client),
        Box::new(opencrab_actions::SharedExecutor(executor.clone())),
        max_iterations,
    );

    // #676（案Y）: 送るプロバイダのモデルは、出力上限（max_output_tokens）を model_pricing から
    // 実能力値で解決して engine に渡す。未登録（NULL / 0 以下 / 行なし）なら fail loud で
    // ターンを止める（グローバルな任意定数を既定に置かない）。「送るか」はプロバイダの能力宣言
    // （router 経由・core で名前突き合わせしない）。送らないプロバイダ（chatgpt/codex/cursor/acp）
    // は解決も要求もせず、engine は上限未指定のまま＝プロバイダ内部既定に委ねる（切り捨ては
    // 方針3の incomplete→Length→bail が担う）。解決は effective_model（ターン単位）で行う——
    // context_window 予算計算と同じ流儀・同じ粒度（select_llm の per-iteration 上書きは追わない）。
    if state
        .llm_router
        .get()
        .sends_max_output_tokens(&effective_model)
    {
        let max_out = {
            let conn = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("db lock failed: {e}"))?;
            resolve_model_max_output_tokens(&conn, &effective_model).map_err(anyhow::Error::msg)?
        };
        engine.set_max_output_tokens(max_out);
    }

    // #284: LLM へ返す tool_result のサイズ上限と退避先。engine 側で上限を効かせ、
    // 全文はワークスペースへ残す（エージェントが read_file で続きを読める）。
    // 退避先は inline のログ callback / dispatch 経路と**同じ root**を使う。
    engine.set_tool_result_offload(session_id.to_string(), Some(tool_result_workspace.clone()));

    // #289: 走行中のターンにも新着ユーザー発言を届ける。
    //
    // `conversation` は呼び出し側がこの関数に入る**前**に組んでおり、以後ターン内では
    // 組み直さない。ツール往復が長引くとその間の発言が次ターンまで見えず、「やめて」の
    // ような緊急の指示ほど効かなかった（#289 のエビデンス）。ここで注入口を挿し、
    // ツール往復のたびに差分だけを入力へ足す。
    //
    // watermark をここ（会話構築の**後**）で取ることで、履歴に載っている発言を二重に
    // 見せない。届けるだけで応答は強制しない（#288 の強制は撤回済み）。
    //
    // depth 0 限定。サブタスク（depth>0）は背景処理であって対話の当事者ではなく、
    // 親ターンが同じ発言を注入する以上、こちらにも足すと同じ発言が二重に流れる。
    if depth == 0 {
        // #323 / B2: Nostr は 1 セッションに全相手が同居するため、走行中注入を返信中の
        // 相手（inbound=`OnlySpeaker` / resume=`Silent`）に絞る。他ゲートウェイは既定
        // （`AllOthers`）のままで挙動は変わらない。
        engine.set_live_inbound(std::sync::Arc::new(
            SessionLiveInbound::new(state.db.clone(), session_id, agent_id)
                .with_scope(req.live_inbound_scope.clone()),
        ));
    } else {
        // #647: サブタスク（depth>0）は走行中ユーザー発話の当事者ではないが、親/オーナーからの
        // steer（追加指示）は反復の合間に読む。ユーザー発話版と同じ `LiveInboundSource` 機構を
        // steer 専用ソースで通す。sub-session（`subtask-{id}` = ここでの `session_id`）に積まれた
        // `log_type='steer'` の行だけを差分注入する。auto-dispatch はこの経路（`run_agent_response`）
        // を通らないので steer 注入口も持たない＝`steer_subtask` 側が `NotSteerable` を返す。
        engine.set_live_inbound(std::sync::Arc::new(SubtaskSteerInbound::new(
            state.db.clone(),
            session_id,
        )));
    }

    // 自動 dispatch（非ブロック）フックの注入。depth0 かつ完了再注入 sink が配線
    // されているときだけ有効化する。sink 未配線（REST 一発呼び等）や sub-engine は
    // 従来どおり全ツール inline 実行（後方互換・非破壊）。
    if depth == 0 && state.subtask_auto_dispatch {
        if let Some(sink) = req.completion_sink.clone() {
            let registry = subtask_registry.clone();
            // inbound の返信先（gateway 不透明 token / #167）を dispatcher へ渡す。
            // dispatch した subtask の `SpawnedSubtask.reply_target` に載り、settle 時に
            // sink へ届く（session_id から返信先を復元できない gateway 用）。
            let dispatcher = opencrab_actions::SubtaskToolDispatcher::new(
                executor.clone(),
                registry,
                state.db.clone(),
                sink,
                agent_id.to_string(),
                session_id.to_string(),
            )
            .with_reply_target(req.reply_target.clone())
            // このターンの呼び出し元を dispatch した subtask へ引き継ぐ（#298）。
            // 決着で親会話を resume する sink が、元の権限のまま再開できる
            // （落とすと owner/trusted のツールが resume 後に丸ごと消える）。
            .with_caller(run_caller.clone())
            // 大きい tool_result は inline 経路と同様にワークスペースへ退避する
            // （DB へ無制限に入れると resume 時の会話再構築が context 予算を溢れる）。
            .with_workspace_root(Some(tool_result_workspace.clone()))
            // #431: auto-dispatch の起動を親ターンのカウンタへ載せる。上の
            // `SystemGatewayActions`（明示 spawn_subtask）へ渡すのと同一 Arc。
            .with_subtask_starts(req.subtask_starts.clone());
            engine.set_tool_dispatcher(std::sync::Arc::new(dispatcher));
        }
    }

    if let Some(notifier) = run_notifier {
        set_run_notifier_callbacks(&mut engine, &notifier, session_id.to_string());
    }

    // per-agent の thinking 強度を各 ChatRequest に付与（プロバイダーが per-request で優先）。
    if let Some(effort) = &agent_reasoning_effort {
        engine.set_reasoning_effort(effort.clone());
    }
    // 本文URL読取り（オプトイン）。対応プロバイダだけがツールを有効化し、他は無視する。
    if agent_web_search {
        engine.set_web_search(true);
    }

    set_llm_log_callback(
        &mut engine,
        state.db.clone(),
        agent_id.to_string(),
        session_id.to_string(),
        req.trigger_message_id.clone(),
    );

    // Set optional response-text callback (for immediate Discord acknowledgment).
    if let Some(cb) = req.on_response_text {
        engine.set_on_response_text(move |text: String| cb(text));
    }

    // #898: 継続分岐（末尾 CONTINUE の text-only イテレーション）の途中発話フックを転記する。
    // core / actions で型は構造一致（配送・保存を await し、失敗は継続を止める）。
    if let Some(cb) = req.on_continuation_speech {
        engine.set_on_continuation_speech(cb);
    }

    // #930: 走行中に畳み込んだ said の origin を read state（👀）として通知するフックを転記する。
    if let Some(cb) = req.on_read_origin {
        engine.set_on_folded_origin(cb);
    }

    // sleep のメンテナンスラン（#393）はここを配線しない = 生ログ（`memory_sessions`）に
    // 1 行も書かない。整備作業のターンは本人の体験ではなく、記録すると次の宣言ランが
    // 「記憶を整理した」という記憶を作り始める。
    //
    // **落ちるのは `memory_sessions` への書き込みだけ。何を行ったかの運用記録は残る**（#393）:
    // - `llm_logs`: 上の `set_llm_log_callback` は**この分岐の外**で無条件に配線され、engine の
    //   別フック（`set_log_callback`）に載る。LLM コールごとに ChatRequest 全体（＝累積した
    //   messages。ツール結果も含む）と応答・`tool_calls`・トークン数・レイテンシを記録する。
    //   engine 側は `messages.push(...)` を on_tool_call / on_tool_result より**先**に行うので、
    //   ここを配線しなくても累積内容は 1 バイトも変わらない。
    // - `agent_logs`: 各ランが自分で `insert_agent_log`（context="sleep"）する。この関数を通らない。
    //
    // LLM が見る文脈も変わらない（巨大結果の退避は上の `set_tool_result_offload` が担当していて、
    // この callback は永続化専用）。
    if req.persist_turn_logs {
        // gateway 宣言 DI operation の名前（RunRequest.gateway_actions 由来・runtime・core に
        // platform 語彙なし）。これらの tool_call は arguments を会話へ verbatim 保持する。
        let di_op_names: std::collections::HashSet<String> = req
            .gateway_actions
            .as_ref()
            .map(|ga| ga.definitions().into_iter().map(|d| d.name).collect())
            .unwrap_or_default();
        set_turn_log_callbacks(
            &mut engine,
            state.db.clone(),
            agent_id.to_string(),
            session_id.to_string(),
            tool_result_workspace,
            di_op_names,
        );
    }

    let merged_image_urls = merge_image_urls(state, session_id, agent_id, &req.image_urls);

    // ループ再起動 v1（#52）: depth 0 の run が反復上限（stopped_by_limit）で停止し、
    // セッションに active タスクが残っている場合、restart_count 上限まで（v1 では 1 回）
    // クリーンな context でエンジンを再実行する。会話は再構築するため、run-1 中に
    // session_logs へ記録されたトレース + 下で記録する [restart] decision エントリ
    // （台帳 prompt section 経由）が run-2 に見える。
    // 注意: 呼び出し元（message_loop）のセッションロックは run1 + run2 の全期間
    // 保持される。既定無効（agent.loop_restart_enabled）。
    let mut conversation_override: Option<String> = None;
    let mut restarts_this_call: i64 = 0;
    #[allow(unused_assignments)]
    let mut last_waters: Option<(usize, usize)> = None;
    let result = loop {
        // #665: engine（LLM ループ本体）へ入る。文脈構築はここまでに終わっており、この後は LLM 呼び出しと
        // ツール往復。engine 内の debug 行は下の `.instrument(turn_span)` で turn_id 等を継承する。
        tracing::debug!(
            agent_id = %agent_id,
            session_id = %session_id,
            turn_id = %turn_id,
            restart = restarts_this_call,
            stage = "engine",
            "turn: エンジン実行 開始（入）"
        );
        {
            let tools = executor.list_tools();
            let conn = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("db lock poisoned: {e}"))?;
            let conversation_text = conversation_override.as_deref().unwrap_or(conversation);
            let runtime_text =
                opencrab_core::runtime_context::runtime_context_prefix(conversation_text);
            match ensure_request_functions_budget(
                RequestEnvelopeArgs {
                    conn: &conn,
                    agent_id,
                    session_id,
                    default_model: &state.default_model,
                    policy: &state.context_budget_policy(),
                    system_prompt,
                    runtime_context_text: runtime_text,
                    functions_tokens: 0,
                    entrypoint: "run_agent_response",
                },
                &tools,
            ) {
                Ok(env) => {
                    engine.set_conversation_waters(env.conversation_high, env.conversation_low);
                    last_waters = Some((env.conversation_high, env.conversation_low));
                    // #884 PR2: typed history flag が有効なら typed 会話を組んで差し込む。
                    // 失敗時は flat へフォールバック（None）。
                    if state.typed_history_enabled {
                        match opencrab_core::conversation_typed::build_typed_conversation(
                            &conn,
                            session_id,
                            agent_id,
                            env.conversation_high,
                            env.conversation_low,
                            include_memory_index(&env),
                            !state.typed_history_drop_directive,
                        ) {
                            Ok(tc)
                                if typed_exceeds_input_budget(
                                    tc.wire_tokens,
                                    env.water.input_high,
                                ) =>
                            {
                                // #884 PR2 hard cap: PR2 は typed 側を圧縮しないため、typed の wire
                                // トークンがモデルの入力上限（input_high）を超えると provider が
                                // hard-fail する。その turn だけ flat 経路（圧縮あり）へ落とす（§7 fallback）。
                                tracing::warn!(
                                    session_id,
                                    wire_tokens = tc.wire_tokens,
                                    input_high = env.water.input_high,
                                    "typed wire tokens exceed model input budget; falling back to flat for this turn"
                                );
                                engine.set_typed_conversation(None);
                            }
                            Ok(tc) => {
                                tracing::debug!(
                                    session_id,
                                    wire_tokens = tc.wire_tokens,
                                    items = tc.diagnostics.item_count,
                                    unpaired = tc.diagnostics.unpaired_call_count,
                                    opaque = tc.diagnostics.opaque_event_count,
                                    "typed history enabled: sending typed conversation"
                                );
                                engine.set_typed_conversation(Some(tc));
                            }
                            Err(e) => {
                                tracing::warn!(session_id, %e, "typed conversation build failed; falling back to flat");
                                engine.set_typed_conversation(None);
                            }
                        }
                    } else {
                        engine.set_typed_conversation(None);
                    }
                }
                Err(e) => return Err(anyhow::anyhow!("{e}")),
            }
        }
        let result = engine
            .run_with_model_override(
                system_prompt,
                conversation_override.as_deref().unwrap_or(conversation),
                &effective_model,
                Some(model_override.clone()),
                &merged_image_urls,
            )
            .instrument(turn_span.clone())
            .await;
        // #665: engine から戻った。入と対で出す。結果種別（成否・iterations・tool_calls・打ち切り）を載せ、
        // 宙吊りが engine の中か外かをこの行の有無で切り分けられるようにする。
        match &result {
            Ok(r) => tracing::debug!(
                agent_id = %agent_id,
                session_id = %session_id,
                turn_id = %turn_id,
                iterations = r.iterations,
                tool_calls = r.tool_calls_made,
                stopped_by_limit = r.stopped_by_limit,
                stage = "engine",
                "turn: エンジン実行 完了（出）"
            ),
            Err(e) => tracing::debug!(
                agent_id = %agent_id,
                session_id = %session_id,
                turn_id = %turn_id,
                error = %e,
                stage = "engine",
                "turn: エンジン実行 失敗（出）"
            ),
        }

        // harness 剪定メトリクス: XML <function_calls> フォールバックの発火を agent_logs に
        // 記録する（context='harness.xml_fallback'）。「最後に発火したのはいつか・どのモデルか」を
        // DB で照会でき、足場の消し時を判断できる。docs/harness-inventory.md 参照。
        // 注: codex プロバイダはこのフォールバックに意図的に依存しているため、発火自体は異常ではない。
        if let Ok(ref engine_result) = result {
            if engine_result.xml_fallback_parses > 0 {
                // run 中に set_model で切り替わっている可能性があるため、override の現在値を優先する
                // （イテレーション単位の正確なモデルはエンジンの debug ログ側にある）。
                let fired_model = model_override
                    .lock()
                    .ok()
                    .and_then(|g| g.clone())
                    .unwrap_or_else(|| effective_model.clone());
                crate::agent_log::agent_log(
                    &state.db,
                    Some(agent_id),
                    crate::agent_log::LogLevel::Info,
                    "harness.xml_fallback",
                    &format!(
                        "XML <function_calls> fallback fired {} time(s) (model: {fired_model})",
                        engine_result.xml_fallback_parses
                    ),
                );
            }
        }

        // 再起動判定。継続しないケースは全て result を返して抜ける。
        match prepare_loop_restart(
            state,
            agent_id,
            session_id,
            depth,
            restarts_this_call,
            req.trigger_message_id.as_deref(),
            &result,
        ) {
            Some(conversation) => {
                restarts_this_call += 1;
                conversation_override = Some(conversation);
            }
            None => break result,
        }
    };

    // 記憶インデックスの背景ビルドとスキル利用回数は depth 0（メインターン）のみ。
    // sub-engine の内部 run では走らせない（旧 `execute_spawn_subtask` の sub-engine は
    // どちらも持たなかった。サブタスクごとに LLM 支出が増えるのを避ける）。
    if depth == 0 {
        if let Some((high, low)) = last_waters {
            spawn_background_turn_end_snapshot(state, session_id, agent_id, high, low);
        }
        spawn_background_index_build(state, agent_id, &effective_model);
        if let Ok(ref engine_result) = result {
            record_used_skills(state, agent_id, session_id, &engine_result.response);
        }
    }

    // #665: engine まで到達した正常経路では結果に応じて outcome を上書きする。実際の「終了」ログは
    // `turn_end` の Drop が出す（正常/エラー/早期 return いずれの経路でも 1 行出る）。これが出ていて
    // 上位（gateway の配送・記録）の行が続かなければ、詰まりは run_agent_response より外（返信送信・
    // 転記）側にある、という切り分けができる。
    turn_end.outcome = if result.is_ok() { "ok" } else { "engine_error" };

    result
}

/// #665: ターン実行の「終了」ログを**構造的に必ず**出すための Drop ガード。
///
/// [`run_agent_response`] は末尾に到達する前に setup 段の `?`（workspace 解決など）で early-return
/// し得る。「終了」を関数末尾 1 箇所で出すと、その `?` や panic で抜けたときログが出ず、「入って
/// 止まった」と「エラーで抜けた」が区別できない（この計装の目的が壊れる）。スコープ離脱で必ず 1 行
/// 出すことで、最後の 1 行が常に真を語る。**純可視化・制御フローには一切影響しない**（Drop は
/// 戻り値を変えない）。`outcome` は正常経路で `ok` / `engine_error` に上書きし、既定は `aborted`
/// （終了到達前の `?`/panic）。
struct TurnEndLog {
    agent_id: String,
    session_id: String,
    turn_id: String,
    outcome: &'static str,
}

impl Drop for TurnEndLog {
    fn drop(&mut self) {
        tracing::debug!(
            agent_id = %self.agent_id,
            session_id = %self.session_id,
            turn_id = %self.turn_id,
            outcome = self.outcome,
            stage = "run",
            "turn: ターン実行 終了（出）"
        );
    }
}
