//! Discord adapter: protocol-2 変換と discovery。core/store は使わない。

use opencrab_port::{AddressKind, GateInstanceId, MembershipDiscovery};
use serde_json::{json, Value};
use serenity::model::channel::ChannelType;

pub const KIND_ID: &str = "discord";
pub const PROTOCOL: u64 = 2;
pub const ADDRESS_FORM: &str = "[0-9]+";
pub const ORIGIN_SCOPE: &str = "kind_address";
pub const INGRESS_DISCOVERY: &str = "membership";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootConfig {
    pub instance_id: GateInstanceId,
    pub revision: u64,
    pub socket: String,
    pub token: Option<String>,
    pub boot_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelFacts {
    pub channel_type: Option<ChannelType>,
    pub guild_id: Option<String>,
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscordDiscovery {
    pub discovery: MembershipDiscovery,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaidEvent {
    pub address: String,
    pub author_id: String,
    pub author_display: Option<String>,
    pub content_text: String,
    pub origin: String,
    pub reply_to: Option<String>,
    pub attachments: Vec<Value>,
    pub discovery: MembershipDiscovery,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DiscoveryError {
    Unresolved,
}

/// Serenity `ChannelType` → `address_kind`（v15 §2.2 全 variant）。
pub fn address_kind_from_channel_type(channel_type: ChannelType) -> Option<AddressKind> {
    match channel_type {
        ChannelType::Private | ChannelType::GroupDm => Some(AddressKind::Dm),
        ChannelType::NewsThread | ChannelType::PublicThread | ChannelType::PrivateThread => {
            Some(AddressKind::Thread)
        }
        ChannelType::Text
        | ChannelType::Voice
        | ChannelType::Category
        | ChannelType::News
        | ChannelType::Stage
        | ChannelType::Directory
        | ChannelType::Forum => Some(AddressKind::Guild),
        ChannelType::Unknown(_) => None,
        _ => None,
    }
}

/// dispatch → cache → http の順で既に集めた facts から carrier を作る。
/// type が確定できない、または guild なのに guild_id が空なら fail-closed。
pub fn resolve_discord_discovery(facts: &ChannelFacts) -> Result<DiscordDiscovery, DiscoveryError> {
    let address_kind = facts
        .channel_type
        .and_then(address_kind_from_channel_type)
        .ok_or(DiscoveryError::Unresolved)?;
    let guild_id = match address_kind {
        AddressKind::Guild => {
            let guild_id = facts
                .guild_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or(DiscoveryError::Unresolved)?;
            Some(guild_id.to_string())
        }
        AddressKind::Dm | AddressKind::Thread => None,
    };
    let label = facts
        .label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Ok(DiscordDiscovery {
        discovery: MembershipDiscovery {
            address_kind,
            guild_id,
            label,
        },
    })
}

pub fn discovery_to_wire(discovery: &MembershipDiscovery) -> Value {
    let mut object = serde_json::Map::new();
    object.insert(
        "address_kind".into(),
        discovery.address_kind.as_wire().into(),
    );
    if let Some(guild_id) = &discovery.guild_id {
        object.insert("guild_id".into(), guild_id.clone().into());
    }
    if let Some(label) = &discovery.label {
        object.insert("label".into(), label.clone().into());
    }
    Value::Object(object)
}

pub fn protocol2_hello(instance_id: &GateInstanceId, revision: u64) -> Value {
    json!({
        "id": "hello-1",
        "m": "hello",
        "protocol": PROTOCOL,
        "kind_id": KIND_ID,
        "instance_id": instance_id.as_str(),
        "revision": revision,
        "origin_scope": ORIGIN_SCOPE,
        "address_form": ADDRESS_FORM,
        "ingress_discovery": INGRESS_DISCOVERY,
        "tools": [],
        "effects": ["say", "react", "ui"],
        "capabilities": [],
        "actions": []
    })
}

pub fn protocol2_ready(id: &str, connection_epoch: u64) -> Value {
    json!({
        "id": id,
        "m": "ready",
        "connection_epoch": connection_epoch
    })
}

pub fn protocol2_failed(id: &str, connection_epoch: u64, code: &str) -> Value {
    json!({
        "id": id,
        "m": "failed",
        "connection_epoch": connection_epoch,
        "code": code
    })
}

pub fn said_event_to_wire(id: &str, event: &SaidEvent) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("id".into(), id.into());
    object.insert("m".into(), "event".into());
    object.insert("kind".into(), "said".into());
    object.insert("address".into(), event.address.clone().into());
    object.insert("author".into(), json!({"id": event.author_id}));
    object.insert("content".into(), json!({"text": event.content_text}));
    object.insert("origin".into(), event.origin.clone().into());
    object.insert("discovery".into(), discovery_to_wire(&event.discovery));
    if let Some(reply_to) = &event.reply_to {
        object.insert("reply_to".into(), reply_to.clone().into());
    }
    if !event.attachments.is_empty() {
        object.insert(
            "attachments".into(),
            Value::Array(event.attachments.clone()),
        );
    }
    Value::Object(object)
}

#[derive(Clone, Debug)]
pub struct MessageCreateInput<'a> {
    pub channel_id: &'a str,
    pub message_id: &'a str,
    pub author_id: &'a str,
    pub self_id: &'a str,
    pub content: &'a str,
    pub referenced_message_id: Option<&'a str>,
    pub image_urls: &'a [(&'a str, Option<&'a str>)],
    pub non_image_notes: &'a [String],
    pub discovery: MembershipDiscovery,
}

/// Message Create → `said`。自 bot は呼ぶ側で除外する。
pub fn map_message_create(input: MessageCreateInput<'_>) -> Option<SaidEvent> {
    // 自 bot だけ除外し、他 bot は通す（v15 §3 Message Create）。
    if exclude_self_author(input.author_id, input.self_id) {
        return None;
    }
    let mut text = input.content.to_string();
    for note in input.non_image_notes {
        if !note.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(note);
        }
    }
    let attachments = input
        .image_urls
        .iter()
        .map(|(url, origin_author)| {
            let mut attachment = serde_json::Map::new();
            attachment.insert("kind".into(), "image".into());
            attachment.insert("url".into(), (*url).into());
            if let Some(origin_author) = origin_author {
                attachment.insert("origin_author".into(), (*origin_author).into());
            }
            Value::Object(attachment)
        })
        .collect();
    Some(SaidEvent {
        address: input.channel_id.to_string(),
        author_id: input.author_id.to_string(),
        author_display: None,
        content_text: text,
        origin: input.message_id.to_string(),
        reply_to: input.referenced_message_id.map(str::to_string),
        attachments,
        discovery: input.discovery,
    })
}

pub fn exclude_self_author(author_id: &str, self_id: &str) -> bool {
    author_id == self_id
}

pub fn read_boot_config_from_env<F>(mut get: F) -> Result<BootConfig, String>
where
    F: FnMut(&str) -> Option<String>,
{
    let instance = get("OPENCRAB_GATE_INSTANCE_ID")
        .ok_or_else(|| "OPENCRAB_GATE_INSTANCE_ID is required".to_string())?;
    let instance_id = GateInstanceId::parse(instance)?;
    let revision = get("OPENCRAB_GATE_REVISION")
        .ok_or_else(|| "OPENCRAB_GATE_REVISION is required".to_string())?
        .parse::<u64>()
        .map_err(|_| "OPENCRAB_GATE_REVISION must be u64".to_string())?;
    let socket = get("OPENCRAB_GATE_SOCKET")
        .ok_or_else(|| "OPENCRAB_GATE_SOCKET is required".to_string())?;
    if socket.is_empty() {
        return Err("OPENCRAB_GATE_SOCKET must not be empty".into());
    }
    let boot_error = get("OPENCRAB_GATE_BOOT_ERROR_CODE").filter(|value| !value.is_empty());
    let token = get("OPENCRAB_GATE_TOKEN").filter(|value| !value.is_empty());
    Ok(BootConfig {
        instance_id,
        revision,
        socket,
        token,
        boot_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_type_table_matches_v15() {
        assert_eq!(
            address_kind_from_channel_type(ChannelType::Private),
            Some(AddressKind::Dm)
        );
        assert_eq!(
            address_kind_from_channel_type(ChannelType::GroupDm),
            Some(AddressKind::Dm)
        );
        assert_eq!(
            address_kind_from_channel_type(ChannelType::NewsThread),
            Some(AddressKind::Thread)
        );
        assert_eq!(
            address_kind_from_channel_type(ChannelType::PublicThread),
            Some(AddressKind::Thread)
        );
        assert_eq!(
            address_kind_from_channel_type(ChannelType::PrivateThread),
            Some(AddressKind::Thread)
        );
        assert_eq!(
            address_kind_from_channel_type(ChannelType::Text),
            Some(AddressKind::Guild)
        );
        assert_eq!(
            address_kind_from_channel_type(ChannelType::Unknown(99)),
            None
        );
    }

    #[test]
    fn guild_discovery_requires_nonempty_guild_id() {
        let err = resolve_discord_discovery(&ChannelFacts {
            channel_type: Some(ChannelType::Text),
            guild_id: None,
            label: Some("general".into()),
        });
        assert_eq!(err, Err(DiscoveryError::Unresolved));
        let ok = resolve_discord_discovery(&ChannelFacts {
            channel_type: Some(ChannelType::Text),
            guild_id: Some("123".into()),
            label: Some("general".into()),
        })
        .unwrap();
        assert_eq!(ok.discovery.address_kind, AddressKind::Guild);
        assert_eq!(ok.discovery.guild_id.as_deref(), Some("123"));
        assert_eq!(ok.discovery.label.as_deref(), Some("general"));
    }

    #[test]
    fn dm_and_thread_forbid_guild_id_on_wire() {
        let dm = resolve_discord_discovery(&ChannelFacts {
            channel_type: Some(ChannelType::Private),
            guild_id: Some("ignored".into()),
            label: None,
        })
        .unwrap();
        assert_eq!(dm.discovery.address_kind, AddressKind::Dm);
        assert_eq!(dm.discovery.guild_id, None);
        let thread = resolve_discord_discovery(&ChannelFacts {
            channel_type: Some(ChannelType::PublicThread),
            guild_id: Some("ignored".into()),
            label: Some("thread".into()),
        })
        .unwrap();
        assert_eq!(thread.discovery.address_kind, AddressKind::Thread);
        assert_eq!(thread.discovery.guild_id, None);
    }

    #[test]
    fn hello_is_protocol2_membership() {
        let instance =
            GateInstanceId::parse("018f0000-0000-7000-8000-000000000021".to_string()).unwrap();
        let hello = protocol2_hello(&instance, 1);
        assert_eq!(hello["protocol"], 2);
        assert_eq!(hello["kind_id"], "discord");
        assert_eq!(hello["ingress_discovery"], "membership");
        assert_eq!(hello["origin_scope"], "kind_address");
        assert_eq!(hello["effects"], json!(["say", "react", "ui"]));
        assert_eq!(hello["tools"], json!([]));
    }

    #[test]
    fn self_message_is_excluded_other_bot_is_kept() {
        let discovery = MembershipDiscovery {
            address_kind: AddressKind::Guild,
            guild_id: Some("1".into()),
            label: None,
        };
        assert!(map_message_create(MessageCreateInput {
            channel_id: "10",
            message_id: "20",
            author_id: "bot-self",
            self_id: "bot-self",
            content: "hi",
            referenced_message_id: None,
            image_urls: &[],
            non_image_notes: &[],
            discovery: discovery.clone(),
        })
        .is_none());
        let kept = map_message_create(MessageCreateInput {
            channel_id: "10",
            message_id: "20",
            author_id: "other-bot",
            self_id: "bot-self",
            content: "hi",
            referenced_message_id: None,
            image_urls: &[],
            non_image_notes: &[],
            discovery,
        })
        .unwrap();
        assert_eq!(kept.author_id, "other-bot");
        assert_eq!(kept.origin, "20");
    }
}
