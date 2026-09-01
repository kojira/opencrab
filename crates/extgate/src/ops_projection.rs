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

use crate::operation_calls::{invoke_and_wait, invoke_utterance};
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
    /// 宣言 schema が `"format":"short-ref"` を付けた top-level field だけを短縮参照解決する
    /// （全 string 無差別ではなく的を絞る・レビュー要望）。gateway が参照フィールドを標示し、core は
    /// その field のみ ConversationRefs で解決する（platform 語彙なし・generic）。
    fn resolve_payload(&self, operation: &str, payload: &Value) -> Value {
        let fields = self.short_ref_fields(operation);
        if fields.is_empty() {
            return payload.clone();
        }
        let Value::Object(map) = payload else {
            return payload.clone();
        };
        let refs = match self.build_refs() {
            Some(r) => r,
            None => return payload.clone(),
        };
        let mut out = map.clone();
        for field in &fields {
            if let Some(Value::String(s)) = out.get(field) {
                if let Some(id) = refs.resolve_short_ref(s) {
                    out.insert(field.clone(), Value::String(id));
                }
            }
        }
        Value::Object(out)
    }

    /// 宣言 schema の top-level properties から `"format":"short-ref"` 標示の field 名を集める。
    fn short_ref_fields(&self, operation: &str) -> Vec<String> {
        self.declarations
            .iter()
            .find(|d| d.name == operation)
            .and_then(|d| d.input_schema.get("properties"))
            .and_then(|p| p.as_object())
            .map(|props| {
                props
                    .iter()
                    .filter(|(_, spec)| {
                        spec.get("format").and_then(|f| f.as_str()) == Some("short-ref")
                    })
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// この op が**発話クラス**（撃ちっぱなし・§3.3.1 C2）か。宣言 field（additive・R3 (a)）を
    /// 優先し、無ければ core 既知名（R3 (c)）へフォールバックする。
    fn is_utterance_op(&self, decl: &GatewayOperationDeclaration) -> bool {
        decl.class
            .utterance
            .unwrap_or_else(|| opencrab_gateway::is_known_utterance_op(&decl.name))
    }

    /// 名前から発話クラス判定（execute 経路用。宣言 snapshot を引く）。
    fn is_utterance_name(&self, name: &str) -> bool {
        self.declarations
            .iter()
            .find(|d| d.name == name)
            .map(|d| self.is_utterance_op(d))
            .unwrap_or(false)
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
                    // C1（発話クラス化・§3.3.1）: 従来は全 DI op を Dispatchable 固定していたが、
                    // 発話クラス（撃ちっぱなしの発言 op）は subtask 化せず配送経路（Utterance）へ、
                    // 照会/道具クラスは従来どおり Dispatchable（常時 detach）とする。分類は宣言
                    // field（additive・R3 (a)）を優先し、無ければ core 既知名（R3 (c)・既知名の
                    // 集約は `opencrab_gateway::is_known_utterance_op`）。
                    dispatch: if self.is_utterance_op(d) {
                        DispatchMode::Utterance
                    } else {
                        DispatchMode::Dispatchable
                    },
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
        let payload = self.resolve_payload(name, args);

        // 発話クラス（撃ちっぱなし・§3.3.1 C5）: operation_call を作らず delivery で crash-safe
        // 永続し、await しない（settle/resume を起こさない）。モデルへは最小 ack（成功封筒・
        // データなし）を返す——engine 側で機械行にせず本文だけ会話へ残す（C6）。
        if self.is_utterance_name(name) {
            let (body, kind, target_origin) = opencrab_gateway::utterance_body(name, &payload);
            let target_id = target_origin.as_deref().and_then(event_id_from_origin);
            return match invoke_utterance(
                &self.state,
                &self.instance_id,
                &self.binding_id,
                &self.agent_id,
                &self.session_id,
                name,
                &payload,
                &body,
                &kind,
                target_id.as_deref(),
                target_origin.as_deref(),
            )
            .await
            {
                Ok(()) => GatewayActionResult {
                    success: true,
                    data: None,
                    error: None,
                },
                Err(e) => GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(e.code.as_str().to_string()),
                },
            };
        }

        // 照会/道具クラス: 背景 subtask 内での await（option B）。turn は既に spawned で
        // 返り detach 済み。
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

/// origin（`…:<lane>:<64hex>`）末尾の 64hex を取り出す（platform SDK 名に非依存）。
fn event_id_from_origin(origin: &str) -> Option<String> {
    let last = origin.rsplit(':').next()?;
    if last.len() == 64 && last.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(last.to_ascii_lowercase())
    } else {
        None
    }
}
