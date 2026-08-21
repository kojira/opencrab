//! Server-Sent Events (SSE) の共有デコーダ。
//!
//! 各プロバイダのストリーミング応答を「TCPチャンク単位」で解析すると、
//! 次の問題が起きる:
//! - 1チャンクに複数の `data:` イベントが含まれると、最後の1つ以外が失われる。
//! - `data:` を含まないチャンク（`[DONE]` 終端や keep-alive コメント）がエラーになる。
//! - チャンク境界を跨いだ行やマルチバイトUTF-8が壊れる／破棄される。
//!
//! `line_stream` はバイト列をバッファし、改行区切りの「完全な行」だけを取り出して
//! 返すことで、これらを構造的に解消する。行の切り出しは ASCII の `\n` 境界で行い、
//! 完全な行のみをUTF-8デコードするため、マルチバイト文字がチャンク境界で分断されても
//! 壊れない。

use anyhow::Result;
use futures::stream::{BoxStream, Stream, StreamExt};

/// バイトストリーム（SSEボディ）を、チャンク境界を跨いでバッファしつつ
/// 改行区切りの完全な行ストリームへ変換する。
///
/// - 各出力は末尾の `\r`/`\n` を除いた1行。
/// - ストリーム終了時、バッファに残った末尾行があれば最後に1つ emit する。
/// - チャンク取得エラーはそのまま `Err` として1要素 emit する。
pub fn line_stream<S, B>(byte_stream: S) -> BoxStream<'static, Result<String>>
where
    S: Stream<Item = reqwest::Result<B>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
{
    let s = futures::stream::unfold(
        (byte_stream.boxed(), Vec::<u8>::new(), false),
        |(mut byte_stream, mut buf, mut ended)| async move {
            loop {
                // バッファ内に完全な行があれば取り出して返す。
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line_bytes)
                        .trim_end_matches(['\r', '\n'])
                        .to_string();
                    return Some((Ok(line), (byte_stream, buf, ended)));
                }

                if ended {
                    // ストリーム終了: 残った末尾行をフラッシュ。
                    if !buf.is_empty() {
                        let line = String::from_utf8_lossy(&buf).trim().to_string();
                        buf.clear();
                        if !line.is_empty() {
                            return Some((Ok(line), (byte_stream, buf, ended)));
                        }
                    }
                    return None;
                }

                match byte_stream.next().await {
                    Some(Ok(chunk)) => buf.extend_from_slice(chunk.as_ref()),
                    Some(Err(e)) => {
                        return Some((
                            Err(anyhow::anyhow!("stream chunk error: {e}")),
                            (byte_stream, buf, ended),
                        ));
                    }
                    None => ended = true,
                }
            }
        },
    );
    Box::pin(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn splits_events_across_chunk_boundaries() {
        // "data: A\n" と "data: B\n" が1つのチャンクにまとまり、
        // さらにイベントがチャンク境界で分断されるケース。
        let chunks: Vec<reqwest::Result<Vec<u8>>> = vec![
            Ok(b"data: A\ndata: B\nda".to_vec()),
            Ok(b"ta: C\n".to_vec()),
            Ok(b"data: [DONE]\n".to_vec()),
        ];
        let byte_stream = futures::stream::iter(chunks);
        let lines: Vec<String> = line_stream(byte_stream).map(|r| r.unwrap()).collect().await;
        assert_eq!(
            lines,
            vec![
                "data: A".to_string(),
                "data: B".to_string(),
                "data: C".to_string(),
                "data: [DONE]".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn preserves_multibyte_utf8_split_across_chunks() {
        // 日本語「あ」(E3 81 82) がチャンク境界で分断されても壊れない。
        let full = "data: あい\n".as_bytes().to_vec();
        let (first, second) = full.split_at(7); // 途中で分割
        let chunks: Vec<reqwest::Result<Vec<u8>>> = vec![Ok(first.to_vec()), Ok(second.to_vec())];
        let byte_stream = futures::stream::iter(chunks);
        let lines: Vec<String> = line_stream(byte_stream).map(|r| r.unwrap()).collect().await;
        assert_eq!(lines, vec!["data: あい".to_string()]);
    }

    #[tokio::test]
    async fn flushes_trailing_line_without_newline() {
        let chunks: Vec<reqwest::Result<Vec<u8>>> = vec![Ok(b"data: X".to_vec())];
        let byte_stream = futures::stream::iter(chunks);
        let lines: Vec<String> = line_stream(byte_stream).map(|r| r.unwrap()).collect().await;
        assert_eq!(lines, vec!["data: X".to_string()]);
    }
}
