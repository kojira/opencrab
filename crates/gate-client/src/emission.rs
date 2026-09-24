//! Gateway-owned durable side of the generic two-ledger delivery protocol.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS emission_ledger (
  binding_id TEXT NOT NULL,
  delivery_id TEXT NOT NULL,
  payload_digest TEXT NOT NULL CHECK(length(payload_digest)=64),
  delivery_guarantee TEXT NOT NULL CHECK(delivery_guarantee IN ('exactly_once','at_most_once_indeterminate')),
  request_identity TEXT NOT NULL,
  prepared_request BLOB NOT NULL,
  adapter_protocol_digest TEXT NOT NULL CHECK(length(adapter_protocol_digest)=64),
  state TEXT NOT NULL CHECK(state IN ('prepared','receipted','failed','indeterminate','operator_blocked')),
  external_reference TEXT,
  prepared_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  core_acknowledged_at INTEGER,
  ambiguity_deadline INTEGER,
  external_attempted_at INTEGER,
  PRIMARY KEY(binding_id, delivery_id)
);
CREATE INDEX IF NOT EXISTS emission_ledger_pending
 ON emission_ledger(binding_id, prepared_at, delivery_id)
 WHERE core_acknowledged_at IS NULL;
CREATE TRIGGER IF NOT EXISTS emission_ledger_immutable
BEFORE UPDATE OF payload_digest,delivery_guarantee,request_identity,prepared_request,adapter_protocol_digest,prepared_at
ON emission_ledger BEGIN
 SELECT RAISE(ABORT,'emission evidence is immutable')
 WHERE NEW.payload_digest IS NOT OLD.payload_digest
    OR NEW.delivery_guarantee IS NOT OLD.delivery_guarantee
    OR NEW.request_identity IS NOT OLD.request_identity
    OR NEW.prepared_request IS NOT OLD.prepared_request
    OR NEW.adapter_protocol_digest IS NOT OLD.adapter_protocol_digest
    OR NEW.prepared_at IS NOT OLD.prepared_at;
END;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmissionState {
    Prepared,
    Receipted,
    Failed,
    Indeterminate,
    OperatorBlocked,
}

impl EmissionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Receipted => "receipted",
            Self::Failed => "failed",
            Self::Indeterminate => "indeterminate",
            Self::OperatorBlocked => "operator_blocked",
        }
    }
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "prepared" => Self::Prepared,
            "receipted" => Self::Receipted,
            "failed" => Self::Failed,
            "indeterminate" => Self::Indeterminate,
            "operator_blocked" => Self::OperatorBlocked,
            _ => return None,
        })
    }
    pub const fn terminal(self) -> bool {
        !matches!(self, Self::Prepared)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmissionRow {
    pub binding_id: String,
    pub delivery_id: String,
    pub payload_digest: String,
    pub delivery_guarantee: String,
    pub request_identity: String,
    pub prepared_request: Vec<u8>,
    pub adapter_protocol_digest: String,
    pub state: EmissionState,
    pub external_reference: Option<String>,
    pub prepared_at: i64,
    pub core_acknowledged_at: Option<i64>,
    pub ambiguity_deadline: Option<i64>,
    pub external_attempted_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeAction {
    SendPrepared,
    ReportTerminal(EmissionState),
    OperatorBlocked,
}

pub fn reconnect_action(
    row: &EmissionRow,
    current_guarantee: &str,
    recognized_protocol_digest: &str,
) -> ResumeAction {
    if row.state.terminal() {
        return ResumeAction::ReportTerminal(row.state);
    }
    let satisfies = current_guarantee == row.delivery_guarantee
        || (current_guarantee == "exactly_once"
            && row.delivery_guarantee == "at_most_once_indeterminate");
    if !satisfies || row.adapter_protocol_digest != recognized_protocol_digest {
        return ResumeAction::OperatorBlocked;
    }
    ResumeAction::SendPrepared
}

pub struct EmissionLedger {
    conn: Mutex<Connection>,
}

impl EmissionLedger {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &self,
        binding_id: &str,
        delivery_id: &str,
        payload_digest: &str,
        delivery_guarantee: &str,
        request_identity: &str,
        prepared_request: &[u8],
        adapter_protocol_digest: &str,
        now: i64,
        ambiguity_deadline: Option<i64>,
    ) -> anyhow::Result<EmissionRow> {
        anyhow::ensure!(matches!(
            delivery_guarantee,
            "exactly_once" | "at_most_once_indeterminate"
        ));
        anyhow::ensure!(payload_digest.len() == 64 && adapter_protocol_digest.len() == 64);
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("emission ledger lock"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO emission_ledger(binding_id,delivery_id,payload_digest,delivery_guarantee,
             request_identity,prepared_request,adapter_protocol_digest,state,prepared_at,updated_at,ambiguity_deadline)
             VALUES(?1,?2,?3,?4,?5,?6,?7,'prepared',?8,?8,?9)
             ON CONFLICT(binding_id,delivery_id) DO NOTHING",
            params![binding_id,delivery_id,payload_digest,delivery_guarantee,request_identity,
                prepared_request,adapter_protocol_digest,now,ambiguity_deadline],
        )?;
        let row = query_row(&tx, binding_id, delivery_id)?
            .ok_or_else(|| anyhow::anyhow!("emission row missing"))?;
        anyhow::ensure!(
            row.payload_digest == payload_digest,
            "payload digest conflict"
        );
        anyhow::ensure!(
            row.delivery_guarantee == delivery_guarantee,
            "delivery guarantee conflict"
        );
        anyhow::ensure!(
            row.request_identity == request_identity,
            "request identity conflict"
        );
        anyhow::ensure!(
            row.prepared_request == prepared_request,
            "prepared request conflict"
        );
        anyhow::ensure!(
            row.adapter_protocol_digest == adapter_protocol_digest,
            "adapter protocol conflict"
        );
        tx.commit()?;
        Ok(row)
    }

    pub fn get(&self, binding_id: &str, delivery_id: &str) -> anyhow::Result<Option<EmissionRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("emission ledger lock"))?;
        query_row(&conn, binding_id, delivery_id)
    }

    pub fn mark_external_attempted(
        &self,
        binding_id: &str,
        delivery_id: &str,
        now: i64,
    ) -> anyhow::Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("emission ledger lock"))?;
        Ok(conn.execute(
            "UPDATE emission_ledger SET external_attempted_at=?3,updated_at=?3
             WHERE binding_id=?1 AND delivery_id=?2 AND state='prepared' AND external_attempted_at IS NULL",
            params![binding_id,delivery_id,now],
        )? == 1)
    }

    pub fn terminal(
        &self,
        binding_id: &str,
        delivery_id: &str,
        state: EmissionState,
        external_reference: Option<&str>,
        now: i64,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(state.terminal());
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("emission ledger lock"))?;
        let changed = conn.execute(
            "UPDATE emission_ledger SET state=?3,external_reference=?4,updated_at=?5
             WHERE binding_id=?1 AND delivery_id=?2 AND state='prepared'",
            params![
                binding_id,
                delivery_id,
                state.as_str(),
                external_reference,
                now
            ],
        )?;
        if changed == 0 {
            let row = query_row(&conn, binding_id, delivery_id)?
                .ok_or_else(|| anyhow::anyhow!("emission row missing"))?;
            anyhow::ensure!(
                row.state == state && row.external_reference.as_deref() == external_reference,
                "terminal outcome conflict"
            );
        }
        Ok(changed == 1)
    }

    pub fn acknowledge_core(
        &self,
        binding_id: &str,
        delivery_id: &str,
        now: i64,
    ) -> anyhow::Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("emission ledger lock"))?;
        Ok(conn.execute(
            "UPDATE emission_ledger SET core_acknowledged_at=COALESCE(core_acknowledged_at,?3),updated_at=?3
             WHERE binding_id=?1 AND delivery_id=?2 AND state!='prepared'",
            params![binding_id,delivery_id,now],
        )? == 1)
    }

    pub fn prune_acknowledged_before(&self, cutoff: i64) -> anyhow::Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("emission ledger lock"))?;
        Ok(conn.execute(
            "DELETE FROM emission_ledger WHERE core_acknowledged_at IS NOT NULL
             AND core_acknowledged_at<=?1 AND state IN ('receipted','failed')",
            [cutoff],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    #[test]
    fn s7_prepare_send_receipt_ack_crash_window_matrix() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = EmissionLedger::open(&temp.path().join("gateway.db")).unwrap();
        assert!(
            ledger.get("b", "d").unwrap().is_none(),
            "before prepare sends nothing"
        );
        ledger
            .prepare(
                "b",
                "d",
                &digest('a'),
                "exactly_once",
                "event",
                b"signed",
                &digest('b'),
                1,
                None,
            )
            .unwrap();
        assert_eq!(
            ledger.get("b", "d").unwrap().unwrap().external_attempted_at,
            None
        );
        ledger.mark_external_attempted("b", "d", 2).unwrap();
        assert_eq!(
            ledger.get("b", "d").unwrap().unwrap().external_attempted_at,
            Some(2)
        );
        ledger
            .terminal("b", "d", EmissionState::Receipted, Some("event"), 3)
            .unwrap();
        let receipted = ledger.get("b", "d").unwrap().unwrap();
        assert_eq!(receipted.state, EmissionState::Receipted);
        assert_eq!(receipted.core_acknowledged_at, None);
        ledger.acknowledge_core("b", "d", 4).unwrap();
        assert_eq!(
            ledger.get("b", "d").unwrap().unwrap().core_acknowledged_at,
            Some(4)
        );
    }

    #[test]
    fn s7_gateway_ledger_replay_conflict_terminal_and_retention_matrix() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = EmissionLedger::open(&temp.path().join("gateway.db")).unwrap();
        let first = ledger
            .prepare(
                "b",
                "d",
                &digest('a'),
                "exactly_once",
                "event-id",
                b"signed",
                &digest('b'),
                1,
                None,
            )
            .unwrap();
        assert_eq!(first.state, EmissionState::Prepared);
        assert_eq!(
            ledger
                .prepare(
                    "b",
                    "d",
                    &digest('a'),
                    "exactly_once",
                    "event-id",
                    b"signed",
                    &digest('b'),
                    2,
                    None
                )
                .unwrap()
                .prepared_at,
            1
        );
        assert!(ledger
            .prepare(
                "b",
                "d",
                &digest('c'),
                "exactly_once",
                "event-id",
                b"signed",
                &digest('b'),
                2,
                None
            )
            .is_err());
        assert!(ledger
            .prepare(
                "b",
                "d",
                &digest('a'),
                "at_most_once_indeterminate",
                "event-id",
                b"signed",
                &digest('b'),
                2,
                None
            )
            .is_err());
        assert!(ledger.mark_external_attempted("b", "d", 3).unwrap());
        assert!(ledger
            .terminal("b", "d", EmissionState::Receipted, Some("external"), 4)
            .unwrap());
        assert!(!ledger
            .terminal("b", "d", EmissionState::Receipted, Some("external"), 5)
            .unwrap());
        assert!(ledger
            .terminal("b", "d", EmissionState::Failed, None, 5)
            .is_err());
        assert!(ledger.acknowledge_core("b", "d", 6).unwrap());
        assert_eq!(ledger.prune_acknowledged_before(5).unwrap(), 0);
        assert_eq!(ledger.prune_acknowledged_before(6).unwrap(), 1);
    }

    #[test]
    fn s7_reconnect_weaker_same_stronger_protocol_matrix() {
        let row = EmissionRow {
            binding_id: "b".into(),
            delivery_id: "d".into(),
            payload_digest: digest('a'),
            delivery_guarantee: "at_most_once_indeterminate".into(),
            request_identity: "n".into(),
            prepared_request: b"p".to_vec(),
            adapter_protocol_digest: digest('b'),
            state: EmissionState::Prepared,
            external_reference: None,
            prepared_at: 1,
            core_acknowledged_at: None,
            ambiguity_deadline: None,
            external_attempted_at: None,
        };
        assert_eq!(
            reconnect_action(&row, "at_most_once_indeterminate", &digest('b')),
            ResumeAction::SendPrepared
        );
        assert_eq!(
            reconnect_action(&row, "exactly_once", &digest('b')),
            ResumeAction::SendPrepared
        );
        assert_eq!(
            reconnect_action(&row, "at_most_once_indeterminate", &digest('c')),
            ResumeAction::OperatorBlocked
        );
        let mut exact = row.clone();
        exact.delivery_guarantee = "exactly_once".into();
        assert_eq!(
            reconnect_action(&exact, "at_most_once_indeterminate", &digest('b')),
            ResumeAction::OperatorBlocked
        );
    }

    #[test]
    fn s7_terminal_and_ambiguous_rows_are_never_sendable() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = EmissionLedger::open(&temp.path().join("gateway.db")).unwrap();
        for (id, state) in [
            ("r", EmissionState::Receipted),
            ("f", EmissionState::Failed),
            ("i", EmissionState::Indeterminate),
            ("o", EmissionState::OperatorBlocked),
        ] {
            ledger
                .prepare(
                    "b",
                    id,
                    &digest('a'),
                    "at_most_once_indeterminate",
                    id,
                    id.as_bytes(),
                    &digest('b'),
                    1,
                    Some(2),
                )
                .unwrap();
            ledger.terminal("b", id, state, None, 2).unwrap();
            assert!(ledger.get("b", id).unwrap().unwrap().state.terminal());
            assert!(!ledger.mark_external_attempted("b", id, 3).unwrap());
        }
    }
}

fn query_row(
    conn: &Connection,
    binding_id: &str,
    delivery_id: &str,
) -> anyhow::Result<Option<EmissionRow>> {
    conn.query_row(
        "SELECT binding_id,delivery_id,payload_digest,delivery_guarantee,request_identity,
         prepared_request,adapter_protocol_digest,state,external_reference,prepared_at,
         core_acknowledged_at,ambiguity_deadline,external_attempted_at FROM emission_ledger
         WHERE binding_id=?1 AND delivery_id=?2",
        params![binding_id, delivery_id],
        |row| {
            let state: String = row.get(7)?;
            Ok(EmissionRow {
                binding_id: row.get(0)?,
                delivery_id: row.get(1)?,
                payload_digest: row.get(2)?,
                delivery_guarantee: row.get(3)?,
                request_identity: row.get(4)?,
                prepared_request: row.get(5)?,
                adapter_protocol_digest: row.get(6)?,
                state: EmissionState::parse(&state).ok_or(rusqlite::Error::InvalidQuery)?,
                external_reference: row.get(8)?,
                prepared_at: row.get(9)?,
                core_acknowledged_at: row.get(10)?,
                ambiguity_deadline: row.get(11)?,
                external_attempted_at: row.get(12)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}
