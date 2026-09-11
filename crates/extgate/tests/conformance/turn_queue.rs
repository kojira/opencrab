
fn session_log_count(h: &Harness, session_id: &str) -> i64 {
    let conn = h.state.db.lock().unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
        [session_id],
        |r| r.get(0),
    )
    .unwrap()
}

/// turn 実行中に届いた said が消えず、turn 終了後に処理される。
#[tokio::test(start_paused = true)]
async fn said_during_turn_is_recorded_and_runs_after() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-1").await;
    let session_id = format!("extgate-{binding_id}");
    let (release_tx, release_rx) = oneshot::channel();
    *h.runtime.hold_rx.lock().unwrap() = Some(release_rx);

    let client = InstanceClient::connect(&h.sock, instance_id, 1, "u1".into(), config_digest())
        .await
        .expect("connect");
    wait_client_bound(&client, "chan-1", &binding_id).await;

    let entered = h.runtime.turn_entered.notified();
    tokio::pin!(entered);
    let first = client
        .post_said("chan-1", "origin-1", "first", &[])
        .await
        .unwrap_or_else(|e| panic!("first refuse {e:?}"));
    assert!(
        matches!(first, SaidOutcome::Accepted { seq: 1 }),
        "{first:?}"
    );
    for _ in 0..80 {
        tokio::select! {
            _ = &mut entered => break,
            _ = async {
                tokio::time::advance(Duration::from_millis(5)).await;
            } => {}
        }
    }

    let second = client
        .post_said("chan-1", "origin-2", "second-during-turn", &[])
        .await
        .unwrap_or_else(|e| panic!("second refuse {e:?}"));
    assert!(
        matches!(second, SaidOutcome::Accepted { seq: 2 }),
        "said during turn must be accepted, got {second:?}"
    );
    assert_eq!(session_log_count(&h, &session_id), 2);
    assert_eq!(
        h.runtime.turns.load(Ordering::SeqCst),
        0,
        "second turn waits"
    );

    release_tx.send(()).unwrap();
    for _ in 0..80 {
        if h.runtime.turns.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::advance(Duration::from_millis(20)).await;
    }
    assert_eq!(
        h.runtime.turns.load(Ordering::SeqCst),
        2,
        "queued said runs after the held turn"
    );
}

/// row318 / §9A 汎化の回帰固定: 非 nostr（ここでは discord）kind の said も session log の
/// metadata に `external_origin` を記録し、汎用 `ConversationRefs` が platform 非依存に e番号 /
/// u番号 を採番できる。旧実装は `record_inbound` が `kind_id == "nostr"` でだけ external_origin を
/// 書いており、Discord 等に e番号が一切付かなかった（reply/reaction が e番号を解決できない）。
/// その汎用機構への platform 名漏れ（DI 違反）を剥がした変更を固定する。
#[tokio::test(start_paused = true)]
async fn non_nostr_said_records_external_origin_for_e_numbering() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    // put_instance は kind_id = "discord"（= 非 nostr）で登録する。
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-di").await;
    let session_id = format!("extgate-{binding_id}");

    let client = InstanceClient::connect(&h.sock, instance_id, 1, "u1".into(), config_digest())
        .await
        .expect("connect");
    wait_client_bound(&client, "chan-di", &binding_id).await;

    let origin = "discord:message:v1:100:200";
    let out = client
        .post_said("chan-di", origin, "こんにちは", &[])
        .await
        .unwrap_or_else(|e| panic!("said refuse {e:?}"));
    assert!(matches!(out, SaidOutcome::Accepted { seq: 1 }), "{out:?}");

    // 1) discord kind でも session log の metadata に external_origin が入る（回帰固定）。
    let logs = {
        let conn = h.state.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap()
    };
    let speech = logs
        .iter()
        .find(|l| l.log_type == "speech" && l.speaker_id.as_deref() == Some("u1"))
        .expect("inbound speech log");
    let meta: serde_json::Value =
        serde_json::from_str(speech.metadata_json.as_deref().expect("metadata_json")).unwrap();
    assert_eq!(
        meta["external_origin"], origin,
        "非 nostr kind で external_origin が未記録: {meta}"
    );

    // 2) 汎用採番（core conversation.rs）が platform 非依存に e/u 番号を割り当てる（§9A）。
    let refs = opencrab_core::conversation::ConversationRefs::build(&logs, "the-bot-agent");
    assert_eq!(
        refs.resolve_short_ref("e1").as_deref(),
        Some(origin),
        "e1 が origin へ解決できない（e番号未採番）"
    );
    assert_eq!(
        refs.resolve_short_ref("u1").as_deref(),
        Some("u1"),
        "u1 が話者へ解決できない"
    );
}

/// キュー満杯は seq=null で拒否し、履歴に残さない。
#[tokio::test]
async fn session_queue_overflow_is_seq_null_and_counted() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = format!("extgate-{binding_id}");
    let (release_tx, release_rx) = oneshot::channel();
    *h.runtime.hold_rx.lock().unwrap() = Some(release_rx);

    for i in 0..32 {
        let id = format!("s{i}");
        write_frame(
            &mut s,
            &json!({
                "id": id,
                "m": "said",
                "binding_id": binding_id,
                "origin": format!("o-{i}"),
                "author_id": "u1",
                "text": format!("m{i}"),
                "attachments": []
            }),
        )
        .await;
        let v = read_said_response(&mut s, &id).await;
        assert_eq!(v["m"], "ok", "said {i} {v}");
        assert_eq!(v["seq"], i + 1, "said {i} {v}");
    }
    write_frame(
        &mut s,
        &json!({
            "id": "sover",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o-overflow",
            "author_id": "u1",
            "text": "too-many",
            "attachments": []
        }),
    )
    .await;
    let overflow = read_said_response(&mut s, "sover").await;
    assert_eq!(overflow["m"], "ok");
    assert!(overflow["seq"].is_null(), "{overflow}");
    assert_eq!(session_log_count(&h, &session_id), 32);
    assert!(h.state.turn_queues.dropped() >= 1);
    assert!(h.state.probe.turn_queue_dropped.load(Ordering::SeqCst) >= 1);
    let _ = release_tx.send(());
}

async fn wait_turns(h: &Harness, n: usize) {
    let got = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if h.runtime.turns.load(Ordering::SeqCst) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        got.is_ok(),
        "expected {n} turns, got {}",
        h.runtime.turns.load(Ordering::SeqCst)
    );
}

/// 遅いツール相当（`tool_hold`）を解放しなくても turn が終わる。
/// 修正前は sink 無し → `tool_hold` を待つのでこのテストは赤。
#[tokio::test]
async fn v3_turn_returns_before_slow_tool_finishes() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let (_tool_tx, tool_rx) = oneshot::channel();
    *h.runtime.tool_hold_rx.lock().unwrap() = Some(tool_rx);

    write_frame(
        &mut s,
        &json!({
            "id": "slow1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o-slow",
            "author_id": "u1",
            "text": "sleep 120 を実行して",
            "attachments": []
        }),
    )
    .await;
    let v = read_said_response(&mut s, "slow1").await;
    assert_eq!(v["m"], "ok", "{v}");

    wait_turns(&h, 1).await;
    assert!(
        h.runtime.sink_seen.load(Ordering::SeqCst),
        "V3 RunRequest に completion_sink が付いていること"
    );
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 1);
}

/// ツール実行中（`tool_hold` 未解放）に届いた said が次の turn を起こせる。
#[tokio::test]
async fn said_during_detached_tool_starts_next_turn() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let (_tool_tx, tool_rx) = oneshot::channel();
    *h.runtime.tool_hold_rx.lock().unwrap() = Some(tool_rx);

    write_frame(
        &mut s,
        &json!({
            "id": "d1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o-1",
            "author_id": "u1",
            "text": "sleep 120",
            "attachments": []
        }),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "d1").await["m"], "ok");
    wait_turns(&h, 1).await;

    write_frame(
        &mut s,
        &json!({
            "id": "d2",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o-2",
            "author_id": "u1",
            "text": "追いメンション",
            "attachments": []
        }),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "d2").await["m"], "ok");
    wait_turns(&h, 2).await;
    assert_eq!(
        h.runtime.turns.load(Ordering::SeqCst),
        2,
        "ツール完了を待たずに次 turn が走ること"
    );
}

/// 決着本文は DB に着地したあと、resume の 1 turn で会話に載る。
#[tokio::test]
async fn settlement_is_consumed_on_next_turn() {
    let h = Harness::start().await;
    let (mut s, instance_id, binding_id) = ready_pair(&h).await;
    let session_id = format!("extgate-{binding_id}");

    write_frame(
        &mut s,
        &json!({
            "id": "c1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o-1",
            "author_id": "u1",
            "text": "sleep 120",
            "attachments": []
        }),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "c1").await["m"], "ok");
    wait_turns(&h, 1).await;

    let sink = ExtgateCompletionSink {
        state: Arc::clone(&h.state),
        runtime: h.runtime.clone(),
        instance_id,
        binding_id: binding_id.clone(),
        agent_id: "agent-1".into(),
        session_id: session_id.clone(),
        kind_id: "discord".into(),
        author_id: "u1".into(),
        delivery_mode: DeliveryMode::Say,
        prompt_suffix: String::new(),
    };
    settle_completed(
        &h.runtime.subtask_registry_for(&session_id),
        &h.state.db,
        &sink,
        SettleContext {
            parent_session_id: session_id.clone(),
            agent_id: "agent-1".into(),
            subtask_id: "st-sleep".into(),
            sub_session_id: String::new(),
            exit_reason: "completed".into(),
            lifecycle: SubtaskLifecycle::new(),
        },
        r#"{"ok":true,"slept":120}"#,
    );
    wait_turns(&h, 2).await;

    let resume_conv = h
        .runtime
        .conversations
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap_or_default();
    assert!(
        resume_conv.contains("subtask_completed") && resume_conv.contains("slept"),
        "決着結果が次 turn の会話に載ること: {resume_conv}"
    );
}

/// **#838 row284 の穴を塞ぐ本命**。決着を fake せず `settle_completed`（＝実経路の
/// `dispatch_settled`）を通し、session_id が **`extgate-` 接頭辞でない再利用セッション**
/// （Nostr は `canonical_session_id` が address = `nostr-<agent_id>` へフォールバックする）
/// でも `ExtgateCompletionSink::deliver_continuation` → resume turn が起きることを検証する。
///
/// 旧実装は `dispatch_settled` が `ev.session_id.starts_with("extgate-")` で親判定していたため、
/// `nostr-agent-1` は門前払いされ resume が一切起きなかった（このテストは turns==0 のまま落ちる）。
/// 既存の `settlement_is_consumed_on_next_turn` は `extgate-{binding_id}` を使うため接頭辞判定を
/// 素通りし、この穴を踏めていなかった。
#[tokio::test]
async fn settlement_on_reused_nostr_session_resumes() {
    let h = Harness::start().await;
    // 連結済みの instance/binding を用意する（Say 配送の送出先）。ただし決着させる親
    // セッションは binding の canonical（extgate-…）ではなく、Nostr 再利用の address 形式。
    let (_s, instance_id, binding_id) = ready_pair(&h).await;
    let session_id = "nostr-agent-1".to_string();
    assert!(
        !session_id.starts_with("extgate-"),
        "テスト前提: session が extgate- 接頭辞でないこと"
    );

    let sink = ExtgateCompletionSink {
        state: Arc::clone(&h.state),
        runtime: h.runtime.clone(),
        instance_id,
        binding_id,
        agent_id: "agent-1".into(),
        session_id: session_id.clone(),
        kind_id: "nostr".into(),
        author_id: "npub-u1".into(),
        delivery_mode: DeliveryMode::Say,
        prompt_suffix: String::new(),
    };
    settle_completed(
        &h.runtime.subtask_registry_for(&session_id),
        &h.state.db,
        &sink,
        SettleContext {
            parent_session_id: session_id.clone(),
            agent_id: "agent-1".into(),
            subtask_id: "st-sleep".into(),
            sub_session_id: String::new(),
            exit_reason: "completed".into(),
            lifecycle: SubtaskLifecycle::new(),
        },
        r#"{"ok":true,"slept":30}"#,
    );

    // resume turn が実際に走ること（旧実装なら guard に阻まれ turns は 0 のまま）。
    wait_turns(&h, 1).await;
    assert_eq!(
        h.runtime.turns.load(Ordering::SeqCst),
        1,
        "nostr- 再利用セッションの決着でも resume turn が 1 回走ること"
    );
    let resume_conv = h
        .runtime
        .conversations
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap_or_default();
    assert!(
        resume_conv.contains("subtask_completed") && resume_conv.contains("slept"),
        "決着結果が resume turn の会話に載ること: {resume_conv}"
    );
}

/// 親ターンが実行中なら、保存済みcompletionは親のiterationへ委ね、別resumeを待機させない。
#[tokio::test]
async fn settlement_during_active_parent_does_not_start_another_resume() {
    let h = Harness::start().await;
    let (_s, instance_id, binding_id) = ready_pair(&h).await;
    let session_id = format!("extgate-{binding_id}");
    let sink = ExtgateCompletionSink {
        state: Arc::clone(&h.state),
        runtime: h.runtime.clone(),
        instance_id,
        binding_id,
        agent_id: "agent-1".into(),
        session_id: session_id.clone(),
        kind_id: "web".into(),
        author_id: "user-1".into(),
        delivery_mode: DeliveryMode::Say,
        prompt_suffix: String::new(),
    };

    let locks = h.runtime.session_locks();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let active_session = session_id.clone();
    let active = tokio::spawn(async move {
        locks
            .run_serialized(&active_session, async move {
                entered_tx.send(()).unwrap();
                release_rx.await.unwrap();
            })
            .await;
    });
    entered_rx.await.unwrap();

    settle_completed(
        &h.runtime.subtask_registry_for(&session_id),
        &h.state.db,
        &sink,
        SettleContext {
            parent_session_id: session_id.clone(),
            agent_id: "agent-1".into(),
            subtask_id: "st-active".into(),
            sub_session_id: String::new(),
            exit_reason: "completed".into(),
            lifecycle: SubtaskLifecycle::new(),
        },
        "active-parent-result",
    );

    release_tx.send(()).unwrap();
    active.await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        h.runtime.turns.load(Ordering::SeqCst),
        0,
        "実行中の親の後ろへ別resume turnを待機させない"
    );
    let logs = {
        let conn = h.state.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap()
    };
    assert!(
        logs.iter().any(|log| log.content.contains("active-parent-result")),
        "completion結果自体は親sessionへ保存する"
    );
}

