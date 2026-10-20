//! D-1091: `hello` に gateway 自身の instance 用の環境変数（名前→値）を載せる。
//!
//! 「その binding が誰のアカウントか」を知っているのは gateway だけなので、gateway が宣言する。
//! core は名前も値も解釈せず、`execute_shell` の子へそのまま渡す。空なら従来と同じ frame。

use serde_json::{json, Value};

use crate::wire::RuntimeCapabilities;

/// D-1091: hello with the environment this gateway declares for its own instance.
///
/// The gateway is the only party that knows which account an instance belongs to, so it declares
/// the environment (name → value) and core passes it through to `execute_shell` children of that
/// instance's turns. Core interprets neither the names nor the values. An empty map is exactly
/// the previous frame.
pub fn hello_frame_with_capabilities_and_env(
    id: &str,
    instance_id: &str,
    revision: u64,
    config_digest: &str,
    operations: Option<&Value>,
    capabilities: RuntimeCapabilities,
    run_env: &std::collections::BTreeMap<String, String>,
) -> Value {
    let mut frame = json!({
        "id": id,
        "m": "hello",
        "protocol": 3,
        "instance_id": instance_id,
        "revision": revision,
        "config_digest": config_digest,
        "operation_protocol": 1,
        "final_delivery": capabilities.final_delivery.as_str(),
        "operations": operations.cloned().unwrap_or_else(|| json!([])),
    });
    if !run_env.is_empty() {
        frame["run_env"] = json!(run_env);
    }
    frame
}
