use super::*;
use opencrab_core::a2ui::{
    PendingUiSurface, RenderError, RenderedMessage, UiRenderer, UiResponseSink, UserActionResponse,
};
use opencrab_gateway::GatewayCaller;
use std::sync::Mutex;

/// 最小の `UiRenderer` フェイク。描画要求を記録するだけ。
struct FakeRenderer {
    rendered: Mutex<Vec<(String, String)>>,
}

#[async_trait::async_trait]
impl UiRenderer for FakeRenderer {
    async fn render(
        &self,
        surface_id: &str,
        _components: &[A2uiComponent],
        channel: &RenderTarget,
    ) -> Result<RenderedMessage, RenderError> {
        self.rendered
            .lock()
            .unwrap()
            .push((surface_id.to_string(), channel.channel_id.clone()));
        Ok(RenderedMessage {
            platform: channel.platform.clone(),
            message_id: Some("msg-1".into()),
            channel_id: channel.channel_id.clone(),
        })
    }

    async fn update_on_response(
        &self,
        _rendered: &RenderedMessage,
        _response: &UserActionResponse,
    ) -> Result<(), RenderError> {
        Ok(())
    }

    async fn update_on_timeout(&self, _rendered: &RenderedMessage) -> Result<(), RenderError> {
        Ok(())
    }
}

#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<UiResponseEvent>>,
}

impl UiResponseSink for RecordingSink {
    fn on_ui_response(&self, ev: UiResponseEvent) {
        self.events.lock().unwrap().push(ev);
    }
}

fn surface(with_pending: bool, owner_id: &str) -> (A2uiSurface, Arc<RecordingSink>) {
    let sink = Arc::new(RecordingSink::default());
    let s = A2uiSurface {
        renderer: Arc::new(FakeRenderer {
            rendered: Mutex::new(Vec::new()),
        }),
        platform: "fake".to_string(),
        owner_id: owner_id.to_string(),
        pending: with_pending.then(|| PendingUiSurface {
            registry: Arc::new(dashmap::DashMap::new()),
            sink: sink.clone(),
        }),
    };
    (s, sink)
}

fn ctx_with_session() -> GatewayCallContext {
    GatewayCallContext::new(GatewayCaller::Owner, "a1").with_session_id("sess-1")
}

fn text_component() -> serde_json::Value {
    json!([{ "id": "t1", "component": "Text", "text": "hi" }])
}

#[tokio::test]
async fn send_ui_without_session_fails_closed() {
    let db = opencrab_db::Db::memory().unwrap();
    let (s, _sink) = surface(true, "owner1");
    let ctx = GatewayCallContext::new(GatewayCaller::Owner, "a1");
    let r = send_ui(
        &db,
        &s,
        &json!({"channel_id": "1", "components": text_component()}),
        &ctx,
    )
    .await;
    assert!(!r.success);
    assert_eq!(
        r.error.unwrap(),
        "send_ui はセッション文脈でのみ実行できます（session_id 不明）"
    );

    // 空文字の session_id も同じく拒否する（"" で登録しない）。
    let ctx = GatewayCallContext::new(GatewayCaller::Owner, "a1").with_session_id("");
    let r = send_ui(
        &db,
        &s,
        &json!({"channel_id": "1", "components": text_component()}),
        &ctx,
    )
    .await;
    assert!(!r.success);
}

#[tokio::test]
async fn send_ui_requires_channel_and_components() {
    let db = opencrab_db::Db::memory().unwrap();
    let (s, _sink) = surface(true, "owner1");
    let ctx = ctx_with_session();

    let r = send_ui(&db, &s, &json!({"components": text_component()}), &ctx).await;
    assert_eq!(r.error.unwrap(), "channel_idパラメータが必要です");

    let r = send_ui(&db, &s, &json!({"channel_id": "1"}), &ctx).await;
    assert_eq!(r.error.unwrap(), "componentsパラメータが必要です");
}

#[tokio::test]
async fn send_ui_registers_pending_with_core_types_only() {
    let db = opencrab_db::Db::memory().unwrap();
    let (s, _sink) = surface(true, "owner-42");
    let ctx = ctx_with_session();

    let r = send_ui(
        &db,
        &s,
        &json!({"channel_id": "555", "components": text_component()}),
        &ctx,
    )
    .await;
    assert!(r.success, "{:?}", r.error);
    let data = r.data.unwrap();
    let interaction_id = data["interaction_id"].as_str().unwrap().to_string();
    assert_eq!(
        data["surface_id"].as_str().unwrap(),
        format!("interaction:{interaction_id}")
    );
    assert_eq!(data["status"], "pending");
    assert_eq!(
        data["message"],
        "UIを送信しました。ユーザーの応答を待機中..."
    );

    let reg = &s.pending.as_ref().unwrap().registry;
    let pending = reg.get(&interaction_id).expect("registered");
    assert_eq!(pending.session_id, "sess-1");
    assert_eq!(pending.agent_id, "a1");
    assert_eq!(pending.target.channel_id, "555");
    assert_eq!(pending.target.platform, "fake");
    // owner 識別子は描画面の値。空文字を渡すと判定が無効になるので固定する。
    assert_eq!(pending.owner_id, "owner-42");

    // DB へは platform 列付きで永続化され、message_id が書き戻る。
    let conn = db.lock().unwrap();
    let row = opencrab_db::queries::get_pending_interaction(&conn, &interaction_id)
        .unwrap()
        .unwrap();
    assert_eq!(row.platform, "fake");
    assert_eq!(row.channel_id, "555");
    assert_eq!(row.message_id.as_deref(), Some("msg-1"));
    // 再開先のセッションは DB 行にも入る（#196）。ここが空だとプロセス再起動後に
    // 「どの会話へ戻すか」が引けない。
    assert_eq!(row.session_id, "sess-1");
    assert!(row.owner_only);
    assert_eq!(row.timeout_secs, 300);
}

#[tokio::test]
async fn send_ui_without_pending_surface_only_renders() {
    let db = opencrab_db::Db::memory().unwrap();
    let (s, _sink) = surface(false, "owner1");
    let ctx = ctx_with_session();
    let r = send_ui(
        &db,
        &s,
        &json!({"channel_id": "9", "components": text_component()}),
        &ctx,
    )
    .await;
    assert!(r.success);
    assert!(s.pending.is_none());
}

#[tokio::test]
async fn send_ui_clamps_timeout_and_reads_owner_only() {
    let db = opencrab_db::Db::memory().unwrap();
    let (s, _sink) = surface(true, "o");
    let ctx = ctx_with_session();
    let r = send_ui(
        &db,
        &s,
        &json!({
            "channel_id": "1",
            "components": text_component(),
            "timeout_secs": 999999,
            "owner_only": false,
        }),
        &ctx,
    )
    .await;
    let id = r.data.unwrap()["interaction_id"]
        .as_str()
        .unwrap()
        .to_string();
    let reg = &s.pending.as_ref().unwrap().registry;
    assert_eq!(reg.get(&id).unwrap().timeout_secs, 3600);
    let conn = db.lock().unwrap();
    let row = opencrab_db::queries::get_pending_interaction(&conn, &id)
        .unwrap()
        .unwrap();
    assert!(!row.owner_only);
    assert_eq!(row.timeout_secs, 3600);
    // `owner_only=false` でも保留状態の owner 識別子は落とさない（移設前と同じ）。
    assert_eq!(reg.get(&id).unwrap().owner_id, "o");
}

/// 保留状態は**描画物を持たず部品ツリーを持つ**。transport（Discord の Form
/// モーダル等）は応答時にここから描画物を組み直せるので、コアが transport の UI
/// ライブラリの型を知る必要も、型消去も要らない。
#[tokio::test]
async fn pending_state_keeps_the_component_tree_not_a_render() {
    let db = opencrab_db::Db::memory().unwrap();
    let (s, _sink) = surface(true, "o");
    let ctx = ctx_with_session();
    let components = json!([
        { "id": "b1", "component": "Button", "text": "open", "action": { "name": "go" } },
        { "id": "f1", "component": "Form", "title": "T", "children": ["i1"], "action": { "name": "go" } },
        { "id": "i1", "component": "TextInput", "label": "L" },
    ]);
    let r = send_ui(
        &db,
        &s,
        &json!({"channel_id": "1", "components": components}),
        &ctx,
    )
    .await;
    assert!(r.success, "{:?}", r.error);
    let id = r.data.unwrap()["interaction_id"]
        .as_str()
        .unwrap()
        .to_string();
    let reg = &s.pending.as_ref().unwrap().registry;
    let pending = reg.get(&id).unwrap();

    // 再導出の材料が揃っている: 部品ツリーと surface_id。
    assert_eq!(pending.surface_id, format!("interaction:{id}"));
    let ids: Vec<&str> = pending
        .a2ui_components
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(ids, vec!["b1", "f1", "i1"]);
    assert!(matches!(
        pending
            .a2ui_components
            .iter()
            .find(|c| c.id == "f1")
            .map(|c| &c.component_type),
        Some(opencrab_core::a2ui::A2uiComponentType::Form { .. })
    ));
}

#[tokio::test]
async fn timeout_fires_sink_and_removes_registration() {
    let db = opencrab_db::Db::memory().unwrap();
    let (s, sink) = surface(true, "o");
    let ctx = ctx_with_session();
    let r = send_ui(
        &db,
        &s,
        // clamp の下限 10 秒まで縮められるが、テストでは登録解除を手で行う。
        &json!({"channel_id": "77", "components": text_component(), "timeout_secs": 10}),
        &ctx,
    )
    .await;
    let id = r.data.unwrap()["interaction_id"]
        .as_str()
        .unwrap()
        .to_string();

    // タイムアウト経路と同じ内容を直接検証する（sleep させない）。
    let reg = s.pending.as_ref().unwrap().registry.clone();
    let (_, pending) = reg.remove(&id).unwrap();
    sink.on_ui_response(UiResponseEvent {
        interaction_id: id.clone(),
        session_id: pending.session_id.clone(),
        agent_id: pending.agent_id.clone(),
        target: pending.target.clone(),
        response: A2uiUserAction {
            surface_id: pending.surface_id.clone(),
            component_id: "_timeout".into(),
            action_name: "timeout".into(),
            context: None,
            responder_id: "system".into(),
        },
        caller: pending.caller.clone(),
    });
    let events = sink.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].session_id, "sess-1");
    assert_eq!(events[0].target.channel_id, "77");
    assert_eq!(events[0].response.action_name, "timeout");
    assert_eq!(events[0].response.responder_id, "system");
    assert!(reg.get(&id).is_none());
}

/// #302: タイムアウト経路も**UI を描いた run の呼び出し元**で resume する。
///
/// 実際の監視タスクを回す（時計は止めて即進める）。誰も押さなかっただけで
/// `Agent` へ倒すと、オーナー発のターンが降格して owner/trusted のツールが
/// `policy_allows` で丸ごと消える（#298 と同じ症状）。逆に元が `Agent` の
/// ターンは `Agent` のまま = 昇格経路にはならない。
#[tokio::test(start_paused = true)]
async fn timeout_resume_inherits_the_drawing_run_caller() {
    async fn fire_timeout(caller: GatewayCaller) -> crate::traits::CallerIdentity {
        let db = opencrab_db::Db::memory().unwrap();
        let (s, sink) = surface(true, "o");
        let ctx = GatewayCallContext::new(caller, "a1").with_session_id("sess-1");
        let r = send_ui(
            &db,
            &s,
            &json!({"channel_id": "77", "components": text_component(), "timeout_secs": 10}),
            &ctx,
        )
        .await;
        assert!(r.success);
        // 監視タスクの sleep(10s) を消化させる（start_paused なので実時間は進まない）。
        tokio::time::sleep(std::time::Duration::from_secs(11)).await;
        tokio::task::yield_now().await;
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1, "タイムアウトが sink へ届いていない");
        assert_eq!(events[0].response.action_name, "timeout");
        events[0].caller.clone()
    }

    assert_eq!(
        fire_timeout(GatewayCaller::Owner).await,
        crate::traits::CallerIdentity::Owner,
        "タイムアウトで resume したオーナー発のターンが降格している"
    );
    assert_eq!(
        fire_timeout(GatewayCaller::Agent).await,
        crate::traits::CallerIdentity::Agent,
        "タイムアウトの resume が権限の昇格経路になってはならない"
    );
}

/// #302: 保留登録は**UI を描いた run の呼び出し元**を保持する。
///
/// 応答（クリック・タイムアウト）の resume はここから引き継ぐ。応答者から
/// 導出しないのは、`channel_id` が自由引数で描画先チャンネルと resume 先
/// セッション（`ctx.session_id`）が独立しているため。
#[tokio::test]
async fn pending_registration_records_the_drawing_run_caller() {
    let db = opencrab_db::Db::memory().unwrap();
    let (s, _sink) = surface(true, "owner-42");

    // オーナー発のターンが描いた UI。
    let r = send_ui(
        &db,
        &s,
        &json!({"channel_id": "555", "components": text_component()}),
        &ctx_with_session(),
    )
    .await;
    let id = r.data.unwrap()["interaction_id"]
        .as_str()
        .unwrap()
        .to_string();
    let reg = s.pending.as_ref().unwrap().registry.clone();
    assert_eq!(
        reg.get(&id).unwrap().caller,
        crate::traits::CallerIdentity::Owner,
        "オーナー発のターンが描いた UI の resume が降格する"
    );

    // 最小権限のターンが描いた UI は `Agent` のまま（昇格経路にならない）。
    let agent_ctx = GatewayCallContext::new(GatewayCaller::Agent, "a1").with_session_id("sess-1");
    let r = send_ui(
        &db,
        &s,
        &json!({"channel_id": "555", "components": text_component()}),
        &agent_ctx,
    )
    .await;
    let id = r.data.unwrap()["interaction_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        reg.get(&id).unwrap().caller,
        crate::traits::CallerIdentity::Agent,
        "UI 応答が権限の昇格経路になってはならない"
    );
}

/// プロセス再起動を模す: メモリ上の登録簿は空、DB には `pending` の行だけがある。
///
/// この状態で起動時の掃除を走らせると、行は**期限切れとして明示的に閉じられ**、
/// 閉じた記録から再開先のセッションが引ける（#196）。ボタン押下がどこにも届かない
/// まま行が `pending` で残り続けることはない。
#[tokio::test]
async fn stale_rows_are_closed_with_their_session_after_a_restart() {
    let db = opencrab_db::Db::memory().unwrap();
    let (s, _sink) = surface(true, "o");
    let ctx = ctx_with_session();
    let r = send_ui(
        &db,
        &s,
        &json!({"channel_id": "42", "components": text_component()}),
        &ctx,
    )
    .await;
    let id = r.data.unwrap()["interaction_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 再起動 = メモリ上の登録簿が消える（DB 行だけが残る）。
    let registry = s.pending.as_ref().unwrap().registry.clone();
    registry.clear();
    assert!(registry.get(&id).is_none());

    let conn = db.lock().unwrap();
    let closed = opencrab_db::queries::cleanup_stale_pending_interactions(&conn).unwrap();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].id, id);
    // ここが #196 の要: 閉じた行から再開先のセッションが引ける。
    assert_eq!(closed[0].session_id, "sess-1");
    assert_eq!(closed[0].agent_id, "a1");
    assert_eq!(closed[0].platform, "fake");
    assert_eq!(closed[0].channel_id, "42");

    let row = opencrab_db::queries::get_pending_interaction(&conn, &id)
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "timeout");
}

#[test]
fn send_ui_definition_is_stable() {
    let def = send_ui_definition();
    assert_eq!(def.name, "send_ui");
    assert!(def
        .description
        .starts_with("A2UIコンポーネントで構成されたUIを送信し"));
    assert_eq!(
        def.parameters["required"],
        json!(["channel_id", "components"])
    );
}

/// `A2uiUserAction` の型が `UiResponseEvent` の一部として運ばれることの確認と、
/// **本文（再注入テキスト）を持たない**ことの構造的な固定（#152 の二重返信対策と
/// 同じ契約）。フィールドを足すとこのテストのフィールド網羅が落ちる。
#[test]
fn ui_response_event_carries_no_reply_body() {
    let ev = UiResponseEvent {
        interaction_id: "i".into(),
        session_id: "s".into(),
        agent_id: "a".into(),
        target: RenderTarget {
            channel_id: "c".into(),
            platform: "p".into(),
        },
        response: A2uiUserAction {
            surface_id: "sf".into(),
            component_id: "cid".into(),
            action_name: "an".into(),
            context: None,
            responder_id: "r".into(),
        },
        caller: crate::traits::CallerIdentity::Owner,
    };
    // 分解束縛で全フィールドを列挙する。本文フィールドを足すとここが落ちる。
    let UiResponseEvent {
        interaction_id,
        session_id,
        agent_id,
        target,
        response,
        caller,
    } = ev;
    assert_eq!(interaction_id, "i");
    assert_eq!(session_id, "s");
    assert_eq!(agent_id, "a");
    assert_eq!(target.platform, "p");
    assert_eq!(response.action_name, "an");
    assert_eq!(caller, crate::traits::CallerIdentity::Owner);
}
