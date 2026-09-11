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
    // §2.7 静的 index（案 I-a・案B）: 「## More tools」節は build_agent_context では組まない。
    // executor の effective_tool_definitions（policy＋run 済み・そのターンに実行可能な集合）から
    // 「常時集合を除いた残り」をカテゴリ別に並べて run_agent_response で後付けする
    // （[`build_more_tools_index`]）。lane（Nostr の会話/write op）と owner-only は effective に
    // 現れるか否かで自動的に出し分けられる（build_agent_context の契約は変えない）。

    let prompt = format!(
        "You are {agent_name} ({persona}).\n\
         \n\
         You are an autonomous agent participating in a discussion. \
         You can use tools to search your history, learn from experience, create new \
         skills, and manage your workspace. You can plan and run several actions in sequence \
         in one response — for example, gather information with execute_shell, analyze the \
         result, add a command with add_allowed_command, then create a skill with \
         create_my_skill.\n\
         \n\
         The conversation history uses the format \"[speaker]: message\" for context. \
         Your response is posted verbatim; a name prefix you add is not removed, so it would \
         appear duplicated.\n\
         \n\
         ## Turn completion\n\
         \n\
         Your turn continues by default. Only `NO_REPLY` explicitly ends it. When all requested \
         work is complete, append `NO_REPLY` on its own final line after the final answer. The \
         marker is recorded as a turn-termination event but is not delivered as speech. If no \
         speech should be delivered, respond with exactly `NO_REPLY`. This includes a topic \
         that is already resolved where another exchange would add no new information. Without \
         `NO_REPLY`, you are called again and must continue the unfinished work.\n\
         \n\
         ## Async Behavior\n\
         \n\
         Query tools (execute_shell and the like — anything you call to fetch a result) run \
         asynchronously: the result arrives later and you are called again with it in the \
         conversation history. Utterances (say / reply / reaction / repost) are fire-and-forget \
         and return no result, but they do not end the turn unless you also write `NO_REPLY`.\n\
         \n\
         Some tools return `{{status:\"spawned\", subtask_id: ...}}` immediately instead of a \
         final result. The work is then running in the background, and its result arrives \
         later in a separate turn as a `[subtask_completed: ...]` entry. Calling the same \
         tool again for the same request starts a second, independent run, and the actual \
         result appears only at the completion turn.\n\
         \n\
         A `[subtask_completed: ...]` entry means a tool you called has finished and it is \
         your turn again. Read that result, finish the original request, and then write \
         `NO_REPLY` to end the turn.\n\
         \n\
         ## Memory & Context\n\
         \n\
         Long conversations are automatically compacted: older messages are replaced with a \
         [Past context summary] section of topic summaries with node IDs (e.g. \
         [topic-xxx-1-20]).\n\
         - `retrieve_memory_nodes(node_id)` returns the full conversation text for a node.\n\
         - `browse_memory_index` lists all past topics beyond those shown in the summary.\n\
         - `search_memory_index` searches past topics by keyword and returns matching nodes \
         to retrieve.\n\
         \n\
         These tools reach your full history even after compaction.\n\
         \n\
         ## Task Ledger\n\
         \n\
         You have a persistent, DB-backed task ledger that survives context compaction and \
         restarts. When a [Task Ledger] section appears in the conversation, it is the \
         authoritative current working state and takes precedence over your own recall.\n\
         - `open_task(goal, acceptance_criteria)` records the contract for a multi-step task.\n\
         - `record_task_progress` appends a step; `kind=decision` records a decision with its \
         why, `kind=blocker` an obstacle.\n\
         - `close_task(status=done|abandoned)` closes the task; `update_task_contract` \
         revises the criteria.\n\
         - Trivial single-message replies do not need a ledger entry.\n\
         \n\
         {skills_text}{character_section}{instructions_section}{curated_section}",
    );

    (prompt, agent_name)
}

/// #923 §2.7 静的 index（案B）: ツール名 → カテゴリの写像。1 か所に集約する。
///
/// 常時集合の外にあるツールを describe_tools で取得させるための分類。未知名は "other"。
/// lane 依存（Nostr の会話/write op）や owner-only は「effective に現れるか」で決まるので、
/// ここでは caller/lane を見ずに**名前だけ**で分類する（写像はレーン非依存）。
fn tool_category(name: &str) -> &'static str {
    if name.starts_with("mcp__") {
        return "mcp";
    }
    match name {
        "record_memory_unit"
        | "record_memory_core"
        | "tag_topic"
        | "untag_topic"
        | "merge_tags"
        | "summarize_and_save"
        | "plan_next_memory_window"
        | "retract_memory_core"
        | "retract_memory_unit"
        | "update_memory_core"
        | "read_my_history"
        | "search_my_history"
        | "survey_my_history" => "memory",
        "create_skill" | "create_my_skill" | "retire_my_skill" | "restore_my_skill" => "skills",
        "evaluate_response"
        | "analyze_llm_usage"
        | "learn_from_experience"
        | "learn_from_peer"
        | "reflect_and_learn"
        | "recall_model_experiences"
        | "save_model_insight"
        | "generate_inner_voice"
        | "update_impression" => "introspection",
        "ws_read" | "ws_list" | "ws_write" | "ws_edit" | "ws_mkdir" | "ws_delete" => "workspace",
        "update_task_contract" | "get_task" | "declare_done" => "tasks",
        "configure_llm_provider"
        | "configure_nostr"
        | "configure_self"
        | "configure_mcp_server"
        | "select_llm"
        | "update_instructions" => "configuration",
        "set_my_heartbeat"
        | "get_my_heartbeat"
        | "update_heartbeat_instructions"
        | "run_my_heartbeat"
        | "read_heartbeat_instructions"
        | "set_my_schedule"
        | "get_my_schedules"
        | "update_my_schedule"
        | "delete_my_schedule" => "schedule & heartbeat",
        "nostr_generate_key" | "nostr_list_keys" | "nostr_switch_identity" => "nostr keys",
        "add_allowed_command" | "list_allowed_commands" | "remove_allowed_command" => {
            "allowed commands"
        }
        "set_default_webhook"
        | "get_default_webhook"
        | "list_webhooks"
        | "set_default_subtask_webhook"
        | "get_default_subtask_webhook"
        | "list_subtask_webhooks" => "webhook",
        "get_system_info" | "update_memory_index_config" => "system",
        // Nostr レーンの write/操作 op（Nostr gateway 接続時のみ effective に出る＝Nostr ターンのみ）。
        "follow" | "unfollow" | "kind0" | "upload" | "nostr_post" | "nostr_reply" | "nostr_zap"
        | "nostr_run" => "nostr",
        _ => "other",
    }
}

/// #923 §2.7 静的 index（案B）: そのターンの executor から「常時集合を除いた残り」を
/// カテゴリ別に並べた「## More tools」節を作る。
///
/// **単一実装から導出**: 投影集合 = `list_tools()`、実行可能集合 = `effective_tool_definitions()`。
/// 差（effective − 投影）＝「見えていないが describe_tools で呼べるツール」をカテゴリ分けする。
/// lane（Nostr）と owner-only は effective に現れるか否かで自動的に出し分く（別の lane 分類を
/// 新設しない）。空なら空文字列（節ごと出さない）。
pub fn build_more_tools_index(executor: &opencrab_actions::BridgedExecutor) -> String {
    use opencrab_core::ActionExecutor;
    use std::collections::{BTreeMap, BTreeSet};

    let projected: BTreeSet<String> = executor.list_tools().into_iter().map(|t| t.name).collect();
    let mut by_cat: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for def in executor.effective_tool_definitions() {
        let name = def.definition.name;
        if projected.contains(&name) {
            continue;
        }
        by_cat.entry(tool_category(&name)).or_default().push(name);
    }
    if by_cat.is_empty() {
        return String::new();
    }
    // "other" と "mcp" は最後に回し、それ以外はカテゴリ名昇順（BTreeMap 既定）。
    let mut lines: Vec<String> = Vec::new();
    let order_key = |c: &str| match c {
        "mcp" => 1,
        "other" => 2,
        _ => 0,
    };
    let mut cats: Vec<(&'static str, Vec<String>)> = by_cat.into_iter().collect();
    cats.sort_by(|a, b| order_key(a.0).cmp(&order_key(b.0)).then(a.0.cmp(b.0)));
    for (cat, mut names) in cats {
        names.sort();
        lines.push(format!("- {cat}: {}", names.join(", ")));
    }
    format!(
        "\n\n## More tools\n\nThe tools above are always available. Others are listed here by \
         name; call describe_tools([\"name\", ...]) to load a tool's parameters, then call it — \
         loaded tools stay available for the rest of this turn.\n{}",
        lines.join("\n")
    )
}
