//! watch JSONL → V3 said。origin 規約と版付きアンカー。

use opencrab_gate_client::wire::Attachment;
use serde::Deserialize;
use sha2::{Digest, Sha256};

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
    pub attachments: Vec<Attachment>,
    pub system_context: String,
    pub reply_target: Option<String>,
    pub route: Route,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundlePlace {
    pub bundle_id: String,
    pub index: u32,
    pub count: u32,
    pub origins: Vec<String>,
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
        return Route::Immediate;
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

/// 自分（self_pubkey）への `#p` タグを持つイベントか。
///
/// default(mention) 車線は `mention_lane_filter` の `--npub`（self への #p）で、こうしたイベントを
/// **必ず**拾って即時処理する。watch(timeline) 車線が同じイベントを先に握っても、自分宛て `#p` は
/// default 車線へ即時処理を譲る（`run::handle_line` で watch 車線側を捨てる）ための判定。owner が
/// フォロイーにも含まれると、owner のメンション/リプライが両車線に届いてレースになり、watch 車線が
/// 勝つと watch origin で取り込まれて権限デバウンスへ降格する取りこぼし（QC #10）が起きるのを防ぐ。
pub fn is_self_p_tag_mention(event: &WatchEvent, self_pubkey: &str) -> bool {
    p_tag_is_self(event, self_pubkey)
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
    let _ = (self_pubkey, beyond_self);
    let attachments = image_urls(event)
        .into_iter()
        .map(|url| Attachment::ImageUrl { url })
        .collect();
    Some(SaidMap {
        origin,
        author_id: author_id.clone(),
        text: history,
        attachments,
        system_context: bundle
            .map(|place| bundle_response_context(place.count))
            .unwrap_or_else(|| response_context(event, &author_id)),
        reply_target: bundle.is_none().then(|| parent_event_id(event)).flatten(),
        route,
    })
}

pub fn bundle_members_line(origins: &[String]) -> String {
    format!(
        "[NOSTRBUNDLE/V1 {}]",
        serde_json::to_string(origins).expect("string list serializes")
    )
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

fn bundle_response_context(count: u32) -> String {
    format!(
        "[Nostr] タイムラインの束ね（{count} 件）です。窓内を1ターンの文脈に載せています。\
         心が動いた投稿には本文をそのまま書いて独立投稿で触れてよいです。\
         特定投稿に反応するなら reply(e番号, 本文)／reaction(e番号)／repost(e番号) を使ってください。\
         反応不要なら NO_REPLY とだけ答えてください。"
    )
}

fn response_context(event: &WatchEvent, author_id: &str) -> String {
    let short: String = author_id.chars().take(12).collect();
    format!(
        "[Nostr] {short}… さんの投稿（kind:{}／{}）への応答です。\n\
         普通の投稿は本文をそのまま書いてください。\n\
         この投稿へ返信するなら reply(e番号, 本文)、リアクションは reaction(e番号)、\
         リポストは repost(e番号) を使ってください。\n\
         反応が不要なら NO_REPLY とだけ答えてください。",
        event.kind,
        inbound_kind_label(event),
    )
}

fn image_urls(event: &WatchEvent) -> Vec<String> {
    let mut urls = extract_image_urls(&event.content);
    for tag in &event.tags {
        if tag.first().is_some_and(|name| name == "imeta") {
            for value in tag.iter().skip(1) {
                if let Some(url) = value.strip_prefix("url ") {
                    push_image_url(&mut urls, url.trim());
                }
            }
        }
    }
    urls
}

fn extract_image_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for token in text.split_whitespace() {
        let candidate = token.trim_matches(|c: char| {
            matches!(
                c,
                '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '"' | '\'' | ',' | ';'
            )
        });
        push_image_url(&mut urls, candidate);
    }
    urls
}

fn push_image_url(urls: &mut Vec<String>, candidate: &str) {
    let url = candidate.trim_end_matches([')', ']', '}', '.', ',', '!', '?']);
    if !url.starts_with("https://") {
        return;
    }
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if [".jpg", ".jpeg", ".png", ".gif", ".webp"]
        .iter()
        .any(|extension| path.ends_with(extension))
        && !urls.iter().any(|existing| existing == url)
    {
        urls.push(url.to_string());
    }
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

fn history_text(event: &WatchEvent) -> String {
    // §9A.2: from=（話者 pubkey と同一の二重表記）と target=（say ベース返信は発端 origin へ
    // 自動配送されるため LLM 指定不要）を会話へ出さない。種別ラベルだけ残す。npub/note/64hex
    // の識別子は会話から排除する（トークン削減の主因）。
    let anchor = format!(
        "[Nostr kind:{kind} {label}]",
        kind = event.kind,
        label = inbound_kind_label(event),
    );
    if event.content.trim().is_empty() {
        anchor
    } else {
        format!("{}\n{anchor}", event.content)
    }
}

/// 返信/リアクション/リポストが指す対象ノートの event_id（NIP-10: `reply` マーク優先・無ければ
/// 最後の e タグ）。非 hex や e タグ無しは None。会話表示の `(reply→e番号)` 解決用（row295c 6b）。
fn parent_event_id(event: &WatchEvent) -> Option<String> {
    let e_tags: Vec<&Vec<String>> = event
        .tags
        .iter()
        .filter(|t| t.first().map(|s| s == "e").unwrap_or(false))
        .collect();
    let chosen = e_tags
        .iter()
        .find(|t| t.get(3).map(|m| m == "reply").unwrap_or(false))
        .or_else(|| e_tags.last())?;
    chosen.get(1).and_then(|v| normalize_author_id(v))
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
        assert_eq!(mapped.text, "hello\n[Nostr kind:1 リプライ]");
        assert_eq!(mapped.reply_target, Some("bb".repeat(32)));
        assert!(mapped.system_context.contains("kind:1／リプライ"));
        assert_eq!(
            mapped.origin,
            format!("nostr:event:v1:watch:17:{}", "aa".repeat(32))
        );
        assert_eq!(mapped.author_id, "22".repeat(32));
        assert_eq!(mapped.route, Route::Immediate);
    }

    #[test]
    fn parent_event_id_prefers_reply_marker_else_last_e() {
        // reply マーク優先。
        let marked = ev(
            1,
            vec![
                vec!["e".into(), "aa".repeat(32), String::new(), "root".into()],
                vec!["e".into(), "bb".repeat(32), String::new(), "reply".into()],
            ],
        );
        assert_eq!(
            parent_event_id(&marked).as_deref(),
            Some("bb".repeat(32).as_str())
        );
        // マーク無しは最後の e。
        let unmarked = ev(
            1,
            vec![
                vec!["e".into(), "aa".repeat(32)],
                vec!["e".into(), "cc".repeat(32)],
            ],
        );
        assert_eq!(
            parent_event_id(&unmarked).as_deref(),
            Some("cc".repeat(32).as_str())
        );
        // e タグ無しは None。
        assert_eq!(
            parent_event_id(&ev(1, vec![vec!["p".into(), "dd".repeat(32)]])),
            None
        );
    }

    #[test]
    fn is_self_p_tag_mention_matches_only_self_p_tag() {
        let self_pk = self_pk();
        // 自分宛て #p → true（default 車線へ譲る対象）。
        let to_self = ev(1, vec![vec!["p".into(), self_pk.clone()]]);
        assert!(is_self_p_tag_mention(&to_self, &self_pk));
        // 他人宛て #p → false（watch 車線が扱う）。
        let to_other = ev(1, vec![vec!["p".into(), "99".repeat(32)]]);
        assert!(!is_self_p_tag_mention(&to_other, &self_pk));
        // #p なし → false。
        let no_p = ev(1, vec![vec!["e".into(), "bb".repeat(32)]]);
        assert!(!is_self_p_tag_mention(&no_p, &self_pk));
    }

    #[test]
    fn mention_lane_said_is_immediate() {
        let self_pk = self_pk();
        let event = ev(1, vec![vec!["p".into(), self_pk.clone()]]);
        let mapped = map_event(&event, &self_pk, false, &Lane::default_lane(), None).unwrap();
        assert_eq!(mapped.route, Route::Immediate);
        assert!(!mapped.text.contains("NOSTRGATE"));
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
        assert_eq!(mapped.route, Route::Immediate);
        let mapped = map_event(&event, &self_pk, true, &Lane::watch(17), None).unwrap();
        assert_eq!(mapped.route, Route::Bundle);
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
        let origins = vec![
            decisive_origin(&Lane::watch(17), &ids[0]),
            decisive_origin(&Lane::watch(17), &ids[1]),
        ];
        let place = BundlePlace {
            bundle_id: bid.clone(),
            index: 1,
            count: 2,
            origins: origins.clone(),
        };
        let mapped = map_event(&event, &self_pk, true, &Lane::watch(17), Some(&place)).unwrap();
        assert_eq!(mapped.route, Route::Bundle);
        assert_eq!(mapped.text, "hello\n[Nostr kind:1 メンション]");
        assert!(!mapped.text.contains("NOSTRBUNDLE"));
    }
}
