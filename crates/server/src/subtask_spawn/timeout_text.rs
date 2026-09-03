/// #332: subtask がタイムアウトで打ち切られたときに `subtask_completed` の本文
/// （`result`）として残す文言を組む。**タイムアウト限定**（他の `exit_reason` の本文は
/// 一切変えない）。
///
/// 旧文言は `"Subtask timed out."` の一文だけで、「終わった」という通知に見えて何を
/// すべきかが書かれておらず、エージェントが `NO_REPLY` を選んでいた（issue #332 実測）。
/// ここでは **「未完了・対応が必要」と読める形**にし、途中経過の在り処（sub セッションの
/// ログ）と、経過・上限の実数を添える。
///
/// **返信は強制しない**。これは情報として渡すだけで、実際に反応するかどうかは
/// エージェントが決める（オーナー方針「人間の反応を無視するかはエージェントが決めればいい。
/// 強制はしなくていい」/ 評価を会話へ割り込ませない #291・#292 と同じ線引き）。文言は
/// 具体的な機能（例: 再開）を前提にせず、いま取れる手だけを例示にとどめる。
pub(super) fn timeout_result_text(
    sub_session_id: &str,
    elapsed_secs: u64,
    timeout_secs: u64,
) -> String {
    format!(
        "サブタスクが制限時間（{timeout_secs}秒）内に終わらず、約{elapsed_secs}秒で打ち切られました。\
         未完了なので対応が必要です。どこまで進んだかは `{sub_session_id}` セッションのログに\
         残っているので、確認して次の手（別の方法を試す・改めて依頼し直す・見送る等）を\
         判断してください。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #332: タイムアウトの本文が「未完了・対応が必要」と読め、途中経過の在り処
    /// （`subtask-{id}` セッション）と経過・上限の実数を含むこと。**返信は強制しない**
    /// ので命令形の強制文言（「返信せよ」等）は入れない。旧文言 `"Subtask timed out."`
    /// のような「終わった」だけの通知に戻ったら落ちる。
    #[test]
    fn timeout_result_text_prompts_action_without_forcing_reply() {
        let text = timeout_result_text("subtask-abc123", 300, 300);

        // 「対応が必要」と読める（issue の核心: 何をすべきか分かる）。
        assert!(text.contains("対応が必要"), "対応を促す文言が無い: {text}");
        // 「未完了」と明示する（#444 と同じく timeout を completed と言わない）。
        assert!(text.contains("未完了"), "未完了と明示していない: {text}");
        // 完了を肯定する言い回しにはしない（「完了しました」等）。
        assert!(
            !text.contains("完了しました"),
            "timeout なのに完了を断言している: {text}"
        );
        // 途中経過の在り処（sub セッション名 = subtask-{id}）を指す。
        assert!(
            text.contains("subtask-abc123"),
            "ログの在り処（sub セッション）が無い: {text}"
        );
        // 経過秒・上限秒の実数が入る（何がどれだけ経ったか）。
        assert!(text.contains("300"), "経過/上限の実数が無い: {text}");
        // 返信を強制しない: 命令的な返信要求を入れない（促すが強制はしない）。
        assert!(
            !text.contains("返信して") && !text.contains("必ず返信"),
            "返信を強制する文言が入っている: {text}"
        );
        // 旧文言そのものには戻らない。
        assert_ne!(text, "Subtask timed out.");
    }

    /// 経過秒と上限秒は引数がそのまま反映される（固定文字列ではない）。
    #[test]
    fn timeout_result_text_reflects_elapsed_and_limit() {
        let text = timeout_result_text("subtask-xyz", 42, 120);
        assert!(text.contains("120"), "上限秒が反映されない: {text}");
        assert!(text.contains("42"), "経過秒が反映されない: {text}");
        assert!(text.contains("subtask-xyz"));
    }
}
