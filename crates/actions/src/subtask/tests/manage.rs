pub(super) mod cases {
    use super::super::sink::cases::RecordingSink;
    use super::super::*;

    /// 与えた parent_session_id で「即完了しない」fake subtask を registry へ登録し、
    /// その JoinHandle を返す（abort されたか検証するため）。
    pub(crate) fn insert_fake_subtask(
        registry: &SubtaskRegistry,
        subtask_id: &str,
        parent_session_id: &str,
    ) -> tokio::task::JoinHandle<()> {
        let handle = tokio::spawn(std::future::pending::<()>());
        registry.insert(
            subtask_id.to_string(),
            SpawnedSubtask {
                abort_handle: handle.abort_handle(),
                session_id: format!("subtask-{subtask_id}"),
                parent_session_id: parent_session_id.to_string(),
                agent_id: "agent-a".to_string(),
                label: "long job".to_string(),
                tool_name: "spawn_subtask".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: None,
                caller: CallerIdentity::Agent,
                lifecycle: SubtaskLifecycle::new(),
                steerable: false,
            },
        );
        handle
    }

    /// steer テスト用: `steerable` と `caller` を指定できる fake subtask を登録する（#647）。
    pub(crate) fn insert_fake_subtask_ex(
        registry: &SubtaskRegistry,
        subtask_id: &str,
        parent_session_id: &str,
        caller: CallerIdentity,
        steerable: bool,
    ) -> tokio::task::JoinHandle<()> {
        let handle = tokio::spawn(std::future::pending::<()>());
        registry.insert(
            subtask_id.to_string(),
            SpawnedSubtask {
                abort_handle: handle.abort_handle(),
                session_id: format!("subtask-{subtask_id}"),
                parent_session_id: parent_session_id.to_string(),
                agent_id: "agent-a".to_string(),
                label: "long job".to_string(),
                tool_name: "spawn_subtask".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: None,
                caller,
                lifecycle: SubtaskLifecycle::new(),
                steerable,
            },
        );
        handle
    }

    /// #647: 親セッションからの steer は steerable なサブへ届き、sub-session の履歴へ
    /// `log_type=steer` で記録される（通常発話と区別）。RUNNING のまま・除去しない。
    #[tokio::test]
    async fn steer_subtask_records_and_keeps_running() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let parent = "web-agent-a-conv1";
        let _handle =
            insert_fake_subtask_ex(&registry, "st-1", parent, CallerIdentity::Agent, true);

        let outcome = steer_subtask(
            &registry,
            &db,
            "st-1",
            "条件を1つ足して: 出力は JSON で",
            CallerIdentity::Agent,
            Some(parent),
        );
        assert_eq!(outcome, SteerOutcome::Accepted);
        // 状態機械は不変: RUNNING のまま registry に残る（cancel と違い除去しない）。
        assert!(registry.contains_key("st-1"), "steer 後も registry に残る");
        // sub-session の履歴に log_type=steer が 1 本落ちる。
        let conn = db.lock().unwrap();
        let logs =
            opencrab_db::queries::list_recent_session_logs(&conn, "subtask-st-1", 10).unwrap();
        let steer_logs: Vec<_> = logs
            .iter()
            .filter(|l| l.log_type == STEER_LOG_TYPE)
            .collect();
        assert_eq!(steer_logs.len(), 1, "steer が 1 本記録される");
        assert!(steer_logs[0].content.contains("JSON"), "本文が記録される");
    }

    /// #647 受け入れ条件 4: auto-dispatch（`steerable=false`）へは NotSteerable を返し、
    /// **黙って捨てない**。steer ログも書かない（読む主体がいないため）。
    #[tokio::test]
    async fn steer_subtask_auto_dispatch_is_not_steerable() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let parent = "web-agent-a-conv1";
        let _handle =
            insert_fake_subtask_ex(&registry, "st-ad", parent, CallerIdentity::Agent, false);

        let outcome = steer_subtask(
            &registry,
            &db,
            "st-ad",
            "方向を変えて",
            CallerIdentity::Agent,
            Some(parent),
        );
        assert_eq!(outcome, SteerOutcome::NotSteerable);
        let conn = db.lock().unwrap();
        let logs =
            opencrab_db::queries::list_recent_session_logs(&conn, "subtask-st-ad", 10).unwrap();
        assert!(
            !logs.iter().any(|l| l.log_type == STEER_LOG_TYPE),
            "NotSteerable のとき steer ログを書かない"
        );
    }

    /// #647: 認可は cancel と同じ。他セッションの Agent からは Unauthorized。
    #[tokio::test]
    async fn steer_subtask_foreign_session_unauthorized() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let _handle = insert_fake_subtask_ex(
            &registry,
            "st-f",
            "web-other-c9",
            CallerIdentity::Agent,
            true,
        );

        let outcome = steer_subtask(
            &registry,
            &db,
            "st-f",
            "x",
            CallerIdentity::Agent,
            Some("web-agent-a-conv1"),
        );
        assert_eq!(outcome, SteerOutcome::Unauthorized);
    }

    /// #647: owner はセッションをまたいで steer できる（cancel と同じ owner 等価規則）。
    #[tokio::test]
    async fn steer_subtask_owner_allowed_cross_session() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let _handle = insert_fake_subtask_ex(
            &registry,
            "st-o",
            "web-other-c9",
            CallerIdentity::Agent,
            true,
        );

        let outcome = steer_subtask(
            &registry,
            &db,
            "st-o",
            "追加指示",
            CallerIdentity::Owner,
            Some("some-other-session"),
        );
        assert_eq!(outcome, SteerOutcome::Accepted);
    }

    /// #647 受け入れ条件 3: registry に無く、sub-session が completed/cancelled のときは
    /// AlreadySettled を返す（決着済みへ送ったことが呼び出し側に分かる）。
    #[tokio::test]
    async fn steer_subtask_settled_session_is_already_settled() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        // registry には入れず、決着済みの sub-session 行だけを用意する。
        {
            let conn = db.lock().unwrap();
            let session = opencrab_db::queries::SessionRow {
                id: "subtask-st-done".to_string(),
                mode: "subtask".to_string(),
                theme: "Subtask: done".to_string(),
                phase: "active".to_string(),
                turn_number: 0,
                status: "completed".to_string(),
                participant_ids_json: "[\"agent-a\"]".to_string(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: None,
            };
            opencrab_db::queries::insert_session(&conn, &session).unwrap();
        }
        let outcome = steer_subtask(
            &registry,
            &db,
            "st-done",
            "遅れて届いた指示",
            CallerIdentity::Owner,
            Some("web-agent-a-conv1"),
        );
        assert_eq!(outcome, SteerOutcome::AlreadySettled);
    }

    /// #647: registry にも DB にも無い ID は NotFound。
    #[tokio::test]
    async fn steer_subtask_missing_is_not_found() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let outcome = steer_subtask(
            &registry,
            &db,
            "nope",
            "x",
            CallerIdentity::Owner,
            Some("web-agent-a-conv1"),
        );
        assert_eq!(outcome, SteerOutcome::NotFound);
    }

    /// #161: 親セッションからの cancel_subtask は abort + 除去し、親ログへ
    /// tool_cancelled を記録する。
    #[tokio::test]
    async fn cancel_subtask_parent_session_aborts_and_removes() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let parent = "web-agent-a-conv1";
        let handle = insert_fake_subtask(&registry, "st-1", parent);

        let outcome = cancel_subtask(
            &registry,
            &db,
            None,
            None,
            "st-1",
            CallerIdentity::Agent,
            Some(parent),
        );
        assert_eq!(outcome, CancelOutcome::Cancelled);
        assert!(registry.is_empty(), "cancel 後に registry から除去される");
        // 実際に abort された。
        assert!(handle.await.unwrap_err().is_cancelled());
        // 親セッションログに tool_cancelled が着地する。
        let conn = db.lock().unwrap();
        let logs = opencrab_db::queries::list_recent_session_logs(&conn, parent, 10).unwrap();
        assert!(
            logs.iter().any(|l| l.log_type == "tool_cancelled"),
            "親ログに tool_cancelled が記録される"
        );
    }

    /// #302: 停止通知も registry のエントリの呼び出し元をそのまま運ぶ。
    ///
    /// `on_subtask_cancelled` の既定実装は resume しないので現状は無害だが、sink が
    /// これを override した瞬間に「停止だけ最小権限へ降格する」が復活しうる。
    #[tokio::test]
    async fn cancel_subtask_carries_the_parent_caller_to_the_sink() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let parent = "web-agent-a-conv1";
        let _handle = insert_fake_subtask(&registry, "st-1", parent);
        // オーナー発のターンが spawn した状態にする。
        registry.get_mut("st-1").unwrap().caller = CallerIdentity::Owner;

        let sink = RecordingSink::default();
        // オーナー由来の subtask なので、停止できるのもオーナーのターン（#331）。
        let outcome = cancel_subtask(
            &registry,
            &db,
            Some(&sink),
            None,
            "st-1",
            CallerIdentity::Owner,
            Some(parent),
        );
        assert_eq!(outcome, CancelOutcome::Cancelled);

        let cancelled = sink.cancelled.lock().unwrap();
        assert_eq!(cancelled.len(), 1);
        assert_eq!(
            cancelled[0].caller,
            CallerIdentity::Owner,
            "停止通知が元ターンの呼び出し元を落としている"
        );
    }

    /// #161: 存在しない subtask_id は NotFound（権限拒否ではない）。
    #[tokio::test]
    async fn cancel_subtask_missing_is_not_found() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let outcome = cancel_subtask(
            &registry,
            &db,
            None,
            None,
            "nope",
            CallerIdentity::Agent,
            Some("web-a-c1"),
        );
        assert_eq!(outcome, CancelOutcome::NotFound);
    }

    /// #161: 他セッションが親の subtask は Unauthorized で拒否し、abort もしない。
    #[tokio::test]
    async fn cancel_subtask_foreign_session_unauthorized() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let handle = insert_fake_subtask(&registry, "st-x", "web-other-c9");

        let outcome = cancel_subtask(
            &registry,
            &db,
            None,
            None,
            "st-x",
            CallerIdentity::Agent,
            Some("web-me-c1"),
        );
        assert_eq!(outcome, CancelOutcome::Unauthorized);
        // 拒否したのでエントリは残り、abort もされない。
        assert!(registry.contains_key("st-x"));
        handle.abort(); // テスト後始末。
    }

    /// #161: session 文脈が無い agent は他人の subtask を停止できない（Unauthorized）。
    #[tokio::test]
    async fn cancel_subtask_no_session_unauthorized() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let handle = insert_fake_subtask(&registry, "st-ns", "web-other-c9");

        let outcome = cancel_subtask(
            &registry,
            &db,
            None,
            None,
            "st-ns",
            CallerIdentity::Agent,
            None,
        );
        assert_eq!(outcome, CancelOutcome::Unauthorized);
        assert!(registry.contains_key("st-ns"));
        handle.abort();
    }

    /// #161: owner は無関係なセッション文脈からでも停止できる。
    #[tokio::test]
    async fn cancel_subtask_owner_bypasses_session_check() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let handle = insert_fake_subtask(&registry, "st-any", "web-other-c9");

        let outcome = cancel_subtask(
            &registry,
            &db,
            None,
            None,
            "st-any",
            CallerIdentity::Owner,
            None,
        );
        assert_eq!(outcome, CancelOutcome::Cancelled);
        assert!(registry.is_empty());
        assert!(handle.await.unwrap_err().is_cancelled());
    }

    /// #331: セッションを 1 本にした（#323）結果、親セッションが一致していても、Owner 由来の
    /// subtask は非オーナー（caller=Agent）のターンからは停止できない。旧 per-相手 セッションでは
    /// 別セッションで構造的に不可能だった性質を caller で復元する。
    #[tokio::test]
    async fn cancel_subtask_non_owner_cannot_cancel_owner_spawned() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let parent = "nostr-agent-a"; // 1本化セッション（親＝呼び出し元セッション）。
        let handle = insert_fake_subtask(&registry, "st-owner", parent);
        // オーナー発のターンが spawn した subtask にする。
        registry.get_mut("st-owner").unwrap().caller = CallerIdentity::Owner;

        // 見知らぬ相手（caller=Agent）のターン。session は親と一致している。
        let outcome = cancel_subtask(
            &registry,
            &db,
            None,
            None,
            "st-owner",
            CallerIdentity::Agent,
            Some(parent),
        );
        assert_eq!(
            outcome,
            CancelOutcome::Unauthorized,
            "非オーナーは Owner 由来の subtask を止められない"
        );
        assert!(registry.contains_key("st-owner"), "拒否したので残る");
        handle.abort();
    }

    /// #331: 同じ状況でも Owner のターンからは従来どおり停止できる。
    #[tokio::test]
    async fn cancel_subtask_owner_can_cancel_owner_spawned() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let parent = "nostr-agent-a";
        let handle = insert_fake_subtask(&registry, "st-owner", parent);
        registry.get_mut("st-owner").unwrap().caller = CallerIdentity::Owner;

        let outcome = cancel_subtask(
            &registry,
            &db,
            None,
            None,
            "st-owner",
            CallerIdentity::Owner,
            Some(parent),
        );
        assert_eq!(outcome, CancelOutcome::Cancelled);
        assert!(handle.await.unwrap_err().is_cancelled());
    }

    /// #331: Agent 由来の subtask は従来どおり Agent のターンから止められる（正常系を壊さない）。
    #[tokio::test]
    async fn cancel_subtask_agent_can_cancel_agent_spawned() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let parent = "nostr-agent-a";
        // 既定の caller=Agent。
        let handle = insert_fake_subtask(&registry, "st-agent", parent);

        let outcome = cancel_subtask(
            &registry,
            &db,
            None,
            None,
            "st-agent",
            CallerIdentity::Agent,
            Some(parent),
        );
        assert_eq!(outcome, CancelOutcome::Cancelled);
        assert!(handle.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn registry_holds_spawned_subtask() {
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let handle = tokio::spawn(async {
            // 即完了せず abort_handle を有効に保つ。
            std::future::pending::<()>().await;
        })
        .abort_handle();

        let entry = SpawnedSubtask {
            abort_handle: handle,
            session_id: "sub-session-1".to_string(),
            parent_session_id: "discord-123".to_string(),
            agent_id: "agent-a".to_string(),
            label: "compile the report".to_string(),
            tool_name: "spawn_subtask".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: Some("channel:456".to_string()),
            caller: CallerIdentity::Agent,
            lifecycle: SubtaskLifecycle::new(),
            steerable: false,
        };
        registry.insert("sub-1".to_string(), entry);

        assert_eq!(registry.len(), 1);
        let got = registry.get("sub-1").unwrap();
        assert_eq!(got.parent_session_id, "discord-123");
        assert_eq!(got.reply_target.as_deref(), Some("channel:456"));

        // abort して registry から除去（cancel 相当）。
        got.abort_handle.abort();
        drop(got);
        registry.remove("sub-1");
        assert!(registry.is_empty());
    }
}
