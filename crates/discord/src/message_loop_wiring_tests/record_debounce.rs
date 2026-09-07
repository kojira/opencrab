
// ---- #543: デバウンスを (channel, sender) から (channel, 権限レベル) へ移し、run 発火直前で間引く ----

fn discord_msg(channel: &str, sender: &str, text: &str) -> IncomingMessage {
    IncomingMessage::new(
        MessageSource::Discord {
            guild_id: "111".to_string(),
            channel_id: channel.to_string(),
        },
        MessageContent::Text(text.to_string()),
        Sender::new(sender, sender),
    )
}

/// #543 + row 116-117: record-only パスは記録まで済ませ、推論（run）は起こさない。
/// 👀 は読まれるまで付けない（仕様変更。弱化ではない）。合流窓の非トリガーを
/// 「捨てずに記録だけする」土台。
#[tokio::test]
async fn record_only_records_inbound_but_does_not_run() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let session_locks = Arc::new(SessionLocks::new());

    process_incoming_message(
        discord_msg("222", "user-1", "記録だけして"),
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
        true,  // record_only
        false, // 読むターンがまだ無いので 👀 なし
        None,
    )
    .await;

    assert_eq!(
        state.inbound_records.lock().unwrap().clone(),
        vec!["記録だけして".to_string()],
        "record_only でも発言は記録される"
    );
    assert!(
        state.runs.lock().unwrap().is_empty(),
        "record_only なのに推論（run）が走った"
    );
}

fn discord_msg_with_id(
    channel: &str,
    sender: &str,
    text: &str,
    message_id: &str,
) -> IncomingMessage {
    discord_msg(channel, sender, text).with_metadata(
        "discord_message_id",
        serde_json::Value::String(message_id.to_string()),
    )
}

fn seen_reaction_logs(
    incoming: IncomingMessage,
    record_only: bool,
    mark_seen: bool,
    whitelisted: bool,
) -> String {
    crate::owner_warning::capture::captured_logs(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (mut state, gateway, gateway_actions) = make_deps();
            state.channel_whitelisted = whitelisted;
            let (event_tx, _event_rx) = create_event_channel();
            let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
            let session_locks = Arc::new(SessionLocks::new());
            process_incoming_message(
                incoming,
                gateway,
                state,
                vec!["crab".to_string()],
                gateway_actions,
                "owner-1".to_string(),
                session_locks,
                false,
                None, // v3_liveness: このテストは V3 二重受信ゲートの対象外
                None,
                event_tx,
                registry,
                record_only,
                mark_seen,
                None,
            )
            .await;
        });
    })
}

/// row 116-117 (a): record-only は読まれるまで 👀 なし。
#[test]
fn record_only_does_not_add_seen_until_read() {
    let logs = seen_reaction_logs(
        discord_msg_with_id("222", "user-1", "あとで読む", "1111111111111111111"),
        true,
        false,
        true,
    );
    assert!(
        !logs.contains("👀"),
        "record-only なのに 👀 を付けようとしている: {logs}"
    );
}

/// row 116-117 (b): ターン開始で読んだ分に 👀。record-only でも、文脈に含まれたら付く。
#[test]
fn record_only_gets_seen_when_included_in_a_turn() {
    let logs = seen_reaction_logs(
        discord_msg_with_id("222", "user-1", "合流分", "4444444444444444444"),
        true,
        true,
        true,
    );
    assert!(
        logs.contains("👀"),
        "読むターンに含めた record-only に 👀 が無い: {logs}"
    );
    assert!(
        logs.contains("4444444444444444444"),
        "👀 の付与先が元の投稿になっていない: {logs}"
    );
}

/// row 116-117 (b): ターン開始で読んだ分に 👀。
#[test]
fn turn_start_adds_seen_for_messages_in_context() {
    let logs = seen_reaction_logs(
        discord_msg_with_id("222", "user-1", "今読む", "2222222222222222222"),
        false,
        true,
        true,
    );
    assert!(
        logs.contains("👀"),
        "ターン開始で読んだのに 👀 を付けていない: {logs}"
    );
    assert!(
        logs.contains("Failed to add reaction"),
        "リアクション付与を試みていない: {logs}"
    );
    assert!(
        logs.contains("2222222222222222222"),
        "👀 の付与先が元の投稿になっていない: {logs}"
    );
}

/// row 116-117 (c): whitelist 落ちは従来どおり 👀 なし。
#[test]
fn whitelist_drop_does_not_add_seen() {
    let logs = seen_reaction_logs(
        discord_msg_with_id("222", "user-1", "対象外", "3333333333333333333"),
        false,
        true,
        false,
    );
    assert!(
        !logs.contains("👀"),
        "whitelist 落ちなのに 👀 を付けようとしている: {logs}"
    );
}

/// #543 本体: 合流窓が別 sender の複数メッセージを含んでも、**全メッセージが個別に記録され**、
/// **run は 1 回だけ**起きる。run は DB から会話全体を読むので、合流分は全部・正しい帰属で
/// 文脈に入る（＝「ユーザーの発言を捨てる」ことをしない）。
#[tokio::test]
async fn debounced_window_records_every_message_but_runs_once() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let session_locks = Arc::new(SessionLocks::new());

    // フラッシュの再現: 非トリガーは record-only、内容のある最後のメッセージが run トリガー。
    process_incoming_message(
        discord_msg("222", "agent-a", "エージェントAの発言"),
        gateway.clone(),
        state.clone(),
        vec!["crab".to_string()],
        gateway_actions.clone(),
        "owner-1".to_string(),
        session_locks.clone(),
        false,
        None, // v3_liveness: このテストは V3 二重受信ゲートの対象外
        None,
        event_tx.clone(),
        registry.clone(),
        true,  // 非トリガー（record-only）
        false, // この呼び出し単体ではまだ読んでいない
        None,
    )
    .await;
    process_incoming_message(
        discord_msg("222", "agent-c", "エージェントCの発言"),
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
        false, // トリガー（run を起こす）
        true,  // このターンで読む
        None,
    )
    .await;

    state.wait_for_run().await;

    let records = state.inbound_records.lock().unwrap().clone();
    assert!(
        records.contains(&"エージェントAの発言".to_string()),
        "records={records:?}"
    );
    assert!(
        records.contains(&"エージェントCの発言".to_string()),
        "records={records:?}"
    );
    assert_eq!(records.len(), 2, "合流しても畳まず 2 件記録: {records:?}");
    assert_eq!(state.runs.lock().unwrap().len(), 1, "合流窓の run は 1 回");
}

/// #556: デバウンス**バッファ**のキーは **channel だけ**（権限では割らない）。同一 channel は
/// 送信者・権限が何であれ同じバッファ、別 channel は別バッファ。権限による run 分割は
/// フラッシュ時のグループ化（`consecutive_trust_groups`）で行うので、キー自体は caller を見ない。
#[test]
fn debounce_window_key_is_channel_only() {
    let a = discord_msg("222", "owner-1", "hi");
    let b = discord_msg("222", "gaikoku", "yo");
    let other_channel = discord_msg("999", "owner-1", "hi");

    assert_eq!(
        debounce_window_key(&a),
        debounce_window_key(&b),
        "同一 channel は送信者（権限）が違っても同じバッファに入るべき"
    );
    assert_ne!(
        debounce_window_key(&a),
        debounce_window_key(&other_channel),
        "別 channel は別バッファのはず"
    );
}

/// #556: フラッシュ時のグループ化 = 到着順のまま「連続した同一 trust_level」で切る。
/// オーナー指示の 4 ケースを固定する（**権限が揃った場合**の挙動。#489 未修正の実運用では
/// co_agent が `Agent`(=0) に落ちるので owner→co_agent は同権限にならない点に注意）。
///
/// run の回数 = 「内容のあるメッセージを含むグループ」の数。4 ケースは全員が内容ありなので
/// run 回数 = グループ数。
#[test]
fn flush_groups_consecutive_same_privilege() {
    use opencrab_actions::{consecutive_trust_groups, CallerIdentity};
    let owner = CallerIdentity::Owner;
    let co_agent = CallerIdentity::CoAgent {
        agent_id: "agent-a".to_string(),
    };
    let external = CallerIdentity::Agent;

    // trust_level: Owner=2, CoAgent=2(owner 等価), Agent=0。
    let levels =
        |cs: &[&CallerIdentity]| -> Vec<u8> { cs.iter().map(|c| c.trust_level()).collect() };

    // A(owner) → D(外部) → B(co_agent) = [2,0,2] → [A][D][B] = 3 グループ（run 3 回）。
    assert_eq!(
        consecutive_trust_groups(&levels(&[&owner, &external, &co_agent])),
        vec![(0, 1), (1, 1), (2, 1)],
        "owner→外部→co_agent は権限が 2,0,2 と交互なので 3 グループ"
    );
    // A(owner) → B(co_agent) → D(外部) = [2,2,0] → [A,B][D] = 2 グループ（run 2 回）。
    assert_eq!(
        consecutive_trust_groups(&levels(&[&owner, &co_agent, &external])),
        vec![(0, 2), (2, 1)],
        "owner→co_agent は同権限(2)で合流、外部(0)は別グループ"
    );
    // A(owner) → A(owner) = [2,2] → [A,A] = 1 グループ（run 1 回）。
    assert_eq!(
        consecutive_trust_groups(&levels(&[&owner, &owner])),
        vec![(0, 2)],
        "同一 owner 連続は 1 グループ"
    );
    // D(外部) → D(外部) → A(owner) = [0,0,2] → [D,D][A] = 2 グループ（run 2 回）。
    assert_eq!(
        consecutive_trust_groups(&levels(&[&external, &external, &owner])),
        vec![(0, 2), (2, 1)],
        "外部連続は 1 グループ、owner は別グループ"
    );
}

/// #556 統合: **権限レベルが違うメッセージは同一 channel でも別グループ＝別 run**に分かれる
/// 通しテスト（`run_discord_loop` の flush を一巡）。ここが降格を防ぐ核心: owner の run は
/// caller=Owner のまま走り、外部発言に引きずられない。
///
/// harness の `resolve_caller` は owner-1 を `Owner`(rank 2)・それ以外を `TrustedUser`(rank 1)
/// に解決する。owner→外部は [owner][外部] の 2 グループなので run は 2 回。窓を丸ごと 1 run に
/// する（グループ化を外す）とこのテストが run 1 回で落ちる（回帰ガード）。記録は個別に 2 件。
#[tokio::test]
async fn different_privilege_messages_split_into_separate_runs_through_the_loop() {
    let (state, gateway, gateway_actions) = make_deps();
    let (tx, rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());

    let loop_state = state.clone();
    let handle = tokio::spawn(super::run_discord_loop(
        gateway,
        loop_state,
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        None,
        Some((tx.clone(), rx)),
        false,
        None, // v3_liveness: このテストは V3 二重受信ゲートの対象外
        None,
        registry,
    ));

    // 同一 channel・2 秒窓に Owner（sender=owner-1, rank 2）→ 非 Owner（sender=gaikoku,
    // rank 1=TrustedUser）。連続同権限グループ化なので [owner][外部] の 2 グループ＝2 run。
    for (sender, text) in [
        ("owner-1", "オーナーの発言"),
        ("gaikoku", "外部ユーザーの発言"),
    ] {
        tx.send(super::LoopEvent::IncomingMessage(discord_msg(
            "222", sender, text,
        )))
        .expect("event loop receiver closed");
    }

    state.wait_for_runs(2).await;

    let records = state.inbound_records.lock().unwrap().clone();
    assert_eq!(
        records.len(),
        2,
        "全メッセージ個別に記録される（畳まない・落とさない）: {records:?}"
    );
    assert!(
        records.contains(&"オーナーの発言".to_string()),
        "{records:?}"
    );
    assert!(
        records.contains(&"外部ユーザーの発言".to_string()),
        "{records:?}"
    );
    assert_eq!(
        state.runs.lock().unwrap().len(),
        2,
        "権限が違うメッセージは別グループ＝別 run（owner と外部で 2 回）"
    );

    // 降格しない: owner の run は caller=Owner、外部の run は caller=TrustedUser。
    // どちらの run にどちらの caller が載るかは順序に依存しうるので集合で確認する。
    let callers = vec![state.observed_caller(0), state.observed_caller(1)];
    assert!(
        callers.contains(&CallerIdentity::Owner),
        "owner の指示は owner 権限の run で走るべき（降格しない）: {callers:?}"
    );
    assert!(
        callers.contains(&CallerIdentity::TrustedUser),
        "外部発言は非 owner 権限の run で走るべき: {callers:?}"
    );

    handle.abort();
}

/// #543: run トリガーは「内容のある最後のメッセージ」。末尾が空でも内容のある方を選び、
/// 全メッセージが空のときだけ run を起こさない（間引きで run を消さない・空を選ばない）。
#[test]
fn run_trigger_selection_uses_latest_message_with_content() {
    let has = discord_msg("222", "u", "本文あり");
    let empty = discord_msg("222", "u", "");
    assert!(incoming_has_content(&has));
    assert!(!incoming_has_content(&empty));

    let window = [has, empty];
    assert_eq!(window.iter().rposition(incoming_has_content), Some(0));

    let all_empty = [discord_msg("222", "u", ""), discord_msg("222", "u", "")];
    assert_eq!(all_empty.iter().rposition(incoming_has_content), None);
}

/// #543/#556 統合: **実イベントループ（`run_discord_loop`）の flush を一巡させる**通しテスト。
///
/// 個別ユニットだけでなく、バッファ蓄積 → デバウンス期限 → フラッシュ → 全メッセージ記録 →
/// run 1 回、という配線そのものを固定する。ここは**全員が同権限（非オーナー = TrustedUser）
/// なので連続同権限グループは 1 つ**＝ run 1 回になる（権限が混ざる場合の分割は
/// `different_privilege_messages_split_into_separate_runs_through_the_loop` が固定する）。
/// **末尾に空メッセージ**を混ぜて「トリガー＝グループ内の内容のある**最後**」（単なる最後では
/// ない）も守る: 空をトリガーに選ぶと run が消え、本テストが落ちる。
#[tokio::test]
async fn debounce_flush_records_all_messages_and_runs_once_through_the_loop() {
    let (state, gateway, gateway_actions) = make_deps();
    let (tx, rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());

    let loop_state = state.clone();
    let handle = tokio::spawn(super::run_discord_loop(
        gateway,
        loop_state,
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        None,
        Some((tx.clone(), rx)),
        false,
        None, // v3_liveness: このテストは V3 二重受信ゲートの対象外
        None,
        registry,
    ));

    // 同一チャンネル・同権限（非オーナー = TrustedUser）の内容あり 2 通 ＋ 末尾に空 1 通。
    // 別 sender でも同権限なので 1 グループにまとまり、1 回のフラッシュで 1 run。
    for (sender, text) in [
        ("agent-a", "エージェントAの発言"),
        ("agent-c", "エージェントCの発言"),
        ("agent-c", ""),
    ] {
        tx.send(super::LoopEvent::IncomingMessage(discord_msg(
            "222", sender, text,
        )))
        .expect("event loop receiver closed");
    }

    // 2 秒窓のフラッシュ → トリガー（内容のある最後 = エージェントC）が run を起こすのを待つ。
    state.wait_for_run().await;

    let records = state.inbound_records.lock().unwrap().clone();
    assert_eq!(
        records.len(),
        2,
        "窓の内容ありメッセージは全部個別に記録される（畳まない・落とさない）: {records:?}"
    );
    assert!(
        records.contains(&"エージェントAの発言".to_string()),
        "{records:?}"
    );
    assert!(
        records.contains(&"エージェントCの発言".to_string()),
        "{records:?}"
    );
    assert_eq!(
        state.runs.lock().unwrap().len(),
        1,
        "窓の run は 1 回（末尾の空メッセージでトリガーを落とさない）"
    );

    handle.abort();
}
