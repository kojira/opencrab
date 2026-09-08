//! Nostr instance / binding の決定的 ID と §3.1 の lane 畳み。

use opencrab_db::queries::SessionWatchRow;

use crate::session::{nostr_session_id, NOSTR_SESSION_PREFIX};

const DNS_NS: uuid::Uuid = uuid::Uuid::NAMESPACE_DNS;

/// `kind_id="nostr"` の instance ID。
pub fn nostr_instance_id(agent_id: &str) -> String {
    uuid::Uuid::new_v5(
        &DNS_NS,
        format!("opencrab:nostr:instance:{agent_id}").as_bytes(),
    )
    .to_string()
}

/// address は既存 session_id と byte-equal。
pub fn nostr_binding_id(agent_id: &str, address: &str) -> String {
    uuid::Uuid::new_v5(
        &DNS_NS,
        format!("opencrab:nostr:binding:{agent_id}:{address}").as_bytes(),
    )
    .to_string()
}

/// 1 session = 1 binding。watch 行は id ASC の N lane。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBindingPlan {
    pub address: String,
    pub binding_id: String,
    pub skip_default_lane: bool,
    pub watch_ids: Vec<i64>,
}

/// `nostr-{agent}` に watch があれば default lane を起動しない。
pub fn skip_default_loop(watches: &[SessionWatchRow], default_session_id: &str) -> bool {
    watches.iter().any(|w| w.session_id == default_session_id)
}

/// session ごと 1 binding。N watch を 1 購読へ潰さない。
pub fn plan_session_bindings(
    agent_id: &str,
    watches: &[SessionWatchRow],
) -> Result<Vec<SessionBindingPlan>, BindingPlanError> {
    let default_sid = nostr_session_id(agent_id);
    let mut by_session: Vec<(String, Vec<i64>)> = Vec::new();
    for w in watches {
        if !w.session_id.starts_with(NOSTR_SESSION_PREFIX) {
            return Err(BindingPlanError::NotNostrSession { watch_id: w.id });
        }
        if w.interval_secs <= 0 {
            return Err(BindingPlanError::NonPositiveInterval { watch_id: w.id });
        }
        match by_session.iter_mut().find(|(sid, _)| sid == &w.session_id) {
            Some((_, ids)) => ids.push(w.id),
            None => by_session.push((w.session_id.clone(), vec![w.id])),
        }
    }
    for (_, ids) in &mut by_session {
        ids.sort_unstable();
    }
    let mut plans = Vec::new();
    if !by_session.iter().any(|(sid, _)| sid == &default_sid) {
        plans.push(SessionBindingPlan {
            address: default_sid.clone(),
            binding_id: nostr_binding_id(agent_id, &default_sid),
            skip_default_lane: false,
            watch_ids: Vec::new(),
        });
    }
    for (sid, watch_ids) in by_session {
        let skip_default_lane = sid == default_sid && !watch_ids.is_empty();
        plans.push(SessionBindingPlan {
            binding_id: nostr_binding_id(agent_id, &sid),
            address: sid,
            skip_default_lane,
            watch_ids,
        });
    }
    Ok(plans)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingPlanError {
    NotNostrSession { watch_id: i64 },
    NonPositiveInterval { watch_id: i64 },
}

impl std::fmt::Display for BindingPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotNostrSession { watch_id } => {
                write!(
                    f,
                    "session_watches.id={watch_id} の session_id が nostr- 系ではない"
                )
            }
            Self::NonPositiveInterval { watch_id } => {
                write!(
                    f,
                    "session_watches.id={watch_id} の interval_secs が正の整数ではない"
                )
            }
        }
    }
}

impl std::error::Error for BindingPlanError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(id: i64, session_id: &str, interval: i64) -> SessionWatchRow {
        SessionWatchRow {
            id,
            session_id: session_id.into(),
            agent_id: "a1".into(),
            interval_secs: interval,
            filter_json: "{}".into(),
            created_at: "t".into(),
        }
    }

    #[test]
    fn instance_and_binding_ids_are_deterministic() {
        let a = nostr_instance_id("a1");
        let b = nostr_instance_id("a1");
        assert_eq!(a, b);
        assert_ne!(a, nostr_instance_id("a2"));
        let bind = nostr_binding_id("a1", "nostr-a1");
        assert_eq!(bind, nostr_binding_id("a1", "nostr-a1"));
        assert_ne!(bind, nostr_binding_id("a1", "nostr-a1-other"));
        assert_eq!(
            bind,
            uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_DNS,
                b"opencrab:nostr:binding:a1:nostr-a1"
            )
            .to_string()
        );
    }

    #[test]
    fn zero_watches_is_default_lane_one_binding() {
        let plans = plan_session_bindings("a1", &[]).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].address, "nostr-a1");
        assert!(!plans[0].skip_default_lane);
        assert!(plans[0].watch_ids.is_empty());
    }

    #[test]
    fn default_session_watches_skip_default_lane_and_share_one_binding() {
        let watches = vec![watch(17, "nostr-a1", 60), watch(3, "nostr-a1", 30)];
        let plans = plan_session_bindings("a1", &watches).unwrap();
        assert_eq!(plans.len(), 1);
        assert!(plans[0].skip_default_lane);
        assert_eq!(plans[0].watch_ids, vec![3, 17]);
        assert_eq!(plans[0].address, "nostr-a1");
        assert!(skip_default_loop(&watches, "nostr-a1"));
    }

    #[test]
    fn other_nostr_session_is_its_own_binding() {
        let watches = vec![
            watch(2, "nostr-a1-extra", 15),
            watch(1, "nostr-a1-extra", 45),
        ];
        let plans = plan_session_bindings("a1", &watches).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].address, "nostr-a1");
        assert!(!plans[0].skip_default_lane);
        assert_eq!(plans[1].address, "nostr-a1-extra");
        assert!(!plans[1].skip_default_lane);
        assert_eq!(plans[1].watch_ids, vec![1, 2]);
    }

    #[test]
    fn non_nostr_session_is_fail_loud() {
        let err = plan_session_bindings("a1", &[watch(1, "discord-x", 10)]).unwrap_err();
        assert!(matches!(
            err,
            BindingPlanError::NotNostrSession { watch_id: 1 }
        ));
    }

    #[test]
    fn non_positive_interval_is_fail_loud() {
        let err = plan_session_bindings("a1", &[watch(9, "nostr-a1", 0)]).unwrap_err();
        assert!(matches!(
            err,
            BindingPlanError::NonPositiveInterval { watch_id: 9 }
        ));
    }
}
