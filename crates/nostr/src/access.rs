//! Gateway-owned admission identities used to build an in-memory allow set.

#[derive(Debug, Default, Clone)]
pub struct NostrGateAllowKeys {
    pub owner: Vec<String>,
    pub co_agents: Vec<String>,
    pub co_agent_identities: Vec<(String, String)>,
    pub trusted_users: Vec<String>,
}
