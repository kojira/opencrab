// ================================================================================
// #203 / #184: REST + Discord の実配線で「最後の走行中 subtask の停止」がセッションを
// 完了させることの e2e 固定（実際に起きていた不具合の再発防止）。
//
// #204 より前の壊れ方: 合成層（`SystemGatewayActions`）が「inner が `cancel_subtask` を
// 定義していれば inner へ委譲」していたため、Discord が有効だと停止が Discord 実装へ
// 流れ、REST の完了受け口（`RestCompletionSink::on_subtask_cancelled`）が**一度も
// 呼ばれず**、セッションが永久に `active` のまま残っていた。#204 で委譲を撤去したが、
// 「transport gateway が inner として配線された構成」を実際に作るテストが無かったため、
// 配線全体（共有 registry → 停止の実体 → 停止 sink → `sessions.status`）が繋がって
// いることは読解でしか裏付けられていなかった。
//
// ## なぜ HTTP エンドポイントを叩かないのか
//
// `send_agent_message` は run のあとに必ず `complete_session_if_idle`（registry が空なら
// `completed`）を通す。つまり **sink が一度も呼ばれなくても、同じリクエストの終わりで
// セッションは `completed` になる** = HTTP 層の観測ではこの不具合を検知できない
// （旧実装でも緑になる）。そこで停止ターンだけはハンドラ step 9 と同一の `RunRequest`
// （REST の sink + 共有 registry + transport gateway を inner）を組んで
// `process::run_agent_response` を直接呼び、完了が **停止 sink 経由でだけ**起きることを
// 観測する。実ネットワークには出ない（`DiscordGatewayActions::from_token` が内部で
// 組む Http クライアントは接続しない）。
// ================================================================================

/// 「走行中 subtask を 1 本抱えた REST セッション」を作り、`inner` を transport gateway
/// として配線した run から `cancel_subtask` を呼ぶ。
///
/// 返り値は [`CancelObservation`]（assert は呼び出し側が行う）。
///
/// `make_inner` はハンドラと同じ材料（共有 DB / workspace_base）から transport gateway を
/// 組むためのファクトリ。本番（`send_agent_message` step 6）と同じく `state.db` を渡す。
#[cfg(feature = "discord")]
async fn cancel_last_subtask_in_rest_run_with_inner(
    make_inner: impl FnOnce(opencrab_db::Db, String) -> Arc<dyn opencrab_gateway::GatewayActions>,
) -> CancelObservation {
    let (app, db, mock, state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "DiscordWired", "TestPersona").await;
    let session_id = format!("agent-msg-{agent_id}-u1");

    // 1. 走行中 subtask を共有 registry へ入れた状態で HTTP を 1 回通し、sessions 行を
    //    作りつつ `active` のまま残す（= 本番の「dispatch 済みでまだ走っている」状態）。
    let registry = state.subtask_registries.registry_for(&session_id);
    let handle = insert_running_subtask(&registry, "st-dw-1", &session_id, &agent_id);
    mock.push_text_response("走らせています");
    let (status, _) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "長いのをやって", "user_id": "u1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        session_status(&db, &session_id).as_deref(),
        Some("active"),
        "前提が崩れている: 走行中 subtask があるのに session が active でない"
    );

    // 2. 停止ターン。`send_agent_message` step 9 と同一の RunRequest を組む。
    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-cancel-dw".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "cancel_subtask".to_string(),
            arguments: serde_json::json!({"subtask_id": "st-dw-1"}).to_string(),
        },
    }]);
    mock.push_text_response("止めました");

    let sink: Arc<dyn opencrab_actions::SubtaskCompletionSink> =
        Arc::new(opencrab_server::api::agents_messages::RestCompletionSink {
            db: db.clone(),
            registry: registry.clone(),
            state: state.clone(),
            agent_name: "DiscordWired".to_string(),
        });
    let run_req = opencrab_actions::RunRequest::new(
        &agent_id,
        "DiscordWired",
        &session_id,
        "system",
        "user: さっきのを止めて",
        "rest",
        opencrab_actions::CallerIdentity::Agent,
    )
    .with_dispatch(Some(registry.clone()), sink)
    .with_gateway_actions(make_inner(db.clone(), state.workspace_base.clone()));
    opencrab_server::process::run_agent_response(&state, run_req)
        .await
        .expect("停止ターンの run が失敗した");

    // 観測だけして返す（assert は呼び出し側。症状 = `sessions.status` を先に主張させたい）。
    let removed_from_registry = !registry.contains_key("st-dw-1");
    let aborted = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .map(|r| r.unwrap_err().is_cancelled())
        .unwrap_or(false);
    CancelObservation {
        session_status: session_status(&db, &session_id),
        removed_from_registry,
        aborted,
    }
}

/// [`cancel_last_subtask_in_rest_run_with_inner`] の観測結果。
#[cfg(feature = "discord")]
struct CancelObservation {
    /// 停止後の親セッションの `sessions.status`（本題。`completed` でなければ #184 の再発）。
    session_status: Option<String>,
    /// 共有 registry から当該 subtask が外れたか。
    removed_from_registry: bool,
    /// subtask のタスクが実際に abort されたか。
    aborted: bool,
}

/// **#184 の実害バグの e2e 固定**: Discord の gateway actions を実際に inner として
/// 配線した REST の run で最後の走行中 subtask を停止すると、セッションが `completed`
/// になる（停止 sink が発火する唯一の経路）。
///
/// 落ちるとき: 合成層が停止を own で処理しなくなったとき（inner へ委譲する / own の
/// 分岐から sink 通知が抜けるなど）。Discord は #204 以降 `cancel_subtask` を定義しない
/// ので、委譲すれば `Unknown action` になり sink は呼ばれない。
#[cfg(feature = "discord")]
#[tokio::test]
async fn test_rest_cancel_completes_session_with_discord_gateway_wired() {
    let obs = cancel_last_subtask_in_rest_run_with_inner(|db, workspace_base| {
        // 接続しない（Http クライアントを組むだけ）。Discord API は一度も叩かない。
        // serenity の型は discord クレート内（from_token）に閉じる。
        Arc::new(opencrab_discord::DiscordGatewayActions::from_token(
            "dummy-token",
            db,
            workspace_base,
            None,
        ))
    })
    .await;
    assert_eq!(
        obs.session_status.as_deref(),
        Some("completed"),
        "REST + Discord 配線で最後の走行中 subtask を停止したのにセッションが completed に\
         ならない（RestCompletionSink::on_subtask_cancelled が呼ばれていない = #184 の再発）"
    );
    assert!(
        obs.removed_from_registry,
        "停止が共有 registry に到達していない（not found のまま）"
    );
    assert!(obs.aborted, "停止したのに subtask が abort されていない");
}

/// **#204 前の構成そのものの再現**: inner（Discord 相当）が `cancel_subtask` を**同名で
/// 定義していても**、停止は own が処理してセッションが `completed` になる。
///
/// 落ちるとき: 合成層の停止を `report_progress` と同じ「inner が定義していれば委譲」
/// パターンに戻したとき。委譲先は sink を触らないので、セッションは `active` のまま残る
/// （= #184 で報告された永久 active そのもの）。
#[cfg(feature = "discord")]
#[tokio::test]
async fn test_rest_cancel_completes_session_even_if_inner_defines_cancel_subtask() {
    /// 実際の Discord gateway actions に「`cancel_subtask` の定義と実装」を足した inner。
    /// #204 で撤去した旧 Discord 実装と同じ形（sink を触らずに成功を返す）。
    struct CancelDefiningInner {
        discord: opencrab_discord::DiscordGatewayActions,
        cancel_calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl opencrab_gateway::GatewayActions for CancelDefiningInner {
        fn definitions(&self) -> Vec<opencrab_gateway::GatewayActionDef> {
            let mut defs = self.discord.definitions();
            defs.push(opencrab_gateway::GatewayActionDef {
                name: "cancel_subtask".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "discord cancel (旧実装相当)".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"subtask_id": {"type": "string"}},
                    "required": ["subtask_id"]
                }),
            });
            defs
        }

        async fn execute(
            &self,
            name: &str,
            args: &serde_json::Value,
            ctx: &opencrab_gateway::GatewayCallContext,
        ) -> opencrab_gateway::GatewayActionResult {
            if name == "cancel_subtask" {
                self.cancel_calls
                    .lock()
                    .unwrap()
                    .push(args["subtask_id"].as_str().unwrap_or("?").to_string());
                // 旧 Discord 実装は完了 sink を知らない = セッション整合を取らない。
                return opencrab_gateway::GatewayActionResult {
                    success: true,
                    data: Some(serde_json::json!({"cancelled": true, "reached_inner": true})),
                    error: None,
                };
            }
            self.discord.execute(name, args, ctx).await
        }
    }

    let cancel_calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = cancel_calls.clone();
    let obs = cancel_last_subtask_in_rest_run_with_inner(move |db, workspace_base| {
        Arc::new(CancelDefiningInner {
            discord: opencrab_discord::DiscordGatewayActions::from_token(
                "dummy-token",
                db,
                workspace_base,
                None,
            ),
            cancel_calls: recorded,
        })
    })
    .await;

    let delegated = cancel_calls.lock().unwrap().clone();
    assert_eq!(
        obs.session_status.as_deref(),
        Some("completed"),
        "inner が cancel_subtask を定義していると停止 sink が落ちてセッションが永久 active に\
         なる（#184 の再発 / 委譲パターンへの逆戻り）。inner へ届いた停止: {delegated:?}"
    );
    assert!(
        delegated.is_empty(),
        "cancel_subtask が inner へ委譲されている（own が処理しなければ sink が発火しない）: {delegated:?}"
    );
    assert!(
        obs.removed_from_registry,
        "停止が共有 registry に到達していない（not found のまま）"
    );
    assert!(obs.aborted, "停止したのに subtask が abort されていない");
}

/// #169: 走行中 subtask があるあいだは session を `completed` にしない。
#[tokio::test]
async fn test_rest_session_stays_active_while_subtask_runs() {
    let (app, db, mock, state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Runner", "TestPersona").await;
    let session_id = format!("agent-msg-{agent_id}-u1");

    let registry = state.subtask_registries.registry_for(&session_id);
    let handle = insert_running_subtask(&registry, "st-running", &session_id, &agent_id);

    mock.push_text_response("走らせています");
    let (status, _) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "進捗どう", "user_id": "u1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        session_status(&db, &session_id).as_deref(),
        Some("active"),
        "走行中 subtask があるのに session が completed になっている"
    );
    handle.abort();
}

/// #169 非退行: 走行中 subtask が無ければ従来どおり応答後に `completed` になる。
#[tokio::test]
async fn test_rest_session_completed_when_no_subtask_runs() {
    let (app, db, mock, _state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Plain", "TestPersona").await;
    let session_id = format!("agent-msg-{agent_id}-u1");

    mock.push_text_response("できました");
    let (status, _) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "やって", "user_id": "u1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        session_status(&db, &session_id).as_deref(),
        Some("completed")
    );
}

/// #169: 最後の subtask が決着した時点で `RestCompletionSink` が session を完了させる
/// （走行中は active のままなので、誰かが最後に完了させないと永久 active になる）。
#[tokio::test]
async fn test_rest_sink_completes_session_after_last_subtask_settles() {
    let (app, db, mock, state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Sinker", "TestPersona").await;

    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-sink-1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "learn_from_experience".to_string(),
            arguments: serde_json::json!({
                "skill_name": "sink_check",
                "description": "d",
                "situation_pattern": "s",
                "guidance": "g"
            })
            .to_string(),
        },
    }]);
    mock.push_text_response("開始しました");
    // #638: subtask の決着が**継続ターン**を起こすようになったので、その 1 本分の応答も要る
    // （以前は REST だけ継続しなかったため 2 本で足りていた）。継続ターンが終わってから
    // `sessions.status` の整合が行われる。本文は #631 の最小再現（`HELLO_631` を返させる）に
    // 合わせ、**継続ターンの応答だと一意に分かる文言**にする——下でセッションログに
    // この本文が残ることを assert し、「継続が走った」だけでなく「結果が読める」ことまで留める。
    mock.push_text_response("HELLO_631 を確認しました");

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "覚えて", "user_id": "u1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let session_id = resp["session_id"].as_str().unwrap().to_string();

    let mut completed = false;
    for _ in 0..100 {
        if session_status(&db, &session_id).as_deref() == Some("completed") {
            completed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        completed,
        "全 subtask 決着後も session が completed にならない"
    );
    assert!(!state.subtask_registries.has_running(&session_id));

    // #638/#631 の実症状の錠前: 継続ターンの**本文がセッションログに残る**こと。
    //
    // status が completed になるだけでは「継続が走った」までしか言えない。#631 で利用者が
    // 困っていたのは「subtask の結果を受けた続きの発話が返ってこない」ことなので、その発話が
    // `GET /api/sessions/{id}/logs` の源（memory_sessions）へ永続化されるところまで留める。
    // 継続を削るとこの assert が落ちる（`mock` の 3 本目が消費されないため文言も現れない）。
    let logs = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap()
    };
    assert!(
        logs.iter().any(|l| l.content.contains("HELLO_631")),
        "継続ターンの応答がセッションログに残っていない（#631: 結果を受けた続きが読めない）: {:?}",
        logs.iter().map(|l| l.content.as_str()).collect::<Vec<_>>()
    );
}

