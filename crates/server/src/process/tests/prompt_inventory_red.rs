use super::build_agent_context;

fn prompt() -> String {
    let conn = opencrab_db::init_memory().unwrap();
    let (p, _name) = build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);
    p
}

/// 空白（改行・連続空白）を 1 個の半角スペースへ畳み、前後を trim する。
/// house style の `\n\`（行末で literal 改行）や字下げの差を吸収して節単位で比較するため。
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `heading`（"## X"）から次の "## " 見出し直前までを節として切り出す。
fn extract_section(prompt: &str, heading: &str) -> String {
    let start = prompt
        .find(heading)
        .unwrap_or_else(|| panic!("見出しが無い: {heading}\n{prompt}"));
    let after = &prompt[start + heading.len()..];
    let end = after
        .find("## ")
        .map(|e| start + heading.len() + e)
        .unwrap_or(prompt.len());
    prompt[start..end].to_string()
}

/// §1-1: 撤去対象（行動指示・禁止）の文が system prompt に **0 件**（1 文 1 assert・count）。
/// §2 撤去 14 件（A3/A7/A9/A17/A21/A22/A23/A24/A25/A26/A27/A28/A33/A50）＋
/// §6 裁定の撤去（A12 日本語文／A37「Never promise to reply」／Peer Review 節丸ごと／
/// 導入部 req/reply マーカー文）。
#[test]
fn removed_command_and_prohibition_lines_are_gone() {
    let prompt = prompt();
    // (fragment, ラベル) — fragment は現 tip の prompt に一意に存在する断片。
    let removed: &[(&str, &str)] = &[
        // --- §2 撤去 14 件 ---
        ("Respond thoughtfully to the conversation.", "A3 丁寧に返せ"),
        (
            "Just reply with the message content directly.",
            "A7 本文だけ返せ",
        ),
        (
            "複数のアクションを連続して呼び出してください",
            "A9 連続呼び出せ命令",
        ),
        (
            "Briefly tell the user you've started",
            "A17 開始を宣言せよ（#916 二重投稿元凶）",
        ),
        ("Check what the result contains", "A21 結果を確認せよ"),
        (
            "If there's more to do: continue with the next step",
            "A22 続きをせよ",
        ),
        (
            "If the task is done: summarize and reply to the user",
            "A23 要約して返せ",
        ),
        (
            "If no reply is needed: respond with NO_REPLY",
            "A24 不要なら NO_REPLY",
        ),
        (
            "Do NOT repeat what you already said in the previous turn.",
            "A25 繰り返すな",
        ),
        (
            "Do NOT re-explain what you're about to do if you already said it.",
            "A26 再説明するな",
        ),
        ("Just act on the result.", "A27 結果に対処せよ"),
        (
            "Before responding after [subtask_completed",
            "A28 二重投稿コーチング塊",
        ),
        (
            "Never say you cannot post multiple messages",
            "A33 できないと言うな",
        ),
        (
            "Read the raw content in the `part X/N` messages with fresh eyes",
            "A50 fresh eyes 作法",
        ),
        // --- §6 裁定の撤去 ---
        (
            "Bot だからという理由では黙らない",
            "A12 bot 判定文（§6 撤去）",
        ),
        (
            "Never promise to reply",
            "A37 後で返すと約束するな（§6 撤去）",
        ),
        ("## Peer Review", "Peer Review 節見出し（§6 丸ごと撤去）"),
        ("Do NOT stay silent", "A49 沈黙するな（Peer Review 節）"),
        (
            "you must reply to another bot",
            "A49 別 bot に返信せよ（Peer Review 節）",
        ),
        ("request_peer_review", "Peer Review 節本文"),
        (
            "レビュアーとして応答し",
            "導入部 req/reply マーカー文（§3.1 で撤去）",
        ),
        // --- テストレビュー指摘の否定側（旧命令残置による恒真を塞ぐ） ---
        // 点1: A43（Task Ledger の先合意強制・§6 事実形へ）
        (
            "FIRST agree the goal and acceptance criteria",
            "A43 先に合意せよ（§6 事実化で撤去）",
        ),
        (
            "Do not start executing before the contract is clear",
            "A43 契約前に着手するな（§6 事実化で撤去）",
        ),
        // 点2: A42/A44/A45（Task Ledger の旧命令文）
        (
            "trust it over your own recall",
            "A42 旧・自分の記憶より信頼せよ（§3.5 で takes precedence へ）",
        ),
        (
            "after each meaningful step",
            "A44 旧・各ステップ毎に呼べ（§3.5 事実化）",
        ),
        (
            "when the contract is met",
            "A45 旧・契約充足で close せよ（§3.5 事実化）",
        ),
        // 点3: A6/A10/A11（導入〜Silent Reply の旧文）
        (
            "but you must NOT include your own name prefix",
            "A6 旧・名前 prefix を入れるな（§3.1 事実化）",
        ),
        (
            "返答不要な場合は NO_REPLY とだけ",
            "A10 旧和文・NO_REPLY とだけ返せ（§3.1 事実化）",
        ),
        (
            "グループチャットで自分に関係ない会話の場合",
            "A11 旧和文・無関係な会話（§3.1 事実化）",
        ),
        (
            "既に話が完結している場合",
            "A11 旧和文・話が完結（§3.1 事実化）",
        ),
        // 点4: A39（Memory の旧手順文）
        (
            "to get the full conversation text",
            "A39 旧手順・全文取得（§3.4 事実化）",
        ),
        (
            "Use `browse_memory_index` to explore",
            "A39 旧手順・browse_memory_index（§3.4 事実化）",
        ),
    ];
    for (frag, label) in removed {
        let count = prompt.matches(frag).count();
        assert_eq!(
            count, 0,
            "撤去対象が残存（{label}）: count={count} / fragment={frag:?}\n{prompt}"
        );
    }
}

/// §1-2: §3 の書換後・事実文が存在する（節ごと代表 1–2 文）。
/// 各文は撤去された命令の対（否定側は上の removed_* テストが担保）。
#[test]
fn rewritten_fact_sentences_are_present() {
    let prompt = prompt();
    let present: &[(&str, &str)] = &[
        // 3.1 導入（A6 事実化）: 逐語投稿の事実
        ("Your response is posted verbatim", "A6 逐語投稿の事実"),
        // 3.1 Silent Reply（A10/A11 事実化）
        (
            "is not delivered or saved",
            "A10 NO_REPLY は配送も保存もされない",
        ),
        (
            "You may reply with NO_REPLY when a group-chat message does not involve you",
            "A11 沈黙は許容（命令でなく）",
        ),
        // 3.2 Async（A18 事実化・team-lead 指定例）
        (
            "Calling the same tool again for the same request starts a second, independent run",
            "A18 再呼び出しは 2 本目が走る（事実）",
        ),
        // 3.3 Continuing（A30 事実化）
        (
            "Each utterance call is delivered when it is made",
            "A30 各発話は呼んだ時点で配送（事実）",
        ),
        // 3.4 Memory（A39 事実化）: ツール契約
        (
            "returns the full conversation text",
            "A39 retrieve_memory_nodes の返り値（事実）",
        ),
        // 3.5 Task Ledger（A43 事実化・§6 裁定）
        (
            "records the contract for a multi-step task",
            "A43 open_task はコントラクトを記録（事実）",
        ),
        // 3.5 Task Ledger（A42 事実化・優先関係）
        (
            "takes precedence over your own recall",
            "A42 台帳が自分の記憶に優先（事実）",
        ),
    ];
    for (frag, label) in present {
        assert!(
            prompt.contains(frag),
            "書換後の事実文が無い（{label}）: fragment={frag:?}\n{prompt}"
        );
    }
}

/// §1-3: 「## Async Behavior」節が §3.2 の全文と一致（節単位・空白正規化）。
#[test]
fn async_behavior_section_matches_design() {
    let prompt = prompt();
    let expected = r#"## Async Behavior

Query tools (execute_shell and the like — anything you call to fetch a result) run
asynchronously: the result arrives later and you are called again with it in the conversation
history. Utterances (say / reply / reaction / repost) do not work this way — see "Continuing
your turn".

Some tools return `{status:"spawned", subtask_id: ...}` immediately instead of a final result.
The work is then running in the background, and its result arrives later in a separate turn as a
`[subtask_completed: ...]` entry. Calling the same tool again for the same request starts a
second, independent run, and the actual result appears only at the completion turn.

A `[subtask_completed: ...]` entry means a tool you called has finished and it is your turn
again."#;
    let got = extract_section(&prompt, "## Async Behavior");
    assert_eq!(
            normalize_ws(&got),
            normalize_ws(expected),
            "Async Behavior 節が §3.2 全文と一致しない\n--- got ---\n{got}\n--- expected ---\n{expected}"
        );
}

/// §1-3: 「## Continuing your turn」節が §3.3 の全文と一致（節単位・空白正規化）。
#[test]
fn continuing_your_turn_section_matches_design() {
    let prompt = prompt();
    let expected = r#"## Continuing your turn

Utterances (say / reply / reaction / repost) are fire-and-forget: they return no result and you
are not called again for them. Each utterance call is delivered when it is made, so N messages
require N calls in this one response.

Plain text in a response is posted as ONE message. To post several separate plain messages, post
the first, end that response with `CONTINUE` on its own line, and post the next in the following
response (repeat as needed).

After a response whose only actions are utterances, the turn ends. Ending a response with
`CONTINUE` on its own line — which may sit alongside a reply — calls you again in this same turn
with your speech already delivered, so you can keep working after speaking. Without `CONTINUE`
and without a query/tool call, the turn ends."#;
    let got = extract_section(&prompt, "## Continuing your turn");
    assert_eq!(
            normalize_ws(&got),
            normalize_ws(expected),
            "Continuing your turn 節が §3.3 全文と一致しない\n--- got ---\n{got}\n--- expected ---\n{expected}"
        );
}

/// §7: 「## Silent Reply」節が §3.1 の全文と一致（節単位・空白正規化）。
/// 期待文は §3.1 下書きから §6（A12 撤去）・§7（Peer Review ポインタ A13 撤去）を反映した後の形。
#[test]
fn silent_reply_section_matches_design() {
    let prompt = prompt();
    let expected = r#"## Silent Reply
A response of exactly NO_REPLY (with no other text in it) is not delivered or saved. You may
reply with NO_REPLY when a group-chat message does not involve you, or when the topic is already
resolved and a further exchange would add no new information."#;
    let got = extract_section(&prompt, "## Silent Reply");
    assert_eq!(
            normalize_ws(&got),
            normalize_ws(expected),
            "Silent Reply 節が §3.1（§6/§7 反映後）全文と一致しない\n--- got ---\n{got}\n--- expected ---\n{expected}"
        );
}

/// §7: 「## Memory & Context」節が §3.4 の全文と一致（節単位・空白正規化）。
#[test]
fn memory_section_matches_design() {
    let prompt = prompt();
    let expected = r#"## Memory & Context

Long conversations are automatically compacted: older messages are replaced with a
[Past context summary] section of topic summaries with node IDs (e.g. [topic-xxx-1-20]).
- `retrieve_memory_nodes(node_id)` returns the full conversation text for a node.
- `browse_memory_index` lists all past topics beyond those shown in the summary.
- `search_memory_index` searches past topics by keyword and returns matching nodes to retrieve.

These tools reach your full history even after compaction."#;
    let got = extract_section(&prompt, "## Memory & Context");
    assert_eq!(
            normalize_ws(&got),
            normalize_ws(expected),
            "Memory & Context 節が §3.4 全文と一致しない\n--- got ---\n{got}\n--- expected ---\n{expected}"
        );
}

/// §7: 「## Task Ledger」節が §3.5 の全文と一致（節単位・空白正規化）。
/// 空 DB では Task Ledger 以降の節（skills/character/instructions/curated）は空なので
/// 節末が prompt 末尾になる（extract_section は次 "## " が無ければ末尾まで切り出す）。
#[test]
fn task_ledger_section_matches_design() {
    let prompt = prompt();
    let expected = r#"## Task Ledger

You have a persistent, DB-backed task ledger that survives context compaction and restarts.
When a [Task Ledger] section appears in the conversation, it is the authoritative current
working state and takes precedence over your own recall.
- `open_task(goal, acceptance_criteria)` records the contract for a multi-step task.
- `record_task_progress` appends a step; `kind=decision` records a decision with its why,
  `kind=blocker` an obstacle.
- `close_task(status=done|abandoned)` closes the task; `update_task_contract` revises the
  criteria.
- Trivial single-message replies do not need a ledger entry."#;
    let got = extract_section(&prompt, "## Task Ledger");
    assert_eq!(
        normalize_ws(&got),
        normalize_ws(expected),
        "Task Ledger 節が §3.5 全文と一致しない\n--- got ---\n{got}\n--- expected ---\n{expected}"
    );
}

/// §6 A49/§7: Peer Review 節・レビュアー名簿（A57）・導入部マーカー文（A13）を丸ごと撤去。
/// 名簿は登録レビュアーが居ないと空になり恒真化するため、co-agent レビュアーを seed した上で
/// 「名簿が出ない」ことを観測する（現 tip では seed すると名簿が出るので **赤**）。
#[test]
fn peer_review_section_and_roster_are_removed() {
    let conn = opencrab_db::init_memory().unwrap();
    // 表示名つき co-agent レビュアーを seed（現 tip なら名簿に載る）。
    opencrab_db::queries::add_trusted_user(
        &conn,
        opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
        "r1",
        "a1",
        "42",
        opencrab_db::queries::TrustedUserPermission::CoAgent,
        "owner",
        "2026-01-01",
        "Crab B",
    )
    .unwrap();
    let (prompt, _name) =
        build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);

    for (frag, label) in [
        ("## Peer Review", "Peer Review 節見出し"),
        ("Your registered peer reviewers", "A57 レビュアー名簿見出し"),
        ("Crab B", "seed したレビュアー表示名（名簿本体）"),
        ("request_peer_review", "Peer Review 節本文"),
        ("レビュアーとして応答し", "A13 導入部マーカー文"),
    ] {
        assert_eq!(
            prompt.matches(frag).count(),
            0,
            "Peer Review 撤去が未達（{label}）: fragment={frag:?}\n{prompt}"
        );
    }
}
