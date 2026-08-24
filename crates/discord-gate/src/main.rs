//! opencrab-discord-gate — Discord I/O と protocol 2 変換だけ。core/store は使わない。
//!
//! 子は runner が exec する。token は `OPENCRAB_GATE_TOKEN`。argv にも log にも出さない。

use opencrab_discord_gate::{
    address_kind_from_channel_type, exclude_self_author, map_message_create, protocol2_failed,
    protocol2_hello, protocol2_ready, read_boot_config_from_env, resolve_discord_discovery,
    said_event_to_wire, try_build_voice_providers, ChannelFacts, DiscoveryError,
    MessageCreateInput, SaidEvent, VoiceSessionManager,
};
use opencrab_port::GateInstanceId;
use serde_json::{json, Value};
use serenity::all::{
    Channel, ChannelId, ChannelType, Context, CreateMessage, EditMessage, EventHandler,
    GatewayIntents, Http, Message, MessageId, ReactionType, Ready,
};
use serenity::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex};

const MAX_LINE: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

enum Resp {
    Ok(Value),
    Err(Value),
}

struct CoreLink {
    out: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<String, oneshot::Sender<Resp>>>,
    next_id: AtomicU64,
    active: AtomicBool,
}

impl CoreLink {
    fn new(out: mpsc::UnboundedSender<String>) -> Arc<Self> {
        Arc::new(Self {
            out,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(2),
            active: AtomicBool::new(false),
        })
    }

    fn send_line(&self, value: &Value) {
        let _ = self.out.send(value.to_string());
    }

    async fn request(&self, mut body: Value) -> Result<Value, String> {
        let id = format!("g-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        body.as_object_mut()
            .ok_or_else(|| "request body must be object".to_string())?
            .insert("id".into(), id.clone().into());
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        self.send_line(&body);
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(Resp::Ok(value))) => Ok(value),
            Ok(Ok(Resp::Err(value))) => Err(value.to_string()),
            Ok(Err(_)) => Err("core response dropped".into()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err("core request timeout".into())
            }
        }
    }
}

struct Handler {
    core: Arc<CoreLink>,
    self_id: Arc<Mutex<Option<String>>>,
    ready: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        *self.self_id.lock().await = Some(ready.user.id.to_string());
        if let Some(tx) = self.ready.lock().await.take() {
            let _ = tx.send(());
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if !self.core.active.load(Ordering::SeqCst) {
            return;
        }
        let self_id = match *self.self_id.lock().await {
            Some(ref id) => id.clone(),
            None => return,
        };
        if exclude_self_author(&msg.author.id.to_string(), &self_id) {
            return;
        }
        let facts = match collect_channel_facts(&ctx, &msg).await {
            Ok(facts) => facts,
            Err(DiscoveryError::Unresolved) => {
                eprintln!(
                    "opencrab-discord-gate: discord_discovery_unresolved address={} kind=message_create",
                    msg.channel_id
                );
                return;
            }
        };
        let discovery = match resolve_discord_discovery(&facts) {
            Ok(resolved) => resolved.discovery,
            Err(DiscoveryError::Unresolved) => {
                eprintln!(
                    "opencrab-discord-gate: discord_discovery_unresolved address={} kind=message_create",
                    msg.channel_id
                );
                return;
            }
        };
        let mut image_urls = Vec::new();
        let mut notes = Vec::new();
        for attachment in &msg.attachments {
            let is_image = attachment
                .content_type
                .as_deref()
                .is_some_and(|value| value.starts_with("image/"));
            if is_image {
                image_urls.push((attachment.url.clone(), Some(msg.author.id.to_string())));
            } else {
                notes.push(format!("[attachment: {}]", attachment.filename));
            }
        }
        let image_refs: Vec<(&str, Option<&str>)> = image_urls
            .iter()
            .map(|(url, author)| (url.as_str(), author.as_deref()))
            .collect();
        let channel_id = msg.channel_id.to_string();
        let message_id = msg.id.to_string();
        let author_id = msg.author.id.to_string();
        let reply_to = msg
            .referenced_message
            .as_ref()
            .map(|referenced| referenced.id.to_string());
        let Some(event) = map_message_create(MessageCreateInput {
            channel_id: &channel_id,
            message_id: &message_id,
            author_id: &author_id,
            self_id: &self_id,
            content: &msg.content,
            referenced_message_id: reply_to.as_deref(),
            image_urls: &image_refs,
            non_image_notes: &notes,
            discovery,
        }) else {
            return;
        };
        if let Err(error) = self.core.request(said_event_to_wire("event", &event)).await {
            eprintln!("opencrab-discord-gate: event rejected: {error}");
        }
    }
}

async fn collect_channel_facts(
    ctx: &Context,
    msg: &Message,
) -> Result<ChannelFacts, DiscoveryError> {
    let mut channel_type = None;
    let mut guild_id = msg.guild_id.map(|id| id.to_string());
    let mut label = None;

    if let Some(guild) = msg.guild_id.and_then(|id| ctx.cache.guild(id)) {
        if let Some(guild_channel) = guild.channels.get(&msg.channel_id) {
            channel_type = Some(guild_channel.kind);
            if guild_id.is_none() {
                guild_id = Some(guild_channel.guild_id.to_string());
            }
            if !guild_channel.name.is_empty() {
                label = Some(guild_channel.name.clone());
            }
        }
    }

    let type_ready = channel_type.is_some();
    let guild_ready =
        !needs_guild_id(channel_type) || guild_id.as_deref().is_some_and(|v| !v.is_empty());
    if !type_ready || !guild_ready || label.is_none() {
        match ctx.http.get_channel(msg.channel_id).await {
            Ok(Channel::Guild(guild_channel)) => {
                if channel_type.is_none() {
                    channel_type = Some(guild_channel.kind);
                }
                if guild_id.as_deref().is_none_or(str::is_empty) {
                    guild_id = Some(guild_channel.guild_id.to_string());
                }
                if label.is_none() && !guild_channel.name.is_empty() {
                    label = Some(guild_channel.name);
                }
            }
            Ok(Channel::Private(private)) => {
                if channel_type.is_none() {
                    channel_type = Some(ChannelType::Private);
                }
                if label.is_none() {
                    label = Some(private.recipient.name.clone());
                }
            }
            Ok(_) | Err(_) => {
                if channel_type.is_none() {
                    return Err(DiscoveryError::Unresolved);
                }
            }
        }
    }

    let Some(resolved_type) = channel_type else {
        return Err(DiscoveryError::Unresolved);
    };
    if address_kind_from_channel_type(resolved_type).is_none() {
        return Err(DiscoveryError::Unresolved);
    }
    if needs_guild_id(Some(resolved_type)) && guild_id.as_deref().is_none_or(str::is_empty) {
        return Err(DiscoveryError::Unresolved);
    }
    Ok(ChannelFacts {
        channel_type: Some(resolved_type),
        guild_id,
        label,
    })
}

fn needs_guild_id(channel_type: Option<ChannelType>) -> bool {
    matches!(
        channel_type.and_then(address_kind_from_channel_type),
        Some(opencrab_port::AddressKind::Guild)
    )
}

async fn apply_effect(
    http: &Http,
    address: &str,
    kind: &str,
    payload: &Value,
    target: Option<&str>,
    voice: Option<&Arc<VoiceSessionManager>>,
    speaker: Option<&str>,
) -> Result<Option<String>, String> {
    let channel = ChannelId::new(
        address
            .parse::<u64>()
            .map_err(|_| "effect.address must be a Discord snowflake".to_string())?,
    );
    match kind {
        "say" => {
            let text = payload
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "say payload.text is required".to_string())?;
            let sent = channel
                .send_message(http, CreateMessage::new().content(text))
                .await
                .map_err(|error| format!("discord say rejected: {error}"))?;
            if let (Some(voice), Some(speaker)) = (voice, speaker) {
                voice.maybe_speak(address, speaker, text);
            }
            Ok(Some(sent.id.to_string()))
        }
        "react" => {
            let symbol = payload
                .get("symbol")
                .and_then(Value::as_str)
                .ok_or_else(|| "react payload.symbol is required".to_string())?;
            let message_id = target
                .ok_or_else(|| "react target is required".to_string())?
                .parse::<u64>()
                .map_err(|_| "react target must be a Discord snowflake".to_string())?;
            channel
                .create_reaction(
                    http,
                    MessageId::new(message_id),
                    ReactionType::Unicode(symbol.to_string()),
                )
                .await
                .map_err(|error| format!("discord react rejected: {error}"))?;
            Ok(None)
        }
        "ui" => {
            let mode = payload
                .get("mode")
                .and_then(Value::as_str)
                .ok_or_else(|| "ui payload.mode is required".to_string())?;
            match mode {
                "create" => Err("ui renderer is not in this slice".into()),
                "disable" => {
                    let message_id = payload
                        .get("message_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "ui disable message_id is required".to_string())?
                        .parse::<u64>()
                        .map_err(|_| {
                            "ui disable message_id must be a Discord snowflake".to_string()
                        })?;
                    channel
                        .edit_message(
                            http,
                            MessageId::new(message_id),
                            EditMessage::new().components(Vec::new()),
                        )
                        .await
                        .map_err(|error| format!("discord ui disable rejected: {error}"))?;
                    Ok(None)
                }
                other => Err(format!("ui payload.mode is unsupported: {other}")),
            }
        }
        other => Err(format!("undeclared effect kind: {other}")),
    }
}

struct Incoming {
    hello_epoch: oneshot::Sender<u64>,
    effects: mpsc::UnboundedSender<Value>,
}

async fn read_core(
    mut read_half: tokio::net::unix::OwnedReadHalf,
    core: Arc<CoreLink>,
    incoming: Incoming,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    let mut hello_tx = Some(incoming.hello_epoch);
    loop {
        match read_half.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            if pos > MAX_LINE {
                return;
            }
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = &line[..line.len() - 1];
            let Ok(text) = std::str::from_utf8(line) else {
                return;
            };
            let Ok(value) = serde_json::from_str::<Value>(text) else {
                return;
            };
            let Some(object) = value.as_object() else {
                return;
            };
            if let Some(id) = object.get("id").and_then(Value::as_str) {
                if id == "hello-1" {
                    if let Some(tx) = hello_tx.take() {
                        let epoch = value
                            .pointer("/ok/connection_epoch")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let _ = tx.send(epoch);
                    }
                    continue;
                }
                if let Some(tx) = core.pending.lock().await.remove(id) {
                    if object.contains_key("ok") {
                        let _ = tx.send(Resp::Ok(value.get("ok").cloned().unwrap_or(json!({}))));
                    } else {
                        let _ = tx.send(Resp::Err(value.get("err").cloned().unwrap_or(json!({}))));
                    }
                    continue;
                }
            }
            match object.get("m").and_then(Value::as_str) {
                Some("effect") => {
                    let _ = incoming.effects.send(value);
                }
                Some("activity") => {}
                Some("tool") => {
                    let id = object.get("id").and_then(Value::as_str).unwrap_or("");
                    core.send_line(
                        &json!({"id": id, "err": {"code": "unknown_message", "at": "tool"}}),
                    );
                }
                _ => {}
            }
        }
    }
}

async fn effect_worker(
    core: Arc<CoreLink>,
    http: Option<Arc<Http>>,
    voice: Option<Arc<VoiceSessionManager>>,
    owner_agent_id: Option<String>,
    mut rx: mpsc::UnboundedReceiver<Value>,
) {
    while let Some(value) = rx.recv().await {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !core.active.load(Ordering::SeqCst) {
            core.send_line(
                &json!({"id": id, "err": {"code": "instance_not_ready", "at": "effect"}}),
            );
            continue;
        }
        let Some(http) = http.as_ref() else {
            core.send_line(
                &json!({"id": id, "err": {"code": "instance_not_ready", "at": "effect"}}),
            );
            continue;
        };
        let address = value.get("address").and_then(Value::as_str).unwrap_or("");
        let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
        let payload = value.get("payload").cloned().unwrap_or(json!({}));
        let target = value.get("target").and_then(Value::as_str);
        let speaker = payload
            .get("speaker")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| owner_agent_id.clone());
        match apply_effect(
            http,
            address,
            kind,
            &payload,
            target,
            voice.as_ref(),
            speaker.as_deref(),
        )
        .await
        {
            Ok(origin) => {
                let mut ok = serde_json::Map::new();
                ok.insert("delivered".into(), true.into());
                if let Some(origin) = origin {
                    ok.insert("origin".into(), origin.into());
                }
                core.send_line(&json!({"id": id, "ok": ok}));
            }
            Err(error) => {
                core.send_line(
                    &json!({"id": id, "err": {"code": "failed", "at": "effect", "detail": error}}),
                );
            }
        }
    }
}

async fn voice_said_worker(core: Arc<CoreLink>, mut rx: mpsc::UnboundedReceiver<SaidEvent>) {
    while let Some(event) = rx.recv().await {
        if !core.active.load(Ordering::SeqCst) {
            continue;
        }
        if let Err(error) = core.request(said_event_to_wire("event", &event)).await {
            eprintln!("opencrab-discord-gate: voice said rejected: {error}");
        }
    }
}

async fn connect_core(
    socket: &str,
    instance: &GateInstanceId,
    revision: u64,
) -> (
    Arc<CoreLink>,
    oneshot::Receiver<u64>,
    mpsc::UnboundedReceiver<Value>,
) {
    let stream = loop {
        match UnixStream::connect(socket).await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    };
    let (read_half, mut write_half) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if write_half.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = write_half.flush().await;
        }
    });
    let core = CoreLink::new(out_tx);
    let (hello_tx, hello_rx) = oneshot::channel();
    let (effect_tx, effect_rx) = mpsc::unbounded_channel();
    tokio::spawn(read_core(
        read_half,
        core.clone(),
        Incoming {
            hello_epoch: hello_tx,
            effects: effect_tx,
        },
    ));
    core.send_line(&protocol2_hello(instance, revision));
    (core, hello_rx, effect_rx)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let boot = read_boot_config_from_env(|name| std::env::var(name).ok()).unwrap_or_else(|error| {
        panic!("{error}");
    });
    let (core, hello_rx, effect_rx) =
        connect_core(&boot.socket, &boot.instance_id, boot.revision).await;
    let epoch = hello_rx
        .await
        .ok()
        .filter(|epoch| *epoch > 0)
        .unwrap_or_else(|| panic!("core rejected discord hello"));
    if let Some(code) = boot.boot_error {
        tokio::spawn(effect_worker(core.clone(), None, None, None, effect_rx));
        core.send_line(&protocol2_failed("failed-1", epoch, &code));
        tokio::time::sleep(Duration::from_millis(100)).await;
        return;
    }
    let token = boot
        .token
        .clone()
        .unwrap_or_else(|| panic!("OPENCRAB_GATE_TOKEN is required after a successful hello"));
    let http = Http::new(&token);
    if let Err(error) = http.get_current_user().await {
        tokio::spawn(effect_worker(core.clone(), None, None, None, effect_rx));
        core.send_line(&protocol2_failed(
            "failed-1",
            epoch,
            "rest_current_user_failed",
        ));
        eprintln!("opencrab-discord-gate: REST current-user failed");
        let _ = error;
        return;
    }
    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_VOICE_STATES;
    let songbird = songbird::Songbird::serenity_from_config(
        songbird::Config::default().decode_mode(songbird::driver::DecodeMode::Decode),
    );
    let (ready_tx, ready_rx) = oneshot::channel();
    let handler = Handler {
        core: core.clone(),
        self_id: Arc::new(Mutex::new(None)),
        ready: Arc::new(Mutex::new(Some(ready_tx))),
    };
    let mut client = serenity::Client::builder(&token, intents)
        .event_handler(handler)
        .voice_manager_arc(songbird.clone())
        .await
        .unwrap_or_else(|error| panic!("discord client build failed: {error}"));
    let http_for_effects = client.http.clone();
    let (voice_tx, voice_rx) = mpsc::unbounded_channel();
    let voice = try_build_voice_providers(&boot.voice_config).map(|(stt, tts)| {
        VoiceSessionManager::new(
            songbird,
            stt,
            tts,
            boot.voice_config.tts.clone(),
            boot.voice_config.stt.language.clone(),
            voice_tx,
            http_for_effects.clone(),
        )
    });
    tokio::spawn(voice_said_worker(core.clone(), voice_rx));
    tokio::spawn(effect_worker(
        core.clone(),
        Some(http_for_effects),
        voice,
        boot.owner_agent_id.clone(),
        effect_rx,
    ));
    tokio::spawn(async move {
        if let Err(error) = client.start().await {
            eprintln!("opencrab-discord-gate: gateway failed: {error}");
        }
    });
    if ready_rx.await.is_err() {
        core.send_line(&protocol2_failed("failed-1", epoch, "gateway_ready_failed"));
        return;
    }
    core.active.store(true, Ordering::SeqCst);
    core.send_line(&protocol2_ready("ready-1", epoch));
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
