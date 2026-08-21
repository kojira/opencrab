//! Discord VC（ボイスチャンネル）対話セッション。
//!
//! # 話者分離について
//!
//! Discord の音声はユーザーごとに独立した RTP ストリーム（SSRC）で届き、
//! `SpeakingStateUpdate` イベントで SSRC ↔ user_id の対応が通知される。
//! つまり「誰の声か」は音声処理による推定ではなく **プロトコルレベルで確定**
//! しており、誤認しない。本モジュールはユーザー（SSRC）ごとに独立した
//! `SpeechSegmenter` を持ち、発話の切れ目（無音 800ms / 最長 15s）で
//! セグメントを確定して STT に渡す。
//!
//! # パイプライン
//!
//! 受信: VoiceTick(20ms) → SSRC 別に蓄積 → 無音で確定 → 48k stereo→16k mono
//! → WAV → STT → `LoopEvent::IncomingMessage`（source=discord_voice、送信者は
//! 実際の Discord ユーザー）としてメッセージループへ注入 → 以降は通常の
//! テキスト会話と同じ経路（whitelist / セッションロック / 履歴）で処理される。
//!
//! 送信: エージェント返信テキスト → TTS（エージェント別の声）→ VC 再生。

use std::sync::Arc;

use anyhow::{Context, Result};
use songbird::events::context_data::VoiceTick;
use songbird::model::payload::Speaking;
use songbird::{CoreEvent, Event, EventContext, EventHandler as SongbirdEventHandler};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use opencrab_voice::audio::{downmix_48k_stereo_to_16k_mono, pcm_to_wav, rms, SpeechSegmenter};
use opencrab_voice::{SttProvider, TtsProvider};

use crate::message_loop::LoopEvent;

/// STT に渡す最低 RMS（16bit PCM）。これ未満は環境ノイズとして捨てる。
const MIN_SEGMENT_RMS: f64 = 250.0;

/// TTS に渡すテキストの最大文字数（長広舌の読み上げ防止）。
const MAX_TTS_CHARS: usize = 400;

/// その話者の声を文字起こしするか。
///
/// 外すのは**自分自身の声だけ**。自分の TTS を自分で拾うと「聞く → 返す → 読み上げる →
/// また聞く」の無限ループになる。他エージェント（bot）の声は拾う — それが会話であり、
/// 以前は bot を一律に外していたため VC で他エージェントの発言が一切文字起こしされて
/// いなかった。
///
/// 自分の id がまだ取れていない（`None`）ときは**拾わない**。取れない状態で拾うと
/// 上のループを止める手段が無くなる（id はリトライで取り直せるが、ループは止まらない）。
fn should_transcribe(self_user_id: Option<u64>, speaker_user_id: u64) -> bool {
    match self_user_id {
        Some(self_id) => self_id != speaker_user_id,
        None => false,
    }
}

/// アクティブな VC セッション。
#[derive(Clone)]
struct ActiveSession {
    guild_id: u64,
    /// STT 結果の注入先（および返信の読み上げ対象）テキストチャンネル。
    text_channel_id: String,
    /// この VC に参加しているエージェント。
    agent_id: String,
}

/// STT/TTS プロバイダと設定の束。ダッシュボードからの設定変更で
/// まとめて差し替える（`VoiceRuntime::apply_settings`）。
struct ProviderSet {
    stt: Arc<dyn SttProvider>,
    tts: Arc<dyn TtsProvider>,
    tts_cfg: opencrab_voice::TtsConfig,
    stt_language: Option<String>,
}

/// VC セッションの管理と TTS 再生。
pub struct VoiceSessionManager {
    songbird: Arc<songbird::Songbird>,
    /// 現在のプロバイダ束。読みは短命ロック → Arc clone。
    /// await をまたいで guard を保持しないこと。
    providers: std::sync::RwLock<Arc<ProviderSet>>,
    event_tx: mpsc::UnboundedSender<LoopEvent>,
    http: Arc<serenity::http::Http>,
    /// guild_id → セッション。1 ギルドにつき同時 1 VC。
    sessions: dashmap::DashMap<u64, ActiveSession>,
    /// user_id → 表示名キャッシュ。
    user_cache: dashmap::DashMap<u64, String>,
    /// 自分自身の Discord user id（`GET /users/@me`）。文字起こしから外す唯一の相手。
    /// 一度取れれば以後は使い回す。
    self_user_id: tokio::sync::OnceCell<u64>,
}

impl VoiceSessionManager {
    pub fn new(
        songbird: Arc<songbird::Songbird>,
        stt: Arc<dyn SttProvider>,
        tts: Arc<dyn TtsProvider>,
        tts_cfg: opencrab_voice::TtsConfig,
        stt_language: Option<String>,
        event_tx: mpsc::UnboundedSender<LoopEvent>,
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

    /// 現在のプロバイダ束のスナップショット。
    fn providers(&self) -> Arc<ProviderSet> {
        self.providers.read().unwrap().clone()
    }

    /// VC に参加して受信を開始する。
    ///
    /// `text_channel_id` は STT 結果の注入先。省略時は VC 自体のチャンネル ID
    /// （VC のテキストチャット）。このチャンネルがエージェントの whitelist に
    /// 入っている必要がある（通常のテキスト会話と同じゲート）。
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

        let session = ActiveSession {
            guild_id,
            text_channel_id: text_channel_id.unwrap_or_else(|| vc_channel_id.to_string()),
            agent_id: agent_id.to_string(),
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
            // 再 join（VC 移動・注入先変更）時、songbird は同一ギルドの Call を
            // 再利用するため、古い VoiceReceiver が残ると STT が多重実行され
            // 旧チャンネルへ注入され続ける。必ず張り替える。
            call.remove_all_global_events();
            call.add_global_event(CoreEvent::SpeakingStateUpdate.into(), receiver.clone());
            call.add_global_event(CoreEvent::VoiceTick.into(), receiver.clone());
            call.add_global_event(CoreEvent::ClientDisconnect.into(), receiver);
        }
        info!(guild_id, vc_channel_id, agent_id, "joined voice channel");
        Ok(())
    }

    /// VC から退出する。
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

    /// 対象テキストチャンネル宛の返信を、対応する VC で読み上げる（非同期・失敗は警告のみ）。
    ///
    /// VC セッションが無い / エージェントが違う場合は何もしない。
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

    /// TTS 合成して VC で再生する。
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
        // play_input は即時ミックス（同時再生で音が重なる）。連続する返信が
        // 重ならないよう builtin-queue で直列再生する。
        let _handle = call.enqueue_input(songbird::input::Input::from(wav)).await;
        Ok(())
    }

    /// ユーザーの表示名を解決する（キャッシュ付き）。
    ///
    /// 話者が bot かどうかは**見ない**。他エージェントの声を拾うのが会話であり、
    /// 外すのは自分自身の声だけ（[`should_transcribe`]）。
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

    /// 自分自身の Discord user id。取得できるまで毎回問い合わせ、取れたら以後は使い回す。
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

    /// 確定した発話セグメントを STT → メッセージループへ注入する。
    async fn process_segment(
        &self,
        session: &ActiveSession,
        user_id: u64,
        pcm_48k_stereo: Vec<i16>,
    ) {
        if !should_transcribe(self.self_user_id().await, user_id) {
            return;
        }
        let user_name = self.resolve_user_name(user_id).await;
        let mono = downmix_48k_stereo_to_16k_mono(&pcm_48k_stereo);
        if rms(&mono) < MIN_SEGMENT_RMS {
            debug!(user_id, "segment below RMS threshold; skipping STT");
            return;
        }
        let wav = pcm_to_wav(&mono, 16_000, 1);
        let providers = self.providers();
        let text = match providers
            .stt
            .transcribe(&wav, providers.stt_language.as_deref())
            .await
        {
            Ok(t) => t,
            Err(e) => {
                warn!(user_id, error = %e, "STT failed");
                return;
            }
        };
        if text.trim().is_empty() {
            return;
        }
        info!(user_id, user = %user_name, text = %text, "voice transcribed");

        let mut msg = opencrab_gateway::IncomingMessage::new(
            opencrab_gateway::MessageSource::Discord {
                guild_id: session.guild_id.to_string(),
                channel_id: session.text_channel_id.clone(),
            },
            opencrab_gateway::MessageContent::text(&text),
            opencrab_gateway::Sender {
                id: user_id.to_string(),
                name: user_name,
                avatar_url: None,
            },
        );
        msg.metadata.insert(
            "source".to_string(),
            serde_json::Value::String("discord_voice".to_string()),
        );
        let _ = self.event_tx.send(LoopEvent::IncomingMessage(msg));
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

/// songbird のイベントハンドラ（1 VC につき 1 つ）。
///
/// SSRC ごとに `SpeechSegmenter` を持ち、VoiceTick(20ms) を振り分ける。
#[derive(Clone)]
struct VoiceReceiver {
    mgr: Arc<VoiceSessionManager>,
    session: ActiveSession,
    /// SSRC → Discord user_id（SpeakingStateUpdate で更新）。
    ssrc_users: Arc<dashmap::DashMap<u32, u64>>,
    /// SSRC → セグメンタ。std Mutex で保護（await をまたがない）。
    segmenters: Arc<dashmap::DashMap<u32, std::sync::Mutex<SpeechSegmenter>>>,
}

impl VoiceReceiver {
    fn on_speaking_update(&self, s: &Speaking) {
        if let Some(uid) = s.user_id {
            self.ssrc_users.insert(s.ssrc, uid.0);
        }
    }

    /// ユーザーの VC 切断時、溜まっている発話を強制確定する。
    /// 切断された SSRC は以後 speaking にも silent にも現れなくなるため、
    /// これが無いと「言い残して即退出」した発話が失われる。
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
        // 発話中の SSRC: フレームを蓄積
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
        // 無音の SSRC: 無音カウントを進め、閾値で確定
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

/// TTS 用にテキストを整形する。
///
/// コードブロック・URL・メンション・Markdown 記号は読み上げに不向きなので
/// 置換/除去し、長すぎる場合は切り詰める。
pub fn clean_for_tts(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code_block = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if !in_code_block {
                out.push_str("（コードは省略） ");
            }
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        out.push_str(line);
        out.push(' ');
    }
    // URL → 「リンク」
    let mut cleaned = String::with_capacity(out.len());
    for word in out.split(' ') {
        if word.starts_with("http://") || word.starts_with("https://") {
            cleaned.push_str("リンク");
        } else {
            cleaned.push_str(word);
        }
        cleaned.push(' ');
    }
    // メンション <@123> / <#123> / <@&123> と Markdown 記号を除去
    let mut result = String::with_capacity(cleaned.len());
    let mut chars = cleaned.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            // <...> をスキップ（メンション・カスタム絵文字）
            let mut consumed = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == '>' {
                    closed = true;
                    break;
                }
                consumed.push(c2);
                if consumed.len() > 64 {
                    break;
                }
            }
            if !closed {
                result.push('<');
                result.push_str(&consumed);
            }
            continue;
        }
        if matches!(c, '*' | '_' | '`' | '#' | '~' | '|') {
            continue;
        }
        result.push(c);
    }
    let trimmed = result.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.chars().count() > MAX_TTS_CHARS {
        let cut: String = trimmed.chars().take(MAX_TTS_CHARS).collect();
        format!("{cut}、以下省略")
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_for_tts_strips_code_and_urls() {
        let input = "結果です。\n```rust\nfn main() {}\n```\n詳細は https://example.com/x を見てください **重要**";
        let out = clean_for_tts(input);
        assert!(!out.contains("fn main"), "{out}");
        assert!(out.contains("（コードは省略）"));
        assert!(out.contains("リンク"));
        assert!(!out.contains("**"));
        assert!(!out.contains("https://"));
    }

    #[test]
    fn test_clean_for_tts_strips_mentions() {
        let out = clean_for_tts("<@123456> さん、<#987> を見て");
        assert!(!out.contains('<') && !out.contains('>'), "{out}");
        assert!(out.contains("さん、"));
    }

    #[test]
    fn test_clean_for_tts_caps_length() {
        let long = "あ".repeat(1000);
        let out = clean_for_tts(&long);
        assert!(out.chars().count() <= MAX_TTS_CHARS + 10);
        assert!(out.ends_with("以下省略"));
    }

    #[test]
    fn test_clean_for_tts_plain_text_passthrough() {
        assert_eq!(
            clean_for_tts("こんにちは。元気です。"),
            "こんにちは。元気です。"
        );
    }

    /// **VC で外すのは自分の声だけ。他エージェントの声は拾う。**
    ///
    /// 以前は「話者が bot なら文字起こししない」だったため、同じ VC に居る他エージェント
    /// の発言が一切テキスト化されず、VC でエージェント同士が会話できなかった（#317）。
    #[test]
    fn only_my_own_voice_is_excluded_from_transcription() {
        assert!(
            !should_transcribe(Some(100), 100),
            "自分の声を文字起こししている（自分の TTS を自分で拾う無限ループ）"
        );
        assert!(
            should_transcribe(Some(100), 200),
            "他エージェント／他ユーザーの声を捨てている（VC で会話が成立しない）"
        );
    }

    /// 自分の id が取れていない間は拾わない（ループを止める手段が無いため）。
    #[test]
    fn unknown_self_id_means_no_transcription() {
        assert!(!should_transcribe(None, 200));
    }
}
