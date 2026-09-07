use crate::tokens::estimate_tokens;

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
/// 渡された予算（`build_conversation_inner` が受け取る `context_budget_tokens`）に対する割合に
/// する。ここで全体予算の 30% を基準にすると、渡された予算を要約が丸ごと食い潰して
/// **上の症状がそのまま再現する**。渡された予算に対する割合にすることで、
/// 「実会話を優先し、ハートビート履歴は下限だけ」というオーナー決定とも向きが揃う。
#[allow(dead_code)]
pub(super) const PAST_SUMMARY_BUDGET_NUM: usize = 3;
#[allow(dead_code)]
pub(super) const PAST_SUMMARY_BUDGET_DEN: usize = 10;

/// `[Past context summary]` のヘッダ。**予算判定にはこのヘッダぶんも含める**
/// （セクション全体で 30% に収める）。
#[allow(dead_code)]
const PAST_SUMMARY_HEADER: &str =
    "[Past context summary (use retrieve_memory_nodes with short_id to recall details)]\n";

/// 予算に入らず落とした topic 要約があることを本人へ伝える 1 行（#406）。
///
/// **落としたことを黙らない。** 本番では 2,400 件級が文脈から消えるので、この 1 行が
/// 唯一の復旧導線になる。したがって**書いてある呼び方が実際に通ること**が要件で、
/// `past_summary_budget_tests::omitted_notice_matches_the_real_tool_surface` で固定する。
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
pub(super) fn build_past_context_summary_section(
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
