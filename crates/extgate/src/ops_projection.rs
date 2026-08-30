//! 宣言能力の投影（DI 拡張 §8）。extgate live registry の宣言を `GatewayActions` として engine の
//! tool set へ載せ、invoke を背景 subtask 内で await する（option B）。
//!
//! generic で platform 語彙を持たない: operation 名・schema・class は宣言 data のみに由来し、core は
//! 分岐しない。短縮参照（uN/eN/cN）の解決は core 側で `ConversationRefs`（会話ログ初出順・追加永続
//! なし）を使い汎用に行う。gateway は解決後の origin/pubkey から platform ID を導く（層分離・§9.3）。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use opencrab_core::conversation::ConversationRefs;
use opencrab_gateway::{
    DispatchMode, GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext,
    SubEngineAccess, ToolClass, ToolSharing,
};

use crate::operation_calls::invoke_and_wait;
use crate::operations::{GatewayOperationDeclaration, Sharing, SubEngine};
use crate::registry::ExtgateState;

/// 1 binding/session に対する宣言能力の投影。RunRequest.gateway_actions へ載せる。
pub struct ExtgateOpsGatewayActions {
    state: Arc<ExtgateState>,
    instance_id: String,
    binding_id: String,
    session_id: String,
    agent_id: String,
    declarations: Arc<Vec<GatewayOperationDeclaration>>,
}

impl ExtgateOpsGatewayActions {
    /// live registry の宣言 snapshot から投影を作る。宣言があるときだけ Some（空宣言＝能力ゼロ
    /// は None で従来挙動）。
    pub fn for_binding(
        state: Arc<ExtgateState>,
        instance_id: &str,
        binding_id: &str,
        session_id: &str,
        agent_id: &str,
    ) -> Option<Self> {
        let declarations = {
            let reg = state.lock_registry().ok()?;
            reg.get(instance_id)?.declarations.clone()
        };
        if declarations.is_empty() {
            return None;
        }
        Some(Self {
            state,
            instance_id: instance_id.to_string(),
            binding_id: binding_id.to_string(),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            declarations,
        })
    }

    /// payload 内の短縮参照文字列（uN/eN/cN）を実 ID へ解決する（core 側・汎用）。会話ログから
    /// `ConversationRefs` を決定的に再構築して逆引きする。解決できない値はそのまま通す。
    fn resolve_payload(&self, payload: &Value) -> Value {
        let refs = match self.build_refs() {
            Some(r) => r,
            None => return payload.clone(),
        };
        resolve_refs_in_value(payload, &refs)
    }

    fn build_refs(&self) -> Option<ConversationRefs> {
        let conn = self.state.db.lock().ok()?;
        let logs =
            opencrab_db::queries::list_session_logs_by_session(&conn, &self.session_id).ok()?;
        drop(conn);
        Some(ConversationRefs::build(&logs, &self.agent_id))
    }
}

fn map_sub_engine(s: SubEngine) -> SubEngineAccess {
    match s {
        SubEngine::NotExposed => SubEngineAccess::NotExposed,
        SubEngine::Blocked => SubEngineAccess::Blocked,
        SubEngine::Allowed => SubEngineAccess::Allowed,
    }
}

fn map_sharing(s: Sharing) -> ToolSharing {
    match s {
        Sharing::AgentBound => ToolSharing::AgentBound,
        Sharing::ConversationBound => ToolSharing::ConversationBound,
    }
}

/// `uN` / `eN` / `cN`（prefix + 数字）だけを短縮参照とみなす。
fn is_short_ref(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(prefix) = chars.next() else {
        return false;
    };
    if !matches!(prefix, 'u' | 'e' | 'c') {
        return false;
    }
    let rest = chars.as_str();
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

fn resolve_refs_in_value(value: &Value, refs: &ConversationRefs) -> Value {
    match value {
        Value::String(s) if is_short_ref(s) => match refs.resolve_short_ref(s) {
            Some(id) => Value::String(id),
            None => value.clone(),
        },
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| resolve_refs_in_value(v, refs))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), resolve_refs_in_value(v, refs)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

#[async_trait]
impl GatewayActions for ExtgateOpsGatewayActions {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        self.declarations
            .iter()
            .map(|d| GatewayActionDef {
                name: d.name.clone(),
                description: d.description.clone(),
                parameters: d.input_schema.clone(),
                class: ToolClass {
                    // DI-01: 常時 detach。これは core 内部の dispatch 配線であって、廃止した宣言
                    // schema の `class.dispatch` field を復活させるものではない（宣言に dispatch は
                    // なく、常時 detach は core 動作として保証される）。
                    dispatch: DispatchMode::Dispatchable,
                    sub_engine: map_sub_engine(d.class.sub_engine),
                    sharing: map_sharing(d.class.sharing),
                },
            })
            .collect()
    }

    async fn execute(
        &self,
        name: &str,
        args: &Value,
        _ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // 宣言外は SystemGatewayActions から回ってこない想定だが fail-closed。
        if !self.declarations.iter().any(|d| d.name == name) {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("operation_unknown".to_string()),
            };
        }
        let payload = self.resolve_payload(args);
        // 背景 subtask 内での await（option B）: turn は既に spawned で返り detach 済み。
        match invoke_and_wait(
            &self.state,
            &self.instance_id,
            &self.binding_id,
            name,
            &payload,
        )
        .await
        {
            Ok(result) => GatewayActionResult {
                success: true,
                data: Some(result),
                error: None,
            },
            Err(e) => GatewayActionResult {
                success: false,
                data: None,
                error: Some(e.code.as_str().to_string()),
            },
        }
    }
}
