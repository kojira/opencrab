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

/// 会話履歴が空のときに `build_conversation_inner` が返すマーカー（#691 の判定にも使う）。
const NO_MESSAGES_MARKER: &str = "No messages yet.";

/// 応答直前に会話履歴の末尾へ置く出力指示（#691）。
///
/// opus-5 は 1:1 の長い会話で「次のユーザー発言」を続きとして**捏造**する傾向がある
/// （モデル固有の挙動・オーナー観測）。会話履歴は `[ID] [時刻]:` 形式で 1 行 1 発話に
/// 連結されるため、生成モデルが最も自然な予測として「次の話者行」を書き足してしまう。
/// 生成点に最も近い指示が最も効く（オーナー裁定・opencrab2 実測）ので、履歴の**直後**
/// （＝生成点の直前）にこの 1 行だけを置く。ロール分離・履歴形式の変更・出力の
/// フィルタはしない（オーナー裁定で対策から除外）。履歴が空（`NO_MESSAGES_MARKER`）の
/// ときは真似る対象が無いので付けない。
pub const RESPONSE_ONLY_DIRECTIVE: &str = "ここから先はあなた自身の本文のみを書く。`[ID] [時刻]:` 形式の行（他の話者の発言の再現・引用・続き）を出力してはならない。";

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
    // #536: 最後の `parts.join("\n\n")` の区切りも出力へ含まれるので計上する。
    // prefix N 個 + inner の N+1 パートで区切りは N 本（prefix が空なら 0 本）。
    inner_budget = inner_budget.saturating_sub(prefix_sections.len() * estimate_tokens("\n\n"));
    // #691: 履歴の直後に置く出力指示のぶんを会話予算から先に引く。prepend 前の返り値が
    // `context_budget_tokens` を超えないという契約（下の budget テスト群）を保つため、
    // #536 の区切り計上と同じ流儀で組み込み前に確保する。履歴が空で指示を付けない場合は
    // 数十トークン過剰に確保するだけで実害はない（空履歴は "No messages yet." のみ）。
    let directive_cost = estimate_tokens(RESPONSE_ONLY_DIRECTIVE) + estimate_tokens("\n\n");
    inner_budget = inner_budget.saturating_sub(directive_cost);
    let inner = build_conversation_inner(conn, session_id, agent_id, inner_budget)?;

    // #691: 履歴が空（真似る対象が無い）ときは出力指示を付けない。
    let history_is_empty = inner == NO_MESSAGES_MARKER;

    let mut parts = prefix_sections;
    parts.push(inner);
    let mut out = parts.join("\n\n");
    if !history_is_empty {
        // 応答直前の出力指示を履歴の**直後**（＝生成点の直前）へ 1 行だけ置く（#691）。
        out.push_str("\n\n");
        out.push_str(RESPONSE_ONLY_DIRECTIVE);
    }
    Ok(out)
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
        // フォールバック: 要約がない → ヘッダ 1 行 + 予算駆動の直近ウィンドウ（#609 / #610）。
        // 圧縮パスと同じ `build_recent_window` を使う（全ログを `fit_logs_to_budget` へ渡す）。
        let header = "[Note: Earlier messages were omitted due to context length. Showing most recent messages.]\n\n";
        let recent_text = build_recent_window(
            conn,
            session_id,
            agent_id,
            context_budget_tokens.saturating_sub(estimate_tokens(header)),
        );
        return Ok(format!("{header}{recent_text}"));
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

    // 直近ウィンドウは**予算だけ**で組む（#609 / #610 レビュー①②）。要約は 30% で頭打ちなので、
    // 直近会話には常に 50% 以上（ヘッダぶんを除いて ~70%）が残る（#406）。セッションの全ログを
    // `build_recent_window` へ渡し、`fit_logs_to_budget` が末尾から予算いっぱいまで詰める。
    // 落ちた分は `fit_logs_to_budget` の省略マーカーが告知する（`fit` に全件が渡るので、渡る前に
    // 対象外になって黙って消える行が存在しない）。索引済み領域と内容が重複してもよい。
    let recent_text = build_recent_window(
        conn,
        session_id,
        agent_id,
        context_budget_tokens.saturating_sub(overhead_tokens),
    );

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
/// 渡された予算（[`build_conversation_inner`] が受け取る `context_budget_tokens`）に対する割合に
/// する。ここで全体予算の 30% を基準にすると、渡された予算を要約が丸ごと食い潰して
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
/// 全件を tiktoken に通すのは丸ごと無駄になる。直近ログに窓を入れたのと同じ理由（#405 / #406 レビュー）。
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

/// ツール結果から、会話へ残す**参照**を組む（#709）。本文は載せない。
///
/// #707 では「読み直せるか」で分け、`ws_read` / `ws_list` だけを参照化した。**その軸が誤り**
/// だった——落とすのは**会話履歴からだけ**で、記録（`memory_sessions`）には完全な本文が残る。
/// 読み直せないもの（`execute_shell` の出力）も失われない。
///
/// 実害（本番実測 2026-08-21）: らぼみのプロンプトが 46 万文字に達し、cursor(grok) が空応答を
/// 返して沈黙した。しきい値は実測 20〜30 万文字。tool_result の内訳は execute_shell 10 万・
/// inner_voice 6 万・read_my_history 3.2 万…で、**読みだけを参照化しても 5.7% しか減らない**。
///
/// **ターン内の挙動は変わらない**（ツール往復は会話再構成を通らない）。落とすのは次のターン
/// 以降への持ち越しだけで、往復は増えない。
///
/// **失敗した結果は本文を残す**（#707 / #709 レビュー指摘）。参照へ潰すとエラー理由が消え、
/// 成功したように読める文字列に化ける——握り潰しであり #692 / #284 の理念に反する。ツール層の
/// 失敗（`success: false`）だけでなく、**コマンドの非ゼロ終了**も対象（`execute_shell` は非ゼロ
/// でも `success: true` を返すため、それだけでは捕まらない）。
///
/// 成功した結果の本文は記録（`memory_sessions`）に残る。退避しきい値を超えた大きい結果は
/// `workspace/tmp` にも残り、平文の退避 notice（非 JSON）はここを素通しするので読み方の案内が
/// 壊れない。**しきい値未満の結果はエージェントからは読み直せない**ので、再取得を案内するのは
/// 同じものが返ると保証できるとき（読み・一覧）だけにする。
pub(crate) fn result_reference(tool_name: &str, result_json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(result_json) {
        Ok(v) => v,
        // 判断材料が無いので推測で捨てず、そのまま残す。
        Err(_) => return result_json.to_string(),
    };
    if v.get("success").and_then(|x| x.as_bool()) != Some(true) {
        return result_json.to_string();
    }
    let null = serde_json::Value::Null;
    let d = v.get("data").unwrap_or(&null);

    // ディレクトリ一覧: 件数と正しい再取得ツール。
    if let Some(entries) = d.get("entries").and_then(|x| x.as_array()) {
        let path = d.get("path").and_then(|x| x.as_str()).unwrap_or("?");
        return format!(
            "{path} を一覧した（{} 件・内容は会話に残していない。必要ならもう一度 {tool_name} で見る）",
            entries.len()
        );
    }

    // ファイルの読み: 元のファイル名がそのまま参照になる。
    if let Some(path) = d.get("path").and_then(|x| x.as_str()) {
        if d.get("content").is_some() {
            let start = d.get("start_line").and_then(|x| x.as_u64());
            let lines = d
                .get("content")
                .and_then(|x| x.as_str())
                .map(|c| c.lines().count());
            let range = match (start, lines) {
                (Some(st), Some(n)) if n > 0 => format!("{st}〜{} 行目", st as usize + n - 1),
                (Some(st), _) => format!("{st} 行目から"),
                (None, Some(n)) => format!("{n} 行"),
                _ => "全体".to_string(),
            };
            let tokens = d
                .get("estimated_tokens")
                .and_then(|x| x.as_u64())
                .map(|t| format!("・約 {t} トークン"))
                .unwrap_or_default();
            let more = if d.get("has_more").and_then(|x| x.as_bool()) == Some(true) {
                "・続きあり"
            } else {
                ""
            };
            return format!(
                "{path} の {range} を読んだ{tokens}{more}（本文は会話に残していない。必要ならもう一度 {tool_name} で読む）"
            );
        }
    }

    // コマンド実行: 終了コードと規模。
    //
    // **失敗したコマンドは stderr を本文で残す**（#709 レビュー指摘）。`execute_shell` は非ゼロ
    // 終了でもツール層では `success: true` を返すので、上の `success != true` 判定では捕まらない。
    // ここを塞がないと**コンパイルエラーやテスト失敗の理由がターンをまたぐと消える**——エージェント
    // が最も頻繁に読むものであり、握り潰しになる（#692 / #284 の理念に反する）。
    if let Some(code) = d.get("exit_code").and_then(|x| x.as_i64()) {
        let out = d.get("stdout").and_then(|x| x.as_str()).unwrap_or("");
        let err = d.get("stderr").and_then(|x| x.as_str()).unwrap_or("");
        if code != 0 {
            // **失敗したコマンドは結果をそのまま残す**（`success: false` と同じ扱い）。
            //
            // 当初は stderr だけを残したが、それでは**片肺**だった（#709 レビュー 2 巡目）:
            // `cargo build` のコンパイルエラーは stderr に出るが、`cargo test` / pytest / jest の
            // **assert 失敗やパニックの詳細は stdout** に出る。エージェントが最も頻繁に読むものを
            // 取りこぼしていた。どちらに出るかをこちらが決められない以上、失敗は丸ごと残す。
            //
            // 会話を圧迫する心配は要らない——1 件あたりは上流の退避しきい値（2,500 トークン）で
            // 既に頭打ちで、それを超える大きい失敗出力は退避ファイル側に落ちている。
            return result_json.to_string();
        }
        return format!(
            "終了コード 0・出力 {} 文字{}（本文は会話に残していない）",
            out.chars().count(),
            if err.is_empty() {
                String::new()
            } else {
                format!("・stderr {} 文字", err.chars().count())
            }
        );
    }

    // それ以外（記憶・検索・内なる声など）: 規模だけ残す。何を呼んだかは tool_name が示す。
    //
    // **「もう一度呼ぶ」とは言わない**（#709 レビュー指摘）。ここへ落ちるツールには非冪等なものが
    // 混じる——`generate_inner_voice` を呼び直すと**別の思考**が生成され、過去のそれは回収できない。
    // 回収できないものを回収できることにする誘導は、失敗を成功に見せるのと同じ質の嘘になる。
    // 再取得を案内するのは、同じものが返ると保証できるとき（読み・一覧）だけにする。
    let size = serde_json::to_string(d)
        .map(|t| t.chars().count())
        .unwrap_or(0);
    format!("結果 {size} 文字（本文は会話に残していない）")
}

/// heartbeat セッションで過去に積まれた指示文（プロンプト scaffolding）か（#501）。
///
/// 以前は `scheduler.rs::run_one_heartbeat` が発火のたびに `log_type='system'` かつ
/// `speaker_id='heartbeat'` で同一文面の指示文（「[ハートビート] 現在の会話…出力形式:
/// SPEAK/LEARN/IDLE」）をセッションログへ挿入していた。毎 tick 積まれて会話へ再注入され、
/// 「同じ指示 → IDLE」の対が何十回も文脈に並んで挙動を歪めていた（本番の heartbeat
/// セッションでは system の 192 件がこの重複）。**#501 で指示文は system プロンプトへ移し**
/// （`scheduler::run_one_heartbeat`）、書き込み側（scheduler）は挿入をやめた。既存 DB に
/// 積まれた分は DB を書き換えず、会話再構成でここが落とす。
///
/// subtask の完了本文（`settle_completed` が書く `system` かつ **`speaker_id=None`**,
/// #404 / #405）とは `speaker_id` で区別する。完了本文は次 tick で読む契約があるので
/// **落とさない**。判定は `memory_index::is_heartbeat_noise` と同じ述語で、
/// `speaker_id='heartbeat'` を書くのは（過去も含め）`run_one_heartbeat` だけ（grep 済み）。
fn is_heartbeat_prompt_scaffolding(log: &opencrab_db::queries::SessionLogRow) -> bool {
    log.log_type == "system"
        && log.speaker_id.as_deref() == Some(opencrab_db::queries::HEARTBEAT_SPEAKER_ID)
}

/// 会話文字列に載せるログだけを残す（#291 / #501）。
///
/// `evaluation` を落とす（#291）のに加え、heartbeat 指示文 scaffolding
/// （[`is_heartbeat_prompt_scaffolding`]）は**会話から全件落とす**（#501）。指示文はその
/// tick の system プロンプトへ 1 度だけ入る（`scheduler::run_one_heartbeat`）ようになったので
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

/// 直近会話ウィンドウを**予算だけ**で組む（#609 / #610 レビュー①②）。
///
/// セッションの全ログを取得し、`fit_logs_to_budget` が末尾から `budget_tokens`
/// いっぱいまで詰める。窓の大きさは予算のみが決め、取得件数の固定上限は無い。
/// 落ちた分はすべて `fit_logs_to_budget` の省略マーカーが告知する ——
/// `fit` に全件が渡るので「渡る前に対象外になって黙って消える」行が存在しない。
///
/// 索引の進み具合（旧 `indexed_boundary`）にも取得上限（旧 `RECENT_LOG_FETCH_LIMIT=500`）にも
/// 依存しない。全ログを渡すので #284 の「直近ユーザー発言の混ぜ戻し」（旧
/// `merge_recent_user_speeches`）も不要になった——直近ユーザー発言は必ず入力に含まれ、
/// `fit_logs_to_budget` の必須枠（`RECENT_MIN_USER_SPEECHES`）が拾う。
///
/// 圧縮パス（topic 要約あり）と topic 無しフォールバックの両方がこの 1 本を共有する。
fn build_recent_window(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    budget_tokens: usize,
) -> String {
    let logs = match opencrab_db::queries::list_session_logs_by_session(conn, session_id) {
        Ok(l) => retain_conversation_logs(l), // ASC のまま。reverse 不要
        Err(e) => {
            tracing::warn!(session_id = %session_id, "failed to list session logs for recent window: {e}");
            return String::new();
        }
    };
    fit_logs_to_budget(&logs, agent_id, budget_tokens)
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
    // #536: 省略マーカーも実際に出力へ含まれるのに予算へ計上していなかった（会計のバグ）。
    // 連続区間（tail）は tail_budget 内に収めてあるが、前置するマーカーぶんが未計上で、
    // 通常サイズの行でも出力が予算を数十トークン超えることがあった。組み上げた総量が予算を
    // 超えるなら、連続区間の**最古**を 1 行ずつ落として（落ちた分はマーカーが件数として
    // 吸収する）予算内へ収める。floor（`RECENT_MIN_LOGS`）と must は割らない —— 直近下限
    // だけで予算を超える極小予算では従来どおり超過する（floor は #536 の対象外）。#284 の
    // 最新ユーザー発言（飛び地）と #404 の末尾行は末尾側なので削られない。
    let render = |start_idx: usize| -> String {
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
                // 飛び地と連続区間のあいだに落とした分（古い飛び地の連投を含む）。
                // #536 のトリムで連続区間が飛び地のすぐ後ろまで縮むと空区間になりうるので、
                // 非空のときだけ出す（空マーカー "0 older messages" を作らない）。
                if idx + 1 < start_idx {
                    parts.push(format_omission_marker(&logs[idx + 1..start_idx]));
                }
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
    };

    let mut out = render(start_idx);
    while estimate_tokens(&out) > budget_tokens && formatted.len() - start_idx > RECENT_MIN_LOGS {
        start_idx += 1;
        out = render(start_idx);
    }
    out
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
            // #707: **読みの本文は次のターンへ持ち越さない**。
            //
            // 以前はツール結果の JSON を丸ごと会話へ再生していた。実測（直近 100 件）では
            // tool_result 37 件 22KB に対し人と自分の発言は 13 件 2KB——**会話の 9 割が作業の
            // 残骸で、人の言葉は 5%**。この状態でコンパクションが走れば押し出されるのは古い
            // 人の発言になる（#284「ユーザー発言が 1 件も残らない」／#692 の捏造の下地）。
            //
            // 読みは**もう一度呼べば同じものが得られる**ので、会話には参照だけを残す。落とす
            // のは次のターン以降への持ち越しだけで、そのターンの中では従来どおり本文がモデル
            // へ渡る（ツール往復は会話再構成を通らない）。記録（DB）も完全なまま残す。
            format!(
                "[tool_result]{}:\n[id={}]: {} → {}",
                ts,
                tool_call_id,
                tool_name,
                result_reference(tool_name, &log.content)
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

/// `[Past context summary]` の予算上限（#406）。
///
/// 事故当時、このセクションには上限が無く、topic 2,446 件・要約 248,340 文字が全件
/// 連結され、1 ハートビートの入力が 284,486 トークンになっていた。ここで固定するのは
/// **上限が効くこと**と**切り詰めの向き（新しい方が残る）**の 2 点。
#[cfg(test)]
mod past_summary_budget_tests {
    use super::{
        build_conversation_string, estimate_tokens, PAST_SUMMARY_BUDGET_DEN,
        PAST_SUMMARY_BUDGET_NUM,
    };

    const AGENT: &str = "a1";
    const SESSION: &str = "s1";

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
    /// `topics.is_empty()` の fallback 経路（`build_recent_window`）とは別物で、
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

/// #609: 直近ウィンドウは索引の進み具合ではなく**予算**で決まる。
///
/// 索引ビルダーが現ターンとほぼ同時刻まで進むと `indexed_boundary`（topic が覆う最後の
/// log_id）がライブ末尾に張り付き、旧実装では `id > boundary` がほぼ空になって下限
/// フォールバック（`RECENT_MIN_LOGS`）へ縮退し、予算が大量に余っているのに直近 raw が
/// 十数件しか載らなかった。ここでは**索引が全ログを覆った（末尾に張り付いた）状態**を
/// 作り、それでも直近ウィンドウが予算ぶんの raw を載せることを固定する。
#[cfg(test)]
mod budget_driven_recent_window_tests {
    use super::{
        build_conversation_string, build_recent_window, retain_conversation_logs, RECENT_MIN_LOGS,
    };

    const AGENT: &str = "a1";
    const SESSION: &str = "s1";

    /// 任意の log_type / speaker で 1 行積む（retain が落とす行の混入や #284 の作り込み用）。
    fn insert_raw(
        conn: &rusqlite::Connection,
        log_type: &str,
        speaker: Option<&str>,
        content: &str,
    ) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: SESSION.to_string(),
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

    /// `n` 件の（エージェント自身の）発話を積む。id は 1..=n（autoincrement）。
    fn seed_logs(conn: &rusqlite::Connection, n: usize) {
        for i in 0..n {
            opencrab_db::queries::insert_session_log(
                conn,
                &opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: AGENT.to_string(),
                    session_id: SESSION.to_string(),
                    log_type: "speech".to_string(),
                    content: format!(
                        "recent log line {i} about the release plan and the follow-up work"
                    ),
                    speaker_id: Some(AGENT.to_string()),
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                },
            )
            .unwrap();
        }
    }

    /// 索引が全ログ（1..=end）を覆う topic を 1 件置く。`end_log_id` を最終 log_id に
    /// することで `indexed_boundary` をライブ末尾へ張り付かせる（＝旧実装の縮退条件）。
    fn seed_topic_covering_all(conn: &rusqlite::Connection, end_log_id: i64) {
        opencrab_db::queries::insert_index_node(
            conn,
            &opencrab_db::queries::IndexNodeRow {
                id: "t-all".to_string(),
                agent_id: AGENT.to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: "作業ログ".to_string(),
                summary: "リリース準備の一連の作業をまとめた要約。".to_string(),
                start_log_id: Some(1),
                end_log_id: Some(end_log_id),
                source_session_id: Some(SESSION.to_string()),
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-08-14T00:00:00Z".to_string(),
                updated_at: "2026-08-14T00:00:00Z".to_string(),
                short_id: Some("t-all".to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }

    /// 索引が末尾に張り付いていても、直近ウィンドウは予算ぶんの raw を載せる。
    ///
    /// 旧実装なら `id > indexed_boundary` が空 → `RECENT_MIN_LOGS`(=10) 件へ縮退し、
    /// ここが十数件で頭打ちになる。予算駆動なら残り予算いっぱいまで詰まる。
    #[test]
    fn recent_window_fills_budget_even_when_index_reaches_live_tail() {
        let conn = opencrab_db::init_memory().unwrap();
        const N: usize = 200;
        seed_logs(&conn, N);
        // 索引を全ログの末尾へ張り付かせる（id は 1..=N）。
        seed_topic_covering_all(&conn, N as i64);

        // 全文（~200 件）は予算を超えてコンパクションへ入るが、残り予算は十数件どころか
        // 数十件を載せられるだけ余っている。
        const BUDGET: usize = 2_000;
        let out = build_conversation_string(&conn, SESSION, AGENT, BUDGET).unwrap();

        // 前提: topic 要約ありのコンパクション経路に入っている。
        assert!(
            out.contains("[Past context summary"),
            "コンパクション経路に入っていない（テストの前提が崩れている）: {out}"
        );
        // 直近ウィンドウが存在する。
        let recent = &out[out
            .find("[Recent conversation]")
            .unwrap_or_else(|| panic!("直近会話セクションが無い: {out}"))..];

        // 索引が末尾に張り付いていても、下限（10 件）へ縮退せず予算ぶんが載る。
        let raw_count = recent.matches("recent log line").count();
        assert!(
            raw_count >= 40,
            "直近 raw が予算ぶん載っていない（索引の進み具合で縮退している疑い）: \
             {raw_count} 件 (RECENT_MIN_LOGS={RECENT_MIN_LOGS}): {recent}"
        );
        // 「直近 10 件」より深いログまで届いている（旧実装では末尾 10 件=190..199 しか
        // 載らず、これは落ちる）。
        assert!(
            recent.contains("recent log line 160"),
            "予算があるのに深いログまで届いていない（末尾 10 件で頭打ち）: {recent}"
        );
    }

    /// 取得件数の固定上限が無い（#610 レビュー①）。旧 `RECENT_LOG_FETCH_LIMIT=500` の頭打ちが消えた。
    ///
    /// `build_recent_window` を直接叩き、500 を超える件数を積んで巨大予算を渡す。予算で 1 件も
    /// 落ちないので、**一番古い行（末尾から N 件目）まで**出力に載る。旧実装は末尾 500 件しか
    /// 取得しなかったので index 0（＝末尾から 700 件目）は原理的に載らなかった。
    #[test]
    fn recent_window_has_no_fixed_fetch_cap() {
        let conn = opencrab_db::init_memory().unwrap();
        const N: usize = 700; // > 旧 RECENT_LOG_FETCH_LIMIT (500)
        seed_logs(&conn, N);
        // 予算で 1 件も落とさない（全件が fit に載る）。
        let out = build_recent_window(&conn, SESSION, AGENT, usize::MAX);
        assert!(
            out.contains("recent log line 0"),
            "末尾 500 件より深い最古の行が載っていない（取得上限が残っている疑い）"
        );
        assert!(
            out.contains(&format!("recent log line {}", N - 1)),
            "最新行が載っていない"
        );
    }

    /// **pre-fit 欠落ゼロ**（#610 レビュー①の核心）。`fit` に渡る入力が
    /// `retain_conversation_logs(全件)` と一致する ——「fit に渡る前に対象外になって黙って消える行」
    /// がゼロであること。#609 が本当に守りたいのはこれで、省略マーカーの**文言には依存しない**。
    ///
    /// evaluation（#291）と heartbeat scaffolding（#501）は retain が落とすが、それ以外の全ログは
    /// fit に渡る。巨大予算で fit が 1 件も落とさない状態にし、**retain 後の全件が出力に現れ、
    /// retain が落とす行は現れない**ことを固定する。
    #[test]
    fn recent_window_feeds_every_retained_log_to_fit() {
        let conn = opencrab_db::init_memory().unwrap();
        // 会話に載る行（speech）を積む。
        seed_logs(&conn, 30);
        // retain が落とす行を途中に混ぜる: evaluation（#291）と heartbeat scaffolding（#501）。
        insert_raw(
            &conn,
            "evaluation",
            Some("evaluator"),
            "採点結果は却下マーカー",
        );
        insert_raw(
            &conn,
            "system",
            Some(opencrab_db::queries::HEARTBEAT_SPEAKER_ID),
            "巡回指示マーカー",
        );
        // 落とす行の後ろにも会話が続く形にする。
        for i in 30..35 {
            insert_raw(
                &conn,
                "speech",
                Some(AGENT),
                &format!("recent log line {i} tail"),
            );
        }

        // 期待値: retain 後の全件。fit に渡る入力がこれと一致することを固定する。
        let all = opencrab_db::queries::list_session_logs_by_session(&conn, SESSION).unwrap();
        let retained = retain_conversation_logs(all);
        assert_eq!(retained.len(), 35, "retain の残存件数が想定と違う");

        // 巨大予算 → fit は 1 件も落とさない。
        let out = build_recent_window(&conn, SESSION, AGENT, usize::MAX);

        // retain が残す行はすべて出力に現れる（pre-fit で 1 件も落ちない）。
        for log in &retained {
            assert!(
                out.contains(&log.content),
                "retain 後の行が fit に渡っていない（pre-fit 欠落）: {}",
                log.content
            );
        }
        // retain が落とす行は現れない。
        assert!(
            !out.contains("採点結果は却下マーカー"),
            "evaluation が混ざった: {out}"
        );
        assert!(
            !out.contains("巡回指示マーカー"),
            "heartbeat scaffolding が混ざった: {out}"
        );
    }

    /// #284 の維持（`merge_recent_user_speeches` 削除後）。大量のツール往復で古いユーザー発言が
    /// 末尾から押し出されても、**一番新しいユーザー発言**は小予算でも必ず載る。
    ///
    /// 全ログを fit へ渡すので、混ぜ戻し（旧 merge）が無くても直近ユーザー発言は入力に含まれ、
    /// `fit_logs_to_budget` の必須枠（`RECENT_MIN_USER_SPEECHES` / 飛び地 A′）が拾う。
    #[test]
    fn newest_user_speech_survives_tool_flood_after_merge_removal() {
        let conn = opencrab_db::init_memory().unwrap();
        // 一番古い位置に置くユーザー発言（これが「今の指示」で、末尾からは押し出される）。
        insert_raw(
            &conn,
            "speech",
            Some("kojira"),
            "この指示は消えてはいけない",
        );
        // 末尾を埋める巨大なツール往復（ユーザー発言を連続区間の外へ押し出す）。
        for i in 0..40 {
            insert_raw(
                &conn,
                "tool_result",
                Some(AGENT),
                &format!("結果 {i}: {}", "x".repeat(400)),
            );
        }
        // topic を 1 件置いてコンパクション経路（切り詰めではない方）へ入れる。
        seed_topic_covering_all(&conn, 100);

        let out = build_conversation_string(&conn, SESSION, AGENT, 400).unwrap();
        assert!(
            out.contains("[Past context summary"),
            "テストの前提: コンパクション経路に入ること: {out}"
        );
        assert!(
            out.contains("この指示は消えてはいけない"),
            "一番新しいユーザー発言が押し出された（#284 が壊れた）: {out}"
        );
    }

    /// topic 無しフォールバックも圧縮パスと同じ `build_recent_window` を通り、予算駆動になる（#610 レビュー②）。
    ///
    /// topic を 1 件も置かず切り詰めフォールバックへ落とす。予算はコンパクションを起こすが、
    /// 下限（`RECENT_MIN_LOGS`=10）ではなく予算ぶん（数十件）が載る。取得上限が無いこと自体は
    /// 共有関数を直接叩く [`recent_window_has_no_fixed_fetch_cap`] で固定済み。
    #[test]
    fn topic_less_fallback_routes_through_budget_driven_window() {
        let conn = opencrab_db::init_memory().unwrap();
        const N: usize = 300;
        seed_logs(&conn, N); // topic は置かない → 切り詰めフォールバック
        const BUDGET: usize = 2_000;
        let out = build_conversation_string(&conn, SESSION, AGENT, BUDGET).unwrap();
        // 前提: topic 無しの切り詰めフォールバック（要約は出ない / ヘッダは出る）。
        assert!(
            !out.contains("[Past context summary"),
            "切り詰め経路になっていない: {out}"
        );
        assert!(
            out.contains("[Note: Earlier messages were omitted"),
            "コンパクションが起きていない（マーカー無し）: {out}"
        );
        // 予算駆動: 下限 10 件ではなく予算ぶん（>=40 件）が載る。
        let raw_count = out.matches("recent log line").count();
        assert!(
            raw_count >= 40,
            "予算があるのに下限件数へ縮退している: {raw_count} 件 (RECENT_MIN_LOGS={RECENT_MIN_LOGS})"
        );
        // 一番新しい行は必ず載る。
        assert!(
            out.contains(&format!("recent log line {}", N - 1)),
            "最新行が載っていない: {out}"
        );
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
/// フォールバック**（[`super::build_recent_window`]）を埋める。
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

    /// topic 要約が 1 件も無い（＝`build_recent_window` フォールバック）状態で
    /// 履歴が予算を大きく超えても、組み上がった会話は予算内に収まる。
    ///
    /// 既存の fits テストは全て topic ありの summary 経路。topic 生成が追いつく前の
    /// セッションや要約が引けない経路はこの切り詰めフォールバックへ落ちるため、そこも
    /// 頭打ちになることを固定する。行は通常サイズ（下限が巨大行で予算を割る #536 の
    /// floor 経路とは別条件）。
    ///
    /// #536: 省略マーカーを予算へ計上したので、以前は必要だった余白（`MARKER_SLACK`）を
    /// 外し、**厳密に `<= BUDGET`** で固定する。
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
            toks <= BUDGET,
            "切り詰め経路で出力が予算 {BUDGET} を超えた: {toks} トークン。\
             省略マーカーの計上（#536）が効いていない可能性がある"
        );
    }

    /// #536 の回帰ガード: 省略マーカーを予算へ計上する前は、通常サイズの行でも複数の
    /// 予算値で出力が予算を数十トークン超えていた（実測 budget=6,000 で +12、10,000 で
    /// +11）。マーカー計上後は**どの予算でも厳密に `<= budget`**。マーカーが実際に出る
    /// （コンパクションが起きる）予算帯を複数点で固定する。
    #[test]
    fn omission_markers_are_counted_so_output_never_exceeds_budget() {
        let conn = opencrab_db::init_memory().unwrap();
        // 全文が下の最大予算（20,000）も超えるだけの通常サイズ行を積む（topic 無し →
        // 切り詰め経路）。どの予算でもコンパクション（＝マーカー）が起きるようにする。
        for i in 0..2_000 {
            insert_speech(
                &conn,
                &format!("log line {i} about the release plan and the follow-up work"),
            );
        }
        // マーカーが出る（＝コンパクションが起きる）予算帯を複数点で。#536 前は
        // 6,000 / 10,000 で超過していた。
        for budget in [2_000usize, 4_000, 6_000, 8_000, 10_000, 20_000] {
            let out = build_conversation_string(&conn, SESSION, AGENT, budget).unwrap();
            assert!(
                out.contains("[Note: Earlier messages were omitted"),
                "budget={budget} でコンパクションが起きていない（マーカー計上を検証できない）: {out}"
            );
            let toks = estimate_tokens(&out);
            assert!(
                toks <= budget,
                "budget={budget} で出力が予算を超えた: {toks} トークン（+{}）。\
                 マーカー計上（#536）の回帰",
                toks - budget
            );
        }
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
/// 以前は `scheduler.rs::run_one_heartbeat` が発火のたびに同一文面の指示文（`system` /
/// `speaker_id='heartbeat'`）をセッションログへ挿入し、それが全件そのまま会話へ復元されて
/// いた。本番の heartbeat セッションでは同一指示が 192 件並び、「同じ指示 → IDLE」の対を
/// 何十回も見せて挙動を歪めていた。#501 で指示文は system プロンプトへ移した
/// （`scheduler::run_one_heartbeat`）ので、会話再構成では指示文 scaffolding を**全件落とす**。
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

/// #691: 応答直前の出力指示（`RESPONSE_ONLY_DIRECTIVE`）が履歴の直後に付くこと、
/// および履歴が空のときは付かないことを固定する。
#[cfg(test)]
mod response_only_directive_tests {
    use super::{build_conversation_string, NO_MESSAGES_MARKER, RESPONSE_ONLY_DIRECTIVE};

    fn seed_speech(conn: &rusqlite::Connection, speaker: &str, content: &str) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "a1".to_string(),
                session_id: "s1".to_string(),
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

    #[test]
    fn directive_is_appended_after_history() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_speech(&conn, "owner", "こんばんは");
        let out = build_conversation_string(&conn, "s1", "a1", 100_000).unwrap();
        // 生成点に最も近い＝出力の末尾に置く。
        assert!(out.trim_end().ends_with(RESPONSE_ONLY_DIRECTIVE), "{out}");
        // 1 回だけ。
        assert_eq!(out.matches(RESPONSE_ONLY_DIRECTIVE).count(), 1);
        // 履歴（発話）は指示より前にある。
        let hist_pos = out.find("こんばんは").unwrap();
        let dir_pos = out.find(RESPONSE_ONLY_DIRECTIVE).unwrap();
        assert!(hist_pos < dir_pos, "{out}");
    }

    #[test]
    fn directive_is_omitted_when_history_is_empty() {
        let conn = opencrab_db::init_memory().unwrap();
        // ログを 1 件も積まない。
        let out = build_conversation_string(&conn, "s1", "a1", 100_000).unwrap();
        assert_eq!(out, NO_MESSAGES_MARKER);
        assert!(!out.contains(RESPONSE_ONLY_DIRECTIVE), "{out}");
    }
}

/// #709: **ツール結果の本文を次のターンへ持ち越さない**ことの回帰ガード。
#[cfg(test)]
mod result_reference_tests {
    use super::result_reference;

    /// 読みは元のファイル名がそのまま参照になる。
    #[test]
    fn read_results_leave_only_a_reference() {
        let body = "秘密の設計メモ本文".repeat(50);
        let result = serde_json::json!({
            "success": true,
            "data": {
                "path": "docs/design.md",
                "content": body,
                "start_line": 1,
                "estimated_tokens": 18_000,
                "has_more": true,
            }
        })
        .to_string();

        let r = result_reference("ws_read", &result);
        assert!(
            !r.contains("秘密の設計メモ本文"),
            "本文が会話へ載っている: {r}"
        );
        assert!(r.contains("docs/design.md"), "ファイル名が無い: {r}");
        assert!(r.contains("18000"), "規模が無い: {r}");
        assert!(r.contains("続きあり"), "続きの有無が無い: {r}");
    }

    /// #709 の中心: **コマンド実行の結果も**本文を持ち越さない。
    ///
    /// #707 は「読み直せないので落としたら失われる」として本文を残していたが、記録
    /// （memory_sessions）には完全な本文が残るので失われない。らぼみの tool_result 30 万文字の
    /// うち execute_shell が 10 万で最大——ここを落とさないと沈黙は解けない。
    #[test]
    fn shell_results_also_leave_only_a_reference() {
        let out = "x".repeat(50_000);
        let result = serde_json::json!({
            "success": true,
            "data": {"exit_code": 0, "stdout": out, "stderr": ""}
        })
        .to_string();

        let r = result_reference("execute_shell", &result);
        assert!(
            !r.contains(&"x".repeat(100)),
            "コマンド出力が会話へ載っている（#709 の状態に戻っている）: {r:.120}"
        );
        assert!(r.contains("終了コード 0"), "終了コードが無い: {r}");
        assert!(r.contains("50000"), "出力の規模が無い: {r}");
    }

    /// #709 レビュー指摘: **コマンドの非ゼロ終了は stderr を本文で残す**。
    ///
    /// `execute_shell` は非ゼロ終了でもツール層では `success: true` を返すので、`success` 判定
    /// では捕まらない。ここを塞がないとコンパイルエラーやテスト失敗の理由がターンをまたぐと
    /// 消える——エージェントが最も頻繁に読むものであり、握り潰しになる。
    #[test]
    fn failed_commands_keep_their_stderr() {
        let result = serde_json::json!({
            "success": true,
            "data": {
                "exit_code": 1,
                "stdout": "",
                "stderr": "error[E0308]: mismatched types\n  --> src/main.rs:42:9"
            }
        })
        .to_string();

        let r = result_reference("execute_shell", &result);
        assert!(
            r.contains("E0308") && r.contains("src/main.rs:42"),
            "失敗の理由（stderr）が消えている: {r}"
        );
        assert!(
            r.contains("exit_code") || r.contains("1"),
            "失敗だと分からない: {r}"
        );
    }

    /// #709 レビュー 2 巡目: **失敗詳細が stdout に出るケース**（cargo test / pytest / jest）でも
    /// 本文が残る。stderr だけを残す形では `cargo build` しか塞げていなかった。
    #[test]
    fn failed_commands_keep_stdout_details_too() {
        let result = serde_json::json!({
            "success": true,
            "data": {
                "exit_code": 101,
                "stdout": "thread 'tests::budget' panicked at src/lib.rs:88:\nassertion failed: used <= budget",
                "stderr": "error: test failed, to rerun pass `-p opencrab-core --lib`"
            }
        })
        .to_string();

        let r = result_reference("execute_shell", &result);
        assert!(
            r.contains("assertion failed") && r.contains("tests::budget"),
            "stdout に出た失敗詳細が消えている: {r}"
        );
    }

    /// #709 レビュー指摘: 非冪等なツールに「もう一度呼ぶ」と言わない。
    ///
    /// `generate_inner_voice` を呼び直すと**別の思考**が生成され、過去のそれは回収できない。
    /// 回収できないものを回収できることにする誘導は、失敗を成功に見せるのと同じ質の嘘になる。
    #[test]
    fn non_idempotent_tools_do_not_promise_recovery() {
        let result = serde_json::json!({
            "success": true, "data": {"voice": "思考の断片".repeat(200)}
        })
        .to_string();

        let r = result_reference("generate_inner_voice", &result);
        assert!(
            !r.contains("思考の断片思考の断片"),
            "本文が載っている: {r:.80}"
        );
        assert!(
            !r.contains("もう一度"),
            "回収できないのに再取得を約束している: {r}"
        );
    }

    /// 失敗した結果は本文を残す（参照へ潰すと「成功した」ことに化ける）。
    #[test]
    fn failed_results_keep_their_error() {
        let failed = serde_json::json!({
            "success": false, "data": null, "error": "path not found: docs/missing.md"
        })
        .to_string();

        let r = result_reference("ws_read", &failed);
        assert!(r.contains("path not found"), "失敗の理由が消えている: {r}");
        assert!(!r.contains("読んだ"), "読めていないのに読んだことに: {r}");
    }

    /// 一覧は件数と正しい再取得ツールを出す。
    #[test]
    fn list_reference_points_at_the_right_tool() {
        let listed = serde_json::json!({
            "success": true, "data": {"path": "src", "entries": ["a.rs","b.rs","c.rs"]}
        })
        .to_string();

        let r = result_reference("ws_list", &listed);
        assert!(r.contains("3 件"), "件数が無い: {r}");
        assert!(r.contains("ws_list"), "誤ったツールへ誘導: {r}");
    }

    /// その他のツール（記憶・検索など）も規模だけ残す。
    #[test]
    fn other_tools_leave_size_only() {
        let big = serde_json::json!({
            "success": true, "data": {"hits": vec!["長い検索結果".repeat(500)]}
        })
        .to_string();

        let r = result_reference("search_my_history", &big);
        assert!(
            !r.contains("長い検索結果長い検索結果"),
            "本文が載っている: {r:.80}"
        );
        assert!(r.contains("文字"), "規模が分からない: {r}");
    }
}
