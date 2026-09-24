//! Current internal co-agent relationship authority.
//!
//! Gateways authenticate external identities and project only the generic co-agent ID plus the
//! relationship revision. Core owns this internal relationship and re-reads it at every covered
//! execution/emission boundary.

use opencrab_core::authorization::{AuthorizationBoundary, RelationshipAuthority};

pub fn relationship_is_current(
    db: &opencrab_db::Db,
    target_agent_id: &str,
    authority: &RelationshipAuthority,
) -> bool {
    db.lock().ok().is_some_and(|conn| {
        opencrab_db::queries::co_agent_relationship_is_current(
            &conn,
            target_agent_id,
            &authority.co_agent_id,
            authority.relationship_revision,
        )
        .unwrap_or(false)
    })
}

pub fn make_check(
    db: opencrab_db::Db,
    target_agent_id: String,
    authority: RelationshipAuthority,
) -> opencrab_core::authorization::AuthorizationCheck {
    std::sync::Arc::new(move |_boundary| relationship_is_current(&db, &target_agent_id, &authority))
}

pub fn authorize_timed_subtask_entry(
    depth: u32,
    check: &opencrab_core::authorization::AuthorizationCheck,
) -> anyhow::Result<()> {
    if depth > 0 && !check(AuthorizationBoundary::TimedSubtaskContinuation) {
        anyhow::bail!("authorization_revoked:timed_subtask_continuation");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_db::queries::TrustedCoAgentRow;

    fn fixture() -> (opencrab_db::Db, RelationshipAuthority) {
        let db = opencrab_db::Db::memory().expect("db");
        {
            let conn = db.lock().unwrap();
            opencrab_db::queries::insert_trusted_co_agent(
                &conn,
                &TrustedCoAgentRow {
                    id: "relationship-1".into(),
                    agent_id: "target-agent".into(),
                    co_agent_id: "peer-agent".into(),
                    allowed_actions: None,
                    created_by: "owner".into(),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    relationship_revision: 1,
                    active: true,
                },
            )
            .unwrap();
        }
        (
            db,
            RelationshipAuthority {
                co_agent_id: "peer-agent".into(),
                relationship_revision: 1,
            },
        )
    }

    #[test]
    fn s6_real_timed_subtask_entry_rejects_revoke_and_revision_bump_before_model() {
        for mutation in ["revoke", "revision_bump"] {
            let (db, authority) = fixture();
            {
                let conn = db.lock().unwrap();
                match mutation {
                    "revoke" => assert!(opencrab_db::queries::delete_trusted_co_agent(
                        &conn,
                        "target-agent",
                        "peer-agent",
                    )
                    .unwrap()),
                    "revision_bump" => {
                        assert!(opencrab_db::queries::bump_trusted_co_agent_revision(
                            &conn,
                            "target-agent",
                            "peer-agent",
                        )
                        .unwrap())
                    }
                    _ => unreachable!(),
                }
            }
            let check = make_check(db, "target-agent".to_string(), authority);
            assert!(
                authorize_timed_subtask_entry(1, &check).is_err(),
                "{mutation}"
            );
        }
    }
}
