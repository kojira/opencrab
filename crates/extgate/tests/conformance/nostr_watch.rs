
/// QC 893e4e98: watch Bundle の 1 件目 Accepted が pending_turn を握り、
/// 後続 member が Busy → coordinator が all_in 待ちのまま turn が無い。
/// record のあと間隔を進め、receipt で全 member を通すと turn が発火する。
#[tokio::test(start_paused = true)]
async fn watch_bundle_records_then_fires_turn_after_interval() {
    use std::collections::HashSet;
    use std::sync::atomic::AtomicU32;

    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    let address = "nostr-agent-1";
    insert_named_session(&h, address);
    let watch_id = {
        let conn = h.state.db.lock().unwrap();
        opencrab_db::queries::insert_session_watch(&conn, address, "agent-1", 120, "{}").unwrap()
    };
    let author = "aa".repeat(32);
    let origins = [
        format!("nostr:event:v1:watch:{watch_id}:{}", "11".repeat(32)),
        format!("nostr:event:v1:watch:{watch_id}:{}", "22".repeat(32)),
        format!("nostr:event:v1:watch:{watch_id}:{}", "33".repeat(32)),
    ];
    let idx = AtomicU32::new(0);
    let origins_hook = origins.to_vec();
    h.state.set_nostr_said_admit(Arc::new(move |_, _, _| {
        let i = idx.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(NostrSaidDecision::Accept {
            watch_id: Some(watch_id),
            immediate: false,
            bundle: Some(NostrBundleAdmit {
                bundle_id: "d781a3e7eaacca5606f44e87025f0503c12cf273ed33834fe6de37b6f55ad6bd"
                    .into(),
                index: i,
                count: 3,
                origins: origins_hook.clone(),
            }),
        })
    }));
    let follow_key = author.clone();
    h.state.set_nostr_watch_sets(Arc::new(move |_| {
        Some(NostrWatchSets {
            followees: HashSet::from([follow_key.clone()]),
            owner: HashSet::new(),
            co_agents: HashSet::new(),
            trusted_users: HashSet::new(),
        })
    }));

    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-instances/{instance_id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind_id": "nostr",
                        "subject_id": h.subject_id,
                        "enabled": true,
                        "config_b64": config_b64(),
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert!(
        st == StatusCode::CREATED || st == StatusCode::OK,
        "{st} {}",
        String::from_utf8_lossy(&body)
    );
    put_binding(&h, &binding_id, &instance_id, address).await;

    let client = InstanceClient::connect(
        &h.sock,
        instance_id.clone(),
        1,
        author.clone(),
        config_digest(),
    )
    .await
    .expect("connect");
    let mut bound = false;
    for _ in 0..80 {
        if client.binding_for_address(address).await.as_deref() == Some(binding_id.as_str()) {
            bound = true;
            break;
        }
        tokio::time::advance(Duration::from_millis(5)).await;
    }
    assert!(bound, "bind ack");

    tokio::time::advance(Duration::from_secs(120)).await;

    let before_logs = {
        let conn = h.state.db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
            [address],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
    };
    let before_turns = h.runtime.turns.load(Ordering::SeqCst);
    let members_json = serde_json::to_string(&origins).unwrap();
    let event_ids = ["11".repeat(32), "22".repeat(32), "33".repeat(32)];

    for (i, origin) in origins.iter().enumerate() {
        let text = format!(
            "[NOSTRGATE/V1 {{\"beyond_self\":true,\"bundle_id\":\"d781a3e7eaacca5606f44e87025f0503c12cf273ed33834fe6de37b6f55ad6bd\",\"count\":3,\"event_id\":\"{}\",\"has_e\":false,\"index\":{},\"kind\":1,\"p_self\":false,\"route\":\"bundle\",\"watch_id\":{watch_id}}}]\n[NOSTRBUNDLE/V1 {members_json}]\nいかかつ東京\n[Nostr kind:1 メンション from={author} target=note1x]",
            event_ids[i],
            i + 1
        );
        let outcome = client
            .post_said_receipt_with_author_label(
                address,
                origin,
                &author,
                Some("Nostr Alice"),
                &text,
                &[],
            )
            .await
            .unwrap_or_else(|e| panic!("member {} refuse {e:?}", i + 1));
        assert!(
            matches!(outcome, SaidOutcome::Accepted { .. }),
            "member {} {outcome:?}",
            i + 1
        );
        if i == 0 {
            let logs = {
                let conn = h.state.db.lock().unwrap();
                conn.query_row(
                    "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
                    [address],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
            };
            assert!(logs > before_logs, "first member records");
            assert_eq!(
                h.runtime.turns.load(Ordering::SeqCst),
                before_turns,
                "incomplete bundle does not fire"
            );
        }
    }

    for _ in 0..80 {
        if h.runtime.turns.load(Ordering::SeqCst) > before_turns {
            break;
        }
        tokio::time::advance(Duration::from_millis(20)).await;
    }
    assert!(
        h.runtime.turns.load(Ordering::SeqCst) > before_turns,
        "all receipts enqueue a turn"
    );
    assert_eq!(
        h.runtime.sender_names.lock().unwrap().last().map(String::as_str),
        Some("Nostr Alice"),
        "bundle trigger preserves the gateway-provided label"
    );
}

/// Defect B（QC #10）実経路: watch 車線が勝ったオーナー/フォロイーのリプライは、kind ラベルを
/// 実 nostr kind から導出することで**即時 turn 起動**する（interval を待たない）。修正前は kind_label が
/// 一律 "said" のため即応判定に当たらず interval 分保留された（53 秒沈黙 → 別依頼と合流）。
#[tokio::test(start_paused = true)]
async fn watch_lane_owner_reply_fires_turn_immediately() {
    use std::collections::HashSet;

    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    let address = "nostr-agent-imm";
    insert_named_session(&h, address);
    let watch_id = {
        let conn = h.state.db.lock().unwrap();
        opencrab_db::queries::insert_session_watch(&conn, address, "agent-1", 120, "{}").unwrap()
    };
    let author = "aa".repeat(32);
    h.state.set_nostr_said_admit(Arc::new(move |_, _, _| {
        Ok(NostrSaidDecision::Accept {
            watch_id: Some(watch_id),
            immediate: true,
            bundle: None,
        })
    }));
    let follow_key = author.clone();
    h.state.set_nostr_watch_sets(Arc::new(move |_| {
        Some(NostrWatchSets {
            followees: HashSet::from([follow_key.clone()]),
            owner: HashSet::new(),
            co_agents: HashSet::new(),
            trusted_users: HashSet::new(),
        })
    }));

    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-instances/{instance_id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind_id": "nostr",
                        "subject_id": h.subject_id,
                        "enabled": true,
                        "config_b64": config_b64(),
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert!(
        st == StatusCode::CREATED || st == StatusCode::OK,
        "{st} {}",
        String::from_utf8_lossy(&body)
    );
    put_binding(&h, &binding_id, &instance_id, address).await;

    let client = InstanceClient::connect(
        &h.sock,
        instance_id.clone(),
        1,
        author.clone(),
        config_digest(),
    )
    .await
    .expect("connect");
    wait_client_bound(&client, address, &binding_id).await;

    let before_turns = h.runtime.turns.load(Ordering::SeqCst);
    let event_id = "11".repeat(32);
    let origin = format!("nostr:event:v1:watch:{watch_id}:{event_id}");
    // リプライ（kind:1 + #p 自分宛て相当）。renderer 行の label（§9A: from=/target= なし）から
    // 即応判定が働く。
    let text = format!(
        "[NOSTRGATE/V1 {{\"event_id\":\"{event_id}\",\"kind\":1,\"route\":\"immediate\",\"watch_id\":{watch_id}}}]\nおーい\n[Nostr kind:1 リプライ]"
    );
    let outcome = client
        .post_said_with_author(address, &origin, &author, &text, &[])
        .await
        .unwrap_or_else(|e| panic!("said refuse {e:?}"));
    assert!(
        matches!(outcome, SaidOutcome::Accepted { .. }),
        "{outcome:?}"
    );

    // interval（120s）を進めずに turn が発火することを確認（即応）。
    let mut fired = false;
    for _ in 0..80 {
        if h.runtime.turns.load(Ordering::SeqCst) > before_turns {
            fired = true;
            break;
        }
        tokio::time::advance(Duration::from_millis(20)).await;
    }
    assert!(
        fired,
        "owner/followee リプライは interval を待たず即時 turn 起動する（Defect B）"
    );
}

/// 回帰: watch 車線の**即応対象外**の kind（リポスト等）は従来どおり interval 保留のまま。
/// Defect B は kind ラベルを正すだけで、非即応 kind のデバウンスは壊さない。
#[tokio::test(start_paused = true)]
async fn watch_lane_repost_still_debounced() {
    use std::collections::HashSet;

    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    let address = "nostr-agent-repost";
    insert_named_session(&h, address);
    let watch_id = {
        let conn = h.state.db.lock().unwrap();
        opencrab_db::queries::insert_session_watch(&conn, address, "agent-1", 120, "{}").unwrap()
    };
    let author = "aa".repeat(32);
    h.state.set_nostr_said_admit(Arc::new(move |_, _, _| {
        Ok(NostrSaidDecision::Accept {
            watch_id: Some(watch_id),
            immediate: true,
            bundle: None,
        })
    }));
    let follow_key = author.clone();
    h.state.set_nostr_watch_sets(Arc::new(move |_| {
        Some(NostrWatchSets {
            followees: HashSet::from([follow_key.clone()]),
            owner: HashSet::new(),
            co_agents: HashSet::new(),
            trusted_users: HashSet::new(),
        })
    }));

    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-instances/{instance_id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind_id": "nostr",
                        "subject_id": h.subject_id,
                        "enabled": true,
                        "config_b64": config_b64(),
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert!(
        st == StatusCode::CREATED || st == StatusCode::OK,
        "{st} {}",
        String::from_utf8_lossy(&body)
    );
    put_binding(&h, &binding_id, &instance_id, address).await;

    let client = InstanceClient::connect(
        &h.sock,
        instance_id.clone(),
        1,
        author.clone(),
        config_digest(),
    )
    .await
    .expect("connect");
    wait_client_bound(&client, address, &binding_id).await;

    let before_turns = h.runtime.turns.load(Ordering::SeqCst);
    let event_id = "11".repeat(32);
    let origin = format!("nostr:event:v1:watch:{watch_id}:{event_id}");
    let text = format!(
        "[NOSTRGATE/V1 {{\"event_id\":\"{event_id}\",\"kind\":6,\"route\":\"immediate\",\"watch_id\":{watch_id}}}]\n\n[Nostr kind:6 リポスト]"
    );
    let outcome = client
        .post_said_with_author_label(
            address,
            &origin,
            &author,
            Some("Held Alice"),
            &text,
            &[],
        )
        .await
        .unwrap_or_else(|e| panic!("said refuse {e:?}"));
    assert!(
        matches!(outcome, SaidOutcome::Accepted { .. }),
        "{outcome:?}"
    );

    // interval を進めない間は保留（発火しない）。
    for _ in 0..40 {
        tokio::time::advance(Duration::from_millis(20)).await;
    }
    assert_eq!(
        h.runtime.turns.load(Ordering::SeqCst),
        before_turns,
        "リポストは即応対象外なので interval 前には発火しない"
    );
    // interval 経過で debounce が flush して発火する。
    tokio::time::advance(Duration::from_secs(120)).await;
    let mut fired = false;
    for _ in 0..80 {
        if h.runtime.turns.load(Ordering::SeqCst) > before_turns {
            fired = true;
            break;
        }
        tokio::time::advance(Duration::from_millis(20)).await;
    }
    assert!(fired, "interval 経過で保留分が発火する");
    assert_eq!(
        h.runtime.sender_names.lock().unwrap().last().map(String::as_str),
        Some("Held Alice"),
        "deferred held turn preserves the gateway-provided label"
    );
}

