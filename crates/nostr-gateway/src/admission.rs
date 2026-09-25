//! Nostr受信の許可判定。共有層へ渡す前にgateway内で完結させる。

use opencrab_gate_client::wire::SaidCaller;

use crate::config::AccessConfig;
use crate::map::{normalize_author_id, WatchEvent};

pub fn admit(event: &WatchEvent, self_pubkey: &str, access: &AccessConfig) -> Option<SaidCaller> {
    let author = normalize_author_id(&event.pubkey)?;
    let self_key = normalize_author_id(self_pubkey)?;
    if author == self_key || matches!(event.kind, 4 | 1059) {
        return None;
    }
    if contains(&access.owner, &author) {
        return Some(SaidCaller::Owner);
    }
    if let Some(agent_id) = access
        .co_agents
        .iter()
        .find_map(|(key, agent_id)| (normalized(key) == author).then(|| agent_id.clone()))
    {
        return Some(SaidCaller::CoAgent { agent_id });
    }
    if contains(&access.trusted_users, &author) {
        return Some(SaidCaller::TrustedUser);
    }
    if contains(&access.followees, &author) {
        return Some(SaidCaller::Agent);
    }
    None
}

fn contains(keys: &[String], author: &str) -> bool {
    keys.iter().any(|key| normalized(key) == author)
}

fn normalized(key: &str) -> String {
    normalize_author_id(key).unwrap_or_else(|| key.trim().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(author: &str, kind: u32) -> WatchEvent {
        WatchEvent {
            id: "aa".repeat(32),
            pubkey: author.into(),
            npub: None,
            note_id: None,
            created_at: 1,
            kind,
            content: "hello".into(),
            tags: vec![],
        }
    }

    #[test]
    fn rejects_self_dm_and_unknown_before_core() {
        let self_key = "11".repeat(32);
        let access = AccessConfig::default();
        assert_eq!(admit(&event(&self_key, 1), &self_key, &access), None);
        assert_eq!(admit(&event(&"22".repeat(32), 4), &self_key, &access), None);
        assert_eq!(admit(&event(&"22".repeat(32), 1), &self_key, &access), None);
    }

    #[test]
    fn maps_gateway_owned_roles_to_generic_caller() {
        let self_key = "11".repeat(32);
        let owner = "22".repeat(32);
        let trusted = "33".repeat(32);
        let followee = "44".repeat(32);
        let co = "55".repeat(32);
        let access = AccessConfig {
            owner: vec![owner.clone()],
            trusted_users: vec![trusted.clone()],
            followees: vec![followee.clone()],
            co_agents: [(co.clone(), "agent-b".into())].into_iter().collect(),
        };
        assert_eq!(
            admit(&event(&owner, 1), &self_key, &access),
            Some(SaidCaller::Owner)
        );
        assert_eq!(
            admit(&event(&trusted, 1), &self_key, &access),
            Some(SaidCaller::TrustedUser)
        );
        assert_eq!(
            admit(&event(&followee, 1), &self_key, &access),
            Some(SaidCaller::Agent)
        );
        assert_eq!(
            admit(&event(&co, 1), &self_key, &access),
            Some(SaidCaller::CoAgent {
                agent_id: "agent-b".into()
            })
        );
    }
}
