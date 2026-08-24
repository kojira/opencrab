//! VoiceTick → セグメント確定 → 48k stereo→16k mono → STT → said。
//!
//! 定数と処理順は本体 `voice_session.rs` / `audio.rs` と同じ。

use std::collections::HashMap;

use opencrab_port::MembershipDiscovery;
use opencrab_voice::audio::{downmix_48k_stereo_to_16k_mono, pcm_to_wav, rms, SpeechSegmenter};
use opencrab_voice::SttProvider;

use crate::{map_voice_transcript, SaidEvent};

/// STT に渡す最低 RMS（16bit PCM）。これ未満は環境ノイズとして捨てる。
pub const MIN_SEGMENT_RMS: f64 = 250.0;

/// TTS に渡すテキストの最大文字数（長広舌の読み上げ防止）。
const MAX_TTS_CHARS: usize = 400;

/// テストと VoiceReceiver が共有する tick 入力（songbird VoiceTick の中身）。
pub struct VoiceTickInput<'a> {
    pub speaking: Vec<(u32, &'a [i16])>,
    pub silent: Vec<u32>,
}

/// その話者の声を文字起こしするか。外すのは自分自身の声だけ。
pub fn should_transcribe(self_user_id: Option<u64>, speaker_user_id: u64) -> bool {
    match self_user_id {
        Some(self_id) => self_id != speaker_user_id,
        None => false,
    }
}

/// VoiceTick(20ms) を SSRC 別に振り、無音 800ms / 最大 15s で確定した PCM を返す。
pub fn apply_voice_tick(
    segmenters: &mut HashMap<u32, SpeechSegmenter>,
    tick: VoiceTickInput<'_>,
) -> Vec<(u32, Vec<i16>)> {
    let mut out = Vec::new();
    for (ssrc, pcm) in tick.speaking {
        let seg = segmenters.entry(ssrc).or_default();
        if let Some(segment) = seg.push_frame(pcm) {
            out.push((ssrc, segment.pcm_48k_stereo));
        }
    }
    for ssrc in tick.silent {
        if let Some(seg) = segmenters.get_mut(&ssrc) {
            if let Some(segment) = seg.push_silence() {
                out.push((ssrc, segment.pcm_48k_stereo));
            }
        }
    }
    out
}

/// STT 注入先と話者。引数を 1 つの値にまとめる。
pub struct VoiceTranscriptTarget<'a> {
    pub text_channel_id: &'a str,
    pub author_id: &'a str,
    pub author_display: Option<&'a str>,
    pub discovery: MembershipDiscovery,
    pub self_user_id: Option<u64>,
    pub speaker_user_id: u64,
}

/// 確定セグメントを 48k→16k WAV → STT → said（`source=discord_voice`）。
pub async fn transcribe_segment_to_said(
    stt: &dyn SttProvider,
    language: Option<&str>,
    pcm_48k_stereo: &[i16],
    target: VoiceTranscriptTarget<'_>,
) -> Option<SaidEvent> {
    if !should_transcribe(target.self_user_id, target.speaker_user_id) {
        return None;
    }
    let mono = downmix_48k_stereo_to_16k_mono(pcm_48k_stereo);
    if rms(&mono) < MIN_SEGMENT_RMS {
        return None;
    }
    let wav = pcm_to_wav(&mono, 16_000, 1);
    let text = stt.transcribe(&wav, language).await.ok()?;
    map_voice_transcript(
        target.text_channel_id,
        target.author_id,
        target.author_display,
        &text,
        target.discovery,
    )
}

/// TTS 用にテキストを整形する（本体 `clean_for_tts` 逐語）。
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
    let mut cleaned = String::with_capacity(out.len());
    for word in out.split(' ') {
        if word.starts_with("http://") || word.starts_with("https://") {
            cleaned.push_str("リンク");
        } else {
            cleaned.push_str(word);
        }
        cleaned.push(' ');
    }
    let mut result = String::with_capacity(cleaned.len());
    let mut chars = cleaned.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
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
    use opencrab_port::AddressKind;
    use opencrab_voice::providers::openai_stt::OpenAiSttProvider;
    use opencrab_voice::providers::openai_tts::OpenAiTtsProvider;
    use opencrab_voice::TtsProvider;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn discovery() -> MembershipDiscovery {
        MembershipDiscovery {
            address_kind: AddressKind::Guild,
            guild_id: Some("1".into()),
            label: Some("general".into()),
        }
    }

    /// slice 1 の `spawn_http_mock` と同じ形。voice crate の test_util は crate-private。
    async fn spawn_http_mock(
        status_line: &'static str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap = captured.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let body = body.clone();
                let cap = cap.clone();
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = sock.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        req.extend_from_slice(&buf[..n]);
                        if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&req[..pos]);
                            let clen = headers
                                .lines()
                                .find_map(|l| {
                                    let (k, v) = l.split_once(':')?;
                                    k.eq_ignore_ascii_case("content-length")
                                        .then(|| v.trim().parse::<usize>().ok())?
                                })
                                .unwrap_or(0);
                            if req.len() >= pos + 4 + clen {
                                break;
                            }
                        }
                    }
                    *cap.lock().unwrap() = req;
                    let resp = format!(
                        "HTTP/1.1 {status_line}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), captured)
    }

    fn loud_frame() -> Vec<i16> {
        vec![3000i16; 1920]
    }

    #[test]
    fn only_my_own_voice_is_excluded_from_transcription() {
        assert!(!should_transcribe(Some(100), 100));
        assert!(should_transcribe(Some(100), 200));
    }

    #[test]
    fn unknown_self_id_means_no_transcription() {
        assert!(!should_transcribe(None, 200));
    }

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

    #[test]
    fn tick_finalizes_after_800ms_silence_and_downmixes_48k_to_16k() {
        let mut segmenters = HashMap::new();
        let frame = loud_frame();
        for _ in 0..50 {
            let got = apply_voice_tick(
                &mut segmenters,
                VoiceTickInput {
                    speaking: vec![(7, &frame)],
                    silent: vec![],
                },
            );
            assert!(got.is_empty());
        }
        for _ in 0..39 {
            let got = apply_voice_tick(
                &mut segmenters,
                VoiceTickInput {
                    speaking: vec![],
                    silent: vec![7],
                },
            );
            assert!(got.is_empty());
        }
        let got = apply_voice_tick(
            &mut segmenters,
            VoiceTickInput {
                speaking: vec![],
                silent: vec![7],
            },
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 7);
        assert_eq!(got[0].1.len(), 1920 * 50);
        let mono = downmix_48k_stereo_to_16k_mono(&got[0].1);
        assert_eq!(mono.len(), (1920 * 50) / 6);
    }

    #[test]
    fn tick_force_finalizes_at_15s() {
        let mut segmenters = HashMap::new();
        segmenters
            .entry(1)
            .or_insert_with(SpeechSegmenter::new)
            .max_frames = 50;
        let frame = loud_frame();
        let mut got = None;
        for i in 0..60 {
            let out = apply_voice_tick(
                &mut segmenters,
                VoiceTickInput {
                    speaking: vec![(1, &frame)],
                    silent: vec![],
                },
            );
            if let Some(item) = out.into_iter().next() {
                got = Some((i, item));
                break;
            }
        }
        let (i, (ssrc, pcm)) = got.expect("must force-finalize at max_frames");
        assert_eq!(i, 49);
        assert_eq!(ssrc, 1);
        assert_eq!(pcm.len(), 1920 * 50);
    }

    #[tokio::test]
    async fn tick_pipeline_stt_mock_emits_said_with_discord_voice_metadata() {
        let (url, captured) = spawn_http_mock(
            "200 OK",
            "application/json",
            r#"{"text":" 声です "}"#.as_bytes().to_vec(),
        )
        .await;
        let stt = OpenAiSttProvider::new(Some(url), "whisper-1".into(), "sk-test".into());
        let pcm = vec![3000i16; 1920 * 50];
        let event = transcribe_segment_to_said(
            &stt,
            Some("ja"),
            &pcm,
            VoiceTranscriptTarget {
                text_channel_id: "10",
                author_id: "200",
                author_display: Some("bob"),
                discovery: discovery(),
                self_user_id: Some(100),
                speaker_user_id: 200,
            },
        )
        .await
        .expect("STT said");
        assert_eq!(event.content_text, "声です");
        assert_eq!(event.address, "10");
        assert_eq!(event.author_id, "200");
        assert_eq!(event.metadata["source"], "discord_voice");
        let req = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(req.contains("POST /audio/transcriptions"), "{req}");
        assert!(
            transcribe_segment_to_said(
                &stt,
                None,
                &pcm,
                VoiceTranscriptTarget {
                    text_channel_id: "10",
                    author_id: "100",
                    author_display: None,
                    discovery: discovery(),
                    self_user_id: Some(100),
                    speaker_user_id: 100,
                },
            )
            .await
            .is_none(),
            "own voice must be excluded"
        );
    }

    #[tokio::test]
    async fn say_tts_mock_synthesizes_cleaned_text() {
        let (url, captured) =
            spawn_http_mock("200 OK", "audio/wav", b"RIFFxxxxWAVE".to_vec()).await;
        let tts = OpenAiTtsProvider::new(Some(url), "gpt-4o-mini-tts".into(), "sk-tts".into());
        let cleaned = clean_for_tts("hello <@1> https://x.example");
        let out = tts.synthesize(&cleaned, "alloy").await.unwrap();
        assert_eq!(&out[..4], b"RIFF");
        let req = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(req.contains("POST /audio/speech"), "{req}");
        assert!(req.contains("\"voice\":\"alloy\""), "{req}");
        assert!(!req.contains("https://"), "{req}");
    }
}
