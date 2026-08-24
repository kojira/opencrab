//! `PlaceCreateLegacy` / `PrivateJournalAppendMentor`（DESIGN-DASHBOARD-P2 SLICE 6）。
//! HTTP は持たない。判断と SQL はここ。旧 `sessions` / `memory_sessions` は書かない（INV-2）。
//! mentor は events に載せない（DESIGN-RULINGS C2）。

use crate::Store;
use opencrab_port::{PlaceId, SubjectId};
use rusqlite::{params, OptionalExtension, Transaction};

const SOURCE_SYSTEM: &str = "opencrab";
const DEFAULT_MODE: &str = "autonomous";
const DEFAULT_PHASE: &str = "divergent";
const DEFAULT_STATUS: &str = "active";
const MENTOR_PROVENANCE: &[u8] = br#"{"kind":"mentor"}"#;

#[derive(Debug)]
pub enum PlaceCreateLegacyError {
    UnresolvedParticipant(String),
    Store(rusqlite::Error),
}

impl From<rusqlite::Error> for PlaceCreateLegacyError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

impl std::fmt::Display for PlaceCreateLegacyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnresolvedParticipant(id) => write!(f, "unresolved participant: {id}"),
            Self::Store(error) => write!(f, "{error}"),
        }
    }
}

#[derive(Debug)]
pub enum PrivateJournalError {
    PlaceMissing,
    NoParticipants,
    Store(rusqlite::Error),
}

impl From<rusqlite::Error> for PrivateJournalError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

impl std::fmt::Display for PrivateJournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PlaceMissing => write!(f, "place not found"),
            Self::NoParticipants => write!(f, "place has no participants"),
            Self::Store(error) => write!(f, "{error}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaceCreateLegacy {
    pub theme: String,
    pub mode: Option<String>,
    pub participant_ids: Vec<String>,
    pub max_turns: Option<i64>,
}

fn parse_subject_id(raw: &str) -> Option<SubjectId> {
    raw.parse::<SubjectId>().ok()
}

fn subject_exists(tx: &Transaction<'_>, id: SubjectId) -> crate::Result<bool> {
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM subjects WHERE id=?1)",
        params![id],
        |row| row.get(0),
    )
}

fn resolve_participants(
    tx: &Transaction<'_>,
    raw_ids: &[String],
) -> std::result::Result<Vec<SubjectId>, PlaceCreateLegacyError> {
    let mut subjects = Vec::with_capacity(raw_ids.len());
    for raw in raw_ids {
        let Some(id) = parse_subject_id(raw) else {
            return Err(PlaceCreateLegacyError::UnresolvedParticipant(raw.clone()));
        };
        if !subject_exists(tx, id)? {
            return Err(PlaceCreateLegacyError::UnresolvedParticipant(raw.clone()));
        }
        if subjects.contains(&id) {
            continue;
        }
        subjects.push(id);
    }
    Ok(subjects)
}

impl Store {
    /// `core.command/place.create-legacy-compatible`
    pub fn place_create_legacy(
        &self,
        create: &PlaceCreateLegacy,
        now: i64,
    ) -> std::result::Result<PlaceId, PlaceCreateLegacyError> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let subjects = resolve_participants(&tx, &create.participant_ids)?;
        tx.execute(
            "INSERT INTO places(address,parent_id,policy_json,inherit_from_place,inherit_up_to_seq,created_at)
             VALUES(?1,NULL,'{}',NULL,NULL,?2)",
            params![create.theme, now],
        )?;
        let place_id = tx.last_insert_rowid();
        let mode = create.mode.as_deref().unwrap_or(DEFAULT_MODE);
        let participant_json = serde_json::to_string(&create.participant_ids).map_err(|error| {
            PlaceCreateLegacyError::Store(rusqlite::Error::ToSqlConversionFailure(error.into()))
        })?;
        let source_address = format!("place:{place_id}");
        tx.execute(
            "INSERT INTO place_source_refs(
               source_system,source_address,place_id,source_id,classification,
               theme,mode,phase,source_status,source_turn_number,source_done_count,
               source_max_turns,participant_public_ids,updated_at
             ) VALUES(?1,?2,?3,?4,'live',?5,?6,?7,?8,0,0,?9,?10,?11)",
            params![
                SOURCE_SYSTEM,
                source_address,
                place_id,
                source_address.as_bytes(),
                create.theme,
                mode,
                DEFAULT_PHASE,
                DEFAULT_STATUS,
                create.max_turns,
                participant_json,
                now
            ],
        )?;
        for subject in subjects {
            tx.execute(
                "INSERT INTO memberships(place_id,subject_id,role,shared_seen_seq,joined_at)
                 VALUES(?1,?2,?3,0,?4)",
                params![place_id, subject, "participant", now],
            )?;
        }
        tx.commit()?;
        Ok(place_id)
    }

    /// `core.command/private-journal.append-mentor`
    pub fn private_journal_append_mentor(
        &self,
        place: PlaceId,
        content: &str,
        now: i64,
    ) -> std::result::Result<i64, PrivateJournalError> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let place_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM places WHERE id=?1)",
            params![place],
            |row| row.get(0),
        )?;
        if !place_exists {
            return Err(PrivateJournalError::PlaceMissing);
        }
        let owner: Option<SubjectId> = tx
            .query_row(
                "SELECT subject_id FROM memberships WHERE place_id=?1 ORDER BY subject_id LIMIT 1",
                params![place],
                |row| row.get(0),
            )
            .optional()?;
        let Some(owner) = owner else {
            return Err(PrivateJournalError::NoParticipants);
        };
        let anchor_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM events WHERE place_id=?1",
            params![place],
            |row| row.get(0),
        )?;
        let journal_id: i64 = tx.query_row(
            "SELECT COALESCE(MAX(journal_id),0)+1 FROM private_journal",
            [],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO private_journal(
               journal_id,owner_subject_id,place_id,anchor_seq,content,created_at,provenance
             ) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                journal_id,
                owner,
                place,
                anchor_seq,
                content.as_bytes(),
                now,
                MENTOR_PROVENANCE
            ],
        )?;
        tx.commit()?;
        Ok(journal_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_subjects(store: &Store) -> (SubjectId, SubjectId) {
        let a = store.subject_create(None, "Ada", "Helper", 10).unwrap();
        let b = store.subject_create(None, "Bea", "Helper", 11).unwrap();
        (a, b)
    }

    #[test]
    fn create_writes_place_ref_and_memberships() {
        let store = Store::new_in_memory().unwrap();
        let (a, b) = fixture_subjects(&store);
        let place = store
            .place_create_legacy(
                &PlaceCreateLegacy {
                    theme: "fixture-theme".into(),
                    mode: Some("facilitated".into()),
                    participant_ids: vec![a.to_string(), b.to_string()],
                    max_turns: Some(8),
                },
                20,
            )
            .unwrap();
        let row = store.get_place(place).unwrap().unwrap();
        assert_eq!(row.address.as_deref(), Some("fixture-theme"));
        assert!(row.closed_at.is_none());
        let members = store.members(place).unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].subject, a.min(b));
        let (theme, mode, max_turns, classification, events): (
            String,
            String,
            Option<i64>,
            String,
            i64,
        ) = store
            .c()
            .query_row(
                "SELECT r.theme,r.mode,r.source_max_turns,r.classification,
                        (SELECT COUNT(*) FROM events WHERE place_id=r.place_id)
                 FROM place_source_refs r WHERE r.place_id=?1",
                params![place],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(theme, "fixture-theme");
        assert_eq!(mode, "facilitated");
        assert_eq!(max_turns, Some(8));
        assert_eq!(classification, "live");
        assert_eq!(events, 0);
    }

    #[test]
    fn unresolved_participant_is_error_and_writes_nothing() {
        let store = Store::new_in_memory().unwrap();
        let (a, _) = fixture_subjects(&store);
        let err = store
            .place_create_legacy(
                &PlaceCreateLegacy {
                    theme: "nope".into(),
                    mode: None,
                    participant_ids: vec![a.to_string(), "99".into()],
                    max_turns: None,
                },
                21,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            PlaceCreateLegacyError::UnresolvedParticipant(id) if id == "99"
        ));
        let places: i64 = store
            .c()
            .query_row("SELECT COUNT(*) FROM places", [], |row| row.get(0))
            .unwrap();
        assert_eq!(places, 0);
        let slug = store
            .place_create_legacy(
                &PlaceCreateLegacy {
                    theme: "nope".into(),
                    mode: None,
                    participant_ids: vec!["not-an-id".into()],
                    max_turns: None,
                },
                22,
            )
            .unwrap_err();
        assert!(matches!(
            slug,
            PlaceCreateLegacyError::UnresolvedParticipant(id) if id == "not-an-id"
        ));
    }

    #[test]
    fn duplicate_participant_ids_are_deduped() {
        let store = Store::new_in_memory().unwrap();
        let (a, b) = fixture_subjects(&store);
        let place = store
            .place_create_legacy(
                &PlaceCreateLegacy {
                    theme: "dup-theme".into(),
                    mode: None,
                    participant_ids: vec![a.to_string(), b.to_string(), a.to_string()],
                    max_turns: None,
                },
                23,
            )
            .unwrap();
        let members = store.members(place).unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].subject, a.min(b));
        assert_eq!(members[1].subject, a.max(b));
    }

    #[test]
    fn mentor_writes_journal_not_events() {
        let store = Store::new_in_memory().unwrap();
        let (a, _) = fixture_subjects(&store);
        let place = store
            .place_create_legacy(
                &PlaceCreateLegacy {
                    theme: "mentor-theme".into(),
                    mode: None,
                    participant_ids: vec![a.to_string()],
                    max_turns: None,
                },
                30,
            )
            .unwrap();
        let id = store
            .private_journal_append_mentor(place, "do this", 31)
            .unwrap();
        assert_eq!(id, 1);
        let again = store
            .private_journal_append_mentor(place, "again", 32)
            .unwrap();
        assert_eq!(again, 2);
        let events: i64 = store
            .c()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE place_id=?1",
                params![place],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events, 0);
        let (owner, content, provenance): (i64, Vec<u8>, Vec<u8>) = store
            .c()
            .query_row(
                "SELECT owner_subject_id,content,provenance FROM private_journal
                 WHERE journal_id=?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(owner, a);
        assert_eq!(content, b"do this");
        assert_eq!(provenance, MENTOR_PROVENANCE);
        assert!(matches!(
            store.private_journal_append_mentor(99, "x", 33),
            Err(PrivateJournalError::PlaceMissing)
        ));
    }
}
