//! Discord adapter: protocol-2 変換と discovery。core/store は使わない。

mod voice_join;
mod voice_pipeline;
mod voice_session;

use std::sync::Arc;

use opencrab_port::{AddressKind, GateInstanceId, MembershipDiscovery};
use opencrab_voice::{build_stt, build_tts, SttProvider, TtsProvider, VoiceConfig};
use serde_json::{json, Value};
use serenity::model::channel::ChannelType;

pub const VOICE_CONFIG_B64_ENV: &str = "OPENCRAB_VOICE_CONFIG_B64";
pub const OWNER_AGENT_ID_ENV: &str = "OPENCRAB_GATE_OWNER_AGENT_ID";

pub use voice_join::{
    evaluate_join_voice, evaluate_leave_voice, parse_vc_channel_id, voice_caller_allowed,
    JoinVoicePlan, VoiceCaller, VoiceJoinDeny,
};
pub use voice_pipeline::{
    apply_voice_tick, clean_for_tts, should_transcribe, transcribe_segment_to_said, VoiceTickInput,
    VoiceTranscriptTarget, MIN_SEGMENT_RMS,
};
pub use voice_session::VoiceSessionManager;

pub const KIND_ID: &str = "discord";
pub const PROTOCOL: u64 = 2;
pub const ADDRESS_FORM: &str = "[0-9]+";
pub const ORIGIN_SCOPE: &str = "kind_address";
pub const INGRESS_DISCOVERY: &str = "membership";

#[derive(Clone, Debug)]
pub struct BootConfig {
    pub instance_id: GateInstanceId,
    pub revision: u64,
    pub socket: String,
    pub token: Option<String>,
    pub boot_error: Option<String>,
    /// runner が override>default を B64 で渡す。子は store を読まない。
    pub voice_config: VoiceConfig,
    /// dedicated の instance owner source agent UUID。TTS speaker。
    pub owner_agent_id: Option<String>,
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
    /// Message Create は空。VC STT は `source=discord_voice`。新 EventKind は作らない。
    pub metadata: Value,
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
    if !event.origin.is_empty() {
        object.insert("origin".into(), event.origin.clone().into());
    }
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
    if let Some(metadata) = event.metadata.as_object() {
        if !metadata.is_empty() {
            object.insert("metadata".into(), event.metadata.clone());
        }
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
        metadata: json!({}),
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
    let voice_config = read_voice_config_from_env(&mut get)?;
    let owner_agent_id = get(OWNER_AGENT_ID_ENV).filter(|value| !value.is_empty());
    Ok(BootConfig {
        instance_id,
        revision,
        socket,
        token,
        boot_error,
        voice_config,
        owner_agent_id,
    })
}

fn read_voice_config_from_env<F>(get: &mut F) -> Result<VoiceConfig, String>
where
    F: FnMut(&str) -> Option<String>,
{
    let Some(encoded) = get(VOICE_CONFIG_B64_ENV).filter(|value| !value.is_empty()) else {
        return Ok(VoiceConfig::default());
    };
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded.trim())
        .map_err(|_| "OPENCRAB_VOICE_CONFIG_B64 must be standard base64".to_string())?;
    let json = String::from_utf8(bytes)
        .map_err(|_| "OPENCRAB_VOICE_CONFIG_B64 must be UTF-8 JSON".to_string())?;
    serde_json::from_str(&json)
        .map_err(|error| format!("OPENCRAB_VOICE_CONFIG_B64 is not VoiceConfig JSON: {error}"))
}

/// enabled 失敗は警告して None（本体どおり起動は止めない）。
pub fn try_build_voice_providers(
    cfg: &VoiceConfig,
) -> Option<(Arc<dyn SttProvider>, Arc<dyn TtsProvider>)> {
    if !cfg.enabled {
        return None;
    }
    match (build_stt(&cfg.stt), build_tts(&cfg.tts)) {
        (Ok(stt), Ok(tts)) => Some((stt, tts)),
        (stt, tts) => {
            if let Err(error) = stt {
                eprintln!("opencrab-discord-gate: voice STT provider init failed: {error}");
            }
            if let Err(error) = tts {
                eprintln!("opencrab-discord-gate: voice TTS provider init failed: {error}");
            }
            None
        }
    }
}

/// STT 成功を Message Create と同じ `said` にする。`source=discord_voice` のみ metadata。
pub fn map_voice_transcript(
    text_channel_id: &str,
    author_id: &str,
    author_display: Option<&str>,
    text: &str,
    discovery: MembershipDiscovery,
) -> Option<SaidEvent> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(SaidEvent {
        address: text_channel_id.to_string(),
        author_id: author_id.to_string(),
        author_display: author_display.map(str::to_string),
        content_text: text.to_string(),
        origin: String::new(),
        reply_to: None,
        attachments: Vec::new(),
        discovery,
        metadata: json!({"source": "discord_voice"}),
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
        assert_eq!(kept.metadata, json!({}));
    }

    #[test]
    fn voice_transcript_is_said_with_discord_voice_metadata() {
        let discovery = MembershipDiscovery {
            address_kind: AddressKind::Guild,
            guild_id: Some("1".into()),
            label: Some("general".into()),
        };
        let event =
            map_voice_transcript("10", "user-7", Some("alice"), "  こんにちは  ", discovery)
                .unwrap();
        assert_eq!(event.address, "10");
        assert_eq!(event.author_id, "user-7");
        assert_eq!(event.content_text, "こんにちは");
        assert!(event.origin.is_empty());
        assert_eq!(event.metadata["source"], "discord_voice");
        let wire = said_event_to_wire("event", &event);
        assert_eq!(wire["kind"], "said");
        assert_eq!(wire["metadata"]["source"], "discord_voice");
        assert!(wire.get("origin").is_none());
        assert!(map_voice_transcript(
            "10",
            "user-7",
            None,
            "   ",
            MembershipDiscovery {
                address_kind: AddressKind::Guild,
                guild_id: Some("1".into()),
                label: None,
            },
        )
        .is_none());
    }

    #[test]
    fn missing_voice_config_env_is_default_disabled() {
        let boot = read_boot_config_from_env(|name| match name {
            "OPENCRAB_GATE_INSTANCE_ID" => Some("018f0000-0000-7000-8000-000000000021".into()),
            "OPENCRAB_GATE_REVISION" => Some("1".into()),
            "OPENCRAB_GATE_SOCKET" => Some("/tmp/core.sock".into()),
            _ => None,
        })
        .unwrap();
        assert!(!boot.voice_config.enabled);
        assert_eq!(boot.owner_agent_id, None);
    }

    #[test]
    fn voice_config_b64_must_be_voice_config_json() {
        let err = read_boot_config_from_env(|name| match name {
            "OPENCRAB_GATE_INSTANCE_ID" => Some("018f0000-0000-7000-8000-000000000021".into()),
            "OPENCRAB_GATE_REVISION" => Some("1".into()),
            "OPENCRAB_GATE_SOCKET" => Some("/tmp/core.sock".into()),
            "OPENCRAB_VOICE_CONFIG_B64" => Some("%%%".into()),
            _ => None,
        })
        .unwrap_err();
        assert!(err.contains("base64"), "{err}");
    }

    #[test]
    fn disabled_or_unknown_provider_builds_no_voice_runtime() {
        assert!(try_build_voice_providers(&VoiceConfig::default()).is_none());
        let cfg = VoiceConfig {
            enabled: true,
            stt: opencrab_voice::SttConfig {
                provider: "not-a-provider".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(try_build_voice_providers(&cfg).is_none());
    }
}
