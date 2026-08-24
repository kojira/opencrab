//! `AgentDirectMessage`: place 保証 + said 注入（DESIGN-DASHBOARD-P2）。

use crate::subjects::SubjectCommandError;
use crate::{NewEvent, Store};
use opencrab_port::{Content, EventKind, PlaceId, Role, Seq, SubjectId};
use rusqlite::{params, OptionalExtension};

pub const REST_SESSION_PREFIX: &str = "agent-msg-";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDirectMessage {
    pub place_id: PlaceId,
    pub session_id: String,
    pub said_seq: Seq,
}

impl Store {
    pub fn agent_direct_message(
        &self,
        agent: SubjectId,
        user_id: &str,
        content: &str,
        now: i64,
    ) -> std::result::Result<Option<AgentDirectMessage>, SubjectCommandError> {
        if self.subject_dashboard_view(agent)?.is_none() {
            return Ok(None);
        }
        let session_id = format!("{REST_SESSION_PREFIX}{agent}-{}", user_id.trim());
        let place = {
            let found: Option<PlaceId> = self
                .c()
                .query_row(
                    "SELECT id FROM places WHERE address=?1 ORDER BY id LIMIT 1",
                    params![session_id],
                    |row| row.get(0),
                )
                .optional()?;
            match found {
                Some(id) => id,
                None => self.create_place(Some(&session_id), None, "{}", None, now)?,
            }
        };
        if self.get_membership(place, agent)?.is_none() {
            self.join(place, agent, Role::Participant, 0, now)?;
        }
        let said_seq = self.append(
            place,
            &NewEvent {
                kind: EventKind::Said,
                author_subject: None,
                author_external: Some(user_id.trim().to_string()),
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
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensures_place_and_appends_said() {
        let store = Store::new_in_memory().unwrap();
        let agent = store.subject_create(None, "Ada", "Helper", 10).unwrap();
        let first = store
            .agent_direct_message(agent, " user-1 ", "hello", 20)
            .unwrap()
            .unwrap();
        assert_eq!(first.session_id, format!("agent-msg-{agent}-user-1"));
        assert_eq!(first.said_seq, 1);
        let second = store
            .agent_direct_message(agent, "user-1", "again", 21)
            .unwrap()
            .unwrap();
        assert_eq!(second.place_id, first.place_id);
        assert_eq!(second.said_seq, 2);
        let address: String = store
            .c()
            .query_row(
                "SELECT address FROM places WHERE id=?1",
                params![first.place_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(address, first.session_id);
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
            .agent_direct_message(99, "user-1", "x", 22)
            .unwrap()
            .is_none());
    }
}
