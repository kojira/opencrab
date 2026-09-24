use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

const GRANT_HASH_DOMAIN: &[u8] = b"opencrab/subject-association-grant/v1\0";
const GRANT_BYTES: usize = 32;
const STORED_GRANT_HASH_BYTES: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum SubjectGrantError {
    #[error("agent/subject pair is not live")]
    InvalidPair,
    #[error("subject association grant is invalid")]
    InvalidGrant,
    #[error("subject association grant has expired")]
    Expired,
    #[error(transparent)]
    Store(#[from] rusqlite::Error),
}

/// Issue one short-lived capability for an exact live `(agent_id, subject_id)` pair.
/// The returned token is the only plaintext copy; storage retains only salt plus hash.
pub fn issue_subject_association_grant(
    conn: &mut Connection,
    agent_id: &str,
    subject_id: i64,
    expires_at: i64,
    now: i64,
) -> Result<String, SubjectGrantError> {
    if expires_at <= now {
        return Err(SubjectGrantError::Expired);
    }
    let mut token = [0_u8; GRANT_BYTES];
    let mut salt = [0_u8; GRANT_BYTES];
    getrandom::fill(&mut token).map_err(|_| rusqlite::Error::InvalidQuery)?;
    getrandom::fill(&mut salt).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let stored_hash = stored_grant_hash(&salt, &token);

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_live_pair(&tx, agent_id, subject_id)?;
    tx.execute(
        "INSERT INTO subject_association_grants
             (grant_hash, agent_id, subject_id, expires_at, consumed_at, consumed_instance_id)
         VALUES (?1, ?2, ?3, ?4, NULL, NULL)",
        params![stored_hash.as_slice(), agent_id, subject_id, expires_at],
    )?;
    tx.commit()?;
    Ok(URL_SAFE_NO_PAD.encode(token))
}

/// Consume a grant in the caller's instance-association transaction.
pub fn consume_subject_association_grant_in_tx(
    tx: &Transaction<'_>,
    presented_grant: &str,
    agent_id: &str,
    subject_id: i64,
    instance_id: &str,
    now: i64,
) -> Result<(), SubjectGrantError> {
    let decoded = URL_SAFE_NO_PAD
        .decode(presented_grant)
        .map_err(|_| SubjectGrantError::InvalidGrant)?;
    let token: [u8; GRANT_BYTES] = decoded
        .try_into()
        .map_err(|_| SubjectGrantError::InvalidGrant)?;

    let mut statement = tx.prepare(
        "SELECT grant_hash, agent_id, subject_id, expires_at, consumed_at
         FROM subject_association_grants",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Option<i64>>(4)?,
        ))
    })?;
    let mut matched = None;
    for row in rows {
        let (stored, stored_agent, stored_subject, expires_at, consumed_at) = row?;
        if stored.len() != STORED_GRANT_HASH_BYTES {
            return Err(SubjectGrantError::InvalidGrant);
        }
        let expected = stored_grant_hash(&stored[..GRANT_BYTES], &token);
        if expected.as_slice() == stored.as_slice() {
            if matched.is_some() {
                return Err(SubjectGrantError::InvalidGrant);
            }
            matched = Some((stored, stored_agent, stored_subject, expires_at, consumed_at));
        }
    }
    drop(statement);

    let Some((stored, stored_agent, stored_subject, expires_at, consumed_at)) = matched else {
        return Err(SubjectGrantError::InvalidGrant);
    };
    if stored_agent != agent_id || stored_subject != subject_id || consumed_at.is_some() {
        return Err(SubjectGrantError::InvalidGrant);
    }
    if expires_at <= now {
        return Err(SubjectGrantError::Expired);
    }
    require_live_pair(tx, agent_id, subject_id)?;
    let changed = tx.execute(
        "UPDATE subject_association_grants
         SET consumed_at=?2, consumed_instance_id=?3
         WHERE grant_hash=?1 AND consumed_at IS NULL",
        params![stored, now, instance_id],
    )?;
    if changed != 1 {
        return Err(SubjectGrantError::InvalidGrant);
    }
    Ok(())
}

fn require_live_pair(
    conn: &Connection,
    agent_id: &str,
    subject_id: i64,
) -> Result<(), SubjectGrantError> {
    let live = conn
        .query_row(
            "SELECT 1 FROM agents AS agent
             WHERE agent.agent_id=?1 AND agent.subject_id=?2
               AND NOT EXISTS(
                   SELECT 1 FROM subject_tombstones AS tombstone
                   WHERE tombstone.subject_id=agent.subject_id
               )",
            params![agent_id, subject_id],
            |_| Ok(()),
        )
        .optional()?;
    live.ok_or(SubjectGrantError::InvalidPair)
}

fn stored_grant_hash(salt: &[u8], token: &[u8; GRANT_BYTES]) -> [u8; STORED_GRANT_HASH_BYTES] {
    let mut digest = Sha256::new();
    digest.update(GRANT_HASH_DOMAIN);
    digest.update(salt);
    digest.update(token);
    let mut stored = [0_u8; STORED_GRANT_HASH_BYTES];
    stored[..GRANT_BYTES].copy_from_slice(salt);
    stored[GRANT_BYTES..].copy_from_slice(&digest.finalize());
    stored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::{upsert_agent, AgentRow};

    fn seed_agent(conn: &Connection) -> i64 {
        upsert_agent(
            conn,
            &AgentRow {
                agent_id: "grant-agent".into(),
                name: "grant-agent".into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "p".into(),
                personality: None,
                instructions: String::new(),
                heartbeat_instructions: String::new(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .unwrap();
        conn.query_row(
            "SELECT subject_id FROM agents WHERE agent_id='grant-agent'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn s2_grant_is_hashed_pair_bound_expiring_and_single_use() {
        let mut conn = crate::init_memory().unwrap();
        let subject_id = seed_agent(&conn);
        let grant = issue_subject_association_grant(
            &mut conn,
            "grant-agent",
            subject_id,
            1_000,
            100,
        )
        .unwrap();
        let stored: Vec<u8> = conn
            .query_row(
                "SELECT grant_hash FROM subject_association_grants",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!stored.windows(grant.len()).any(|bytes| bytes == grant.as_bytes()));

        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert!(matches!(
            consume_subject_association_grant_in_tx(
                &tx,
                &grant,
                "other-agent",
                subject_id,
                "00000000-0000-4000-8000-000000000001",
                101,
            ),
            Err(SubjectGrantError::InvalidGrant)
        ));
        consume_subject_association_grant_in_tx(
            &tx,
            &grant,
            "grant-agent",
            subject_id,
            "00000000-0000-4000-8000-000000000001",
            101,
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert!(matches!(
            consume_subject_association_grant_in_tx(
                &tx,
                &grant,
                "grant-agent",
                subject_id,
                "00000000-0000-4000-8000-000000000002",
                102,
            ),
            Err(SubjectGrantError::InvalidGrant)
        ));
    }

    #[test]
    fn s2_concurrent_grant_consumption_has_exactly_one_winner() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("grant-race.sqlite");
        let db = crate::Db::open(path.to_str().unwrap()).unwrap();
        let (subject_id, grant) = {
            let mut conn = db.lock().unwrap();
            let subject_id = seed_agent(&conn);
            let grant = issue_subject_association_grant(
                &mut conn,
                "grant-agent",
                subject_id,
                1_000,
                100,
            )
            .unwrap();
            (subject_id, grant)
        };
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut threads = Vec::new();
        for suffix in ["1", "2"] {
            let db = db.clone();
            let grant = grant.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                let mut conn = db.lock().unwrap();
                barrier.wait();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .unwrap();
                let result = consume_subject_association_grant_in_tx(
                    &tx,
                    &grant,
                    "grant-agent",
                    subject_id,
                    &format!("00000000-0000-4000-8000-00000000000{suffix}"),
                    101,
                );
                if result.is_ok() {
                    tx.commit().unwrap();
                    true
                } else {
                    false
                }
            }));
        }
        barrier.wait();
        let winners = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
    }

    #[test]
    fn s2_expired_grant_is_denied_and_retained() {
        let mut conn = crate::init_memory().unwrap();
        let subject_id = seed_agent(&conn);
        let grant = issue_subject_association_grant(
            &mut conn,
            "grant-agent",
            subject_id,
            200,
            100,
        )
        .unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert!(matches!(
            consume_subject_association_grant_in_tx(
                &tx,
                &grant,
                "grant-agent",
                subject_id,
                "00000000-0000-4000-8000-000000000003",
                200,
            ),
            Err(SubjectGrantError::Expired)
        ));
        drop(tx);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM subject_association_grants",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
    }
}
