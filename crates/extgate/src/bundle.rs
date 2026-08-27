//! Nostr `TimelineBundle` の core coordinator。
//!
//! 表 `nostr_bundle_state` を正とする。V3 4表・wire には足さない。

use rusqlite::{params, Transaction};

use crate::error::GateError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NostrBundleAdmit {
    pub bundle_id: String,
    pub index: u32,
    pub count: u32,
    pub origins: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleApply {
    pub enqueue: bool,
    pub new_admitted: u32,
    pub trigger_origin: Option<String>,
}

#[derive(Debug)]
struct BundleRow {
    manifest_json: String,
    received_bits: String,
    new_admitted_bits: String,
    completed: i64,
}

/// member の origin transaction で coordinator 行を insert/update する。
///
/// 重複 origin は呼ぶ側が existing_seq で弾く。ここへ来た member は非重複。
/// この origin を `external_origins` へ入れる前に呼ぶ（先に入れると old receipt と誤認する）。
/// manifest 不一致は `store_error` で全 rollback。
pub fn apply_bundle_member(
    tx: &Transaction<'_>,
    binding_id: &str,
    admit: &NostrBundleAdmit,
    newly_admitted: bool,
) -> Result<BundleApply, GateError> {
    if admit.origins.len() != admit.count as usize
        || admit.index < 1
        || admit.index as usize > admit.origins.len()
        || admit.bundle_id.is_empty()
    {
        return Err(GateError::store());
    }
    let manifest = serde_json::to_string(&admit.origins).map_err(|_| GateError::store())?;
    let idx = admit.index as usize - 1;
    let existing = load_row(tx, binding_id, &admit.bundle_id)?;
    let (mut received, mut admitted) = match existing {
        Some(row) => {
            if row.completed == 1 {
                return Ok(BundleApply {
                    enqueue: false,
                    new_admitted: ones(&row.new_admitted_bits),
                    trigger_origin: None,
                });
            }
            if row.manifest_json != manifest {
                return Err(GateError::store());
            }
            if row.received_bits.len() != admit.count as usize
                || row.new_admitted_bits.len() != admit.count as usize
            {
                return Err(GateError::store());
            }
            (
                row.received_bits.into_bytes(),
                row.new_admitted_bits.into_bytes(),
            )
        }
        None => {
            let mut received = vec![b'0'; admit.count as usize];
            let admitted = vec![b'0'; admit.count as usize];
            for (i, origin) in admit.origins.iter().enumerate() {
                if origin_exists(tx, binding_id, origin)? {
                    received[i] = b'1';
                }
            }
            (received, admitted)
        }
    };
    if received[idx] == b'0' {
        received[idx] = b'1';
        if newly_admitted {
            admitted[idx] = b'1';
        }
    }
    let received_s = String::from_utf8(received).map_err(|_| GateError::store())?;
    let admitted_s = String::from_utf8(admitted).map_err(|_| GateError::store())?;
    let new_admitted = ones(&admitted_s);
    let all_in = received_s.bytes().all(|b| b == b'1');
    let completed = if all_in { 1 } else { 0 };
    upsert_row(
        tx,
        binding_id,
        &admit.bundle_id,
        &manifest,
        &received_s,
        &admitted_s,
        completed,
    )?;
    let enqueue = all_in && new_admitted > 0;
    let trigger_origin = if enqueue {
        last_admitted_origin(&admit.origins, &admitted_s)
    } else {
        None
    };
    Ok(BundleApply {
        enqueue,
        new_admitted,
        trigger_origin,
    })
}

fn last_admitted_origin(origins: &[String], bits: &str) -> Option<String> {
    origins
        .iter()
        .zip(bits.bytes())
        .rev()
        .find(|(_, bit)| *bit == b'1')
        .map(|(o, _)| o.clone())
}

fn ones(bits: &str) -> u32 {
    bits.bytes().filter(|b| *b == b'1').count() as u32
}

fn load_row(
    tx: &Transaction<'_>,
    binding_id: &str,
    bundle_id: &str,
) -> Result<Option<BundleRow>, GateError> {
    match tx.query_row(
        "SELECT manifest_json, received_bits, new_admitted_bits, completed
         FROM nostr_bundle_state
         WHERE binding_id = ?1 AND bundle_id = ?2",
        params![binding_id, bundle_id],
        |r| {
            Ok(BundleRow {
                manifest_json: r.get(0)?,
                received_bits: r.get(1)?,
                new_admitted_bits: r.get(2)?,
                completed: r.get(3)?,
            })
        },
    ) {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(_) => Err(GateError::store()),
    }
}

fn origin_exists(tx: &Transaction<'_>, binding_id: &str, origin: &str) -> Result<bool, GateError> {
    match tx.query_row(
        "SELECT 1 FROM external_origins WHERE binding_id = ?1 AND origin = ?2",
        params![binding_id, origin],
        |_| Ok(()),
    ) {
        Ok(()) => Ok(true),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(_) => Err(GateError::store()),
    }
}

fn upsert_row(
    tx: &Transaction<'_>,
    binding_id: &str,
    bundle_id: &str,
    manifest: &str,
    received: &str,
    admitted: &str,
    completed: i64,
) -> Result<(), GateError> {
    tx.execute(
        "INSERT INTO nostr_bundle_state
            (binding_id, bundle_id, manifest_json, received_bits, new_admitted_bits, completed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(binding_id, bundle_id) DO UPDATE SET
            manifest_json = excluded.manifest_json,
            received_bits = excluded.received_bits,
            new_admitted_bits = excluded.new_admitted_bits,
            completed = excluded.completed",
        params![binding_id, bundle_id, manifest, received, admitted, completed],
    )
    .map_err(|_| GateError::store())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, TransactionBehavior};

    fn open() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE external_origins (
                binding_id TEXT NOT NULL,
                origin TEXT NOT NULL,
                seq INTEGER NOT NULL,
                PRIMARY KEY(binding_id, origin)
             );
             CREATE TABLE nostr_bundle_state (
                binding_id TEXT NOT NULL,
                bundle_id TEXT NOT NULL,
                manifest_json TEXT NOT NULL,
                received_bits TEXT NOT NULL,
                new_admitted_bits TEXT NOT NULL,
                completed INTEGER NOT NULL CHECK(completed IN (0,1)),
                PRIMARY KEY(binding_id, bundle_id)
             );",
        )
        .unwrap();
        conn
    }

    fn admit(ids: &[&str], index: u32) -> NostrBundleAdmit {
        NostrBundleAdmit {
            bundle_id: "bundle-1".into(),
            index,
            count: ids.len() as u32,
            origins: ids.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn first_member_premarks_old_origins() {
        let mut conn = open();
        conn.execute(
            "INSERT INTO external_origins (binding_id, origin, seq) VALUES ('b', 'o1', 1)",
            [],
        )
        .unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let out = apply_bundle_member(&tx, "b", &admit(&["o1", "o2"], 2), true).unwrap();
        assert!(out.enqueue);
        assert_eq!(out.trigger_origin.as_deref(), Some("o2"));
        tx.commit().unwrap();
        let (recv, adm, done): (String, String, i64) = conn
            .query_row(
                "SELECT received_bits, new_admitted_bits, completed FROM nostr_bundle_state",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(recv, "11");
        assert_eq!(adm, "01");
        assert_eq!(done, 1);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let done = apply_bundle_member(&tx, "b", &admit(&["o1", "o2"], 2), true).unwrap();
        assert!(!done.enqueue);
        assert_eq!(done.new_admitted, 1);
    }

    #[test]
    fn all_new_members_enqueue_once() {
        let mut conn = open();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let first = apply_bundle_member(&tx, "b", &admit(&["o1", "o2"], 1), true).unwrap();
        assert!(!first.enqueue);
        tx.commit().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let second = apply_bundle_member(&tx, "b", &admit(&["o1", "o2"], 2), true).unwrap();
        assert!(second.enqueue);
        assert_eq!(second.new_admitted, 2);
        assert_eq!(second.trigger_origin.as_deref(), Some("o2"));
        tx.commit().unwrap();
        let again = conn.transaction().unwrap();
        let completed = apply_bundle_member(&again, "b", &admit(&["o1", "o2"], 1), true).unwrap();
        assert!(!completed.enqueue);
    }

    #[test]
    fn zero_new_admitted_is_turn_zero() {
        let mut conn = open();
        conn.execute_batch(
            "INSERT INTO external_origins (binding_id, origin, seq) VALUES
                ('b', 'o1', 1), ('b', 'o2', 2);",
        )
        .unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let out = apply_bundle_member(&tx, "b", &admit(&["o1", "o2", "o3"], 3), false).unwrap();
        assert!(!out.enqueue);
        assert_eq!(out.new_admitted, 0);
    }

    #[test]
    fn manifest_mismatch_is_store_error() {
        let mut conn = open();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        apply_bundle_member(&tx, "b", &admit(&["o1", "o2"], 1), true).unwrap();
        tx.commit().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let err = apply_bundle_member(&tx, "b", &admit(&["o1", "o9"], 2), true);
        assert!(err.is_err());
    }
}
