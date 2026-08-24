//! `AgentDirectMessage`: スライス 6 の place 機構 + said（DESIGN-DASHBOARD-P2 SLICE 7）。
//! HTTP は持たない。判断と SQL はここ。旧 `sessions` / `session_logs` は書かない（INV-2）。

use crate::{NewEvent, Store};
use opencrab_port::{Content, EventKind, PlaceId, Seq, SubjectId};
use rusqlite::{params, OptionalExtension, Transaction};

pub const REST_SESSION_PREFIX: &str = "agent-msg-";

const SOURCE_SYSTEM: &str = "opencrab";
const THEME: &str = "direct_message";
const DEFAULT_MODE: &str = "autonomous";
const DEFAULT_PHASE: &str = "divergent";
const DEFAULT_STATUS: &str = "active";
const PLATFORM_REST: &str = "rest";

#[derive(Debug)]
pub enum AgentDirectMessageError {
    Store(rusqlite::Error),
    UnknownPermission(String),
}

impl From<rusqlite::Error> for AgentDirectMessageError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

impl std::fmt::Display for AgentDirectMessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "{error}"),
            Self::UnknownPermission(permission) => {
                write!(f, "unknown trusted_users.permission: {permission}")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDirectMessage {
    pub place_id: PlaceId,
    pub session_id: String,
    pub said_seq: Seq,
    pub caller_type: &'static str,
}

fn rest_caller_type(
    tx: &Transaction<'_>,
    legacy_agent_id: Option<&str>,
    user_id: &str,
) -> std::result::Result<&'static str, AgentDirectMessageError> {
    let Some(agent_id) = legacy_agent_id else {
        return Ok("agent");
    };
    let permission: Option<String> = match tx.query_row(
        "SELECT permission FROM trusted_users WHERE platform=?1 AND user_id=?2 AND agent_id=?3",
        params![PLATFORM_REST, user_id, agent_id],
        |row| row.get(0),
    ) {
        Ok(permission) => Some(permission),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) if error.to_string().contains("no such table") => None,
        Err(error) => return Err(error.into()),
    };
    match permission.as_deref() {
        None => Ok("agent"),
        Some("owner") => Ok("owner"),
        Some("co-agent") => Ok("co_agent"),
        Some("user") => Ok("trusted_user"),
        Some(other) => Err(AgentDirectMessageError::UnknownPermission(
            other.to_string(),
        )),
    }
}

fn ensure_dm_place(
    tx: &Transaction<'_>,
    session_id: &str,
    agent: SubjectId,
    now: i64,
) -> std::result::Result<PlaceId, AgentDirectMessageError> {
    let found: Option<PlaceId> = tx
        .query_row(
            "SELECT id FROM places WHERE address=?1 ORDER BY id LIMIT 1",
            params![session_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(place) = found {
        let member: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM memberships WHERE place_id=?1 AND subject_id=?2)",
            params![place, agent],
            |row| row.get(0),
        )?;
        if !member {
            tx.execute(
                "INSERT INTO memberships(place_id,subject_id,role,shared_seen_seq,joined_at)
                 VALUES(?1,?2,'participant',0,?3)",
                params![place, agent, now],
            )?;
        }
        return Ok(place);
    }
    tx.execute(
        "INSERT INTO places(address,parent_id,policy_json,inherit_from_place,inherit_up_to_seq,created_at)
         VALUES(?1,NULL,'{}',NULL,NULL,?2)",
        params![session_id, now],
    )?;
    let place_id = tx.last_insert_rowid();
    let participant_json = serde_json::to_string(&[agent.to_string()])
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?;
    let source_address = format!("place:{place_id}");
    tx.execute(
        "INSERT INTO place_source_refs(
           source_system,source_address,place_id,source_id,classification,
           theme,mode,phase,source_status,source_turn_number,source_done_count,
           source_max_turns,participant_public_ids,updated_at
         ) VALUES(?1,?2,?3,?4,'live',?5,?6,?7,?8,0,0,NULL,?9,?10)",
        params![
            SOURCE_SYSTEM,
            source_address,
            place_id,
            source_address.as_bytes(),
            THEME,
            DEFAULT_MODE,
            DEFAULT_PHASE,
            DEFAULT_STATUS,
            participant_json,
            now
        ],
    )?;
    tx.execute(
        "INSERT INTO memberships(place_id,subject_id,role,shared_seen_seq,joined_at)
         VALUES(?1,?2,'participant',0,?3)",
        params![place_id, agent, now],
    )?;
    Ok(place_id)
}

impl Store {
    /// `core.command/agent.direct-message`
    pub fn agent_direct_message(
        &self,
        agent: SubjectId,
        user_id: &str,
        content: &str,
        legacy_agent_id: Option<&str>,
        now: i64,
    ) -> std::result::Result<Option<AgentDirectMessage>, AgentDirectMessageError> {
        let user_id = user_id.trim();
        let session_id = format!("{REST_SESSION_PREFIX}{agent}-{user_id}");
        let (place, caller_type) = {
            let mut conn = self.c();
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let kind: Option<String> = tx
                .query_row(
                    "SELECT kind FROM subjects WHERE id=?1",
                    params![agent],
                    |row| row.get(0),
                )
                .optional()?;
            if kind.as_deref() != Some("agent") {
                return Ok(None);
            }
            let place = ensure_dm_place(&tx, &session_id, agent, now)?;
            let caller_type = rest_caller_type(&tx, legacy_agent_id, user_id)?;
            tx.commit()?;
            (place, caller_type)
        };
        let said_seq = self.append(
            place,
            &NewEvent {
                kind: EventKind::Said,
                author_subject: None,
                author_external: Some(user_id.to_string()),
                content: Content::text(content),
                mentions: vec![],
                reply_to: None,
                target: None,
                for_subject: None,
                attachments: vec![],
                metadata: serde_json::json!({}),
            },
            now,
        )?;
        Ok(Some(AgentDirectMessage {
            place_id: place,
            session_id,
            said_seq,
            caller_type,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_trusted(
        store: &Store,
        user_id: &str,
        agent_id: &str,
        permission: &str,
        platform: &str,
    ) {
        store
            .c()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS trusted_users (
                   id TEXT PRIMARY KEY,
                   user_id TEXT NOT NULL,
                   agent_id TEXT NOT NULL,
                   permission TEXT NOT NULL,
                   created_by TEXT NOT NULL,
                   created_at TEXT NOT NULL,
                   display_name TEXT NOT NULL,
                   platform TEXT NOT NULL
                 );",
            )
            .unwrap();
        store
            .c()
            .execute(
                "INSERT INTO trusted_users
                   (id,user_id,agent_id,permission,created_by,created_at,display_name,platform)
                 VALUES(?1,?2,?3,?4,'test','0','n',?5)",
                params![
                    format!("{platform}-{user_id}-{agent_id}"),
                    user_id,
                    agent_id,
                    permission,
                    platform
                ],
            )
            .unwrap();
    }

    #[test]
    fn ensures_place_ref_and_appends_said() {
        let store = Store::new_in_memory().unwrap();
        let agent = store.subject_create(None, "Ada", "Helper", 10).unwrap();
        let first = store
            .agent_direct_message(agent, " user-1 ", "hello", None, 20)
            .unwrap()
            .unwrap();
        assert_eq!(first.session_id, format!("agent-msg-{agent}-user-1"));
        assert_eq!(first.said_seq, 1);
        assert_eq!(first.caller_type, "agent");
        let address: String = store
            .c()
            .query_row(
                "SELECT address FROM places WHERE id=?1",
                params![first.place_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(address, first.session_id);
        let (theme, mode, phase, status, events): (String, String, String, String, i64) = store
            .c()
            .query_row(
                "SELECT r.theme,r.mode,r.phase,r.source_status,
                        (SELECT COUNT(*) FROM events WHERE place_id=r.place_id)
                 FROM place_source_refs r WHERE r.place_id=?1",
                params![first.place_id],
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
        assert_eq!(theme, THEME);
        assert_eq!(mode, DEFAULT_MODE);
        assert_eq!(phase, DEFAULT_PHASE);
        assert_eq!(status, DEFAULT_STATUS);
        assert_eq!(events, 1);
        let second = store
            .agent_direct_message(agent, "user-1", "again", None, 21)
            .unwrap()
            .unwrap();
        assert_eq!(second.place_id, first.place_id);
        assert_eq!(second.said_seq, 2);
        let refs: i64 = store
            .c()
            .query_row(
                "SELECT COUNT(*) FROM place_source_refs WHERE place_id=?1",
                params![first.place_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(refs, 1);
        let kind: String = store
            .c()
            .query_row(
                "SELECT kind FROM events WHERE place_id=?1 AND seq=?2",
                params![first.place_id, 2],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kind, "said");
        assert!(store
            .agent_direct_message(99, "user-1", "x", None, 22)
            .unwrap()
            .is_none());
    }

    #[test]
    fn caller_type_uses_legacy_uuid_and_rest_platform() {
        let store = Store::new_in_memory().unwrap();
        let agent = store.subject_create(None, "Ada", "Helper", 10).unwrap();
        let legacy = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        insert_trusted(&store, "user-1", legacy, "owner", "rest");
        insert_trusted(&store, "user-2", legacy, "co-agent", "rest");
        insert_trusted(&store, "user-3", legacy, "user", "rest");
        insert_trusted(&store, "user-4", legacy, "owner", "discord");
        let owner = store
            .agent_direct_message(agent, "user-1", "a", Some(legacy), 20)
            .unwrap()
            .unwrap();
        assert_eq!(owner.caller_type, "owner");
        let co = store
            .agent_direct_message(agent, "user-2", "b", Some(legacy), 21)
            .unwrap()
            .unwrap();
        assert_eq!(co.caller_type, "co_agent");
        let trusted = store
            .agent_direct_message(agent, "user-3", "c", Some(legacy), 22)
            .unwrap()
            .unwrap();
        assert_eq!(trusted.caller_type, "trusted_user");
        let discord_only = store
            .agent_direct_message(agent, "user-4", "d", Some(legacy), 23)
            .unwrap()
            .unwrap();
        assert_eq!(discord_only.caller_type, "agent");
        let unknown = store
            .agent_direct_message(agent, "user-1", "e", None, 24)
            .unwrap()
            .unwrap();
        assert_eq!(unknown.caller_type, "agent");
    }

    #[test]
    fn unknown_permission_is_error() {
        let store = Store::new_in_memory().unwrap();
        let agent = store.subject_create(None, "Ada", "Helper", 10).unwrap();
        let legacy = "bbbbbbbb-cccc-4ddd-8eee-ffffffffffff";
        insert_trusted(&store, "user-1", legacy, "wizard", "rest");
        let err = store
            .agent_direct_message(agent, "user-1", "x", Some(legacy), 20)
            .unwrap_err();
        assert!(matches!(
            err,
            AgentDirectMessageError::UnknownPermission(p) if p == "wizard"
        ));
    }
}
