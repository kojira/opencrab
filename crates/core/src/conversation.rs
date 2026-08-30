//! 会話文字列の組み立て（トークン予算ベースのコンパクション対応）。
//!
//! セッションログから LLM へ渡す会話文字列を構築する。`build_ledger_section`
//! （[`crate::task_ledger`]）/ `build_impression_section`（[`crate::impression_section`]）
//! と同型で、`conn` を取り会話用のセクションを組む純粋ロジック。server / gateway の型に
//! 依存しないため core に置く（#518 手順 3〜4）。呼び出し元は `server::process`
//! （既存パスを保つ再エクスポート）。

use crate::context_budget::{assemble_from_snapshot, TurnGovernor, CONTEXT_BUDGET_EXHAUSTED};
use crate::tokens::estimate_tokens;

/// コンパクション時に最低限保持する最近のログ件数。
/// 旧 `fit_logs_to_budget` 経路（対比テスト用）だけが参照する。
#[allow(dead_code)]
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
pub const NO_MESSAGES_MARKER: &str = "No messages yet.";

/// 応答直前に会話履歴の末尾へ置く出力指示（#691）。
///
/// opus-5 は 1:1 の長い会話で「次のユーザー発言」を続きとして**捏造**する傾向がある
/// （モデル固有の挙動・オーナー観測）。会話履歴は `[ID] [時刻]:` 形式で 1 行 1 発話に
/// 連結されるため、生成モデルが最も自然な予測として「次の話者行」を書き足してしまう。
/// 生成点に最も近い指示が最も効く（オーナー裁定・実測）ので、履歴の**直後**
/// （＝生成点の直前）にこの 1 行だけを置く。ロール分離・履歴形式の変更・出力の
/// フィルタはしない（オーナー裁定で対策から除外）。履歴が空（`NO_MESSAGES_MARKER`）の
/// ときは真似る対象が無いので付けない。
pub const RESPONSE_ONLY_DIRECTIVE: &str = "ここから先はあなた自身の本文のみを書く。`[ID] [時刻]:` 形式の行（他の話者の発言の再現・引用・続き）を出力してはならない。";

/// セッションログから会話文字列を構築する（トークン予算ベースのコンパクション対応）。
///
/// `context_budget_tokens` はこの会話セクションに使えるトークン予算（`conversation_high`）。
/// Memory Index の注入判定は [`crate::context_budget::apply_line_items`] に一本化する。
/// ここは判定結果（`include_memory_index`）だけを受け取り、部分切り詰めはしない。
pub fn build_conversation_string(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    context_budget_tokens: usize,
) -> Result<String, anyhow::Error> {
    build_conversation_string_with_memory_index(
        conn,
        session_id,
        agent_id,
        context_budget_tokens,
        true,
    )
}

/// [`build_conversation_string`] と同じだが、Memory Index を載せるかを呼び出し側が決める。
pub fn build_conversation_string_with_memory_index(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    context_budget_tokens: usize,
    include_memory_index: bool,
) -> Result<String, anyhow::Error> {
    build_conversation_string_with_waters(
        conn,
        session_id,
        agent_id,
        context_budget_tokens,
        context_budget_tokens / 2,
        include_memory_index,
    )
}

/// 二水位を明示して会話を組む（#826-B）。開始は組立と検査。高水位超過のときだけ刈る。
pub fn build_conversation_string_with_waters(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    conversation_high: usize,
    conversation_low: usize,
    include_memory_index: bool,
) -> Result<String, anyhow::Error> {
    let prefix = build_context_prefix_sections(conn, session_id, agent_id, include_memory_index);

    // conversation_high から引くのは会話車線（台帳・人物像）。MI は fixed 済み。
    let mut inner_budget = conversation_high;
    for section in prefix.billed() {
        inner_budget = inner_budget.saturating_sub(estimate_tokens(section));
    }
    // #536: 最後の `parts.join("\n\n")` の区切りも出力へ含まれるので計上する。
    // 会話車線の区切りだけを conversation_high から引く（MI は fixed 済み）。
    inner_budget = inner_budget.saturating_sub(prefix.billed().count() * estimate_tokens("\n\n"));
    // #691: 履歴の直後に置く出力指示のぶんを会話予算から先に引く。prepend 前の返り値が
    // `context_budget_tokens` を超えないという契約（下の budget テスト群）を保つため、
    // #536 の区切り計上と同じ流儀で組み込み前に確保する。履歴が空で指示を付けない場合は
    // 数十トークン過剰に確保するだけで実害はない（空履歴は "No messages yet." のみ）。
    let directive_cost = estimate_tokens(RESPONSE_ONLY_DIRECTIVE) + estimate_tokens("\n\n");
    inner_budget = inner_budget.saturating_sub(directive_cost);
    let prefix_cost = conversation_high.saturating_sub(inner_budget);
    let inner_low = conversation_low.saturating_sub(prefix_cost);
    let inner = build_conversation_inner(conn, session_id, agent_id, inner_budget, inner_low)?;

    // #691: 履歴が空（真似る対象が無い）ときは出力指示を付けない。
    let history_is_empty = inner == NO_MESSAGES_MARKER;

    let mut parts = prefix.ordered();
    parts.push(inner);
    let mut out = parts.join("\n\n");
    if !history_is_empty {
        // 応答直前の出力指示を履歴の**直後**（＝生成点の直前）へ 1 行だけ置く（#691）。
        out.push_str("\n\n");
        out.push_str(RESPONSE_ONLY_DIRECTIVE);
    }
    Ok(out)
}

/// 会話本文の前に置く固定セクション（台帳 / [Memory Index] / [Impressions]）。
struct ContextPrefixSections {
    ledger: Option<String>,
    memory_index: Option<String>,
    impressions: Option<String>,
}

impl ContextPrefixSections {
    fn billed(&self) -> impl Iterator<Item = &String> {
        self.ledger.iter().chain(self.impressions.iter())
    }

    fn ordered(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if let Some(s) = &self.ledger {
            parts.push(s.clone());
        }
        if let Some(s) = &self.memory_index {
            parts.push(s.clone());
        }
        if let Some(s) = &self.impressions {
            parts.push(s.clone());
        }
        parts
    }
}

/// すべて `session_id` を「いま走っているセッション」として解決する。best-effort で、
/// どれが欠けても会話構築は続行する。
///
/// Memory Index の注入判定は呼び出し側（`apply_line_items`）が行う。
fn build_context_prefix_sections(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    include_memory_index: bool,
) -> ContextPrefixSections {
    // タスク台帳（前向きワーキング状態）を会話の先頭に前置する。
    // system prompt 側は 1h キャッシュされるため、毎ターン変わる台帳状態はここに置く。
    // 台帳の読み出し失敗で返信自体を殺さない（warn して台帳なしで続行）。
    let ledger = match crate::task_ledger::build_ledger_section(conn, agent_id, session_id) {
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
    // 入れる/入れないは `apply_line_items` の判定をそのまま使う。部分切り詰めはしない。
    let memory_index = if include_memory_index {
        match crate::memory_index::build_memory_index_section(conn, agent_id, session_id) {
            Ok(section) => section,
            Err(e) => {
                tracing::warn!(
                    "failed to build memory index section for session {session_id}: {e}"
                );
                None
            }
        }
    } else {
        None
    };

    // [Impressions]: いま話している相手の人物像（#314）。人物像は agent スコープ
    // （経路をまたいで同じ相手なら同じ 1 行）だが、**載せるのは直近の発話者の分だけ**で、
    // 人数もフィールド長もビルダ側で上限が掛かっている。台帳・memory index と同じく
    // best-effort — 読み出しに失敗しても返信は殺さない。
    let impressions =
        match crate::impression_section::build_impression_section(conn, agent_id, session_id) {
            Ok(section) => section,
            Err(e) => {
                tracing::warn!("failed to build impression section for session {session_id}: {e}");
                None
            }
        };

    ContextPrefixSections {
        ledger,
        memory_index,
        impressions,
    }
}

/// 会話文字列本体の構築（タスク台帳の前置は `build_conversation_string` 側で行う）。
///
/// 開始時はスナップショット＋差分の組立と検査だけ。高水位超過のときだけ低水位まで刈る。
/// 現行の開始時 `fit_logs_to_budget` は走らせない。
fn build_conversation_inner(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    conversation_high: usize,
    conversation_low: usize,
) -> Result<String, anyhow::Error> {
    let assembled = assemble_from_snapshot(conn, session_id, agent_id)?;
    if assembled.text == NO_MESSAGES_MARKER {
        return Ok(assembled.text);
    }
    let mut gov = TurnGovernor::new(conversation_high, conversation_low);
    let Some(outcome) = gov.compact_start_if_over(assembled.tokens, &assembled.items) else {
        return Ok(assembled.text);
    };
    if outcome.exhausted {
        return Err(anyhow::anyhow!(
            "{CONTEXT_BUDGET_EXHAUSTED}: conversation tokens after inviolable lanes exceed input_high"
        ));
    }
    Ok(outcome.text)
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
#[allow(dead_code)]
const PAST_SUMMARY_BUDGET_NUM: usize = 3;
#[allow(dead_code)]
const PAST_SUMMARY_BUDGET_DEN: usize = 10;

/// `[Past context summary]` のヘッダ。**予算判定にはこのヘッダぶんも含める**
/// （セクション全体で 30% に収める）。
#[allow(dead_code)]
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
#[allow(dead_code)]
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
/// 実害（本番実測 2026-08-21）: エージェントBのプロンプトが 46 万文字に達し、cursor(grok) が空応答を
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
///
/// **参照が本文より短くならないなら潰さない**（#709 レビュー指摘1）。会話を軽くするための
/// 仕組みが会話を重くするのは本末転倒——`ws_write` などの `{"path":"...","written":true}`
/// のような数十文字の結果は、参照へ化けさせると逆に長くなり、識別子（path）まで消える。長さの
/// 不変条件は入口の `result_reference` が一括で掛け、参照が本文以上なら本文をそのまま残す。
///
/// **失敗は必ず本文ごと残す**（#709 レビュー指摘2）。この系の不変条件——失敗は `success: false`
/// または `data.exit_code != 0` のいずれかでしか表されない——を `signals_failure` に集約する。
/// catch-all の中立文言は成功を主張しないが、それだけでは将来この不変条件を破る新ツールの失敗が
/// 黙って要約されて消える。判定を一箇所に集めることで `failures_are_never_summarized_as_success`
/// テストがどちらの経路が落ちても落ちる。
pub(crate) fn result_reference(tool_name: &str, result_json: &str) -> String {
    let reference = build_result_reference(tool_name, result_json);
    shorter_of_reference_or_body(reference, result_json)
}

/// 長さの不変条件（#709 レビュー指摘1）を 1 箇所に集約する: 参照が本文より短くならないなら
/// 潰す意味がないので本文をそのまま残す。会話を軽くするための仕組みが会話を重くしては本末転倒。
/// `tool_result`（[`result_reference`]）と `subtask_completed`（[`fold_subtask_completed`]）が
/// **同じ判断**を共有し、同じ形の判断を 2 箇所に書かない（[[same-shaped-bugs-mean-one-missing-thing]]）。
/// 小さな結果や、参照器が本文をそのまま返した（失敗・非 JSON）ケースもここを通る。
fn shorter_of_reference_or_body(reference: String, body: &str) -> String {
    if reference.chars().count() >= body.chars().count() {
        body.to_string()
    } else {
        reference
    }
}

/// 失敗を表す形か（#709 レビュー指摘2）。この系の不変条件を一箇所に集約する:
/// **失敗は `success: false` または `data.exit_code != 0` のいずれかでしか表されない**。
/// 失敗した結果を参照へ潰すと「成功した」ように読める文字列へ化ける（握り潰し・#692 / #284）ので
/// 本文を丸ごと残す。`execute_shell` は非ゼロ終了でもツール層では `success: true` を返すため、
/// `success` 判定だけでは捕まらない——両経路をここで見る。将来この不変条件を破る新ツール
/// （`success: true` のまま data の中で失敗を表す等）が入ると失敗が catch-all で「結果 N 文字」へ
/// 潰れて黙って消えるので、判定をここへ集約し `failures_are_never_summarized_as_success` で固定する。
fn signals_failure(v: &serde_json::Value, d: &serde_json::Value) -> bool {
    if v.get("success").and_then(|x| x.as_bool()) != Some(true) {
        return true;
    }
    matches!(d.get("exit_code").and_then(|x| x.as_i64()), Some(code) if code != 0)
}

/// 会話へ残す参照本体を組む。長さの不変条件（参照が本文以上なら本文を残す）は入口の
/// `result_reference` が掛けるので、ここでは形ごとの参照を作ることに集中する。
fn build_result_reference(tool_name: &str, result_json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(result_json) {
        Ok(v) => v,
        // 判断材料が無いので推測で捨てず、そのまま残す。
        Err(_) => return result_json.to_string(),
    };
    let null = serde_json::Value::Null;
    let d = v.get("data").unwrap_or(&null);

    // 失敗は参照へ潰さず本文を丸ごと残す（握り潰し防止）。判定は signals_failure に集約。
    if signals_failure(&v, d) {
        return result_json.to_string();
    }

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

    // コマンド実行（成功のみ）: 終了コードと規模。非ゼロ終了は上の signals_failure が本文ごと
    // 残しているので、ここへ来るのは成功したコマンドだけ——`cargo build` / `cargo test` /
    // pytest / jest の失敗詳細（stderr にも stdout にも出る）はどちらも丸ごと残る。
    if let Some(code) = d.get("exit_code").and_then(|x| x.as_i64()) {
        debug_assert_eq!(
            code, 0,
            "非ゼロ終了は signals_failure が本文ごと残すはず（#709 の不変条件が破れている）"
        );
        let out = d.get("stdout").and_then(|x| x.as_str()).unwrap_or("");
        let err = d.get("stderr").and_then(|x| x.as_str()).unwrap_or("");
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

    // それ以外（記憶・検索・内なる声・mutation など）: 規模だけ残す。何を呼んだかは tool_name が示す。
    //
    // **path があれば参照に含める**（#709 レビュー指摘1）。`ws_write` / `ws_edit` / `ws_delete` /
    // `ws_mkdir` などは `{"path":"...","written":true}` を返す——「何をしたか」の対象を消さない。
    //
    // **「もう一度呼ぶ」とは言わない**（#709 レビュー指摘）。ここへ落ちるツールには非冪等なものが
    // 混じる——`generate_inner_voice` を呼び直すと**別の思考**が生成され、過去のそれは回収できない。
    // 回収できないものを回収できることにする誘導は、失敗を成功に見せるのと同じ質の嘘になる。
    // 再取得を案内するのは、同じものが返ると保証できるとき（読み・一覧）だけにする。
    let size = serde_json::to_string(d)
        .map(|t| t.chars().count())
        .unwrap_or(0);
    match d.get("path").and_then(|x| x.as_str()) {
        Some(path) => {
            format!("{path} を {tool_name} した（結果 {size} 文字・本文は会話に残していない）")
        }
        None => format!("結果 {size} 文字（本文は会話に残していない）"),
    }
}

/// `subtask_completed` の内側 `result`（ツール実行の本文）を会話へ持ち越さない形へ畳む（#713）。
///
/// opencrab は「ツールを常に切り離す」ため、実運用ではツール結果の主経路が **`tool_result` では
/// なく完了本文の入れ子 `result`** になる。#709 が `tool_result` で塞いだのと同じ塊がここに現れる
/// （本番実測でツール結果 JSON が 150 件 / 281,347 文字・subtask_completed 全体の 86%）。
///
/// **判定の核**——`signals_failure`（失敗は本文ごと残す）と長さ不変条件——は [`result_reference`] と
/// **共有**し、**文言だけ** subtask 用に分ける（[[single-source-of-truth-no-parallel-paths]]）。
/// `tool_result` と決定的に違うのは**再取得できないこと**（切り離したサブタスクの結果は取り直せない
/// ＝非冪等・罠3）。だから参照は「もう一度読む」と**約束しない**。代わりに監査の在り処——完了本文の
/// `session_id`（サブセッション）——を指し、全文が記録に残ることだけ伝える。
///
/// 返すのは会話へ載せる `result` の中身。畳めない/畳むべきでない形——生産者A（サブエージェント）の
/// 散文応答・timeout / error の平文・退避 notice・失敗・想定外 JSON——は `result_str` を**そのまま
/// 返す**（握り潰さない・fail-safe）。生産者A の散文は「エージェントが出した答え・報告書」であって
/// 塊ではなく、参照化しない（#713 決定B）。
///
/// **受容した制限（決定B のサイレントな誤分類・#716 レビュー指摘2）**: 生産者A と B を分ける唯一の
/// 信号は**値の形**（`success` 封筒の有無）だけで、完了本文に由来（切り離しツール／サブエージェント
/// 会話）を示すフィールドは無い（`settle_completed` は `engine_result.response` をそのまま `result` へ
/// 載せる）。したがってサブエージェントが最終応答として `{"success":true,"data":{…}}` 形の JSON を
/// 返すと生産者B と区別できず**畳まれる**。これは受容する: 長さ不変条件で小さい答えは残る／DB に全文が
/// 残る／`session_id` ポインタも残る／本番実測で該当例は未発見。由来フィールドを生産者側へ足すのは
/// スコープ外（別 issue）。将来この誤分類が実害化したときここから辿れるよう明記しておく。
fn fold_subtask_completed(exit_reason: &str, result_str: &str) -> String {
    // ゲート1（外側）: completed 以外は本文を丸ごと残す。timeout / error / stopped_by_limit は
    // 「プロセス完了＝clean」ではない。**completed を成功と読み替えない**ための belt——completed でも
    // 下のゲート2でさらに中身を見る（罠1）。
    if exit_reason != "completed" {
        return result_str.to_string();
    }

    // ゲート2（内側・データの形で分岐）。
    let inner: serde_json::Value = match serde_json::from_str(result_str) {
        Ok(v) => v,
        // 非 JSON = 生産者A の散文 / timeout・error の平文 / 退避 notice（読み方レシピ入り）。
        // #709 が非 JSON を素通しするのと同じ——推測で捨てない（罠4・fail-safe）。
        Err(_) => return result_str.to_string(),
    };

    let reference = match &inner {
        // 単一ツール: `{"success":bool,"data":{…}}`（`tool_result` と同一形）。
        serde_json::Value::Object(_) => {
            // 生産者B のツール結果は `success` の封筒を持つ。その封筒でない任意の JSON
            // オブジェクト（生産者A がたまたま JSON を返した等）は「答え・成果物」であって
            // ツール結果ではない——畳まず残す（決定B・fail-safe）。※封筒が無い場合は下の
            // `signals_failure` も失敗側へ倒すが、意図を明示するためここで先に切る。
            if inner.get("success").is_none() {
                return result_str.to_string();
            }
            let null = serde_json::Value::Null;
            let d = inner.get("data").unwrap_or(&null);
            // 失敗は参照へ潰さず本文を丸ごと残す。判定は #709 と共有（success:false または
            // data.exit_code!=0）。stdout / stderr を選り分けない（罠2）。
            if signals_failure(&inner, d) {
                return result_str.to_string();
            }
            build_subtask_single_reference(d)
        }
        // 複数ツール（batch）: `[{"tool":…,"tool_call_id":…,"result":<value>}, …]`。
        serde_json::Value::Array(items) => {
            // どれか 1 要素でも失敗なら配列を丸ごと残す（罠2）。判定は要素の `result` に
            // 同じ `signals_failure` を掛ける（非オブジェクト要素は失敗側＝本文保持へ倒す）。
            if items.iter().any(batch_entry_signals_failure) {
                return result_str.to_string();
            }
            build_subtask_batch_reference(items.len(), result_str)
        }
        // 想定外の JSON（scalar 等）: 推測で捨てず残す（fail-safe）。
        _ => return result_str.to_string(),
    };

    // 長さ不変条件（#709 と共有）: 参照が本文以上なら本文を残す。
    shorter_of_reference_or_body(reference, result_str)
}

/// 監査の在り処を指す共通の後置き（#713 決定A）。**再取得は約束しない**（罠3）——
/// `result` の read/list 文言（「もう一度 X で読む」）は切り離した subtask には嘘になる。全文が
/// 記録（`memory_sessions` / `session_logs`）に残ることだけ伝える。生 session_id/subtask_id の
/// UUID は会話に出さない（row295b・在り処はヘッダの s 番号が示す）。
fn subtask_audit_suffix() -> String {
    "（本文は会話に残していない・全文は記録に残る）".to_string()
}

/// 単一ツールの subtask 参照。`exit_code` があれば終了コードと出力規模、無ければ結果規模。
/// ここへ来るのは `signals_failure` を通った成功だけなので終了コードは 0。ヘッダが `[s{n} 完了]`
/// を示すので本文は要約だけ（生 UUID を出さない）。
fn build_subtask_single_reference(d: &serde_json::Value) -> String {
    let suffix = subtask_audit_suffix();
    if let Some(code) = d.get("exit_code").and_then(|x| x.as_i64()) {
        // 非ゼロ終了は `signals_failure` が本文ごと残すので、ここへ来るのは成功（code==0）だけ。
        // 双子の `build_result_reference` と対称に tripwire を置き、不変条件が破れたら気づく。
        debug_assert_eq!(
            code, 0,
            "非ゼロ終了は signals_failure が本文ごと残すはず（#709 の不変条件が破れている）"
        );
        // stdout **と** stderr の両方を数える（統括レビュー指摘1）。cargo build の warning など
        // 正常終了でも stderr へ大量に出るツールがあり、stdout だけだと「出力 0 文字」と事実と
        // 違う表示になる。本文は落とすが、会話に残す唯一の数字は正しくする。
        let out = d.get("stdout").and_then(|x| x.as_str()).unwrap_or("");
        let err = d.get("stderr").and_then(|x| x.as_str()).unwrap_or("");
        let stderr_note = if err.is_empty() {
            String::new()
        } else {
            format!("・stderr {} 文字", err.chars().count())
        };
        return format!(
            "終了コード {code}・出力 {} 文字{stderr_note}{suffix}",
            out.chars().count(),
        );
    }
    // exit_code の無いツール結果は data の規模だけ残す（何を呼んだかは記録に残る）。
    let size = serde_json::to_string(d)
        .map(|t| t.chars().count())
        .unwrap_or(0);
    format!("結果 {size} 文字{suffix}")
}

/// batch（複数ツール）の subtask 参照。件数と合計規模（配列 JSON の文字数）を残す。
fn build_subtask_batch_reference(count: usize, result_str: &str) -> String {
    format!(
        "{count} 件のツール結果・合計 {} 文字{}",
        result_str.chars().count(),
        subtask_audit_suffix(),
    )
}

/// batch 要素（`{"tool":…,"tool_call_id":…,"result":<value>}`）が失敗を表すか。
/// 要素の `result` に #709 の `signals_failure` を掛ける。`result` が非オブジェクト
/// （パースできず String で入った・欠落した等）は失敗側へ倒す＝配列全体を本文保持（fail-safe）。
fn batch_entry_signals_failure(entry: &serde_json::Value) -> bool {
    let null = serde_json::Value::Null;
    let result = entry.get("result").unwrap_or(&null);
    let d = result.get("data").unwrap_or(&null);
    signals_failure(result, d)
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
pub fn retain_conversation_logs(
    logs: Vec<opencrab_db::queries::SessionLogRow>,
) -> Vec<opencrab_db::queries::SessionLogRow> {
    logs.into_iter()
        .filter(|l| !is_excluded_from_conversation(l))
        .filter(|l| !is_heartbeat_prompt_scaffolding(l))
        .collect()
}

#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub(crate) fn fit_logs_to_budget(
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

#[allow(dead_code)]
fn format_logs(logs: &[opencrab_db::queries::SessionLogRow]) -> String {
    logs.iter()
        .map(format_single_log)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn format_single_log(log: &opencrab_db::queries::SessionLogRow) -> String {
    format_single_log_with_echo(log, None, None)
}

/// 会話ローカルの短縮参照（§9A）。u=外部話者 / e=受信イベント / c=tool call。全順序ログの
/// 初出順で採番する。写像は決定的（同じログ列 → 同じ番号）なので追加の永続や migration は
/// 不要で、番号は不変（append-only ログの初出順が安定なため）。core の enum/DB/error に個別
/// platform 語彙を足さず、log の汎用 field（speaker_id / external_origin / tool_call id）で採る。
#[derive(Debug, Default, Clone)]
pub struct ConversationRefs {
    agent_id: String,
    /// 自分（agent_id）の表示名（agents.name）。空/未設定なら agent_id のまま。
    agent_name: Option<String>,
    speakers: std::collections::HashMap<String, usize>,
    events: std::collections::HashMap<String, usize>,
    /// event_id（origin 末尾の 64hex）→ e 番号。返信/リアクションの対象解決（row295c 6b）に使う。
    event_ids: std::collections::HashMap<String, usize>,
    calls: std::collections::HashMap<String, usize>,
    /// subtask_id → s 番号（セッション局所・初出順）。spawn 受理と完了本文の両方から採る。
    subtasks: std::collections::HashMap<String, usize>,
}

impl ConversationRefs {
    /// 全順序ログから初出順で採番する。
    pub fn build(logs: &[opencrab_db::queries::SessionLogRow], agent_id: &str) -> Self {
        let mut refs = ConversationRefs {
            agent_id: agent_id.to_string(),
            ..Default::default()
        };
        for log in logs {
            match log.log_type.as_str() {
                "speech" => {
                    if let Some(sp) = log.speaker_id.as_deref() {
                        if sp != agent_id && !refs.speakers.contains_key(sp) {
                            let n = refs.speakers.len() + 1;
                            refs.speakers.insert(sp.to_string(), n);
                        }
                    }
                    if let Some(origin) = external_origin_of(log) {
                        if !refs.events.contains_key(&origin) {
                            let n = refs.events.len() + 1;
                            // event_id（origin 末尾の 64hex）→ e 番号も引けるようにする（row295c 6b
                            // の (reply→e番号) 解決）。origin lane が違っても event_id で照合する。
                            if let Some(eid) = event_id_of_origin(&origin) {
                                refs.event_ids.entry(eid).or_insert(n);
                            }
                            refs.events.insert(origin, n);
                        }
                    }
                }
                "tool_call" => {
                    for id in tool_call_ids_of(log) {
                        refs.assign_call(&id);
                    }
                }
                "tool_result" | "tool_cancelled" => {
                    if let Some(id) = tool_call_id_of_result(log) {
                        refs.assign_call(&id);
                    }
                    // spawn 受理の tool_result 本文 `{"data":{"subtask_id":…}}` から採番（初出）。
                    if let Some(sid) = subtask_id_of_tool_result(log) {
                        refs.assign_subtask(&sid);
                    }
                }
                "system" => {
                    // 完了本文 `{"type":"subtask_completed","subtask_id":…}` からも採番。
                    if let Some(sid) = subtask_id_of_system(log) {
                        refs.assign_subtask(&sid);
                    }
                }
                _ => {}
            }
        }
        refs
    }

    /// 自分の表示名を設定する（組み立て側が agents.name を引いて渡す）。空は無視。
    pub fn set_agent_name(&mut self, name: impl Into<String>) {
        let name = name.into();
        if !name.is_empty() {
            self.agent_name = Some(name);
        }
    }

    fn assign_call(&mut self, id: &str) {
        if !self.calls.contains_key(id) {
            let n = self.calls.len() + 1;
            self.calls.insert(id.to_string(), n);
        }
    }

    fn assign_subtask(&mut self, id: &str) {
        if !id.is_empty() && !self.subtasks.contains_key(id) {
            let n = self.subtasks.len() + 1;
            self.subtasks.insert(id.to_string(), n);
        }
    }

    /// 話者表示。自分は名前だけ（§9A.2・生 UUID を出さない）、外部話者は u 番号。
    fn speaker_label(&self, speaker: &str) -> String {
        if speaker == self.agent_id {
            self.agent_name
                .clone()
                .unwrap_or_else(|| speaker.to_string())
        } else if let Some(n) = self.speakers.get(speaker) {
            format!("u{n}")
        } else {
            speaker.to_string()
        }
    }

    fn event_of(&self, log: &opencrab_db::queries::SessionLogRow) -> Option<usize> {
        external_origin_of(log).and_then(|o| self.events.get(&o).copied())
    }

    fn call_of(&self, id: &str) -> Option<usize> {
        self.calls.get(id).copied()
    }

    /// 短縮参照トークン（`uN` / `eN` / `cN`）を裏の実 ID へ逆引きする（§9A・DI 能力の引数解決）。
    /// `uN`→話者 speaker_id（Nostr では pubkey）、`eN`→受信イベントの external_origin、
    /// `cN`→tool_call id。汎用（platform 非依存）で、未知トークンや未割当番号は None。
    pub fn resolve_short_ref(&self, token: &str) -> Option<String> {
        let token = token.trim();
        let mut chars = token.chars();
        let prefix = chars.next()?;
        let num: usize = chars.as_str().parse().ok()?;
        let map = match prefix {
            'u' => &self.speakers,
            'e' => &self.events,
            'c' => &self.calls,
            _ => return None,
        };
        map.iter()
            .find(|(_, &n)| n == num)
            .map(|(id, _)| id.clone())
    }

    /// subtask_id → s 番号（未知は None）。
    fn subtask_of(&self, id: &str) -> Option<usize> {
        self.subtasks.get(id).copied()
    }

    /// event_id → e 番号（会話内に無い＝未知は None → 表示側は `→外部`）。
    fn event_num_by_id(&self, event_id: &str) -> Option<usize> {
        self.event_ids.get(event_id).copied()
    }
}

/// origin（`…:<lane>:<event_id>`）末尾の 64hex を取り出す。特定 SDK 名に依存しない。
fn event_id_of_origin(origin: &str) -> Option<String> {
    let last = origin.rsplit(':').next()?;
    if last.len() == 64 && last.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(last.to_ascii_lowercase())
    } else {
        None
    }
}

/// 受信メタが記録する対象ノート event_id（`reply_target`・row295c 6b）。旧行は未記録＝None。
fn reply_target_of(log: &opencrab_db::queries::SessionLogRow) -> Option<String> {
    let meta: serde_json::Value = serde_json::from_str(log.metadata_json.as_deref()?).ok()?;
    meta.get("reply_target")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 完了本文（system・type=subtask_completed）の subtask_id。
fn subtask_id_of_system(log: &opencrab_db::queries::SessionLogRow) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&log.content).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("subtask_completed") {
        return None;
    }
    v.get("subtask_id")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// spawn 受理の tool_result 本文の subtask_id（flat `{"subtask_id":…}` / data 包み形の両対応）。
fn subtask_id_of_tool_result(log: &opencrab_db::queries::SessionLogRow) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&log.content).ok()?;
    let scope = v.get("data").unwrap_or(&v);
    scope
        .get("subtask_id")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 受信イベントの external_origin（inbound 記録が metadata に載せる汎用 field）。
fn external_origin_of(log: &opencrab_db::queries::SessionLogRow) -> Option<String> {
    let meta: serde_json::Value = serde_json::from_str(log.metadata_json.as_deref()?).ok()?;
    meta.get("external_origin")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn tool_call_id_of_result(log: &opencrab_db::queries::SessionLogRow) -> Option<String> {
    let meta: serde_json::Value = serde_json::from_str(log.metadata_json.as_deref()?).ok()?;
    meta.get("tool_call_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// spawn 受理 tool_result（`data.status=="spawned"` の subtask_id）。並行バッチは 1 つの subtask を
/// call ごとに重複記録する（同一 subtask_id の spawn 受理が call 数だけ並ぶ）ので、表示では初出だけ
/// 残し 2 件目以降を落とす（row295 item4・二重表示）。組み立て側が seen 集合で判定する。
pub(crate) fn spawn_ack_subtask_id(log: &opencrab_db::queries::SessionLogRow) -> Option<String> {
    if log.log_type != "tool_result" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&log.content).ok()?;
    // spawn 受理は flat 形（`{"status":"spawned","subtask_id":…,"tool":…}`）。data 包み形にも一応対応。
    let scope = v.get("data").unwrap_or(&v);
    if scope.get("status").and_then(|s| s.as_str()) != Some("spawned") {
        return None;
    }
    scope
        .get("subtask_id")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn tool_call_ids_of(log: &opencrab_db::queries::SessionLogRow) -> Vec<String> {
    let Some(meta) = log
        .metadata_json
        .as_deref()
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
    else {
        return Vec::new();
    };
    let Some(tcj) = meta.get("tool_calls_json").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    let Ok(calls) = serde_json::from_str::<serde_json::Value>(tcj) else {
        return Vec::new();
    };
    calls
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|it| it.get("id").and_then(|v| v.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 長文イベントの切り詰め閾値（§9A.5）。タイムライン束ね（watch 車線）は 200 字、
/// 自分宛て（mention/reply 車線）は 2,000 字。origin は汎用 external_id の車線標識で判定する。
/// self 発話（origin なし）は切り詰めない。
fn render_limit(origin: Option<&str>) -> Option<usize> {
    match origin {
        Some(o) if o.contains(":watch:") => Some(200),
        Some(_) => Some(2000),
        None => None,
    }
}

/// `limit` 字を超える本文を切り詰め、末尾に「…(全N字)」を付す（§9A.5）。resolve 案内は
/// 能力実装スライスで追加する。N は元本文の全文字数。
fn truncate_body(content: &str, limit: usize) -> String {
    let total = content.chars().count();
    if total <= limit {
        return content.to_string();
    }
    let head: String = content.chars().take(limit).collect();
    format!("{head}…(全{total}字)")
}

/// 表示時の legacy メタ剥がし（§9A・row294b）。会話組み立て時のみ適用し、保存データは
/// 書き換えない。受信転記の本文へ焼き込まれた種別ラベル行（`[… kind:N …]` 形。新形も旧
/// `from=…/target=…` 付きも）を落とし、本文に残る生の長い識別子（bech32・64hex）を短縮する。
/// 種別ラベルは即時判定（受信側の内部処理）に使うが会話表示には不要（row294b: メンションと
/// リプライは別物・表示にラベル不要）。core は transport を名指ししないので、行判定は汎用マーカー
/// ` kind:<数字>` で行う（外部 origin の車線標識と同じく特定 SDK に依存しない）。
///
/// #826 の会話 snapshot（旧レンダリング済み blob）にも read 時に適用するため crate 公開する
/// （`context_budget::governor::assemble_from_snapshot`）。行単位で処理するので、単一ログ本文にも
/// 複数行の snapshot blob にも同じ規則で効く。
pub(crate) fn strip_inbound_meta_for_display(content: &str) -> String {
    strip_meta_lines(content, elide_raw_identifiers)
}

/// 凍結 snapshot blob 専用の掃除（row295d）。単一ログ経路（[`strip_inbound_meta_for_display`]）に
/// 加えて、旧レンダリング由来の legacy 識別子—UUID（subtask/session）・`call_…`（tool call id）・
/// `"digest":"…"`（モデル不要な内部整合値）—も除去/短縮する。新形式（既に §9A・c/s 番号・→log:N）
/// には該当パターンが無いので無影響。単一ログの note 本文へは適用しない（利用者本文の過剰除去を避ける）。
pub(crate) fn strip_frozen_snapshot(content: &str) -> String {
    strip_meta_lines(content, |line| {
        elide_raw_identifiers(&clean_legacy_ids(line))
    })
}

/// **検知器**（row318）: 新規 delta 描画行に生の長識別子（UUID / `call_…` / bech32 / 32hex 以上 /
/// `"digest":"…"`）が残っていないかを見る。残っていれば「描画器が短縮形を出し損ねた＝バグ」なので
/// その行を返す（呼び出し側が WARN する・fail-loud）。スクラブ（[`strip_frozen_snapshot`]）は凍結
/// snapshot blob 専用で、delta 行はここで**置換せず検知だけ**する（本番に `<uuid…>` の無意味な
/// プレースホルダを出さない）。正しく描画できていれば常に `None`。
pub(crate) fn leaked_identifier_in_delta(rendered: &str) -> Option<String> {
    for line in rendered.split('\n') {
        if elide_raw_identifiers(&clean_legacy_ids(line)) != line {
            return Some(line.to_string());
        }
    }
    None
}

/// メタ行を落とし、残る各行に `per_line` を適用し、末尾空行を畳む共通ルーチン。
fn strip_meta_lines(content: &str, per_line: impl Fn(&str) -> String) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in content.split('\n') {
        // メタ行（`[… kind:1 …]`）を落とす（新形 / 旧 from=/target= 付きの両方）。
        if is_inbound_meta_line(line.trim()) {
            continue;
        }
        lines.push(per_line(line));
    }
    // メタ行を落とした跡の末尾空行を畳む。
    while lines.last().is_some_and(|s| s.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// snapshot blob の legacy 識別子（UUID / `call_…` / `"digest":"…"`）を除去/短縮する（row295d）。
fn clean_legacy_ids(line: &str) -> String {
    let line = elide_uuids(line);
    let line = elide_call_ids(&line);
    elide_digest_values(&line)
}

/// UUID（`8-4-4-4-12` hex）を `<uuid…>` へ。64hex（ダッシュ無し）はここでは当たらず elide_raw が担う。
fn elide_uuids(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(len) = uuid_len_at(&bytes[i..]) {
            out.push_str("<uuid…>");
            i += len;
        } else {
            let ch = line[i..].chars().next().expect("char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// 位置 0 から UUID（`8-4-4-4-12` hex）なら消費バイト長（36）を返す。前後境界も見る。
fn uuid_len_at(b: &[u8]) -> Option<usize> {
    let groups = [8usize, 4, 4, 4, 12];
    let mut pos = 0;
    for (gi, &g) in groups.iter().enumerate() {
        if gi > 0 {
            if b.get(pos) != Some(&b'-') {
                return None;
            }
            pos += 1;
        }
        for _ in 0..g {
            match b.get(pos) {
                Some(c) if c.is_ascii_hexdigit() => pos += 1,
                _ => return None,
            }
        }
    }
    // 直後が英数なら、より長い hex トークンの一部＝UUID ではない。ダッシュは許す
    // （`nostr-<uuid>-<channel>` のような dashed session id 内に埋まった UUID も剥がす・row295d 変種）。
    if matches!(b.get(pos), Some(c) if c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(pos)
}

/// `call_<英数16+>`（tool call id）を `<call…>` へ短縮。
fn elide_call_ids(line: &str) -> String {
    const NEEDLE: &str = "call_";
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(pos) = rest.find(NEEDLE) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + NEEDLE.len()..];
        let n = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .count();
        if n >= 16 {
            out.push_str("<call…>");
            rest = &after[n..];
        } else {
            out.push_str(NEEDLE);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// `"digest":"<hex>"` の値を `…` へ（モデル不要な内部整合値・row295d）。
fn elide_digest_values(line: &str) -> String {
    const NEEDLE: &str = "\"digest\":\"";
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(pos) = rest.find(NEEDLE) {
        out.push_str(&rest[..pos + NEEDLE.len()]);
        let after = &rest[pos + NEEDLE.len()..];
        let end = after.find('"').unwrap_or(after.len());
        let val = &after[..end];
        if !val.is_empty() && val.bytes().all(|b| b.is_ascii_hexdigit()) {
            out.push('…');
        } else {
            out.push_str(val);
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// `[… kind:<数字> …]` 形の受信メタ行か（transport 非依存の汎用判定）。
fn is_inbound_meta_line(trimmed: &str) -> bool {
    const MARKER: &str = " kind:";
    if !(trimmed.starts_with('[') && trimmed.ends_with(']')) {
        return false;
    }
    match trimmed.find(MARKER) {
        Some(idx) => trimmed[idx + MARKER.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// 受信メタ行 `[… kind:N ラベル …]` から会話ヘッダの関係注記を作る（row295c）。
///
/// kind ラベル全廃で「そもそもリアクション/リプライか」が失われた欠陥への対処。リプライ/
/// リアクション/リポストだけ種別を残す（素の投稿・メンション・DM・長文は注記なし）。対象ノートは
/// 受信メタ `reply_target`（row295c 6b で記録）→ 会話内 e 番号へ解決。会話に無い（旧行の未記録・
/// 窓外）対象は `→外部`。
fn inbound_relation_annotation(
    log: &opencrab_db::queries::SessionLogRow,
    refs: &ConversationRefs,
) -> Option<String> {
    let label = log
        .content
        .split('\n')
        .map(str::trim)
        .find(|l| is_inbound_meta_line(l))
        .and_then(meta_line_label)?;
    let relation = match label {
        "リプライ" => "reply",
        "リアクション" => "reaction",
        "リポスト" => "repost",
        _ => return None,
    };
    // 対象ノートは受信メタの reply_target（row295c 6b）→ 会話内 e 番号。会話に無い（旧行の
    // 未記録・窓外）対象は `→外部`。
    let target = reply_target_of(log)
        .and_then(|t| refs.event_num_by_id(&t))
        .map(|n| format!("e{n}"))
        .unwrap_or_else(|| "外部".to_string());
    Some(format!("({relation}→{target})"))
}

/// `[… kind:<数字> <ラベル> …]` からラベル語だけを取り出す（新形も旧 from=/target= 付きも）。
fn meta_line_label(trimmed: &str) -> Option<&str> {
    const MARKER: &str = " kind:";
    let idx = trimmed.find(MARKER)?;
    let after = trimmed[idx + MARKER.len()..].trim_start_matches(|c: char| c.is_ascii_digit());
    let after = after.strip_prefix(' ')?;
    let end = after.find([' ', ']']).unwrap_or(after.len());
    Some(&after[..end])
}

/// 生の長い識別子の bech32 HRP。長い順に試す（`nprofile1` が `npub1` より先）。
const BECH32_HRPS: &[&str] = &["nprofile1", "nevent1", "naddr1", "npub1", "note1", "nsec1"];

/// 行内の生の長い識別子（bech32・64hex）を短縮する。英数の連続を 1 トークンとして境界で切り、
/// 通常語や短い hash は温存する。
fn elide_raw_identifiers(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut token = String::new();
    for ch in line.chars() {
        if ch.is_ascii_alphanumeric() {
            token.push(ch);
        } else {
            if !token.is_empty() {
                out.push_str(&elide_identifier_token(&token));
                token.clear();
            }
            out.push(ch);
        }
    }
    if !token.is_empty() {
        out.push_str(&elide_identifier_token(&token));
    }
    out
}

fn elide_identifier_token(tok: &str) -> String {
    for hrp in BECH32_HRPS {
        if let Some(body) = tok.strip_prefix(hrp) {
            if body.len() >= 30
                && body
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            {
                return format!("<{}…>", hrp.trim_end_matches('1'));
            }
        }
    }
    // 生 hex 識別子（pubkey/event_id=64hex・ダッシュ無し UUID/subtask=32hex 等）。短い hash や
    // git short-sha を巻き込まないよう 32 桁以上に限る（row318: 32hex 変種も長物ゼロの対象）。
    if tok.len() >= 32
        && tok
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    {
        return "<id…>".to_string();
    }
    tok.to_string()
}

/// 完了済み tool_call の arguments を `{ref,digest,bytes}` に置換して読む。
/// 未決着 call は `completed_ids` に無いので全文のまま。`refs` があれば §9A の短縮参照
/// （u/e/c 番号・識別子排除・長文切り詰め）を適用する。None なら従来の生表示（単体整形・
/// live inbound 注入・テスト）。
pub fn format_single_log_with_echo(
    log: &opencrab_db::queries::SessionLogRow,
    completed_ids: Option<&std::collections::HashSet<String>>,
    refs: Option<&ConversationRefs>,
) -> String {
    let ts = log
        .created_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        // 括弧間スペースを出さない（`[u5][時刻]e3575:` 形・行数×毎ターンで効く・row295b）。
        .map(|dt| dt.format("[%Y-%m-%d %H:%M:%S]").to_string())
        .unwrap_or_default();

    match log.log_type.as_str() {
        "speech" => match refs {
            Some(r) => {
                let speaker = r.speaker_label(log.speaker_id.as_deref().unwrap_or(&log.agent_id));
                let eref = r.event_of(log).map(|n| format!("e{n}")).unwrap_or_default();
                // 関係注記（row295c）: リプライ/リアクション/リポストは種別を残す。ラベル全廃で
                // 「そもそもリアクションか」が失われた欠陥への対処。対象ノート(→e番号)は現状の
                // 受信転記に記録が無いため `→外部` の最小表記（真の →e番号 は target 記録の
                // データスライス後・報告参照）。素の投稿/メンションは注記なし。
                let relation = inbound_relation_annotation(log, r).unwrap_or_default();
                // 表示時に legacy メタ行・生識別子を剥がしてから切り詰める（row294b・保存は不変）。
                let cleaned = strip_inbound_meta_for_display(&log.content);
                let content = match render_limit(external_origin_of(log).as_deref()) {
                    Some(lim) => truncate_body(&cleaned, lim),
                    None => cleaned,
                };
                format!("[{speaker}]{ts}{eref}{relation}:\n{content}")
            }
            None => {
                let speaker = log.speaker_id.as_deref().unwrap_or(&log.agent_id);
                format!("[{}]{}:\n{}", speaker, ts, log.content)
            }
        },
        "tool_call" => {
            // §9A.2 / row318: 自分の話者行も **描画時に** 名前（くらぶ）へ。生 speaker_id（agent UUID）を
            // 文字列に入れる瞬間を作らない（後段スクラブで <uuid…> にしない）。refs 無し（単体表示）は
            // 従来どおり生 speaker（テスト・live 注入）。
            let raw_speaker = log.speaker_id.as_deref().unwrap_or(&log.agent_id);
            let speaker = match refs {
                Some(r) => r.speaker_label(raw_speaker),
                None => raw_speaker.to_string(),
            };
            if let Some(meta_json) = log.metadata_json.as_deref() {
                if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_json) {
                    // §9A.1 / row292: DI operation の call は arguments を verbatim 保持する
                    // （reply 本文が次ターンで消えない）。log 参照への短縮（→log:N）から除外する。
                    let preserve: std::collections::HashSet<&str> = meta
                        .get("preserve_arg_call_ids")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                        .unwrap_or_default();
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
                                        // 完了済み call の引数は本文を持ち越さない。以前は
                                        // {ref,digest,bytes} を出していたが digest はモデルに不要な
                                        // 内部整合値なので出さず、log 参照だけ短く残す（row295b・#707）。
                                        // ただし DI operation の call（preserve_arg_call_ids）は reply
                                        // 本文が次ターンで消えないよう verbatim 保持し短縮しない（§9A.1/row292）。
                                        let args = if completed_ids
                                            .is_some_and(|set| set.contains(id))
                                            && !preserve.contains(id)
                                        {
                                            format!("→log:{}", log.id.unwrap_or(0))
                                        } else {
                                            args
                                        };
                                        // §9A: call_id を c 番号へ短縮（call_ 生 ID を排除）。
                                        let call_ref = refs
                                            .and_then(|r| r.call_of(id))
                                            .map(|n| format!("c{n}"))
                                            .unwrap_or_else(|| format!("id={id}"));
                                        Some(format!("[{}]: {}({})", call_ref, name, args))
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
            let call_ref = refs
                .and_then(|r| r.call_of(tool_call_id))
                .map(|n| format!("c{n}"))
                .unwrap_or_else(|| format!("id={tool_call_id}"));
            // spawn 受理（`status=="spawned"`）は subtask_id を **描画時に** s 番号へ。生 UUID を
            // 結果本文へ載せない（row295b/row318・result_reference は spawn 受理を success 封筒無しと
            // 見て本文丸ごと返すため、ここで先に短縮形へ分岐する）。refs 無しは "subtask"。
            if let Some(sid) = spawn_ack_subtask_id(log) {
                let sref = refs
                    .and_then(|r| r.subtask_of(&sid))
                    .map(|n| format!("s{n}"))
                    .unwrap_or_else(|| "subtask".to_string());
                return format!(
                    "[tool_result]{ts}:\n[{call_ref}]: {tool_name} → subtask {sref} を起動（本文は会話に残していない）"
                );
            }
            format!(
                "[tool_result]{}:\n[{}]: {} → {}",
                ts,
                call_ref,
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
            let call_ref = refs
                .and_then(|r| r.call_of(tool_call_id))
                .map(|n| format!("c{n}"))
                .unwrap_or_else(|| format!("id={tool_call_id}"));
            format!(
                "[tool_cancelled]{}:\n[{}]: {} がキャンセルされた\n{}",
                ts, call_ref, tool_name, log.content
            )
        }
        "system" => {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&log.content) {
                if let Some(kind) = value.get("type").and_then(|v| v.as_str()) {
                    // #713: `subtask_completed` **だけ**、入れ子 `result`（ツール実行の本文）を
                    // 会話へ持ち越さず参照へ畳む。他の system type は現状どおり丸ごと
                    // pretty-print（範囲外・塊の証拠なし）。**厳密一致**で 1 type だけ分岐し、
                    // 他の type を巻き込まない（設計 Q2 #8）。
                    if kind == "subtask_completed" {
                        return format_subtask_completed(&value, &log.content, &ts, refs);
                    }
                    let content = serde_json::to_string_pretty(&value)
                        .unwrap_or_else(|_| log.content.clone());
                    return format!("[system: {}]{}:\n{}", kind, ts, content);
                }
            }
            format!("[system]{}:\n{}", ts, log.content)
        }
        // catch-all: 未知の log_type は `content` を**丸ごと**運ぶ（設計 Q2 #9 の構造的盲点）。
        // 将来 log_type を足した人が本文の持ち越しに気づけるよう
        // `unknown_log_types_carry_full_body_through_catch_all` で固定する。
        other => format!("[{}]{}:\n{}", other, ts, log.content),
    }
}

/// `subtask_completed` の完了本文を会話行へ整形する（#713）。入れ子 `result`（ツール実行の本文）を
/// [`fold_subtask_completed`] で参照へ畳んでから、外側の封筒（`subtask_id` / `session_id` /
/// `exit_reason`）はそのまま pretty-print する——監査の相関（起動応答との突き合わせ・記録の在り処）を
/// 会話から消さない。畳めない形（失敗・散文・退避 notice 等）では `result` は原文のまま残るので、
/// 表示は従来の pretty-print と一致する（挙動を変えるのは畳めたときだけ）。
fn format_subtask_completed(
    value: &serde_json::Value,
    raw_content: &str,
    ts: &str,
    refs: Option<&ConversationRefs>,
) -> String {
    // `result` は文字列（`settle_completed` が `result_text` を JSON 文字列として載せる）。
    // 想定外に文字列でなければ触らず pretty-print に委ねる（fail-safe・稀）。
    let Some(result_str) = value.get("result").and_then(|v| v.as_str()) else {
        let pretty =
            serde_json::to_string_pretty(value).unwrap_or_else(|_| raw_content.to_string());
        return format!("[subtask 完了]{ts}:\n{pretty}");
    };

    let exit_reason = value
        .get("exit_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let subtask_id = value
        .get("subtask_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // subtask_id はセッション局所 s 番号へ（生 UUID を出さない・row295b）。refs 無し（単体表示）は
    // 採番できないので "subtask"。session_id/exit_reason 等の定型 field は会話に出さない。
    let label = refs
        .and_then(|r| r.subtask_of(subtask_id))
        .map(|n| format!("s{n}"))
        .unwrap_or_else(|| "subtask".to_string());

    // 本文は畳んだ result だけ（ツール結果 blob は要約・散文はそのまま・切り詰めは fold 内の不変条件）。
    let body = fold_subtask_completed(exit_reason, result_str);
    format!("[{label} 完了]{ts}:\n{body}")
}

#[cfg(test)]
mod format_log_tests {
    use super::{format_single_log, format_single_log_with_echo};
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

    #[test]
    fn completed_tool_call_arguments_become_ref_digest_bytes() {
        let tcj = serde_json::json!([{
            "id": "tc-1",
            "type": "function",
            "function": { "name": "search", "arguments": "{\"q\":\"rust\"}" }
        }])
        .to_string();
        let mut log = tool_call_log(&tcj);
        log.id = Some(42);
        let mut done = std::collections::HashSet::new();
        done.insert("tc-1".into());
        let out = format_single_log_with_echo(&log, Some(&done), None);
        assert!(out.contains("search"), "{out}");
        // 完了済み call は log 参照だけ（digest/bytes はモデルに不要なので出さない・row295b）。
        assert!(out.contains("→log:42"), "{out}");
        assert!(!out.contains("digest"), "digest は出さない: {out}");
        assert!(!out.contains("bytes"), "bytes は出さない: {out}");
        assert!(
            !out.contains(r#"{"q":"rust"}"#),
            "完了済み arguments は全文を残さない: {out}"
        );
        let unresolved =
            format_single_log_with_echo(&log, Some(&std::collections::HashSet::new()), None);
        assert!(
            unresolved.contains(r#"{"q":"rust"}"#),
            "未決着 call は全文: {unresolved}"
        );
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
    use super::{build_conversation_string, build_conversation_string_with_memory_index};
    use crate::context_budget::{
        apply_line_items, compute_water_levels, ContextBudgetPolicy, MeasuredLineItems,
        MemoryIndexDecision, MemoryIndexOmitReason,
    };
    use crate::tokens::estimate_tokens;

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
        // 残予算に収まらなければ丸ごと省略（部分切り詰めなし）。判定は apply_line_items。
        let conn = opencrab_db::init_memory().unwrap();
        seed_index(&conn);
        seed_logs(&conn, 3);
        let section = crate::memory_index::build_memory_index_section(&conn, "a1", "cur-sess")
            .unwrap()
            .expect("index section");
        let cost = estimate_tokens(&section);
        let policy = ContextBudgetPolicy {
            absolute_cap_a: 100,
            memory_index_token_cap: 4_000,
            ..ContextBudgetPolicy::default()
        };
        let water = compute_water_levels(10_000, 50, &policy).unwrap();
        // input_high=100, mandatory=80, remaining=20。MI は残予算を超えて省略。
        assert!(
            cost > 20,
            "fixture MI should exceed remaining 20, got {cost}"
        );
        let env = apply_line_items(
            water,
            MeasuredLineItems {
                system: 10,
                runtime_context: 10,
                functions: 10,
                memory_index: cost,
                memory_index_entry_count: 3,
                conversation: 0,
            },
            &policy,
        )
        .unwrap();
        assert_eq!(
            env.memory_index_decision,
            MemoryIndexDecision::Omit {
                reason: MemoryIndexOmitReason::ExceedsRemainingBudget
            }
        );
        let include = matches!(env.memory_index_decision, MemoryIndexDecision::Inject);
        // MI 省略が主題。会話車線の高水位は envelope の 20 tok ではなく十分確保する。
        let out =
            build_conversation_string_with_memory_index(&conn, "cur-sess", "a1", 100_000, include)
                .unwrap();
        assert!(!out.contains("[Memory Index]"));
        assert!(!out.contains("5月は逆引き辞書を設計した。"));
    }

    #[test]
    fn dedicated_cap_omits_memory_index_entirely() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_index(&conn);
        seed_logs(&conn, 3);
        let section = crate::memory_index::build_memory_index_section(&conn, "a1", "cur-sess")
            .unwrap()
            .expect("index section");
        let cost = estimate_tokens(&section);
        let policy = ContextBudgetPolicy {
            memory_index_token_cap: 1,
            ..ContextBudgetPolicy::default()
        };
        let water = compute_water_levels(200_000, 4_096, &policy).unwrap();
        let env = apply_line_items(
            water,
            MeasuredLineItems {
                system: 10,
                runtime_context: 10,
                functions: 10,
                memory_index: cost,
                memory_index_entry_count: 3,
                conversation: 0,
            },
            &policy,
        )
        .unwrap();
        assert_eq!(
            env.memory_index_decision,
            MemoryIndexDecision::Omit {
                reason: MemoryIndexOmitReason::ExceedsDedicatedCap
            }
        );
        let include = matches!(env.memory_index_decision, MemoryIndexDecision::Inject);
        let out = build_conversation_string_with_memory_index(
            &conn,
            "cur-sess",
            "a1",
            env.conversation_high,
            include,
        )
        .unwrap();
        assert!(!out.contains("[Memory Index]"));
        assert!(!out.contains("5月は逆引き辞書を設計した。"));
        assert!(out.contains("メッセージ 2 の内容"));
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
        // Memory Index は専用 cap 判定（apply_line_items）済みとして通す。
        // 826-B で現行セッション topic の [Past context summary] は廃止。
        let out = build_conversation_string(&conn, "cur-sess", "a1", 900).unwrap();
        assert_eq!(out.matches("[Memory Index]").count(), 1);
        assert!(!out.contains("[Past context summary"));
        // 他セッション topic は Memory Index 側のみ。現セッション topic はどちらにも出ない。
        assert!(!out.contains("[t-cur]"));
        assert_eq!(out.matches("[t-other]").count(), 1);
        let mi_pos = out.find("[Memory Index]").unwrap();
        let tother_pos = out.find("[t-other]").unwrap();
        assert!(tother_pos > mi_pos);
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
        build_conversation_string, build_past_context_summary_section, estimate_tokens,
        PAST_SUMMARY_BUDGET_DEN, PAST_SUMMARY_BUDGET_NUM,
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

    fn topic_rows(conn: &rusqlite::Connection) -> Vec<opencrab_db::queries::IndexNodeRow> {
        opencrab_db::queries::get_topic_nodes_for_session(conn, AGENT, SESSION).unwrap()
    }

    fn built_summary(conn: &rusqlite::Connection, budget: usize) -> String {
        build_past_context_summary_section(&topic_rows(conn), budget)
    }

    /// topic が数千件あっても、セクションは予算の 30% を超えない。
    ///
    /// 826-B で本番組立からは外したので、ヘルパーを直接叩く。
    #[test]
    fn past_summary_stays_within_thirty_percent_of_the_budget() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_topics(&conn, 2_000);

        const BUDGET: usize = 4_000;
        let cap = BUDGET / PAST_SUMMARY_BUDGET_DEN * PAST_SUMMARY_BUDGET_NUM;
        let section = built_summary(&conn, cap);
        let used = estimate_tokens(&section);
        assert!(
            used <= cap,
            "[Past context summary] が予算の 30% ({cap}) を超えた: {used} トークン"
        );
        assert!(
            !section.contains("TOPIC-000"),
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
        seed_topics(&conn, 100);

        let section = built_summary(&conn, 1_200);
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
        seed_topics(&conn, 100);

        let section = built_summary(&conn, 1_200);
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
        seed_topics(&conn, 3);

        let section = built_summary(&conn, 4_000);

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
        assert!(
            out.contains("[old_history_summary]"),
            "二水位圧縮の印が無い: {out}"
        );
        assert!(out.contains("log line 399"), "直近ログが落ちている: {out}");
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
            match build_conversation_string(&conn, SESSION, AGENT, budget) {
                Ok(out) => {
                    assert!(!out.is_empty(), "budget={budget} で空文字になった");
                }
                Err(e) => {
                    let msg = format!("{e}");
                    assert!(
                        msg.contains(crate::context_budget::CONTEXT_BUDGET_EXHAUSTED),
                        "budget={budget} は exhausted 以外: {msg}"
                    );
                }
            }
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
        build_conversation_string, build_recent_window, estimate_tokens, retain_conversation_logs,
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

        assert!(
            out.contains("[old_history_summary]"),
            "二水位圧縮の印が無い: {out}"
        );
        assert!(
            out.contains(&format!("recent log line {}", N - 1)),
            "最新行が載っていない: {out}"
        );
        assert!(
            estimate_tokens(&out) <= BUDGET,
            "出力が予算 {BUDGET} を超えた: {} トークン",
            estimate_tokens(&out)
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
        insert_raw(&conn, "speech", Some("owner"), "この指示は消えてはいけない");
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
        assert!(
            !out.contains("[Past context summary"),
            "廃止した topic 要約が出ている: {out}"
        );
        assert!(
            out.contains("[old_history_summary]"),
            "コンパクションが起きていない（マーカー無し）: {out}"
        );
        assert!(
            out.contains(&format!("recent log line {}", N - 1)),
            "最新行が載っていない: {out}"
        );
        assert!(
            estimate_tokens(&out) <= BUDGET,
            "出力が予算 {BUDGET} を超えた: {} トークン",
            estimate_tokens(&out)
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
        assert!(
            !out.contains("[Past context summary"),
            "廃止した topic 要約が出ている: {out}"
        );
        assert!(
            out.contains("[old_history_summary]"),
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
                out.contains("[old_history_summary]"),
                "budget={budget} でコンパクションが起きていない: {out}"
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
    const USER: &str = "owner";
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
            "owner",
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
        insert(&conn, "speech", Some("owner"), "新着あった？");
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
    /// （memory_sessions）には完全な本文が残るので失われない。エージェントBの tool_result 30 万文字の
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

    /// #709 レビュー指摘1: 小さな mutation 結果を参照化しても、書いた対象（path）を会話から
    /// 消さない。`ws_write` の `{"path":"...","written":true}` が「結果 N 文字」に化けると
    /// **どのファイルを書いたのかが会話から消える**——削減効果ゼロなのに作業記憶を削っていた。
    #[test]
    fn mutation_results_keep_their_path() {
        let result = serde_json::json!({
            "success": true,
            "data": {"path": "crates/core/src/lib.rs", "written": true}
        })
        .to_string();

        let r = result_reference("ws_write", &result);
        assert!(
            r.contains("crates/core/src/lib.rs"),
            "書いたファイルが会話から消えた: {r}"
        );
        assert!(!r.contains("\"written\""), "本文がそのまま載っている: {r}");
    }

    /// #709 レビュー指摘1: 参照が本文より長くなるなら潰さず本文を残す（会話を軽くする仕組みが
    /// 会話を重くしない）。極小の結果は参照化の固定オーバーヘッドの方が長くなる。
    #[test]
    fn tiny_results_are_never_expanded() {
        // 参照文（path + tool_name + 定型句）の方が本文より長くなる極小ケース。
        let result = serde_json::json!({
            "success": true, "data": {"path": "x"}
        })
        .to_string();

        let r = result_reference("configure_self", &result);
        assert!(
            r.chars().count() <= result.chars().count(),
            "参照が本文より長い（会話を重くしている）: ref={} body={} / {r}",
            r.chars().count(),
            result.chars().count()
        );
    }

    /// #709 レビュー指摘2: 失敗は必ず本文ごと残る——catch-all の「結果 N 文字」へ潰れて黙って
    /// 消えることはない。この系の不変条件（失敗は `success:false` **または** `exit_code!=0`）を
    /// `signals_failure` に集約したので、どちらの経路を落としてもこのテストが落ちる。
    #[test]
    fn failures_are_never_summarized_as_success() {
        // (a) ツール層の失敗: success:false。
        let tool_fail = serde_json::json!({
            "success": false, "data": {"foo": "bar"}, "error": "boom"
        })
        .to_string();
        assert_eq!(
            result_reference("some_tool", &tool_fail),
            tool_fail,
            "success:false が要約されて消えた"
        );

        // (b) コマンドの非ゼロ終了: execute_shell は success:true のまま返す。
        let cmd_fail = serde_json::json!({
            "success": true, "data": {"exit_code": 2, "stdout": "", "stderr": "boom"}
        })
        .to_string();
        assert_eq!(
            result_reference("execute_shell", &cmd_fail),
            cmd_fail,
            "非ゼロ終了が要約されて消えた"
        );
    }
}

/// #713: `subtask_completed` の入れ子 `result`（ツール実行の本文）を会話へ持ち越さない。
///
/// opencrab は「ツールを常に切り離す」ため、実運用ではツール結果の主経路がここ（`tool_result`
/// ではなく完了本文の入れ子 `result`）。全テストは公開入口 `format_single_log` を通して実際の
/// `system` アーム分岐を叩く（変異検知の best altitude）。
#[cfg(test)]
mod subtask_completed_folding_tests {
    use super::format_single_log;
    use opencrab_db::queries::SessionLogRow;

    /// `settle_completed` が書く完了本文と同一形の system ログを作る。`result` は**文字列**
    /// （`result_text` を JSON 文字列として載せる）。`speaker_id=None`（scaffolding と区別）。
    fn subtask_completed_log(exit_reason: &str, result_str: &str) -> SessionLogRow {
        let content = serde_json::json!({
            "type": "subtask_completed",
            "subtask_id": "st-1",
            "session_id": "subtask-st-1",
            "exit_reason": exit_reason,
            "result": result_str,
        })
        .to_string();
        SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "parent".to_string(),
            log_type: "system".to_string(),
            content,
            speaker_id: None,
            turn_number: None,
            metadata_json: None,
            created_at: None,
        }
    }

    /// 1. 単一ツール成功（execute_shell 大出力）→ 参照化され stdout 本文が会話に載らない。
    ///    監査の相関（起動応答との突き合わせ）を残すため封筒（subtask_id / exit_reason）と
    ///    記録の在り処（session=）も残る。
    #[test]
    fn single_tool_success_leaves_only_a_reference() {
        let out = "x".repeat(50_000);
        let result = serde_json::json!({
            "success": true, "data": {"exit_code": 0, "stdout": out, "stderr": ""}
        })
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &result));
        assert!(
            !o.contains(&"x".repeat(100)),
            "stdout 本文が会話へ載っている（#709 の状態）: {o:.120}"
        );
        assert!(o.contains("終了コード 0"), "終了コードが無い: {o}");
        assert!(o.contains("50000"), "出力規模が無い: {o}");
        // row295b: 生 UUID（subtask_id/session）は会話に出さない。在り処はヘッダの s 番号
        // （refs 無しの単体表示では "subtask"）が示す。
        assert!(o.contains("[subtask 完了]"), "完了ヘッダが無い: {o}");
        assert!(!o.contains("st-1"), "生 subtask_id が残存: {o}");
        assert!(!o.contains("session="), "生 session UUID が残存: {o}");
    }

    /// 1b. 正常終了でも stderr に大量に出るツール（cargo build の warning 等）の規模を数える
    ///     （#716 レビュー指摘1）。stdout が空でも「出力 0 文字」で終わらず stderr の規模を出す。
    #[test]
    fn single_tool_reference_counts_stderr_size() {
        let err = "warning: unused variable `x`\n".repeat(2_000);
        let result = serde_json::json!({
            "success": true,
            "data": {"exit_code": 0, "stdout": "", "stderr": err}
        })
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &result));
        assert!(
            !o.contains("warning: unused variable"),
            "stderr 本文が会話へ載っている: {o:.120}"
        );
        assert!(o.contains("出力 0 文字"), "stdout 規模が無い: {o}");
        assert!(
            o.contains(&format!("stderr {} 文字", err.chars().count())),
            "stderr の規模が数えられていない（stdout だけ数えて事実と違う表示）: {o}"
        );
    }

    /// 2. 単一ツール失敗（execute_shell は success:true のまま exit_code!=0）→ 本文を丸ごと残す。
    ///    stderr / stdout を選り分けず両方残す（罠2）。
    #[test]
    fn single_tool_failure_keeps_the_whole_body() {
        let result = serde_json::json!({
            "success": true,
            "data": {
                "exit_code": 1,
                "stdout": "panicked at src/lib.rs:88",
                "stderr": "error[E0308]: mismatched types"
            }
        })
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &result));
        assert!(o.contains("E0308"), "stderr の失敗理由が消えた: {o}");
        assert!(
            o.contains("src/lib.rs:88"),
            "stdout の失敗詳細が消えた: {o}"
        );
    }

    /// 3. `exit_reason=="completed"` でも内側 `exit_code!=0` なら本文保持（外側 completed に騙されない・罠1）。
    #[test]
    fn completed_outer_does_not_mask_inner_nonzero_exit() {
        let stdout = "assertion failed: used <= budget ".repeat(2_000);
        let result = serde_json::json!({
            "success": true,
            "data": {"exit_code": 101, "stdout": stdout, "stderr": "test failed"}
        })
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &result));
        assert!(
            o.contains("assertion failed: used <= budget"),
            "外側 completed で内側の失敗詳細が消えた: {o:.120}"
        );
    }

    /// 4. `exit_reason ∈ {timeout,error,stopped_by_limit}` → 畳めるはずの本文でも丸ごと残す（罠1）。
    #[test]
    fn non_completed_exit_reasons_keep_the_body() {
        let out = "x".repeat(50_000);
        // completed なら畳まれる成功ツール結果。exit_reason だけで保持へ倒れることを見る。
        let result = serde_json::json!({
            "success": true, "data": {"exit_code": 0, "stdout": out, "stderr": ""}
        })
        .to_string();

        for er in ["timeout", "error", "stopped_by_limit"] {
            let o = format_single_log(&subtask_completed_log(er, &result));
            assert!(
                o.contains(&"x".repeat(100)),
                "exit_reason={er} で本文が畳まれた（completed 以外は保持のはず）: {o:.120}"
            );
        }
    }

    /// 5. 生産者A（サブエージェント最終応答・非 JSON の散文）→ そのまま残る（要約で消さない・決定B）。
    #[test]
    fn producer_a_prose_is_left_intact() {
        let prose = "サブエージェントが出した最終応答の本文。".repeat(50);
        let o = format_single_log(&subtask_completed_log("completed", &prose));
        assert!(
            o.contains("サブエージェントが出した最終応答の本文。"),
            "散文の答えが消えた（決定B に反する）: {o:.120}"
        );
        assert!(
            !o.contains("本文は会話に残していない"),
            "散文を参照へ潰した: {o:.120}"
        );
    }

    /// 5b. 生産者A がたまたま JSON オブジェクトを返しても（`success` 封筒が無い）畳まない（決定B・fail-safe）。
    #[test]
    fn producer_a_bare_json_object_is_not_folded() {
        let answer = serde_json::json!({
            "answer": "これはサブエージェントの答え".repeat(100), "confidence": "high"
        })
        .to_string();
        let o = format_single_log(&subtask_completed_log("completed", &answer));
        assert!(
            o.contains("これはサブエージェントの答え"),
            "ツール封筒でない JSON を畳んで答えを消した: {o:.120}"
        );
    }

    /// 6. 退避 notice（非 JSON・読み方レシピ入り）→ そのまま素通し（レシピが壊れない・罠4）。
    #[test]
    fn offload_notice_passes_through_untouched() {
        let notice =
            "結果が大きいため退避しました: workspace/offload/abc.txt（全 1234 行）。読み方: ws_read で行範囲を指定して読む。";
        let o = format_single_log(&subtask_completed_log("completed", notice));
        assert!(
            o.contains("workspace/offload/abc.txt") && o.contains("ws_read で行範囲"),
            "退避 notice の読み方レシピが壊れた: {o}"
        );
    }

    /// 7. batch 配列で 1 要素でも失敗 → 配列全体を保持（罠2）。
    #[test]
    fn batch_with_one_failure_keeps_the_whole_array() {
        let arr = serde_json::json!([
            {"tool":"execute_shell","tool_call_id":"c1",
             "result":{"success":true,"data":{"exit_code":0,"stdout":"ok"}}},
            {"tool":"ws_read","tool_call_id":"c2",
             "result":{"success":false,"data":null,"error":"path not found: docs/missing.md"}}
        ])
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &arr));
        assert!(
            o.contains("path not found: docs/missing.md"),
            "batch の失敗要素が消えた: {o:.160}"
        );
    }

    /// 8. batch 全成功 → 「N 件のツール結果・合計 M 文字」参照。個々の stdout 本文は載らない。
    #[test]
    fn batch_all_success_becomes_a_count_reference() {
        let big = "y".repeat(20_000);
        let arr = serde_json::json!([
            {"tool":"execute_shell","tool_call_id":"c1",
             "result":{"success":true,"data":{"exit_code":0,"stdout":big}}},
            {"tool":"execute_shell","tool_call_id":"c2",
             "result":{"success":true,"data":{"exit_code":0,"stdout":big}}}
        ])
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &arr));
        assert!(
            !o.contains(&"y".repeat(100)),
            "batch の本文が会話へ載っている: {o:.120}"
        );
        assert!(o.contains("2 件"), "件数が無い: {o}");
        assert!(o.contains("[subtask 完了]"), "完了ヘッダが無い: {o}");
        assert!(!o.contains("session="), "生 session UUID が残存: {o}");
        assert!(!o.contains("st-1"), "生 subtask_id が残存: {o}");
    }

    /// 9. 参照が本文以上に長くなる極小結果 → 本文を残す（長さ不変条件・#709 と共有）。
    #[test]
    fn tiny_results_are_never_expanded() {
        // 参照より短い極小結果は本文を残す（長さ不変条件・#709 と共有）。参照文言が短くなった
        // （row295b で生 UUID/接頭辞を落とした）ぶん閾値は下がるが、参照が本文以上なら本文を残す
        // 不変条件は不変——空 data で確認する。
        let result = serde_json::json!({"success": true, "data": {}}).to_string();
        let o = format_single_log(&subtask_completed_log("completed", &result));
        assert!(
            !o.contains("本文は会話に残していない"),
            "極小結果を参照へ潰した（長さ不変条件が効いていない）: {o}"
        );
    }

    /// 10. 参照に再取得誘導（「もう一度…読む」）を**含まない**（非冪等・罠3）。
    #[test]
    fn reference_never_promises_refetch() {
        let out = "x".repeat(50_000);
        let result = serde_json::json!({
            "success": true, "data": {"exit_code": 0, "stdout": out}
        })
        .to_string();

        let o = format_single_log(&subtask_completed_log("completed", &result));
        assert!(
            !o.contains("もう一度"),
            "回収できない subtask に再取得を約束している: {o}"
        );
    }

    /// 11a. **構造テスト: top-level の未知 log_type が catch-all で全文を運ぶことの固定**。
    ///
    /// `format_single_log` の catch-all は log_type を問わず `content` を**丸ごと**会話へ運ぶ。
    /// 本文を畳む（参照化する）経路を持つのは現在 2 つだけ——`tool_result`（result_reference）と
    /// `system` + `type=="subtask_completed"`（format_subtask_completed → fold_subtask_completed）。
    /// ここでは top-level の未知 log_type が catch-all で全文を運ぶことを固定する。
    ///
    /// **このテストが守る向き・守らない向き（#716 レビュー 2 巡目・正確に）**:
    /// - **守る**: 許可集合 `FOLDS_BODY_TOP_LEVEL` を更新せずに、census 内の type を**畳む枝を足した**とき
    ///   赤くなる（残骸を減らす安全な方向の変更を「意図的に決めた」証跡として要求する）。
    /// - **守らない**: 新しい log_type が**畳まれずに追加され、30 万文字を運び始める**ケース——**これは
    ///   #713 の再発そのもの**（`subtask_completed` は誰も畳まず 324,176 文字を黙って積み上げた）だが、
    ///   このテストは通ってしまう。catch-all は元から全文を運ぶのが仕様なので、新カリアが catch-all を
    ///   通っても assert は緑のまま。**「新しい運び手はこのテストが検知する」と誤解しないこと。**
    /// - **危険な向き（畳まれない新カリアの追加）を機械的に捕まえるには**、生産者側（`log_type` /
    ///   `type` を書く箇所）を型で持って分類を強制する必要があり、**それは別スコープ**（別 issue）。
    ///   守られていないのに守られていると信じるのが最悪なので、ここに非対称性を明記する。
    #[test]
    fn unknown_log_types_carry_full_body_through_catch_all() {
        // 本文を畳む経路を持つ log_type（top-level）はこれだけ。system は subtype で分岐するので
        // 11b で別に census する。ここを増やすときは畳み経路を意識的に足したことの証跡になる。
        const FOLDS_BODY_TOP_LEVEL: &[&str] = &["tool_result"];

        let body = "運ばれてはいけない生本文".repeat(30);
        let raw_log = |log_type: &str| SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "s".to_string(),
            log_type: log_type.to_string(),
            content: body.clone(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };

        // 将来 type を含む代表 census。畳み集合に無い type は catch-all で全文を運ぶ。
        for lt in [
            "evaluation_note",
            "reflection",
            "brand_new_type_2027",
            "some_future_log_kind",
        ] {
            assert!(
                !FOLDS_BODY_TOP_LEVEL.contains(&lt),
                "census が畳み集合と衝突している（テストの前提が壊れた）: {lt}"
            );
            let out = format_single_log(&raw_log(lt));
            assert!(
                out.contains(&body),
                "catch-all が log_type={lt} の本文を落としている。畳み経路を足したなら \
                 FOLDS_BODY_TOP_LEVEL とこのテストを更新し「畳むか・運ぶか」を明示的に決めること: {out:.80}"
            );
        }
    }

    /// 11b. **構造テスト: `system` の未知サブタイプが全文を運ぶことの固定（#716 レビュー指摘1）**。
    ///
    /// #713 自身が「`system` の 1 サブタイプ（`type=="subtask_completed"`）」として現れたとおり、
    /// **次の運び手も最も自然には `system` + 新しい `type`**（切り離しツールの別カリア等）として来る。
    /// その形は 11a の top-level census を素通りし、`system` アームの `if kind == "subtask_completed"`
    /// も素通りして `to_string_pretty` の丸ごと pretty-print に落ちて**本文を会話へ運ぶ**。11a だけでは
    /// **#713 が生きている次元（system サブタイプ）そのものを見ていなかった**ので、その次元を固定する。
    ///
    /// **このテストが守る向き・守らない向き（#716 レビュー 2 巡目・正確に）**:
    /// - **守る**: 許可集合 `FOLDS_BODY_SYSTEM_SUBTYPES` を更新せずに、census 内のサブタイプを**畳む枝を
    ///   足した**とき赤くなる（system アームに fold 枝を足す＝本文を減らす方向の変更を「意図的に決めた」
    ///   証跡として要求する）。
    /// - **守らない**: 新しい system サブタイプが**畳まれずに追加され、30 万文字を運び始める**ケース——
    ///   **これは #713 の再発そのもの**だが、このテストは通ってしまう。system アームは未知 type を元から
    ///   pretty-print で全文運ぶのが現状の仕様なので、新カリアがそこを通っても assert は緑のまま。
    ///   統括指示の目的「新しい type を足した人が本文の持ち越しに気づかず素通しできてしまう状態を無くす」は
    ///   **この census では達成できていない**（＝ fold を足す向きだけを縛り、運び手を足す向きは縛れない）。
    /// - **危険な向き（畳まれない新カリアの追加）を機械的に捕まえるには**、生産者側（`log_type='system'` を
    ///   書く箇所）を型で持って分類を強制する必要があり、**それは別スコープ**（別 issue）。守られていない
    ///   のに守られていると信じるのが最悪なので、この非対称性をここに明記する。
    #[test]
    fn unknown_system_subtypes_carry_full_body() {
        // 本文を畳む（参照化する）system サブタイプはこれだけ。ここを増やす＝新しい切り離し系
        // カリアを畳むと決めたことの証跡。増やさずに畳む枝を足すと下の census が赤くなる。
        const FOLDS_BODY_SYSTEM_SUBTYPES: &[&str] = &["subtask_completed"];

        let body = "運ばれてはいけない生本文".repeat(30);
        // system ログは content に `type` を持つ JSON。畳まれなければ pretty-print が本文（note）を運ぶ。
        let system_log = |subtype: &str| SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "s".to_string(),
            log_type: "system".to_string(),
            content: serde_json::json!({ "type": subtype, "note": body }).to_string(),
            speaker_id: None,
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };

        // 将来のカリアを含む代表 census。許可集合に無いサブタイプは全文を運ぶ。
        for subtype in [
            "reflection_note",
            "handoff",
            "new_tool_carrier_2027",
            "some_future_system_kind",
        ] {
            assert!(
                !FOLDS_BODY_SYSTEM_SUBTYPES.contains(&subtype),
                "census が畳み集合と衝突している（テストの前提が壊れた）: {subtype}"
            );
            let out = format_single_log(&system_log(subtype));
            assert!(
                out.contains(&body),
                "system サブタイプ type={subtype} の本文が会話から落ちた。切り離し系の新カリアを \
                 畳むなら FOLDS_BODY_SYSTEM_SUBTYPES とこのテストを更新し、そうでなければ本文を \
                 運ぶ（握り潰さない）こと: {out:.80}"
            );
        }
    }
}

/// §9A 会話レンダリング（u/e/c 短縮参照・識別子排除・長文切り詰め）の固定。
#[cfg(test)]
mod render_refs_tests {
    use super::*;
    use opencrab_db::queries::SessionLogRow;

    fn speech(agent: &str, speaker: &str, text: &str, origin: Option<&str>) -> SessionLogRow {
        SessionLogRow {
            id: None,
            agent_id: agent.to_string(),
            session_id: "s".into(),
            log_type: "speech".into(),
            content: text.to_string(),
            speaker_id: Some(speaker.to_string()),
            turn_number: None,
            metadata_json: origin.map(|o| serde_json::json!({ "external_origin": o }).to_string()),
            created_at: None,
        }
    }

    fn tool_call(agent: &str, ids: &[&str]) -> SessionLogRow {
        let calls: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| serde_json::json!({"id": id, "function": {"name": "reply", "arguments": "{}"}}))
            .collect();
        let tcj = serde_json::Value::Array(calls).to_string();
        SessionLogRow {
            id: Some(1),
            agent_id: agent.to_string(),
            session_id: "s".into(),
            log_type: "tool_call".into(),
            content: String::new(),
            speaker_id: Some(agent.to_string()),
            turn_number: None,
            metadata_json: Some(serde_json::json!({ "tool_calls_json": tcj }).to_string()),
            created_at: None,
        }
    }

    fn tool_result(agent: &str, id: &str) -> SessionLogRow {
        SessionLogRow {
            id: Some(2),
            agent_id: agent.to_string(),
            session_id: "s".into(),
            log_type: "tool_result".into(),
            content: "ok".into(),
            speaker_id: Some(agent.to_string()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({"tool_call_id": id, "tool_name": "reply"}).to_string(),
            ),
            created_at: None,
        }
    }

    #[test]
    fn speakers_numbered_by_first_appearance_self_has_no_u() {
        let logs = vec![
            speech("me", "pk_alice", "hi", Some("nostr:event:v1:default:e1")),
            speech("me", "me", "hello", None),
            speech("me", "pk_bob", "yo", Some("nostr:event:v1:default:e2")),
            speech("me", "pk_alice", "again", Some("nostr:event:v1:default:e3")),
        ];
        let refs = ConversationRefs::build(&logs, "me");
        assert_eq!(refs.speaker_label("pk_alice"), "u1");
        assert_eq!(refs.speaker_label("pk_bob"), "u2");
        // 自分は u 番号なし（生の agent_id のまま = 名前だけの位置づけ）。
        assert_eq!(refs.speaker_label("me"), "me");
        // 未知話者は生のまま。
        assert_eq!(refs.speaker_label("pk_carol"), "pk_carol");
    }

    #[test]
    fn events_numbered_per_origin_first_appearance() {
        let logs = vec![
            speech("me", "pk_a", "a", Some("nostr:event:v1:default:AAA")),
            speech("me", "pk_b", "b", Some("nostr:event:v1:watch:7:BBB")),
        ];
        let refs = ConversationRefs::build(&logs, "me");
        let a = format_single_log_with_echo(&logs[0], None, Some(&refs));
        assert!(a.starts_with("[u1]e1:"), "{a}");
        let b = format_single_log_with_echo(&logs[1], None, Some(&refs));
        assert!(b.starts_with("[u2]e2:"), "{b}");
        // 生 ID（npub/note/hex/origin）は会話へ出さない。
        assert!(!a.contains("AAA") && !a.contains("nostr:event"));
    }

    #[test]
    fn numbers_are_stable_when_new_logs_arrive() {
        let mut logs = vec![speech("me", "pk_a", "a", Some("nostr:event:v1:default:X"))];
        let first = ConversationRefs::build(&logs, "me");
        assert_eq!(first.speaker_label("pk_a"), "u1");
        logs.push(speech("me", "pk_b", "b", Some("nostr:event:v1:default:Y")));
        let second = ConversationRefs::build(&logs, "me");
        // 既存の番号は不変（初出順は append-only で安定）。
        assert_eq!(second.speaker_label("pk_a"), "u1");
        assert_eq!(second.speaker_label("pk_b"), "u2");
    }

    #[test]
    fn tool_calls_and_results_share_c_numbers() {
        let logs = vec![
            tool_call("me", &["call_aaa", "call_bbb"]),
            tool_result("me", "call_aaa"),
        ];
        let refs = ConversationRefs::build(&logs, "me");
        let call_render = format_single_log_with_echo(&logs[0], None, Some(&refs));
        assert!(call_render.contains("[c1]: reply("), "{call_render}");
        assert!(call_render.contains("[c2]: reply("), "{call_render}");
        assert!(!call_render.contains("call_aaa"));
        let result_render = format_single_log_with_echo(&logs[1], None, Some(&refs));
        assert!(result_render.contains("[c1]:"), "{result_render}");
        assert!(!result_render.contains("call_aaa"));
    }

    #[test]
    fn timeline_items_truncate_at_200_direct_at_2000() {
        let long = "あ".repeat(5000);
        let tl = speech("me", "pk_a", &long, Some("nostr:event:v1:watch:3:E"));
        let refs = ConversationRefs::build(std::slice::from_ref(&tl), "me");
        let out = format_single_log_with_echo(&tl, None, Some(&refs));
        assert!(out.contains("…(全5000字)"), "{}", &out[..out.len().min(80)]);
        // 200 字 + マーカー。元 5000 字は載らない。
        assert!(out.chars().count() < 400);

        let direct = speech("me", "pk_a", &long, Some("nostr:event:v1:default:E"));
        let refs2 = ConversationRefs::build(std::slice::from_ref(&direct), "me");
        let out2 = format_single_log_with_echo(&direct, None, Some(&refs2));
        assert!(out2.contains("…(全5000字)"));
        // 2000 字保持（自分宛て）。
        assert!(out2.chars().count() > 2000);
    }

    #[test]
    fn self_speech_is_not_truncated() {
        let long = "x".repeat(5000);
        let mine = speech("me", "me", &long, None);
        let refs = ConversationRefs::build(std::slice::from_ref(&mine), "me");
        let out = format_single_log_with_echo(&mine, None, Some(&refs));
        assert!(!out.contains("…(全"));
    }

    #[test]
    fn none_refs_keeps_legacy_rendering() {
        let ev = speech("me", "pk_a", "hi", Some("nostr:event:v1:default:E"));
        let out = format_single_log_with_echo(&ev, None, None);
        // refs なしは従来の生表示（u/e 番号なし）。
        assert!(out.starts_with("[pk_a]"), "{out}");
    }

    /// §9A.1 / row292: DI operation（reply）の tool_call は完了後も arguments（本文）が
    /// 会話へ verbatim 残る。nostr_run 時代の本文喪失の再発防止。preserve_arg_call_ids を
    /// 付けない同一 call は従来どおり →log 参照へ短縮され本文が消えることを対照で示す。
    fn reply_tool_call(preserve: bool) -> SessionLogRow {
        let tcj = serde_json::json!([{
            "id": "call_reply1",
            "function": {"name": "reply", "arguments": "{\"event\":\"e3\",\"text\":\"次ターンに残るべき本文\"}"}
        }])
        .to_string();
        let meta = if preserve {
            serde_json::json!({"tool_calls_json": tcj, "preserve_arg_call_ids": ["call_reply1"]})
        } else {
            serde_json::json!({ "tool_calls_json": tcj })
        };
        SessionLogRow {
            id: Some(7),
            agent_id: "me".into(),
            session_id: "s".into(),
            log_type: "tool_call".into(),
            content: String::new(),
            speaker_id: Some("me".into()),
            turn_number: None,
            metadata_json: Some(meta.to_string()),
            created_at: None,
        }
    }

    #[test]
    fn di_reply_body_survives_digest_next_turn() {
        // call は完了済み（次ターン相当）。
        let completed: std::collections::HashSet<String> =
            std::iter::once("call_reply1".to_string()).collect();

        // preserve あり: 本文が残る（digest されない）。
        let kept = format_single_log_with_echo(&reply_tool_call(true), Some(&completed), None);
        assert!(
            kept.contains("次ターンに残るべき本文"),
            "DI reply 本文が次ターンで消えている: {kept}"
        );

        // 対照（preserve なし）: 従来どおり digest されて本文が消える。
        let lost = format_single_log_with_echo(&reply_tool_call(false), Some(&completed), None);
        assert!(
            !lost.contains("次ターンに残るべき本文"),
            "preserve なしなら →log 短縮で消えるはず（対照）: {lost}"
        );
    }

    // row294b 追修 1/2: 表示時に legacy メタ行・種別ラベル行・生識別子を剥がす。
    #[test]
    fn strips_legacy_meta_line_and_raw_ids_at_display() {
        let npub = format!("npub1{}", "q".repeat(58));
        let note = format!("note1{}", "p".repeat(58));
        let body = format!("こんにちは\n[Nostr kind:1 メンション from={npub} target={note}]");
        let ev = speech("me", "pk_a", &body, Some("nostr:event:v1:default:E"));
        let refs = ConversationRefs::build(std::slice::from_ref(&ev), "me");
        let out = format_single_log_with_echo(&ev, None, Some(&refs));
        assert!(out.contains("こんにちは"), "本文は残す: {out}");
        assert!(
            !out.contains("[Nostr kind:"),
            "種別ラベル行を出さない: {out}"
        );
        assert!(
            !out.contains(&npub) && !out.contains(&note),
            "生 ID を出さない: {out}"
        );
        assert!(!out.contains("from=") && !out.contains("target="), "{out}");
    }

    #[test]
    fn strips_new_9a_label_line_at_display() {
        // 新 §9A 形（from=/target= 無し）でもラベル行は表示に出さない。
        let ev = speech(
            "me",
            "pk_a",
            "やあ\n[Nostr kind:1 メンション]",
            Some("nostr:event:v1:default:E"),
        );
        let refs = ConversationRefs::build(std::slice::from_ref(&ev), "me");
        let out = format_single_log_with_echo(&ev, None, Some(&refs));
        assert!(out.contains("やあ"), "{out}");
        assert!(!out.contains("[Nostr kind:"), "{out}");
        assert!(!out.contains("メンション"), "ラベル語も残さない: {out}");
    }

    #[test]
    fn elides_bare_identifiers_in_body_but_keeps_short_hashes() {
        let pubkey = "b".repeat(64); // 64hex
        let body = format!("引用: npub1{} と {pubkey} と call_abc123", "q".repeat(58));
        let ev = speech("me", "pk_a", &body, Some("nostr:event:v1:default:E"));
        let refs = ConversationRefs::build(std::slice::from_ref(&ev), "me");
        let out = format_single_log_with_echo(&ev, None, Some(&refs));
        assert!(!out.contains(&pubkey), "64hex を短縮: {out}");
        assert!(!out.contains("npub1qqq"), "bech32 を短縮: {out}");
        assert!(out.contains("<npub…>") && out.contains("<id…>"), "{out}");
        // 短い識別子（tool call の一部など）は温存する。
        assert!(out.contains("call_abc123"), "短い hash は温存: {out}");
    }

    #[test]
    fn strip_meta_is_display_only_not_stored() {
        // strip はレンダリング専用。ログ本文（保存データ相当）は変更しない。
        let body = "本文\n[Nostr kind:1 メンション]".to_string();
        let ev = speech("me", "pk_a", &body, Some("nostr:event:v1:default:E"));
        assert_eq!(ev.content, body, "保存データは書き換えない");
    }

    // row295c: 自分の話者行は UUID でなく名前（agents.name）。
    #[test]
    fn self_speaker_shows_name_not_uuid() {
        let logs = vec![speech("agent-uuid-xyz", "agent-uuid-xyz", "やあ", None)];
        let mut refs = ConversationRefs::build(&logs, "agent-uuid-xyz");
        refs.set_agent_name("くらぶ");
        let out = format_single_log_with_echo(&logs[0], None, Some(&refs));
        assert!(out.starts_with("[くらぶ]"), "{out}");
        assert!(!out.contains("agent-uuid-xyz"), "生 UUID が残存: {out}");
    }

    // row295b: 括弧間スペースを出さない（ts あり）。
    #[test]
    fn header_has_no_spaces_between_brackets() {
        let mut ev = speech("me", "pk_a", "hi", Some("nostr:event:v1:default:E"));
        ev.created_at = Some("2026-08-30T11:14:42+00:00".into());
        let refs = ConversationRefs::build(std::slice::from_ref(&ev), "me");
        let out = format_single_log_with_echo(&ev, None, Some(&refs));
        assert!(out.starts_with("[u1][2026-08-30 11:14:42]e1:"), "{out}");
        assert!(!out.contains("] ["), "括弧間スペース: {out}");
    }

    // row295c: リプライ/リアクション/リポストは関係注記を残す（素の投稿は注記なし）。
    #[test]
    fn reply_and_reaction_get_relation_annotation() {
        let reply = speech(
            "me",
            "pk_a",
            "そうだね\n[Nostr kind:1 リプライ]",
            Some("nostr:event:v1:default:R"),
        );
        let reaction = speech(
            "me",
            "pk_a",
            "🫧\n[Nostr kind:7 リアクション]",
            Some("nostr:event:v1:default:X"),
        );
        let plain = speech(
            "me",
            "pk_a",
            "こんにちは\n[Nostr kind:1 メンション]",
            Some("nostr:event:v1:default:M"),
        );
        let logs = vec![reply, reaction, plain];
        let refs = ConversationRefs::build(&logs, "me");
        let o_reply = format_single_log_with_echo(&logs[0], None, Some(&refs));
        assert!(o_reply.contains("(reply→外部):"), "{o_reply}");
        assert!(
            !o_reply.contains("[Nostr kind:"),
            "ラベル行は出さない: {o_reply}"
        );
        let o_reaction = format_single_log_with_echo(&logs[1], None, Some(&refs));
        assert!(o_reaction.contains("(reaction→外部):"), "{o_reaction}");
        assert!(o_reaction.contains("🫧"), "本文が残る: {o_reaction}");
        let o_plain = format_single_log_with_echo(&logs[2], None, Some(&refs));
        assert!(!o_plain.contains("→外部"), "素の投稿に注記なし: {o_plain}");
    }

    // row295c 6b: reply_target が記録されていれば会話内の e 番号へ解決する。
    #[test]
    fn reply_target_resolves_to_e_number_when_recorded() {
        let target_id = "cc".repeat(32);
        let target = speech(
            "me",
            "pk_a",
            "元投稿",
            Some(&format!("nostr:event:v1:default:{target_id}")),
        );
        let mut reply = speech(
            "me",
            "pk_b",
            "そうだね\n[Nostr kind:1 リプライ]",
            Some(&format!("nostr:event:v1:default:{}", "dd".repeat(32))),
        );
        reply.metadata_json = Some(
            serde_json::json!({
                "external_origin": format!("nostr:event:v1:default:{}", "dd".repeat(32)),
                "reply_target": target_id,
            })
            .to_string(),
        );
        let logs = vec![target, reply];
        let refs = ConversationRefs::build(&logs, "me");
        let out = format_single_log_with_echo(&logs[1], None, Some(&refs));
        assert!(
            out.contains("(reply→e1)"),
            "対象が e 番号解決されない: {out}"
        );
    }

    // row295d: 凍結 snapshot blob の UUID / call_ / digest hex を除去・短縮する。
    #[test]
    fn frozen_snapshot_elides_uuid_call_and_digest() {
        let blob = "[me][2026-08-30 06:06:45]:\n[tool_call]:\n[c1]: execute_shell({\"ref\":\"log:1\",\"digest\":\"15e51315716f5bc7\",\"bytes\":116})\ncall_XH2Y1M9nLDkUzHxvC3J2RLCb → spawned subtask df58ec83-960c-45e3-b69c-ff493b133afc";
        let out = strip_frozen_snapshot(blob);
        assert!(
            !out.contains("df58ec83-960c-45e3-b69c-ff493b133afc"),
            "UUID 残存: {out}"
        );
        assert!(out.contains("<uuid…>"), "{out}");
        assert!(
            !out.contains("call_XH2Y1M9nLDkUzHxvC3J2RLCb"),
            "call_ 残存: {out}"
        );
        assert!(out.contains("<call…>"), "{out}");
        assert!(!out.contains("15e51315716f5bc7"), "digest hex 残存: {out}");
        assert!(
            out.contains("\"digest\":\"…\""),
            "digest 短縮形が無い: {out}"
        );
        // 新形式（→log 参照・c 番号）は保持。
        assert!(out.contains("log:1") && out.contains("[c1]"), "{out}");
    }

    // row295d 変種: dashed session id（`nostr-<uuid>-<channel>`）に埋まった UUID も剥がす。
    #[test]
    fn frozen_snapshot_elides_uuid_embedded_in_dashed_session_id() {
        let blob = "session=nostr-33196264-5908-4f04-b24a-efd7aa6d2014-caldera へ完了";
        let out = strip_frozen_snapshot(blob);
        assert!(
            !out.contains("33196264-5908-4f04-b24a-efd7aa6d2014"),
            "埋め込み UUID 残存: {out}"
        );
        assert!(out.contains("<uuid…>"), "{out}");
        // 周辺（session=nostr-…-caldera）は残ってよい（生 UUID だけ落とす）。
        assert!(out.contains("caldera") && out.contains("完了"), "{out}");
    }

    // 単一ログ経路（per-log）は UUID/call_/digest を触らない（利用者本文の過剰除去を避ける）。
    #[test]
    fn per_log_strip_leaves_uuid_untouched() {
        let body = "予約番号は df58ec83-960c-45e3-b69c-ff493b133afc です";
        assert_eq!(strip_inbound_meta_for_display(body), body);
    }

    // row295b: subtask_completed は s 番号ヘッダ＋result 本文のみ（生 UUID/定型 field を出さない）。
    #[test]
    fn subtask_completed_uses_s_number_not_uuid() {
        let spawn = SessionLogRow {
            id: Some(1),
            agent_id: "me".into(),
            session_id: "s".into(),
            log_type: "tool_result".into(),
            content: r#"{"success":true,"data":{"subtask_id":"sub-xyz-1","status":"spawned"}}"#
                .into(),
            speaker_id: Some("me".into()),
            turn_number: None,
            metadata_json: Some(r#"{"tool_call_id":"tc-1","tool_name":"spawn_subtask"}"#.into()),
            created_at: None,
        };
        let done = SessionLogRow {
            id: Some(2),
            agent_id: "me".into(),
            session_id: "s".into(),
            log_type: "system".into(),
            content: r#"{"type":"subtask_completed","subtask_id":"sub-xyz-1","session_id":"subtask-sub-xyz-1","exit_reason":"completed","result":"調査おわり"}"#.into(),
            speaker_id: None,
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        let logs = vec![spawn, done];
        let refs = ConversationRefs::build(&logs, "me");
        let out = format_single_log_with_echo(&logs[1], None, Some(&refs));
        assert!(out.contains("[s1 完了]"), "s 番号ヘッダが無い: {out}");
        assert!(out.contains("調査おわり"), "result 本文が残る: {out}");
        assert!(!out.contains("sub-xyz-1"), "生 UUID が残存: {out}");
        assert!(!out.contains("exit_reason"), "定型 field が残存: {out}");
    }
}
