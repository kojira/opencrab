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
    let mut ended = None;
    for _ in 0..80 {
        if let Some(v) = read_frame_opt(&mut s).await {
            match v["m"].as_str() {
                Some("say") => saw_say = true,
                Some("activity") if v["state"] == "ended" => ended = Some(v),
                _ => {}
            }
        }
        if ended.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let ended = ended.expect("沈黙ターンでも activity ended は出る");
    assert_eq!(ended["silent_origins"], json!(["nr"]));
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
    let mut saw_ended = false;
    for _ in 0..80 {
        if let Some(v) = read_frame_opt(&mut s).await {
            match v["m"].as_str() {
                Some("turn_failed") => turn_failed = Some(v),
                Some("activity") if v["state"] == "ended" => saw_ended = true,
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
    assert!(!saw_ended, "engine Err must not emit authoritative ended");
    if let Some(v) = read_frame_opt(&mut s).await {
        assert_ne!(v["state"], "ended", "ended must not follow turn_failed");
    }
    let conn = h.state.db.lock().unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM deliveries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "失敗ターンは say を出さない");
}

#[tokio::test]
async fn budget_failure_none_emits_no_authoritative_ended() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    h.runtime.budget_fails.store(true, Ordering::SeqCst);
    write_frame(
        &mut s,
        &json!({
            "id": "none-1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "none-origin",
            "author_id": "u1",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "none-1").await["m"], "ok");

    while let Some(v) = read_frame_opt(&mut s).await {
        assert!(
            !(v["m"] == "activity" && v["state"] == "ended"),
            "engine None must not emit authoritative ended: {v}"
        );
    }
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn visible_a_and_folded_b_share_ended_with_completion_and_only_b_silent() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    *h.runtime.reply.lock().unwrap() = "__VISIBLE_A_SILENT_FOLDED__".into();
    let (release_tx, release_rx) = oneshot::channel();
    *h.runtime.hold_rx.lock().unwrap() = Some(release_rx);
    let entered = h.runtime.turn_entered.notified();

    write_frame(
        &mut s,
        &json!({
            "id": "a",
            "m": "said",
            "binding_id": binding_id,
            "origin": "origin-a",
            "author_id": "u1",
            "text": "slow work",
            "attachments": []
        }),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "a").await["m"], "ok");
    entered.await;
    write_frame(
        &mut s,
        &json!({
            "id": "b",
            "m": "said",
            "binding_id": binding_id,
            "origin": "origin-b",
            "author_id": "u1",
            "text": "no reply needed",
            "attachments": []
        }),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "b").await["m"], "ok");
    release_tx.send(()).unwrap();

    let mut say_id = None;
    let mut ended = None;
    for _ in 0..20 {
        let Some(v) = read_frame_opt(&mut s).await else {
            continue;
        };
        match v["m"].as_str() {
            Some("say") => {
                assert_eq!(v["payload"]["text"], "visible-a");
                say_id = v["id"].as_str().map(str::to_string);
                write_frame(&mut s, &json!({"id": v["id"], "m": "ok"})).await;
            }
            Some("activity") if v["state"] == "ended" => ended = Some(v),
            _ => {}
        }
        if say_id.is_some() && ended.is_some() {
            break;
        }
    }
    let say_id = say_id.expect("A visible say");
    let ended = ended.expect("successful lifecycle ended");
    assert_eq!(ended["completed_target"], say_id);
    assert_eq!(ended["silent_origins"], json!(["origin-b"]));
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
    let mut saw_stopped = false;
    let mut saw_ended = false;
    let mut saw_say = false;
    // #964: LLM call 直前の read が発端 origin を運び、その後の started は origin を持たない。
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
                Some("activity") if v["state"] == "stopped" => {
                    saw_stopped = true;
                    activity_order.push("stopped");
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
        if saw_started && saw_stopped && read_origin.is_some() && saw_ended && saw_say {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(saw_started && saw_stopped && saw_ended && saw_say);
    assert!(!started_had_origin, "started は origin を持たない");
    assert_eq!(read_origin.as_deref(), Some("omo-1"));
    assert_eq!(activity_order, ["read", "started", "stopped"]);
    assert!(!ended_had_origin);
}

