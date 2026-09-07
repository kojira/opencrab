
/// **resume（subtask 完了）の run に載る登録簿は、呼び出し側が渡した実体そのもので
/// なければならない。**
///
/// `cancel_subtask` はセッションの登録簿から subtask を引くので、resume 実行に別の
/// 登録簿を渡すと、その run が auto-dispatch した subtask はどこからも停止できない
/// （Discord の `cancel_subtask` が常に "not found" を返す）。dispatch が有効か
/// （bool）だけの検査では別実体の取り違えを検出できないため、`Arc::ptr_eq` で固定する。
#[tokio::test]
async fn resume_run_carries_the_caller_registry_so_cancel_can_reach_it() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());

    process_subtask_completed(
        "discord-crab-111-222".to_string(),
        "crab".to_string(),
        "st-1".to_string(),
        "結果本文".to_string(),
        "completed".to_string(),
        222,
        "222".to_string(),
        "111".to_string(),
        false,
        gateway,
        state.clone(),
        gateway_actions,
        None,
        event_tx,
        registry.clone(),
        opencrab_actions::CallerIdentity::Agent,
    )
    .await;

    let (session_id, observed, has_sink) = state.observed(0);
    assert_eq!(session_id, "discord-crab-111-222");
    let observed = observed.expect("run に登録簿が載っていない（非ブロック実行が無効）");
    assert!(
        Arc::ptr_eq(&observed, &registry),
        "resume の応答生成に渡した登録簿が、停止処理が引くものと別インスタンスになっている"
    );
    assert!(
        has_sink,
        "resume の run に完了 sink が無い（掘削の完了が再注入されない）"
    );
}

/// **inbound（Discord 受信）の run に載る登録簿も、ループが持つ共有実体そのもので
/// なければならない。**
///
/// こちらが本番の主経路。ここで別の登録簿を渡すと、通常の会話から auto-dispatch した
/// background subtask が `cancel_subtask` の到達範囲から外れて停止不能になる。
#[tokio::test]
async fn inbound_run_carries_the_shared_registry_so_cancel_can_reach_it() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let session_locks = Arc::new(SessionLocks::new());

    let incoming = IncomingMessage::new(
        MessageSource::Discord {
            guild_id: "111".to_string(),
            channel_id: "222".to_string(),
        },
        MessageContent::Text("掘削して".to_string()),
        Sender::new("user-1", "だれか"),
    );

    process_incoming_message(
        incoming,
        gateway,
        state.clone(),
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        session_locks,
        false,
        None, // v3_liveness: このテストは V3 二重受信ゲートの対象外
        None,
        event_tx,
        registry.clone(),
        false,
        true,
        None,
    )
    .await;

    // 応答生成は `SessionLocks::spawn_serialized` の中で走るので、観測の通知を待つ。
    state.wait_for_run().await;

    let (session_id, observed, has_sink) = state.observed(0);
    assert_eq!(session_id, "discord-crab-111-222");
    let observed = observed.expect("run に登録簿が載っていない（非ブロック実行が無効）");
    assert!(
        Arc::ptr_eq(&observed, &registry),
        "inbound の応答生成に渡した登録簿が、停止処理が引くものと別インスタンスになっている"
    );
    assert!(
        has_sink,
        "inbound の run に完了 sink が無い（掘削の完了が再注入されない）"
    );
}

/// **per-agent（legacy）ループは、V3 gateway process が同じ agent を受信中なら退く**
/// （DESIGN-DISCORD-GATE §8.1 の二重受信防止ゲート）。
///
/// これが無いと legacy 車線が V3 と並走して同一メッセージを二重処理する（実バグの症状:
/// V3 が正しい返信を出す横で legacy が 👀→NO_REPLY→🤐 を付ける）。`served_by_dedicated_gateway`
/// （legacy manager 自身の生死を OR）は per-agent ループ内では常に true になり使えないので、
/// V3 liveness だけを見る専用 probe（`v3_liveness`）で判定する。probe が false（V3 死亡/未接続/
/// ロック失敗）なら退かず処理を続ける＝外形を減らさない（fail-open）。ここは両方向を固定する。
#[tokio::test]
async fn per_agent_loop_defers_to_live_v3_gateway() {
    // (1) V3 稼働中（probe=true）→ 退く: run も 受信記録 も起きない（V3 車線が処理する）。
    {
        let (state, gateway, gateway_actions) = make_deps();
        let (event_tx, _event_rx) = create_event_channel();
        let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
        let session_locks = Arc::new(SessionLocks::new());
        let incoming = IncomingMessage::new(
            MessageSource::Discord {
                guild_id: "111".to_string(),
                channel_id: "222".to_string(),
            },
            MessageContent::Text("sleep 30して".to_string()),
            Sender::new("user-1", "だれか"),
        );
        let v3_live: crate::message_loop::V3LivenessProbe = Arc::new(|_agent: &str| true);
        process_incoming_message(
            incoming,
            gateway,
            state.clone(),
            vec!["crab".to_string()],
            gateway_actions,
            "owner-1".to_string(),
            session_locks,
            false,
            Some(v3_live),
            None, // voice
            event_tx,
            registry,
            false,
            true,
            None,
        )
        .await;
        // ゲートは accept_inbound より手前で早期 return するので、run も spawn されない。
        assert!(
            state.runs.lock().unwrap().is_empty(),
            "V3 稼働中なのに legacy が run した（二重処理）"
        );
        assert!(
            state.inbound_records.lock().unwrap().is_empty(),
            "V3 稼働中なのに legacy が受信記録した（二重処理・二重記録）"
        );
    }

    // (2) V3 死亡（probe=false）→ 退かず従来どおり処理: run が観測される（外形不減）。
    {
        let (state, gateway, gateway_actions) = make_deps();
        let (event_tx, _event_rx) = create_event_channel();
        let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
        let session_locks = Arc::new(SessionLocks::new());
        let incoming = IncomingMessage::new(
            MessageSource::Discord {
                guild_id: "111".to_string(),
                channel_id: "222".to_string(),
            },
            MessageContent::Text("sleep 30して".to_string()),
            Sender::new("user-1", "だれか"),
        );
        let v3_dead: crate::message_loop::V3LivenessProbe = Arc::new(|_agent: &str| false);
        process_incoming_message(
            incoming,
            gateway,
            state.clone(),
            vec!["crab".to_string()],
            gateway_actions,
            "owner-1".to_string(),
            session_locks,
            false,
            Some(v3_dead),
            None, // voice
            event_tx,
            registry,
            false,
            true,
            None,
        )
        .await;
        state.wait_for_run().await;
        assert_eq!(
            state.runs.lock().unwrap().len(),
            1,
            "V3 死亡時に legacy が退いてしまった（外形減・誰も応答しない）"
        );
    }
}

/// **inbound の run は「発言終わり」🏁 判定用の subtask 起動カウンタを載せる**（#431）。
///
/// このカウンタが未配線（`None`）だと、run 側（auto-dispatch / 明示 `spawn_subtask`）が
/// 加算する先を失い、ゲートは常に「subtask を起こしていない」と見る。結果、掘削を
/// 投げたターンに 🏁 が付いて『調べますね🏁』の数分後に続きが届く逆情報に戻る。
/// **配線を落としても他のテストは落ちない**（ゲートの単体テストはカウンタの値を
/// 直接与えるため）ので、ここで配線そのものを固定する。
#[tokio::test]
async fn inbound_run_carries_the_end_of_speech_subtask_counter() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let session_locks = Arc::new(SessionLocks::new());

    let incoming = IncomingMessage::new(
        MessageSource::Discord {
            guild_id: "111".to_string(),
            channel_id: "222".to_string(),
        },
        MessageContent::Text("掘削して".to_string()),
        Sender::new("user-1", "だれか"),
    );

    process_incoming_message(
        incoming,
        gateway,
        state.clone(),
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        session_locks,
        false,
        None, // v3_liveness: このテストは V3 二重受信ゲートの対象外
        None,
        event_tx,
        registry,
        false,
        true,
        None,
    )
    .await;

    state.wait_for_run().await;

    let counter = state
        .observed_subtask_starts(0)
        .expect("inbound の run に subtask 起動カウンタが載っていない（🏁 の判定が効かない）");
    // ターンごとに新しいカウンタで、run に入る時点では 0（前のターンの掘削を
    // 引きずらない）。
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "ターン開始時のカウンタは 0 でなければならない"
    );
}

/// **subtask 完了 resume の run にもカウンタが載る**（#431）。
///
/// resume ターンが**さらに**掘削を投げたときに、その resume へ 🏁 を付けず次の resume
/// へ委ねるための配線。ここが落ちると鎖の途中に印が付く。
#[tokio::test]
async fn resume_run_carries_the_end_of_speech_subtask_counter() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());

    process_subtask_completed(
        "discord-crab-111-222".to_string(),
        "crab".to_string(),
        "st-1".to_string(),
        "結果".to_string(),
        "completed".to_string(),
        222,
        "222".to_string(),
        "111".to_string(),
        false,
        gateway,
        state.clone(),
        gateway_actions,
        None,
        event_tx,
        registry,
        CallerIdentity::Owner,
    )
    .await;

    let counter = state
        .observed_subtask_starts(0)
        .expect("resume の run に subtask 起動カウンタが載っていない");
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// **Discord の受信は共通の受信フックを必ず通る**（#156 S4）。
///
/// ピアレビュー返信の回収は以前 Discord の受信ループが直接呼ぶ専用関数だった。汎用層へ
/// 移した後にこの呼び出しが落ちると、回収は**静かに止まる**（返信は普通の発言として
/// 流れるだけなのでログにも異常が出ない）。ここでフックの呼び出しと、渡す由来・帰属・
/// 本文を固定する。回収そのもののゲートは汎用層（`crates/server/src/peer_review.rs`）の
/// テストが持つ。
#[tokio::test]
async fn inbound_goes_through_the_shared_inbound_hook() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let session_locks = Arc::new(SessionLocks::new());

    let reply = "[Peer Review] score: 0.7, gaps: none, summary: ok";
    let incoming = IncomingMessage::new(
        MessageSource::Discord {
            guild_id: "111".to_string(),
            channel_id: "222".to_string(),
        },
        MessageContent::Text(reply.to_string()),
        Sender::new("user-1", "crab-b"),
    );

    process_incoming_message(
        incoming,
        gateway,
        state.clone(),
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        session_locks,
        false,
        None, // v3_liveness: このテストは V3 二重受信ゲートの対象外
        None,
        event_tx,
        registry,
        false,
        true,
        None,
    )
    .await;

    // フックは応答生成と同じ直列タスクの中（会話組み立ての前）で呼ばれる。
    state.wait_for_run().await;

    let hooks = state.inbound_hooks.lock().unwrap();
    let call = hooks
        .first()
        .expect("受信が共通フック（on_inbound_message）を通っていない — 返信の回収が死ぬ");
    assert_eq!(call.source, opencrab_actions::TranscriptSource::Discord);
    // 帰属は**受信側エージェント**（誰の台帳に回収するか）。送信者と取り違えない。
    assert_eq!(call.agent_id, "crab");
    assert_eq!(call.session_id, "discord-crab-111-222");
    assert_eq!(call.sender_id, "user-1");
    assert_eq!(call.sender_name, "crab-b");
    assert_eq!(call.text, reply);
}

/// **ユーザー発言の記録はセッションロックの外（＝ロック待ちより前）で確定する。**（#284 P0-1）
///
/// 以前は `spawn_serialized` の内側で記録していたため、そのセッションで長い推論が
/// 走っている間に届いた発言は、推論が終わるまで DB に入らなかった。その窓でプロセスが
/// 落ちる／タスクが失われると**発言が永久に消える**（実際に起きた #284 の症状）。
///
/// ここでは同一セッションのロックをテスト側で握ったまま `process_incoming_message` を
/// 呼び、**ロックを握ったままでも記録が済んでいる**ことを固定する。実装をロックの
/// 内側へ戻すと、記録がロック解放待ちになりこのテストが落ちる。
#[tokio::test]
async fn inbound_message_is_recorded_before_the_session_lock_is_acquired() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let session_locks = Arc::new(SessionLocks::new());
    let session_id = "discord-crab-111-222";

    // 同一セッションのロックを掴んだまま離さないタスク（＝走行中の長い推論の代役）。
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
    let holder_locks = session_locks.clone();
    let holder = tokio::spawn(async move {
        holder_locks
            .run_serialized(session_id, async move {
                let _ = held_tx.send(());
                let _ = release_rx.await;
            })
            .await;
    });
    held_rx.await.expect("ロックが取得されなかった");

    let incoming = IncomingMessage::new(
        MessageSource::Discord {
            guild_id: "111".to_string(),
            channel_id: "222".to_string(),
        },
        MessageContent::Text("全員フォローして".to_string()),
        Sender::new("user-1", "owner"),
    );

    process_incoming_message(
        incoming,
        gateway,
        state.clone(),
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        session_locks,
        false,
        None, // v3_liveness: このテストは V3 二重受信ゲートの対象外
        None,
        event_tx,
        registry,
        false,
        true,
        None,
    )
    .await;

    // ロックはまだ握られている（応答生成は 1 件も走れていない）。
    assert!(
        state.runs.lock().unwrap().is_empty(),
        "テストの前提が崩れている: ロックを握ったままなのに応答生成が走った"
    );
    // それでも発言は記録済みでなければならない。
    let records = state.inbound_records.lock().unwrap().clone();
    assert_eq!(
        records,
        vec!["全員フォローして".to_string()],
        "ユーザー発言がセッションロックの解放待ちになっている（ロック中に失うと消える）"
    );

    let _ = release_tx.send(());
    holder.await.unwrap();
}

/// **記録に失敗したら黙って進まない。**（#284 P0-3）
///
/// `record_inbound_message` は best-effort ではなく成否を返す。false を無視すると、
/// エージェントはその発言を一度も見ないまま応答する（＝ #284 の症状そのもの）。
/// 呼び出し側が戻り値を評価していることを固定する（評価していなければ `#[must_use]`
/// と警告で気づけるが、警告はビルド設定で消せるのでテストでも縛る）。
#[test]
fn failed_inbound_record_is_detected_not_swallowed() {
    // `captured_logs` はスレッドローカルの捕捉先に依存するので、`#[tokio::test]` ではなく
    // 「捕捉クロージャの中で current-thread ランタイムを回す」形にする（同じスレッドで
    // 走らせないと warn を拾えない）。
    let logs = crate::owner_warning::capture::captured_logs(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (state, gateway, gateway_actions) = make_deps();
            state
                .inbound_record_fails
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let (event_tx, _event_rx) = create_event_channel();
            let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
            let session_locks = Arc::new(SessionLocks::new());

            let incoming = IncomingMessage::new(
                MessageSource::Discord {
                    guild_id: "111".to_string(),
                    channel_id: "222".to_string(),
                },
                MessageContent::Text("つらい".to_string()),
                Sender::new("user-1", "owner"),
            );

            process_incoming_message(
                incoming,
                gateway,
                state.clone(),
                vec!["crab".to_string()],
                gateway_actions,
                "owner-1".to_string(),
                session_locks,
                false,
                None, // v3_liveness: このテストは V3 二重受信ゲートの対象外
                None,
                event_tx,
                registry,
                false,
                true,
                None,
            )
            .await;

            // 記録は試みられている（＝呼び出し自体は消えていない）。
            assert_eq!(state.inbound_records.lock().unwrap().len(), 1);
        });
    });

    // #286: 「呼ばれたこと」だけを見るとトートロジーになる。**false を受けて実際に
    // エスカレーションが出る**ところまで検査する（戻り値を捨てる実装に戻ると落ちる）。
    assert!(
        logs.contains("failed to persist an inbound user message"),
        "記録失敗が握り潰されている（警告が出ていない）: {logs}"
    );
    assert!(
        logs.contains("discord-crab-111-222"),
        "どのセッションで落ちたか分からない: {logs}"
    );
}
