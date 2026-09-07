    /// 1 バッチ分の「実際に届く全文」を連結する（添付があれば添付本体）。
    fn join_delivered(msgs: &[webhook::WebhookMessage]) -> String {
        msgs.iter()
            .map(|m| m.delivered_text())
            .collect::<Vec<_>>()
            .join("")
    }

    use super::*;
    use opencrab_actions::subtask::SettleKind;

    // ---- RFC #152 S1: DiscordCompletionSink（完了の再注入経路） ----
    //
    // この sink は「dispatch した全ツールの結果を親会話へ戻す」唯一の口なので、
    // 空実装にしても他テストが緑のままだと退行を検知できない（#165 レビュー P1。
    // web sink に対する同種の指摘と同じ）。実 mpsc を張って、送出の有無と
    // routing 復元内容をここで直接固定する。

    /// テスト用: 実チャネルを張った sink と受信側を作る。
    fn sink_with_channel() -> (
        DiscordCompletionSink,
        tokio::sync::mpsc::UnboundedReceiver<LoopEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (DiscordCompletionSink { event_tx: Some(tx) }, rx)
    }

    fn settled(session_id: &str, kind: SettleKind, exit_reason: &str) -> SubtaskSettled {
        settled_as(
            session_id,
            kind,
            exit_reason,
            opencrab_actions::CallerIdentity::Agent,
        )
    }

    /// 親ターンの呼び出し元を明示した決着イベント（#298）。
    fn settled_as(
        session_id: &str,
        kind: SettleKind,
        exit_reason: &str,
        caller: opencrab_actions::CallerIdentity,
    ) -> SubtaskSettled {
        SubtaskSettled {
            session_id: session_id.to_string(),
            agent_id: "agent-x".to_string(),
            subtask_id: "st-1".to_string(),
            exit_reason: exit_reason.to_string(),
            kind,
            reply_target: None,
            caller,
        }
    }

    /// 完了 → `LoopEvent::SubtaskCompleted` がちょうど 1 本流れ、guild/channel が
    /// parent_session_id から復元される（本文は運ばない = `result` は空）。
    #[test]
    fn discord_sink_emits_loop_event_on_completion() {
        let (sink, mut rx) = sink_with_channel();
        opencrab_actions::dispatch_settled(
            &sink,
            settled(
                "discord-agent-x-111222333-444555666",
                SettleKind::Completed,
                "completed",
            ),
        );

        match rx.try_recv().expect("完了は LoopEvent を 1 本送る") {
            LoopEvent::SubtaskCompleted {
                session_id,
                agent_id,
                subtask_id,
                result,
                exit_reason,
                channel_id,
                channel_id_str,
                guild_id,
                is_dm,
                caller,
            } => {
                assert_eq!(session_id, "discord-agent-x-111222333-444555666");
                assert_eq!(caller, opencrab_actions::CallerIdentity::Agent);
                assert_eq!(agent_id, "agent-x");
                assert_eq!(subtask_id, "st-1");
                // 本文は DB（session_logs）から読み直す契約（RFC §1.3）。
                assert_eq!(result, "");
                assert_eq!(exit_reason, "completed");
                assert_eq!(channel_id, 444_555_666);
                assert_eq!(channel_id_str, "444555666");
                assert_eq!(guild_id, "111222333");
                assert!(!is_dm);
            }
            _ => panic!("SubtaskCompleted 以外のイベントが流れた"),
        }
        assert!(rx.try_recv().is_err(), "余分なイベントを送ってはならない");
    }

    /// #298: 決着通知の呼び出し元をそのまま `LoopEvent` へ載せる。
    ///
    /// resume は `process_subtask_completed` が受けて `RunRequest` を組むので、
    /// ここで落とすと元ターンが最小権限へ降格し、owner/trusted のツールが消える。
    #[test]
    fn discord_sink_forwards_the_original_caller() {
        for caller in [
            opencrab_actions::CallerIdentity::Owner,
            opencrab_actions::CallerIdentity::TrustedUser,
            opencrab_actions::CallerIdentity::Agent,
        ] {
            let (sink, mut rx) = sink_with_channel();
            opencrab_actions::dispatch_settled(
                &sink,
                settled_as(
                    "discord-agent-x-111222333-444555666",
                    SettleKind::Completed,
                    "progress",
                    caller.clone(),
                ),
            );
            match rx.try_recv().expect("完了は LoopEvent を 1 本送る") {
                LoopEvent::SubtaskCompleted {
                    caller: forwarded, ..
                } => assert_eq!(
                    forwarded, caller,
                    "決着通知の呼び出し元が resume まで運ばれていない"
                ),
                _ => panic!("SubtaskCompleted 以外のイベントが流れた"),
            }
        }
    }

    /// DM（guild_id 空）の親セッションでも復元でき、`is_dm` が立つ。
    #[test]
    fn discord_sink_restores_dm_routing() {
        let (sink, mut rx) = sink_with_channel();
        opencrab_actions::dispatch_settled(
            &sink,
            settled(
                "discord-agent-x--444555666",
                SettleKind::Completed,
                "timeout",
            ),
        );

        match rx.try_recv().expect("DM でも LoopEvent を送る") {
            LoopEvent::SubtaskCompleted {
                guild_id,
                channel_id,
                is_dm,
                exit_reason,
                ..
            } => {
                assert_eq!(guild_id, "");
                assert_eq!(channel_id, 444_555_666);
                assert!(is_dm, "guild_id が空なら DM 扱い");
                // exit_reason は完了理由をそのまま運ぶ（completed 以外も再注入する）。
                assert_eq!(exit_reason, "timeout");
            }
            _ => panic!("SubtaskCompleted 以外のイベントが流れた"),
        }
        assert!(rx.try_recv().is_err());
    }

    /// 非 Discord の親セッション（web / heartbeat / nostr / ネストした subtask）は
    /// 正常系としてスキップする。各 gateway の sink が自分のセッションだけを拾う。
    #[test]
    fn discord_sink_skips_non_discord_sessions() {
        for session_id in [
            "web-agent-x-conv-1",
            "heartbeat-agent-x",
            "nostr-agent-x-npub1abc",
            "subtask-11111111-2222-3333-4444-555555555555",
            "agent-msg-agent-x-user-1",
            "",
        ] {
            let (sink, mut rx) = sink_with_channel();
            opencrab_actions::dispatch_settled(
                &sink,
                settled(session_id, SettleKind::Completed, "completed"),
            );
            assert!(
                rx.try_recv().is_err(),
                "非 Discord セッション '{session_id}' で LoopEvent を送ってはならない"
            );
        }
    }

    /// Discord 形式に見えて壊れている session_id（channel が数値でない等）も送らない。
    #[test]
    fn discord_sink_skips_malformed_discord_sessions() {
        for session_id in [
            "discord-agent-x-111-notanumber",
            "discord-agent-x-notanumber-444",
            "discord--111-444",
            "discord-agent-x",
        ] {
            let (sink, mut rx) = sink_with_channel();
            opencrab_actions::dispatch_settled(
                &sink,
                settled(session_id, SettleKind::Completed, "completed"),
            );
            assert!(
                rx.try_recv().is_err(),
                "壊れた session_id '{session_id}' で LoopEvent を送ってはならない"
            );
        }
    }

    /// **意図的な差分**: Discord は `SettleKind::Progress` でも LoopEvent を送る
    /// （web / Nostr の sink は Completed 以外を捨てる）。
    ///
    /// `report_progress` のデバウンス発火はこの sink を通ってメインエンジンを
    /// 呼び直す実況機能で、main の `send_subtask_completed_event(..., "progress")`
    /// から続く既存挙動。ここで捨てると進捗実況が黙って消えるため、
    /// web / Nostr と同じ `kind != Completed` ガードは**入れない**。
    /// 差分を退行ではなく仕様として固定するためのテスト。
    #[test]
    fn discord_sink_forwards_progress_unlike_web_and_nostr() {
        let (sink, mut rx) = sink_with_channel();
        opencrab_actions::dispatch_settled(
            &sink,
            settled(
                "discord-agent-x-111222333-444555666",
                SettleKind::Progress,
                "progress",
            ),
        );

        match rx.try_recv().expect("進捗もメインエンジンへ再注入する") {
            LoopEvent::SubtaskCompleted { exit_reason, .. } => {
                assert_eq!(exit_reason, "progress");
            }
            _ => panic!("SubtaskCompleted 以外のイベントが流れた"),
        }
    }

    /// `on_subtask_cancelled` は既定実装（debug ログのみ）のまま = 停止では
    /// 再注入しない（止めたのに返信が届くのを防ぐ）。
    #[test]
    fn discord_sink_does_not_reinject_on_cancel() {
        let (sink, mut rx) = sink_with_channel();
        sink.on_subtask_cancelled(settled(
            "discord-agent-x-111222333-444555666",
            SettleKind::Cancelled,
            "cancelled",
        ));
        assert!(
            rx.try_recv().is_err(),
            "cancel で LoopEvent を送ってはならない"
        );
    }

    /// event_tx 未設定（イベントループの無い構築）は no-op で panic しない。
    #[test]
    fn discord_sink_without_event_tx_is_noop() {
        let sink = DiscordCompletionSink { event_tx: None };
        opencrab_actions::dispatch_settled(
            &sink,
            settled(
                "discord-agent-x-111222333-444555666",
                SettleKind::Completed,
                "completed",
            ),
        );
    }

