//! 音声入出力プロバイダ層（STT / TTS）。
//!
//! Discord VC 対話などの上位層から使う。プロバイダは trait で抽象化し、
//! 設定でエンドポイント・モデル・話者を差し替えられる。
//!
//! - STT: OpenAI 互換 `/v1/audio/transcriptions`（OpenAI 本家のほか、
//!   faster-whisper-server / LocalAI 等のローカル互換サーバも同じ形）
//! - TTS: VOICEVOX（ローカル・日本語・話者 ID で声を分けられる）と
//!   OpenAI `/v1/audio/speech`

pub mod audio;
pub mod providers;

use anyhow::Result;
use async_trait::async_trait;

/// 文字起こしプロバイダ。入力は WAV バイト列（16kHz mono 推奨）。
#[async_trait]
pub trait SttProvider: Send + Sync {
    fn name(&self) -> &str;
    /// WAV バイト列を文字起こしする。`language` は BCP-47 ヒント（例: "ja"）。
    async fn transcribe(&self, wav: &[u8], language: Option<&str>) -> Result<String>;
}

/// 音声合成プロバイダ。出力は WAV バイト列。
#[async_trait]
pub trait TtsProvider: Send + Sync {
    fn name(&self) -> &str;
    /// `voice` はプロバイダ固有の話者指定
    /// （VOICEVOX: スタイル ID の数字文字列 / OpenAI: "alloy" 等）。
    async fn synthesize(&self, text: &str, voice: &str) -> Result<Vec<u8>>;
}

/// 稼働中の音声ランタイム（VC セッション管理側）が実装するトレイト。
///
/// ダッシュボードのプロバイダー設定変更を再起動なしで反映するための
/// 差し替え口。サーバ本体は discord クレートに依存しないため、
/// このトレイト経由で疎結合に更新を届ける。
pub trait VoiceRuntime: Send + Sync {
    /// STT/TTS プロバイダと設定を差し替える。進行中の発話処理は
    /// 古いプロバイダで完走してよい（非破壊的スワップ）。
    fn apply_settings(
        &self,
        stt: std::sync::Arc<dyn SttProvider>,
        tts: std::sync::Arc<dyn TtsProvider>,
        tts_cfg: TtsConfig,
        stt_language: Option<String>,
    );
}

/// STT/TTS の実行設定（config.toml の [voice] から構築される）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct VoiceConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub stt: SttConfig,
    #[serde(default)]
    pub tts: TtsConfig,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SttConfig {
    /// "openai"（互換サーバ含む）
    #[serde(default = "default_stt_provider")]
    pub provider: String,
    /// 省略時 https://api.openai.com/v1 。ローカル互換サーバに差し替え可。
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default = "default_stt_model")]
    pub model: String,
    /// API キーを読む環境変数名（キーそのものは TOML に書かない）。
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    /// 文字起こし言語ヒント（例: "ja"）。
    #[serde(default)]
    pub language: Option<String>,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            provider: default_stt_provider(),
            base_url: None,
            model: default_stt_model(),
            api_key_env: default_api_key_env(),
            language: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TtsConfig {
    /// "voicevox" | "openai"
    #[serde(default = "default_tts_provider")]
    pub provider: String,
    /// VOICEVOX: 省略時 http://localhost:50021 / OpenAI: 省略時 api.openai.com
    #[serde(default)]
    pub base_url: Option<String>,
    /// OpenAI TTS のモデル（VOICEVOX では未使用）。
    #[serde(default = "default_tts_model")]
    pub model: String,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    /// 既定の話者（VOICEVOX: スタイル ID / OpenAI: voice 名）。
    #[serde(default = "default_tts_voice")]
    pub default_voice: String,
    /// agent_id → 話者の対応。エージェントごとに声を分ける。
    #[serde(default)]
    pub agent_voices: std::collections::HashMap<String, String>,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            provider: default_tts_provider(),
            base_url: None,
            model: default_tts_model(),
            api_key_env: default_api_key_env(),
            default_voice: default_tts_voice(),
            agent_voices: Default::default(),
        }
    }
}

impl TtsConfig {
    /// エージェントに割り当てる話者を解決する。
    pub fn voice_for_agent(&self, agent_id: &str) -> &str {
        self.agent_voices
            .get(agent_id)
            .map(|s| s.as_str())
            .unwrap_or(&self.default_voice)
    }
}

fn default_stt_provider() -> String {
    "openai".to_string()
}
fn default_stt_model() -> String {
    "whisper-1".to_string()
}
fn default_api_key_env() -> String {
    "OPENAI_API_KEY".to_string()
}
fn default_tts_provider() -> String {
    "voicevox".to_string()
}
fn default_tts_model() -> String {
    "gpt-4o-mini-tts".to_string()
}
fn default_tts_voice() -> String {
    // VOICEVOX: 3 = ずんだもん（ノーマル）。導入直後でも喋れる無難な既定。
    "3".to_string()
}

/// 設定から STT プロバイダを構築する。
pub fn build_stt(cfg: &SttConfig) -> Result<std::sync::Arc<dyn SttProvider>> {
    match cfg.provider.as_str() {
        "openai" => Ok(std::sync::Arc::new(
            providers::openai_stt::OpenAiSttProvider::new(
                cfg.base_url.clone(),
                cfg.model.clone(),
                std::env::var(&cfg.api_key_env).unwrap_or_default(),
            ),
        )),
        other => anyhow::bail!("unknown STT provider: {other}"),
    }
}

/// 設定から TTS プロバイダを構築する。
pub fn build_tts(cfg: &TtsConfig) -> Result<std::sync::Arc<dyn TtsProvider>> {
    match cfg.provider.as_str() {
        "voicevox" => Ok(std::sync::Arc::new(
            providers::voicevox::VoicevoxProvider::new(cfg.base_url.clone()),
        )),
        "openai" => Ok(std::sync::Arc::new(
            providers::openai_tts::OpenAiTtsProvider::new(
                cfg.base_url.clone(),
                cfg.model.clone(),
                std::env::var(&cfg.api_key_env).unwrap_or_default(),
            ),
        )),
        other => anyhow::bail!("unknown TTS provider: {other}"),
    }
}
