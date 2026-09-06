use super::super::super::*;
// 本体は `is_owner_equivalent()` / `trust_level()` 経由でしか caller を見ないので、
// 列挙子そのものを組み立てるのはテストだけ。本体側の `use` に混ぜると未使用警告になる。
use opencrab_gateway::GatewayCaller;

pub(crate) fn owner_ctx() -> GatewayCallContext {
    GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
}

pub(crate) fn agent_ctx() -> GatewayCallContext {
    GatewayCallContext::new(GatewayCaller::Agent, "agent-x")
}
pub(crate) fn trusted_ctx() -> GatewayCallContext {
    GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x")
}
