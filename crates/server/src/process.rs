//! エージェントのメッセージ処理に関する共通ロジック。
//!
//! REST API (`api/sessions.rs`) と Discordゲートウェイ (`discord.rs`) の
//! 両方から利用される。

use std::sync::{Arc, OnceLock};

use tiktoken_rs::CoreBPE;

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
    if let Ok(conn) = state.db.lock() {
        if let Ok(agent_cmds) = opencrab_db::queries::list_agent_allowed_commands(&conn, agent_id) {
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
    }
    tools_cfg
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
pub fn build_agent_context(conn: &rusqlite::Connection, agent_id: &str) -> (String, String) {
    let agent = opencrab_db::queries::get_agent(conn, agent_id)
        .ok()
        .flatten();
    let skills = opencrab_db::queries::list_skills(conn, agent_id, true).unwrap_or_default();
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
/// trusted_users の permission='co_agent' 行（選定ロジックは
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
/// context_window が不明な場合のデフォルト予算（トークン数）。
const DEFAULT_CONTEXT_BUDGET_TOKENS: usize = 100_000;

fn get_tokenizer() -> &'static CoreBPE {
    static TOKENIZER: OnceLock<CoreBPE> = OnceLock::new();
    TOKENIZER
        .get_or_init(|| tiktoken_rs::o200k_base().expect("failed to load o200k_base tokenizer"))
}

/// 文字列の正確なトークン数を返す (tiktoken o200k_base)。
fn estimate_tokens(s: &str) -> usize {
    get_tokenizer().encode_with_special_tokens(s).len()
}

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

    let mut inner_budget = context_budget_tokens;
    for section in [&ledger_section, &memory_index_section]
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
        Ok(logs) => logs,
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
        recent_logs = logs;
    }

    // 予算内に収まるようにログを後ろから詰める
    let recent_text = fit_logs_to_budget(&recent_logs, remaining_budget);

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

fn build_full_conversation(conn: &rusqlite::Connection, session_id: &str) -> String {
    let logs = match opencrab_db::queries::list_session_logs_by_session(conn, session_id) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(session_id = %session_id, "Failed to list session logs: {e}");
            return "No messages yet.".to_string();
        }
    };
    if logs.is_empty() {
        return "No messages yet.".to_string();
    }
    format_logs(&logs)
}

fn build_truncated_conversation(
    conn: &rusqlite::Connection,
    session_id: &str,
    budget_tokens: usize,
) -> String {
    let mut logs = match opencrab_db::queries::list_recent_session_logs(conn, session_id, 500) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(session_id = %session_id, "Failed to list recent session logs for truncation: {e}");
            vec![]
        }
    };
    logs.reverse();

    let header = "[Note: Earlier messages were omitted due to context length. Showing most recent messages.]\n\n";
    let header_tokens = estimate_tokens(header);
    let remaining = budget_tokens.saturating_sub(header_tokens);
    let recent_text = fit_logs_to_budget(&logs, remaining);

    format!("{header}{recent_text}")
}

/// ログを末尾（最新）から逆順に辿り、予算内に収まる分だけ返す。
/// 最低 RECENT_MIN_LOGS 件は常に含める。
fn fit_logs_to_budget(
    logs: &[opencrab_db::queries::SessionLogRow],
    budget_tokens: usize,
) -> String {
    if logs.is_empty() {
        return String::new();
    }

    // まず各ログを文字列化
    let formatted: Vec<String> = logs.iter().map(format_single_log).collect();

    // 後ろから詰めていく
    let mut used_tokens = 0;
    let mut start_idx = formatted.len();

    for (i, line) in formatted.iter().enumerate().rev() {
        let line_tokens = estimate_tokens(line) + 1; // +1 for newline
        if used_tokens + line_tokens > budget_tokens
            && (formatted.len() - start_idx) >= RECENT_MIN_LOGS
        {
            break;
        }
        used_tokens += line_tokens;
        start_idx = i;
    }

    formatted[start_idx..].join("\n")
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

/// verify 段: 契約付き active タスクに対する evaluator 評価（record-only）。
///
/// - 対象: depth 0 の run で、セッションに contract 非空の active タスクがあり、
///   かつこの run が実際にツールを実行した場合のみ（雑談/アイドル tick では評価しない）。
/// - evaluator は生成 run とは別の新しい context（ツール無し・temperature 0）で呼ぶ。
///   生のトレース（session_logs、この agent の分のみ）を渡す — 要約を渡すのは自己採点と同義。
/// - **記録専用**: 評価は session_logs (log_type=evaluation) とタスク台帳 progress に
///   記録され、エージェントは次ターンの会話でそれを見て自己修正する。同一ターン内の
///   強制再実行はしない — run の回答は評価前に配信済みのため、再実行は二重返信・
///   セッションロック長期保持・context 超過を生む。
/// - この run が契約タスクと無関係だった場合（relevant=false）は何も記録しない。
/// - evaluator の失敗で返信は殺さない（warn してスキップ）。
async fn run_verify_stage(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
    effective_model: &str,
    model_override: &Arc<std::sync::Mutex<Option<String>>>,
    engine_result: &opencrab_core::EngineResult,
    trace_checkpoint: i64,
) {
    let cfg = &state.evaluator;

    // ツールを一切使っていない run は世界を変えていないので評価しない
    // （heartbeat のアイドル tick 等で毎回 evaluator を回さないためのガード）。
    if engine_result.tool_calls_made == 0 {
        return;
    }

    // タスクとトレースを1ロックスコープで読む（read の一貫性 + ロック往復削減）
    let (task, trace) = {
        let Ok(conn) = state.db.lock() else { return };
        let task = opencrab_db::queries::get_active_task_for_session(&conn, agent_id, session_id)
            .ok()
            .flatten();
        let Some(task) = task else { return };
        let trace =
            opencrab_db::queries::list_session_logs_after_id(&conn, session_id, trace_checkpoint)
                .map(|logs| {
                    // マルチエージェントセッションで他エージェントの作業を「証拠」に混ぜない
                    let own: Vec<_> = logs
                        .into_iter()
                        .filter(|l| l.agent_id == agent_id)
                        .collect();
                    opencrab_core::evaluator::format_trace(&own)
                })
                .unwrap_or_default();
        (task, trace)
    };
    let Some(contract) = task.contract.clone().filter(|c| !c.trim().is_empty()) else {
        return;
    };

    // 設定 typo（threshold > 1.0 / NaN 等）で合格が数学的に不可能にならないよう防衛
    let threshold = if cfg.threshold.is_finite() {
        cfg.threshold.clamp(0.0, 1.0)
    } else {
        0.7
    };

    // 評価モデル: 設定 > run 中の set_model 切替 > エージェントの実効モデル
    let eval_model = cfg
        .model
        .clone()
        .or_else(|| model_override.lock().ok().and_then(|g| g.clone()))
        .unwrap_or_else(|| effective_model.to_string());
    let eval_llm = LlmRouterAdapter::new(state.llm_router.clone())
        .with_metrics(MetricsContext {
            db: state.db.clone(),
            agent_id: agent_id.to_string(),
            session_id: Some(session_id.to_string()),
            pricing: PricingRegistry::default(),
            last_metrics_id: Arc::new(std::sync::Mutex::new(None)),
            current_purpose: Arc::new(std::sync::Mutex::new("evaluation".to_string())),
        })
        .with_agent_id(agent_id);

    let eval = match opencrab_core::evaluator::evaluate_against_contract(
        &eval_llm,
        &eval_model,
        &task.goal,
        &contract,
        &engine_result.response,
        &trace,
    )
    .await
    {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(session_id = %session_id, "evaluator failed, skipping verify: {e}");
            return;
        }
    };

    if !eval.relevant {
        tracing::debug!(
            session_id = %session_id,
            task_id = task.id,
            "verify stage: run not related to contract task, skipping record"
        );
        return;
    }

    let passed = eval.score >= threshold;
    let gaps_text = if eval.gaps.is_empty() {
        "(none)".to_string()
    } else {
        eval.gaps
            .iter()
            .map(|g| format!("- {g}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    tracing::info!(
        session_id = %session_id,
        task_id = task.id,
        score = eval.score,
        passed = passed,
        "verify stage evaluation"
    );

    // トレースと台帳に評価を記録（原則 VII: 後から読める）。
    // このエントリは次ターンの会話再構築に [evaluation] として含まれ、
    // エージェントが gaps を見て自己修正する（ターン跨ぎの verify ループ）。
    if let Ok(conn) = state.db.lock() {
        let metadata = serde_json::json!({
            "task_id": task.id,
            "score": eval.score,
            "threshold": threshold,
            "passed": passed,
            "gaps": eval.gaps,
        });
        let next_step = if passed {
            String::new()
        } else {
            "\nAddress these gaps in your next turn (claims without evidence in the trace do not count).".to_string()
        };
        opencrab_db::queries::insert_session_log_best_effort(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                log_type: "evaluation".to_string(),
                content: format!(
                    "score {:.2}/{:.2} ({}) — {}\ngaps:\n{gaps_text}{next_step}",
                    eval.score,
                    threshold,
                    if passed { "passed" } else { "not satisfied" },
                    eval.summary,
                ),
                speaker_id: Some("evaluator".to_string()),
                turn_number: None,
                metadata_json: Some(metadata.to_string()),
                created_at: None,
            },
        );
        let _ = opencrab_db::queries::insert_task_progress(
            &conn,
            task.id,
            "progress",
            &format!(
                "[evaluation] score {:.2} ({}): {}",
                eval.score,
                if passed { "passed" } else { "below threshold" },
                eval.summary,
            ),
        );
    }
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
        if !urls.is_empty() {
            tracing::debug!(
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
            .with_reply_target(req.reply_target.clone());
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

    set_turn_log_callbacks(
        &mut engine,
        state.db.clone(),
        agent_id.to_string(),
        session_id.to_string(),
        tool_result_workspace,
    );

    let merged_image_urls = merge_image_urls(state, session_id, agent_id, &req.image_urls);

    let verify_enabled = depth == 0 && state.evaluator.enabled;
    let verify_model_override = model_override.clone();

    // ループ再起動 v1（#52）: depth 0 の run が反復上限（stopped_by_limit）で停止し、
    // セッションに active タスクが残っている場合、restart_count 上限まで（v1 では 1 回）
    // クリーンな context でエンジンを再実行する。会話は再構築するため、run-1 中に
    // session_logs へ記録されたトレース + verify の evaluation + 下で記録する
    // [restart] decision エントリ（台帳 prompt section 経由）が run-2 に見える。
    // 注意: 呼び出し元（message_loop）のセッションロックは run1 + verify + run2 の
    // 全期間保持される。既定無効（agent.loop_restart_enabled）。
    let mut conversation_override: Option<String> = None;
    let mut restarts_this_call: i64 = 0;
    let result = loop {
        // verify 段用: この run が session_logs に残すトレースの開始位置を記録
        // （verify が走らない構成では余計なクエリを打たない）
        let trace_checkpoint = if verify_enabled {
            match state.db.lock() {
                Ok(conn) => opencrab_db::queries::list_recent_session_logs(&conn, session_id, 1)
                    .ok()
                    .and_then(|v| v.first().and_then(|l| l.id))
                    .unwrap_or(0),
                Err(_) => 0,
            }
        } else {
            0
        };

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
                let fired_model = verify_model_override
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

        // verify 段 (evaluator): 契約付き active タスクがある場合、独立した context で
        // rubric 評価して記録する（record-only、LOOPS I/II/VI）。再実行 run にも各自かかる。
        if verify_enabled {
            if let Ok(ref engine_result) = result {
                run_verify_stage(
                    state,
                    agent_id,
                    session_id,
                    &effective_model,
                    &verify_model_override,
                    engine_result,
                    trace_checkpoint,
                )
                .await;
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
            "co_agent",
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
            "co_agent",
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
            "trusted_user",
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
            "co_agent",
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
            "co_agent",
            "owner",
            "2026-01-01",
            "Crab B",
        )
        .unwrap();

        let (prompt, _name) = build_agent_context(&conn, "a1");

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
        let (prompt, _name) = build_agent_context(&conn, "a1");
        assert!(
            !prompt.contains("channel_id"),
            "shared system prompt must not name a transport destination argument:\n{prompt}"
        );
        assert!(prompt.contains("taken from the current conversation"));
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
