use super::super::*;
use super::support::*;
use opencrab_gateway::GatewayCaller;

// ---- #157 S7: ピアレビュー依頼（request_peer_review）の gateway 非依存化 ----

/// 素テキスト配送口を提供する inner のフェイク（Discord の代役）。
struct DeliveryProvidingInner {
    delivery: Arc<FakeTextDelivery>,
    calls: std::sync::Mutex<Vec<String>>,
    /// true なら `request_peer_review` を**再定義**する（negative assert 用）。
    redefines_peer_review: bool,
}

/// 送信を記録するだけの [`TextDelivery`]。Discord と同じ規約
/// （数値宛先 / `<@id>` / 1900 chars）を模す。
#[derive(Default)]
struct FakeTextDelivery {
    sent: std::sync::Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl opencrab_core::text_delivery::TextDelivery for FakeTextDelivery {
    fn validate_target(&self, target: &str) -> Result<(), String> {
        if target.parse::<u64>().is_ok() {
            Ok(())
        } else {
            Err(format!("無効なchannel_id: {target}"))
        }
    }
    fn mention(&self, user_id: &str) -> String {
        format!("<@{user_id}>")
    }
    fn chunk_limit(&self) -> usize {
        1900
    }
    async fn send_text(&self, target: &str, text: &str) -> Result<(), String> {
        self.sent
            .lock()
            .unwrap()
            .push((target.to_string(), text.to_string()));
        Ok(())
    }
}

impl DeliveryProvidingInner {
    fn new() -> Self {
        Self {
            delivery: Arc::new(FakeTextDelivery::default()),
            calls: std::sync::Mutex::new(Vec::new()),
            redefines_peer_review: false,
        }
    }
    /// transport が誤って移設済みツールを再定義した構成。
    fn redefining() -> Self {
        Self {
            redefines_peer_review: true,
            ..Self::new()
        }
    }
}

#[async_trait]
impl GatewayActions for DeliveryProvidingInner {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        // transport 側は `request_peer_review` を**定義しない**（移設済み）。
        let mut defs = vec![GatewayActionDef {
            name: "fake_transport_tool".to_string(),
            class: opencrab_gateway::ToolClass {
                dispatch: opencrab_gateway::DispatchMode::Inline,
                sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                sharing: opencrab_gateway::ToolSharing::AgentBound,
            },
            description: "x".to_string(),
            parameters: json!({"type": "object"}),
        }];
        if self.redefines_peer_review {
            defs.push(GatewayActionDef {
                name: "request_peer_review".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::Blocked,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "transport の古い実装".to_string(),
                parameters: json!({"type": "object"}),
            });
        }
        defs
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
    fn text_delivery(&self) -> Option<Arc<dyn opencrab_core::text_delivery::TextDelivery>> {
        Some(self.delivery.clone())
    }
}

/// `own_definitions()` に `request_peer_review` が 1 件だけある（transport 非依存で
/// 全ターンに露出）。消すと分類・sub-engine 遮断の属性検査が空振りする。
#[test]
fn request_peer_review_is_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    assert_eq!(
        defs.iter()
            .filter(|d| d.name == "request_peer_review")
            .count(),
        1,
        "request_peer_review must be defined exactly once in own_definitions"
    );
}

/// **移設の本題（#157）**: Discord 無効の構成（`inner = None` / web・REST・heartbeat・
/// Nostr のターン）でも `request_peer_review` が**定義に現れる**。
///
/// `send_ui`（描画面が無いと露出しない）とはここが違う: 配送口が無いのは
/// 「送れない」だけで、ツールの存在自体を transport の有無に依存させない。
#[test]
fn request_peer_review_is_defined_even_without_discord() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    assert!(
        names.contains(&"request_peer_review".to_string()),
        "Discord 無効の構成でも定義に出ること: {names:?}"
    );
}

/// **移設の本題**: transport が Discord でなくても、素テキストの配送口を提供すれば
/// 依頼が実際に投稿される（ヘッダ + part X/N）。
#[tokio::test]
async fn request_peer_review_works_for_any_transport_that_provides_delivery() {
    let state = crate::test_app_state();
    let inner = Arc::new(DeliveryProvidingInner::new());
    let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);

    let ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("fake-session-1");
    let r = actions
        .execute(
            "request_peer_review",
            &json!({"content": "raw diff", "channel_id": "555"}),
            &ctx,
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    let data = r.data.unwrap();
    assert_eq!(data["channel_id"], "555");
    assert_eq!(data["parts"], 1);
    assert_eq!(
        data["message"],
        "ピアレビュー依頼を投稿しました。[Peer Review] で始まる返信を待ってください。"
    );

    // ヘッダ + part 1/1 の 2 通が配送口へ出た。
    let sent = inner.delivery.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].0, "555");
    assert!(sent[0].1.starts_with("[Peer Review Request] from agent-x"));
    assert_eq!(sent[1].1, "part 1/1\nraw diff");

    // **inner へ委譲していない**（own が唯一の実装）。
    assert!(
        !inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "request_peer_review"),
        "request_peer_review must not be delegated to inner: {:?}",
        inner.calls.lock().unwrap()
    );
}

/// 宛先の妥当性判定と文言は transport（配送口）の責務。移設前の
/// `無効なchannel_id: …` がそのまま返る。
#[tokio::test]
async fn invalid_target_error_comes_from_the_transport() {
    let state = crate::test_app_state();
    let inner = Arc::new(DeliveryProvidingInner::new());
    let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);
    let ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("fake-session-1");
    let r = actions
        .execute(
            "request_peer_review",
            &json!({"content": "diff", "channel_id": "not-a-number"}),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert_eq!(r.error.unwrap(), "無効なchannel_id: not-a-number");
    // 1 通も出していない（fail-closed）。
    assert!(inner.delivery.sent.lock().unwrap().is_empty());
}

/// 配送口を持たない transport では**定義には出るが実行は明示エラー**（fail-closed）。
/// 黙って inner へ落とさない。
#[tokio::test]
async fn request_peer_review_is_refused_without_a_delivery() {
    let state = crate::test_app_state();
    let ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("web-session-1");

    // inner なし。
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute(
            "request_peer_review",
            &json!({"content": "diff", "channel_id": "1"}),
            &ctx,
        )
        .await;
    assert!(!r.success);
    // 既存の 8 種のエラー文言は変えていない。ここは移設で新設した文言で、
    // 共有プロンプトが全ターンでレビュー依頼を促すため**次の行動**まで書く。
    assert_eq!(
            r.error.unwrap(),
            "request_peer_review はこのゲートウェイでは利用できません（メッセージを送信できません）。\
             このターンの transport はテキストを送れないため、ピアレビュー依頼は省略して先へ進んでよい。"
        );

    // 配送口を提供しない inner を挟んでも同じ（inner へ委譲しない）。
    let inner = Arc::new(RecordingInner::new(&["some_transport_tool"]));
    let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    assert!(
        names.contains(&"request_peer_review".to_string()),
        "{names:?}"
    );
    let r = actions
        .execute(
            "request_peer_review",
            &json!({"content": "diff", "channel_id": "1"}),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert!(
        !inner.calls().iter().any(|c| c == "request_peer_review"),
        "must not fall through to inner: {:?}",
        inner.calls()
    );
}

/// **sub-engine からの遮断**（移設前は Discord 側テストが固定していた不変条件）。
///
/// `request_peer_review` の定義は `class.sub_engine == Blocked`（`Allowed` ではない）を
/// 名乗るので、合成 gateway が露出していても depth >= 1 では一覧に出ず、名前指定でも
/// 権限拒否（`rejected:` マーカー）になる。
#[tokio::test]
async fn request_peer_review_is_blocked_in_sub_engine() {
    let state = crate::test_app_state();
    let transport = Arc::new(DeliveryProvidingInner::new());

    // 本番と同じ入れ子の配線（`crates/server/src/process.rs`）。
    let depth0: Arc<dyn GatewayActions> = Arc::new(SystemGatewayActions::new(
        state.clone(),
        Some(transport),
        None,
        None,
    ));
    assert!(depth0
        .definitions()
        .iter()
        .any(|d| d.name == "request_peer_review"));
    // 配送口が入れ子の内側まで転送されている（能力を黙って落とさない）。
    assert!(depth0.text_delivery().is_some());

    let depth1: Arc<dyn GatewayActions> = Arc::new(SystemGatewayActions::new(
        state,
        Some(depth0.clone()),
        None,
        None,
    ));
    assert!(depth1.text_delivery().is_some());

    let sub = opencrab_actions::SubEngineGatewayActions::new(depth1);
    let names: Vec<String> = sub.definitions().into_iter().map(|d| d.name).collect();
    assert!(
        !names.contains(&"request_peer_review".to_string()),
        "request_peer_review must NOT be exposed to the sub-engine: {names:?}"
    );

    let r = sub
        .execute(
            "request_peer_review",
            &json!({"content": "diff", "channel_id": "1"}),
            &sub_ctx("subtask-s1"),
        )
        .await;
    assert!(!r.success);
    // 移設前と同じ分類（実在するが許可外 = 権限拒否）。
    let err = r.error.as_deref().unwrap();
    assert!(
        err.starts_with(opencrab_actions::REJECTION_CODE_PREFIX),
        "request_peer_review must be a policy rejection: {err}"
    );
    assert!(
        !err.contains("Unknown gateway action"),
        "分類が「そんなツールは無い」へ退行している: {err}"
    );

    // 多層防御: 定義自身が `class.sub_engine == Blocked` を名乗る（分類の権威は属性）。
    let class = SystemGatewayActions::own_definitions()
        .into_iter()
        .find(|d| d.name == "request_peer_review")
        .expect("request_peer_review が own_definitions() に無い")
        .class;
    assert_eq!(
        class.sub_engine,
        opencrab_gateway::SubEngineAccess::Blocked,
        "request_peer_review は sub-engine 拒否属性を名乗るべき"
    );
}

/// `request_peer_review` は inline（配送系）。分類の権威は定義の `class.dispatch`。
#[test]
fn request_peer_review_stays_inline_after_the_move() {
    let class = SystemGatewayActions::own_definitions()
        .into_iter()
        .find(|d| d.name == "request_peer_review")
        .expect("request_peer_review が own_definitions() に無い")
        .class;
    assert_eq!(class.dispatch, opencrab_gateway::DispatchMode::Inline);
}

/// **negative assert（#157 S7）**: transport（Discord）が `request_peer_review` を
/// 再定義しても own が処理する（委譲パターンにしない）。
///
/// 委譲のままにすると、dedup（own 優先）で定義は own に食われるのに実行は transport の
/// 古い実装へ流れ、レビュアー解決や台帳記録が黙ってバイパスされる。
#[tokio::test]
async fn own_handles_request_peer_review_even_if_the_transport_redefines_it() {
    let state = crate::test_app_state();
    let inner = Arc::new(DeliveryProvidingInner::redefining());
    let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);

    // 定義は 1 件だけ（own 優先の dedup）。
    let defs = actions.definitions();
    assert_eq!(
        defs.iter()
            .filter(|d| d.name == "request_peer_review")
            .count(),
        1
    );

    let ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("fake-session-1");
    let r = actions
        .execute(
            "request_peer_review",
            &json!({"content": "diff", "channel_id": "7"}),
            &ctx,
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    // own の実装が動いた証拠: 配送口へヘッダ + part が出て、inner の execute は
    // 呼ばれていない。
    assert_eq!(inner.delivery.sent.lock().unwrap().len(), 2);
    assert!(
        !inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "request_peer_review"),
        "own must not delegate: {:?}",
        inner.calls.lock().unwrap()
    );
}
