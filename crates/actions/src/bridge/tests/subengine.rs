use super::*;
use crate::bridge::REJECTION_CODE_PREFIX;
use opencrab_gateway::{GatewayActionDef, GatewayActionResult, GatewayCallContext};
use serde_json::json;

// ---- RFC #152 S2: 合成 gateway 注入 + deny-by-default 最外周フィルタ ----

/// server ツール（nostr_generate_key）と transport ツール（report_progress）と、
/// 開放してはならないツール（send_ui）を同時に提供する、合成 gateway のフェイク。
/// `SystemGatewayActions`（server ツール + inner の union）の到達性だけを模す。
///
/// sub-engine の到達可否は各定義の `class.sub_engine` 属性で決まる（PR-2B）ので、
/// フェイクも実属性を再現する: `nostr_generate_key` / `report_progress` は
/// `Allowed`、`send_ui` は `Blocked`。
struct FakeCompositeGateway;

#[async_trait]
impl GatewayActions for FakeCompositeGateway {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        [
            (
                "nostr_generate_key",
                opencrab_gateway::SubEngineAccess::Allowed,
            ),
            (
                "report_progress",
                opencrab_gateway::SubEngineAccess::Allowed,
            ),
            ("send_ui", opencrab_gateway::SubEngineAccess::Blocked),
        ]
        .iter()
        .map(|(n, sub_engine)| GatewayActionDef {
            name: n.to_string(),
            class: opencrab_gateway::ToolClass {
                dispatch: opencrab_gateway::DispatchMode::Inline,
                sub_engine: *sub_engine,
                sharing: opencrab_gateway::ToolSharing::AgentBound,
            },
            description: format!("{n} desc"),
            parameters: json!({"type": "object", "properties": {}}),
        })
        .collect()
    }
    async fn execute(
        &self,
        name: &str,
        _args: &serde_json::Value,
        _ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // 合成 gateway に到達したことを data で可視化する。
        GatewayActionResult {
            success: true,
            data: Some(json!({ "reached": name })),
            error: None,
        }
    }
}

/// deny-by-default: 合成 gateway の definitions 和集合から、許可リストの
/// ツールだけが sub-engine に見える（send_ui 等は消える）。
#[test]
fn subengine_definitions_only_expose_allowlisted_tools() {
    let sub = SubEngineGatewayActions::new(Arc::new(FakeCompositeGateway));
    let names: Vec<String> = sub.definitions().into_iter().map(|d| d.name).collect();
    assert!(
        names.contains(&"report_progress".to_string()),
        "report_progress must remain reachable"
    );
    assert!(
        names.contains(&"nostr_generate_key".to_string()),
        "nostr_generate_key must be reachable after S2 triage"
    );
    assert!(
        !names.contains(&"send_ui".to_string()),
        "send_ui must NOT be exposed to the sub-engine"
    );
}

/// 許可された server ツール（nostr_generate_key）は合成 gateway へ到達・実行できる。
#[tokio::test]
async fn subengine_reaches_allowed_server_tool() {
    let sub = SubEngineGatewayActions::new(Arc::new(FakeCompositeGateway));
    let ctx = GatewayCallContext::new(opencrab_gateway::GatewayCaller::Agent, "a1")
        .with_session_id("subtask-x")
        .with_depth(1);
    let r = sub.execute("nostr_generate_key", &json!({}), &ctx).await;
    assert!(
        r.success,
        "nostr_generate_key must reach the composite gateway"
    );
    assert_eq!(r.data.unwrap()["reached"], "nostr_generate_key");
}

/// 許可されていない server/transport ツール（send_ui）は depth>=1 で到達不能
/// （rejected: マーカー付きで拒否される。合成 gateway には届かない）。
#[tokio::test]
async fn subengine_blocks_disallowed_tool() {
    let sub = SubEngineGatewayActions::new(Arc::new(FakeCompositeGateway));
    let ctx = GatewayCallContext::new(opencrab_gateway::GatewayCaller::Agent, "a1")
        .with_session_id("subtask-x")
        .with_depth(1);
    let r = sub.execute("send_ui", &json!({}), &ctx).await;
    assert!(!r.success, "send_ui must be blocked in the sub-engine");
    let err = r.error.unwrap();
    assert!(
        err.starts_with(REJECTION_CODE_PREFIX),
        "block must be a structural rejection, got: {err}"
    );
    // 合成 gateway の実行痕跡（reached）が data に無い＝届いていない。
    assert!(r.data.is_none());

    // 未知名（実在しないツール）は **拒否マーカーを付けない**。分類器が幻覚の
    // ツール名を「権限で弾かれた」と誤分類すると、リトライや権限系の扱いが壊れる。
    let unknown = sub
        .execute("no_such_tool_at_all", &serde_json::json!({}), &ctx)
        .await;
    assert!(!unknown.success);
    let unknown_err = unknown.error.unwrap();
    assert!(
        !unknown_err.starts_with(REJECTION_CODE_PREFIX),
        "未知名は権限拒否として扱わない: {unknown_err}"
    );
    assert!(
        unknown_err.contains("Unknown gateway action"),
        "未知名は通常の失敗として返す: {unknown_err}"
    );
}

/// **許可リストは MCP スロットを覆わない**（危険の所在を固定する）。
///
/// `BridgedExecutor` は gateway と MCP を別スロットで持つ。sub-engine の許可リストは
/// gateway スロットに被せるものなので、MCP ツールは**素通りする**。したがって
/// 「sub-engine は最小権限」を保つには、MCP を注入する側（`crates/server` の応答生成）で
/// **深さを見て注入しない**必要がある。ここではその前提（＝許可リストに頼れないこと）を
/// 固定する。将来 MCP を許可リスト経由に通す設計へ変えたら、このテストが落ちるので
/// そのとき深さゲートを緩められる。
#[tokio::test]
async fn allowlist_does_not_cover_the_mcp_slot() {
    // gateway 側は許可リストで絞る。
    let gateway: Arc<dyn GatewayActions> =
        Arc::new(SubEngineGatewayActions::new(Arc::new(FakeCompositeGateway)));
    // MCP 側は絞られていない別スロット（同じフェイクを流用して「素通り」を見る）。
    let mcp: Arc<dyn GatewayActions> = Arc::new(FakeCompositeGateway);

    let gateway_names: Vec<String> = gateway.definitions().into_iter().map(|d| d.name).collect();
    assert!(
        !gateway_names.contains(&"send_ui".to_string()),
        "gateway スロットは許可リストで絞られる: {gateway_names:?}"
    );

    let mcp_names: Vec<String> = mcp.definitions().into_iter().map(|d| d.name).collect();
    assert!(
        mcp_names.contains(&"send_ui".to_string()),
        "MCP スロットは許可リストを通らない（この前提が崩れたら深さゲートを見直す）: {mcp_names:?}"
    );
}

/// report_progress は引き続き transport gateway へ委譲され動く（S1 挙動不変）。
#[tokio::test]
async fn subengine_report_progress_still_reaches_inner() {
    let sub = SubEngineGatewayActions::new(Arc::new(FakeCompositeGateway));
    let ctx = GatewayCallContext::new(opencrab_gateway::GatewayCaller::Agent, "a1")
        .with_session_id("subtask-x")
        .with_depth(1);
    let r = sub.execute("report_progress", &json!({}), &ctx).await;
    assert!(r.success);
    assert_eq!(r.data.unwrap()["reached"], "report_progress");
}
