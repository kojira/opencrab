#[tokio::test]
async fn noreply_empty_failed_make_zero_say() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    *h.runtime.reply.lock().unwrap() = "NO_REPLY".into();
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "nr",
            "author_id": "u1",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let _ = read_frame(&mut s).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let conn = h.state.db.lock().unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM deliveries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn completion_response_outbox_applies_only_after_delivery_ack() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);
    {
        let conn = h.state.db.lock().unwrap();
        let result_log_id = opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "agent-1".into(),
                session_id: session_id.clone(),
                log_type: "tool_result".into(),
                content: "done".into(),
                speaker_id: None,
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
        opencrab_db::queries::enqueue_tool_completion_event(
            &conn,
            &opencrab_db::queries::NewToolCompletionEvent {
                event_id: "effect-event",
                session_id: &session_id,
                causal_turn_id: "turn",
                tool_call_id: "t1",
                execution_id: "exec",
                result_log_id,
                completed_at: "2026-01-01T00:00:00Z",
            },
        )
        .unwrap();
        let ids = vec!["effect-event".to_string()];
        opencrab_db::queries::mark_tool_completion_events_included(
            &conn,
            &ids,
            "effect-request",
            "digest",
            r#"{"model":"model","messages":[]}"#,
        )
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        opencrab_db::queries::record_tool_completion_response_in_transaction(
            &tx,
            "effect-request",
            r#"{"choices":[]}"#,
        )
        .unwrap();
        opencrab_db::queries::mark_tool_completion_events_consumed_in_transaction(
            &tx,
            &ids,
            "effect-request",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    write_frame(
        &mut s,
        &json!({
            "id": "s-effect", "m": "said", "binding_id": binding_id,
            "origin": "effect-origin", "author_id": "u1", "text": "hi", "attachments": []
        }),
    )
    .await;
    let _ = read_frame(&mut s).await;
    let say = loop {
        let frame = read_frame(&mut s).await;
        if frame["m"] == "say" {
            break frame;
        }
    };
    assert!(say["id"]
        .as_str()
        .unwrap()
        .starts_with("effect-request:"));
    assert!(opencrab_db::queries::load_pending_tool_completion_effect(
        &h.state.db.lock().unwrap(),
        &session_id,
    )
    .unwrap()
    .is_some());
    write_frame(&mut s, &json!({"id": say["id"], "m": "ok"})).await;
    for _ in 0..50 {
        if opencrab_db::queries::load_pending_tool_completion_effect(
            &h.state.db.lock().unwrap(),
            &session_id,
        )
        .unwrap()
        .is_none()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("delivery ACK did not apply completion outbox");
}

// 裁定A（2026-08-31）: 決着（say/no_reply）を配送した**後**に activity ended を出す。
// 返信ターンでは say フレームが ended より先に届き、gate-client は saw_say=true を見てから
// ended を処理するので、返信ターンで偽 CompletedNoReply が立たない（＝Discord の偽 🤐 撤去）。
#[tokio::test]
async fn reply_turn_emits_say_before_ended() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "d1",
            "author_id": "u1",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let _ = read_frame(&mut s).await; // said ok
    let mut order: Vec<&str> = Vec::new();
    for _ in 0..80 {
        if let Some(v) = read_frame_opt(&mut s).await {
            match v["m"].as_str() {
                Some("say") => {
                    order.push("say");
                    write_frame(&mut s, &json!({"id": v["id"], "m": "ok"})).await;
                }
                Some("activity") if v["state"] == "ended" => order.push("ended"),
                _ => {}
            }
        }
        if order.contains(&"say") && order.contains(&"ended") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let say_idx = order.iter().position(|x| *x == "say").expect("say frame");
    let ended_idx = order
        .iter()
        .position(|x| *x == "ended")
        .expect("ended frame");
    assert!(
        say_idx < ended_idx,
        "say は ended より先に届く（裁定A）: {order:?}"
    );
}

// 沈黙（NO_REPLY）ターンは ended を出すが say は出さない。gate-client はこの ended で
// saw_say=false を見て CompletedNoReply を正しく立てる（＝真の沈黙にだけ 🤐）。
#[tokio::test]
async fn no_reply_turn_emits_ended_without_say() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    *h.runtime.reply.lock().unwrap() = "NO_REPLY".into();
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "nr",
            "author_id": "u1",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let _ = read_frame(&mut s).await; // said ok
    let mut saw_say = false;
    let mut saw_ended = false;
    for _ in 0..80 {
        if let Some(v) = read_frame_opt(&mut s).await {
            match v["m"].as_str() {
                Some("say") => saw_say = true,
                Some("activity") if v["state"] == "ended" => saw_ended = true,
                _ => {}
            }
        }
        if saw_ended {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(saw_ended, "沈黙ターンでも activity ended は出る");
    assert!(!saw_say, "沈黙（NO_REPLY）ターンで say は出ない");
}

#[tokio::test]
async fn turn_failed_emits_frame_with_origin() {
    // R3(❌): エンジン/プロバイダ失敗（DeliveryEffect::Failed）で core→gate に turn_failed(origin)
    // が届く（gateway が発端メッセージへ ❌ を付ける材料）。error 本文は wire に載らず、say は 0。
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    *h.runtime.reply.lock().unwrap() = "__FAIL__".into();
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "boom-1",
            "author_id": "u1",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let ok = read_frame(&mut s).await;
    assert_eq!(ok["seq"], 1);
    let mut turn_failed: Option<Value> = None;
    let mut saw_say = false;
    for _ in 0..80 {
        if let Some(v) = read_frame_opt(&mut s).await {
            match v["m"].as_str() {
                Some("turn_failed") => turn_failed = Some(v),
                Some("say") => saw_say = true,
                _ => {}
            }
        }
        if turn_failed.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let tf = turn_failed.expect("turn_failed frame must be emitted on turn failure");
    assert_eq!(tf["origin"], "boom-1");
    assert_eq!(tf["binding_id"], binding_id);
    // error 本文は wire に載せない（多エージェント相互反応ループ防止・#668）。
    assert!(tf.get("error").is_none() && tf.get("detail").is_none());
    assert!(!saw_say);
    let conn = h.state.db.lock().unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM deliveries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "失敗ターンは say を出さない");
}

#[tokio::test]
async fn e2e_omoikane_flow() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "omoikane").await;
    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    let acked = ack_bind(&mut s).await;
    assert_eq!(acked, binding_id);
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "omo-1",
            "author_id": "user-1",
            "text": "hello",
            "attachments": []
        }),
    )
    .await;
    let ok = read_frame(&mut s).await;
    assert_eq!(ok["seq"], 1);
    let mut saw_started = false;
    let mut saw_ended = false;
    let mut saw_say = false;
    // #964: started は origin を持たず、LLM call 直前の read が発端 origin を運ぶ。
    let mut started_had_origin = false;
    let mut read_origin: Option<String> = None;
    let mut activity_order = Vec::new();
    let mut ended_had_origin = false;
    for _ in 0..80 {
        if let Some(v) = read_frame_opt(&mut s).await {
            match v["m"].as_str() {
                Some("activity") if v["state"] == "started" => {
                    saw_started = true;
                    started_had_origin = !v["origin"].is_null();
                    activity_order.push("started");
                }
                Some("activity") if v["state"] == "read" => {
                    read_origin = v["origin"].as_str().map(str::to_string);
                    activity_order.push("read");
                }
                Some("activity") if v["state"] == "ended" => {
                    saw_ended = true;
                    ended_had_origin = !v["origin"].is_null();
                }
                Some("say") => {
                    assert_eq!(v["payload"]["text"], "hello from agent");
                    write_frame(&mut s, &json!({"id": v["id"], "m": "ok"})).await;
                    saw_say = true;
                }
                _ => {}
            }
        }
        if saw_started && read_origin.is_some() && saw_ended && saw_say {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(saw_started && saw_ended && saw_say);
    assert!(!started_had_origin, "started は origin を持たない");
    assert_eq!(read_origin.as_deref(), Some("omo-1"));
    assert_eq!(activity_order.first(), Some(&"started"));
    assert_eq!(activity_order.get(1), Some(&"read"));
    assert!(!ended_had_origin);
}

