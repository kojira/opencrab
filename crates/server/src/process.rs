//! エージェントのメッセージ処理に関する共通ロジック。
//!
//! REST API (`api/sessions.rs`) と Discordゲートウェイ (`discord.rs`) の
//! 両方から利用される。

use std::sync::Arc;

use opencrab_core::tokens::estimate_tokens;
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
    resolve_run_tools_config(state, agent_id)
        .shell
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
    let curated_categories = ["long_term", "user_profile", "agent_rules"];
    let curated_sections: Vec<String> = curated_categories
        .iter()
        .filter_map(|cat| {
            let memories =
                opencrab_db::queries::get_curated_memories(conn, agent_id, cat).unwrap_or_default();
            if memories.is_empty() {
                return None;
            }
            let content = memories
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
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
         - 他のBotが話している場合（Bot同士のループを防ぐ）。ただし例外が2つ: \
         (1) メッセージが {req_marker} で始まる場合はレビュアーとして応答する、\
         (2) 自分が依頼したレビューへの {reply_marker} で始まる返信は記録・対応する。\
         いずれも下記 Peer Review セクションに従うこと\n\
         - 既に話が完結している場合\n\
         \n\
         ## Async Behavior\n\
         \n\
         You work asynchronously. When you call a tool, the result arrives later — and you\n\
         are called again with the result in the conversation history.\n\
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

/// `provider:model` 形式（またはモデル名のみ）を pricing 参照用に分割する。
pub fn split_llm_model_spec(full: &str) -> (&str, &str) {
    if let Some(i) = full.find(':') {
        (&full[..i], &full[i + 1..])
    } else {
        ("", full)
    }
}

/// コンパクション時に最低限保持する最近のログ件数。
const RECENT_MIN_LOGS: usize = 10;
/// コンパクション時に**必ず**保持する直近ユーザー発言の件数（#284）。
///
/// `RECENT_MIN_LOGS` は「直近 N 件のログ」しか保証しない。ツール往復が走ると
/// 直近 10 件が tool_call / tool_result だけで埋まり、ユーザー発言が 1 件も
/// プロンプトに載らないまま応答する（= #284 の事故）。ログ種別に関係なく
/// 「直近のユーザー発言 N 件」を別枠で確保し、予算配分でも最優先で取る。
///
/// 5 件の根拠: 実例では直近 10 件が tool_result 5 + evaluation 2 + 自分の発言 3 で
/// 埋まり、ユーザー発言が 0 件になっていた。ユーザーは指示を短文で連投する
/// （「全員フォローして」「無視？」「つらい」）ため、1〜2 件では直前の言い直しだけを
/// 拾って元の指示を落とす。5 件なら一連の連投をまたいで意図が読める。
const RECENT_MIN_USER_SPEECHES: usize = 5;
/// context_window が不明な場合のデフォルト予算（トークン数）。
const DEFAULT_CONTEXT_BUDGET_TOKENS: usize = 100_000;

/// セッションログから会話文字列を構築する（トークン予算ベースのコンパクション対応）。
///
/// `context_budget_tokens` はこの会話セクションに使えるトークン予算。
/// 全文が予算内ならそのまま返す。超えたら memory_index の topic 要約で古い部分を置き換え、
/// 最近のログを予算内で最大限保持する。
pub fn build_conversation_string(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    context_budget_tokens: usize,
) -> Result<String, anyhow::Error> {
    // タスク台帳（前向きワーキング状態）を会話の先頭に前置する。
    // system prompt 側は 1h キャッシュされるため、毎ターン変わる台帳状態はここに置く。
    // 台帳の読み出し失敗で返信自体を殺さない（warn して台帳なしで続行）。
    let ledger_section =
        match opencrab_core::task_ledger::build_ledger_section(conn, agent_id, session_id) {
            Ok(section) => section,
            Err(e) => {
                tracing::warn!("failed to build task ledger section for session {session_id}: {e}");
                None
            }
        };

    // [Memory Index]: 長期記憶のコンパクトな目次を常時前置する（月次要約 + 現在月
    // topic、short_id 付き）。台帳と同じく「動的状態は会話側」（system は 1h
    // キャッシュ）。best-effort — 失敗しても返信は殺さない。
    // コンパクション時の [Past context summary]（build_conversation_inner 内、
    // 現セッションの topic のみ）とは役割が異なり、こちらは現セッション由来の
    // topic を除外するため short_id が両方に出ることはない（invariant）。
    let memory_index_section =
        match opencrab_core::memory_index::build_memory_index_section(conn, agent_id, session_id) {
            Ok(section) => section,
            Err(e) => {
                tracing::warn!(
                    "failed to build memory index section for session {session_id}: {e}"
                );
                None
            }
        };
    // 予算比ガード: セクションはフルサイズで ~2.5k tokens（日本語 ≈0.7 tok/char）に
    // なりうる。小さいコンテキスト予算（小型モデル）では会話本文を圧迫するため、
    // 予算の 1/4 を超えるなら注入しない（100k 級の既定予算では常に通る）。
    let memory_index_section = memory_index_section.filter(|s| {
        let cost = estimate_tokens(s);
        if cost * 4 > context_budget_tokens {
            tracing::debug!(
                session_id = %session_id,
                section_tokens = cost,
                budget = context_budget_tokens,
                "skipping [Memory Index] section: exceeds 1/4 of context budget"
            );
            false
        } else {
            true
        }
    });

    // [Impressions]: いま話している相手の人物像（#314）。人物像は agent スコープ
    // （経路をまたいで同じ相手なら同じ 1 行）だが、**載せるのは直近の発話者の分だけ**で、
    // 人数もフィールド長もビルダ側で上限が掛かっている。台帳・memory index と同じく
    // best-effort — 読み出しに失敗しても返信は殺さない。
    let impression_section = match opencrab_core::impression_section::build_impression_section(
        conn, agent_id, session_id,
    ) {
        Ok(section) => section,
        Err(e) => {
            tracing::warn!("failed to build impression section for session {session_id}: {e}");
            None
        }
    };

    let mut inner_budget = context_budget_tokens;
    for section in [&ledger_section, &memory_index_section, &impression_section]
        .into_iter()
        .flatten()
    {
        inner_budget = inner_budget.saturating_sub(estimate_tokens(section));
    }
    let inner = build_conversation_inner(conn, session_id, agent_id, inner_budget)?;

    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = ledger_section {
        parts.push(s);
    }
    if let Some(s) = memory_index_section {
        parts.push(s);
    }
    if let Some(s) = impression_section {
        parts.push(s);
    }
    parts.push(inner);
    Ok(parts.join("\n\n"))
}

/// 会話文字列本体の構築（タスク台帳の前置は `build_conversation_string` 側で行う）。
fn build_conversation_inner(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    context_budget_tokens: usize,
) -> Result<String, anyhow::Error> {
    // まず全文を試す
    let full = build_full_conversation(conn, session_id);
    if full == "No messages yet." {
        return Ok(full);
    }

    // 全文が予算内ならそのまま返す
    if estimate_tokens(&full) <= context_budget_tokens {
        return Ok(full);
    }

    // 予算超過 → コンパクション
    // memory_index の topic 要約を取得
    let topics = match opencrab_db::queries::get_topic_nodes_for_session(conn, agent_id, session_id)
    {
        Ok(t) => t,
        Err(e) => {
            return Err(anyhow::anyhow!(
                "Failed to get topic nodes for session {session_id}: {e}"
            ));
        }
    };

    if topics.is_empty() {
        // フォールバック: 要約がない場合は最新ログを予算内で切り詰め
        return Ok(build_truncated_conversation(
            conn,
            session_id,
            agent_id,
            context_budget_tokens,
        ));
    }

    // [Past context summary] セクション構築
    // node_id を併記してエージェントが retrieve_memory_nodes で全文検索できるようにする
    let summary_section: String = topics
        .iter()
        .map(|t| {
            let key = t.short_id.as_deref().unwrap_or(&t.id);
            let date_hint = match (t.date_from.as_deref(), t.date_to.as_deref()) {
                (Some(from), Some(to)) if from == to => format!(" ({})", &from[5..]),
                (Some(from), Some(to)) => format!(" ({}~{})", &from[5..], &to[5..]),
                (Some(from), None) => format!(" ({})", &from[5..]),
                _ => String::new(),
            };
            format!("- [{}]{} {}: {}", key, date_hint, t.title, t.summary)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let summary_header =
        "[Past context summary (use retrieve_memory_nodes with short_id to recall details)]\n";
    let recent_header = "\n\n[Recent conversation]\n";
    let overhead_tokens = estimate_tokens(summary_header)
        + estimate_tokens(&summary_section)
        + estimate_tokens(recent_header);

    // 残りの予算を最近のログに割り当て
    let remaining_budget = context_budget_tokens.saturating_sub(overhead_tokens);

    // indexed_boundary: topic でカバーされている最後の log_id
    let indexed_boundary = topics
        .iter()
        .filter_map(|t| t.end_log_id)
        .max()
        .unwrap_or(0);

    // indexed_boundary 以降のログを取得
    let mut recent_logs = match opencrab_db::queries::list_session_logs_after_id(
        conn,
        session_id,
        indexed_boundary,
    ) {
        Ok(logs) => retain_conversation_logs(logs),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "Failed to list session logs after id for session {session_id}: {e}"
            ));
        }
    };

    // ログが少なければ追加取得（最低 RECENT_MIN_LOGS 件は確保）
    if recent_logs.len() < RECENT_MIN_LOGS {
        let mut logs =
            match opencrab_db::queries::list_recent_session_logs(conn, session_id, RECENT_MIN_LOGS)
            {
                Ok(l) => l,
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Failed to list recent session logs for session {session_id}: {e}"
                    ));
                }
            };
        logs.reverse();
        recent_logs = retain_conversation_logs(logs);
    }

    // #284: 直近のユーザー発言が要約境界より前に落ちていても必ず混ぜ戻す。
    let recent_logs = merge_recent_user_speeches(conn, session_id, agent_id, recent_logs);

    // 予算内に収まるようにログを後ろから詰める
    let recent_text = fit_logs_to_budget(&recent_logs, agent_id, remaining_budget);

    Ok(format!(
        "{summary_header}{summary_section}{recent_header}{recent_text}"
    ))
}

/// context_budget_tokens を呼び出し元で計算するヘルパー。
/// model_pricing の context_window と compaction_ratio から予算を算出する。
pub fn compute_context_budget(
    conn: &rusqlite::Connection,
    provider: &str,
    model: &str,
    compaction_ratio: f64,
) -> usize {
    let context_window = opencrab_db::queries::get_model_pricing(conn, provider, model)
        .ok()
        .flatten()
        .and_then(|p| p.context_window)
        .unwrap_or(DEFAULT_CONTEXT_BUDGET_TOKENS as i32) as usize;
    ((context_window as f64) * compaction_ratio) as usize
}

/// 会話文字列から除外する log_type か（#291）。
///
/// `evaluation` は evaluator（別 context の採点者）が書く行で、**エージェント本人の
/// 発話でも相手の発話でもない**。これを会話へ混ぜると、採点結果とその指示文が人間の
/// 発言と同じ土俵に並び、直前のユーザー発言より採点の圧が勝ってしまう（#291 の実害）。
/// 過去に記録済みの行も会話には出さないため、書き込み側を止めるだけでなく読み出し側
/// でも落とす。台帳や記憶など「本人が見に行く場所」に置くのは妨げない。
fn is_excluded_from_conversation(log: &opencrab_db::queries::SessionLogRow) -> bool {
    log.log_type == "evaluation"
}

/// 会話文字列に載せるログだけを残す（#291）。
fn retain_conversation_logs(
    logs: Vec<opencrab_db::queries::SessionLogRow>,
) -> Vec<opencrab_db::queries::SessionLogRow> {
    logs.into_iter()
        .filter(|l| !is_excluded_from_conversation(l))
        .collect()
}

fn build_full_conversation(conn: &rusqlite::Connection, session_id: &str) -> String {
    let logs = match opencrab_db::queries::list_session_logs_by_session(conn, session_id) {
        Ok(l) => retain_conversation_logs(l),
        Err(e) => {
            tracing::warn!(session_id = %session_id, "Failed to list session logs: {e}");
            return "No messages yet.".to_string();
        }
    };
    if logs.is_empty() {
        return "No messages yet.".to_string();
    }
    // #272 P1: どの範囲のログが会話文字列に入ったかを後追いできるようにする。
    // 会話文字列そのものは秘匿・肥大のため出さず、件数と最古/最新 log_id のみ。
    tracing::debug!(
        session_id = %session_id,
        log_count = logs.len(),
        oldest_log_id = ?logs.first().and_then(|l| l.id),
        newest_log_id = ?logs.last().and_then(|l| l.id),
        "build_full_conversation: logs included"
    );
    format_logs(&logs)
}

fn build_truncated_conversation(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    budget_tokens: usize,
) -> String {
    let mut logs = match opencrab_db::queries::list_recent_session_logs(conn, session_id, 500) {
        Ok(l) => retain_conversation_logs(l),
        Err(e) => {
            tracing::warn!(session_id = %session_id, "Failed to list recent session logs for truncation: {e}");
            vec![]
        }
    };
    logs.reverse();
    // #284: 500 件の窓から溢れていてもユーザー発言だけは必ず含める。
    let logs = merge_recent_user_speeches(conn, session_id, agent_id, logs);

    let header = "[Note: Earlier messages were omitted due to context length. Showing most recent messages.]\n\n";
    let header_tokens = estimate_tokens(header);
    let remaining = budget_tokens.saturating_sub(header_tokens);
    let recent_text = fit_logs_to_budget(&logs, agent_id, remaining);

    format!("{header}{recent_text}")
}

/// 中略した区間に差し込む印（会話が連続していないことを LLM に明示する）。
const OMITTED_MARKER: &str = "[... older messages omitted due to context length ...]";

/// エージェント自身ではない話者の発言か（= ユーザー／他エージェントの生発言）。
///
/// **判定は行の `agent_id` 列ではなく、`agent_id` 引数（＝ 応答するエージェント）と
/// `speaker_id` を比べること**（#286）。DB 側の `list_recent_user_speech_logs` も
/// 最初から `speaker_id != <agent_id 引数>` で比較しており、2 つの述語は必ず一致させる
/// こと（片方だけ変えると、混ぜ戻した行がここで捨てられて元の症状に戻る）。
///
/// なぜ行の `agent_id` 列を見ないか（#286 の経緯）: 当時ゲートウェイ受信の行は
/// `agent_id` 列にも**送信者 ID** が入り（`agent_id == speaker_id`）、行内 2 列の
/// 突き合わせでは Discord / Nostr の受信行でこの述語が恒偽になった。実際それで #284 の
/// 保証が本番経路で丸ごと no-op だった（当時の該当 4,490 件すべてが `==`）。#377 で
/// 受信行は `agent_id`＝受信側 / `speaker_id`＝送信者 に直ったので列は縮退しなくなったが、
/// **述語は引き続き `speaker_id` と `agent_id` 引数で比べる**（行の `agent_id` 列は無関係）。
fn is_user_speech(log: &opencrab_db::queries::SessionLogRow, agent_id: &str) -> bool {
    log.log_type == "speech" && log.speaker_id.as_deref().is_some_and(|s| s != agent_id)
}

/// 直近のユーザー発言をログ列へ混ぜ戻す（#284）。
///
/// `logs` は「要約境界より後ろ」や「直近 N 件」で切られているため、ツール往復が
/// 長引くとユーザーの生発言が 1 件も入らないことがある。セッション全体から直近
/// `RECENT_MIN_USER_SPEECHES` 件のユーザー発言を取り、id で重複排除して時系列へ
/// マージする。取得に失敗しても会話構築は続行する（best-effort）。
fn merge_recent_user_speeches(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    mut logs: Vec<opencrab_db::queries::SessionLogRow>,
) -> Vec<opencrab_db::queries::SessionLogRow> {
    let speeches = match opencrab_db::queries::list_recent_user_speech_logs(
        conn,
        session_id,
        agent_id,
        RECENT_MIN_USER_SPEECHES,
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(session_id = %session_id, "failed to load recent user speeches: {e}");
            return logs;
        }
    };
    let present: std::collections::HashSet<i64> = logs.iter().filter_map(|l| l.id).collect();
    let mut added = 0usize;
    for s in speeches {
        match s.id {
            Some(id) if !present.contains(&id) => {
                logs.push(s);
                added += 1;
            }
            _ => {}
        }
    }
    if added > 0 {
        tracing::info!(
            session_id = %session_id,
            added,
            "re-injected recent user speeches that fell outside the recent-log window"
        );
        // id 未設定の行（テスト等）は末尾に寄せる。
        logs.sort_by_key(|l| l.id.unwrap_or(i64::MAX));
    }
    logs
}

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

/// ログを末尾（最新）から逆順に辿り、予算内に収まる分だけ返す。
///
/// 保証は 2 つ:
/// - 最低 `RECENT_MIN_LOGS` 件は常に含める（従来どおり）。
/// - 直近 `RECENT_MIN_USER_SPEECHES` 件のユーザー発言は**予算より先に枠を取り**、
///   末尾の連続区間から外れていても必ず含める（#284）。巨大なツール結果が
///   末尾を占めてもユーザーの指示は消えない。
fn fit_logs_to_budget(
    logs: &[opencrab_db::queries::SessionLogRow],
    agent_id: &str,
    budget_tokens: usize,
) -> String {
    if logs.is_empty() {
        return String::new();
    }

    // まず各ログを文字列化
    let formatted: Vec<String> = logs.iter().map(format_single_log).collect();
    let line_tokens: Vec<usize> = formatted
        .iter()
        .map(|line| estimate_tokens(line) + 1) // +1 for newline
        .collect();

    // #284: 直近のユーザー発言を必須枠として先に確保する。
    let must: std::collections::BTreeSet<usize> = logs
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, log)| is_user_speech(log, agent_id))
        .take(RECENT_MIN_USER_SPEECHES)
        .map(|(i, _)| i)
        .collect();
    let must_tokens: usize = must.iter().map(|&i| line_tokens[i]).sum();

    // 残り予算で末尾から詰めていく
    let tail_budget = budget_tokens.saturating_sub(must_tokens);
    let mut used_tokens = 0;
    let mut start_idx = formatted.len();

    for i in (0..formatted.len()).rev() {
        if must.contains(&i) {
            // 予算確保済み。ここまでは連続区間として取り込む。
            start_idx = i;
            continue;
        }
        if used_tokens + line_tokens[i] > tail_budget
            && (formatted.len() - start_idx) >= RECENT_MIN_LOGS
        {
            break;
        }
        used_tokens += line_tokens[i];
        start_idx = i;
    }

    // 連続区間 + それより古い必須ユーザー発言を時系列で結合する。
    let mut selected: Vec<usize> = must.iter().copied().filter(|&i| i < start_idx).collect();
    selected.extend(start_idx..formatted.len());

    let mut parts: Vec<String> = Vec::with_capacity(selected.len());
    let mut prev: Option<usize> = None;
    for i in selected {
        if prev.is_some_and(|p| i > p + 1) {
            parts.push(OMITTED_MARKER.to_string());
        }
        parts.push(formatted[i].clone());
        prev = Some(i);
    }
    parts.join("\n")
}

fn format_logs(logs: &[opencrab_db::queries::SessionLogRow]) -> String {
    logs.iter()
        .map(format_single_log)
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_single_log(log: &opencrab_db::queries::SessionLogRow) -> String {
    let ts = log
        .created_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.format(" [%Y-%m-%d %H:%M:%S]").to_string())
        .unwrap_or_default();

    match log.log_type.as_str() {
        "speech" => {
            let speaker = log.speaker_id.as_deref().unwrap_or(&log.agent_id);
            format!("[{}]{}:\n{}", speaker, ts, log.content)
        }
        "tool_call" => {
            let speaker = log.speaker_id.as_deref().unwrap_or(&log.agent_id);
            if let Some(meta_json) = log.metadata_json.as_deref() {
                if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_json) {
                    if let Some(tool_calls_json) =
                        meta.get("tool_calls_json").and_then(|v| v.as_str())
                    {
                        if let Ok(tool_calls) =
                            serde_json::from_str::<serde_json::Value>(tool_calls_json)
                        {
                            if let Some(items) = tool_calls.as_array() {
                                let call_lines: Vec<String> = items
                                    .iter()
                                    .filter_map(|item| {
                                        let id = item.get("id")?.as_str()?;
                                        // 正準形状 {function:{name, arguments:"<json-string>"}} と
                                        // 旧形状 {name, arguments:<object>} の両方に対応する。
                                        let (name, args) = if let Some(func) = item.get("function")
                                        {
                                            let name = func.get("name")?.as_str()?;
                                            let args = func
                                                .get("arguments")
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.to_string())
                                                .unwrap_or_default();
                                            (name, args)
                                        } else {
                                            let name = item.get("name")?.as_str()?;
                                            let args = item
                                                .get("arguments")
                                                .map(|value| value.to_string())
                                                .unwrap_or_default();
                                            (name, args)
                                        };
                                        Some(format!("[id={}]: {}({})", id, name, args))
                                    })
                                    .collect();
                                if !call_lines.is_empty() {
                                    return format!(
                                        "[{}]{}:\n[tool_call]:\n{}",
                                        speaker,
                                        ts,
                                        call_lines.join("\n")
                                    );
                                }
                            }
                        }
                    }
                }
            }
            format!("[{}]{}:\n[tool_call]:\n{}", speaker, ts, log.content)
        }
        "tool_result" => {
            let meta = log
                .metadata_json
                .as_deref()
                .and_then(|meta_json| serde_json::from_str::<serde_json::Value>(meta_json).ok());
            let tool_call_id = meta
                .as_ref()
                .and_then(|value| value.get("tool_call_id").and_then(|v| v.as_str()))
                .unwrap_or("?");
            let tool_name = meta
                .as_ref()
                .and_then(|value| value.get("tool_name").and_then(|v| v.as_str()))
                .unwrap_or("unknown");
            format!(
                "[tool_result]{}:\n[id={}]: {} → {}",
                ts, tool_call_id, tool_name, log.content
            )
        }
        "tool_cancelled" => {
            let meta = log
                .metadata_json
                .as_deref()
                .and_then(|meta_json| serde_json::from_str::<serde_json::Value>(meta_json).ok());
            let tool_call_id = meta
                .as_ref()
                .and_then(|value| value.get("tool_call_id").and_then(|v| v.as_str()))
                .unwrap_or("?");
            let tool_name = meta
                .as_ref()
                .and_then(|value| value.get("tool_name").and_then(|v| v.as_str()))
                .unwrap_or("unknown");
            format!(
                "[tool_cancelled]{}:\n[id={}]: {} がキャンセルされた\n{}",
                ts, tool_call_id, tool_name, log.content
            )
        }
        "system" => {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&log.content) {
                if let Some(kind) = value.get("type").and_then(|v| v.as_str()) {
                    let content = serde_json::to_string_pretty(&value)
                        .unwrap_or_else(|_| log.content.clone());
                    return format!("[system: {}]{}:\n{}", kind, ts, content);
                }
            }
            format!("[system]{}:\n{}", ts, log.content)
        }
        other => format!("[{}]{}:\n{}", other, ts, log.content),
    }
}

/// 変動コンテキストを最後のuserメッセージに前置するヘルパー（実体は
/// [`opencrab_core::runtime_context`] / #190 S2）。
///
/// 純関数なので下位層へ移した。ゲートウェイ側のクレート（`opencrab-web-gateway` 等）が
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

        let log_row = opencrab_db::queries::LlmLogRow {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: log_agent_id.clone(),
            session_id: Some(log_session_id.clone()),
            model: Some(log.request.model.clone()),
            prompt: serde_json::to_string(&log.request).unwrap_or_default(),
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
            error_code: log.error_str.as_ref().map(|_| "error".to_string()),
            error_body: log.error_str.clone(),
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

/// ターンの tool_call / tool_result を session_logs に記録するコールバックの配線
/// （#33: 段の分解。tool_result はサイズ上限超過時にワークスペースへ退避）。
fn set_turn_log_callbacks(
    engine: &mut opencrab_core::SkillEngine,
    db: opencrab_db::Db,
    agent_id: String,
    session_id: String,
    tool_result_workspace: std::path::PathBuf,
) {
    {
        let tc_db = db.clone();
        let tc_agent = agent_id.clone();
        let tc_session = session_id.clone();
        engine.set_on_tool_call(move |content: String, tool_calls_json: String| {
            if let Ok(conn) = tc_db.lock() {
                // LLMがtext+tool_callsを同時に返した場合、textをspeechとして記録する
                if !content.trim().is_empty() {
                    let speech_log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: tc_agent.clone(),
                        session_id: tc_session.clone(),
                        log_type: "speech".to_string(),
                        content: content.clone(),
                        speaker_id: Some(tc_agent.clone()),
                        turn_number: None,
                        metadata_json: None,
                        created_at: None,
                    };
                    if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &speech_log) {
                        tracing::error!(agent_id = %tc_agent, session_id = %tc_session, "Failed to insert speech log (with tool_call): {e}");
                    }
                }
                let log = opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: tc_agent.clone(),
                    session_id: tc_session.clone(),
                    log_type: "tool_call".to_string(),
                    content,
                    speaker_id: Some(tc_agent.clone()),
                    turn_number: None,
                    metadata_json: Some(
                        serde_json::json!({"tool_calls_json": tool_calls_json}).to_string(),
                    ),
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
        engine.set_on_tool_result(
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

/// エージェントにメッセージを処理させ、応答テキストを返す。
///
/// SkillEngine + BridgedExecutor + LlmRouterAdapter のフルパイプラインを実行する。
/// 実行要求は `RunRequest`（#33: 13位置引数の置き換え）で受ける。
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
    let mut dispatcher = opencrab_actions::ActionDispatcher::new();
    // このエージェント専用の許可コマンド（DB管理）を、ローカルコピーにのみマージする。
    // グローバル config に足すと他エージェントにも許可が漏れる（per-agent スコープの担保）。
    let tools_cfg = resolve_run_tools_config(state, agent_id);
    // 登録はここで **スナップショット**される（`register_tools_from_config` は
    // `ShellToolConfig` を clone して `ShellToolAction` に持たせる）。したがって
    // 走行中に設定を書き換えても本ターンのツールには届かない。許可コマンドの
    // 追加・削除が効くのは常に**次の run** からである（#202 の根拠）。
    opencrab_actions::register_tools_from_config(&tools_cfg, &mut dispatcher);
    // MCP の trusted_only サーバは信頼された呼び出し元のターンでのみ露出する。
    let caller_is_trusted = matches!(
        ctx.caller,
        opencrab_actions::CallerIdentity::Owner
            | opencrab_actions::CallerIdentity::CoAgent { .. }
            | opencrab_actions::CallerIdentity::TrustedUser
    );
    // 走行中 subtask の共有 registry を **1 度だけ**解決する。`SystemGatewayActions`
    // （cancel_subtask / report_progress）と自動 dispatcher、そして `spawn_subtask`
    // （#175 S4）が同一 Arc を見ることで「停止の到達性」が保たれる。呼び出し側が
    // registry を渡さなかった場合も、この run 内では全員が同じフレッシュな registry を
    // 共有する（以前は dispatcher だけがフレッシュ生成し、cancel が not found になった）。
    let subtask_registry: opencrab_actions::SubtaskRegistry = req
        .subtask_registry
        .clone()
        .unwrap_or_else(|| std::sync::Arc::new(dashmap::DashMap::new()));

    let executor = {
        // inbound の返信先（gateway 不透明 token / #167）をツール実行の文脈
        // （`GatewayCallContext.reply_target`）まで運ぶ（#158 S1）。宛先を引数で受ける
        // gateway アクションが、引数省略時のフォールバックとして読む。
        let bridged = opencrab_actions::BridgedExecutor::new(dispatcher, ctx)
            .with_depth(depth)
            .with_reply_target(req.reply_target.clone())
            // この run のツール許可リスト（#368）。`Some` のときだけ有効で、可視性
            // （`list_tools`）と実行（`dispatch_inner`）の両方を、全スロット
            // （dispatcher / gateway own = `SystemGatewayActions` / MCP）にわたって
            // 許可リスト内に絞る。既存 caller/depth ゲートの**上乗せ**。sleep 整理ラン
            // だけが渡し、他ターン（`None`）は従来どおり無制限。
            .with_tool_allowlist(req.tool_allowlist.clone());
        // サーバ内設定ツール（configure_llm_provider 等）を transport 非依存で全ターンに
        // 供給する。既存 gateway（Discord/Nostr）は inner として委譲される（composite）。
        // owner 限定ツールは bridge の OWNER_ONLY_ACTIONS が可視性/実行を強制する。
        // 共有 registry を neutral 層の cancel_subtask（#161）へ配線する。dispatcher が
        // 使う registry と同一 Arc を渡すことで、auto-dispatch された subtask を
        // cancel_subtask で停止できる（Discord では gateway_actions の registry とも同一）。
        let system_actions: std::sync::Arc<dyn opencrab_gateway::GatewayActions> =
            std::sync::Arc::new(crate::system_actions::SystemGatewayActions::new(
                state.clone(),
                req.gateway_actions,
                Some(subtask_registry.clone()),
                // 停止も 1 箇所（neutral な cancel_subtask）から sink へ通知する。停止は
                // `on_subtask_cancelled`（既定 no-op）なので resume する sink の挙動は
                // 変わらず、REST だけがセッション状態の整合を取る。
                req.completion_sink.clone(),
            ));
        // depth >= 1（sub-engine）は許可リストで最外周を絞る（#63 / RFC #152 S2）。
        // **合成後**（server ツール + transport の union）に被せるのが要点で、これが
        // 無いと再入実行がそのまま設定ツールや `spawn_subtask` へ到達し、サブタスクの
        // ネスト禁止も崩れる。deny-by-default なので新ツールを足しても自動では開かない。
        let gateway_actions: std::sync::Arc<dyn opencrab_gateway::GatewayActions> = if depth == 0 {
            system_actions
        } else {
            std::sync::Arc::new(opencrab_actions::SubEngineGatewayActions::new(
                system_actions,
            ))
        };
        let bridged = bridged.with_gateway_actions(gateway_actions);
        // 接続済み MCP サーバのツールを注入する（本ターンの caller で trusted_only を出し分け）。
        //
        // **depth 0 限定**。MCP は gateway とは別スロットなので、sub-engine の許可リスト
        // （`SubEngineGatewayActions`）を通らない。深さを見ずに注入すると、deny-by-default
        // のはずの sub-engine が運用者の繋いだ任意の外部ツール（送信・削除を含みうる）に
        // 到達できてしまい、最小権限の前提が崩れる。移設前の sub-engine も MCP は
        // 持っていなかった（`git show <移設前>:...subtask_engine.rs` に注入なし）ので、
        // ここは従来挙動の維持でもある。sub-engine へ開けたい場合は許可リストと同じく
        // 明示的な opt-in を設計してから行う。
        match state.mcp_manager.as_ref() {
            Some(m) if depth == 0 => {
                let provider = m.provider_for(agent_id, caller_is_trusted);
                bridged.with_mcp_actions(std::sync::Arc::new(provider))
            }
            _ => bridged,
        }
    };
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
            .with_workspace_root(Some(tool_result_workspace.clone()));
            engine.set_tool_dispatcher(std::sync::Arc::new(dispatcher));
        }
    }

    // サブタスク走行の実況（#175 S4）。ツール呼び出しと結果を進捗として通知口へ流す。
    // 購読していない（`wants_progress()` が false）ならフック自体を挿さず、要約の計算も
    // 省く（旧 `execute_spawn_subtask` と同じ判定）。
    if let Some(notifier) = run_notifier {
        if notifier.wants_progress() {
            let on_call = notifier.clone();
            engine.set_on_tool_call(move |assistant_content, tool_calls_json| {
                on_call.on_progress(&summarize_tool_calls(&assistant_content, &tool_calls_json));
            });
            let on_result = notifier.clone();
            engine.set_on_tool_result(move |_tool_call_id, tool_name, result_json, is_error| {
                let status = if is_error { "failed" } else { "completed" };
                let preview: String = result_json.chars().take(500).collect();
                on_result.on_progress(&format!("tool `{tool_name}` {status}\n{preview}"));
            });
        }
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
        set_turn_log_callbacks(
            &mut engine,
            state.db.clone(),
            agent_id.to_string(),
            session_id.to_string(),
            tool_result_workspace,
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
    let result = loop {
        let result = engine
            .run_with_model_override(
                system_prompt,
                conversation_override.as_deref().unwrap_or(conversation),
                &effective_model,
                Some(model_override.clone()),
                &merged_image_urls,
            )
            .await;

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
        spawn_background_index_build(state, agent_id, &effective_model);
        if let Ok(ref engine_result) = result {
            record_used_skills(state, agent_id, session_id, &engine_result.response);
        }
    }

    result
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
    let (prov, mdl) = {
        let eff =
            opencrab_db::queries::effective_model_for_agent(&conn, agent_id, &state.default_model)
                .unwrap_or_else(|_| state.default_model.clone());
        let (p, m) = split_llm_model_spec(&eff);
        (p.to_string(), m.to_string())
    };
    let budget = compute_context_budget(&conn, &prov, &mdl, state.compaction_ratio);
    let rebuilt = match build_conversation_string(&conn, session_id, agent_id, budget) {
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
    let theme = opencrab_db::queries::get_session(&conn, session_id)
        .ok()
        .flatten()
        .map(|s| s.theme)
        .unwrap_or_default();
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

#[cfg(test)]
mod format_log_tests {
    use super::format_single_log;
    use opencrab_db::queries::SessionLogRow;

    fn tool_call_log(tool_calls_json: &str) -> SessionLogRow {
        SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "s1".to_string(),
            log_type: "tool_call".to_string(),
            content: String::new(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({ "tool_calls_json": tool_calls_json }).to_string(),
            ),
            created_at: None,
        }
    }

    #[test]
    fn renders_canonical_tool_call_shape() {
        // 正準形状: {id, type, function:{name, arguments:"<json-string>"}}
        let tcj = serde_json::json!([{
            "id": "tc-1",
            "type": "function",
            "function": { "name": "search", "arguments": "{\"q\":\"rust\"}" }
        }])
        .to_string();
        let out = format_single_log(&tool_call_log(&tcj));
        assert!(out.contains("search"), "tool name must render: {out}");
        assert!(out.contains("tc-1"), "tool id must render: {out}");
        assert!(
            out.contains(r#"{"q":"rust"}"#),
            "arguments must render: {out}"
        );
    }

    #[test]
    fn renders_legacy_flat_tool_call_shape() {
        // 旧形状（既存DB行の後方互換）: {id, name, arguments:<object>}
        let tcj = serde_json::json!([{
            "id": "tc-9",
            "name": "old_tool",
            "arguments": { "a": 1 }
        }])
        .to_string();
        let out = format_single_log(&tool_call_log(&tcj));
        assert!(
            out.contains("old_tool"),
            "legacy tool name must render: {out}"
        );
        assert!(out.contains("tc-9"), "legacy tool id must render: {out}");
    }

    /// [#323] 1 つのセッションに複数の相手の発言が混ざっても、**誰の発言かが分かる**。
    ///
    /// Nostr の session を agent 単位（`nostr-{agent_id}`）へ寄せたことで、以前は
    /// 相手ごとに分かれていた会話が 1 本に集まる。会話文字列は `[{speaker_id}]:` 形式で
    /// 出るので、発言者は session ではなく行の `speaker_id` が区別する（Nostr の受信転記は
    /// `speaker_id` に相手の pubkey を入れる）。**新しい概念を足す必要は無い**ことの固定。
    #[test]
    fn different_speakers_in_one_session_stay_distinguishable() {
        let speech = |speaker: &str, text: &str| SessionLogRow {
            id: None,
            agent_id: speaker.to_string(),
            session_id: "nostr-agent-1".to_string(),
            log_type: "speech".to_string(),
            content: text.to_string(),
            speaker_id: Some(speaker.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };

        let alice = format_single_log(&speech("pubkey-alice", "こんばんは"));
        let bob = format_single_log(&speech("pubkey-bob", "こんばんは"));
        let agent = format_single_log(&speech("agent-1", "こんばんは"));

        assert!(alice.starts_with("[pubkey-alice]"), "{alice}");
        assert!(bob.starts_with("[pubkey-bob]"), "{bob}");
        assert!(agent.starts_with("[agent-1]"), "{agent}");
        // 本文が同じでも行としては別物（発言者が潰れていない）。
        assert_ne!(alice, bob);
        assert_ne!(alice, agent);
    }
}

#[cfg(test)]
mod memory_index_section_injection_tests {
    use super::build_conversation_string;

    fn mk_node(
        id: &str,
        node_type: &str,
        parent: Option<&str>,
        title: &str,
        source_session_id: Option<&str>,
        date_from: Option<&str>,
    ) -> opencrab_db::queries::IndexNodeRow {
        opencrab_db::queries::IndexNodeRow {
            id: id.to_string(),
            agent_id: "a1".to_string(),
            parent_id: parent.map(String::from),
            node_type: node_type.to_string(),
            source_type: "session_log".to_string(),
            title: title.to_string(),
            summary: format!("{title} の要約"),
            start_log_id: None,
            end_log_id: None,
            source_session_id: source_session_id.map(String::from),
            date_from: date_from.map(String::from),
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-06-01T00:00:00Z".to_string(),
            updated_at: "2026-06-01T00:00:00Z".to_string(),
            short_id: Some(id.to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        }
    }

    fn seed_index(conn: &rusqlite::Connection) {
        use opencrab_db::queries::*;
        insert_index_node(conn, &mk_node("r1", "root", None, "root", None, None)).unwrap();
        insert_index_node(
            conn,
            &mk_node("pmay", "period", Some("r1"), "2026-05", None, None),
        )
        .unwrap();
        insert_index_node(
            conn,
            &mk_node("pjun", "period", Some("r1"), "2026-06", None, None),
        )
        .unwrap();
        update_period_rollup(conn, "pmay", "5月は逆引き辞書を設計した。", "[\"FTS\"]").unwrap();
        insert_index_node(
            conn,
            &mk_node("s1", "session", Some("pjun"), "S", None, None),
        )
        .unwrap();
        insert_index_node(
            conn,
            &mk_node(
                "t-other",
                "topic",
                Some("s1"),
                "他セッション話題",
                Some("other-sess"),
                Some("2026-06-10"),
            ),
        )
        .unwrap();
        insert_index_node(
            conn,
            &mk_node(
                "t-cur",
                "topic",
                Some("s1"),
                "現セッション話題",
                Some("cur-sess"),
                Some("2026-06-11"),
            ),
        )
        .unwrap();
    }

    fn seed_logs(conn: &rusqlite::Connection, n: usize) {
        for i in 0..n {
            opencrab_db::queries::insert_session_log(
                conn,
                &opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: "a1".to_string(),
                    session_id: "cur-sess".to_string(),
                    log_type: "speech".to_string(),
                    content: format!("メッセージ {i} の内容がここに入る。{}", "詳細".repeat(40)),
                    speaker_id: Some("a1".to_string()),
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn injects_memory_index_exactly_once_under_budget() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_index(&conn);
        seed_logs(&conn, 3);
        let out = build_conversation_string(&conn, "cur-sess", "a1", 100_000).unwrap();
        assert_eq!(out.matches("[Memory Index]").count(), 1);
        // 月次要約が会話履歴に載る（中心要件）
        assert!(out.contains("5月は逆引き辞書を設計した。"));
        // 現在月 topic: 他セッションのみ
        assert!(out.contains("[t-other]"));
        assert!(!out.contains("[t-cur]"));
        // 予算内なので通常の全文会話が続く
        assert!(out.contains("メッセージ 2 の内容"));
    }

    #[test]
    fn no_index_means_no_section() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_logs(&conn, 2);
        let out = build_conversation_string(&conn, "cur-sess", "a1", 100_000).unwrap();
        assert!(!out.contains("[Memory Index]"));
    }

    #[test]
    fn tiny_budget_skips_section() {
        // 予算比ガード: セクションが予算の 1/4 を超えるなら注入しない（小型モデル保護）
        let conn = opencrab_db::init_memory().unwrap();
        seed_index(&conn);
        seed_logs(&conn, 3);
        let out = build_conversation_string(&conn, "cur-sess", "a1", 100).unwrap();
        assert!(!out.contains("[Memory Index]"));
    }

    #[test]
    fn compaction_path_keeps_short_id_sets_disjoint() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_index(&conn);
        // 現セッション topic に log 範囲を持たせ、コンパクション時の
        // [Past context summary] に出るようにする
        seed_logs(&conn, 40);
        conn.execute(
            "UPDATE memory_index_nodes SET start_log_id = 1, end_log_id = 20 WHERE id = 't-cur'",
            [],
        )
        .unwrap();
        // セクションの予算比ガード（1/4）は通しつつ、会話本文はコンパクションを
        // 強制する中間サイズの予算
        let out = build_conversation_string(&conn, "cur-sess", "a1", 900).unwrap();
        assert_eq!(out.matches("[Memory Index]").count(), 1);
        assert_eq!(out.matches("[Past context summary").count(), 1);
        // 現セッション topic は Past context summary 側のみ、他セッション topic は
        // Memory Index 側のみ（short_id 集合が素）
        assert_eq!(out.matches("[t-cur]").count(), 1);
        assert_eq!(out.matches("[t-other]").count(), 1);
        let mi_pos = out.find("[Memory Index]").unwrap();
        let pcs_pos = out.find("[Past context summary").unwrap();
        let tcur_pos = out.find("[t-cur]").unwrap();
        let tother_pos = out.find("[t-other]").unwrap();
        assert!(mi_pos < pcs_pos);
        assert!(tother_pos > mi_pos && tother_pos < pcs_pos);
        assert!(tcur_pos > pcs_pos);
    }
}

#[cfg(test)]
mod redact_secret_fields_tests {
    // redaction 本体は inline / dispatch 両経路で共有するため actions 側にある。
    use opencrab_actions::redact_secret_fields_json;

    #[test]
    fn test_redacts_nsec_nested_in_actionresult_wrapper() {
        // set_on_tool_result に渡る実際の形は ActionResult ラッパ全体で、
        // nsec は data の中にネストする。
        let wrapper = r#"{"success":true,"data":{"nsec":"nsec1supersecret","npub":"npub1abc","pubkey":"hex","warning":"w"},"error":null}"#;
        let out = redact_secret_fields_json(wrapper);
        assert!(!out.contains("supersecret"), "nsec leaked: {out}");
        assert!(out.contains("[redacted]"));
        // 非秘密は保持。
        assert!(out.contains("npub1abc"));
        assert!(out.contains("hex"));
        // トップレベル nsec も潰す。
        let top = r#"{"nsec":"nsec1x","npub":"npub1y"}"#;
        assert!(!redact_secret_fields_json(top).contains("nsec1x"));
        // JSON 不能は placeholder に。
        let bad = "nsec1raw npub1 not-json";
        let out = redact_secret_fields_json(bad);
        assert!(!out.contains("nsec1raw"));
        assert!(out.contains("redacted"));
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
/// `speaker_id != <agent_id 引数>`（[`super::is_user_speech`] 参照）。
#[cfg(test)]
mod recent_user_speech_guarantee_tests {
    use super::{build_conversation_string, RECENT_MIN_USER_SPEECHES};
    use opencrab_actions::transcript::{InboundMessageRecord, TranscriptSource};

    const AGENT: &str = "a1";
    const USER: &str = "kojira";
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
                    sender_name: "kojira",
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
            out.contains("[Past context summary"),
            "テストの前提が崩れている（コンパクション経路に入っていない）: {out}"
        );
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
    /// 戻しても "kojira" != "a1" で真になり、**このテストは落ちない**。
    ///
    /// それでも無防備になった性質は無い: 行の形が直ったので列比較でも正しい答えになる
    /// （＝ #286 のバグ自体が成立しなくなった）。述語が引数比較であるべきことは
    /// [`super::is_user_speech`] の doc とその近傍テストが担い、ここは「ゲートウェイ形状の
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
    const USER: &str = "kojira";
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
                    sender_name: "kojira",
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

    /// Bot 同士のループ防止（Silent Reply の元の意図）は残る — 撤回の非退行検査。
    #[test]
    fn bot_loop_prevention_survives_the_revert() {
        let conn = opencrab_db::init_memory().unwrap();
        let (prompt, _name) =
            build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);

        assert!(prompt.contains("## Silent Reply"), "prompt:\n{prompt}");
        assert!(
            prompt.contains("他のBotが話している場合（Bot同士のループを防ぐ）"),
            "bot loop prevention was lost:\n{prompt}"
        );
    }
}

/// #291: 既に DB にある `evaluation` 行を会話文字列へ復元しない。
///
/// 対話ターンからの evaluator 呼び出しは撤去したが、過去に記録された行は本番 DB に
/// 残る。読み出し側でも落とさないと、次のターンで採点結果と「次ターンでギャップを
/// 埋めろ」という指示文が復活し、直前のユーザー発言と同じ土俵に並んでしまう。
/// 全文経路・コンパクション経路・切り詰め経路のすべてで落ちることを確かめる。
#[cfg(test)]
mod evaluation_not_in_conversation_tests {
    use super::build_conversation_string;

    const AGENT: &str = "a1";
    const SESSION: &str = "s1";

    /// 事故当時と同じ形の evaluation 行（採点 + 指示文）。
    const EVAL_CONTENT: &str = "score 0.05/0.70 (not satisfied) — 証拠が無い\ngaps:\n- 未検証\nAddress these gaps in your next turn (claims without evidence in the trace do not count).";

    fn insert(conn: &rusqlite::Connection, log_type: &str, speaker: &str, content: &str) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: SESSION.to_string(),
                log_type: log_type.to_string(),
                content: content.to_string(),
                speaker_id: Some(speaker.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    fn seed(conn: &rusqlite::Connection) {
        insert(
            conn,
            "speech",
            "kojira",
            "既存フォローはわたしだけなのでは？",
        );
        insert(conn, "evaluation", "evaluator", EVAL_CONTENT);
        insert(conn, "speech", AGENT, "確認します。");
    }

    #[test]
    fn evaluation_rows_are_dropped_from_the_full_conversation() {
        let conn = opencrab_db::init_memory().unwrap();
        seed(&conn);

        let out = build_conversation_string(&conn, SESSION, AGENT, 100_000).unwrap();
        assert!(
            !out.contains("[evaluation]"),
            "evaluation 行が会話に復元されている: {out}"
        );
        assert!(
            !out.contains("Address these gaps in your next turn"),
            "採点の指示文が会話に復元されている: {out}"
        );
        // 人間の発言は残る（除外が効きすぎていないこと）。
        assert!(out.contains("既存フォローはわたしだけなのでは？"), "{out}");
        assert!(out.contains("確認します。"), "{out}");
    }

    /// コンパクション経路（topic 要約あり）でも落ちること。
    #[test]
    fn evaluation_rows_are_dropped_from_the_compacted_conversation() {
        let conn = opencrab_db::init_memory().unwrap();
        seed(&conn);
        for i in 0..30 {
            insert(
                &conn,
                "tool_result",
                AGENT,
                &format!("結果 {i}: {}", "x".repeat(400)),
            );
            insert(&conn, "evaluation", "evaluator", EVAL_CONTENT);
        }
        // topic 要約を 1 件置いてコンパクション経路（切り詰めではない方）へ入れる。
        opencrab_db::queries::insert_index_node(
            &conn,
            &opencrab_db::queries::IndexNodeRow {
                id: "t1".to_string(),
                agent_id: AGENT.to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: "作業ログ".to_string(),
                summary: "フォロー作業を進めていた。".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: Some(SESSION.to_string()),
                date_from: Some("2026-07-01".to_string()),
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-07-01T00:00:00Z".to_string(),
                updated_at: "2026-07-01T00:00:00Z".to_string(),
                short_id: Some("t1".to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();

        let out = build_conversation_string(&conn, SESSION, AGENT, 300).unwrap();
        assert!(
            out.contains("[Past context summary"),
            "テストの前提: コンパクション経路に入ること: {out}"
        );
        assert!(
            !out.contains("[evaluation]"),
            "コンパクション経路で evaluation 行が残っている: {out}"
        );
        assert!(
            !out.contains("Address these gaps in your next turn"),
            "コンパクション経路で採点の指示文が残っている: {out}"
        );
    }

    #[test]
    fn evaluation_rows_are_dropped_from_the_truncated_conversation() {
        let conn = opencrab_db::init_memory().unwrap();
        seed(&conn);
        // 全文が予算に収まらない状態にして切り詰め経路へ落とす。
        for i in 0..30 {
            insert(
                &conn,
                "tool_result",
                AGENT,
                &format!("結果 {i}: {}", "x".repeat(400)),
            );
            insert(&conn, "evaluation", "evaluator", EVAL_CONTENT);
        }

        let out = build_conversation_string(&conn, SESSION, AGENT, 300).unwrap();
        assert!(
            !out.contains("[evaluation]"),
            "切り詰め経路で evaluation 行が残っている: {out}"
        );
        assert!(
            !out.contains("Address these gaps in your next turn"),
            "切り詰め経路で採点の指示文が残っている: {out}"
        );
    }
}

/// `[Impressions]` セクションが会話文字列に載ること（#314）。
///
/// **相手が変わればセクションの中身も変わる**（全員分を常に載せない）。相手の
/// 人物像が無い場合はセクション自体が出ず、会話の組み立ては壊れない。
#[cfg(test)]
mod impression_section_injection_tests {
    use super::build_conversation_string;

    const AGENT: &str = "a1";

    fn insert_speech(conn: &rusqlite::Connection, session_id: &str, speaker_id: &str) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: speaker_id.to_string(),
                session_id: session_id.to_string(),
                log_type: "speech".to_string(),
                content: "こんにちは".to_string(),
                speaker_id: Some(speaker_id.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    fn write_impression(conn: &rusqlite::Connection, session_id: &str, target_id: &str) {
        opencrab_db::queries::upsert_impression(
            conn,
            &opencrab_db::queries::ImpressionRow {
                id: format!("imp-{target_id}"),
                agent_id: AGENT.to_string(),
                session_id: session_id.to_string(),
                target_id: target_id.to_string(),
                target_name: format!("name-{target_id}"),
                personality: format!("personality-{target_id}"),
                communication_style: String::new(),
                recent_behavior: String::new(),
                agreement: "中立".to_string(),
                notes: String::new(),
                last_updated_turn: 0,
            },
        )
        .unwrap();
    }

    /// 別経路（別セッション）で書いた人物像が、いま話しているセッションのプロンプトに載る。
    #[test]
    fn injects_impression_of_the_current_speaker_across_sessions() {
        let conn = opencrab_db::init_memory().unwrap();
        write_impression(&conn, "discord-sess", "u1");
        insert_speech(&conn, "nostr-sess", "u1");

        let out = build_conversation_string(&conn, "nostr-sess", AGENT, 100_000).unwrap();
        assert_eq!(out.matches("[Impressions]").count(), 1);
        assert!(out.contains("personality-u1"), "{out}");
    }

    /// 話していない相手の人物像は載らない。
    #[test]
    fn omits_impressions_of_people_not_speaking() {
        let conn = opencrab_db::init_memory().unwrap();
        write_impression(&conn, "s1", "u1");
        write_impression(&conn, "s1", "u2");
        insert_speech(&conn, "s1", "u1");

        let out = build_conversation_string(&conn, "s1", AGENT, 100_000).unwrap();
        assert!(out.contains("personality-u1"), "{out}");
        assert!(!out.contains("personality-u2"), "{out}");
    }

    /// 相手の人物像が無くてもセクションが出ないだけで、会話は普通に組み立つ。
    #[test]
    fn no_impression_means_no_section() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_speech(&conn, "s1", "u1");

        let out = build_conversation_string(&conn, "s1", AGENT, 100_000).unwrap();
        assert!(!out.contains("[Impressions]"), "{out}");
        assert!(out.contains("こんにちは"), "{out}");
    }
}
