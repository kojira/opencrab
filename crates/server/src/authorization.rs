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

pub fn run_if_current<T>(
    db: &opencrab_db::Db,
    target_agent_id: &str,
    authority: Option<&RelationshipAuthority>,
    _boundary: AuthorizationBoundary,
    side_effect: impl FnOnce() -> T,
) -> Result<T, &'static str> {
    if authority.is_some_and(|authority| !relationship_is_current(db, target_agent_id, authority)) {
        return Err("authorization_revoked");
    }
    Ok(side_effect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_db::queries::TrustedCoAgentRow;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

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
    fn s6_seven_boundaries_fail_closed_after_revoke_and_revision_bump() {
        let mut violations = Vec::new();
        for mutation in ["revoke", "revision_bump"] {
            for boundary in AuthorizationBoundary::ALL {
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
                let effects = AtomicUsize::new(0);
                let result =
                    run_if_current(&db, "target-agent", Some(&authority), boundary, || {
                        effects.fetch_add(1, Ordering::SeqCst)
                    });
                if result != Err("authorization_revoked") || effects.load(Ordering::SeqCst) != 0 {
                    violations.push(format!("{mutation}:{boundary:?}"));
                }
            }
        }
        assert!(
            violations.is_empty(),
            "unwanted side effects crossed revoked boundaries: {violations:?}"
        );
    }

    #[test]
    fn s6_queued_and_concurrent_boundaries_never_use_cached_authority() {
        for boundary in AuthorizationBoundary::ALL {
            let (db, authority) = fixture();
            let start = Arc::new(Barrier::new(2));
            let effects = Arc::new(AtomicUsize::new(0));
            let worker_db = db.clone();
            let worker_authority = authority.clone();
            let worker_start = Arc::clone(&start);
            let worker_effects = Arc::clone(&effects);
            let worker = std::thread::spawn(move || {
                worker_start.wait();
                run_if_current(
                    &worker_db,
                    "target-agent",
                    Some(&worker_authority),
                    boundary,
                    || worker_effects.fetch_add(1, Ordering::SeqCst),
                )
            });
            {
                let conn = db.lock().unwrap();
                opencrab_db::queries::bump_trusted_co_agent_revision(
                    &conn,
                    "target-agent",
                    "peer-agent",
                )
                .unwrap();
            }
            start.wait();
            assert_eq!(worker.join().unwrap(), Err("authorization_revoked"));
            assert_eq!(effects.load(Ordering::SeqCst), 0, "{boundary:?}");
        }
    }
}
