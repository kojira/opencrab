use super::super::super::*;

/// 指定した名前のツールを定義し、`execute` の到達を記録する inner のフェイク。
pub(crate) struct RecordingInner {
    names: Vec<String>,
    calls: std::sync::Mutex<Vec<String>>,
}

impl RecordingInner {
    pub(crate) fn new(names: &[&str]) -> Self {
        Self {
            names: names.iter().map(|s| s.to_string()).collect(),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }
    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl GatewayActions for RecordingInner {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        self.names
            .iter()
            .map(|n| GatewayActionDef {
                name: n.clone(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: format!("{n} (inner)"),
                parameters: json!({"type": "object"}),
            })
            .collect()
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
}
