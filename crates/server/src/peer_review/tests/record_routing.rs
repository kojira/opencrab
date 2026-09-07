    /// 回収の 3 つのゲート（marker / 登録済み co_agent / 未回収の依頼）と 1 依頼 1 記録。
    #[test]
    fn record_reply_gates_and_writes() {
        let db = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        let task_id = {
            let conn = db.lock().unwrap();
            // 送信者 "42" をこのエージェントの co_agent として登録
            opencrab_db::queries::add_trusted_user(
                &conn,
                opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
                "row-1",
                "a1",
                "42",
                TrustedUserPermission::CoAgent,
                "owner",
                "2026-01-01",
                "Crab B",
            )
            .unwrap();
            let task_id =
                opencrab_db::queries::insert_task_ledger(&conn, "a1", "s1", "goal", None).unwrap();
            // 未回収のレビュー依頼を記録
            opencrab_db::queries::insert_task_progress(
                &conn,
                task_id,
                "progress",
                "[peer review requested] posted to channel 1 (1 parts)",
            )
            .unwrap();
            task_id
        };
        let reply = "[Peer Review] score: 0.6\ngaps:\n- missing tests\nsummary: incomplete";

        // marker 無し → 記録しない
        assert!(!record_discord_reply(
            &db, "a1", "s1", "42", "crab-b", "hello"
        ));
        // 未登録送信者（co_agent でない）→ 記録しない
        assert!(!record_discord_reply(
            &db, "a1", "s1", "99", "stranger", reply
        ));
        // active タスクの無いセッション → 記録しない
        assert!(!record_discord_reply(
            &db, "a1", "other", "42", "crab-b", reply
        ));
        // 正常系
        assert!(record_discord_reply(&db, "a1", "s1", "42", "crab-b", reply));
        // 依頼が回収済みになったので、追加の返信は記録しない（1依頼1記録）
        assert!(!record_discord_reply(
            &db, "a1", "s1", "42", "crab-b", reply
        ));

        let conn = db.lock().unwrap();
        let progress = opencrab_db::queries::list_recent_task_progress(&conn, task_id, 10).unwrap();
        assert_eq!(progress.len(), 2); // requested + received
        assert!(progress[1]
            .content
            .contains("[peer review] score 0.60 (from crab-b)"));
        assert!(progress[1].content.contains("missing tests"));
    }

    /// 同じ送信者識別子でも、**登録された経路が違えば解決しない**（fail-closed / #214）。
    ///
    /// 由来（[`TranscriptSource`]）から経路を引くので、経路の列を持たない由来
    /// （Nostr）では回収しない。「とりあえず discord で引く」に戻すとこのテストが落ちる。
    #[test]
    fn harvest_resolves_sender_in_the_route_that_declared_it() {
        let db = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        {
            let conn = db.lock().unwrap();
            opencrab_db::queries::add_trusted_user(
                &conn,
                opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
                "row-1",
                "a1",
                "42",
                TrustedUserPermission::CoAgent,
                "owner",
                "2026-01-01",
                "Crab B",
            )
            .unwrap();
            let task_id =
                opencrab_db::queries::insert_task_ledger(&conn, "a1", "s1", "goal", None).unwrap();
            opencrab_db::queries::insert_task_progress(
                &conn,
                task_id,
                "progress",
                "[peer review requested] posted to channel 1 (1 parts)",
            )
            .unwrap();
        }
        let reply = "[Peer Review] score: 0.6 summary: ok";
        let record = InboundMessageRecord {
            session_id: "s1",
            recipient_agent_id: "agent-a",
            sender_id: "42",
            sender_name: "crab-b",
            avatar_url: None,
            channel_id: Some("1"),
            pubkey: None,
            text: reply,
            image_urls: &[],
        };
        // 経路の列を持たない由来 → 回収しない（識別子が偶然一致しても受理しない）
        assert!(!harvest_inbound_reply(
            &db,
            TranscriptSource::Nostr,
            "a1",
            &record
        ));
        assert!(trusted_platform_for(TranscriptSource::Nostr).is_none());
        // Discord は登録済み経路 → 回収する
        assert!(harvest_inbound_reply(
            &db,
            TranscriptSource::Discord,
            "a1",
            &record
        ));
    }

