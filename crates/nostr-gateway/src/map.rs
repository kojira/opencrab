//! watch JSONL → V3 said。origin 規約と版付きアンカー。

use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

const MAX_ANCHOR_FIELD_CHARS: usize = 128;

#[derive(Debug, Clone, Deserialize)]
pub struct WatchEvent {
    pub id: String,
    pub pubkey: String,
    #[serde(default)]
    pub npub: Option<String>,
    #[serde(default)]
    pub note_id: Option<String>,
    pub created_at: i64,
    pub kind: u32,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub tags: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Default,
    Immediate,
    Bundle,
}

impl Route {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Immediate => "immediate",
            Self::Bundle => "bundle",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub kind: LaneKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneKind {
    Default,
    Watch { id: i64 },
}

impl Lane {
    pub fn default_lane() -> Self {
        Self {
            kind: LaneKind::Default,
        }
    }

    pub fn watch(id: i64) -> Self {
        Self {
            kind: LaneKind::Watch { id },
        }
    }

    pub fn origin_token(&self) -> String {
        match self.kind {
            LaneKind::Default => "default".into(),
            LaneKind::Watch { id } => format!("watch:{id}"),
        }
    }

    pub fn watch_id(&self) -> Option<i64> {
        match self.kind {
            LaneKind::Default => None,
            LaneKind::Watch { id } => Some(id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaidMap {
    pub origin: String,
    pub author_id: String,
    pub text: String,
    pub route: Route,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundlePlace {
    pub bundle_id: String,
    pub index: u32,
    pub count: u32,
}

pub fn parse_watch_line(line: &str) -> Option<WatchEvent> {
    let t = line.trim();
    if !t.starts_with('{') {
        return None;
    }
    match serde_json::from_str::<WatchEvent>(t) {
        Ok(event) => Some(event),
        Err(error) => {
            tracing::warn!(%error, "watch jsonl object dropped");
            None
        }
    }
}

pub fn normalize_author_id(pubkey: &str) -> Option<String> {
    let s = pubkey.trim();
    if s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Some(s.to_ascii_lowercase());
    }
    None
}

pub fn decisive_origin(lane: &Lane, event_id: &str) -> String {
    format!("nostr:event:v1:{}:{event_id}", lane.origin_token())
}

pub fn classify_route(
    event: &WatchEvent,
    self_pubkey: &str,
    beyond_self: bool,
    lane: &Lane,
) -> Route {
    if matches!(lane.kind, LaneKind::Default) {
        return Route::Default;
    }
    if event.kind == 4 || event.kind == 1059 {
        return Route::Immediate;
    }
    if event.kind == 7 || event.kind == 6 || event.kind == 16 {
        return Route::Immediate;
    }
    let to_self = p_tag_is_self(event, self_pubkey);
    if event.kind == 30023 {
        return if to_self || e_tag_is_self(event, self_pubkey) {
            Route::Immediate
        } else {
            Route::Bundle
        };
    }
    if to_self {
        return Route::Immediate;
    }
    if !beyond_self && !has_e_tag(event) {
        return Route::Immediate;
    }
    Route::Bundle
}

pub fn map_event(
    event: &WatchEvent,
    self_pubkey: &str,
    beyond_self: bool,
    lane: &Lane,
    bundle: Option<&BundlePlace>,
) -> Option<SaidMap> {
    let event_id = normalize_event_id(&event.id)?;
    let author_id = normalize_author_id(&event.pubkey)?;
    let route = match bundle {
        Some(_) => Route::Bundle,
        None => classify_route(event, self_pubkey, beyond_self, lane),
    };
    let origin = decisive_origin(lane, &event_id);
    let history = history_text(event);
    let anchor = v1_anchor(
        event,
        self_pubkey,
        beyond_self,
        lane,
        route,
        &event_id,
        bundle,
    );
    Some(SaidMap {
        origin,
        author_id,
        text: format!("{anchor}\n{history}"),
        route,
    })
}

pub fn bundle_id(binding_id: &str, watch_id: i64, event_ids: &[String]) -> String {
    let mut data = Vec::new();
    data.extend_from_slice(b"nostr-bundle-v1\0");
    data.extend_from_slice(binding_id.as_bytes());
    data.push(0);
    data.extend_from_slice(watch_id.to_string().as_bytes());
    data.push(0);
    for id in event_ids {
        data.extend_from_slice(id.as_bytes());
    }
    hex_lower(&Sha256::digest(&data))
}

fn normalize_event_id(id: &str) -> Option<String> {
    normalize_author_id(id)
}

fn follow_key(raw: &str) -> String {
    normalize_author_id(raw).unwrap_or_else(|| raw.trim().to_ascii_lowercase())
}

fn has_e_tag(event: &WatchEvent) -> bool {
    event
        .tags
        .iter()
        .any(|t| t.first().map(|s| s == "e").unwrap_or(false))
}

fn p_tag_is_self(event: &WatchEvent, self_pubkey: &str) -> bool {
    let self_key = follow_key(self_pubkey);
    event.tags.iter().any(|t| {
        t.first().map(|s| s == "p").unwrap_or(false)
            && t.get(1).is_some_and(|p| follow_key(p) == self_key)
    })
}

fn e_tag_is_self(event: &WatchEvent, self_pubkey: &str) -> bool {
    let self_key = follow_key(self_pubkey);
    event.tags.iter().any(|t| {
        t.first().map(|s| s == "e").unwrap_or(false)
            && t.iter().skip(1).any(|v| follow_key(v) == self_key)
    })
}

fn inbound_kind_label(event: &WatchEvent) -> &'static str {
    if event.kind == 4 || event.kind == 1059 {
        return "DM";
    }
    if event.kind == 7 {
        return "リアクション";
    }
    if event.kind == 30023 {
        return "長文";
    }
    if has_e_tag(event) {
        return "リプライ";
    }
    "メンション"
}

fn sanitize_anchor_field(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_ANCHOR_FIELD_CHARS)
        .collect()
}

fn reply_target(event: &WatchEvent) -> &str {
    event.note_id.as_deref().unwrap_or(&event.id)
}

fn author_key(event: &WatchEvent) -> &str {
    event
        .npub
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&event.pubkey)
}

fn history_text(event: &WatchEvent) -> String {
    let anchor = format!(
        "[Nostr kind:{kind} {label} from={author} target={target}]",
        kind = event.kind,
        label = inbound_kind_label(event),
        author = sanitize_anchor_field(author_key(event)),
        target = sanitize_anchor_field(reply_target(event)),
    );
    if event.content.trim().is_empty() {
        anchor
    } else {
        format!("{}\n{anchor}", event.content)
    }
}

fn v1_anchor(
    event: &WatchEvent,
    self_pubkey: &str,
    beyond_cfg: bool,
    lane: &Lane,
    route: Route,
    event_id: &str,
    bundle: Option<&BundlePlace>,
) -> String {
    let p_self = p_tag_is_self(event, self_pubkey);
    let mut obj = Map::new();
    obj.insert("beyond_self".into(), json!(beyond_cfg));
    obj.insert(
        "bundle_id".into(),
        bundle
            .map(|b| Value::String(b.bundle_id.clone()))
            .unwrap_or(Value::Null),
    );
    obj.insert(
        "count".into(),
        bundle.map(|b| json!(b.count)).unwrap_or(Value::Null),
    );
    obj.insert("event_id".into(), json!(event_id));
    obj.insert("has_e".into(), json!(has_e_tag(event)));
    obj.insert(
        "index".into(),
        bundle.map(|b| json!(b.index)).unwrap_or(Value::Null),
    );
    obj.insert("kind".into(), json!(event.kind));
    obj.insert("p_self".into(), json!(p_self));
    obj.insert("route".into(), json!(route.as_str()));
    obj.insert(
        "watch_id".into(),
        lane.watch_id().map(|id| json!(id)).unwrap_or(Value::Null),
    );
    format!("[NOSTRGATE/V1 {}]", Value::Object(obj))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_pk() -> String {
        "11".repeat(32)
    }

    fn ev(kind: u32, tags: Vec<Vec<String>>) -> WatchEvent {
        WatchEvent {
            id: "aa".repeat(32),
            pubkey: "22".repeat(32),
            npub: None,
            note_id: Some("note1abc".into()),
            created_at: 1,
            kind,
            content: "hello".into(),
            tags,
        }
    }

    #[test]
    fn origin_convention() {
        let id = "aa".repeat(32);
        assert_eq!(
            decisive_origin(&Lane::default_lane(), &id),
            format!("nostr:event:v1:default:{id}")
        );
        assert_eq!(
            decisive_origin(&Lane::watch(17), &id),
            format!("nostr:event:v1:watch:17:{id}")
        );
    }

    #[test]
    fn v1_anchor_key_order_and_nulls() {
        let self_pk = self_pk();
        let event = ev(
            1,
            vec![
                vec!["p".into(), self_pk.clone()],
                vec!["e".into(), "bb".repeat(32)],
            ],
        );
        let mapped = map_event(&event, &self_pk, false, &Lane::watch(17), None).unwrap();
        let expected = format!(
            "[NOSTRGATE/V1 {{\"beyond_self\":false,\"bundle_id\":null,\"count\":null,\"event_id\":\"{}\",\"has_e\":true,\"index\":null,\"kind\":1,\"p_self\":true,\"route\":\"immediate\",\"watch_id\":17}}]\nhello\n[Nostr kind:1 リプライ from={} target=note1abc]",
            "aa".repeat(32),
            "22".repeat(32)
        );
        assert_eq!(mapped.text, expected);
        assert_eq!(
            mapped.origin,
            format!("nostr:event:v1:watch:17:{}", "aa".repeat(32))
        );
        assert_eq!(mapped.author_id, "22".repeat(32));
        assert_eq!(mapped.route, Route::Immediate);
    }

    #[test]
    fn default_lane_route_is_default() {
        let self_pk = self_pk();
        let event = ev(1, vec![vec!["p".into(), self_pk.clone()]]);
        let mapped = map_event(&event, &self_pk, false, &Lane::default_lane(), None).unwrap();
        assert_eq!(mapped.route, Route::Default);
        assert!(mapped.text.contains("\"route\":\"default\""));
        assert!(mapped.text.contains("\"watch_id\":null"));
        assert_eq!(
            mapped.origin,
            format!("nostr:event:v1:default:{}", "aa".repeat(32))
        );
    }

    #[test]
    fn dm_kind_is_immediate_not_discard() {
        let self_pk = self_pk();
        let event = ev(4, vec![]);
        let mapped = map_event(&event, &self_pk, true, &Lane::watch(1), None).unwrap();
        assert_eq!(mapped.route, Route::Immediate);
        let event = ev(1059, vec![]);
        let mapped = map_event(&event, &self_pk, true, &Lane::watch(1), None).unwrap();
        assert_eq!(mapped.route, Route::Immediate);
    }

    #[test]
    fn jsonl_skips_non_object() {
        assert!(parse_watch_line("info: subscribed").is_none());
        assert!(parse_watch_line("").is_none());
        let line = format!(
            r#"{{"id":"{}","pubkey":"{}","created_at":1,"kind":1,"content":"x","tags":[]}}"#,
            "aa".repeat(32),
            "22".repeat(32)
        );
        assert!(parse_watch_line(&line).is_some());
    }

    #[test]
    fn reject_non_hex_author() {
        let mut event = ev(1, vec![]);
        event.pubkey = "not-a-key".into();
        assert!(map_event(&event, &self_pk(), false, &Lane::default_lane(), None).is_none());
    }

    #[test]
    fn beyond_self_uses_watch_config_not_p_self() {
        let self_pk = self_pk();
        let event = ev(1, vec![]);
        let mapped = map_event(&event, &self_pk, false, &Lane::watch(17), None).unwrap();
        assert!(mapped.text.contains("\"beyond_self\":false"));
        assert!(mapped.text.contains("\"p_self\":false"));
        let mapped = map_event(&event, &self_pk, true, &Lane::watch(17), None).unwrap();
        assert!(mapped.text.contains("\"beyond_self\":true"));
        assert!(mapped.text.contains("\"p_self\":false"));
    }

    #[test]
    fn bundle_fields_and_deterministic_id() {
        let ids = vec!["aa".repeat(32), "bb".repeat(32)];
        let bid = bundle_id("bind-1", 17, &ids);
        assert_eq!(bid.len(), 64);
        assert_eq!(bid, bundle_id("bind-1", 17, &ids));
        assert_ne!(bid, bundle_id("bind-2", 17, &ids));
        let self_pk = self_pk();
        let event = ev(1, vec![]);
        let place = BundlePlace {
            bundle_id: bid.clone(),
            index: 1,
            count: 2,
        };
        let mapped = map_event(&event, &self_pk, true, &Lane::watch(17), Some(&place)).unwrap();
        assert_eq!(mapped.route, Route::Bundle);
        assert!(mapped.text.contains(&format!("\"bundle_id\":\"{bid}\"")));
        assert!(mapped.text.contains("\"index\":1"));
        assert!(mapped.text.contains("\"count\":2"));
        assert!(mapped.text.contains("\"route\":\"bundle\""));
    }
}
