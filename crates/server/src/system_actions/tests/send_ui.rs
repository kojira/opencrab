use super::super::*;
use super::support::*;
use opencrab_gateway::GatewayCaller;

// ---- #156 S3: A2UI 送信（send_ui）の gateway 非依存化 ----

/// A2UI 描画面を提供する inner のフェイク（Discord の代役）。
struct A2uiProvidingInner {
    surface: Arc<opencrab_core::a2ui::A2uiSurface>,
    calls: std::sync::Mutex<Vec<String>>,
}

struct NoopRenderer;

#[async_trait]
impl opencrab_core::a2ui::UiRenderer for NoopRenderer {
    async fn render(
        &self,
        _surface_id: &str,
        _components: &[opencrab_core::a2ui::A2uiComponent],
        channel: &opencrab_core::a2ui::RenderTarget,
    ) -> Result<opencrab_core::a2ui::RenderedMessage, opencrab_core::a2ui::RenderError> {
        Ok(opencrab_core::a2ui::RenderedMessage {
            platform: channel.platform.clone(),
            message_id: Some("m1".into()),
            channel_id: channel.channel_id.clone(),
        })
    }
    async fn update_on_response(
        &self,
        _rendered: &opencrab_core::a2ui::RenderedMessage,
        _response: &opencrab_core::a2ui::UserActionResponse,
    ) -> Result<(), opencrab_core::a2ui::RenderError> {
        Ok(())
    }
    async fn update_on_timeout(
        &self,
        _rendered: &opencrab_core::a2ui::RenderedMessage,
    ) -> Result<(), opencrab_core::a2ui::RenderError> {
        Ok(())
    }
}

struct CountingUiSink(std::sync::Mutex<usize>);

impl opencrab_core::a2ui::UiResponseSink for CountingUiSink {
    fn on_ui_response(&self, _ev: opencrab_core::a2ui::UiResponseEvent) {
        *self.0.lock().unwrap() += 1;
    }
}

impl A2uiProvidingInner {
    fn new(owner_id: &str) -> Self {
        Self {
            surface: Arc::new(opencrab_core::a2ui::A2uiSurface {
                renderer: Arc::new(NoopRenderer),
                platform: "fake".to_string(),
                owner_id: owner_id.to_string(),
                pending: Some(opencrab_core::a2ui::PendingUiSurface {
                    registry: Arc::new(dashmap::DashMap::new()),
                    sink: Arc::new(CountingUiSink(std::sync::Mutex::new(0))),
                }),
            }),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl GatewayActions for A2uiProvidingInner {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        // transport 側は `send_ui` を**定義しない**（移設済み）。
        vec![GatewayActionDef {
            name: "fake_transport_tool".to_string(),
            class: opencrab_gateway::ToolClass {
                dispatch: opencrab_gateway::DispatchMode::Inline,
                sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                sharing: opencrab_gateway::ToolSharing::AgentBound,
            },
            description: "x".to_string(),
            parameters: json!({"type": "object"}),
        }]
    }
    async fn execute(
        &self,
        name: &str,
        _args: &Value,
        _ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        self.calls.lock().unwrap().push(name.to_string());
        GatewayActionResult {
            success: true,
            data: Some(json!({ "reached_inner": name })),
            error: None,
        }
    }
    fn a2ui_surface(&self) -> Option<Arc<opencrab_core::a2ui::A2uiSurface>> {
        Some(self.surface.clone())
    }
}

/// `own_definitions()` に `send_ui` が 1 件だけある（transport 非依存で全ターンに露出）。
/// 消すと `send_ui` の分類・sub-engine 遮断の属性検査が空振りする。
#[test]
fn send_ui_is_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    assert_eq!(
        defs.iter().filter(|d| d.name == "send_ui").count(),
        1,
        "send_ui must be defined exactly once in own_definitions"
    );
}

/// **移設の本題**: transport 固有の gateway が Discord でなくても、A2UI 描画面を
/// 提供すれば `send_ui` が露出し、実体（gateway 非依存層）が動く。
#[tokio::test]
async fn send_ui_works_for_any_transport_that_provides_a_surface() {
    let state = crate::test_app_state();
    let inner = Arc::new(A2uiProvidingInner::new("owner-1"));
    let actions = SystemGatewayActions::new(state.clone(), Some(inner.clone()), None, None);

    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    assert!(names.contains(&"send_ui".to_string()), "{names:?}");

    let ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("fake-session-1");
    let r = actions
        .execute(
            "send_ui",
            &json!({
                "channel_id": "42",
                "components": [{"id": "t", "component": "Text", "text": "hi"}],
            }),
            &ctx,
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    let interaction_id = r.data.unwrap()["interaction_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 保留状態は transport の描画面の登録簿に載る（コアの型だけ）。
    let surface = inner.a2ui_surface().unwrap();
    let pending = surface.pending.as_ref().unwrap();
    let entry = pending.registry.get(&interaction_id).expect("registered");
    assert_eq!(entry.target.channel_id, "42");
    assert_eq!(entry.target.platform, "fake");
    // オーナー限定ゲートの識別子が空文字にならない（空だと誰でも操作できてしまう）。
    assert_eq!(entry.owner_id, "owner-1");

    // **inner へ委譲していない**（own が唯一の実装）。
    assert!(
        !inner.calls.lock().unwrap().iter().any(|c| c == "send_ui"),
        "send_ui must not be delegated to inner: {:?}",
        inner.calls.lock().unwrap()
    );
}

/// 描画面を持たない transport（web / Nostr / REST / heartbeat）のターンでは
/// **露出しない**（移設前の露出範囲＝Discord 経路のみ、と一致させる）。
/// 名前で呼ばれても inner へ落とさず明示エラー（fail-closed）。
#[tokio::test]
async fn send_ui_is_hidden_and_refused_without_a_surface() {
    let state = crate::test_app_state();
    // inner なし（web / REST / Nostr / heartbeat、Discord feature 無効ビルド）。
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    assert!(!names.contains(&"send_ui".to_string()), "{names:?}");

    let ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("web-session-1");
    let r = actions
        .execute(
            "send_ui",
            &json!({"channel_id": "1", "components": []}),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert_eq!(
        r.error.unwrap(),
        "send_ui はこのゲートウェイでは利用できません（UI を描画できません）"
    );

    // A2UI を提供しない inner を挟んでも同じ（inner へ委譲しない）。
    let inner = Arc::new(RecordingInner::new(&["some_transport_tool"]));
    let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    assert!(!names.contains(&"send_ui".to_string()), "{names:?}");
    let r = actions
        .execute(
            "send_ui",
            &json!({"channel_id": "1", "components": []}),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert!(
        !inner.calls().iter().any(|c| c == "send_ui"),
        "must not fall through to inner: {:?}",
        inner.calls()
    );
}

/// **sub-engine からの遮断**（移設前は Discord 側テストが固定していた不変条件）。
///
/// `send_ui` の定義は `class.sub_engine == Blocked`（`Allowed` ではない）を名乗るので、
/// 合成 gateway が `send_ui` を露出していても depth >= 1 では一覧に出ず、名前指定でも
/// 権限拒否（`rejected:` マーカー）になる。
#[tokio::test]
async fn send_ui_is_blocked_in_sub_engine() {
    let state = crate::test_app_state();
    let transport = Arc::new(A2uiProvidingInner::new("owner-1"));

    // **本番と同じ入れ子の配線**を組む（`crates/server/src/process.rs`）:
    //   depth0: SystemGatewayActions(inner = transport)             ← 親ターン
    //   spawn_subtask が ctx.root_gateway = depth0 の合成 gateway を子へ渡す
    //   depth1: SystemGatewayActions(inner = depth0 の合成 gateway) ← 子ターン
    //           を SubEngineGatewayActions で包む
    // 1 段構成で組むと、内側の合成 gateway が描画面を転送できているかを検出できない。
    let depth0: Arc<dyn GatewayActions> = Arc::new(SystemGatewayActions::new(
        state.clone(),
        Some(transport),
        None,
        None,
    ));
    // 親ターンでは露出する（前提の確認）。
    assert!(depth0.definitions().iter().any(|d| d.name == "send_ui"));

    let depth1: Arc<dyn GatewayActions> = Arc::new(SystemGatewayActions::new(
        state,
        Some(depth0.clone()),
        None,
        None,
    ));
    // 描画面が入れ子の内側まで届いている（届かないと下の拒否分類が
    // 「Unknown gateway action」へ変わる）。
    assert!(
        depth1.definitions().iter().any(|d| d.name == "send_ui"),
        "A2UI 描画面が入れ子の合成 gateway へ転送されていない"
    );

    let sub = opencrab_actions::SubEngineGatewayActions::new(depth1);
    let names: Vec<String> = sub.definitions().into_iter().map(|d| d.name).collect();
    assert!(
        !names.contains(&"send_ui".to_string()),
        "send_ui must NOT be exposed to the sub-engine: {names:?}"
    );

    let r = sub
        .execute(
            "send_ui",
            &json!({"channel_id": "1", "components": []}),
            &sub_ctx("subtask-s1"),
        )
        .await;
    assert!(!r.success, "send_ui must be blocked in the sub-engine");
    // 移設前と同じ分類（実在するが許可外 = 権限拒否）。「そんなツールは無い」に
    // 落ちると幻覚ツール名と同じ扱いになり、拒否の観測が変わる。
    let err = r.error.as_deref().unwrap();
    assert!(
        err.starts_with(opencrab_actions::REJECTION_CODE_PREFIX),
        "send_ui must be a policy rejection, not an unknown tool: {err}"
    );
    assert!(
        !err.contains("Unknown gateway action"),
        "分類が「そんなツールは無い」へ退行している: {err}"
    );

    // 多層防御: 定義自身が `class.sub_engine == Blocked` を名乗る（分類の権威は属性）。
    let send_ui_class = SystemGatewayActions::own_definitions()
        .into_iter()
        .find(|d| d.name == "send_ui")
        .expect("send_ui が own_definitions() に無い")
        .class;
    assert_eq!(
        send_ui_class.sub_engine,
        opencrab_gateway::SubEngineAccess::Blocked,
        "send_ui は sub-engine 拒否属性を名乗るべき"
    );
}

/// `send_ui` は inline（配送系 + ユーザー応答待ち）。分類の権威は定義の `class.dispatch`。
#[test]
fn send_ui_stays_inline_after_the_move() {
    let class = SystemGatewayActions::own_definitions()
        .into_iter()
        .find(|d| d.name == "send_ui")
        .expect("send_ui が own_definitions() に無い")
        .class;
    assert_eq!(class.dispatch, opencrab_gateway::DispatchMode::Inline);
}
