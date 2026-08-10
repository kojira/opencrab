//! 会話文字列の組み立て（トークン予算ベースのコンパクション対応）。
//!
//! セッションログから LLM へ渡す会話文字列を構築する。`build_ledger_section`
//! （[`crate::task_ledger`]）/ `build_impression_section`（[`crate::impression_section`]）
//! と同型で、`conn` を取り会話用のセクションを組む純粋ロジック。server / gateway の型に
//! 依存しないため core に置く（#518 手順 3〜4）。呼び出し元は `server::process`
//! （既存パスを保つ再エクスポート）。

use crate::tokens::estimate_tokens;

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
pub const RECENT_MIN_USER_SPEECHES: usize = 5;

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
    let prefix_sections =
        build_context_prefix_sections(conn, session_id, agent_id, context_budget_tokens);

    let mut inner_budget = context_budget_tokens;
    for section in &prefix_sections {
        inner_budget = inner_budget.saturating_sub(estimate_tokens(section));
    }
    let inner = build_conversation_inner(conn, session_id, agent_id, inner_budget)?;

    let mut parts = prefix_sections;
    parts.push(inner);
    Ok(parts.join("\n\n"))
}

/// 会話本文の前に置く固定セクション（台帳 / [Memory Index] / [Impressions]）を組む。
///
/// すべて `session_id` を「いま走っているセッション」として解決する。best-effort で、
/// どれが欠けても会話構築は続行する。
fn build_context_prefix_sections(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    context_budget_tokens: usize,
) -> Vec<String> {
    // タスク台帳（前向きワーキング状態）を会話の先頭に前置する。
    // system prompt 側は 1h キャッシュされるため、毎ターン変わる台帳状態はここに置く。
    // 台帳の読み出し失敗で返信自体を殺さない（warn して台帳なしで続行）。
    let ledger_section = match crate::task_ledger::build_ledger_section(conn, agent_id, session_id)
    {
        Ok(section) => section,
        Err(e) => {
            tracing::warn!("failed to build task ledger section for session {session_id}: {e}");
            None
        }
    };

    // [Memory Index]: 長期記憶のコンパクトな目次を常時前置する（月次要約 + 本人が
    // 宣言した記憶の単位 + 未宣言の現在月 topic、short_id 付き）。台帳と同じく
    // 「動的状態は会話側」（system は 1h キャッシュ）。best-effort — 失敗しても
    // 返信は殺さない。
    // コンパクション時の [Past context summary]（build_conversation_inner 内、
    // 現セッションの topic のみ）とは役割が異なり、こちらは現セッション由来の
    // topic を除外するため short_id が両方に出ることはない（invariant）。
    // 宣言ユニットはエージェント単位（生涯スコープ）でこちらにだけ出る
    // （get_topic_nodes_for_session は node_type='topic' しか拾わない / #403）。
    let memory_index_section =
        match crate::memory_index::build_memory_index_section(conn, agent_id, session_id) {
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
    let impression_section =
        match crate::impression_section::build_impression_section(conn, agent_id, session_id) {
            Ok(section) => section,
            Err(e) => {
                tracing::warn!("failed to build impression section for session {session_id}: {e}");
                None
            }
        };

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
    parts
}

/// ハートビート文脈に載せる実会話セクションのヘッダ（#404）。
///
/// **「あなた宛とは限らない」「出力形式は末尾の指示に従う」を明示する**のが要点。
/// このセクションは実会話（人の発言）なので、素で入れると「直近の発言に返事をする」
/// 形へ引っ張られ、`SPEAK:` / `IDLE` の出力形式を落としうる。形式指示はハートビート
/// 専用セッション側の末尾に残る（[`build_heartbeat_conversation_string`] の並び順）。
///
/// 「発言だけを載せる」ことも明示する（[`CHANNEL_CONVERSATION_LOG_TYPE`]）。名乗りと
/// 中身が食い違うと、モデルは見えていない往復を見たものとして扱いうる。**自分の発言も
/// 入る**ことを書くのも同じ理由: 本番のツール往復が多いチャンネルではこのセクションの
/// 9 割（文字）が自分の発話で、`people said` とだけ名乗るのは実態と食い違う。
const CHANNEL_CONVERSATION_HEADER: &str = "[Channel conversation] (the most recent messages actually exchanged in this channel, including your own. Tool calls, tool results, system events and older messages are not shown here. You are not necessarily being addressed here; follow the response-format instruction at the end of this prompt.)\n";

/// 実会話セクションで読む直近ログの窓（件数）。**[`CHANNEL_CONVERSATION_LOG_TYPE`] で
/// 絞ったあと**の件数（絞り込みは SQL 側で掛ける）。
///
/// 全件読み（`list_session_logs_by_session`）を避けるためのもので、本番の実会話
/// セッションは最大 5,383 行 / 851KB あり、毎 tick これを `db.lock()` を握ったまま
/// tiktoken に通すのは高い。
const CHANNEL_CONVERSATION_LOG_WINDOW: usize = 500;

/// ハートビート文脈で実会話へ割く予算の割合（分子／分母）。
///
/// 残りがハートビート専用セッションの履歴に回る。実会話を厚くするのは #404 の実測に
/// よる: 同一チャンネルでハートビート専用セッション 15,246 行に対し実会話 11,396 行が
/// あり、前者は「静けさは続いてる」等の自分の警戒心の反復で情報密度が低い。
/// 専用セッション側は「直前に何を喋ったか（同じことを繰り返さない）」と末尾の出力形式
/// 指示さえ残れば足り、これらは `fit_logs_to_budget` の直近下限（`RECENT_MIN_LOGS`）で
/// 予算 0 でも保たれる。
const HEARTBEAT_CHANNEL_BUDGET_NUM: usize = 3;
const HEARTBEAT_CHANNEL_BUDGET_DEN: usize = 4;

/// ハートビート用の会話文字列を組む（#404）。
///
/// ハートビートは `heartbeat-{agent_id}-{channel_id}` という専用セッションで走るため、
/// 素の [`build_conversation_string`] では**同じチャンネルの実会話が 1 行も見えない**。
/// ここでは専用セッションの履歴に加えて、`channel_session_id`（実会話）を
/// `[Channel conversation]` として差し込む。
///
/// 並び順は `前置セクション → 実会話 → ハートビート専用セッション` で固定する。
/// 専用セッションの**最後のログが今回のハートビートプロンプト（出力形式の規約を含む）**
/// なので、これを末尾に置くことで `SPEAK:` / `IDLE` のパース前提が変わらない。
///
/// コンパクションはセッションを跨がない: 専用セッション側は従来どおり自分の topic 要約
/// （`[Past context summary]`）でコンパクションし、実会話側は topic 要約を使わず直近窓
/// で切る。実会話の長期の要約は `[Memory Index]`（別セッション由来の topic を載せる）が
/// 既に担っており、`[Past context summary]` を 2 つ出すと short_id 集合が素という
/// 既存の不変条件が壊れるため。
///
/// 実会話セクションに載せるのは**発言だけ**（[`CHANNEL_CONVERSATION_LOG_TYPE`]）。
///
/// `channel_session_id` が None（エージェント単位 tick 等）または発言が 1 件も無ければ、
/// 出力は [`build_conversation_string`] と同一になる。
pub fn build_heartbeat_conversation_string(
    conn: &rusqlite::Connection,
    heartbeat_session_id: &str,
    channel_session_id: Option<&str>,
    agent_id: &str,
    context_budget_tokens: usize,
) -> Result<String, anyhow::Error> {
    let prefix_sections =
        build_context_prefix_sections(conn, heartbeat_session_id, agent_id, context_budget_tokens);

    let mut inner_budget = context_budget_tokens;
    for section in &prefix_sections {
        inner_budget = inner_budget.saturating_sub(estimate_tokens(section));
    }

    let channel_section = channel_session_id
        .filter(|id| !id.is_empty() && *id != heartbeat_session_id)
        .and_then(|id| {
            build_channel_conversation_section(
                conn,
                id,
                agent_id,
                inner_budget / HEARTBEAT_CHANNEL_BUDGET_DEN * HEARTBEAT_CHANNEL_BUDGET_NUM,
            )
        });

    // 実会話が実際に使った分だけ引く（予算内に収まったチャンネルは残りを専用セッション
    // へ返す）。直近下限で割当を超えることもあるため saturating。
    let heartbeat_budget = match &channel_section {
        Some(s) => inner_budget.saturating_sub(estimate_tokens(s)),
        None => inner_budget,
    };
    let inner = build_conversation_inner(conn, heartbeat_session_id, agent_id, heartbeat_budget)?;

    let mut parts = prefix_sections;
    if let Some(s) = channel_section {
        parts.push(s);
    }
    parts.push(inner);
    Ok(parts.join("\n\n"))
}

/// 実会話セクションに載せる log_type（#404）。**これが唯一の判定元**で、
/// [`build_channel_conversation_section`] は SQL の `WHERE log_type = ?` にそのまま渡す。
/// Rust 側に同じ述語を二重に置かない（ずれると「絞ったつもりで漏れる／落としすぎる」）。
///
/// **`speech` だけを載せる。** #404 が直したいのは「エージェントが自分の過去の発話しか
/// 手本を持てず、2 ヶ月ほぼ同一の発話を繰り返していた」ことなので、要るのは**何が話され
/// たか**であって自分のツール往復ではない。本番のツール往復が多いチャンネルでは
/// 直近 500 行の内訳が tool_result 162 行 / 104KB・system 55 行 / 50KB・speech 184 行 /
/// 49KB・tool_call 95 行 / 13KB で、**speech はバイトで 2 割強**しかない。素通しだと
/// 実会話へ割いた枠の大半をツール結果が食い、目的が達成されない。
///
/// 落とすものの内訳:
/// - `tool_call` / `tool_result` / `tool_cancelled`: 自分の作業機構であって発言ではない。
///   結果そのものは飛ぶが、それを人へ説明した直後の自分の `speech` は残る。
/// - `system`: Discord セッションでは subtask の生成・完了・タイムアウト・進捗報告の
///   JSON（本番の全 724 行がこれ）。これも自分の機構で、1 行あたりが最も重い
///   （ハートビート対象チャンネルの最大は 1 行 134,863 文字 = それだけで予算超過）。
/// - `inner_voice`: `generate_inner_voice` が書く**自分の内心**。自己参照を増やす方向に
///   働くので、この目的では最も入れたくない。
/// - `evaluation`: 元から会話に載せない（#291）。
/// - `interaction_response`: 人が UI を操作したことの記録（本番全 DB で 16 行、
///   ハートビート対象チャンネルには 0 行）。**発言ではない**ので落とすが、これは人の
///   行動の記録であって上の「自分の機構」とは性質が違う。人の**発言**を落とす根拠には
///   しないこと。
///
/// **この絞り込みは実会話セクションだけに掛ける。**ハートビート専用セッション側は
/// 従来どおり全種別を載せる — subtask の完了本文が次 tick で文脈へ載ることに
/// `heartbeat_run_request` の `NoopCompletionSink` が依存している。
const CHANNEL_CONVERSATION_LOG_TYPE: &str = "speech";

/// 実会話セッションを `[Channel conversation]` セクションへ整形する（#404）。
///
/// [`CHANNEL_CONVERSATION_LOG_TYPE`] の直近 [`CHANNEL_CONVERSATION_LOG_WINDOW`] 件を
/// 取り、予算内へ詰める。**絞り込みは SQL 側**で、窓 N 件は絞ったあとの件数になる
/// （生の N 件から捨てると、ツール往復の多いチャンネルで窓の一部しか発言が残らず、
/// 余った予算がハートビート履歴側へ戻ってしまう — #405 レビュー 2 巡目）。
/// 発言が 1 件も無ければ `None`（セクションごと出さない）。topic 要約は使わない
/// （理由は [`build_heartbeat_conversation_string`] の doc）。
fn build_channel_conversation_section(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    budget_tokens: usize,
) -> Option<String> {
    let mut logs = match opencrab_db::queries::list_recent_session_logs_of_type(
        conn,
        session_id,
        CHANNEL_CONVERSATION_LOG_TYPE,
        CHANNEL_CONVERSATION_LOG_WINDOW,
    ) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(session_id = %session_id, "failed to list channel conversation logs: {e}");
            return None;
        }
    };
    // #425: HB 由来の自己エコー（表示専用の二重記録）は、同じ発話が heartbeat 専用
    // セッション側にも `SPEAK: …` として載っており、この関数を呼ぶ
    // [`build_heartbeat_conversation_string`] は両方を読む。実会話セクションでそのまま
    // 出すと二重表示になるので、印の付いた行だけを落とす。印の無い本人の非 HB 発話・
    // 他者発話は残す（#404 の「自分の発言も含める」意図を壊さない）。ここでの絞り込みは
    // 通常の Discord 返信が読む [`build_conversation_string`] には掛からない（別関数）。
    logs.retain(|l| !opencrab_db::queries::is_heartbeat_channel_echo(l.metadata_json.as_deref()));
    logs.reverse();
    // #284 と同じ保証を実会話セクションでも効かせる。人の発言が窓（直近 500 発言）から
    // 溢れていても直近の分は混ぜ戻す（取得は id 順にマージされる）。
    let logs = merge_recent_user_speeches(conn, session_id, agent_id, logs);
    if logs.is_empty() {
        return None;
    }
    let body_budget = budget_tokens.saturating_sub(estimate_tokens(CHANNEL_CONVERSATION_HEADER));
    let body = fit_logs_to_budget(&logs, agent_id, body_budget);
    if body.is_empty() {
        return None;
    }
    Some(format!("{CHANNEL_CONVERSATION_HEADER}{body}"))
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

    // [Past context summary] セクション構築（予算の 30% 以内 / 新しい方を残す。#406）
    let summary_section = build_past_context_summary_section(
        &topics,
        context_budget_tokens / PAST_SUMMARY_BUDGET_DEN * PAST_SUMMARY_BUDGET_NUM,
    );

    // 要約が落ちた（予算が極小）ときは先頭の空行を作らない。
    let recent_header = if summary_section.is_empty() {
        "[Recent conversation]\n"
    } else {
        "\n\n[Recent conversation]\n"
    };
    let overhead_tokens = estimate_tokens(&summary_section) + estimate_tokens(recent_header);

    // 残りの予算を最近のログに割り当て。要約は 30% で頭打ちなので、直近会話には
    // 常に 50% 以上（ヘッダぶんを除いて ~70%）が残る（#406）。
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

    Ok(format!("{summary_section}{recent_header}{recent_text}"))
}

/// `[Past context summary]` に割く文脈予算の割合（分子／分母 = 30%）。
///
/// **オーナー指定の配分（#406）**: 長期 20%（`[Memory Index]`。既存の 1/4 ガードが
/// 掛かっており段階 1 では触らない）／短期 30%（このセクション）／直近 50%
/// （`[Channel conversation]` + `[Recent conversation]`）。
///
/// なぜ上限が要るか（実測。次に読む人が測り直さずに済むように残す）: このセクションは
/// **そのセッションの topic 要約の全件連結**で、長寿命セッションほど無限に伸びる。
/// 本番のハートビート専用セッションでは topic 2,446 件・要約の総文字数 248,340 に達し、
/// 上限が無いため `remaining_budget` が 0 に張り付き、**会話本文は `RECENT_MIN_LOGS`
/// 件しか残らなかった**。1 ハートビートあたりの入力が 284,486 トークン（キャッシュ読 0 /
/// 35 秒）で、同条件の別エージェント（38,078 トークン）の 7.5 倍。載っていたのは
/// ほぼ自分の過去出力の要約で、当該エージェントは 2 ヶ月ほぼ同一の発話を繰り返していた。
///
/// **割合の基準は「このセクションを組む関数に渡された予算」**であって全体予算ではない。
/// ハートビート経路では `[Channel conversation]` が先に `inner_budget × 3/4` を取り、
/// 残りが [`build_conversation_inner`] へ渡る（[`build_heartbeat_conversation_string`]）。
/// ここで全体予算の 30% を基準にすると、渡された予算を要約が丸ごと食い潰して
/// **上の症状がそのまま再現する**。渡された予算に対する割合にすることで、
/// 「実会話を優先し、ハートビート履歴は下限だけ」というオーナー決定とも向きが揃う。
const PAST_SUMMARY_BUDGET_NUM: usize = 3;
const PAST_SUMMARY_BUDGET_DEN: usize = 10;

/// `[Past context summary]` のヘッダ。**予算判定にはこのヘッダぶんも含める**
/// （セクション全体で 30% に収める）。
const PAST_SUMMARY_HEADER: &str =
    "[Past context summary (use retrieve_memory_nodes with short_id to recall details)]\n";

/// 予算に入らず落とした topic 要約があることを本人へ伝える 1 行（#406）。
///
/// **落としたことを黙らない。** 本番では 2,400 件級が文脈から消えるので、この 1 行が
/// 唯一の復旧導線になる。したがって**書いてある呼び方が実際に通ること**が要件で、
/// [`past_summary_budget_tests::omitted_notice_matches_the_real_tool_surface`] で固定する。
///
/// short_id は落ちた行と一緒に消えているため、`retrieve_memory_nodes` を直接は叩けない
/// （`node_ids` 必須 / 1〜5 件。`crates/actions/src/memory_access.rs`）。**キーワードも
/// 日付範囲も受け取らない**し、日付範囲を取る記憶検索ツールはそもそも存在しない。
/// system prompt 側と同じ導線（`search_memory_index` で逆引き → ヒットした short_id を
/// `retrieve_memory_nodes` へ）を書く。
pub fn past_summary_omitted_notice(dropped: usize) -> String {
    format!(
        "- [... {dropped} older topic summaries were omitted to fit the context budget. \
         They are not lost: call search_memory_index(query) to find them, \
         then retrieve_memory_nodes on a hit ...]"
    )
}

/// `[Past context summary]` セクションを予算内で組む（#406）。
///
/// **切り詰めの向き: 新しい方を残し、古い方から落とす。** 供給元の
/// `get_topic_nodes_for_session` は `ORDER BY start_log_id ASC`（＝**古い順**）なので、
/// 素直に前から詰めると古い方だけが残る。ここでは**末尾（新しい方）から**予算いっぱいまで
/// 詰め、最後に表示順を時系列（古い→新しい）へ戻す。
///
/// 予算にセクション全体（ヘッダ + 省略の告知 + 残した行）が入らなければ空文字を返す
/// （＝セクションごと出さない）。`[Memory Index]` の 1/4 ガードと同じ扱いで、
/// 予算が極小のときに「告知だけで予算を使い切る」ことを避ける。
///
/// **コストは `O(残す件数)`。** 整形（`format!`）と計測（`estimate_tokens`）は末尾から
/// 逐次行い、予算を超えた時点で止める。それより古い topic には触れない。この関数は毎ターン
/// `db.lock()` を握ったまま呼ばれ（`main.rs` の会話構築）、本番の最大セッションは
/// topic 2,450 件 / title+summary 248,884 文字あるのに実際に残るのは 29 件なので、
/// 全件を tiktoken に通すのは丸ごと無駄になる。[`CHANNEL_CONVERSATION_LOG_WINDOW`] に
/// 窓を入れたのと同じ理由（#405 / #406 レビュー）。
fn build_past_context_summary_section(
    topics: &[opencrab_db::queries::IndexNodeRow],
    budget_tokens: usize,
) -> String {
    // node_id を併記してエージェントが retrieve_memory_nodes で全文検索できるようにする
    let format_line = |t: &opencrab_db::queries::IndexNodeRow| {
        let key = t.short_id.as_deref().unwrap_or(&t.id);
        let date_hint = match (t.date_from.as_deref(), t.date_to.as_deref()) {
            (Some(from), Some(to)) if from == to => format!(" ({})", &from[5..]),
            (Some(from), Some(to)) => format!(" ({}~{})", &from[5..], &to[5..]),
            (Some(from), None) => format!(" ({})", &from[5..]),
            _ => String::new(),
        };
        format!("- [{}]{} {}: {}", key, date_hint, t.title, t.summary)
    };

    let header_tokens = estimate_tokens(PAST_SUMMARY_HEADER);
    // 新しい方（末尾）から予算いっぱいまで詰める。`kept` は**新しい順**に積まれる。
    let mut kept: Vec<String> = Vec::new();
    let mut kept_tokens: Vec<usize> = Vec::new();
    let mut used = 0usize;
    let mut dropped = 0usize;
    for (i, t) in topics.iter().enumerate().rev() {
        let line = format_line(t);
        let cost = estimate_tokens(&line) + 1; // +1 for newline
        if header_tokens + used + cost > budget_tokens {
            // これより古い側は整形も計測もせずに落とす。
            dropped = i + 1;
            break;
        }
        used += cost;
        kept.push(line);
        kept_tokens.push(cost);
    }

    if dropped == 0 {
        // 先頭まで到達した = 全件が予算内。告知は出さない。
        if kept.is_empty() {
            return String::new();
        }
        kept.reverse();
        return format!("{PAST_SUMMARY_HEADER}{}", kept.join("\n"));
    }

    // 告知の 1 行ぶんは後から確保する。残した中の**最古**（= `kept` の末尾）から外す。
    // 告知の長さは件数の桁でしか変わらないので、この縮めは高々数回で収束する。
    let mut notice = past_summary_omitted_notice(dropped);
    let mut notice_tokens = estimate_tokens(&notice) + 1;
    while !kept.is_empty() && header_tokens + notice_tokens + used > budget_tokens {
        used -= kept_tokens.pop().unwrap_or(0);
        kept.pop();
        dropped += 1;
        notice = past_summary_omitted_notice(dropped);
        notice_tokens = estimate_tokens(&notice) + 1;
    }
    if header_tokens + notice_tokens + used > budget_tokens {
        // 告知すら入らない極小予算。セクションごと出さない。
        return String::new();
    }

    // 表示順を時系列（古い→新しい）へ戻す。
    kept.reverse();
    let mut body = vec![notice];
    body.extend(kept);
    format!("{PAST_SUMMARY_HEADER}{}", body.join("\n"))
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

/// heartbeat セッションで過去に積まれた指示文（プロンプト scaffolding）か（#501）。
///
/// 以前は `scheduler.rs::run_one_fire` が発火のたびに `log_type='system'` かつ
/// `speaker_id='heartbeat'` で同一文面の指示文（「[ハートビート] 現在の会話…出力形式:
/// SPEAK/LEARN/IDLE」）をセッションログへ挿入していた。毎 tick 積まれて会話へ再注入され、
/// 「同じ指示 → IDLE」の対が何十回も文脈に並んで挙動を歪めていた（本番の heartbeat
/// セッションでは system の 192 件がこの重複）。**#501 で指示文は system プロンプトへ移し**
/// （`heartbeat_turn::build_context`）、書き込み側（scheduler）は挿入をやめた。既存 DB に
/// 積まれた分は DB を書き換えず、会話再構成でここが落とす。
///
/// subtask の完了本文（`settle_completed` が書く `system` かつ **`speaker_id=None`**,
/// #404 / #405）とは `speaker_id` で区別する。完了本文は次 tick で読む契約があるので
/// **落とさない**。判定は `memory_index::is_heartbeat_noise` と同じ述語で、
/// `speaker_id='heartbeat'` を書くのは（過去も含め）`run_one_fire` だけ（grep 済み）。
fn is_heartbeat_prompt_scaffolding(log: &opencrab_db::queries::SessionLogRow) -> bool {
    log.log_type == "system"
        && log.speaker_id.as_deref() == Some(opencrab_db::queries::HEARTBEAT_SPEAKER_ID)
}

/// 会話文字列に載せるログだけを残す（#291 / #501）。
///
/// `evaluation` を落とす（#291）のに加え、heartbeat 指示文 scaffolding
/// （[`is_heartbeat_prompt_scaffolding`]）は**会話から全件落とす**（#501）。指示文はその
/// tick の system プロンプトへ 1 度だけ入る（`heartbeat_turn::build_context`）ようになったので
/// 会話履歴には不要。新規ターンはそもそも書かない（`scheduler.rs`）が、既存 DB に積まれた分
/// （本番で 192 件）は DB を書き換えず読み出し側でここが落とす。subtask 完了本文
/// （`system` かつ `speaker_id=None`, #404 / #405）は `speaker_id` で区別して残す。
fn retain_conversation_logs(
    logs: Vec<opencrab_db::queries::SessionLogRow>,
) -> Vec<opencrab_db::queries::SessionLogRow> {
    logs.into_iter()
        .filter(|l| !is_excluded_from_conversation(l))
        .filter(|l| !is_heartbeat_prompt_scaffolding(l))
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

/// 連続区間より前に落とした古いメッセージ群に添える印（#504）。
///
/// 飛び地としての生発言（文脈も応答有無も分からないユーザー発言）は載せないが、
/// 「何かがあった」ことは伝わるべきなので、落とした件数と期間（先頭〜末尾の
/// タイムスタンプ差）を書く。表記は従来の英語マーカーに揃える。
fn format_omission_marker(omitted: &[opencrab_db::queries::SessionLogRow]) -> String {
    let count = omitted.len();
    let noun = if count == 1 { "message" } else { "messages" };
    match omission_span_label(omitted) {
        Some(span) => {
            format!("[... {count} older {noun} over {span} omitted due to context length ...]")
        }
        None => format!("[... {count} older {noun} omitted due to context length ...]"),
    }
}

/// 落とした区間の期間ラベル（先頭と末尾の `created_at` の差）。
///
/// ログは時系列順なので `first` が最古・`last` が最新。どちらかの `created_at` が
/// 無い／パースできなければ `None`（マーカーは件数だけになる）。
fn omission_span_label(omitted: &[opencrab_db::queries::SessionLogRow]) -> Option<String> {
    let parse = |log: &opencrab_db::queries::SessionLogRow| {
        log.created_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    };
    let first = parse(omitted.first()?)?;
    let last = parse(omitted.last()?)?;
    let dur = last - first;
    let unit = |n: i64, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
    let days = dur.num_days();
    if days >= 1 {
        return Some(unit(days, "day"));
    }
    let hours = dur.num_hours();
    if hours >= 1 {
        return Some(unit(hours, "hour"));
    }
    let minutes = dur.num_minutes();
    if minutes >= 1 {
        return Some(unit(minutes, "minute"));
    }
    None
}

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

/// ログを末尾（最新）から逆順に辿り、予算内に収まる分だけ返す。
///
/// 保証は 2 つ:
/// - 最低 `RECENT_MIN_LOGS` 件は常に含める（従来どおり）。
/// - 直近 `RECENT_MIN_USER_SPEECHES` 件のユーザー発言は**予算より先に枠を取る**。
///   これにより末尾の連続区間が直近のユーザー発言まで届き、巨大なツール結果が
///   末尾を占めてもユーザーの指示は連続区間内に載る（#284）。
///
/// **連続区間の外に押し出されたユーザー発言（＝飛び地）は原則載せない**（#504）。
/// 文脈も応答有無も失われた裸の発言は「無いより悪い」ため。ただし A′ の決定で、
/// **一番新しいユーザー発言 1 件だけは飛び地でも必ず載せる**（＝「今の指示」）。
/// それより古い飛び地は落とし、件数と期間を書いた省略マーカーに集約する
/// （[`format_omission_marker`]）。枠取り自体は残すので #284 の届き方は変わらない。
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

    // 連続区間の外にある必須ユーザー発言（＝飛び地）は、文脈も応答有無も失われた
    // 裸の発言になり「無いより悪い」ため原則載せない（#504）。ただし A′ の決定に従い
    // **一番新しいユーザー発言 1 件だけは飛び地でも必ず載せる** — これが「今の指示」で、
    // #284 が本当に守りたかったもの。それより古い飛び地（連投の言い直し等）は落とす。
    // `must` の枠取り（tail_budget から先取り）自体は残しているので、連続区間が直近の
    // ユーザー発言まで届く #284 の効果は保たれ、届かないほど古い連投だけが飛び地になる。
    //
    // `must` は直近のユーザー発言集合なので `max()` がそのまま「一番新しいユーザー発言」。
    // それが連続区間内（`>= start_idx`）なら飛び地は不要（None）。
    let forced_orphan = must.iter().copied().max().filter(|&i| i < start_idx);

    let mut parts: Vec<String> = Vec::with_capacity(formatted.len() - start_idx + 3);
    match forced_orphan {
        Some(idx) => {
            // 飛び地より前に落とした分（件数＋期間で「何かがあった」ことを残す）。
            if idx > 0 {
                parts.push(format_omission_marker(&logs[..idx]));
            }
            // 一番新しいユーザー発言（飛び地でも必ず載せる）。
            parts.push(formatted[idx].clone());
            // 飛び地と連続区間のあいだに落とした分（古い飛び地の連投を含む。
            // ループは非 must 行で break するため `idx + 1 < start_idx` が保証され非空）。
            parts.push(format_omission_marker(&logs[idx + 1..start_idx]));
        }
        None => {
            // 一番新しいユーザー発言は連続区間に入っている（または must が空）。
            // 連続区間より前に落とした分だけを 1 つのマーカーに集約する。
            if start_idx > 0 {
                parts.push(format_omission_marker(&logs[..start_idx]));
            }
        }
    }
    parts.extend(formatted[start_idx..].iter().cloned());
    parts.join("\n")
}

fn format_logs(logs: &[opencrab_db::queries::SessionLogRow]) -> String {
    logs.iter()
        .map(format_single_log)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn format_single_log(log: &opencrab_db::queries::SessionLogRow) -> String {
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

/// #404: ハートビート文脈に同じチャンネルの実会話が入ることを固定する。
///
/// 症状は「ハートビートが自分の過去のハートビートしか見えない」。実会話が入ること、
/// 専用セッションの履歴と共存すること、予算逼迫時も実会話が優先されること、そして
/// **出力形式の指示が末尾に残る**（`SPEAK:` パースの前提が変わらない）ことを見る。
#[cfg(test)]
mod heartbeat_conversation_tests {
    use super::{build_conversation_string, build_heartbeat_conversation_string};

    const AGENT: &str = "a1";
    const HB_SESSION: &str = "heartbeat-a1-222";
    const CH_SESSION: &str = "discord-a1-111-222";
    /// main.rs がハートビートプロンプト末尾に付ける規約（ランタイム固定部分）。
    const FORMAT_INSTRUCTION: &str = "出力形式: SPEAK/LEARN/IDLE のいずれか。SPEAKの場合のみ 'SPEAK: <メッセージ>' の形式で一言。";

    fn insert(
        conn: &rusqlite::Connection,
        session_id: &str,
        log_type: &str,
        speaker: &str,
        content: String,
    ) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: session_id.to_string(),
                log_type: log_type.to_string(),
                content,
                speaker_id: Some(speaker.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    /// 実会話: 人の発言（speaker != agent）とエージェントの返信が並ぶ。
    fn seed_channel(conn: &rusqlite::Connection, n: usize) {
        for i in 0..n {
            insert(
                conn,
                CH_SESSION,
                "speech",
                "human-1",
                format!("channel message {i} from a person talking about the release plan"),
            );
            insert(
                conn,
                CH_SESSION,
                "speech",
                AGENT,
                format!("channel reply {i} from the agent about the release plan"),
            );
        }
    }

    /// ハートビート専用セッション: 過去に積まれた指示文（system/heartbeat）と自分の判断
    /// （speech）の反復。#501 以降 scheduler は指示文をログへ書かないが、既存 DB には残るため
    /// **会話再構成で除外されること**をこのシードで検証する（speech は残る）。
    fn seed_heartbeat(conn: &rusqlite::Connection, n: usize) {
        for i in 0..n {
            insert(
                conn,
                HB_SESSION,
                "system",
                "heartbeat",
                format!("[ハートビート] 現在の会話「開発」。静けさが続くなら黙っていてよい。\n{FORMAT_INSTRUCTION}"),
            );
            insert(
                conn,
                HB_SESSION,
                "speech",
                AGENT,
                format!("heartbeat note {i}: 静けさは続いてる。IDLE"),
            );
        }
        insert(
            conn,
            HB_SESSION,
            "system",
            "heartbeat",
            format!("[ハートビート] 現在の会話「開発」。静けさが続くなら黙っていてよい。\n{FORMAT_INSTRUCTION}"),
        );
    }

    /// ツール往復・system イベント・内心。**実会話セクションには出さない側**。
    /// 本番のツール往復が多いチャンネルの内訳（直近 500 行で tool_result 104KB /
    /// system 50KB に対し speech 49KB）を縮めて再現する。
    fn seed_channel_tool_traffic(conn: &rusqlite::Connection, n: usize) {
        for i in 0..n {
            insert(
                conn,
                CH_SESSION,
                "tool_call",
                AGENT,
                format!("TOOLCALL-{i} execute_shell"),
            );
            insert(
                conn,
                CH_SESSION,
                "tool_result",
                AGENT,
                format!("TOOLPAYLOAD-{i} {}", "x".repeat(300)),
            );
            insert(
                conn,
                CH_SESSION,
                "system",
                AGENT,
                format!(
                    r#"{{"exit_reason":"completed","result":"SUBTASKEVENT-{i}","session_id":"subtask-x"}}"#
                ),
            );
            insert(
                conn,
                CH_SESSION,
                "inner_voice",
                AGENT,
                format!("INNERVOICE-{i} 自分の内心をここに書いている"),
            );
        }
    }

    /// 両セッションに現在月の topic を 1 件ずつ置く（`[Memory Index]` と
    /// `[Past context summary]` の担当分けを見るため）。
    fn seed_topics_for_both_sessions(conn: &rusqlite::Connection) {
        use opencrab_db::queries::*;
        let mk = |id: &str,
                  node_type: &str,
                  parent: Option<&str>,
                  title: &str,
                  source_session_id: Option<&str>,
                  date_from: Option<&str>| IndexNodeRow {
            id: id.to_string(),
            agent_id: AGENT.to_string(),
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
        };
        insert_index_node(conn, &mk("r1", "root", None, "root", None, None)).unwrap();
        insert_index_node(
            conn,
            &mk("pjun", "period", Some("r1"), "2026-06", None, None),
        )
        .unwrap();
        insert_index_node(conn, &mk("s1", "session", Some("pjun"), "S", None, None)).unwrap();
        insert_index_node(
            conn,
            &mk(
                "t-ch",
                "topic",
                Some("s1"),
                "実会話の話題",
                Some(CH_SESSION),
                Some("2026-06-10"),
            ),
        )
        .unwrap();
        insert_index_node(
            conn,
            &mk(
                "t-hb",
                "topic",
                Some("s1"),
                "ハートビートの話題",
                Some(HB_SESSION),
                Some("2026-06-11"),
            ),
        )
        .unwrap();
        // ハートビート側の topic に古いログ範囲を持たせ、コンパクション時の
        // [Past context summary] に載るようにする。
        conn.execute(
            "UPDATE memory_index_nodes SET start_log_id = 1, end_log_id = 20 WHERE id = 't-hb'",
            [],
        )
        .unwrap();
    }

    /// 中心要件: 同じチャンネルの実会話がハートビート文脈へ入り、専用セッションの
    /// 履歴と共存する。並び順は 実会話 → 専用セッション（出力形式指示が末尾）。
    #[test]
    fn channel_conversation_enters_heartbeat_context() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_channel(&conn, 3);
        seed_heartbeat(&conn, 3);

        let out = build_heartbeat_conversation_string(
            &conn,
            HB_SESSION,
            Some(CH_SESSION),
            AGENT,
            100_000,
        )
        .unwrap();

        assert_eq!(out.matches("[Channel conversation]").count(), 1);
        assert!(
            out.contains("channel message 2 from a person"),
            "実会話の直近の発言が文脈に入っていない: {out}"
        );
        assert!(
            out.contains("channel message 0 from a person"),
            "予算内なら実会話は全文載る: {out}"
        );
        // 専用セッションの履歴も残る（片方に置き換わらない）。
        assert!(out.contains("heartbeat note 2"), "{out}");

        let ch_pos = out.find("[Channel conversation]").unwrap();
        let hb_pos = out.find("heartbeat note 0").unwrap();
        assert!(ch_pos < hb_pos, "実会話は専用セッションより前に置く: {out}");
        // #501: 指示文（[ハートビート]…FORMAT_INSTRUCTION）は system プロンプトへ移した
        // ので会話文字列には現れない（SPEAK: パースの前提は system プロンプト側が担保する）。
        assert!(
            !out.contains(FORMAT_INSTRUCTION),
            "指示文が会話に残っている（system プロンプトへ移したはず）: {out}"
        );
        assert!(
            !out.contains("静けさが続くなら黙っていてよい"),
            "指示本文が会話に残っている: {out}"
        );

        // 対比: 従来の呼び出しでは実会話は 1 行も見えない（#404 の症状）。
        let plain = build_conversation_string(&conn, HB_SESSION, AGENT, 100_000).unwrap();
        assert!(!plain.contains("channel message"), "{plain}");
    }

    /// #425: HB 発話は heartbeat 専用セッション（`SPEAK: …`）と実会話セッション（案 A の
    /// 二重記録）の両方に載る。HB 文脈組み立ては両方を読むので、実会話セクションでは
    /// [`HEARTBEAT_CHANNEL_ECHO_METADATA`] 印の付いた自己エコーを落として二重表示を防ぐ。
    /// 印の無い本人の通常返信・他者発言は残す（#404 の意図）。
    #[test]
    fn heartbeat_channel_echo_is_deduplicated_in_heartbeat_context() {
        let conn = opencrab_db::init_memory().unwrap();

        // 実会話セッション: 他者発言 + 本人の通常返信（印なし）。
        insert(
            &conn,
            CH_SESSION,
            "speech",
            "human-1",
            "リリース計画どうする？".to_string(),
        );
        insert(
            &conn,
            CH_SESSION,
            "speech",
            AGENT,
            "通常返信 明日まとめます".to_string(),
        );
        // 本人の HB 発話を案 A で会話セッションへ二重記録（印つき）。
        opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: CH_SESSION.to_string(),
                log_type: "speech".to_string(),
                content: "ECHOUTTERANCE リリースの件、進めます".to_string(),
                speaker_id: Some(AGENT.to_string()),
                turn_number: None,
                metadata_json: Some(
                    opencrab_db::queries::HEARTBEAT_CHANNEL_ECHO_METADATA.to_string(),
                ),
                created_at: None,
            },
        )
        .unwrap();

        // heartbeat 専用セッション: 同じ発話が SPEAK: 形式で載る + 末尾は今回のプロンプト。
        insert(
            &conn,
            HB_SESSION,
            "speech",
            AGENT,
            "SPEAK: ECHOUTTERANCE リリースの件、進めます".to_string(),
        );
        insert(
            &conn,
            HB_SESSION,
            "system",
            "heartbeat",
            format!("[ハートビート] 現在の会話「開発」。\n{FORMAT_INSTRUCTION}"),
        );

        let out = build_heartbeat_conversation_string(
            &conn,
            HB_SESSION,
            Some(CH_SESSION),
            AGENT,
            100_000,
        )
        .unwrap();

        // 実会話セクションには他者発言と本人の通常返信が残る。
        assert!(
            out.contains("リリース計画どうする？"),
            "他者発言は実会話セクションに残る: {out}"
        );
        assert!(
            out.contains("通常返信 明日まとめます"),
            "本人の非 HB 発話は残る（#404 の「自分の発言も含める」意図）: {out}"
        );
        // HB 由来の自己エコーは実会話セクションから落ち、heartbeat 専用セッション側
        // （SPEAK: 形式）にのみ現れる ＝ 会話文字列中に 1 回だけ。
        assert_eq!(
            out.matches("ECHOUTTERANCE").count(),
            1,
            "HB 発話が二重表示されていない（実会話セクションからは印で除外）: {out}"
        );
        assert!(
            out.contains("SPEAK: ECHOUTTERANCE"),
            "残る 1 回は heartbeat 専用セッション側（SPEAK: 形式）: {out}"
        );

        // 対比: 通常返信ターンが読む build_conversation_string（実会話セッション）は印を
        // 無視して素通しするので、本人の HB 投稿がそのまま会話文脈に現れる（#425 の修正点）。
        let plain = build_conversation_string(&conn, CH_SESSION, AGENT, 100_000).unwrap();
        assert!(
            plain.contains("ECHOUTTERANCE"),
            "通常返信ターンでは本人の HB 投稿が会話文脈に現れる（#425 の直したい経路）: {plain}"
        );
    }

    /// 予算が足りない場合でも実会話は落ちない。#501: 指示文は system プロンプトへ移したので
    /// 会話には現れない（コンパクション経路でも）。
    #[test]
    fn compaction_keeps_channel_conversation_and_drops_instruction() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_channel(&conn, 60);
        seed_heartbeat(&conn, 60);

        let out =
            build_heartbeat_conversation_string(&conn, HB_SESSION, Some(CH_SESSION), AGENT, 1_200)
                .unwrap();

        assert_eq!(out.matches("[Channel conversation]").count(), 1);
        assert!(
            out.contains("channel message 59 from a person"),
            "直近の実会話が落ちている: {out}"
        );
        assert!(out.contains("heartbeat note 59"), "{out}");
        assert!(
            !out.contains(FORMAT_INSTRUCTION),
            "コンパクション後の会話に指示文が残っている（system プロンプトへ移したはず）: {out}"
        );
    }

    /// セッションを跨いだ要約はしない: **両方のセッションに topic がある**状態でも
    /// `[Past context summary]` は専用セッション側の 1 つだけ。実会話側の topic は
    /// `[Memory Index]`（現セッション以外の topic を載せる）にだけ出る = short_id
    /// 集合が素、という既存の不変条件がハートビート経路でも保たれる。
    #[test]
    fn cross_session_summaries_do_not_double_up() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_channel(&conn, 3);
        seed_heartbeat(&conn, 40);
        seed_topics_for_both_sessions(&conn);

        let out =
            build_heartbeat_conversation_string(&conn, HB_SESSION, Some(CH_SESSION), AGENT, 1_500)
                .unwrap();

        assert_eq!(out.matches("[Memory Index]").count(), 1, "{out}");
        assert_eq!(
            out.matches("[Past context summary").count(),
            1,
            "要約セクションはハートビート側の 1 つだけ: {out}"
        );
        // short_id は片方にしか出ない。
        assert_eq!(out.matches("[t-hb]").count(), 1, "{out}");
        assert_eq!(out.matches("[t-ch]").count(), 1, "{out}");
        let mi_pos = out.find("[Memory Index]").unwrap();
        let pcs_pos = out.find("[Past context summary").unwrap();
        let t_ch_pos = out.find("[t-ch]").unwrap();
        let t_hb_pos = out.find("[t-hb]").unwrap();
        assert!(
            t_ch_pos > mi_pos && t_ch_pos < pcs_pos,
            "実会話の topic は Memory Index 側にだけ出る: {out}"
        );
        assert!(
            t_hb_pos > pcs_pos,
            "ハートビートの topic は Past context summary 側にだけ出る: {out}"
        );
    }

    /// 予算配分: 実会話を優先する（専用セッションの履歴は反復が多く情報密度が低い）。
    #[test]
    fn channel_conversation_gets_the_larger_share_of_the_budget() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_channel(&conn, 60);
        seed_heartbeat(&conn, 60);

        let out =
            build_heartbeat_conversation_string(&conn, HB_SESSION, Some(CH_SESSION), AGENT, 1_200)
                .unwrap();

        let channel_lines = out.matches("channel message ").count();
        let heartbeat_lines = out.matches("heartbeat note ").count();
        assert!(
            channel_lines > heartbeat_lines,
            "実会話より自分のハートビート履歴が多く載っている (channel={channel_lines}, heartbeat={heartbeat_lines})"
        );
        assert!(
            channel_lines >= 10,
            "実会話がほとんど載っていない (channel={channel_lines})"
        );
    }

    /// channel セッションを渡さない（エージェント単位 tick）なら従来と完全同一。
    #[test]
    fn without_channel_session_output_is_unchanged() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_heartbeat(&conn, 5);

        let plain = build_conversation_string(&conn, HB_SESSION, AGENT, 100_000).unwrap();
        let hb =
            build_heartbeat_conversation_string(&conn, HB_SESSION, None, AGENT, 100_000).unwrap();
        assert_eq!(plain, hb);
    }

    /// 実会話セクションは**人が何を話したか**で埋める。ツール往復・system イベント・
    /// 内心が枠を食わない（#404 の目的は自己参照のループを破ること）。
    #[test]
    fn channel_section_carries_speech_not_tool_traffic() {
        let conn = opencrab_db::init_memory().unwrap();
        // 発言が先・ツール往復が後（＝ツール往復の方が新しい）。素通しだと直近から
        // 詰める `fit_logs_to_budget` が枠をツール結果で埋め、古い側の発言が落ちる。
        seed_channel(&conn, 8);
        seed_channel_tool_traffic(&conn, 40);
        seed_heartbeat(&conn, 5);

        let out =
            build_heartbeat_conversation_string(&conn, HB_SESSION, Some(CH_SESSION), AGENT, 1_600)
                .unwrap();

        assert_eq!(out.matches("[Channel conversation]").count(), 1, "{out}");
        for i in 0..8 {
            assert!(
                out.contains(&format!("channel message {i} from a person")),
                "人の発言 {i} が枠から落ちている: {out}"
            );
            // #284 の保証が拾うのは人の発言だけ。エージェント側の発言まで残ることが
            // 「ツール往復を落として枠が空いた」ことの証拠になる。
            assert!(
                out.contains(&format!("channel reply {i} from the agent")),
                "エージェント側の発言 {i} が枠から落ちている: {out}"
            );
        }
        assert!(
            !out.contains("TOOLPAYLOAD-"),
            "tool_result が載っている: {out}"
        );
        assert!(!out.contains("TOOLCALL-"), "tool_call が載っている: {out}");
        assert!(
            !out.contains("SUBTASKEVENT-"),
            "system イベントが載っている: {out}"
        );
        assert!(
            !out.contains("INNERVOICE-"),
            "自分の内心が載っている（自己参照を増やす）: {out}"
        );
    }

    /// 窓は**発言で数える**。ツール往復に押し出されて発言が窓から落ちない
    /// （＝生の直近 N 件を取ってから絞るのではなく、SQL 側で絞る / #405 レビュー 2 巡目）。
    ///
    /// 生ログ 600 行のうち古い側 200 行が発言、新しい側 400 行がツール往復。
    /// 「生の直近 500 行を取ってから絞る」形だと、最も古い 100 行の発言が窓の外に落ちる。
    #[test]
    fn channel_window_counts_speech_not_raw_logs() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_channel(&conn, 100); // speech 200 行（id 1..200）
        seed_channel_tool_traffic(&conn, 100); // 非 speech 400 行（id 201..600）
        seed_heartbeat(&conn, 5);

        // 予算は発言 200 行が収まる大きさにして、窓だけが効くようにする。
        let out =
            build_heartbeat_conversation_string(&conn, HB_SESSION, Some(CH_SESSION), AGENT, 40_000)
                .unwrap();

        assert!(
            out.contains("channel message 0 from a person"),
            "生の窓（500 行）の外にある最古の発言が落ちている: {out}"
        );
        assert!(
            out.contains("channel reply 0 from the agent"),
            "生の窓の外にあるエージェントの発言が落ちている: {out}"
        );
        assert!(out.contains("channel message 99 from a person"), "{out}");
        assert!(!out.contains("TOOLPAYLOAD-"), "{out}");
    }

    /// 発言が 1 件も無く、ツール往復だけのチャンネルではセクションごと出さない。
    #[test]
    fn channel_session_with_only_tool_traffic_adds_no_section() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_channel_tool_traffic(&conn, 5);
        seed_heartbeat(&conn, 5);

        let plain = build_conversation_string(&conn, HB_SESSION, AGENT, 100_000).unwrap();
        let hb = build_heartbeat_conversation_string(
            &conn,
            HB_SESSION,
            Some(CH_SESSION),
            AGENT,
            100_000,
        )
        .unwrap();
        assert!(!hb.contains("[Channel conversation]"), "{hb}");
        assert_eq!(plain, hb);
    }

    /// 絞り込みは**実会話セクションだけ**。ハートビート専用セッション側は従来どおり
    /// 全種別を載せる — subtask の完了本文が次 tick で文脈へ載ることに
    /// `heartbeat_run_request` の `NoopCompletionSink` が依存している。
    #[test]
    fn heartbeat_session_still_carries_non_speech_logs() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_channel(&conn, 3);
        seed_heartbeat(&conn, 3);
        insert(
            &conn,
            HB_SESSION,
            "system",
            "system",
            r#"{"exit_reason":"completed","result":"SUBTASKBODY-1 調査の結論はこう"}"#.to_string(),
        );
        insert(
            &conn,
            HB_SESSION,
            "tool_result",
            AGENT,
            "HBTOOLRESULT-1 検索結果".to_string(),
        );

        let out = build_heartbeat_conversation_string(
            &conn,
            HB_SESSION,
            Some(CH_SESSION),
            AGENT,
            100_000,
        )
        .unwrap();

        assert!(
            out.contains("SUBTASKBODY-1"),
            "subtask 完了本文がハートビート文脈から消えた: {out}"
        );
        assert!(out.contains("HBTOOLRESULT-1"), "{out}");
    }

    /// 実会話が 1 行も無いチャンネルでは、セクションごと出さない（空見出しを足さない）。
    #[test]
    fn channel_session_without_logs_adds_no_section() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_heartbeat(&conn, 5);

        let plain = build_conversation_string(&conn, HB_SESSION, AGENT, 100_000).unwrap();
        let hb = build_heartbeat_conversation_string(
            &conn,
            HB_SESSION,
            Some("discord-a1-111-999"),
            AGENT,
            100_000,
        )
        .unwrap();
        assert!(!hb.contains("[Channel conversation]"));
        assert_eq!(plain, hb);
    }
}

/// `[Past context summary]` の予算上限（#406）。
///
/// 事故当時、このセクションには上限が無く、topic 2,446 件・要約 248,340 文字が全件
/// 連結され、1 ハートビートの入力が 284,486 トークンになっていた。ここで固定するのは
/// **上限が効くこと**と**切り詰めの向き（新しい方が残る）**の 2 点。
#[cfg(test)]
mod past_summary_budget_tests {
    use super::{
        build_conversation_string, build_heartbeat_conversation_string, estimate_tokens,
        HEARTBEAT_CHANNEL_BUDGET_DEN, HEARTBEAT_CHANNEL_BUDGET_NUM, PAST_SUMMARY_BUDGET_DEN,
        PAST_SUMMARY_BUDGET_NUM,
    };

    const AGENT: &str = "a1";
    const SESSION: &str = "s1";
    const CH_SESSION: &str = "discord-a1-111-222";

    fn insert_log(conn: &rusqlite::Connection, session_id: &str, speaker: &str, content: String) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: session_id.to_string(),
                log_type: "speech".to_string(),
                content,
                speaker_id: Some(speaker.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    /// 現セッションの topic を `n` 件置く。`start_log_id` は昇順（＝供給元の
    /// `ORDER BY start_log_id ASC` で **TOPIC-000 が最古**になる）。
    fn seed_topics(conn: &rusqlite::Connection, n: usize) {
        for i in 0..n {
            opencrab_db::queries::insert_index_node(
                conn,
                &opencrab_db::queries::IndexNodeRow {
                    id: format!("t{i:03}"),
                    agent_id: AGENT.to_string(),
                    parent_id: None,
                    node_type: "topic".to_string(),
                    source_type: "session_log".to_string(),
                    title: format!("TOPIC-{i:03}"),
                    summary: format!(
                        "summary body for topic {i:03} {}",
                        "padding words to make the line非自明な長さ ".repeat(3)
                    ),
                    start_log_id: Some(i as i64 + 1),
                    end_log_id: None,
                    source_session_id: Some(SESSION.to_string()),
                    date_from: None,
                    date_to: None,
                    depth: 0,
                    child_count: 0,
                    token_count: 0,
                    created_at: "2026-07-01T00:00:00Z".to_string(),
                    updated_at: "2026-07-01T00:00:00Z".to_string(),
                    short_id: Some(format!("t{i:03}")),
                    keywords_json: "[]".to_string(),
                    summary_refreshed_at: None,
                },
            )
            .unwrap();
        }
    }

    /// 会話本文だけで予算を超えるだけのログを積む（＝コンパクション経路へ入れる）。
    fn seed_logs(conn: &rusqlite::Connection, n: usize) {
        for i in 0..n {
            insert_log(
                conn,
                SESSION,
                AGENT,
                format!("log line {i} about the release plan and the follow-up work"),
            );
        }
    }

    /// 出力から 1 セクションぶん（見出しから次の見出しの直前まで）を切り出す。
    fn section<'a>(out: &'a str, marker: &str) -> &'a str {
        let start = out
            .find(marker)
            .unwrap_or_else(|| panic!("{marker} が出力に無い: {out}"));
        let rest = &out[start..];
        let end = [
            "[Channel conversation]",
            "[Past context summary",
            "[Recent conversation]",
        ]
        .iter()
        .filter_map(|m| rest[1..].find(m).map(|i| i + 1))
        .min()
        .unwrap_or(rest.len());
        // セクション間の区切り（`parts.join("\n\n")`）は次のセクション側の予算で
        // 数えられているので、ここでは落とす。
        rest[..end].trim_end()
    }

    /// 出力から `[Past context summary]` セクション（ヘッダ込み）だけを切り出す。
    fn summary_section(out: &str) -> &str {
        section(out, "[Past context summary")
    }

    /// topic が数千件あっても、セクションは予算の 30% を超えない。
    #[test]
    fn past_summary_stays_within_thirty_percent_of_the_budget() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_logs(&conn, 400);
        seed_topics(&conn, 2_000);

        const BUDGET: usize = 4_000;
        let out = build_conversation_string(&conn, SESSION, AGENT, BUDGET).unwrap();
        let cap = BUDGET / PAST_SUMMARY_BUDGET_DEN * PAST_SUMMARY_BUDGET_NUM;
        let used = estimate_tokens(summary_section(out.as_str()));
        assert!(
            used <= cap,
            "[Past context summary] が予算の 30% ({cap}) を超えた: {used} トークン"
        );
        // 上限が無かった頃はここが数万トークンだった。空回りしていないこと（＝実際に
        // 切り詰めが起きて、全件連結にはなっていないこと）も同時に見る。
        assert!(
            !out.contains("TOPIC-000"),
            "2,000 件が全件載っている（切り詰めが起きていない）"
        );
    }

    /// **切り詰めの向き**: 新しい topic が残り、古い topic から落ちる。
    ///
    /// 供給元のクエリは古い順なので、素直に前から詰めると逆になる。詰める向きを
    /// 反転させたらこのテストが落ちること。表示順が時系列（古い→新しい）へ戻って
    /// いることも同時に見る。
    #[test]
    fn past_summary_keeps_the_newest_topics_and_drops_the_oldest() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_logs(&conn, 400);
        seed_topics(&conn, 100);

        let out = build_conversation_string(&conn, SESSION, AGENT, 4_000).unwrap();
        let section = summary_section(out.as_str());
        assert!(
            section.contains("TOPIC-099"),
            "最新の topic が落ちている: {section}"
        );
        assert!(
            !section.contains("TOPIC-000"),
            "最古の topic が残っている（切り詰めの向きが逆）: {section}"
        );
        let newest = section.find("TOPIC-099").unwrap();
        let one_before = section
            .find("TOPIC-098")
            .expect("直前の topic まで落ちている（残す件数が想定より少ない）");
        assert!(
            one_before < newest,
            "表示順が時系列に戻っていない（新しい方が先に出ている）: {section}"
        );
    }

    /// 落としたら黙らない: 件数と引き出し方を本人へ伝える。
    #[test]
    fn past_summary_reports_how_many_were_omitted() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_logs(&conn, 400);
        seed_topics(&conn, 100);

        let out = build_conversation_string(&conn, SESSION, AGENT, 4_000).unwrap();
        let section = summary_section(out.as_str());
        assert!(
            section.contains("older topic summaries were omitted"),
            "落としたことが伝わっていない: {section}"
        );
        assert!(
            section.contains("retrieve_memory_nodes"),
            "引き出し方が書かれていない: {section}"
        );
        // 残った件数と落とした件数の合計は 100。告知の件数が実態と食い違わないこと。
        let kept = (0..100)
            .filter(|i| section.contains(&format!("TOPIC-{i:03}")))
            .count();
        assert!(
            section.contains(&format!("{} older topic summaries", 100 - kept)),
            "告知の件数が残存件数と合っていない（残 {kept} 件）: {section}"
        );
    }

    /// #408: 全 topic が予算内に収まる（早期 return）経路を固定する。既存テストは
    /// いずれも切り詰めが起きる seed（topic 100 件 / 予算 4,000 など）なので、
    /// `build_past_context_summary_section` の `if dropped == 0` を潰す変異（→ `if false`）
    /// を入れても素通りしていた。
    ///
    /// コンパクション（＝ `[Past context summary]` の構築）は**全文が予算を超えたときだけ**
    /// 走る（`build_conversation_inner` の全文フィット早期 return）。そこで**ログは大量**に
    /// 置いて予算超過でコンパクションを起こしつつ、**topic は少数**（3 件）に絞って 30% 枠
    /// （4,000 × 3/10 = 1,200 トークン）に全件が収まる状況を作る。ここで
    /// (1) 切り詰めの告知が出ない (2) 全 topic が出力に含まれる ことを固定する。
    ///
    /// 変異を入れると `dropped == 0` のまま告知構築へ落ち、`past_summary_omitted_notice(0)`
    /// （"0 older topic summaries were omitted ..."）が混入してここで落ちる。
    /// `topics.is_empty()` の fallback 経路（`build_truncated_conversation`）とは別物で、
    /// こちらは「topic はあるが全部入る」ケース。
    #[test]
    fn past_summary_emits_no_notice_when_all_topics_fit() {
        let conn = opencrab_db::init_memory().unwrap();
        // 全文が 4,000 を超えるだけのログを置く（＝コンパクションを起こす）。
        seed_logs(&conn, 400);
        // topic は 3 件だけ。30% 枠（1,200 トークン）に余裕で全件収まる。
        seed_topics(&conn, 3);

        let out = build_conversation_string(&conn, SESSION, AGENT, 4_000).unwrap();
        let section = summary_section(out.as_str());

        // 全 topic が [Past context summary] に出る。
        for i in 0..3 {
            assert!(
                section.contains(&format!("TOPIC-{i:03}")),
                "全 topic が出力に含まれるべき (TOPIC-{i:03}): {section}"
            );
        }
        // 全件が収まるので切り詰めの告知は一切出ない（早期 return 経路）。
        assert!(
            !section.contains("were omitted"),
            "全件が収まるのに切り詰めの告知が出ている（早期 return が壊れている）: {section}"
        );
    }

    /// 要約が予算を食い潰さないので、直近会話の枠が残る（事故当時はここが 0 だった）。
    ///
    /// #500 の位置づけ: これは**コンパクションが機能していることの回復ガード**であって、
    /// heartbeat 障害の再発防止ではない。障害時の会話は予算（525,000）に収まっていたのに、
    /// その予算自体がバックエンドの実上限（371,678）を超えていた。**「予算 ≤ バックエンド
    /// 実上限」の天井はまだコードに無く #535 の管轄**。この `<= BUDGET` assert が意味を持つのは
    /// budget が正しく設定されている前提でのみ。
    #[test]
    fn recent_conversation_keeps_its_share_when_topics_are_huge() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_logs(&conn, 400);
        seed_topics(&conn, 2_000);

        const BUDGET: usize = 4_000;
        let out = build_conversation_string(&conn, SESSION, AGENT, BUDGET).unwrap();
        let recent = &out[out
            .find("[Recent conversation]")
            .unwrap_or_else(|| panic!("直近会話セクションが無い: {out}"))..];
        let used = estimate_tokens(recent);
        assert!(
            used * 2 >= BUDGET,
            "直近会話が予算の 50% ({}) に届いていない: {used} トークン",
            BUDGET / 2
        );
        // 合計が予算を超えないこと（本 issue の本体）。
        assert!(
            estimate_tokens(&out) <= BUDGET,
            "プロンプト全体が予算 {BUDGET} を超えた: {} トークン",
            estimate_tokens(&out)
        );
    }

    /// ハートビート経路でも合計が予算を超えず、実会話と末尾の出力形式指示が残る。
    ///
    /// #500 の位置づけ: **回復ガード**（コンパクションが効くこと）であって障害の再発防止では
    /// ない。死んだのは heartbeat 経路だが、原因は budget 自体がバックエンド実上限を超えた
    /// ことで、このテストでは捕まらない（budget が正しい前提でのみ有効。天井は #535 の管轄）。
    #[test]
    fn heartbeat_total_stays_within_budget_and_keeps_channel_and_format_instruction() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_topics(&conn, 2_000);
        for i in 0..40 {
            insert_log(
                &conn,
                CH_SESSION,
                "human-1",
                format!("CHANNELMSG-{i} 人がチャンネルで実際に話したこと"),
            );
        }
        seed_logs(&conn, 400);
        // 専用セッションの最後のログ = 今回のハートビートプロンプト（出力形式の規約）。
        insert_log(
            &conn,
            SESSION,
            "heartbeat",
            "[ハートビート] 出力形式: SPEAK/LEARN/IDLE のいずれか。".to_string(),
        );

        const BUDGET: usize = 4_000;
        let out =
            build_heartbeat_conversation_string(&conn, SESSION, Some(CH_SESSION), AGENT, BUDGET)
                .unwrap();
        assert!(
            estimate_tokens(&out) <= BUDGET,
            "ハートビート文脈が予算 {BUDGET} を超えた: {} トークン",
            estimate_tokens(&out)
        );
        assert!(
            out.contains("[Channel conversation]") && out.contains("CHANNELMSG-39"),
            "実会話が要約に押し出されている: {out}"
        );
        assert!(
            out.trim_end()
                .ends_with("出力形式: SPEAK/LEARN/IDLE のいずれか。"),
            "末尾の出力形式指示が落ちている（SPEAK: のパースが壊れる）: {out}"
        );
    }

    /// **上限の基準は「渡された予算」であって全体予算ではない。**
    ///
    /// ハートビート経路では `[Channel conversation]` が先に `inner_budget × 3/4` を取り、
    /// **その残り**が `build_conversation_inner` へ渡る。ここで全体予算の 30% を基準に
    /// すると、渡された予算を要約が丸ごと食い潰して #406 の症状（会話本文が
    /// `RECENT_MIN_LOGS` 件しか残らない）が再発する。
    ///
    /// 合計だけを見るテストではこの違いを見分けられない（実会話が小さければ
    /// ハートビート側の予算に余裕が残り、全体予算基準でも合計は収まる）。**実会話に
    /// 3/4 枠を使い切らせたうえで、要約が `渡された予算 × 3/10` 以下**であることを直接見る。
    #[test]
    fn past_summary_cap_is_based_on_the_budget_passed_in_not_the_total() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_topics(&conn, 2_000);
        // 実会話で 3/4 の枠を使い切らせる（これがこのテストの前提）。
        for i in 0..400 {
            insert_log(
                &conn,
                CH_SESSION,
                "human-1",
                format!("CHANNELMSG-{i} 人がチャンネルで実際に話したことの本文"),
            );
        }
        seed_logs(&conn, 400);

        const BUDGET: usize = 4_000;
        let out =
            build_heartbeat_conversation_string(&conn, SESSION, Some(CH_SESSION), AGENT, BUDGET)
                .unwrap();
        // 前置セクションが出ていないこと = inner_budget == BUDGET。以下の計算の前提。
        assert!(
            out.starts_with("[Channel conversation]"),
            "前置セクションが出ており heartbeat_budget を計算できない: {out}"
        );
        let channel = estimate_tokens(section(&out, "[Channel conversation]"));
        let summary = estimate_tokens(summary_section(&out));

        // 前提の確認: 実会話が 3/4 枠をほぼ使い切っていること。使い切っていないと
        // 「渡された予算」と「全体予算」の差が小さく、基準の違いを見分けられない。
        let channel_cap = BUDGET / HEARTBEAT_CHANNEL_BUDGET_DEN * HEARTBEAT_CHANNEL_BUDGET_NUM;
        assert!(
            channel * 10 >= channel_cap * 9,
            "実会話が 3/4 枠（{channel_cap}）を使い切っていない: {channel} トークン"
        );

        let passed_in = BUDGET - channel;
        let cap = passed_in / PAST_SUMMARY_BUDGET_DEN * PAST_SUMMARY_BUDGET_NUM;
        // 全体予算を基準にした場合の上限。これと明確に差がある状態で測る。
        let whole_budget_cap = BUDGET / PAST_SUMMARY_BUDGET_DEN * PAST_SUMMARY_BUDGET_NUM;
        assert!(
            cap * 2 < whole_budget_cap,
            "2 つの基準の差が小さすぎてテストが見分けられない: {cap} vs {whole_budget_cap}"
        );
        assert!(
            summary <= cap,
            "[Past context summary] が**渡された予算**の 30%（{cap}）を超えた: \
             {summary} トークン（全体予算基準なら {whole_budget_cap}）"
        );
    }

    /// 予算が極小でも panic しない（0 除算・スライス外・オーバーフロー）。
    ///
    /// **panic しないことだけを見る。** 極小予算では直近下限（`RECENT_MIN_LOGS`）が予算を
    /// 割って出力が予算を超えうるが、それはここでは固定しない（超過そのものは #536）。
    #[test]
    fn tiny_budget_does_not_panic() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_logs(&conn, 20);
        seed_topics(&conn, 50);

        for budget in 0..=3 {
            let out = build_conversation_string(&conn, SESSION, AGENT, budget).unwrap();
            assert!(!out.is_empty(), "budget={budget} で空文字になった");
            // 予算に告知すら入らないのでセクションごと落ちる。直近ログの下限
            // （RECENT_MIN_LOGS）は引き続き効く。
            assert!(
                out.contains("log line 19"),
                "budget={budget} で直近ログの下限が効いていない: {out}"
            );
        }
    }
}

/// #500: 組み上がった会話が予算で頭打ちになる（＝コンパクションが機能している）ことの
/// **回復ガード**。
///
/// **これは heartbeat 障害の再発防止ではない。** あの日は会話が予算（525,000）に収まって
/// いたのに、その予算自体がバックエンドの実上限（371,678）を超えていた。破れたのは
/// 「予算 ⇔ バックエンド実上限」の間で、そこには今も天井が無く **#535 の管轄**（本番は
/// `context_window` を手で下げているだけ）。ここが守るのは「budget が正しく設定されている
/// 前提で、履歴がいくら伸びてもコンパクションが出力を予算付近まで頭打ちにすること」だけ。
///
/// topic 圧縮経路（[`past_summary_budget_tests::recent_conversation_keeps_its_share_when_topics_are_huge`]）
/// とハートビート経路（[`past_summary_budget_tests::heartbeat_total_stays_within_budget_and_keeps_channel_and_format_instruction`]）
/// の「出力 ≤ 予算」は #406 で既に固定済み。ここは**未カバーだった topic 無しの切り詰め
/// フォールバック**（[`super::build_truncated_conversation`]）を埋める。
///
/// なお `fit_logs_to_budget` は末尾から予算いっぱいまで詰めるので出力はほぼ予算ちょうどに
/// なり、**予算に計上されない省略マーカー / セクション区切りのぶん、通常サイズの行でも
/// 出力は予算を数十トークンだけ超えうる**（実測で +12 程度。#536 の巨大行による超過とは
/// 別の、境界の小さなオーバーヘッド）。回復ガードが見たいのは「履歴全体＝予算の数倍まで
/// 膨らまない」ことなので、予算＋小さな既知の余白で判定する。
#[cfg(test)]
mod budget_fit_recovery_guard_tests {
    use super::{build_conversation_string, estimate_tokens};

    const AGENT: &str = "a1";
    const SESSION: &str = "s1";

    fn insert_speech(conn: &rusqlite::Connection, content: &str) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: SESSION.to_string(),
                log_type: "speech".to_string(),
                content: content.to_string(),
                speaker_id: Some(AGENT.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    /// topic 要約が 1 件も無い（＝`build_truncated_conversation` フォールバック）状態で
    /// 履歴が予算を大きく超えても、組み上がった会話は予算付近で頭打ちになる。
    ///
    /// 既存の fits テストは全て topic ありの summary 経路。topic 生成が追いつく前の
    /// セッションや要約が引けない経路はこの切り詰めフォールバックへ落ちるため、そこも
    /// 頭打ちになることを固定する。行は通常サイズ（下限が巨大行で予算を割る #536 とは別条件）。
    #[test]
    fn truncated_fallback_without_topics_stays_within_budget() {
        let conn = opencrab_db::init_memory().unwrap();
        // 予算を大きく超える履歴を積む（topic は入れない → 切り詰め経路）。
        for i in 0..400 {
            insert_speech(
                &conn,
                &format!("log line {i} about the release plan and the follow-up work"),
            );
        }

        const BUDGET: usize = 4_000;
        // 予算に計上されない省略マーカー / 区切りのぶんの既知の小さな余白。
        const MARKER_SLACK: usize = 128;

        let out = build_conversation_string(&conn, SESSION, AGENT, BUDGET).unwrap();
        // 前提: topic 無しの切り詰めフォールバックに入っている。
        assert!(
            !out.contains("[Past context summary"),
            "topic 要約が出ている＝切り詰め経路のテストになっていない: {out}"
        );
        // コンパクションが実際に起きた（＝古いメッセージが落ちた）ことの印。これが無ければ
        // 全文がそのまま出ており、予算頭打ちを検証できていない。
        assert!(
            out.contains("[Note: Earlier messages were omitted"),
            "切り詰めの注記が無い＝コンパクションが起きていない: {out}"
        );
        let toks = estimate_tokens(&out);
        assert!(
            toks <= BUDGET + MARKER_SLACK,
            "切り詰め経路で出力が予算+余白（{}）を超えた: {toks} トークン。\
             履歴（予算の数倍）が頭打ちにできていない可能性がある",
            BUDGET + MARKER_SLACK
        );
    }
}

/// #504: 文脈から切り離された「飛び地」のユーザー発言は会話へ載せない。
///
/// #284 は「直近ユーザー発言が 1 件も載らない」事故を、末尾の連続区間から外れた
/// ユーザー発言を省略マーカーで挟んだ**飛び地**として個別に載せることで防いでいた。
/// だが飛び地は文脈も応答有無も失われた裸の発言で、オーナー判断は「無いより悪い」。
///
/// そこで A′: **一番新しいユーザー発言 1 件だけは飛び地でも必ず載せ**（＝「今の指示」で、
/// #284 が本当に守りたかったもの）、それより古い飛び地は落とす。落とした分は件数と
/// 期間を書いた省略マーカーに集約する（[`super::format_omission_marker`]）。
///
/// ここは [`super::fit_logs_to_budget`] を直接叩き、行の index と `created_at` を固定して
/// 判定する（DB や予算経路の間接を挟むと、どの発言が飛び地になるかが読みにくい）。
#[cfg(test)]
mod orphan_user_speech_tests {
    use super::fit_logs_to_budget;
    use opencrab_db::queries::SessionLogRow;

    const AGENT: &str = "a1";
    const USER: &str = "kojira";
    const SESSION: &str = "s1";

    fn user_speech(content: &str, created_at: &str) -> SessionLogRow {
        SessionLogRow {
            id: None,
            agent_id: AGENT.to_string(), // 受信側エージェント（#377）
            session_id: SESSION.to_string(),
            log_type: "speech".to_string(),
            content: content.to_string(),
            speaker_id: Some(USER.to_string()), // 送信者 ≠ AGENT → is_user_speech 真
            turn_number: None,
            metadata_json: None,
            created_at: Some(created_at.to_string()),
        }
    }

    fn tool_result(content: &str, created_at: &str) -> SessionLogRow {
        SessionLogRow {
            id: None,
            agent_id: AGENT.to_string(),
            session_id: SESSION.to_string(),
            log_type: "tool_result".to_string(),
            content: content.to_string(),
            speaker_id: Some(AGENT.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: Some(created_at.to_string()),
        }
    }

    /// 連続区間の外にユーザー発言 3 件（8/1・8/6・8/6）を置き、末尾を巨大なツール結果で
    /// 埋め尽くして 3 件すべてを飛び地にする。index の大きい "NEWEST" が一番新しい発言。
    fn orphaned_speeches_then_tool_flood() -> Vec<SessionLogRow> {
        let mut logs = vec![
            user_speech("OLD-A-最古の飛び地", "2026-08-01T00:00:00Z"),
            user_speech("OLD-B-古い飛び地", "2026-08-06T00:00:00Z"),
            user_speech("NEWEST-一番新しい指示", "2026-08-06T12:00:00Z"),
        ];
        // 末尾の連続区間を埋め、予算を使い切らせる（＝上の 3 件を連続区間の外に押し出す）。
        for i in 0..20 {
            logs.push(tool_result(
                &format!("tool output {i}: {}", "data ".repeat(200)),
                "2026-08-06T12:00:00Z",
            ));
        }
        logs
    }

    /// 一番新しいユーザー発言だけは飛び地でも残り、それより古い飛び地は消える。
    #[test]
    fn only_the_newest_orphan_user_speech_is_kept() {
        let logs = orphaned_speeches_then_tool_flood();
        let out = fit_logs_to_budget(&logs, AGENT, 300);
        assert!(
            out.contains("NEWEST-一番新しい指示"),
            "一番新しいユーザー発言が飛び地でも残っていない: {out}"
        );
        assert!(
            !out.contains("OLD-A-最古の飛び地"),
            "古い飛び地が消えていない（OLD-A）: {out}"
        );
        assert!(
            !out.contains("OLD-B-古い飛び地"),
            "古い飛び地が消えていない（OLD-B）: {out}"
        );
    }

    /// 落とした古い区間は、件数と期間を添えた省略マーカーに集約される。
    #[test]
    fn omission_marker_carries_count_and_period() {
        let logs = orphaned_speeches_then_tool_flood();
        let out = fit_logs_to_budget(&logs, AGENT, 300);
        // 一番新しい発言(index 2)より前の飛び地 = index 0..2（OLD-A 8/1・OLD-B 8/6）。
        // 件数 2・期間 5 日がマーカーに出る。
        assert!(
            out.contains("2 older messages over 5 days"),
            "省略マーカーに件数・期間が入っていない: {out}"
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

/// heartbeat 指示文は会話へ積まない（#501）。
///
/// 以前は `scheduler.rs::run_one_fire` が発火のたびに同一文面の指示文（`system` /
/// `speaker_id='heartbeat'`）をセッションログへ挿入し、それが全件そのまま会話へ復元されて
/// いた。本番の heartbeat セッションでは同一指示が 192 件並び、「同じ指示 → IDLE」の対を
/// 何十回も見せて挙動を歪めていた。#501 で指示文は system プロンプトへ移した
/// （`heartbeat_turn::build_context`）ので、会話再構成では指示文 scaffolding を**全件落とす**。
/// subtask 完了本文（`system` / `speaker_id=None`, #404 / #405）は落とさない。
#[cfg(test)]
mod heartbeat_prompt_dedup_tests {
    use super::{build_conversation_string, retain_conversation_logs};

    const AGENT: &str = "a1";
    const HB_SESSION: &str = "heartbeat-a1-222";

    /// 毎 tick 挿入されていた指示文（本番と同形）。#501 以降は書かれないが、既存 DB には残る。
    const HB_PROMPT: &str = "[ハートビート] 現在の会話「（自律ハートビート）」。20分ごとに巡回して新着に反応する。\n出力形式: SPEAK/LEARN/IDLE のいずれか。";

    fn insert(conn: &rusqlite::Connection, log_type: &str, speaker: Option<&str>, content: &str) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: HB_SESSION.to_string(),
                log_type: log_type.to_string(),
                content: content.to_string(),
                speaker_id: speaker.map(|s| s.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    /// 既存 DB に積まれた指示文（複数件）が会話組み立ての出力に**一切現れない**こと。
    /// 落とす filter を戻すと 3 回現れて赤くなる（恒真ではない）。
    #[test]
    fn heartbeat_prompts_never_appear_in_the_conversation() {
        let conn = opencrab_db::init_memory().unwrap();
        // 3 tick 分の（過去の）指示文と、その間の発話・subtask 完了本文を積む。
        insert(&conn, "system", Some("heartbeat"), HB_PROMPT);
        insert(&conn, "speech", Some("kojira"), "新着あった？");
        insert(&conn, "system", Some("heartbeat"), HB_PROMPT);
        insert(&conn, "speech", Some(AGENT), "SPEAK: ありました");
        // subtask 完了本文（#404 / #405）: speaker_id=None なので落としてはならない。
        insert(
            &conn,
            "system",
            None,
            r#"{"type":"subtask_completed","subtask_id":"st-1","result":"調査おわり"}"#,
        );
        insert(&conn, "system", Some("heartbeat"), HB_PROMPT);

        let out = build_conversation_string(&conn, HB_SESSION, AGENT, 100_000).unwrap();

        assert_eq!(
            out.matches("20分ごとに巡回して新着に反応する").count(),
            0,
            "heartbeat 指示文が会話へ復元されている（system プロンプトへ移したはず）: {out}"
        );
        // #404 / #405: subtask 完了本文は残る。
        assert!(
            out.contains("調査おわり"),
            "subtask 完了本文が落ちた: {out}"
        );
        // 発話は両方残る（除外が効きすぎていないこと）。
        assert!(
            out.contains("新着あった？") && out.contains("ありました"),
            "発話が落ちた: {out}"
        );
    }

    /// `retain_conversation_logs` は指示文 scaffolding を全件落とし、subtask 完了本文
    /// （speaker=None）と発話は残す。
    #[test]
    fn retain_drops_all_scaffolds_keeps_completion_and_speech() {
        let mk = |id: i64, log_type: &str, speaker: Option<&str>, content: &str| {
            opencrab_db::queries::SessionLogRow {
                id: Some(id),
                agent_id: AGENT.to_string(),
                session_id: HB_SESSION.to_string(),
                log_type: log_type.to_string(),
                content: content.to_string(),
                speaker_id: speaker.map(|s| s.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            }
        };
        let logs = vec![
            mk(1, "system", Some("heartbeat"), "指示v1"),
            mk(2, "speech", Some(AGENT), "SPEAK: やあ"),
            mk(3, "system", Some("heartbeat"), "指示v2"),
            mk(
                4,
                "system",
                None,
                r#"{"type":"subtask_completed","result":"r"}"#,
            ),
        ];
        let kept = retain_conversation_logs(logs);
        // 指示文 scaffolding は 1 件も残らない。
        assert!(
            !kept.iter().any(
                |l| l.speaker_id.as_deref() == Some(opencrab_db::queries::HEARTBEAT_SPEAKER_ID)
            ),
            "指示文 scaffolding が残った"
        );
        // subtask 完了本文（speaker=None）と発話は残る。
        assert!(
            kept.iter().any(|l| l.speaker_id.is_none()),
            "subtask 完了本文が落ちた"
        );
        assert!(
            kept.iter().any(|l| l.content == "SPEAK: やあ"),
            "発話が落ちた"
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
