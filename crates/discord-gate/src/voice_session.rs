//! Discord VC 対話セッション。本体 `crates/discord/src/voice_session.rs` の session ロジック。
//!
//! STT 成功は LoopEvent ではなく既存 `said`（`source=discord_voice`）を core へ送る。

use std::sync::Arc;

use anyhow::{Context, Result};
use opencrab_port::{AddressKind, MembershipDiscovery};
use opencrab_voice::audio::SpeechSegmenter;
use opencrab_voice::{SttProvider, TtsProvider};
use songbird::events::context_data::VoiceTick;
use songbird::model::payload::Speaking;
use songbird::{CoreEvent, Event, EventContext, EventHandler as SongbirdEventHandler};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::{clean_for_tts, transcribe_segment_to_said, SaidEvent, VoiceTranscriptTarget};

/// アクティブな VC セッション。
#[derive(Clone)]
struct ActiveSession {
    guild_id: u64,
    text_channel_id: String,
    agent_id: String,
    discovery: MembershipDiscovery,
}

struct ProviderSet {
    stt: Arc<dyn SttProvider>,
    tts: Arc<dyn TtsProvider>,
    tts_cfg: opencrab_voice::TtsConfig,
    stt_language: Option<String>,
}

/// VC セッションの管理と TTS 再生。
pub struct VoiceSessionManager {
    songbird: Arc<songbird::Songbird>,
    providers: std::sync::RwLock<Arc<ProviderSet>>,
    event_tx: mpsc::UnboundedSender<SaidEvent>,
    http: Arc<serenity::http::Http>,
    sessions: dashmap::DashMap<u64, ActiveSession>,
    user_cache: dashmap::DashMap<u64, String>,
    self_user_id: tokio::sync::OnceCell<u64>,
}

impl VoiceSessionManager {
    pub fn new(
        songbird: Arc<songbird::Songbird>,
        stt: Arc<dyn SttProvider>,
        tts: Arc<dyn TtsProvider>,
        tts_cfg: opencrab_voice::TtsConfig,
        stt_language: Option<String>,
        event_tx: mpsc::UnboundedSender<SaidEvent>,
        http: Arc<serenity::http::Http>,
    ) -> Arc<Self> {
        Arc::new(Self {
            songbird,
            providers: std::sync::RwLock::new(Arc::new(ProviderSet {
                stt,
                tts,
                tts_cfg,
                stt_language,
            })),
            event_tx,
            http,
            sessions: dashmap::DashMap::new(),
            user_cache: dashmap::DashMap::new(),
            self_user_id: tokio::sync::OnceCell::new(),
        })
    }

    fn providers(&self) -> Arc<ProviderSet> {
        self.providers.read().unwrap().clone()
    }

    pub async fn join(
        self: &Arc<Self>,
        guild_id: u64,
        vc_channel_id: u64,
        text_channel_id: Option<String>,
        agent_id: &str,
    ) -> Result<()> {
        let call = self
            .songbird
            .join(
                songbird::id::GuildId(std::num::NonZeroU64::new(guild_id).context("guild_id=0")?),
                songbird::id::ChannelId(
                    std::num::NonZeroU64::new(vc_channel_id).context("channel_id=0")?,
                ),
            )
            .await
            .context("VC への参加に失敗しました（Bot に Connect 権限はありますか？）")?;

        let text_channel_id = text_channel_id.unwrap_or_else(|| vc_channel_id.to_string());
        let session = ActiveSession {
            guild_id,
            text_channel_id: text_channel_id.clone(),
            agent_id: agent_id.to_string(),
            discovery: MembershipDiscovery {
                address_kind: AddressKind::Guild,
                guild_id: Some(guild_id.to_string()),
                label: None,
            },
        };
        self.sessions.insert(guild_id, session.clone());

        let receiver = VoiceReceiver {
            mgr: self.clone(),
            session,
            ssrc_users: Arc::new(dashmap::DashMap::new()),
            segmenters: Arc::new(dashmap::DashMap::new()),
        };
        {
            let mut call = call.lock().await;
            call.remove_all_global_events();
            call.add_global_event(CoreEvent::SpeakingStateUpdate.into(), receiver.clone());
            call.add_global_event(CoreEvent::VoiceTick.into(), receiver.clone());
            call.add_global_event(CoreEvent::ClientDisconnect.into(), receiver);
        }
        info!(guild_id, vc_channel_id, agent_id, "joined voice channel");
        Ok(())
    }

    pub async fn leave(&self, guild_id: u64) -> Result<()> {
        self.sessions.remove(&guild_id);
        let gid = songbird::id::GuildId(std::num::NonZeroU64::new(guild_id).context("guild_id=0")?);
        self.songbird
            .remove(gid)
            .await
            .context("VC からの退出に失敗しました")?;
        info!(guild_id, "left voice channel");
        Ok(())
    }

    /// VC 無 / agent 不一致は no-op（本体どおり）。
    pub fn maybe_speak(self: &Arc<Self>, channel_id_str: &str, agent_id: &str, text: &str) {
        let Some(sess) = self
            .sessions
            .iter()
            .find(|s| s.text_channel_id == channel_id_str && s.agent_id == agent_id)
            .map(|s| s.clone())
        else {
            return;
        };
        let mgr = self.clone();
        let text = clean_for_tts(text);
        if text.is_empty() {
            return;
        }
        let agent_id = agent_id.to_string();
        tokio::spawn(async move {
            if let Err(e) = mgr.speak(sess.guild_id, &agent_id, &text).await {
                warn!(guild_id = sess.guild_id, error = %e, "TTS playback failed");
            }
        });
    }

    async fn speak(&self, guild_id: u64, agent_id: &str, text: &str) -> Result<()> {
        let providers = self.providers();
        let voice = providers.tts_cfg.voice_for_agent(agent_id).to_string();
        let wav = providers
            .tts
            .synthesize(text, &voice)
            .await
            .context("TTS synthesis failed")?;
        let gid = songbird::id::GuildId(std::num::NonZeroU64::new(guild_id).context("guild_id=0")?);
        let call = self
            .songbird
            .get(gid)
            .context("VC セッションがありません")?;
        let mut call = call.lock().await;
        let _handle = call.enqueue_input(songbird::input::Input::from(wav)).await;
        Ok(())
    }

    async fn resolve_user_name(&self, user_id: u64) -> String {
        if let Some(hit) = self.user_cache.get(&user_id) {
            return hit.clone();
        }
        let resolved = match serenity::model::id::UserId::new(user_id)
            .to_user(&self.http)
            .await
        {
            Ok(u) => u.global_name.clone().unwrap_or_else(|| u.name.clone()),
            Err(e) => {
                warn!(user_id, error = %e, "failed to resolve VC user; using id");
                user_id.to_string()
            }
        };
        self.user_cache.insert(user_id, resolved.clone());
        resolved
    }

    async fn self_user_id(&self) -> Option<u64> {
        self.self_user_id
            .get_or_try_init(|| async {
                self.http
                    .get_current_user()
                    .await
                    .map(|u| u.id.get())
                    .inspect_err(|e| {
                        warn!(error = %e, "failed to resolve own Discord user id for VC");
                    })
            })
            .await
            .ok()
            .copied()
    }

    async fn process_segment(
        &self,
        session: &ActiveSession,
        user_id: u64,
        pcm_48k_stereo: Vec<i16>,
    ) {
        let user_name = self.resolve_user_name(user_id).await;
        let providers = self.providers();
        let author_id = user_id.to_string();
        let Some(event) = transcribe_segment_to_said(
            providers.stt.as_ref(),
            providers.stt_language.as_deref(),
            &pcm_48k_stereo,
            VoiceTranscriptTarget {
                text_channel_id: &session.text_channel_id,
                author_id: &author_id,
                author_display: Some(&user_name),
                discovery: session.discovery.clone(),
                self_user_id: self.self_user_id().await,
                speaker_user_id: user_id,
            },
        )
        .await
        else {
            return;
        };
        info!(user_id, user = %user_name, text = %event.content_text, "voice transcribed");
        let _ = self.event_tx.send(event);
    }

    pub fn has_session_for(&self, text_channel_id: &str, agent_id: &str) -> bool {
        self.sessions
            .iter()
            .any(|s| s.text_channel_id == text_channel_id && s.agent_id == agent_id)
    }
}

impl opencrab_voice::VoiceRuntime for VoiceSessionManager {
    fn apply_settings(
        &self,
        stt: Arc<dyn SttProvider>,
        tts: Arc<dyn TtsProvider>,
        tts_cfg: opencrab_voice::TtsConfig,
        stt_language: Option<String>,
    ) {
        *self.providers.write().unwrap() = Arc::new(ProviderSet {
            stt,
            tts,
            tts_cfg,
            stt_language,
        });
        info!("voice providers hot-swapped");
    }
}

#[derive(Clone)]
struct VoiceReceiver {
    mgr: Arc<VoiceSessionManager>,
    session: ActiveSession,
    ssrc_users: Arc<dashmap::DashMap<u32, u64>>,
    segmenters: Arc<dashmap::DashMap<u32, std::sync::Mutex<SpeechSegmenter>>>,
}

impl VoiceReceiver {
    fn on_speaking_update(&self, s: &Speaking) {
        if let Some(uid) = s.user_id {
            self.ssrc_users.insert(s.ssrc, uid.0);
        }
    }

    fn on_client_disconnect(&self, user_id: u64) {
        let ssrcs: Vec<u32> = self
            .ssrc_users
            .iter()
            .filter(|e| *e.value() == user_id)
            .map(|e| *e.key())
            .collect();
        for ssrc in ssrcs {
            if let Some(seg) = self.segmenters.get(&ssrc) {
                let finalized = seg.lock().unwrap().flush();
                if let Some(segment) = finalized {
                    self.dispatch(ssrc, segment.pcm_48k_stereo);
                }
            }
        }
    }

    fn on_voice_tick(&self, tick: &VoiceTick) {
        for (ssrc, data) in &tick.speaking {
            let Some(pcm) = &data.decoded_voice else {
                continue;
            };
            let seg = self
                .segmenters
                .entry(*ssrc)
                .or_insert_with(|| std::sync::Mutex::new(SpeechSegmenter::new()));
            let finalized = seg.lock().unwrap().push_frame(pcm);
            if let Some(segment) = finalized {
                self.dispatch(*ssrc, segment.pcm_48k_stereo);
            }
        }
        for ssrc in &tick.silent {
            if let Some(seg) = self.segmenters.get(ssrc) {
                let finalized = seg.lock().unwrap().push_silence();
                if let Some(segment) = finalized {
                    self.dispatch(*ssrc, segment.pcm_48k_stereo);
                }
            }
        }
    }

    fn dispatch(&self, ssrc: u32, pcm: Vec<i16>) {
        let Some(user_id) = self.ssrc_users.get(&ssrc).map(|u| *u) else {
            debug!(ssrc, "segment from unmapped SSRC; dropping");
            return;
        };
        let mgr = self.mgr.clone();
        let session = self.session.clone();
        tokio::spawn(async move {
            mgr.process_segment(&session, user_id, pcm).await;
        });
    }
}

#[async_trait::async_trait]
impl SongbirdEventHandler for VoiceReceiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(s) => self.on_speaking_update(s),
            EventContext::VoiceTick(tick) => self.on_voice_tick(tick),
            EventContext::ClientDisconnect(d) => self.on_client_disconnect(d.user_id.0),
            _ => {}
        }
        None
    }
}
