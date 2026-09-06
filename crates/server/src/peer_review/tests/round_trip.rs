    // =======================================================================
    // 往復（#157 S7 の依頼側 → #156 S4 の回収側）を一気通貫で通す
    // =======================================================================
    //
    // 上のテストは往復の**部品**を片側ずつ固定している。継ぎ目は両側にリテラルを
    // 二重書きすることで揃えられていた（依頼が書く `[peer review requested] ...` を、
    // 回収側のテストが手で `insert_task_progress` して再現する）。稼働環境では
    // 部品が揃っていても往復が成立していなかった（#234 で権限の表記が食い違い名簿に
    // 載らず、`request_peer_review` の呼び出し実績 0 件）ので、**実際の呼び出し口の
    // 連なり**で通す:
    //
    //   登録（REST 入口 / #234 の表記） → 名簿（system prompt / #218）
    //   → `request_peer_review`（配送口は偽物） → 台帳の未回収記録
    //   → `AgentRuntime::on_inbound_message`（共通の受信フック） → 台帳の verdict
    //
    // ネットワークには出ない（[`FakeDelivery`]）。DB は in-memory（`test_app_state`）。
    mod round_trip {
        use super::*;
        use axum::extract::{Path, State};
        use axum::Json;
        use opencrab_actions::AgentRuntime;

        const AGENT: &str = "agent-a";
        const SESSION: &str = "sess-rt";
        const REVIEWER_ID: &str = "424242424242424242";
        const REVIEWER_NAME: &str = "Crab B";
        const CHANNEL: &str = "555000111";
        const REPLY: &str =
            "[Peer Review] score: 0.7\ngaps:\n- no tests were run\nsummary: plausible but unverified";

        /// 表示名つきで協働エージェントを登録する。
        ///
        /// **ダッシュボードと同じ REST 入口・同じ表記**（`co-agent`）を通す（#234）。
        /// `platform` は省略 = `discord`（回収側の受理ゲートと同じ経路 / #159）。
        async fn register_reviewer(state: &crate::AppState) {
            let _dto = crate::api::trusted_users::add_trusted_user(
                State(state.clone()),
                Path(AGENT.to_string()),
                Json(crate::api::trusted_users::AddTrustedUserRequest {
                    user_id: REVIEWER_ID.to_string(),
                    permission: Some("co-agent".to_string()),
                    display_name: Some(REVIEWER_NAME.to_string()),
                    platform: None,
                }),
            )
            .await
            .expect("co-agent の登録");
        }

        /// 依頼側のエージェントと active タスクを用意して task_id を返す。
        fn seed_agent_and_task(state: &crate::AppState) -> i64 {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::upsert_agent(
                &conn,
                &opencrab_db::queries::AgentRow {
                    agent_id: AGENT.to_string(),
                    name: "Crab A".to_string(),
                    job_title: None,
                    organization: None,
                    image_url: None,
                    persona_name: "crab".to_string(),
                    personality: None,
                    instructions: String::new(),
                    heartbeat_instructions: String::new(),
                    model: None,
                    reasoning_effort: None,
                    web_search: None,
                    metadata_json: None,
                },
            )
            .unwrap();
            opencrab_db::queries::insert_task_ledger(
                &conn,
                AGENT,
                SESSION,
                "往復を通す",
                Some("台帳に verdict が載ること"),
            )
            .unwrap()
        }

        fn ledger(state: &crate::AppState, task_id: i64) -> Vec<String> {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::list_recent_task_progress(&conn, task_id, 30)
                .unwrap()
                .into_iter()
                .map(|p| p.content)
                .collect()
        }

        /// 受信 1 件（各ゲートウェイの受信ループが共通フックへ渡すのと同じ形）。
        fn inbound<'a>(sender_id: &'a str, text: &'a str) -> InboundMessageRecord<'a> {
            InboundMessageRecord {
                session_id: SESSION,
                recipient_agent_id: AGENT,
                sender_id,
                sender_name: REVIEWER_NAME,
                avatar_url: None,
                channel_id: Some(CHANNEL),
                pubkey: None,
                text,
                image_urls: &[],
            }
        }

        /// 依頼を出すところまで進めて `(state, task_id, 配送口)` を返す。
        async fn request_sent() -> (crate::AppState, i64, FakeDelivery) {
            let state = crate::test_app_state();
            register_reviewer(&state).await;
            let task_id = seed_agent_and_task(&state);
            let delivery = FakeDelivery::new();
            let ctx = GatewayCallContext::new(GatewayCaller::Agent, AGENT)
                .with_session_id(SESSION)
                .with_reply_target(Some(CHANNEL.to_string()));
            let r = request_peer_review(
                &state.db,
                &delivery,
                // 宛先は渡さない（実行文脈の返信先を使う既定の呼び方 / #158 S1）。
                // reviewer は**名簿に出た表示名**をそのまま渡す（#218）。
                &json!({"content": "fn main() {}", "reviewer": REVIEWER_NAME}),
                &ctx,
            )
            .await;
            assert!(r.success, "依頼が失敗した: {:?}", r.error);
            (state, task_id, delivery)
        }

        /// **往復の本線**。登録 → 名簿 → 依頼 → 配送 → 未回収記録 → 受信フック → 台帳。
        #[tokio::test]
        async fn registration_to_roster_to_request_to_harvest() {
            let state = crate::test_app_state();
            register_reviewer(&state).await;
            let task_id = seed_agent_and_task(&state);

            // (2) #920: 名簿は Peer Review 節ごと共有プロンプトから撤去された。表示名も ID も
            //     プロンプトには出ない（reviewer 解決は list_co_agent_reviewers 経由で継続する
            //     ため、依頼自体は下の (3) で reviewer=表示名 を渡して成立する）。
            let prompt = {
                let conn = state.db.lock().unwrap();
                crate::process::build_agent_context(
                    &conn,
                    AGENT,
                    &opencrab_actions::CallerIdentity::Owner,
                )
                .0
            };
            assert!(
                !prompt.contains(&format!("- {REVIEWER_NAME}")),
                "#920: 名簿は共有プロンプトから撤去済み（表示名が残っている）:\n{prompt}"
            );
            assert!(
                !prompt.contains(REVIEWER_ID),
                "共有プロンプトに transport の識別子を出さない（#218）:\n{prompt}"
            );

            // (3) 依頼を実行 → 配送口に渡ったものを検分する
            let delivery = FakeDelivery::new();
            let ctx = GatewayCallContext::new(GatewayCaller::Agent, AGENT)
                .with_session_id(SESSION)
                .with_reply_target(Some(CHANNEL.to_string()));
            let r = request_peer_review(
                &state.db,
                &delivery,
                &json!({
                    "content": "fn main() {}",
                    "reviewer": REVIEWER_NAME,
                    "instructions": "エラー処理を見て"
                }),
                &ctx,
            )
            .await;
            assert!(r.success, "{:?}", r.error);
            let sent = delivery.sent.lock().unwrap().clone();
            assert_eq!(sent.len(), 2, "ヘッダ + part 1/1");
            assert_eq!(sent[0].0, CHANNEL, "宛先は実行文脈の返信先");
            // 目印は**行頭**（回収側の `find_reply_marker` と同じ前提）
            assert!(
                sent[0].1.starts_with(PEER_REVIEW_REQUEST_MARKER),
                "{}",
                sent[0].1
            );
            // メンションは transport の記法で、解決済みの登録済み識別子
            assert!(
                sent[0].1.contains(&format!("<@{REVIEWER_ID}>")),
                "指名レビュアーのメンションが無い: {}",
                sent[0].1
            );
            assert!(sent[0].1.contains("from Crab A — task #"));
            assert!(sent[0].1.contains("instructions: エラー処理を見て"));
            // 本文は要約せず RAW のまま
            assert_eq!(sent[1].1, "part 1/1\nfn main() {}");

            // (4) 未回収として台帳に記録される
            let before = ledger(&state, task_id);
            assert_eq!(before.len(), 1, "{before:?}");
            assert!(
                before[0].starts_with("[peer review requested]"),
                "{}",
                before[0]
            );

            // (5) レビュアーの返信を**共通の受信フック経由**で流す
            state.on_inbound_message(
                TranscriptSource::Discord,
                AGENT,
                &inbound(REVIEWER_ID, REPLY),
            );

            // (6) 台帳に verdict が載る
            let after = ledger(&state, task_id);
            assert_eq!(after.len(), 2, "回収されていない: {after:?}");
            assert!(
                after[1].starts_with(&format!("[peer review] score 0.70 (from {REVIEWER_NAME})")),
                "{}",
                after[1]
            );
            assert!(after[1].contains("no tests were run"));
            assert!(after[1].contains("plausible but unverified"));

            // 1 依頼 1 記録: 同じ返信をもう一度流しても増えない
            state.on_inbound_message(
                TranscriptSource::Discord,
                AGENT,
                &inbound(REVIEWER_ID, REPLY),
            );
            assert_eq!(ledger(&state, task_id).len(), 2, "2 件目が記録された");
        }

        /// ゲート1: 目印が**行頭に無い**返信は回収しない。
        #[tokio::test]
        async fn gate_marker_must_start_a_line() {
            let (state, task_id, _d) = request_sent().await;
            state.on_inbound_message(
                TranscriptSource::Discord,
                AGENT,
                &inbound(
                    REVIEWER_ID,
                    "the diff mentions [Peer Review] score: 0.9 in prose",
                ),
            );
            let l = ledger(&state, task_id);
            assert_eq!(l.len(), 1, "行頭でない目印を回収した: {l:?}");
        }

        /// ゲート1 の裏: **依頼メッセージ自身**が受信フックを通っても回収されない。
        ///
        /// 依頼と回収が同じチャンネルに流れる以上、投稿した依頼が自分の受信ループへ
        /// 戻ってくる経路はありうる。`[Peer Review Request]` が `[Peer Review]` で
        /// 始まってしまうと、依頼した瞬間に verdict が捏造される。
        #[tokio::test]
        async fn gate_own_request_is_not_harvested_as_a_reply() {
            let (state, task_id, delivery) = request_sent().await;
            let header = delivery.sent.lock().unwrap()[0].1.clone();
            state.on_inbound_message(
                TranscriptSource::Discord,
                AGENT,
                &inbound(REVIEWER_ID, &header),
            );
            let l = ledger(&state, task_id);
            assert_eq!(l.len(), 1, "依頼ヘッダが返信として回収された: {l:?}");
        }

        /// ゲート2: 送信者が**登録済みの協働エージェント**でなければ回収しない。
        #[tokio::test]
        async fn gate_sender_must_be_a_registered_co_agent() {
            let (state, task_id, _d) = request_sent().await;
            state.on_inbound_message(
                TranscriptSource::Discord,
                AGENT,
                &inbound("999999999999999999", REPLY),
            );
            assert_eq!(
                ledger(&state, task_id).len(),
                1,
                "未登録の送信者の verdict を受理した"
            );

            // 登録はされているが permission が協働エージェントでなければ同じく落ちる
            // （#234 が壊れたときの形 — 稼働環境で実際に起きていた不具合）。
            //
            // **未回収状態は依頼側を実際に通して作る。** ここで台帳の文言を手書きすると、
            // 依頼側の文言が将来ずれたときにこの検査は「未回収の依頼が無い」ゲートで
            // 落ちて素通りし、権限の検査が黙って消える（この PR が批判している
            // 二重書きそのもの）。
            let (state2, task2, _d2) = request_sent().await;
            let row_id = crate::api::trusted_users::list_trusted_users(
                State(state2.clone()),
                Path(AGENT.to_string()),
            )
            .await
            .0
            .into_iter()
            .find(|u| u.user_id == REVIEWER_ID)
            .expect("登録済みのレビュアー")
            .id;
            let _updated = crate::api::trusted_users::update_trusted_user(
                State(state2.clone()),
                Path((AGENT.to_string(), row_id)),
                Json(crate::api::trusted_users::UpdateTrustedUserRequest {
                    permission: Some("user".to_string()),
                    display_name: None,
                }),
            )
            .await
            .expect("協働エージェントからの降格");

            // 依頼は未回収のまま（= 権限以外のゲートは開いている）
            let before = ledger(&state2, task2);
            assert_eq!(before.len(), 1, "{before:?}");
            assert!(
                before[0].starts_with("[peer review requested]"),
                "権限ゲート以外が閉じていると、この検査は空虚になる: {}",
                before[0]
            );

            state2.on_inbound_message(
                TranscriptSource::Discord,
                AGENT,
                &inbound(REVIEWER_ID, REPLY),
            );
            assert_eq!(
                ledger(&state2, task2).len(),
                1,
                "co-agent でない登録者の verdict を受理した"
            );
        }

        /// ゲート3: **未回収の依頼が無い**タスクには記録しない。
        ///
        /// 依頼を出さずに（= 台帳に `[peer review requested]` が無い状態で）返信だけ流す。
        /// 同じチャンネルで進む第三者間のレビューを自分の台帳へ書き込まないための門。
        #[tokio::test]
        async fn gate_requires_an_outstanding_request() {
            let state = crate::test_app_state();
            register_reviewer(&state).await;
            let task_id = seed_agent_and_task(&state);
            state.on_inbound_message(
                TranscriptSource::Discord,
                AGENT,
                &inbound(REVIEWER_ID, REPLY),
            );
            let l = ledger(&state, task_id);
            assert!(
                l.is_empty(),
                "依頼していないレビューを台帳へ書き込んだ: {l:?}"
            );
        }

        /// ゲート3 の別の崩し方: active タスクの無いセッションでは記録しない。
        #[tokio::test]
        async fn gate_requires_an_active_task_in_the_session() {
            let (state, task_id, _d) = request_sent().await;
            let other = InboundMessageRecord {
                session_id: "sess-other",
                ..inbound(REVIEWER_ID, REPLY)
            };
            state.on_inbound_message(TranscriptSource::Discord, AGENT, &other);
            assert_eq!(
                ledger(&state, task_id).len(),
                1,
                "別セッションの返信を回収した"
            );
        }
    }
