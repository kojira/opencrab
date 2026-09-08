use super::assembly::{RECENT_MIN_LOGS, RECENT_MIN_USER_SPEECHES};
use super::format::format_single_log;
use crate::tokens::estimate_tokens;

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
