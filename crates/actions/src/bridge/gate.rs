//! 実行前ゲート（caller の権限・depth・run の許可リスト）の拒否判定。

use super::{tool_policy, BridgedExecutor, MAX_DEPTH};

impl BridgedExecutor {
    /// caller / depth / 許可リストのゲートで実行前に拒否される理由（#45 / #368）。拒否判定の
    /// 唯一の源で、`dispatch_inner` の拒否と `ActionExecutor::rejects_before_run` が共有する。
    pub(super) fn gate_rejection(&self, name: &str) -> Option<String> {
        let policy = tool_policy(name);
        if policy.owner_only && !self.caller_is_owner() {
            return Some(format!("action '{name}' requires owner"));
        }
        if policy.trusted_only && !self.caller_is_trusted() {
            return Some(format!(
                "action '{name}' requires a trusted caller (owner/co_agent/trusted_user)"
            ));
        }
        if self.depth >= 1 && self.is_blocked_in_subengine(name) {
            return Some(format!(
                "action '{name}' is not available in sub-engines (depth {})",
                self.depth
            ));
        }
        if self.depth >= MAX_DEPTH && policy.depth_capped {
            return Some(format!(
                "{name} is not available at depth {} (max nesting: {MAX_DEPTH})",
                self.depth
            ));
        }
        // この run の許可リスト（#368）: MCP/dispatcher/gateway のどのスロットへ振り分ける
        // **前**に効かせ、全スロットを 1 箇所で覆う（見えないが呼べる、を塞ぐ）。
        if !self.run_allows(name) {
            return Some(format!(
                "action '{name}' is not available in this run (tool allowlist)"
            ));
        }
        None
    }
}
