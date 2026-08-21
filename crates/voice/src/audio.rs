//! 音声変換ユーティリティ。
//!
//! Discord (songbird) の受信 PCM は 48kHz ステレオ i16 インターリーブ。
//! STT へは 16kHz モノラル WAV に変換して渡す。依存を増やさないため
//! 変換は自前実装（音声認識用途では線形補間で十分）。

/// 48kHz ステレオ i16 → 16kHz モノラル i16。
///
/// ステレオはチャンネル平均でダウンミックスし、48k→16k は 3:1 の整数比
/// なので 3 サンプル平均で間引く（ローパスを兼ねる）。
pub fn downmix_48k_stereo_to_16k_mono(pcm: &[i16]) -> Vec<i16> {
    // インターリーブ [L R L R ...] → モノラル
    let mono: Vec<i32> = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|lr| (lr[0] as i32 + lr[1] as i32) / 2)
        .collect();
    mono.chunks(3)
        .map(|w| (w.iter().sum::<i32>() / w.len() as i32) as i16)
        .collect()
}

/// 16bit PCM → WAV バイト列（44 バイトヘッダ + データ）。
pub fn pcm_to_wav(pcm: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let byte_rate = sample_rate * channels as u32 * 2;
    let block_align = channels * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// 発話セグメントの RMS（無音・ノイズだけの区間を STT に投げない判定用）。
pub fn rms(pcm: &[i16]) -> f64 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / pcm.len() as f64).sqrt()
}

/// ユーザー単位の発話セグメンテーション。
///
/// Discord は 20ms ごとに音声パケットが届く。発話が `silence_frames` 分
/// 途切れたら 1 セグメント確定。`max_frames` を超えたら強制確定
/// （長広舌でも STT を回し始めるため）。
pub struct SpeechSegmenter {
    buf: Vec<i16>,
    silent_streak: u32,
    /// セグメント確定とみなす無音フレーム数（20ms 単位）。
    pub silence_frames: u32,
    /// 1 セグメントの最大フレーム数（20ms 単位）。
    pub max_frames: usize,
    frames_in_buf: usize,
}

/// 確定したセグメント（48kHz ステレオ PCM）。
pub struct Segment {
    pub pcm_48k_stereo: Vec<i16>,
}

impl SpeechSegmenter {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            silent_streak: 0,
            silence_frames: 40, // 800ms
            max_frames: 750,    // 15s
            frames_in_buf: 0,
        }
    }

    /// 20ms フレームを追加する。セグメントが確定したら返す。
    pub fn push_frame(&mut self, frame: &[i16]) -> Option<Segment> {
        self.buf.extend_from_slice(frame);
        self.frames_in_buf += 1;
        self.silent_streak = 0;
        if self.frames_in_buf >= self.max_frames {
            return self.take_segment();
        }
        None
    }

    /// このフレームで当該ユーザーが無音だったことを通知する。
    /// 無音が閾値まで続いたらセグメントを確定して返す。
    pub fn push_silence(&mut self) -> Option<Segment> {
        if self.buf.is_empty() {
            return None;
        }
        self.silent_streak += 1;
        if self.silent_streak >= self.silence_frames {
            return self.take_segment();
        }
        None
    }

    /// バッファに溜まっている音声を強制確定する（VC退出時など）。
    pub fn flush(&mut self) -> Option<Segment> {
        self.take_segment()
    }

    fn take_segment(&mut self) -> Option<Segment> {
        self.silent_streak = 0;
        self.frames_in_buf = 0;
        if self.buf.is_empty() {
            return None;
        }
        let pcm = std::mem::take(&mut self.buf);
        // 短すぎる（<300ms 相当 = 48k*2ch*0.3s）セグメントはノイズ扱いで捨てる
        if pcm.len() < 28_800 {
            return None;
        }
        Some(Segment {
            pcm_48k_stereo: pcm,
        })
    }
}

impl Default for SpeechSegmenter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_downmix_ratio() {
        // 48kHz ステレオ 1 秒 = 96000 サンプル → 16kHz モノ 16000 サンプル
        let pcm = vec![1000i16; 96_000];
        let out = downmix_48k_stereo_to_16k_mono(&pcm);
        assert_eq!(out.len(), 16_000);
        assert!(out.iter().all(|&s| s == 1000));
    }

    #[test]
    fn test_downmix_averages_channels() {
        // L=100, R=300 → 200
        let pcm: Vec<i16> = [100i16, 300].repeat(6);
        let out = downmix_48k_stereo_to_16k_mono(&pcm);
        assert!(out.iter().all(|&s| s == 200), "{out:?}");
    }

    #[test]
    fn test_wav_header() {
        let wav = pcm_to_wav(&[0i16; 16000], 16000, 1);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(wav.len(), 44 + 32000);
        // sample rate field
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16000);
        // channels
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
    }

    #[test]
    fn test_segmenter_finalizes_on_silence() {
        let mut seg = SpeechSegmenter::new();
        // 1 フレーム = 20ms の 48k ステレオ = 1920 サンプル
        let frame = vec![500i16; 1920];
        // 1 秒分（50 フレーム）話す
        for _ in 0..50 {
            assert!(seg.push_frame(&frame).is_none());
        }
        // 無音 39 フレームでは確定しない
        for _ in 0..39 {
            assert!(seg.push_silence().is_none());
        }
        // 40 フレーム目で確定
        let s = seg.push_silence().expect("segment must finalize");
        assert_eq!(s.pcm_48k_stereo.len(), 1920 * 50);
        // 確定後は空
        assert!(seg.flush().is_none());
    }

    #[test]
    fn test_segmenter_discards_too_short() {
        let mut seg = SpeechSegmenter::new();
        let frame = vec![500i16; 1920];
        // 200ms（10 フレーム）だけ → ノイズ扱い
        for _ in 0..10 {
            seg.push_frame(&frame);
        }
        for _ in 0..40 {
            if let Some(_s) = seg.push_silence() {
                panic!("short segment must be discarded");
            }
        }
    }

    #[test]
    fn test_segmenter_force_finalize_at_max() {
        let mut seg = SpeechSegmenter::new();
        seg.max_frames = 50; // 1 秒に短縮
        let frame = vec![500i16; 1920];
        let mut got = None;
        for i in 0..60 {
            if let Some(s) = seg.push_frame(&frame) {
                got = Some((i, s));
                break;
            }
        }
        let (i, s) = got.expect("must force-finalize at max_frames");
        assert_eq!(i, 49);
        assert_eq!(s.pcm_48k_stereo.len(), 1920 * 50);
    }

    #[test]
    fn test_rms() {
        assert_eq!(rms(&[]), 0.0);
        assert!(rms(&[0i16; 100]) < 1.0);
        assert!(rms(&[10000i16; 100]) > 9999.0);
    }
}
