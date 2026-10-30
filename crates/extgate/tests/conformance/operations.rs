// ===== DI 拡張: 能力宣言 hello・digest・invoke 往復（§3/§5/§8・実経路） =====

/// hello に載せる最小 operations（reply 1 件・§9.2 スキーマ形・name 昇順）。
fn ops_reply() -> Value {
    json!([{
        "name": "reply",
        "description": "reply to an event",
        "input_schema": {"type": "object", "required": ["event", "text"],
            "properties": {"event": {"type": "string"}, "text": {"type": "string"}}},
        "output_schema": null,
        "callback_schema": null,
        "authorization": {"allowed_callers": ["co_agent", "guest", "owner", "trusted"]},
        "dispatch": "utterance", "sub_engine": "not_exposed",
        "sharing": "conversation_bound", "effect": "utterance"
    }])
}

/// D-1099: nothing to acknowledge; a live instance is enough.
async fn wait_acked(_h: &Harness, _instance_id: &str, _binding_id: &str) {}

async fn hello_with_ops(
    s: &mut UnixStream,
    instance_id: &str,
    _revision: u64,
    ops: &Value,
) -> Value {
    write_frame(
        s,
        &json!({
            "id": "h1", "m": "hello", "protocol": 3,
            "operation_protocol": 1,
            "final_delivery": "automatic",
            "instance_id": instance_id, "agent_id": "agent-1", "operations": ops,
        }),
    )
    .await;
    read_frame(s).await
}

/// hello + operations が受理される。digest の永続化は撤去したので DB 列は NULL のまま（#894）。
#[tokio::test]
async fn di_hello_operations_accepted_and_digest_not_persisted() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let mut s = h.connect().await;
    let ok = hello_with_ops(&mut s, &instance_id, 1, &ops_reply()).await;
    assert_eq!(ok["m"], "ok", "operations つき hello が受理される");
    // digest は永続しない（照合撤去・#894）。列は残るが書かないので NULL のまま。
    let digest: Option<String> = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT operation_declaration_digest FROM gate_instances WHERE instance_id = ?1",
            [&instance_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        digest.is_none(),
        "宣言 digest を永続してはいけない: {digest:?}"
    );
}

/// 同一 revision で宣言（op 名・class）が変わった再接続も受理される。永続 digest の照合を
/// 撤去したため mismatch にならない（#894）。core は hello ごとに宣言を fresh parse する。
#[tokio::test]
async fn di_hello_operations_change_accepted_on_reconnect() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    // 初回 hello（reply 宣言）。
    let mut s1 = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s1, &instance_id, 1, &ops_reply()).await["m"],
        "ok"
    );
    drop(s1);
    wait_not_live(&h, &instance_id).await;
    // 別宣言（op 名も class も違う）で同一 revision に再接続 → 受理される（mismatch なし）。
    let other = json!([{
        "name": "reaction", "description": "d",
        "input_schema": {"type": "object"}, "output_schema": null, "callback_schema": null,
        "authorization": {"allowed_callers": ["co_agent", "guest", "owner", "trusted"]},
        "dispatch": "utterance", "sub_engine": "not_exposed",
        "sharing": "conversation_bound", "effect": "utterance"
    }]);
    let mut s2 = h.connect().await;
    let resp = hello_with_ops(&mut s2, &instance_id, 1, &other).await;
    assert_eq!(resp["m"], "ok", "宣言変更の再接続が拒否された: {resp}");
    // 新宣言（reaction）が live snapshot に反映されている。
    let name = {
        let reg = h.state.lock_registry().unwrap();
        let e = reg.get(&instance_id).expect("再接続後に live でない");
        e.declarations.first().map(|d| d.name.clone())
    };
    assert_eq!(
        name.as_deref(),
        Some("reaction"),
        "新宣言が反映されていない"
    );
    drop(s2);
}

/// 不正な宣言（非 sort）は operation_declaration_invalid + close（DI-22）。
#[tokio::test]
async fn di_hello_invalid_operations_rejected() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    // name 逆順（非 sort）。
    let bad = json!([
        {"name": "reply", "description": "d", "input_schema": {"type": "object"},
         "output_schema": null, "callback_schema": null,
         "authorization": {"allowed_callers": ["co_agent", "guest", "owner", "trusted"]},
        "dispatch": "utterance", "sub_engine": "not_exposed",
        "sharing": "conversation_bound", "effect": "utterance"},
        {"name": "follow", "description": "d", "input_schema": {"type": "object"},
         "output_schema": null, "callback_schema": null,
         "authorization": {"allowed_callers": ["owner"]},
          "dispatch": "background", "sub_engine": "not_exposed",
          "sharing": "agent_bound", "effect": "state_change"}
    ]);
    let mut s = h.connect().await;
    let resp = hello_with_ops(&mut s, &instance_id, 1, &bad).await;
    assert_eq!(resp["m"], "err");
    assert_eq!(resp["code"], "operation_declaration_invalid");
}

/// invoke 往復（成功）: core invoke_and_wait → gateway ok(result) → Ok(result) + call succeeded。
#[tokio::test]
async fn di_invoke_round_trip_success() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let binding_id_key: String = ("chan-di").to_string();
    let mut s = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s, &instance_id, 1, &ops_reply()).await["m"],
        "ok"
    );
    let binding_id = session_row(&h, &instance_id, &binding_id_key).row_id;
    wait_acked(&h, &instance_id, &binding_id).await;

    // core 側で invoke を発行（背景 subtask 相当）。
    let state = Arc::clone(&h.state);
    let inst = instance_id.clone();
    let bind = binding_id.clone();
    let call = tokio::spawn(async move {
        invoke_and_wait(
            &state,
            &inst,
            &bind,
            "reply",
            &json!({"event": "e1", "text": "hi"}),
        )
        .await
    });

    // gateway 側: invoke を受けて ok(result) を返す。
    let invoke = read_until(&mut s, |v| v["m"] == "invoke").await;
    assert_eq!(invoke["operation"], "reply");
    assert_eq!(invoke["payload"]["text"], "hi");
    let call_id = invoke["id"].as_str().unwrap().to_string();
    write_frame(
        &mut s,
        &json!({"id": call_id, "m": "ok", "result": {"posted": true}}),
    )
    .await;

    let out = tokio::time::timeout(Duration::from_secs(2), call)
        .await
        .expect("invoke_and_wait 完了")
        .unwrap();
    assert_eq!(out.unwrap(), json!({"posted": true}), "result が返る");
    // call row が succeeded。
    let state_str: String = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT state FROM gateway_operation_calls WHERE binding_id = ?1",
            [&binding_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state_str, "succeeded");
}

/// invoke 往復（確定拒否）: gateway err(operation_rejected) → Err + call failed。
#[tokio::test]
async fn di_invoke_round_trip_rejected() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let binding_id_key: String = ("chan-di2").to_string();
    let mut s = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s, &instance_id, 1, &ops_reply()).await["m"],
        "ok"
    );
    let binding_id = session_row(&h, &instance_id, &binding_id_key).row_id;
    wait_acked(&h, &instance_id, &binding_id).await;

    let state = Arc::clone(&h.state);
    let inst = instance_id.clone();
    let bind = binding_id.clone();
    let call = tokio::spawn(async move {
        invoke_and_wait(
            &state,
            &inst,
            &bind,
            "reply",
            &json!({"event": "e1", "text": "x"}),
        )
        .await
    });
    let invoke = read_until(&mut s, |v| v["m"] == "invoke").await;
    let call_id = invoke["id"].as_str().unwrap().to_string();
    write_frame(
        &mut s,
        &json!({"id": call_id, "m": "err", "code": "operation_rejected", "detail": null}),
    )
    .await;
    let out = tokio::time::timeout(Duration::from_secs(2), call)
        .await
        .expect("完了")
        .unwrap();
    assert!(out.is_err(), "確定拒否は Err");
    let state_str: String = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT state FROM gateway_operation_calls WHERE binding_id = ?1",
            [&binding_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state_str, "failed");
}

/// 宣言に無い operation の invoke は call/wire 0 で operation_unknown（§5.1）。
#[tokio::test]
async fn di_invoke_undeclared_operation_is_unknown() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let binding_id_key: String = ("chan-di3").to_string();
    let mut s = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s, &instance_id, 1, &ops_reply()).await["m"],
        "ok"
    );
    let binding_id = session_row(&h, &instance_id, &binding_id_key).row_id;
    wait_acked(&h, &instance_id, &binding_id).await;
    // "repost" は宣言していない。
    let out = invoke_and_wait(&h.state, &instance_id, &binding_id, "repost", &json!({})).await;
    assert!(out.is_err(), "未宣言 operation は Err");
    // call row は作られない。
    let count: i64 = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM gateway_operation_calls WHERE binding_id = ?1",
            [&binding_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "未宣言 invoke は call row 0");
}

/// invoke 中に gateway が切断すると call は indeterminate、invoke_and_wait は Err（§5.3・不明を
/// 確定拒否へ捏造しない）。
#[tokio::test]
async fn di_invoke_indeterminate_on_gateway_disconnect() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let binding_id_key: String = ("chan-di-disc").to_string();
    let mut s = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s, &instance_id, 1, &ops_reply()).await["m"],
        "ok"
    );
    let binding_id = session_row(&h, &instance_id, &binding_id_key).row_id;
    wait_acked(&h, &instance_id, &binding_id).await;

    let state = Arc::clone(&h.state);
    let inst = instance_id.clone();
    let bind = binding_id.clone();
    let call = tokio::spawn(async move {
        invoke_and_wait(
            &state,
            &inst,
            &bind,
            "reply",
            &json!({"event": "e1", "text": "x"}),
        )
        .await
    });
    // invoke を受けたら応答せず切断する（gateway が落ちた）。
    let invoke = read_until(&mut s, |v| v["m"] == "invoke").await;
    assert_eq!(invoke["operation"], "reply");
    drop(s);
    let out = tokio::time::timeout(Duration::from_secs(3), call)
        .await
        .expect("完了")
        .unwrap();
    assert!(out.is_err(), "切断は Err（disconnect）");
    // call row は indeterminate。
    for _ in 0..40 {
        let st: Option<String> = h
            .state
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT state FROM gateway_operation_calls WHERE binding_id = ?1",
                [&binding_id],
                |r| r.get(0),
            )
            .ok();
        if st.as_deref() == Some("indeterminate") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("call が indeterminate にならない");
}

/// startup recover: 残 'sending' の operation call を stale indeterminate にする（§7.5）。
#[tokio::test]
async fn di_recover_stale_calls_marks_indeterminate() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let binding_id = session_row(&h, &instance_id, "chan-recover").row_id;
    // 'sending' の call を直に入れる（前プロセスの残骸相当）。
    {
        let conn = h.state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO gateway_operation_calls
               (call_id, binding_id, operation, payload_json, result_json, state, error, created_at, updated_at)
             VALUES ('stale-1', ?1, 'reply', '{}', NULL, 'sending', NULL, 1, 1)",
            [&binding_id],
        )
        .unwrap();
    }
    {
        let mut conn = h.state.db.lock().unwrap();
        recover_stale_calls(&mut conn, now_nanos()).unwrap();
    }
    let (state, error): (String, Option<String>) = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT state, error FROM gateway_operation_calls WHERE call_id = 'stale-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "indeterminate");
    assert_eq!(
        error.as_deref(),
        Some("stale sending recovered after restart")
    );
}

/// option-B 継ぎ目: 投影 ExtgateOpsGatewayActions::execute → invoke_and_wait → gateway ok(result)
/// → GatewayActionResult{success, data}。背景 subtask 内 await の実体（この戻り値を既存
/// dispatch_batch が settle する。dispatch→settle→resume の連鎖自体は
/// settlement_is_consumed_on_next_turn 等で別途検収済み）。
#[tokio::test]
async fn di_projection_reply_is_utterance_fire_and_forget() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let binding_id_key: String = ("chan-proj").to_string();
    let mut s = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s, &instance_id, 1, &ops_reply()).await["m"],
        "ok"
    );
    let binding_id = session_row(&h, &instance_id, &binding_id_key).row_id;
    wait_acked(&h, &instance_id, &binding_id).await;

    let session_id = binding_id_key.clone();
    let ops = ExtgateOpsGatewayActions::for_binding(
        Arc::clone(&h.state),
        &instance_id,
        &binding_id,
        &session_id,
        "agent-1",
    )
    .expect("宣言があるので Some");
    // 投影 tool 定義に reply が出る（宣言→projection）。
    assert!(ops.definitions().iter().any(|d| d.name == "reply"));

    let ctx = GatewayCallContext::for_agent("agent-1");
    // 発話クラス化（§3.3.1 C5・row353）: reply は照会でなく発話クラス。execute は operation_call を
    // 作らず delivery で永続し、await せず即 ack（success・data=None＝領収書を返さない）を返す。
    // subtask/settle/resume は起こさない。wire への invoke は従来どおり送られる（gateway が publish）。
    let result = ops
        .execute("reply", &json!({"event": "e1", "text": "やあ"}), &ctx)
        .await;
    assert!(
        result.success,
        "発話 execute が成功する: {:?}",
        result.error
    );
    assert_eq!(result.data, None, "発話は結果本文（領収書）を返さない");

    // gateway 側は invoke を受け取る（配送は fire-and-forget で継続）。
    let invoke = read_until(&mut s, |v| v["m"] == "invoke").await;
    assert_eq!(invoke["operation"], "reply");
    assert_eq!(invoke["payload"]["text"], "やあ");
    let call_id = invoke["id"].as_str().unwrap().to_string();
    // 応答 ok は deliveries 行を delivered 化するだけ（settle/resume を起こさない）。
    write_frame(&mut s, &json!({"id": call_id, "m": "ok"})).await;

    // 発話本文が speech として永続している（機械行なし・C6）。
    let logs = {
        let conn = h.state.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap()
    };
    assert!(
        logs.iter()
            .any(|l| l.log_type == "speech" && l.content == "やあ"),
        "発話本文が speech として永続していない: {:?}",
        logs.iter()
            .map(|l| (&l.log_type, &l.content))
            .collect::<Vec<_>>()
    );
    assert!(
        !logs
            .iter()
            .any(|l| l.log_type == "tool_call" || l.log_type == "tool_result"),
        "発話 reply が機械行を残した"
    );
}

/// 発話クラスの**失敗経路**（§3.3.1 C9・row353）: gateway が invoke を確定拒否
/// （`operation_rejected`）したとき、(a) 発端 origin つき `turn_failed`（❌）が gateway へ emit され、
/// (b) 発話本文の speech ログは残り（言ったことは消えない）、(c) モデルに領収書は返らない
/// （execute は既に data=None で戻っている）。deliveries 行は `failed` へ落ちる。
#[tokio::test]
async fn di_projection_reply_utterance_rejection_emits_turn_failed_and_keeps_speech() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let binding_id_key: String = ("chan-fail").to_string();
    let mut s = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s, &instance_id, 1, &ops_reply()).await["m"],
        "ok"
    );
    let binding_id = session_row(&h, &instance_id, &binding_id_key).row_id;
    wait_acked(&h, &instance_id, &binding_id).await;

    let session_id = binding_id_key.clone();
    let ops = ExtgateOpsGatewayActions::for_binding(
        Arc::clone(&h.state),
        &instance_id,
        &binding_id,
        &session_id,
        "agent-1",
    )
    .expect("宣言があるので Some");

    let ctx = GatewayCallContext::for_agent("agent-1");
    // (c) 撃ちっぱなし: execute は即 success・data=None。拒否は後から wire で届く（領収書は返らない）。
    let result = ops
        .execute("reply", &json!({"event": "e1", "text": "だめでした"}), &ctx)
        .await;
    assert!(result.success, "execute は即成功で戻る: {:?}", result.error);
    assert_eq!(result.data, None, "発話は結果本文（領収書）を返さない");

    // gateway 側: invoke を受けて確定拒否（invoke の err は `operation_rejected`）。
    let invoke = read_until(&mut s, |v| v["m"] == "invoke").await;
    assert_eq!(invoke["operation"], "reply");
    let call_id = invoke["id"].as_str().unwrap().to_string();
    write_frame(
        &mut s,
        &json!({"id": call_id, "m": "err", "code": "operation_rejected", "detail": null}),
    )
    .await;

    // (a) ❌: gateway は発端 origin つき turn_failed frame を受け取る（say と同一の失敗表面化）。
    let tf = read_until(&mut s, |v| v["m"] == "turn_failed").await;
    assert_eq!(
        tf["origin"], "e1",
        "turn_failed の発端 origin が発話の対象（reply_target）"
    );

    // (b) 発話本文の speech ログは残る（失敗でも「言った」ことは消さない）。
    let logs = {
        let conn = h.state.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap()
    };
    assert!(
        logs.iter()
            .any(|l| l.log_type == "speech" && l.content == "だめでした"),
        "失敗でも発話本文の speech が残る: {:?}",
        logs.iter()
            .map(|l| (&l.log_type, &l.content))
            .collect::<Vec<_>>()
    );

    // deliveries は failed へ（indeterminate/delivered でない）。
    let state_str: String = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT state FROM deliveries WHERE binding_id = ?1",
            [&binding_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state_str, "failed", "拒否された発話 delivery は failed");
}

/// item4: 宣言 schema が `"format":"short-ref"` を付けた field だけ、core が実 ID へ解決してから
/// invoke する（会話の e番号 → external_origin）。全 string 無差別ではなく的を絞る。
#[tokio::test]
async fn di_short_ref_resolution_targets_declared_fields() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let binding_id_key: String = ("chan-ref").to_string();
    let session_id = binding_id_key.clone();
    let origin = format!("nostr:event:v1:default:{}", "ab".repeat(32));
    // 受信イベントを 1 件記録（初出順で e1 になる）。
    {
        let conn = h.state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO memory_sessions
               (agent_id, session_id, log_type, content, speaker_id, metadata_json, created_at)
             VALUES ('agent-1', ?1, 'speech', 'hi', 'pk_x', ?2, '2026-08-30T00:00:00Z')",
            rusqlite::params![session_id, json!({"external_origin": origin}).to_string()],
        )
        .unwrap();
    }
    // event は format:short-ref、text は非参照。
    let ops = json!([{
        "name": "reply", "description": "d",
        "input_schema": {"type": "object", "required": ["event", "text"], "properties": {
            "event": {"type": "string", "format": "short-ref"},
            "text": {"type": "string"}
        }},
        "output_schema": null, "callback_schema": null,
        "authorization": {"allowed_callers": ["co_agent", "guest", "owner", "trusted"]},
        "dispatch": "utterance", "sub_engine": "not_exposed",
        "sharing": "conversation_bound", "effect": "utterance"
    }]);
    let mut s = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s, &instance_id, 1, &ops).await["m"],
        "ok"
    );
    let binding_id = session_row(&h, &instance_id, &binding_id_key).row_id;
    wait_acked(&h, &instance_id, &binding_id).await;

    let proj = ExtgateOpsGatewayActions::for_binding(
        Arc::clone(&h.state),
        &instance_id,
        &binding_id,
        &session_id,
        "agent-1",
    )
    .unwrap();
    let ctx = GatewayCallContext::for_agent("agent-1");
    let exec = tokio::spawn(async move {
        proj.execute("reply", &json!({"event": "e1", "text": "本文"}), &ctx)
            .await
    });
    let invoke = read_until(&mut s, |v| v["m"] == "invoke").await;
    // event は解決済み origin、text は素通り、非宣言 short-ref は解決しない。
    assert_eq!(
        invoke["payload"]["event"],
        json!(origin),
        "e1→origin へ解決"
    );
    assert_eq!(invoke["payload"]["text"], json!("本文"), "非参照は素通り");
    let call_id = invoke["id"].as_str().unwrap().to_string();
    write_frame(&mut s, &json!({"id": call_id, "m": "ok", "result": null})).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), exec).await;
}

