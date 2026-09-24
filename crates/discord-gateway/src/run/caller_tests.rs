use super::*;

#[test]
fn gateway_classifies_authenticated_author_from_its_access_config() {
    let access = AccessConfig {
        owners: vec!["100".into()],
        co_agents: [(
            "200".into(),
            crate::config::CoAgentProjection {
                agent_id: "agent-b".into(),
                relationship_revision: 1,
            },
        )]
        .into_iter()
        .collect(),
        trusted_users: vec!["300".into()],
    };
    assert_eq!(caller_for(&access, "100"), SaidCaller::Owner);
    assert_eq!(
        caller_for(&access, "200"),
        SaidCaller::CoAgent {
            agent_id: "agent-b".into(),
            relationship_revision: 1,
        }
    );
    assert_eq!(caller_for(&access, "300"), SaidCaller::TrustedUser);
    assert_eq!(caller_for(&access, "400"), SaidCaller::Agent);
}
