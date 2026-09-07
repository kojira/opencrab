
// ================================================================================
// #298: resume で呼び出し元（CallerIdentity）を落とさない
//
// `policy_allows`（`crates/actions/src/bridge.rs`）は owner_only / trusted_only の
// ツールを **list_tools からも dispatch からも** 落とす。resume の RunRequest を
// `CallerIdentity::Agent` 固定で組むと、オーナー発のターンが subtask 決着の瞬間に
// 降格し、owner/trusted のツールが丸ごと消える（`report_progress` を呼ぶと自分の
// 権限が落ちる、という自爆的な挙動）。
// ================================================================================

/// subtask 完了 resume は、subtask を spawn した元ターンの呼び出し元を保つ。
#[tokio::test]
async fn subtask_resume_preserves_the_original_caller() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());

    process_subtask_completed(
        "discord-crab-111-222".to_string(),
        "crab".to_string(),
        "st-1".to_string(),
        "結果本文".to_string(),
        "progress".to_string(),
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

    assert_eq!(
        state.observed_caller(0),
        CallerIdentity::Owner,
        "オーナー発のターンが subtask 決着で降格している（owner/trusted のツールが消える）"
    );
}

/// 昇格はしない: 元が `Agent` のターンは resume でも `Agent` のまま。
#[tokio::test]
async fn subtask_resume_keeps_agent_turns_as_agent() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());

    process_subtask_completed(
        "discord-crab-111-222".to_string(),
        "crab".to_string(),
        "st-1".to_string(),
        String::new(),
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
        CallerIdentity::Agent,
    )
    .await;

    assert_eq!(
        state.observed_caller(0),
        CallerIdentity::Agent,
        "resume が権限の昇格経路になってはならない"
    );
}

/// A2UI 応答の resume を 1 回走らせ、`RunRequest` に載った呼び出し元を返す。
///
/// 引き継ぐのは**その UI を描いた run の呼び出し元**（`PendingInteraction.caller`）で、
/// 応答した本人（`responder_id`）からは導出しない（#302）。`send_ui` の `channel_id` は
/// 自由引数で、描画先チャンネルと resume 先セッション（`ctx.session_id`）は独立して
/// いるため、応答者から導くと「`Agent` のターンがオーナーの見るチャンネルへ UI を描き、
/// オーナーが押した瞬間にそのセッションが `Owner` で resume する」＝昇格経路になる。
/// クリックは `handle_component_interaction` の owner-only ゲートで既にオーナー限定
/// なので、応答者から導く実利も無い。
async fn interaction_resume_caller(caller: CallerIdentity) -> CallerIdentity {
    let (state, gateway, gateway_actions) = make_deps();

    process_interaction_response(
        "int-1".to_string(),
        "discord-crab-111-222".to_string(),
        "crab".to_string(),
        222,
        "222".to_string(),
        "111".to_string(),
        opencrab_core::a2ui::A2uiUserAction {
            surface_id: "s-1".to_string(),
            component_id: "btn-1".to_string(),
            action_name: "approve".to_string(),
            context: None,
            // 押せるのはオーナーだけ（owner-only ゲート）。この fake の
            // `resolve_caller` は誰であれ `TrustedUser` を返すので、応答者から
            // 導出する実装に戻すと下の 2 本が両方落ちる。
            responder_id: "owner-1".to_string(),
        },
        false,
        gateway,
        state.clone(),
        gateway_actions,
        caller,
    )
    .await;

    state.observed_caller(0)
}

/// 降格しない: 元がオーナー発のターンなら resume も `Owner`。
#[tokio::test]
async fn interaction_response_resume_preserves_the_drawing_run_caller() {
    assert_eq!(
        interaction_resume_caller(CallerIdentity::Owner).await,
        CallerIdentity::Owner,
        "UI 応答の resume が最小権限へ降格している（owner/trusted のツールが消える）"
    );
}

/// 昇格しない: 元が `Agent` のターンが描いた UI は、**オーナーが押しても** `Agent` のまま。
#[tokio::test]
async fn interaction_response_resume_does_not_escalate_agent_turns() {
    assert_eq!(
        interaction_resume_caller(CallerIdentity::Agent).await,
        CallerIdentity::Agent,
        "UI 応答の resume が権限の昇格経路になってはならない"
    );
}

// ---- NO_REPLY の可視化（#317） ----

/// リアクション付与だけを観測する fake（Discord へは出ない）。
#[derive(Default)]
struct FakeReactionGateway {
    /// (channel_id, message_id, emoji) を呼ばれた順に記録する。
    calls: Mutex<Vec<(u64, u64, String)>>,
}

#[async_trait::async_trait]
impl super::ReactionAdder for FakeReactionGateway {
    async fn add_unicode_reaction(
        &self,
        channel_id: u64,
        message_id: u64,
        emoji: &str,
    ) -> anyhow::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push((channel_id, message_id, emoji.to_string()));
        Ok(())
    }
}

fn engine_result(response: &str) -> EngineResult {
    EngineResult {
        response: response.to_string(),
        iterations: 1,
        tool_calls_made: 0,
        stopped_by_limit: false,
        last_posting_utterance_id: None,
        last_generation_had_continuation_speech: false,
        xml_fallback_parses: 0,
    }
}

async fn no_reply_reaction_calls(response: &str, message_id: &str) -> Vec<(u64, u64, String)> {
    let state = FakeRunner::new();
    let gateway = FakeReactionGateway::default();

    super::handle_agent_response(
        delivery_effect(
            Ok(engine_result(response)),
            opencrab_actions::DeliveryContext::default(),
        ),
        "crab",
        "discord-crab-111-222",
        222,
        "222",
        &state,
        &gateway,
        message_id,
    )
    .await;

    let calls = gateway.calls.lock().unwrap();
    calls.clone()
}

/// **`NO_REPLY` を選んだターンは、元の投稿にリアクションが付く。**
///
/// 付かないと、投稿者からは「読んで黙った」のか「落ちて返せなかった」のか区別が
/// つかない（これが #317 の要望そのもの）。宛先（チャンネル・メッセージ）と絵文字まで
/// 固定する — 宛先を取り違えると無関係な投稿にリアクションが付く。
#[tokio::test]
async fn no_reply_marks_the_original_message_with_a_reaction() {
    let calls = no_reply_reaction_calls("NO_REPLY", "1234567890123456789").await;
    assert_eq!(
        calls.len(),
        1,
        "NO_REPLY なのにリアクションが付いていない（黙ったことが誰にも見えない）"
    );
    assert_eq!(calls[0].0, 222, "リアクション先のチャンネルが違う");
    assert_eq!(
        calls[0].1, 1234567890123456789,
        "リアクション先のメッセージが違う"
    );
    // 👀（読んだ）と同じ絵文字にすると 2 つの状態が区別できなくなる。
    assert_eq!(calls[0].2, "🤐", "NO_REPLY の絵文字が変わっている");
    assert_ne!(calls[0].2, "👀", "受信済みマークと同じ絵文字になっている");
}

/// **普通に返答したターンにはリアクションを付けない。**
///
/// 返答があるのに「黙った」マークが付くと意味が反転する。
#[tokio::test]
async fn a_normal_reply_gets_no_no_reply_reaction() {
    let calls = no_reply_reaction_calls("ふつうの返事", "1234567890123456789").await;
    assert!(
        calls.is_empty(),
        "返答したターンに NO_REPLY のリアクションが付いている"
    );
}

// ---- ターン失敗の可視化（#668） ----

/// `Err` を渡したときの handle_agent_response のリアクション付与を観測する。
async fn failure_reaction_calls(message_id: &str) -> Vec<(u64, u64, String)> {
    let state = FakeRunner::new();
    let gateway = FakeReactionGateway::default();

    super::handle_agent_response(
        delivery_effect(
            Err(anyhow::anyhow!("upstream provider exploded")),
            opencrab_actions::DeliveryContext::default(),
        ),
        "crab",
        "discord-crab-111-222",
        222,
        "222",
        &state,
        &gateway,
        message_id,
    )
    .await;

    let calls = gateway.calls.lock().unwrap();
    calls.clone()
}

/// **ターンがエラーで失敗したら、トリガー投稿に ❌ が付く（本文投稿はしない）。**
///
/// エラー本文をチャンネルへ流すと複数エージェント間で反応し合う無限ループになるため、
/// 「失敗した」ことだけをリアクションで可視化する（#668）。宛先（チャンネル・メッセージ）と
/// 絵文字まで固定する — 取り違えると無関係な投稿に ❌ が付く。
#[tokio::test]
async fn a_failed_turn_marks_the_trigger_message_with_a_cross() {
    let calls = failure_reaction_calls("1234567890123456789").await;
    assert_eq!(
        calls.len(),
        1,
        "失敗ターンなのに ❌ が付いていない（失敗が誰にも見えない）"
    );
    assert_eq!(calls[0].0, 222, "リアクション先のチャンネルが違う");
    assert_eq!(
        calls[0].1, 1234567890123456789,
        "リアクション先のメッセージが違う"
    );
    assert_eq!(calls[0].2, "❌", "失敗の絵文字が変わっている");
    assert_ne!(calls[0].2, "👀", "受信済みマークと同じ絵文字になっている");
    assert_ne!(calls[0].2, "🤐", "NO_REPLY マークと同じ絵文字になっている");
}

/// **成功したターンには ❌ を付けない。**
///
/// 成功したのに失敗マークが付くと意味が反転する（他エージェントが不要に反応しうる）。
#[tokio::test]
async fn a_successful_turn_gets_no_failure_reaction() {
    let calls = no_reply_reaction_calls("ふつうの返事", "1234567890123456789").await;
    assert!(
        !calls.iter().any(|c| c.2 == "❌"),
        "成功ターンに失敗リアクション（❌）が付いている"
    );
}

/// **`NO_REPLY`（黙ると決めた）は失敗ではないので ❌ ではなく 🤐 が付く。**
///
/// 「読んで黙った」と「落ちて返せなかった」を絵文字で区別するのが #317/#668 の要点。
#[tokio::test]
async fn no_reply_is_distinct_from_failure() {
    let calls = no_reply_reaction_calls("NO_REPLY", "1234567890123456789").await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].2, "🤐", "NO_REPLY が失敗（❌）と混同されている");
    assert_ne!(calls[0].2, "❌", "黙ったターンに失敗マークが付いている");
}

/// **どんな送信者の投稿にも 👀 が付く**（#317: bot を特別扱いしない）。
///
/// 仕様変更（row 116-117）: 👀 は「処理対象として確定」ではなく
/// **LLM が読んだ（ターン文脈に含めた）時点**。このピンはターンが走る経路なので
/// 付与タイミングは新しい仕様でも同じ観測になる。bot 特別扱いをしないことも不変。
/// 無限ループを止めるのは**自分自身の投稿の除外**（`is_own_message`）であって、
/// bot フラグではない。
///
/// 観測は warn ログで行う。ここは本物の `DiscordGateway`（`test-token`）なので
/// 付与は必ず失敗するが、**付与を試みたこと**（＝配線が生きていること）はログに残る。
/// 付与をスキップした場合は絵文字がログに一切現れない。
#[test]
fn every_sender_gets_the_seen_reaction_including_other_bots() {
    let logs = crate::owner_warning::capture::captured_logs(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (state, gateway, gateway_actions) = make_deps();
            let (event_tx, _event_rx) = create_event_channel();
            let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
            let session_locks = Arc::new(SessionLocks::new());

            let incoming = IncomingMessage::new(
                MessageSource::Discord {
                    guild_id: "111".to_string(),
                    channel_id: "222".to_string(),
                },
                MessageContent::Text("ねえ".to_string()),
                Sender::new("bot-2", "となりのエージェント"),
            )
            .with_metadata(
                "discord_message_id",
                serde_json::Value::String("1234567890123456789".to_string()),
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
        });
    });

    assert!(
        logs.contains("👀"),
        "他エージェントの投稿に 👀 を付けようとしていない（bot を特別扱いしている）: {logs}"
    );
    // スキップ枝（"Skip reaction: invalid message_id"）も emoji と message_id を
    // ログに出すので、上の assert だけでは「試みた」と「諦めた」を区別できない。
    // 付与を**実際に試みた**（= 送信して 401 で失敗した）ことまで固定する。
    assert!(
        logs.contains("Failed to add reaction"),
        "リアクション付与を試みていない（message_id の解析でスキップされている）: {logs}"
    );
    assert!(
        logs.contains("1234567890123456789"),
        "👀 の付与先メッセージが元の投稿になっていない: {logs}"
    );
}

/// **message_id が無いターンでも落ちない**（付与を諦めるだけ）。
///
/// message_id はメタデータ由来で、欠けることがある。ここで panic すると
/// `spawn_serialized` のタスクごと落ち、セッションの応答経路が壊れる。
#[tokio::test]
async fn no_reply_without_a_message_id_is_skipped_not_fatal() {
    assert!(
        no_reply_reaction_calls("NO_REPLY", "").await.is_empty(),
        "message_id が空なのにリアクションを試みている"
    );
    assert!(
        no_reply_reaction_calls("NO_REPLY", "not-a-number")
            .await
            .is_empty(),
        "数値でない message_id でリアクションを試みている"
    );
}
