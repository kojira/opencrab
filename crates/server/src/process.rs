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

/// この run で使う実行許可設定（`ToolsConfig`）を解決する。
///
/// グローバル設定（`AppState.tools_config`）の**複製**に、そのエージェントの DB 上の
/// 許可コマンドをマージして返す。グローバル側は決して書き換えない — 混ぜると
/// 全エージェントの許可が合流し、あるエージェントの許可が他へ漏れる（#202）。
/// 同じ方針は起動時の設定構築（`crate::main`）と REST の許可コマンド管理
/// （`crate::api::allowed_commands`）にも明文化されている。
///
/// **この関数が毎 run 無条件に呼ばれることが、許可コマンドツールがグローバル設定へ
/// 書き込む必要が無い理由**である（`crate::agent_management`）。DB が信頼できる
/// 情報源で、次の run はここで必ず拾い直す。
pub(crate) fn resolve_run_tools_config(
    state: &AppState,
    agent_id: &str,
) -> opencrab_actions::tools::ToolsConfig {
    let mut tools_cfg = state.tools_config.read().unwrap().clone();
    // DB 障害時は設定ファイル分だけで続行する（許可を落とす方向の degrade なので
    // run を止めるより安全）。ただし**黙って**落とさない: per-agent の許可が丸ごと
    // 消えたことは、ログが無いと誰も気づけない。本番の `Db` は r2d2 プールなので
    // 枯渇時の `lock()` は既定で 30 秒ブロックしてから `Err` になり得る。
    match state.db.lock() {
        Ok(conn) => match opencrab_db::queries::list_agent_allowed_commands(&conn, agent_id) {
            Ok(agent_cmds) => {
                if !agent_cmds.is_empty() {
                    let shell = tools_cfg
                        .shell
                        .get_or_insert_with(opencrab_actions::tools::ShellToolConfig::default);
                    for cmd in agent_cmds {
                        if !shell.allowed_commands.contains(&cmd) {
                            shell.allowed_commands.push(cmd);
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(
                agent_id,
                error = %e,
                "per-agent 許可コマンドの取得に失敗。設定ファイル由来の許可だけで続行する\
                 （この run では追加分が効かない）"
            ),
        },
        Err(e) => tracing::warn!(
            agent_id,
            error = %e,
            "DB 接続の取得に失敗。設定ファイル由来の許可だけで続行する\
             （この run では per-agent の追加分が効かない）"
        ),
    }
    tools_cfg
}

/// Production の turn executor を組み立てるための wiring。
///
/// ここにある値は transport/run ごとの依存物だけで、caller/depth gate、system gateway
/// 合成、shell 登録、allowlist、MCP の depth-0 制限は [`build_turn_executor`] が決める。
pub(crate) struct TurnExecutorWiring {
    pub context: opencrab_actions::ActionContext,
    pub depth: u32,
    pub gateway_actions: Option<Arc<dyn opencrab_gateway::GatewayActions>>,
    pub subtask_registry: opencrab_actions::SubtaskRegistry,
    pub completion_sink: Option<Arc<dyn opencrab_actions::SubtaskCompletionSink>>,
    pub subtask_starts: Option<Arc<std::sync::atomic::AtomicUsize>>,
    pub reply_target: Option<String>,
    pub tool_allowlist: Option<Vec<String>>,
}

/// Production と外形採取が共有する turn executor の唯一の組立口。
///
/// `mcp_provider` は接続済み provider の供給だけを抽象化する。信頼判定と depth-0 限定は
/// この関数内に残るため、呼び出し側が production と異なる注入条件を再実装できない。
pub(crate) fn build_turn_executor<F>(
    state: &AppState,
    wiring: TurnExecutorWiring,
    mcp_provider: F,
) -> opencrab_actions::BridgedExecutor
where
    F: FnOnce(bool) -> Option<Arc<dyn opencrab_gateway::GatewayActions>>,
{
    let mut dispatcher = opencrab_actions::ActionDispatcher::new();
    let tools_config = resolve_run_tools_config(state, &wiring.context.agent_id);
    opencrab_actions::register_tools_from_config(&tools_config, &mut dispatcher);

    let caller_is_trusted = matches!(
        wiring.context.caller,
        opencrab_actions::CallerIdentity::Owner
            | opencrab_actions::CallerIdentity::CoAgent { .. }
            | opencrab_actions::CallerIdentity::TrustedUser
    );
    let system_actions: Arc<dyn opencrab_gateway::GatewayActions> = Arc::new(
        crate::system_actions::SystemGatewayActions::new(
            state.clone(),
            wiring.gateway_actions,
            Some(wiring.subtask_registry),
            wiring.completion_sink,
        )
        .with_subtask_starts(wiring.subtask_starts),
    );
    let gateway_actions: Arc<dyn opencrab_gateway::GatewayActions> = if wiring.depth == 0 {
        system_actions
    } else {
        Arc::new(opencrab_actions::SubEngineGatewayActions::new(
            system_actions,
        ))
    };
    let bridged = opencrab_actions::BridgedExecutor::new(dispatcher, wiring.context)
        .with_depth(wiring.depth)
        .with_reply_target(wiring.reply_target)
        .with_tool_allowlist(wiring.tool_allowlist)
        .with_gateway_actions(gateway_actions);

    if wiring.depth == 0 {
        match mcp_provider(caller_is_trusted) {
            Some(provider) => bridged.with_mcp_actions(provider),
            None => bridged,
        }
    } else {
        bridged
    }
}

/// そのエージェントが `execute_shell` で**実際に実行できる**コマンド名の一覧（#300）。
///
/// [`resolve_run_tools_config`] の結果に `ShellToolConfig::effective_commands()` を
/// 掛けただけのもの。つまり LLM へ渡る `execute_shell` の `Allowed: ...` と
/// **同じ経路・同じ順序・同じ重複排除**で作られる。
///
/// 許可コマンド一覧ツール（`crate::agent_management::list_allowed_commands` /
/// `SystemGatewayActions::manage_allowed_commands` の `action="list"`）は
/// **必ずこの関数を通す**こと。ここを迂回して DB 行だけを返していたのが #300 の不具合で、
/// エージェントが「シェルは DB 行の 2 個しか使えない」と誤認して作業を止めた
/// （実際には設定ファイル由来の 10 個も同じターンで実行できていた）。
/// 出力先ごとに合成を書き直すと、片方だけ実態からずれて同じ誤認が再発する。
///
/// 一方、ダッシュボードの REST（`crate::api::allowed_commands::list_allowed_commands`）は
/// **この関数を通さない**。線引きは「LLM に露出する口 = 実効リスト / HTTP 管理 API =
/// DB 行」で、後者は add/remove と対の管理用ゆえに消せない行を混ぜてはならない。
///
/// DB 読み取りに失敗した場合は [`resolve_run_tools_config`] と同様に設定ファイル分だけを
/// 返す。**これは意図的**で、その run が実際に許可する集合と一致させるためである
/// （一覧だけがエラーを返しても、実行側は設定ファイル分を許可したまま動く）。
pub(crate) fn effective_allowed_commands(state: &AppState, agent_id: &str) -> Vec<String> {
    let tools_cfg = resolve_run_tools_config(state, agent_id);
    // 実際の run では `register_tools_from_config`（crates/actions/src/tools/mod.rs）が
    // `tools.enabled == false` なら **execute_shell 自体を登録しない**。ゲートを閉じた
    // 構成で許可コマンドを並べると「実行できないコマンド」を返し、エージェントが混乱する
    // （#311）。登録側と同じ条件でここも空へ倒す。register_tools_from_config が正で、
    // 一覧はそれに追従する。
    if !tools_cfg.enabled {
        return Vec::new();
    }
    tools_cfg
        .shell
        // 同様に shell 無し / `tools.shell.enabled == false` でも execute_shell は
        // 登録されない。`filter` で両方（None と enabled=false）を空へ落とす。
        .filter(|shell| shell.enabled)
        .map(|shell| {
            shell
                .effective_commands()
                .into_iter()
                .map(|c| c.name)
                .collect()
        })
        .unwrap_or_default()
}

/// sub-engine のツール呼び出しを進捗テキストへ要約する（#175 S4）。
///
/// 旧 Discord 実装（`execute_spawn_subtask`）から移設。`{function:{name}}`（正準）と
/// `{name}`（旧形状）の両方に対応し、assistant 本文は先頭 500 文字だけ添える。
fn summarize_tool_calls(assistant_content: &str, tool_calls_json: &str) -> String {
    let mut names = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(tool_calls_json) {
        if let Some(calls) = value.as_array() {
            for call in calls {
                let name = call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .or_else(|| call.get("name").and_then(|v| v.as_str()));
                if let Some(name) = name {
                    names.push(format!("`{name}`"));
                }
            }
        }
    }
    let tools = if names.is_empty() {
        "tool call".to_string()
    } else {
        names.join(", ")
    };
    let preview: String = assistant_content.trim().chars().take(500).collect();
    if preview.is_empty() {
        format!("calling {tools}")
    } else {
        format!("calling {tools}\n{preview}")
    }
}

/// DBからエージェントの agents 行と skills を読み込んでシステムプロンプトを構築する。
///
/// 返り値: (system_prompt, agent_name)
pub fn build_agent_context(
    conn: &rusqlite::Connection,
    agent_id: &str,
    caller: &opencrab_actions::CallerIdentity,
) -> (String, String) {
    let agent = opencrab_db::queries::get_agent(conn, agent_id)
        .ok()
        .flatten();
    let mut skills = opencrab_db::queries::list_skills(conn, agent_id, true).unwrap_or_default();
    // #352: caller=Agent のターン（素の Agent 権限で走る run。外部 Nostr の受信ターンが
    // 典型例だが、判定軸は transport ではなく caller=Agent）には、オーナーが露出を許可
    // （`agent_visible`）した skill だけを index に出す。既定 false なので、許可が無ければ
    // 1 件も残らず、下の `skills.is_empty()` 分岐で skill セクションごと出さない
    // （空の見出しは残さない）。Owner / CoAgent / TrustedUser は絞らない（従来どおり全部
    // 見える）。read_skill 側の本文ゲート（skill_management.rs）と AND で二重化する。名前を
    // 隠すだけでは read_skill を名前直打ちされるため index と本文の両方で絞る。
    if matches!(caller, opencrab_actions::CallerIdentity::Agent) {
        skills.retain(|s| s.agent_visible);
    }
    // curated 記憶は取り込みが **1 見出し 1 行**（`long_term/<見出し>`）で入れるため、
    // 完全一致で引くと `long_term/*` が 1 件も載らなかった（#428）。前方一致で素の
    // `long_term` と `long_term/<見出し>` の両方を拾い、見出しごとに束ねて注入する。
    // user_profile は単一の完全一致 1 行だけなので出力は従来どおり（見出しは付かない）。
    let curated_categories = ["long_term", "user_profile", "agent_rules"];
    let curated_sections: Vec<String> = curated_categories
        .iter()
        .filter_map(|cat| {
            let memories =
                opencrab_db::queries::get_curated_memories_by_prefix(conn, agent_id, cat)
                    .unwrap_or_default();
            if memories.is_empty() {
                return None;
            }
            // `<cat>/<見出し>` は `### <見出し>` を前置して 1 塊にする。素の `<cat>`
            // （接尾辞なし）は本文だけ。見出しが空の行は本文だけに倒す。
            let prefix = format!("{cat}/");
            let content = memories
                .iter()
                .map(|m| match m.category.strip_prefix(&prefix).map(str::trim) {
                    Some(heading) if !heading.is_empty() => {
                        format!("### {heading}\n{}", m.content)
                    }
                    _ => m.content.clone(),
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            let header = match *cat {
                "long_term" => "## Long-term Memory",
                "user_profile" => "## User Profile",
                "agent_rules" => "## Agent Rules",
                _ => return None,
            };
            Some(format!("\n\n{header}\n{content}"))
        })
        .collect();
    let curated_section = curated_sections.join("");

    let agent_name = agent
        .as_ref()
        .map(|a| a.name.clone())
        .unwrap_or_else(|| agent_id.to_string());

    let persona = agent
        .as_ref()
        .map(|a| a.persona_name.clone())
        .unwrap_or_default();

    let custom_traits = agent
        .as_ref()
        .and_then(|a| a.personality.clone())
        .unwrap_or_default();

    let instructions = agent
        .as_ref()
        .map(|a| a.instructions.clone())
        .unwrap_or_default();

    let peer_reviewers_text = peer_reviewers_section(conn, agent_id);

    let skills_text = if skills.is_empty() {
        String::new()
    } else {
        let list: Vec<String> = skills
            .iter()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect();
        // index（名前 + 説明）だけを載せ、本文は read_skill で必要時に掘り下げさせる（#119）。
        format!(
            "\n\nYour skills (index only — call read_skill(name) to get a skill's full body):\n{}",
            list.join("\n")
        )
    };

    let character_section = if custom_traits.is_empty() {
        String::new()
    } else {
        format!("\n\n{custom_traits}")
    };

    let instructions_section = if instructions.is_empty() {
        String::new()
    } else {
        format!("\n\n## Instructions\n{instructions}")
    };

    // Silent Reply は「相手が Bot か」でシステムが黙らせない。判断はエージェントへ委ね、
    // 沈黙は会話内容（完結した / 新しい情報が無い）で決めさせる（#486・理念: システムは
    // 相手が bot か判定しない）。ループ防止（元の意図）は種別ではなく内容の条件で残す。
    let prompt = format!(
        "You are {agent_name} ({persona}).\n\
         \n\
         You are an autonomous agent participating in a discussion. \
         Respond thoughtfully to the conversation. \
         You can use tools to search your history, learn from experience, \
         create new skills, and manage your workspace.\n\
         \n\
         The conversation history uses the format \"[speaker]: message\" for context, \
         but you must NOT include your own name prefix in your response. \
         Just reply with the message content directly.\n\
         \n\
         あなたは複数のアクションを順番に計画・実行できます。\
         例えば「Xを調べてYを設定する」という指示に対して、\
         1. execute_shell で情報収集、\
         2. 結果を解析、\
         3. add_allowed_command でコマンド追加、\
         4. create_my_skill でスキル作成、\
         のように、複数のアクションを連続して呼び出してください。\n\
         \n\
         ## Silent Reply\n\
         返答不要な場合は NO_REPLY とだけテキストで返してください（他のテキストと混在させない）:\n\
         - グループチャットで自分に関係ない会話の場合\n\
         - 既に話が完結している場合、または同じ話題の往復が続くだけで新しい情報を足せない場合\
         （相手が Bot でも人でも同じ基準で判断する。Bot だからという理由では黙らない）\n\
         ただし、{req_marker} で始まるメッセージにはレビュアーとして応答し、\
         自分が依頼したレビューへの {reply_marker} で始まる返信は記録・対応すること\
         （下記 Peer Review セクションに従う）。\n\
         \n\
         ## Async Behavior\n\
         \n\
         Query tools (execute_shell and the like — anything you call to fetch a result) run\n\
         asynchronously: when you call one, the result arrives later and you are called again\n\
         with it in the conversation history. Utterances (say / reply / reaction / repost) do\n\
         NOT work this way — see \"Continuing your turn\" below.\n\
         \n\
         Some tools return `{{status:\"spawned\", subtask_id: ...}}` immediately instead of a\n\
         final result. This means the work has started in the background and its result will\n\
         arrive later in a separate turn (as a `[subtask_completed: ...]` entry). When you\n\
         see a `spawned` result:\n\
         - Briefly tell the user you've started the task (do not claim it is finished)\n\
         - Do NOT call the same tool again for the same request — it is already running\n\
         - Do NOT invent or guess the result — wait for the actual completion turn\n\
         \n\
         When you see `[subtask_completed: ...]` in the conversation:\n\
         - It means a tool you called has finished, and it's your turn again\n\
         - Check what the result contains\n\
         - If there's more to do: continue with the next step\n\
         - If the task is done: summarize and reply to the user\n\
         - If no reply is needed: respond with NO_REPLY\n\
         \n\
         Do NOT repeat what you already said in the previous turn.\n\
         Do NOT re-explain what you're about to do if you already said it.\n\
         Just act on the result.\n\
         \n\
         Before responding after [subtask_completed: ...]:\n\
         1. Check your last message in the conversation history\n\
         2. If your last message already covers the same result → NO_REPLY\n\
         3. Only respond if you have genuinely NEW information to report\n\
         \n\
         ## Continuing your turn\n\
         \n\
         Utterances (say / reply / reaction / repost) are fire-and-forget: they do NOT\n\
         return a result and you are NOT called again for them. To post several messages,\n\
         put all the calls in ONE response (for example three `reply` calls) — never send\n\
         one and wait to be re-invoked for the next.\n\
         Plain text in a response is posted as ONE message.\n\
         To post several plain messages separately, end each response with `CONTINUE` and\n\
         continue in the next. After a response whose only actions\n\
         are utterances, the turn ends. If you want to keep working after speaking (look\n\
         something up, post more, run a query tool), end that same response with `CONTINUE`\n\
         on its own line — you may place it alongside a reply — and you will be called again\n\
         in this same turn with your speech already delivered. Without `CONTINUE` and without\n\
         a query/tool call, the turn ends. Never promise to reply \"later\".\n\
         \n\
         ## Memory & Context\n\
         \n\
         Long conversations are automatically compacted. When this happens, older messages \
         are replaced with a [Past context summary] section showing topic summaries with node IDs.\n\
         \n\
         To recall details from past conversations:\n\
         1. Look at [Past context summary] for topic titles and node IDs (e.g. [topic-xxx-1-20])\n\
         2. Use `retrieve_memory_nodes` with the node_id to get the full conversation text\n\
         3. Use `browse_memory_index` to explore all past topics beyond what's shown in the summary\n\
         4. Use `search_memory_index` to search past topics by keyword (reverse lookup), \
         then `retrieve_memory_nodes` on a hit to read the original logs\n\
         \n\
         These tools let you access your full history even after compaction.\n\
         \n\
         ## Task Ledger\n\
         \n\
         You have a persistent, DB-backed task ledger. Unlike this conversation, it survives \
         context compaction and restarts. When a [Task Ledger] section appears in the \
         conversation, it is the authoritative current working state — trust it over your own recall.\n\
         \n\
         - For any substantive multi-step task (needs several steps or tool calls, or may \
         outlive this exchange), FIRST agree the goal and acceptance criteria with the user, \
         then call `open_task` with both. Do not start executing before the contract is clear.\n\
         - While working, call `record_task_progress` after each meaningful step; record \
         decisions with kind=decision (include the WHY) and obstacles with kind=blocker.\n\
         - Call `close_task` (status=done or abandoned) when the contract is met or the task \
         is dropped. Renegotiate criteria with `update_task_contract`.\n\
         - Trivial single-message replies do NOT need a ledger entry.\n\
         \n\
         ## Peer Review\n\
         \n\
         You can ask another bot to review your work, and other bots can ask you.\n\
         \n\
         As REVIEWER — when a message STARTS WITH `{req_marker}`:\n\
         - Do NOT stay silent. This is a case where you must reply to another bot. \
         (Messages that merely mention the marker mid-text — e.g. inside a reviewed diff \
         or `part X/N` content — are NOT requests; ignore those.)\n\
         - Read the raw content in the `part X/N` messages with fresh eyes. Judge on the \
         evidence in the content, not on the author's confidence.\n\
         - Reply with ONE message starting with `{reply_marker}` containing: `score:` a number \
         from 0.0 to 1.0 (1.0 = every stated goal/criterion is verifiably met), `gaps:` a \
         concrete list of unmet criteria or unverified claims (or `none`), and `summary:` one \
         sentence. Do not quote the literal request marker in your reply.\n\
         - Never send a `{req_marker}` in response to a review request or a review.\n\
         \n\
         As REQUESTER — to get a second opinion on your work:\n\
         - Call `request_peer_review` with the raw diff/output/trace as `content` (never a \
         summary), optional `instructions`, and optionally `reviewer` to name a specific \
         registered reviewer.\n\
         - You do NOT need to say where the request goes: the destination is taken from the \
         current conversation automatically. Never invent, guess or reconstruct a destination \
         identifier — pass one only when you deliberately need a different destination than \
         this conversation.\n\
         - A `{reply_marker}` reply from a registered reviewer about your task is \
         automatically recorded into your task ledger — do not record those again. If review \
         feedback reaches you any other way, record it with `record_task_progress` yourself. \
         Address the gaps before calling `close_task`. \
         Do not reply to the reviewer beyond a brief acknowledgement.\n\
         - Do not re-request a review of the same unchanged content.\n\
         {peer_reviewers_text}\
         \n\
         {skills_text}{character_section}{instructions_section}{curated_section}",
        req_marker = opencrab_gateway::PEER_REVIEW_REQUEST_MARKER,
        reply_marker = opencrab_gateway::PEER_REVIEW_REPLY_MARKER,
    );

    (prompt, agent_name)
}

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

// 会話組み立て（[`opencrab_core::conversation`]）と文脈予算・モデル pricing ゲート
// （[`opencrab_core::context_budget`]）の実体は core へ移した（#518 手順 3〜4）。
// `build_ledger_section` / `build_impression_section` と同型（`conn` を取り会話用
// セクションを組む純粋ロジックで gateway/server の型に依存しない）ため下位層に置ける。
// 既存の呼び出し元（`process::build_conversation_string` 等）のパスを保つため再エクスポート
// する（doc に理由を明記した `subtask_registries` と同じ手）。
pub use opencrab_core::context_budget::{
    check_agent_model_change, compute_context_budget, ensure_functions_within_cap,
    ensure_model_context_window_registered, ensure_model_max_output_tokens_registered,
    ensure_startup_budget_inputs, measure_functions_tokens, model_context_window_missing_message,
    normalize_model_spec, resolve_agent_request_envelope, resolve_model_max_output_tokens,
    resolve_water_levels, split_llm_model_spec, ContextBudgetEnvelope, ContextBudgetError,
    ContextBudgetPolicy, MemoryIndexDecision, RequestEnvelopeArgs, DEFAULT_MEMORY_INDEX_TOKEN_CAP,
};
pub use opencrab_core::conversation::{
    build_conversation_string, build_conversation_string_with_memory_index,
    build_conversation_string_with_waters,
};

/// 入口共通: コア dispatcher の tool schema を 1 回測る。gateway / MCP は
/// [`ensure_request_functions_budget`] が実 `list_tools` で再検査する。
pub fn core_functions_tokens() -> Result<usize, ContextBudgetError> {
    let defs: Vec<opencrab_core::FunctionDefinition> = opencrab_actions::ActionDispatcher::new()
        .get_definitions(&[])
        .into_iter()
        .map(|d| opencrab_core::FunctionDefinition {
            name: d.name,
            description: if d.description.is_empty() {
                None
            } else {
                Some(d.description)
            },
            parameters: d.parameters,
        })
        .collect();
    measure_functions_tokens(&defs)
}

/// Memory Index を載せるか。判定は envelope 側だけが持つ。
pub fn include_memory_index(env: &ContextBudgetEnvelope) -> bool {
    matches!(env.memory_index_decision, MemoryIndexDecision::Inject)
}

/// #884 PR2 hard cap: typed 側は PR2 では圧縮しないため、typed の wire トークンがモデルの
/// 入力上限（`input_high`）を超えると provider が hard-fail する。超過なら typed を諦めて
/// flat 経路（圧縮あり）へ落とす（§7 fallback）。
pub(crate) fn typed_exceeds_input_budget(wire_tokens: usize, input_high: usize) -> bool {
    wire_tokens > input_high
}

/// ターン終了直後の正時: 派生スナップショットを行追加する（#826-B）。
fn persist_turn_end_snapshot(
    state: &AppState,
    session_id: &str,
    agent_id: &str,
    conversation_high: usize,
    conversation_low: usize,
) -> anyhow::Result<()> {
    let conn = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("db lock poisoned: {e}"))?;
    let assembled =
        opencrab_core::context_budget::assemble_from_snapshot(&conn, session_id, agent_id)?;
    let mut gov =
        opencrab_core::context_budget::TurnGovernor::new(conversation_high, conversation_low);
    gov.finish_turn(
        &conn,
        session_id,
        &assembled.items,
        assembled.through_log_id,
        &assembled.text,
    )?;
    Ok(())
}

/// 利用者の待ち時間に乗せない。失敗は応答をひっくり返さない（正時の失敗は次開始の超過検査で見える）。
fn spawn_background_turn_end_snapshot(
    state: &AppState,
    session_id: &str,
    agent_id: &str,
    conversation_high: usize,
    conversation_low: usize,
) {
    let state = state.clone();
    let session_id = session_id.to_string();
    let agent_id = agent_id.to_string();
    tokio::spawn(async move {
        if let Err(e) = persist_turn_end_snapshot(
            &state,
            &session_id,
            &agent_id,
            conversation_high,
            conversation_low,
        ) {
            tracing::error!(
                target: "context_budget_check",
                session_id = %session_id,
                error = %e,
                "turn-end snapshot persist failed"
            );
        }
    });
}

/// 各 request 前: 実 `list_tools` で functions cap と `fixed >= input_high` を検査する。
///
/// functions 超過も `apply_line_items` 経由にする（一意名 + 全費目 Display + 観測行）。
pub fn ensure_request_functions_budget(
    args: RequestEnvelopeArgs<'_>,
    tools: &[opencrab_core::FunctionDefinition],
) -> Result<ContextBudgetEnvelope, ContextBudgetError> {
    let functions_tokens = measure_functions_tokens(tools)?;
    resolve_agent_request_envelope(RequestEnvelopeArgs {
        functions_tokens,
        ..args
    })
}
// `format_single_log` は `format_live_inbound`（本番経路）が使うので常時取り込む。
pub(crate) use opencrab_core::conversation::format_single_log;
// 以下はテストだけが参照する（本番コードは使わない）。cfg(test) で本番ビルドの
// unused 警告を避ける。子モジュールのテストが `super::` で辿れる。
#[cfg(test)]
use opencrab_core::conversation::{past_summary_omitted_notice, RECENT_MIN_USER_SPEECHES};
/// 1 回の poll で注入する新着発言の上限（#289）。
///
/// 溢れた分は捨てない。watermark は返した行まで進むので、次のイテレーションで続きが
/// 拾われる。上限は「1 イテレーションでプロンプトが跳ねない」ための安全弁である。
const LIVE_INBOUND_POLL_LIMIT: usize = 20;

/// 走行中のターンへ新着ユーザー発言を届ける実体（#289）。
///
/// 会話履歴はターン開始時に 1 度だけ組まれるので、ツール往復が長引く間に届いた発言は
/// 次ターンまでエンジンから見えなかった。この実体はエンジンのループから毎イテレーション
/// 引かれ、**前回以降の差分だけ**を返す。
///
/// 重複注入の防止は `watermark`（取得済みの最大 log id）で行う。id は単調増加なので、
/// 一度返した行が再び返ることはない。初期値は**会話文字列を組んだ後**の最大 id
/// （＝この時点までの発言は履歴側に載っている）。
struct SessionLiveInbound {
    db: opencrab_db::Db,
    session_id: String,
    /// 応答するエージェント。`speaker_id != agent_id` が「自分以外の発言」の述語で、
    /// DB 側（`list_user_speech_logs_after`）と同じ比較をする（#286 の注意書き参照）。
    agent_id: String,
    /// 取得済みの最大 log id。これより後の行だけを次回返す。
    watermark: std::sync::atomic::AtomicI64,
    /// 注入の対象範囲（#323 / B2）。Nostr だけが相手を絞る（既定は全ての他者）。
    scope: opencrab_actions::LiveInboundScope,
}

impl SessionLiveInbound {
    /// 現在の最新 log id を watermark の初期値として組み立てる。
    ///
    /// 取得に失敗した場合は `i64::MAX` を置く（＝何も注入しない）。走行中の注入は
    /// あくまで改善であって、失敗しても既存のターンを壊さないことを優先する。
    fn new(db: opencrab_db::Db, session_id: &str, agent_id: &str) -> Self {
        let latest = match db.lock() {
            Ok(conn) => opencrab_db::queries::list_recent_session_logs(&conn, session_id, 1)
                .ok()
                .and_then(|rows| rows.first().and_then(|l| l.id))
                .unwrap_or(0),
            Err(e) => {
                tracing::warn!(session_id = %session_id, "live inbound watermark unavailable: {e}");
                i64::MAX
            }
        };
        Self {
            db,
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            watermark: std::sync::atomic::AtomicI64::new(latest),
            scope: opencrab_actions::LiveInboundScope::AllOthers,
        }
    }

    /// 注入の対象範囲を差し替える（#323 / B2）。既定（`AllOthers`）は Discord / heartbeat
    /// の従来挙動。Nostr は inbound で `OnlySpeaker`、resume で `Silent` を渡す。
    fn with_scope(mut self, scope: opencrab_actions::LiveInboundScope) -> Self {
        self.scope = scope;
        self
    }
}

impl opencrab_core::LiveInboundSource for SessionLiveInbound {
    fn poll_new_messages(&self) -> Vec<String> {
        use std::sync::atomic::Ordering;

        // 対象範囲（#323 / B2）。Silent は相手が不定なので DB を引くまでもなく空。
        let only_speaker = match &self.scope {
            opencrab_actions::LiveInboundScope::AllOthers => None,
            opencrab_actions::LiveInboundScope::OnlySpeaker(pk) => Some(pk.as_str()),
            opencrab_actions::LiveInboundScope::Silent => return Vec::new(),
        };

        let after_id = self.watermark.load(Ordering::Relaxed);
        let conn = match self.db.lock() {
            Ok(conn) => conn,
            // ロックが取れないだけでターンを落とさない（次のイテレーションで拾える）。
            Err(_) => return Vec::new(),
        };
        let rows = match opencrab_db::queries::list_user_speech_logs_after(
            &conn,
            &self.session_id,
            &self.agent_id,
            after_id,
            only_speaker,
            LIVE_INBOUND_POLL_LIMIT,
        ) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(session_id = %self.session_id, "live inbound poll failed: {e}");
                return Vec::new();
            }
        };
        drop(conn);

        if rows.is_empty() {
            return Vec::new();
        }
        // 返した行まで watermark を進める（＝同じ発言を二度注入しない）。
        if let Some(max_id) = rows.iter().filter_map(|r| r.id).max() {
            self.watermark.store(max_id, Ordering::Relaxed);
        }
        rows.iter().map(format_live_inbound).collect()
    }
}

/// 走行中に届いた発言を LLM へ見せる形に整える（#289）。
///
/// 本文の整形は履歴と同じ [`format_single_log`] を使い、走行中に届いたという**事実**
/// だけを 1 行足す。ここに「必ず返せ」等の指示は書かない — 届けるのが仕事であって、
/// 応答するかどうかはエージェントの判断に委ねる。
fn format_live_inbound(log: &opencrab_db::queries::SessionLogRow) -> String {
    format!(
        "[新着メッセージ: あなたがこのターンを処理している間に届きました]\n{}",
        format_single_log(log)
    )
}

/// 走行中サブタスクへ届いた steer（追加指示）を反復の合間に注入する実体（#647）。
///
/// サブタスクは `run_agent_response` を depth+1 で再入し、親ターンと同じ engine ループ
/// （毎イテレーション `LiveInboundSource::poll_new_messages` を引く / #289）を通る。steer は
/// この既存機構をそのままサブへ通したもので、`SessionLiveInbound`（ユーザー発話版）の
/// steer 版にあたる。sub-session（`subtask-{id}`）に `steer_subtask` が積んだ
/// `log_type='steer'` の行だけを watermark 差分で読み、次の LLM 呼び出しへ user メッセージ
/// として足す。
///
/// 差分（`SessionLiveInbound` との違い）:
/// - 対象 `log_type` は `speech` ではなく `steer`（`STEER_LOG_TYPE`）。発話者フィルタは無い
///   （steer は親/オーナーの明示指示であり、送り主は認可済み）。
/// - depth>0（サブタスク）で配線する。親ターン（depth==0）は従来どおり `SessionLiveInbound`。
///
/// 重複注入防止は `watermark`（取得済み最大 log id）。初期値は engine 起動時点の最新 id
/// なので、以後に届いた steer だけが注入される。
struct SubtaskSteerInbound {
    db: opencrab_db::Db,
    /// サブタスク自身のセッション ID（`subtask-{id}`）。steer はここへ積まれる。
    sub_session_id: String,
    /// 取得済みの最大 log id。これより後の steer 行だけを次回返す。
    watermark: std::sync::atomic::AtomicI64,
}

impl SubtaskSteerInbound {
    /// watermark 初期値を **0（セッション先頭）** にして組み立てる。
    ///
    /// `SessionLiveInbound`（親ターン用）は「起動時点の最新 id」で初期化する。あちらは
    /// 会話履歴をターン開始時に 1 度組んでおり、既に履歴へ載った過去発言を二重注入しない
    /// ためにその値が要る。だが steer の宛先は **spawn したばかりの新規 sub-session**で、
    /// engine が動き出す前に steer が積まれることは無い（過去の steer が存在しない）。
    /// にもかかわらず「最新 id」で初期化すると、spawn 直後〜この `new()` までの窓に届いた
    /// steer を取りこぼす（`steer_subtask` は Accepted を返したのに読まれない）。リプレイの
    /// 心配が無い場所なので 0 から読む方が正しく、「Accepted なのに読まれない」を settle
    /// race（doc 明記済みの許容窓）だけに絞れる。
    fn new(db: opencrab_db::Db, sub_session_id: &str) -> Self {
        Self {
            db,
            sub_session_id: sub_session_id.to_string(),
            watermark: std::sync::atomic::AtomicI64::new(0),
        }
    }
}

impl opencrab_core::LiveInboundSource for SubtaskSteerInbound {
    fn poll_new_messages(&self) -> Vec<String> {
        use std::sync::atomic::Ordering;

        let after_id = self.watermark.load(Ordering::Relaxed);
        let conn = match self.db.lock() {
            Ok(conn) => conn,
            // ロックが取れないだけで反復を落とさない（次のイテレーションで拾える）。
            Err(_) => return Vec::new(),
        };
        let rows = match opencrab_db::queries::list_steer_logs_after(
            &conn,
            &self.sub_session_id,
            after_id,
            LIVE_INBOUND_POLL_LIMIT,
        ) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(session_id = %self.sub_session_id, "steer inbound poll failed: {e}");
                return Vec::new();
            }
        };
        drop(conn);

        if rows.is_empty() {
            return Vec::new();
        }
        // 返した行まで watermark を進める（＝同じ steer を二度注入しない）。
        if let Some(max_id) = rows.iter().filter_map(|r| r.id).max() {
            self.watermark.store(max_id, Ordering::Relaxed);
        }
        rows.iter()
            .map(|r| format_steer_inbound(&r.content))
            .collect()
    }
}

/// 走行中サブへ届いた steer を LLM へ見せる形に整える（#647）。
///
/// 親/オーナーからの**明示の追加指示**であることを明記し、受領/反映を親へ返すよう促す。
/// ただし tool 呼び出しを system レベルで強制はしない（`SessionLiveInbound` と同じ「足すだけ・
/// 応答は判断に委ねる」方針 / #288。steer は指示の性質が強いので促し文言を添える点だけが差）。
fn format_steer_inbound(message: &str) -> String {
    format!(
        "[追加指示 (steer): 親/オーナーからの指示が、あなたがこのタスクを実行している間に届きました]\n\
         {message}\n\
         （この指示を踏まえて以後の方針を調整し、受領した旨と反映内容を report_progress で親へ返してください。）"
    )
}

/// 変動コンテキストを最後のuserメッセージに前置するヘルパー（実体は
/// [`opencrab_core::runtime_context`] / #190 S2）。
///
/// 純関数なので下位層へ移した。transport 側のクレートが
/// `crates/server` を参照せずに使えるようにするため。既存の呼び出し元
/// （`process::prepend_runtime_context(..)`）を変えずに済むよう再エクスポートを残す。
pub use opencrab_core::runtime_context::prepend_runtime_context;

/// Discord用: message_idを含む変動コンテキストを前置するヘルパー
pub fn prepend_runtime_context_discord(
    user_message: &str,
    session_theme: &str,
    message_id: &str,
) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z");
    let tz_name = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string());
    let now = format!("{now} ({tz_name})");
    format!(
        "[Context]\nCurrent date and time: {now}\nCurrent discussion topic: {session_theme}\nDiscord message_id: {message_id}\n\n{user_message}"
    )
}

/// 失敗行の `error_body` に、`llm_logs.prompt` 列と**同じ全体シリアライズ**のサイズ
/// （文字数）を一様に付ける（#706）。空応答など「なぜ答えられなかったか」の行を見た人が、
/// 別列 `prompt` を辿らずに **1 クエリで「長さが原因か」の当たり**を付けられるようにする。
///
/// - `error_str` が `None`（＝成功行）のときは `None` を返し、**サイズを一切測らない**
///   （毎リクエストで 100 万文字規模を再走査しない）。
/// - `prompt_json` は呼び出し側が `prompt` 列用に既に持っている文字列を渡す前提
///   （追加のシリアライズはしない）。文字数は `llm_logs.prompt` と同一スケールなので、
///   運用者の実測帯（プロンプト全体で測った値）と直接比較できる。tool 定義・過去の
///   tool_call arguments も含まれる（本文のみだと過小になり読み違いを招く）。
/// - **閾値判定はしない**。事実（送ったサイズ）だけを残し、判断は読む人に委ねる。
/// - error_code に依らず失敗行へ一様に付くので、**新しい失敗種別を足しても自動で載る**
///   （種別ごとの手書き補間を engine 側に散らさない）。
fn error_body_with_prompt_size(error_str: Option<&str>, prompt_json: &str) -> Option<String> {
    error_str.map(|body| {
        let prompt_chars = prompt_json.chars().count();
        format!(
            "{body} [prompt_chars={prompt_chars}（llm_logs.prompt と同じ全体シリアライズの\
             文字数。provider の usage は当てにならないため実測。閾値判定なし）]"
        )
    })
}

/// LLM 呼び出しログ（llm_logs テーブル）記録コールバックの配線（#33: 段の分解）。
fn set_llm_log_callback(
    engine: &mut opencrab_core::SkillEngine,
    log_db: opencrab_db::Db,
    log_agent_id: String,
    log_session_id: String,
    log_trigger_message_id: Option<String>,
) {
    engine.set_log_callback(move |log: &LlmCallLog| {
        let (prompt_tokens, completion_tokens, total_tokens) = log
            .response
            .as_ref()
            .map(|r| &r.usage)
            .map(|u| {
                (
                    Some(u.prompt_tokens as i64),
                    Some(u.completion_tokens as i64),
                    Some(u.total_tokens as i64),
                )
            })
            .unwrap_or((None, None, None));

        let cache_read_tokens = log
            .response
            .as_ref()
            .map(|r| &r.usage)
            .map(|u| u.cache_read_input_tokens as i64);
        let cache_creation_tokens = log
            .response
            .as_ref()
            .map(|r| &r.usage)
            .map(|u| u.cache_creation_input_tokens as i64);

        let response_str = log
            .response
            .as_ref()
            .map(|r| serde_json::to_string(r).unwrap_or_default())
            .unwrap_or_default();

        // #706: リクエスト全体のシリアライズは prompt 列用に元々ここで走る。空応答など
        // 失敗行の原因（プロンプト長）の当たり付けに、この**同じ**文字列のサイズを使い回す
        // （追加のシリアライズも、成功行での再走査もしない。error_body_with_prompt_size 参照）。
        let prompt_json = serde_json::to_string(&log.request).unwrap_or_default();

        let log_row = opencrab_db::queries::LlmLogRow {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: log_agent_id.clone(),
            session_id: Some(log_session_id.clone()),
            model: Some(log.request.model.clone()),
            prompt: prompt_json.clone(),
            response: response_str,
            tool_calls: log
                .response
                .as_ref()
                .and_then(|r| r.first_message())
                .and_then(|m| m.tool_calls.as_ref())
                .filter(|tc| !tc.is_empty())
                .and_then(|tc| serde_json::to_string(tc).ok()),
            latency_ms: Some(log.latency_ms),
            prompt_tokens,
            completion_tokens,
            total_tokens,
            // #706 / #676 / #539: error_code の判定は engine 側で一元化済み
            // （transport error / context 超過 / 空応答 / 出力上限切り捨て）。ここは
            // その値を写すだけ——文字列一致を process 側で再実装しない（判断は core、
            // ゲート/writer は配送）。
            error_code: log.error_code.clone(),
            error_body: error_body_with_prompt_size(log.error_str.as_deref(), &prompt_json),
            requested_at: Some(log.requested_at.clone()),
            trigger_message_id: log_trigger_message_id.clone(),
            is_bot_iteration: log.is_bot_iteration,
            cache_read_tokens,
            cache_creation_tokens,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Ok(conn) = log_db.lock() {
            if let Err(e) = opencrab_db::queries::insert_llm_log(&conn, &log_row) {
                tracing::error!("Failed to insert llm_log: {e}");
            }
        }
    });
}

/// サブタスク走行の実況（#175 S4）の配線。ツール呼び出しと結果を進捗として通知口へ流す。
///
/// 購読していない（`wants_progress()` が false）ならフック自体を挿さず、要約の計算も
/// 省く（旧 `execute_spawn_subtask` と同じ判定）。
///
/// #397: ここと [`set_turn_log_callbacks`] は**同じ engine の同じフック**に載る。engine
/// 側が代入だった頃は、後から呼ばれる `set_turn_log_callbacks`（`persist_turn_logs` が
/// true のとき）がこの実況を丸ごと上書きして消していた。今は `add_on_tool_*` で足すので
/// 両方生き、配線の順序にも依存しない。
fn set_run_notifier_callbacks(
    engine: &mut opencrab_core::SkillEngine,
    notifier: &std::sync::Arc<dyn opencrab_actions::SubtaskRunNotifier>,
    session_id: String,
) {
    if !notifier.wants_progress() {
        return;
    }
    let on_call = notifier.clone();
    engine.add_on_tool_call(move |assistant_content, tool_calls_json| {
        on_call.on_progress(&summarize_tool_calls(&assistant_content, &tool_calls_json));
    });
    let on_result = notifier.clone();
    engine.add_on_tool_result(move |tool_call_id, tool_name, result_json, is_error| {
        on_result.on_progress(&tool_result_progress_line(
            &tool_name,
            &result_json,
            is_error,
            &session_id,
            &tool_call_id,
        ));
    });
}

/// 実況として通知口へ流す 1 行を組む（ツール名・成否・結果のプレビュー）。
///
/// **無害化してからプレビューを切る**。実況は webhook で系の外へ出る経路なので、
/// 永続化側（[`set_turn_log_callbacks`]）と同じ `sanitize_tool_result_for_log` を通し、
/// nsec が生のまま 500 文字に混ざらないようにする。秘密が結果のどこに現れるかは
/// ツール次第で、先頭 500 文字を見て安全と判断はできないため、**切る前に**通す。
///
/// `workspace_root` は `None` を渡す。engine は callback より手前で `cap_tool_result` を
/// かけており、ここへ来る本文は上限内なので退避は起きない。仮に起きても実況が
/// ワークスペースへ書く必要はない（永続化側と二重に書くことになる）。
fn tool_result_progress_line(
    tool_name: &str,
    result_json: &str,
    is_error: bool,
    session_id: &str,
    tool_call_id: &str,
) -> String {
    let status = if is_error { "failed" } else { "completed" };
    let safe = opencrab_actions::sanitize_tool_result_for_log(
        tool_name,
        result_json,
        session_id,
        tool_call_id,
        None,
    );
    let preview: String = safe.chars().take(500).collect();
    format!("tool `{tool_name}` {status}\n{preview}")
}

/// ターンの tool_call / tool_result を session_logs に記録するコールバックの配線
/// （#33: 段の分解。tool_result はサイズ上限超過時にワークスペースへ退避）。
fn set_turn_log_callbacks(
    engine: &mut opencrab_core::SkillEngine,
    db: opencrab_db::Db,
    agent_id: String,
    session_id: String,
    tool_result_workspace: std::path::PathBuf,
    // §9A.1 / row292: gateway 宣言 DI operation の tool_call は arguments を会話へ verbatim 保持
    // する（reply 本文が次ターンで消えない）。ここに名前が入る call は digest から除外する。
    // 名前は runtime の RunRequest.gateway_actions 由来で core に platform 語彙を持たない。
    di_op_names: std::collections::HashSet<String>,
) {
    {
        let tc_db = db.clone();
        let tc_agent = agent_id.clone();
        let tc_session = session_id.clone();
        engine.add_on_tool_call(move |content: String, tool_calls_json: String| {
            if let Ok(conn) = tc_db.lock() {
                // LLMがtext+tool_callsを同時に返した場合、textをspeechとして記録する。
                // #899 §12.6: 保存前に NO_REPLY 終端解釈（単一実装 visible_speech_after_markers）を
                // 通す。沈黙（前段が空）は監査行を残さない（残すと conversation_typed が次ターンの
                // typed 履歴へ assistant 'NO_REPLY' として再注入する）。
                // ツールのみ生成（content 空）は `Some("")` になるため、旧 `!content.trim().is_empty()`
                // と同じく空/空白を弾く（空 speech 行＝typed の空 assistant を作らない）。
                let visible = opencrab_actions::visible_speech_after_markers(
                    &content,
                    opencrab_actions::DeliveryContext {
                        session_id: &tc_session,
                        agent_id: &tc_agent,
                        origin: "engine",
                    },
                )
                .filter(|body| !body.trim().is_empty());
                if let Some(body) = visible {
                    let speech_log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: tc_agent.clone(),
                        session_id: tc_session.clone(),
                        log_type: "speech".to_string(),
                        content: body,
                        speaker_id: Some(tc_agent.clone()),
                        turn_number: None,
                        metadata_json: None,
                        created_at: None,
                    };
                    if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &speech_log) {
                        tracing::error!(agent_id = %tc_agent, session_id = %tc_session, "Failed to insert speech log (with tool_call): {e}");
                    }
                }
                // 発話クラス（reply/reaction/repost・§3.3.1 C6）は engine が tool_calls_json から
                // 除外して渡す。除外の結果ツールが 1 つも残らない（空配列）ターンは、機械行
                // （空の tool_call ログ）を残さない。発話本文は配送経路が speech として永続する。
                let has_persistable_calls = serde_json::from_str::<serde_json::Value>(
                    &tool_calls_json,
                )
                .ok()
                .and_then(|v| v.as_array().map(|a| !a.is_empty()))
                .unwrap_or(!tool_calls_json.trim().is_empty());
                if !has_persistable_calls {
                    return;
                }
                // DI operation の call id を記録し、会話再構成で arguments を digest 除外する。
                let preserve_ids: Vec<String> =
                    serde_json::from_str::<serde_json::Value>(&tool_calls_json)
                        .ok()
                        .and_then(|v| {
                            v.as_array().map(|items| {
                                items
                                    .iter()
                                    .filter_map(|it| {
                                        let id = it.get("id")?.as_str()?;
                                        let name = it
                                            .get("function")
                                            .and_then(|f| f.get("name"))
                                            .or_else(|| it.get("name"))
                                            .and_then(|n| n.as_str())?;
                                        di_op_names
                                            .contains(name)
                                            .then(|| id.to_string())
                                    })
                                    .collect()
                            })
                        })
                        .unwrap_or_default();
                let metadata = if preserve_ids.is_empty() {
                    serde_json::json!({ "tool_calls_json": tool_calls_json })
                } else {
                    serde_json::json!({
                        "tool_calls_json": tool_calls_json,
                        "preserve_arg_call_ids": preserve_ids,
                    })
                };
                let log = opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: tc_agent.clone(),
                    session_id: tc_session.clone(),
                    log_type: "tool_call".to_string(),
                    content,
                    speaker_id: Some(tc_agent.clone()),
                    turn_number: None,
                    metadata_json: Some(metadata.to_string()),
                    created_at: None,
                };
                if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
                    tracing::error!(agent_id = %tc_agent, session_id = %tc_session, "Failed to insert tool_call log: {e}");
                }
            }
        });
    }

    // on_tool_result callback: save tool_result to DB.
    {
        let tr_db = db;
        let tr_agent = agent_id;
        let tr_session = session_id;
        let tr_workspace = tool_result_workspace;
        engine.add_on_tool_result(
            move |tool_call_id: String, tool_name: String, result_json: String, is_error: bool| {
                // 永続化前の無害化（秘密フィールドのマスク ＋ サイズ上限/ワークスペース
                // 退避）は background dispatch 経路（`SubtaskToolDispatcher` →
                // `settle_completed`）と**共通の関数**を使う。片方だけ素通りすると、
                // 巨大結果や秘密鍵がそのまま session_logs に入り、次ターンの会話
                // 再構築へ再注入される。
                let content = opencrab_actions::sanitize_tool_result_for_log(
                    &tool_name,
                    &result_json,
                    &tr_session,
                    &tool_call_id,
                    Some(tr_workspace.as_path()),
                );

                if let Ok(conn) = tr_db.lock() {
                    let log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: tr_agent.clone(),
                        session_id: tr_session.clone(),
                        log_type: "tool_result".to_string(),
                        content,
                        speaker_id: Some(tr_agent.clone()),
                        turn_number: None,
                        metadata_json: Some(
                            serde_json::json!({
                                "tool_call_id": tool_call_id,
                                "tool_name": tool_name,
                                "is_error": is_error,
                            })
                            .to_string(),
                        ),
                        created_at: None,
                    };
                    if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
                        tracing::error!(agent_id = %tr_agent, session_id = %tr_session, "Failed to insert tool_result log: {e}");
                    }
                }
            },
        );
    }
}

/// 引数の image_urls と、直近ユーザーログ metadata の image_urls をマージする
/// （#33: 段の分解）。
fn merge_image_urls(
    state: &AppState,
    session_id: &str,
    agent_id: &str,
    base: &[String],
) -> Vec<String> {
    {
        let mut urls: Vec<String> = base.to_vec();
        if let Ok(conn) = state.db.lock() {
            if let Ok(logs) = opencrab_db::queries::list_session_logs_by_session(&conn, session_id)
            {
                if let Some(latest_user_log) = logs.iter().rev().find(|log| {
                    log.log_type == "speech"
                        && log
                            .speaker_id
                            .as_deref()
                            .map(|s| s != agent_id)
                            .unwrap_or(true)
                }) {
                    if let Some(ref meta_json) = latest_user_log.metadata_json {
                        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_json) {
                            if let Some(arr) = meta["image_urls"].as_array() {
                                for v in arr {
                                    if let Some(s) = v.as_str() {
                                        let url = s.to_string();
                                        if !urls.contains(&url) {
                                            urls.push(url);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // #272 P1: 画像がどのターンで LLM に載ったかを後追いできるよう INFO に上げる。
        // 署名付き URL は秘匿情報になりうるので件数のみ出す。
        if !urls.is_empty() {
            tracing::info!(
                session_id = %session_id,
                count = urls.len(),
                "run_agent_response: merging image_urls for LLM"
            );
        }
        urls
    }
}

/// 未インデックスのログが閾値を超えていたら、バックグラウンドでメモリインデックスを
/// 構築する（#33: 段の分解。run の応答は待たせない）。
/// スキル名が応答本文で言及される最低文字数。短い名前は他語の部分一致で
/// 誤カウントしやすいので閾値でノイズを抑える。
const MIN_SKILL_NAME_LEN_FOR_MATCH: usize = 4;

/// 応答本文に skill 名が現れているか（大文字小文字無視の部分一致）。
/// `response_lower` は呼び出し側で小文字化済みを渡す。ツール名ベースの
/// 確実な信号が server 経路に無いため、これが「実際に使った」の実用的な検出。
fn skill_mentioned(response_lower: &str, skill_name: &str) -> bool {
    let name = skill_name.trim().to_lowercase();
    name.chars().count() >= MIN_SKILL_NAME_LEN_FOR_MATCH && response_lower.contains(&name)
}

/// depth 0 の run 完了時、応答で言及された有効スキルの利用回数を +1 する。
/// 「実際に使った時だけ」カウントするための best-effort（名前言及ベース）。
fn record_used_skills(state: &AppState, agent_id: &str, session_id: &str, response: &str) {
    if response.trim().is_empty() {
        return;
    }
    let response_lower = response.to_lowercase();
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return,
    };
    let skills = opencrab_db::queries::list_skills(&conn, agent_id, true).unwrap_or_default();
    for s in &skills {
        if skill_mentioned(&response_lower, &s.name) {
            if let Err(e) = opencrab_db::queries::increment_skill_usage(&conn, &s.id) {
                tracing::warn!(skill = %s.name, error = %e, "failed to increment skill usage");
            }
            // スリープ棚卸しの弱い利用ヒント: セッション単位でも記録する（名前一致ベース）。
            if let Err(e) =
                opencrab_db::queries::insert_skill_usage(&conn, agent_id, &s.id, session_id)
            {
                tracing::warn!(skill = %s.name, error = %e, "failed to log skill usage session");
            }
        }
    }
}

fn spawn_background_index_build(state: &AppState, agent_id: &str, effective_model: &str) {
    {
        let index_db = state.db.clone();
        let index_agent_id = agent_id.to_string();
        let index_llm_router = state.llm_router.clone();
        let index_model = effective_model.to_string();
        let inflight = state.index_build_inflight.clone();
        let (index_persona_name, index_personality) = {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::get_agent(&conn, &index_agent_id)
                .ok()
                .flatten()
                .map(|a| (a.persona_name, a.personality))
                .unwrap_or_default()
        };
        tokio::spawn(async move {
            let (unindexed, config) = {
                let Ok(conn) = index_db.lock() else { return };
                let unindexed =
                    opencrab_db::queries::get_unindexed_log_count(&conn, &index_agent_id)
                        .unwrap_or(0);
                let config = opencrab_db::queries::get_memory_index_config(&conn, &index_agent_id)
                    .unwrap_or_else(|_| opencrab_db::queries::AgentMemoryIndexConfig {
                        agent_id: index_agent_id.clone(),
                        batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
                        threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
                        updated_at: String::new(),
                    });
                (unindexed, config)
            };
            if unindexed < config.threshold {
                return;
            }
            // メンテナンスループとの二重ビルド防止（watermark 冪等が正しさの本線、
            // このフラグは同じバッチへの重複 LLM 支出を防ぐだけ）。
            let _guard = match crate::memory_maintenance::try_acquire_build_slot(
                &inflight,
                &index_agent_id,
            ) {
                Some(g) => g,
                None => {
                    tracing::debug!(agent_id = %index_agent_id, "index build already in flight; skipping post-run build");
                    return;
                }
            };
            tracing::info!(
                agent_id = %index_agent_id,
                unindexed = unindexed,
                threshold = config.threshold,
                batch_size = config.batch_size,
                "Starting background memory index build"
            );
            let llm_adapter = LlmRouterAdapter::new(index_llm_router);
            match opencrab_core::memory_index::IndexBuilder::build_incremental(
                &index_db,
                &index_agent_id,
                &llm_adapter,
                &index_model,
                config.batch_size as usize,
                &index_persona_name,
                index_personality.as_deref(),
            )
            .await
            {
                Ok(result) => {
                    tracing::info!(
                        agent_id = %index_agent_id,
                        nodes_created = result.nodes_created,
                        logs_indexed = result.logs_indexed,
                        "Background memory index build completed"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        agent_id = %index_agent_id,
                        error = %e,
                        "Background memory index build FAILED"
                    );
                }
            }
        });
    }
}

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

/// ループ再起動 v1（#52）の判定と準備。
///
/// 再実行すべきなら「再構築した会話文字列」を返す（[restart] decision の記録と
/// restart_count のインクリメントは済ませてある）。それ以外は None。
///
/// - 対象: depth 0、`loop_restart_enabled`、run が Ok かつ stopped_by_limit、
///   セッションに active タスクが残っている場合のみ。
/// - 上限は二重: per-task（restart_count、永続）+ per-call（restarts_this_call）。
///   per-call 上限が無いと、再実行中にエージェントがタスクを close/open して
///   差し替えた場合に新タスク（restart_count=0）で再々実行が始まり非有界になる。
/// - 記録順序: decision → 会話再構築 → increment → 再実行。decision を先に書くのは
///   再構築される会話（台帳セクション）に載せて run-2 へ見せるため。increment は
///   再構築の**後**（失敗時に per-task 予算を消費しない）かつ再実行の**前**
///   （再実行中にクラッシュしても、次回の上限判定が効いて無限再起動しない。
///   decision〜increment 間のクラッシュは「予算未消費のまま decision が残る」だけで
///   無害 — 次の再起動判定はフルの run を経てしか到達しない）。
/// - per-task 予算枯渇時の abandoned 遷移は「この呼び出しで実際に再実行した」
///   （= run-2 も上限で停止した）場合のみ。過去ターンで予算を使い切ったタスクを、
///   後日の上限停止で突然殺さない。abandoned は session_logs にも記録する: 台帳
///   セクションは active タスクしか描画しないため、blocker エントリだけでは次ターン
///   以降のエージェント/ユーザーから不可視になる。
fn prepare_loop_restart(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
    depth: u32,
    restarts_this_call: i64,
    trigger_message_id: Option<&str>,
    result: &anyhow::Result<opencrab_core::EngineResult>,
) -> Option<String> {
    /// v1 の再実行上限（per-task / per-call 共通）。
    const LOOP_RESTART_MAX: i64 = 1;

    if depth != 0 || !state.loop_restart_enabled {
        return None;
    }
    if !matches!(result, Ok(er) if er.stopped_by_limit) {
        return None;
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(session_id = %session_id, "loop restart skipped (db lock): {e}");
            return None;
        }
    };
    let task = opencrab_db::queries::get_active_task_for_session(&conn, agent_id, session_id)
        .ok()
        .flatten()?;

    if task.restart_count >= LOOP_RESTART_MAX {
        // per-task 予算枯渇。この呼び出しで実際に再実行していた場合のみ abandoned に
        // 落とす（= 再実行後もまた上限で停止した）。そうでなければ何もしない
        // （機能導入前と同じ「上限で止まって終わり」— 過去ターンで予算を使い切った
        // タスクを後日の上限停止で突然殺さない）。
        if restarts_this_call > 0 {
            tracing::warn!(
                session_id = %session_id,
                task_id = task.id,
                restart_count = task.restart_count,
                "loop restart budget exhausted; abandoning task"
            );
            let _ = opencrab_db::queries::insert_task_progress(
                &conn,
                task.id,
                "blocker",
                &format!(
                    "[restart] 自動再実行後も反復上限で停止した（restart 上限 {LOOP_RESTART_MAX} 回に到達）。\
                     タスクを abandoned にする。再開には goal/contract の再交渉か人手の介入が必要。"
                ),
            );
            let _ = opencrab_db::queries::update_task_status(&conn, agent_id, task.id, "abandoned");
            // 台帳セクションは active タスクしか描画しない → session_logs 側にも
            // 残して、次ターンの会話から見えるようにする（詳細は get_task で辿れる）。
            opencrab_db::queries::insert_session_log_best_effort(
                &conn,
                &opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    log_type: "task_event".to_string(),
                    content: format!(
                        "Task #{} was abandoned automatically: the run hit the iteration \
                         limit again right after an automatic restart. Goal: {}. Renegotiate \
                         the goal/contract with the user or ask for help before reopening \
                         (full history: get_task).",
                        task.id,
                        task.goal.chars().take(200).collect::<String>(),
                    ),
                    speaker_id: Some("system".to_string()),
                    turn_number: None,
                    metadata_json: Some(
                        serde_json::json!({
                            "task_id": task.id,
                            "event": "abandoned_by_loop_restart",
                        })
                        .to_string(),
                    ),
                    created_at: None,
                },
            );
        }
        return None;
    }
    if restarts_this_call >= LOOP_RESTART_MAX {
        // per-call 安全弁: 再実行中にタスクが差し替わっていても（新タスクは
        // restart_count=0）、この呼び出し内ではこれ以上再実行しない。
        // 新タスクの per-task 予算は消費しない。
        tracing::warn!(
            session_id = %session_id,
            task_id = task.id,
            "loop restart per-call cap reached; not restarting again in this call"
        );
        return None;
    }

    // decision を先に記録する: 直後に再構築する会話へ台帳セクション経由で載り、
    // run-2 が「これは再実行である」ことと埋めるべき gaps の在処を知る。
    // 停止時の EngineResult.response は定型文（"I've reached the maximum..."）で
    // 情報が無いため記録しない — run-1 の実質的な結論はツール実行時の speech ログに
    // 残っており、再構築した会話に含まれる。
    let _ = opencrab_db::queries::insert_task_progress(
        &conn,
        task.id,
        "decision",
        &format!(
            "[restart] 反復上限で停止したため、クリーンな context で自動再実行する（{} 回目 / 上限 {LOOP_RESTART_MAX}）。\
             直近の [evaluation] エントリの gaps を優先的に埋め、この再実行で完了できなければ blocker を記録すること。",
            task.restart_count + 1
        ),
    );

    // 会話を再構築: run-1 のトレース・evaluation（gaps 全文）・上の decision が入る。
    // 呼び出し元が付けていた [Context] 前置（日時 / テーマ / Discord message_id）も
    // ここで再現する（無いと run-2 が現在日時を失い、message_id 依存のゲートウェイ
    // 操作ができなくなる）。
    let (system_prompt, _) =
        build_agent_context(&conn, agent_id, &opencrab_actions::CallerIdentity::Owner);
    let theme = opencrab_db::queries::get_session(&conn, session_id)
        .ok()
        .flatten()
        .map(|s| s.theme)
        .unwrap_or_default();
    let runtime_text = match trigger_message_id {
        Some(message_id) if !message_id.is_empty() => {
            prepend_runtime_context_discord("", &theme, message_id)
        }
        _ => prepend_runtime_context("", &theme),
    };
    let functions_tokens = match core_functions_tokens() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error_name = e.name(),
                "loop restart aborted ({name}): {e}",
                name = e.name()
            );
            return None;
        }
    };
    let env = match resolve_agent_request_envelope(RequestEnvelopeArgs {
        conn: &conn,
        agent_id,
        session_id,
        default_model: &state.default_model,
        policy: &state.context_budget_policy(),
        system_prompt: &system_prompt,
        runtime_context_text: &runtime_text,
        functions_tokens,
        entrypoint: "process_loop_restart",
    }) {
        Ok(env) => env,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error_name = e.name(),
                "loop restart aborted ({name}): {e}",
                name = e.name()
            );
            let _ = opencrab_db::queries::insert_task_progress(
                &conn,
                task.id,
                "progress",
                &format!(
                    "[restart] {} のため自動再実行を中止した（予算は未消費）。",
                    e.name()
                ),
            );
            return None;
        }
    };
    let rebuilt = match build_conversation_string_with_waters(
        &conn,
        session_id,
        agent_id,
        env.conversation_high,
        env.conversation_low,
        include_memory_index(&env),
    ) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                "loop restart aborted (conversation rebuild failed): {e}"
            );
            let _ = opencrab_db::queries::insert_task_progress(
                &conn,
                task.id,
                "progress",
                "[restart] 会話の再構築に失敗したため自動再実行を中止した（予算は未消費）。",
            );
            return None;
        }
    };
    let rebuilt = match trigger_message_id {
        Some(message_id) if !message_id.is_empty() => {
            prepend_runtime_context_discord(&rebuilt, &theme, message_id)
        }
        _ => prepend_runtime_context(&rebuilt, &theme),
    };

    // 再実行の直前にカウントを永続化（再実行中にクラッシュしても上限判定が効く）。
    match opencrab_db::queries::increment_task_restart_count(&conn, agent_id, task.id) {
        Ok(true) => {}
        _ => return None,
    }

    tracing::info!(
        session_id = %session_id,
        task_id = task.id,
        restart_count = task.restart_count + 1,
        "restarting engine run after iteration limit (loop restart v1)"
    );
    Some(rebuilt)
}

#[cfg(test)]
mod skill_mentioned_tests {
    use super::skill_mentioned;

    #[test]
    fn matches_name_case_insensitively() {
        let resp = "この件は Deploy Runbook に従って対応しました。".to_lowercase();
        assert!(skill_mentioned(&resp, "Deploy Runbook"));
        assert!(skill_mentioned(&resp, "deploy runbook"));
    }

    #[test]
    fn ignores_short_names() {
        // 3文字以下は誤マッチ防止で対象外
        let resp = "abc の話".to_lowercase();
        assert!(!skill_mentioned(&resp, "abc"));
    }

    #[test]
    fn no_match_when_absent() {
        let resp = "普通の返答です".to_lowercase();
        assert!(!skill_mentioned(&resp, "translation-helper"));
    }
}

#[cfg(test)]
mod error_body_with_prompt_size_tests {
    use super::error_body_with_prompt_size;

    /// #706: 成功行（error_str=None）にはサイズを付けない（＝毎リクエストで再走査しない）。
    #[test]
    fn success_row_gets_no_size() {
        assert_eq!(
            error_body_with_prompt_size(None, "とても長いプロンプト"),
            None
        );
    }

    /// #706: 失敗行には prompt 列と同じ全体シリアライズの**文字数**が一様に載る。
    /// マルチバイトでもバイト数ではなく文字数（Unicode スカラー数）で数える。
    #[test]
    fn failure_row_appends_prompt_char_count() {
        // "あいうえお" = 5 文字 / 15 バイト。文字数で数えていることを固定する。
        let prompt_json = "あいうえお";
        let out = error_body_with_prompt_size(Some("空でした"), prompt_json)
            .expect("失敗行にはサイズ付き error_body が返るはず");
        assert!(out.contains("空でした"), "元の理由が保たれていない: {out}");
        assert!(
            out.contains("prompt_chars=5"),
            "prompt 列と同じシリアライズの文字数（5）が載っていない: {out}"
        );
        assert!(
            !out.contains("prompt_chars=15"),
            "バイト数で数えてしまっている: {out}"
        );
    }
}

#[cfg(test)]
mod peer_reviewers_section_tests {
    use super::peer_reviewers_section;
    use opencrab_db::queries::TrustedUserPermission;

    #[test]
    fn roster_lists_co_agents_only_and_handles_empty() {
        let conn = opencrab_db::init_memory().unwrap();
        assert_eq!(peer_reviewers_section(&conn, "a1"), "");

        opencrab_db::queries::add_trusted_user(
            &conn,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
            "r1",
            "a1",
            "42",
            TrustedUserPermission::CoAgent,
            "owner",
            "2026-01-01",
            "Crab B",
        )
        .unwrap();
        opencrab_db::queries::add_trusted_user(
            &conn,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
            "r2",
            "a1",
            "43",
            TrustedUserPermission::CoAgent,
            "owner",
            "2026-01-01",
            "",
        )
        .unwrap();
        opencrab_db::queries::add_trusted_user(
            &conn,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
            "r3",
            "a1",
            "44",
            TrustedUserPermission::User,
            "owner",
            "2026-01-01",
            "Human",
        )
        .unwrap();

        let section = peer_reviewers_section(&conn, "a1");
        // 表示名のみ。メンション記法（transport 固有）は共有プロンプトに出さない（#158 S2）。
        assert!(section.contains("- Crab B"));
        assert!(!section.contains("<@"));
        assert!(!section.contains("42"));
        // 表示名が空の行（id=43）は指名できないので載せない
        assert!(!section.contains("43"));
        assert!(!section.contains("Human"));
        // 他エージェントのロスターには出ない
        assert_eq!(peer_reviewers_section(&conn, "a2"), "");
    }

    /// 表示名のある co_agent が居なければロスターは空（id だけの行は載らない）。
    #[test]
    fn roster_is_empty_when_all_display_names_are_blank() {
        let conn = opencrab_db::init_memory().unwrap();
        opencrab_db::queries::add_trusted_user(
            &conn,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
            "r1",
            "a1",
            "42",
            TrustedUserPermission::CoAgent,
            "owner",
            "2026-01-01",
            "",
        )
        .unwrap();
        assert_eq!(peer_reviewers_section(&conn, "a1"), "");
    }
}

/// 共有システムプロンプトに transport 前提が混ざらないことの検査（#158 S2 の完了条件）。
///
/// transport 固有の 1 行（`[Discord context: ...]` 等）は各ゲートウェイが
/// `build_agent_context` の返り値に**後付け**する。共有部分に transport 語が入ると、
/// Discord 以外（Nostr / web / REST / heartbeat）のターンでモデルが存在しない文脈を
/// 参照したり、幻覚した宛先を書いたりする。
#[cfg(test)]
mod shared_prompt_is_transport_neutral_tests {
    use super::build_agent_context;
    use opencrab_db::queries::TrustedUserPermission;

    /// 共有プロンプトから transport 語が消えていること（grep 相当をテスト化）。
    #[test]
    fn shared_prompt_has_no_transport_specific_terms() {
        let conn = opencrab_db::init_memory().unwrap();
        // ロスターも共有プロンプトの一部なので、レビュアーを登録した状態で検査する。
        opencrab_db::queries::add_trusted_user(
            &conn,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
            "r1",
            "a1",
            "42",
            TrustedUserPermission::CoAgent,
            "owner",
            "2026-01-01",
            "Crab B",
        )
        .unwrap();

        let (prompt, _name) =
            build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);

        // ロスターが載っている（= 空プロンプトを検査して通っているのではない）
        assert!(prompt.contains("- Crab B"), "roster missing: {prompt}");

        for needle in ["Discord", "discord", "[Discord context]", "<@"] {
            assert!(
                !prompt.contains(needle),
                "shared system prompt must not contain {needle:?}:\n{prompt}"
            );
        }
    }

    /// 宛先の取得方法を指示していないこと（宛先は実行側が文脈から既定値にする）。
    #[test]
    fn shared_prompt_does_not_teach_destination_lookup() {
        let conn = opencrab_db::init_memory().unwrap();
        let (prompt, _name) =
            build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);
        assert!(
            !prompt.contains("channel_id"),
            "shared system prompt must not name a transport destination argument:\n{prompt}"
        );
        assert!(prompt.contains("taken from the current conversation"));
    }
}

/// #352: caller=Agent のターンには、オーナーが露出許可（`agent_visible`）した skill だけを
/// system prompt の index へ出す。Owner / CoAgent / TrustedUser は絞らない。
#[cfg(test)]
mod agent_visible_skill_index_tests {
    use super::build_agent_context;
    use opencrab_actions::CallerIdentity;
    use opencrab_db::queries::SkillRow;

    fn insert_skill(conn: &rusqlite::Connection, name: &str, agent_visible: bool) {
        let row = SkillRow {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: "a1".to_string(),
            name: name.to_string(),
            description: format!("{name} desc"),
            situation_pattern: "sp".to_string(),
            guidance: "g".to_string(),
            source_type: "experience".to_string(),
            source_context: None,
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
            permission: "\"agent\"".to_string(),
            archived: false,
            created_caller: None,
            agent_visible,
        };
        opencrab_db::queries::insert_skill(conn, &row).unwrap();
    }

    #[test]
    fn agent_caller_sees_only_visible_skills_in_index() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_skill(&conn, "VisibleSkill", true);
        insert_skill(&conn, "HiddenSkill", false);

        let (agent_prompt, _) = build_agent_context(&conn, "a1", &CallerIdentity::Agent);
        assert!(
            agent_prompt.contains("VisibleSkill"),
            "visible skill missing from agent index:\n{agent_prompt}"
        );
        assert!(
            !agent_prompt.contains("HiddenSkill"),
            "hidden skill leaked into agent index:\n{agent_prompt}"
        );

        // Owner / CoAgent / TrustedUser は両方見える（従来どおり / 絞りは caller=Agent のみ）。
        for caller in [
            CallerIdentity::Owner,
            CallerIdentity::CoAgent {
                agent_id: "peer".to_string(),
            },
            CallerIdentity::TrustedUser,
        ] {
            let (p, _) = build_agent_context(&conn, "a1", &caller);
            assert!(
                p.contains("VisibleSkill") && p.contains("HiddenSkill"),
                "caller {caller:?} must see all skills:\n{p}"
            );
        }
    }

    #[test]
    fn agent_caller_with_no_visible_skills_gets_no_skill_section() {
        let conn = opencrab_db::init_memory().unwrap();
        // 既定 false のみ = Agent には 1 件も見えない。
        insert_skill(&conn, "HiddenOnly", false);

        let (agent_prompt, _) = build_agent_context(&conn, "a1", &CallerIdentity::Agent);
        assert!(!agent_prompt.contains("HiddenOnly"));
        // 空の見出しだけ残さない（セクションごと出さない）。
        assert!(
            !agent_prompt.contains("Your skills (index only"),
            "empty skill section header must not appear for agent caller:\n{agent_prompt}"
        );

        // 同じ DB でも Owner にはセクションと skill が出る。
        let (owner_prompt, _) = build_agent_context(&conn, "a1", &CallerIdentity::Owner);
        assert!(owner_prompt.contains("Your skills (index only"));
        assert!(owner_prompt.contains("HiddenOnly"));
    }
}

/// #428: system プロンプトへの curated 記憶注入が `long_term/<見出し>`（取り込みの実形式）を
/// 拾うことを固定する。従来は完全一致で引いていたため本番の `long_term/*` が 1 件も載らず、
/// 手書き reference facts が全エージェントで死んでいた。
#[cfg(test)]
mod curated_long_term_injection_tests {
    use super::build_agent_context;
    use opencrab_actions::CallerIdentity;
    use opencrab_db::queries::CuratedMemoryRow;

    fn curate(conn: &rusqlite::Connection, category: &str, content: &str) {
        opencrab_db::queries::upsert_curated_memory(
            conn,
            &CuratedMemoryRow {
                id: uuid::Uuid::new_v4().to_string(),
                agent_id: "a1".to_string(),
                category: category.to_string(),
                content: content.to_string(),
                created_at: String::new(),
            },
        )
        .unwrap();
    }

    #[test]
    fn long_term_suffixed_headings_are_injected_and_bundled_by_heading() {
        let conn = opencrab_db::init_memory().unwrap();
        // 本番と同じ形: long_term は接尾辞付きだけ（素の `long_term` 行は無い）。
        curate(&conn, "long_term/A100サーバー", "- GPU は 8 枚");
        curate(&conn, "long_term/Nostr", "- リレーは wss://…");
        // user_profile は単一の完全一致（従来から生きている経路）。
        curate(&conn, "user_profile", "owner さんは…");
        // daily_log/* は注入対象外。前方一致が別 prefix を巻き込まないことの確認。
        curate(&conn, "daily_log/2026-08-11", "きょうの日記の本文");

        let (prompt, _) = build_agent_context(&conn, "a1", &CallerIdentity::Owner);

        // long_term セクションが出て、見出しごとに束ねられ、本文も載る。
        assert!(
            prompt.contains("## Long-term Memory"),
            "Long-term Memory セクションが出ていない:\n{prompt}"
        );
        assert!(
            prompt.contains("### A100サーバー")
                && prompt.contains("- GPU は 8 枚")
                && prompt.contains("### Nostr")
                && prompt.contains("- リレーは wss://…"),
            "long_term/<見出し> が見出し付きで注入されていない:\n{prompt}"
        );
        // user_profile は従来どおり見出し無しで注入（回帰していない）。
        assert!(
            prompt.contains("## User Profile") && prompt.contains("owner さんは…"),
            "user_profile の注入が壊れている:\n{prompt}"
        );
        // daily_log は注入されない（前方一致が別カテゴリを巻き込まない）。
        assert!(
            !prompt.contains("きょうの日記の本文"),
            "daily_log が誤って注入されている:\n{prompt}"
        );
    }

    #[test]
    fn no_long_term_data_means_no_section() {
        let conn = opencrab_db::init_memory().unwrap();
        curate(&conn, "user_profile", "profile only");

        let (prompt, _) = build_agent_context(&conn, "a1", &CallerIdentity::Owner);
        // データが無ければ空の見出しは出さない（agent_rules も同様に本番は 0 行）。
        assert!(
            !prompt.contains("## Long-term Memory"),
            "空の long_term 見出しが出ている:\n{prompt}"
        );
        assert!(
            !prompt.contains("## Agent Rules"),
            "空の agent_rules 見出しが出ている:\n{prompt}"
        );
        assert!(prompt.contains("## User Profile"));
    }
}

#[cfg(test)]
mod typed_hard_cap_tests {
    use super::typed_exceeds_input_budget;

    /// #884 PR2 hard cap: wire トークンが input_high を超えるときだけ flat へ落とす。
    #[test]
    fn typed_falls_back_only_above_input_budget() {
        // 上限以下・ちょうど上限は typed を維持（false）。
        assert!(!typed_exceeds_input_budget(0, 1000));
        assert!(!typed_exceeds_input_budget(999, 1000));
        assert!(!typed_exceeds_input_budget(1000, 1000));
        // 上限超過は flat へフォールバック（true）。
        assert!(typed_exceeds_input_budget(1001, 1000));
        assert!(typed_exceeds_input_budget(50_000, 1000));
    }
}

#[cfg(test)]
mod tool_result_progress_line_tests {
    // 実況（progress line）は永続化と同じ無害化（`sanitize_tool_result_for_log`）を通す。
    // #620: 旧来の nsec キー名マスク（SECRET_KEYS）は撤去したので、ここは上限/退避だけを行う。
    use super::tool_result_progress_line;

    /// 実況行がツール名・成否・中身を含み、失敗は failed と出ること。
    ///
    /// #620: nsec キー名マスクは撤去した。`nostr_generate_key` は実際には nsec を返さない
    /// （npub のみ / `crates/nostr/src/actions.rs`）ので、実運用の実況に nsec は元から
    /// 現れない。よってここでは通常の結果で行の体裁だけを固定する。
    #[test]
    fn test_tool_result_progress_line_shape() {
        let wrapper = r#"{"success":true,"data":{"npub":"npub1abc"},"error":null}"#;
        let line = tool_result_progress_line(
            "nostr_generate_key",
            wrapper,
            false,
            "session-1",
            "tool-call-1",
        );
        // 実況として要る情報（ツール名・成否・中身）は落とさない。
        assert!(line.contains("nostr_generate_key"));
        assert!(line.contains("completed"));
        assert!(line.contains("npub1abc"));
        // 撤去したはずのキー名マスクが復活していない。
        assert!(
            !line.contains("[redacted]"),
            "撤去したマスクが効いている: {line}"
        );

        let failed = tool_result_progress_line("read_file", r#"{"error":"nope"}"#, true, "s", "t");
        assert!(failed.contains("failed"), "失敗は failed と出る: {failed}");
    }
}

/// #284: コンテキストが逼迫しても**直近のユーザー発言は必ずプロンプトに載る**。
///
/// 事故当時、直近 10 件（`RECENT_MIN_LOGS`）が tool_result / evaluation / エージェント
/// 自身の発言で埋まり、ユーザーの生発言が 1 件も入らなかった。エージェントは指示を
/// 一度も見ないまま応答していた。ここで固定するのは「ログ種別に関係なく、直近の
/// ユーザー発言 N 件が優先で残る」こと。
///
/// **行の形は本番と同じでなければならない**（#286）。ユーザー発言は**必ず
/// `record_inbound_message` 経由で**入れること（`agent_id`＝受信側 / `speaker_id`＝送信者、
/// #377）。手書きの行だと本番と形がずれ、述語のバグを見逃す。
///
/// 経緯: 以前ゲートウェイ受信は `agent_id` 列にも送信者 ID を入れており
/// （`agent_id == speaker_id`）、「`speaker_id != log.agent_id`」という列比較の述語が
/// Discord / Nostr では常に false になった（当時の該当 4,490 件すべてが `==`）。#377 で
/// 受信行が `agent_id`＝受信側 に直り列は縮退しなくなったが、正しい述語は今も
/// `speaker_id != <agent_id 引数>`（`opencrab_core::conversation::is_user_speech` 参照）。
#[cfg(test)]
mod recent_user_speech_guarantee_tests {
    use super::{build_conversation_string, RECENT_MIN_USER_SPEECHES};
    use opencrab_actions::transcript::{InboundMessageRecord, TranscriptSource};

    const AGENT: &str = "a1";
    const USER: &str = "owner";
    const SESSION: &str = "s1";

    /// ユーザー発言を**本番と同じ書き手**（`record_inbound_message`）で入れる。
    /// 行の形（`agent_id` 列＝受信側エージェント / `speaker_id` 列＝送信者、#377）を
    /// 再現するのがこのテストの肝。
    fn insert_user_speech(conn: &rusqlite::Connection, text: &str) {
        assert!(
            crate::transcript::record_inbound_message(
                conn,
                TranscriptSource::Discord,
                &InboundMessageRecord {
                    session_id: SESSION,
                    recipient_agent_id: AGENT,
                    sender_id: USER,
                    sender_name: "owner",
                    avatar_url: None,
                    channel_id: Some("222"),
                    pubkey: None,
                    text,
                    image_urls: &[],
                },
            ),
            "テストの前提: 受信発言が記録できること"
        );
    }

    /// エージェント自身の行（発言 / ツール往復）。こちらは `agent_id == speaker_id == AGENT`。
    fn insert_agent_row(conn: &rusqlite::Connection, log_type: &str, content: &str) -> i64 {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: SESSION.to_string(),
                log_type: log_type.to_string(),
                content: content.to_string(),
                speaker_id: Some(AGENT.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap()
    }

    fn last_log_id(conn: &rusqlite::Connection) -> i64 {
        conn.query_row("SELECT MAX(id) FROM memory_sessions", [], |r| r.get(0))
            .unwrap()
    }

    /// ユーザー発言のあとに巨大なツール結果が大量に積まれても、発言は残る。
    #[test]
    fn user_speech_survives_a_flood_of_tool_results() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_user_speech(&conn, "もういっそ全員フォローすればいいよ");
        // 直近 10 件を tool_result / 自分の発言で埋める（当時と同じ形）。
        for i in 0..12 {
            insert_agent_row(
                &conn,
                "tool_result",
                &format!("Following 979 user(s): {}", "npub1xxxx ".repeat(200 + i)),
            );
        }
        for i in 0..3 {
            insert_agent_row(&conn, "speech", &format!("確認中です（{i}）"));
        }

        // 全文が入らない予算（＝コンパクション経路）。
        let out = build_conversation_string(&conn, SESSION, AGENT, 500).unwrap();
        assert!(
            out.contains("もういっそ全員フォローすればいいよ"),
            "直近のユーザー発言がプロンプトから落ちている: {out}"
        );
    }

    /// ユーザー発言が要約境界より前に落ちていても混ぜ戻される。
    #[test]
    fn user_speech_is_reinjected_from_before_the_summary_boundary() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_user_speech(&conn, "つらい");
        let user_log = last_log_id(&conn);
        for _ in 0..20 {
            insert_agent_row(&conn, "tool_result", &"x".repeat(400));
        }
        // 現セッションの topic 要約が user_log を含む範囲をカバーしている状態を作る。
        opencrab_db::queries::insert_index_node(
            &conn,
            &opencrab_db::queries::IndexNodeRow {
                id: "t1".to_string(),
                agent_id: AGENT.to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: "過去の話題".to_string(),
                summary: "過去の要約".to_string(),
                start_log_id: Some(1),
                end_log_id: Some(user_log + 10),
                source_session_id: Some(SESSION.to_string()),
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-07-31T00:00:00Z".to_string(),
                updated_at: "2026-07-31T00:00:00Z".to_string(),
                short_id: Some("t1".to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();

        let out = build_conversation_string(&conn, SESSION, AGENT, 400).unwrap();
        assert!(
            out.contains("つらい"),
            "要約境界より前のユーザー発言が混ぜ戻されていない: {out}"
        );
    }

    /// 保証するのは**直近 N 件**であって全件ではない（古い発言まで無条件に積むと
    /// 予算保証が壊れる）。
    #[test]
    fn only_the_most_recent_user_speeches_are_forced_in() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_user_speech(&conn, "とても古い発言マーカー");
        for i in 0..RECENT_MIN_USER_SPEECHES {
            insert_user_speech(&conn, &format!("新しい発言 {i}"));
        }
        for _ in 0..15 {
            insert_agent_row(&conn, "tool_result", &"y".repeat(400));
        }
        let out = build_conversation_string(&conn, SESSION, AGENT, 300).unwrap();
        assert!(
            !out.contains("とても古い発言マーカー"),
            "N 件を超えて古い発言まで強制的に載せている: {out}"
        );
        assert!(out.contains(&format!("新しい発言 {}", RECENT_MIN_USER_SPEECHES - 1)));
    }

    /// 予算に余裕があるときは従来どおり全文が出る（回帰防止）。
    #[test]
    fn full_conversation_is_unchanged_when_it_fits() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_user_speech(&conn, "こんにちは");
        insert_agent_row(&conn, "speech", "はい");
        let out = build_conversation_string(&conn, SESSION, AGENT, 100_000).unwrap();
        assert!(out.contains("こんにちは"));
        assert!(out.contains("はい"));
        assert!(!out.contains("omitted"));
    }

    /// **Discord/Nostr の行の形**（`record_inbound_message` 経由）でも直近ユーザー発言が
    /// 優先枠に入ること。fixture は本番と同じ書き手を使い、受信行が `agent_id`＝受信側 /
    /// `speaker_id`＝送信者（#377）で入ることも下で固定する。
    ///
    /// **このテストはもう #286 を pin していない**（識別力が下がった点は正直に書く）:
    /// #286 は「受信行が `agent_id == speaker_id` になり、列比較の述語
    /// `speaker_id != log.agent_id` が恒偽になる」バグだった。#377 で受信行が
    /// `agent_id != speaker_id`（受信側≠送信者）に直ったため、仮に述語を旧・列比較へ
    /// 戻しても "owner" != "a1" で真になり、**このテストは落ちない**。
    ///
    /// それでも無防備になった性質は無い: 行の形が直ったので列比較でも正しい答えになる
    /// （＝ #286 のバグ自体が成立しなくなった）。述語が引数比較であるべきことは
    /// `opencrab_core::conversation::is_user_speech` の doc とその近傍テストが担い、ここは「ゲートウェイ形状の
    /// 発言が必須枠に載る」という結果だけを固定する。
    #[test]
    fn gateway_shaped_rows_are_recognized_as_user_speech() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_user_speech(&conn, "この発言が消えたら対話が成立しない");
        // 予算を食い潰す巨大なツール往復（末尾の連続区間を占有する）。
        for _ in 0..30 {
            insert_agent_row(&conn, "tool_result", &"z".repeat(600));
        }
        // 受信行が本番と同じ形（agent_id 列＝受信側エージェント / speaker_id 列＝送信者、
        // #377）で入っていることを固定する。
        let (row_agent, row_speaker): (String, String) = conn
            .query_row(
                "SELECT agent_id, speaker_id FROM memory_sessions \
                 WHERE log_type = 'speech' AND speaker_id = ?1 LIMIT 1",
                [USER],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (row_agent.as_str(), row_speaker.as_str()),
            (AGENT, USER),
            "受信行は agent_id 列＝受信側エージェント / speaker_id 列＝送信者（#377）"
        );

        let out = build_conversation_string(&conn, SESSION, AGENT, 300).unwrap();
        assert!(
            out.contains("この発言が消えたら対話が成立しない"),
            "ゲートウェイ形状のユーザー発言が優先枠に入っていない: {out}"
        );
    }
}

/// 走行中ターンへ届ける新着発言の差分取得（#289）。
///
/// `SessionLiveInbound` の契約は 3 つ: (1) ターン開始後に記録された発言だけを返す、
/// (2) 一度返した発言は二度返さない、(3) エージェント自身の発言は返さない。
#[cfg(test)]
mod live_inbound_source_tests {
    use super::SessionLiveInbound;
    use opencrab_actions::transcript::{InboundMessageRecord, TranscriptSource};
    use opencrab_core::LiveInboundSource;

    const AGENT: &str = "a1";
    const USER: &str = "owner";
    const SESSION: &str = "s1";

    /// ユーザー発言を本番と同じ書き手（`record_inbound_message`）で入れる。
    /// この経路の行は `agent_id` 列＝受信側エージェント / `speaker_id` 列＝送信者（#377）。
    /// 述語を `speaker_id != <agent_id 引数>` に合わせてあること（#286）の検査でもある。
    fn insert_user_speech(db: &opencrab_db::Db, text: &str) {
        let conn = db.lock().unwrap();
        assert!(
            crate::transcript::record_inbound_message(
                &conn,
                TranscriptSource::Discord,
                &InboundMessageRecord {
                    session_id: SESSION,
                    recipient_agent_id: AGENT,
                    sender_id: USER,
                    sender_name: "owner",
                    avatar_url: None,
                    channel_id: Some("222"),
                    pubkey: None,
                    text,
                    image_urls: &[],
                },
            ),
            "テストの前提: 受信発言が記録できること"
        );
    }

    fn insert_agent_speech(db: &opencrab_db::Db, text: &str) {
        let conn = db.lock().unwrap();
        opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: SESSION.to_string(),
                log_type: "speech".to_string(),
                content: text.to_string(),
                speaker_id: Some(AGENT.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    /// ターン開始後に記録された発言が届く。走行中の注入はここから始まる。
    #[test]
    fn speech_recorded_during_the_turn_is_delivered() {
        let db = opencrab_db::Db::memory().unwrap();
        insert_user_speech(&db, "調べておいて");
        // ここまでが会話履歴に載っている状態でターンが始まる。
        let source = SessionLiveInbound::new(db.clone(), SESSION, AGENT);

        insert_user_speech(&db, "やめて");

        let out = source.poll_new_messages();
        assert_eq!(out.len(), 1, "新着 1 件だけ: {out:?}");
        assert!(out[0].contains("やめて"), "{}", out[0]);
        assert!(
            out[0].contains("処理している間に届きました"),
            "走行中に届いた事実を添える: {}",
            out[0]
        );
        assert!(
            !out[0].contains("調べておいて"),
            "履歴に載っている発言は再送しない: {}",
            out[0]
        );
    }

    /// 一度返した発言は二度返さない（毎イテレーション足すとプロンプトが膨らむ）。
    #[test]
    fn the_same_speech_is_delivered_once() {
        let db = opencrab_db::Db::memory().unwrap();
        let source = SessionLiveInbound::new(db.clone(), SESSION, AGENT);

        insert_user_speech(&db, "やめて");
        assert_eq!(source.poll_new_messages().len(), 1);
        assert!(
            source.poll_new_messages().is_empty(),
            "2 回目の poll では同じ発言を返さない"
        );

        // その後の新着はきちんと拾える（watermark が進んだだけで塞がっていない）。
        insert_user_speech(&db, "やっぱり続けて");
        let out = source.poll_new_messages();
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("やっぱり続けて"), "{}", out[0]);
    }

    /// 新着が無ければ何も返さない（＝プロンプトは 1 バイトも変わらない）。
    #[test]
    fn nothing_is_delivered_without_new_speech() {
        let db = opencrab_db::Db::memory().unwrap();
        insert_user_speech(&db, "調べておいて");
        let source = SessionLiveInbound::new(db.clone(), SESSION, AGENT);

        assert!(source.poll_new_messages().is_empty());
    }

    /// エージェント自身の発言は注入しない（自分の声が入力へ戻ると自己参照ループになる）。
    #[test]
    fn the_agents_own_speech_is_not_delivered() {
        let db = opencrab_db::Db::memory().unwrap();
        let source = SessionLiveInbound::new(db.clone(), SESSION, AGENT);

        insert_agent_speech(&db, "調べています");

        assert!(source.poll_new_messages().is_empty());
    }

    /// 特定の話者（pubkey）の受信発言を入れる（#323 / B2）。`speaker_id` 列に入る。
    fn insert_speech_from(db: &opencrab_db::Db, speaker: &str, text: &str) {
        let conn = db.lock().unwrap();
        assert!(crate::transcript::record_inbound_message(
            &conn,
            TranscriptSource::Nostr,
            &InboundMessageRecord {
                session_id: SESSION,
                recipient_agent_id: AGENT,
                sender_id: speaker,
                sender_name: speaker,
                avatar_url: None,
                channel_id: None,
                pubkey: Some(speaker),
                text,
                image_urls: &[],
            },
        ));
    }

    /// [#323 / B2] `OnlySpeaker` は返信中の相手の連投だけ注入し、別相手の新着は落とす。
    ///
    /// 1 セッションに全相手が同居する（#323）ため、無制限だと A への返信ターン中に B の
    /// 新着が注入され、B に答えた本文が A への返信として公開リレーへ誤爆する。修正前
    /// （`AllOthers` 相当のまま）はこのテストが落ちる（B の発言も返る）。
    #[test]
    fn only_speaker_scope_injects_only_the_replied_peer() {
        let db = opencrab_db::Db::memory().unwrap();
        let source = SessionLiveInbound::new(db.clone(), SESSION, AGENT).with_scope(
            opencrab_actions::LiveInboundScope::OnlySpeaker("pk-A".to_string()),
        );

        insert_speech_from(&db, "pk-A", "Aの追撃");
        insert_speech_from(&db, "pk-B", "Bの割り込み");

        let out = source.poll_new_messages();
        assert_eq!(out.len(), 1, "返信中の相手の連投だけ: {out:?}");
        assert!(out[0].contains("Aの追撃"), "{}", out[0]);
        assert!(
            !out.iter().any(|s| s.contains("Bの割り込み")),
            "別相手の新着は走行中に注入しない（公開リレーへの誤爆防止）: {out:?}"
        );
    }

    /// [#323 / B2] `Silent` は何も注入しない（resume = 生きた相手が不定）。
    #[test]
    fn silent_scope_injects_nothing() {
        let db = opencrab_db::Db::memory().unwrap();
        let source = SessionLiveInbound::new(db.clone(), SESSION, AGENT)
            .with_scope(opencrab_actions::LiveInboundScope::Silent);

        insert_speech_from(&db, "pk-A", "追撃");
        insert_speech_from(&db, "pk-B", "割り込み");

        assert!(
            source.poll_new_messages().is_empty(),
            "Silent は DB を引くまでもなく空"
        );
    }

    /// [#323 / B2] 既定（`AllOthers`）は従来どおり自分以外の全発言を注入する（非退行）。
    #[test]
    fn all_others_scope_injects_every_peer() {
        let db = opencrab_db::Db::memory().unwrap();
        let source = SessionLiveInbound::new(db.clone(), SESSION, AGENT);

        insert_speech_from(&db, "pk-A", "Aの発言");
        insert_speech_from(&db, "pk-B", "Bの発言");

        let out = source.poll_new_messages();
        assert_eq!(out.len(), 2, "全相手が注入される: {out:?}");
    }
}

/// #288 の強制（NO_REPLY 禁止 / 必ず返せ）がプロンプトから消えていること（#289）。
///
/// 方針は「届いているか」を直すことであって「答えるか」を縛ることではない。判断材料は
/// 与えてよいが、判断そのものはエージェントに委ねる。Bot ループ防止（元の意図）は残す。
#[cfg(test)]
mod no_forced_reply_tests {
    use super::build_agent_context;

    #[test]
    fn the_prompt_does_not_force_a_reply() {
        let conn = opencrab_db::init_memory().unwrap();
        let (prompt, _name) =
            build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);

        for forbidden in [
            "最優先の例外",
            "人間（Bot ではない送信者）があなたに宛てて発言した場合は",
            "If a human spoke to you after your last message",
            "This rule wins over 3.",
        ] {
            assert!(
                !prompt.contains(forbidden),
                "#288 の強制文言が残っている: {forbidden}"
            );
        }
    }

    /// ループ防止（Silent Reply の元の意図）は残るが、判断は相手の種別ではなく会話内容で
    /// 行わせる（#486・理念: システムは相手が bot か判定しない）。
    #[test]
    fn loop_prevention_survives_but_not_by_peer_type() {
        let conn = opencrab_db::init_memory().unwrap();
        let (prompt, _name) =
            build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);

        assert!(prompt.contains("## Silent Reply"), "prompt:\n{prompt}");

        // ループ防止は内容ベースで残る。
        assert!(
            prompt.contains("同じ話題の往復が続くだけで新しい情報を足せない場合"),
            "content-based loop prevention was lost:\n{prompt}"
        );

        // 「相手が Bot だから黙る」という種別ベースの沈黙条件は消えていること。
        assert!(
            !prompt.contains("他のBotが話している場合"),
            "peer-type silence condition still present:\n{prompt}"
        );
    }

    /// #900 / #890 PR2（§11.2）: モデル向け契約を新アーキテクチャに合わせる。
    /// 「result arrives later」はツール（query）起動の文脈にだけ現れ、発話は
    /// (i) fire-and-forget（結果が返らない）・(ii) 複数は 1 応答に並べる・(iii) 末尾 CONTINUE で継続
    /// （reply と併記可）、の 3 点を明示する。「## Continuing your turn」見出しと CONTINUE 指示も残る。
    #[test]
    fn system_prompt_explains_continue_marker() {
        let conn = opencrab_db::init_memory().unwrap();
        let (prompt, _name) =
            build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);

        // 注: prompt の prose は各行末 `\n\` が文中に literal 改行を入れる house style の
        // ため、assert は 1 行内に収まる断片で確認する（長い文全体を連続一致で見ない）。
        assert!(
            prompt.contains("## Continuing your turn"),
            "継続マーカーの見出しが system prompt に無い:\n{prompt}"
        );
        assert!(
            prompt.contains("on its own line"),
            "CONTINUE をその行単独で置く指示が無い:\n{prompt}"
        );
        assert!(
            prompt.contains("`CONTINUE`"),
            "CONTINUE トークンの言及が無い:\n{prompt}"
        );
        assert!(
            prompt.contains("Never promise to reply"),
            "「後で返す」を禁じる指示が無い:\n{prompt}"
        );

        // (i) 発話は fire-and-forget（結果が返らない）と明示する。
        assert!(
            prompt.contains("fire-and-forget")
                && prompt.contains("return a result and you are NOT called again"),
            "発話が fire-and-forget（結果が返らない）である説明が無い:\n{prompt}"
        );
        // (ii) 複数の発話は 1 応答にまとめて並べる。
        assert!(
            prompt.contains("put all the calls in ONE response"),
            "複数発話を 1 応答に並べる指示が無い:\n{prompt}"
        );
        // (iii) CONTINUE は reply と併記できる。
        assert!(
            prompt.contains("place it alongside a reply"),
            "CONTINUE を reply と併記できる説明が無い:\n{prompt}"
        );

        // 「result arrives later」はツール（query）起動の文脈にだけ現れる。旧「When you call a
        // tool, the result arrives later」という発話も含む包括表現は撤去されていること。
        assert!(
            prompt.contains("the result arrives later and you are called again"),
            "ツール結果が後で届く説明（query 文脈）が失われている:\n{prompt}"
        );
        assert!(
            !prompt.contains("When you call a tool, the result arrives later"),
            "旧・発話も含む包括的な非同期説明が残っている:\n{prompt}"
        );

        // #909: 平文は 1 応答 = 1 投稿。別々に複数の平文を投稿するなら各応答末尾に CONTINUE。
        // （reply×N in one response の対。実モデルが CONTINUE を使わず 1 投稿 3 段落で返した対策）。
        assert!(
            prompt.contains("Plain text in a response is posted as ONE message"),
            "平文は 1 応答 = 1 投稿の説明が無い（#909）:\n{prompt}"
        );
        assert!(
            prompt.contains("To post several plain messages separately"),
            "別々の平文投稿は各応答末尾 CONTINUE の説明が無い（#909）:\n{prompt}"
        );
    }
}

#[cfg(test)]
mod past_summary_notice_contract_tests {
    /// 告知が名指しするツールと引数が、実在するツールの実在するパラメータと一致すること。
    ///
    /// 本 PR で 2,400 件級の要約が文脈から消えるので、告知の 1 行が唯一の復旧導線になる。
    /// **「失われていません、こう引けます」と書いて実際には引けない**のが最悪の壊れ方で、
    /// 実際に一度そう書いていた（`retrieve_memory_nodes` に keyword / date range を渡せ、と
    /// 書いたが、このツールは `node_ids` しか受け取らず、日付範囲を取る記憶検索ツールは
    /// 存在しない）。文言とツール定義を突き合わせて固定する。
    #[test]
    fn omitted_notice_matches_the_real_tool_surface() {
        use opencrab_actions::memory_access::{RetrieveMemoryNodesAction, SearchMemoryIndexAction};
        use opencrab_actions::Action;

        let notice = super::past_summary_omitted_notice(42);
        let search = SearchMemoryIndexAction;
        let retrieve = RetrieveMemoryNodesAction;
        let props = |a: &dyn Action| -> Vec<String> {
            a.parameters()["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{} の parameters に properties が無い", a.name()))
                .keys()
                .cloned()
                .collect()
        };

        // 名指ししたツールは実在する（名前はツール定義から取る）。
        assert!(
            notice.contains(search.name()) && notice.contains(retrieve.name()),
            "告知が実在するツールを名指ししていない: {notice}"
        );
        // 渡せと書いた引数を、そのツールが実際に受け取る。
        assert!(
            notice.contains(&format!("{}(query)", search.name())),
            "検索の呼び方が書かれていない: {notice}"
        );
        assert!(
            props(&search).contains(&"query".to_string()),
            "{} は query を受け取らない: {:?}",
            search.name(),
            props(&search)
        );
        // retrieve_memory_nodes は node_ids しか受け取らない。告知はこのツールへ
        // キーワードや日付を渡すよう指示してはならない（＝名前より後ろに出さない）。
        assert_eq!(
            props(&retrieve),
            vec!["node_ids".to_string()],
            "retrieve_memory_nodes のパラメータが変わった。告知の文言を見直すこと"
        );
        let after = &notice[notice.find(retrieve.name()).unwrap()..];
        for forbidden in ["keyword", "date range", "date_range", "query"] {
            assert!(
                !after.contains(forbidden),
                "retrieve_memory_nodes に {forbidden} を渡すよう読める: {notice}"
            );
        }
    }
}

#[cfg(test)]
mod steer_inbound_tests {
    use super::SubtaskSteerInbound;
    use opencrab_core::LiveInboundSource;

    fn insert_log(db: &opencrab_db::Db, session_id: &str, log_type: &str, content: &str) {
        let conn = db.lock().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: String::new(),
            session_id: session_id.to_string(),
            log_type: log_type.to_string(),
            content: content.to_string(),
            speaker_id: None,
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log_best_effort(&conn, &log);
    }

    /// #647: steer ログだけを差分注入し、同じ steer を二度返さない（watermark）。通常発話や
    /// system ログは拾わない。watermark 初期値は 0（セッション先頭）なので、source 構築の
    /// 直前に届いた steer も取りこぼさない（「Accepted なのに読まれない」窓を閉じる）。
    #[test]
    fn polls_only_new_steer_logs_and_dedups() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let sub = "subtask-abc";

        // source 構築の**前**に届いた steer も、watermark=0 なので初回 poll で拾える
        // （spawn 直後〜engine 準備完了の窓での消失を防ぐ / レビュー指摘 1）。
        insert_log(&db, sub, opencrab_actions::STEER_LOG_TYPE, "早い steer");
        let src = SubtaskSteerInbound::new(db.clone(), sub);

        // 構築後にもう 1 本届く。
        insert_log(&db, sub, opencrab_actions::STEER_LOG_TYPE, "JSON で出して");
        // 別 log_type は拾わない。
        insert_log(&db, sub, "speech", "これは発話（対象外）");
        insert_log(&db, sub, "system", "これは system（対象外）");

        let first = src.poll_new_messages();
        assert_eq!(first.len(), 2, "構築前後の steer を両方拾う");
        assert!(
            first[0].contains("早い steer"),
            "構築前の steer も取りこぼさない"
        );
        assert!(first[1].contains("JSON で出して"), "本文が載る");
        assert!(first[0].contains("追加指示"), "steer と分かる整形が付く");

        // 2 回目は新着なし（watermark が進んでいる）。
        assert!(
            src.poll_new_messages().is_empty(),
            "同じ steer を二度注入しない"
        );

        // さらに届いた steer は次の poll で拾う。
        insert_log(&db, sub, opencrab_actions::STEER_LOG_TYPE, "2 通目");
        let third = src.poll_new_messages();
        assert_eq!(third.len(), 1);
        assert!(third[0].contains("2 通目"));
    }
}
