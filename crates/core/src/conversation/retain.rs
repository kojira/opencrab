use super::format::format_single_log;
use super::legacy_budget_fit::fit_logs_to_budget;

#[allow(dead_code)]
fn format_logs(logs: &[opencrab_db::queries::SessionLogRow]) -> String {
    logs.iter()
        .map(format_single_log)
        .collect::<Vec<_>>()
        .join("\n")
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
/// （`is_heartbeat_prompt_scaffolding`）は**会話から全件落とす**（#501）。指示文はその
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
pub(super) fn build_recent_window(
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
