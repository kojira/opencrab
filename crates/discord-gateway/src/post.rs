//! say（通常発言）配送。core からの say を binding の channel への**通常投稿**として送る。
//! 返信は明示 `reply` DI 能力が担うので、say に reply target を暗黙設定しない（設計 §6.1・DI-16）。
//! dry-run / production の分岐は [`crate::transport`] の実装差で吸収する（say も invoke も同一 transport）。
//!
//! 2000 字超の本文は Discord 上限（設計 §1.2-4 / DESIGN-DISCORD-GATE.md:61）に合わせて分割する。
//! 分割規則は旧 `discord::gateway::split_message`（行優先・文字境界・順序保証）を移植したもの。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::transport::{DiscordTransport, TransportOutcome};

/// Discord の 1 メッセージ最大文字数（コードポイント数）。超過分は分割送信する。
pub(crate) const DISCORD_MAX_CHARS: usize = 2000;

pub fn delivery_nonce(delivery_id: &str) -> String {
    delivery_id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(25)
        .collect()
}

fn delivery_chunk_nonce(delivery_id: &str, chunk_index: usize) -> String {
    Sha256::digest(format!("{delivery_id}:{chunk_index}").as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .chars()
        .take(25)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PreparedSayRequest {
    pub channel: String,
    pub text: String,
    pub nonce: String,
    pub enforce_nonce: bool,
}

pub(crate) fn prepare_say_requests(
    delivery_id: &str,
    channel_id: &str,
    text: &str,
) -> Vec<PreparedSayRequest> {
    split_for_discord(text)
        .into_iter()
        .enumerate()
        .map(|(index, text)| PreparedSayRequest {
            channel: channel_id.to_string(),
            text,
            nonce: delivery_chunk_nonce(delivery_id, index),
            enforce_nonce: true,
        })
        .collect()
}

/// say の配送結果（観測性用）。
#[derive(Debug, PartialEq, Eq)]
pub enum SayDelivery {
    /// channel へ通常投稿した（dry-run 含む）。`message_id` は投稿できた**自分のメッセージ**の
    /// snowflake（transport の create_message 応答から得る）。**分割時は最後のチャンク**の id。
    /// gateway はこの id を delivery_id との対応表へ登録し、activity ended 後に core がその
    /// delivery_id を指定した場合だけ 🏁 を付ける。取得できなければ None。
    Posted { message_id: Option<String> },
    /// 投稿失敗（確定拒否・不明どちらも会話配送の失敗として観測）。
    Failed(String),
}

/// say を channel の通常投稿として配送する。`channel_id` は binding address から解決する。
///
/// 旧 `discord::gateway::send_to_channel` と同じ流儀:
/// 分割チャンクを**発生順に逐次投稿**し、途中失敗は **fail-fast**（既送分はそのまま・以降は
/// 送らない・自動再送 0＝§6.4-4）。返す message id は成功時の**最後のチャンク**のもの。
pub async fn deliver_say(
    transport: &Arc<dyn DiscordTransport>,
    channel_id: &str,
    text: &str,
) -> SayDelivery {
    deliver_say_with_nonce(transport, channel_id, text, None).await
}

pub(crate) async fn deliver_prepared_say(
    transport: &Arc<dyn DiscordTransport>,
    requests: &[PreparedSayRequest],
) -> SayDelivery {
    let mut last_id = None;
    for request in requests {
        match transport
            .create_message_with_nonce(
                &request.channel,
                &request.text,
                &request.nonce,
                request.enforce_nonce,
            )
            .await
        {
            TransportOutcome::Ok(value) => last_id = message_id_of(&value),
            TransportOutcome::Rejected => return SayDelivery::Failed("rejected".into()),
            TransportOutcome::Indeterminate => return SayDelivery::Failed("indeterminate".into()),
        }
    }
    SayDelivery::Posted {
        message_id: last_id,
    }
}

pub async fn deliver_say_with_nonce(
    transport: &Arc<dyn DiscordTransport>,
    channel_id: &str,
    text: &str,
    nonce: Option<&str>,
) -> SayDelivery {
    // create_message の Ok は投稿できたメッセージ id を載せる（production=serenity 実 id・
    // dry-run=合成 id）。分割時は最後の成功チャンクの id を対応表へ返す。
    let mut last_id = None;
    for chunk in split_for_discord(text) {
        let outcome = match nonce {
            Some(nonce) => {
                transport
                    .create_message_with_nonce(channel_id, &chunk, nonce, true)
                    .await
            }
            None => transport.create_message(channel_id, &chunk).await,
        };
        match outcome {
            TransportOutcome::Ok(v) => last_id = message_id_of(&v),
            TransportOutcome::Rejected => return SayDelivery::Failed("rejected".into()),
            TransportOutcome::Indeterminate => return SayDelivery::Failed("indeterminate".into()),
        }
    }
    SayDelivery::Posted {
        message_id: last_id,
    }
}

/// `TransportOutcome::Ok` の JSON から message_id を取り出す（欠落時は `None`。production=serenity
/// 実 id・dry-run=合成 id を載せるので通常は `Some`）。
pub(crate) fn message_id_of(v: &Value) -> Option<String> {
    v.get("message_id")
        .and_then(|m| m.as_str())
        .map(str::to_string)
}

/// Discord 文字数上限に応じてチャンク列を返す。
///
/// 上限以下は本文を**そのまま 1 チャンク**にする（trailing newline 等の verbatim 送信を保つ。
/// 旧 `send_to_channel` の fast path 相当）。超過時のみ [`split_message`] で行優先・文字境界に割る。
pub(crate) fn split_for_discord(text: &str) -> Vec<String> {
    if text.chars().count() <= DISCORD_MAX_CHARS {
        return vec![text.to_string()];
    }
    split_message(text, DISCORD_MAX_CHARS)
}

/// Discord の文字数制限に合わせてメッセージを分割する（旧 `discord::gateway::split_message` を移植）。
///
/// 長さは文字数（コードポイント数）で数え、長い行は文字境界で分割する。
/// バイト境界での分割はマルチバイトUTF-8（日本語等）を破壊するため行わない。
/// 空のチャンクは生成しない（Discordは空メッセージを400で拒否する）。
fn split_message(text: &str, max_len: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;

    for line in text.lines() {
        let line_chars = line.chars().count();

        // 1行が制限を超える場合は文字境界でさらに分割
        if line_chars > max_len {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                current_chars = 0;
            }
            let mut piece = String::new();
            let mut piece_chars = 0usize;
            for ch in line.chars() {
                piece.push(ch);
                piece_chars += 1;
                if piece_chars == max_len {
                    chunks.push(std::mem::take(&mut piece));
                    piece_chars = 0;
                }
            }
            if !piece.is_empty() {
                chunks.push(piece);
            }
            continue;
        }

        if current_chars + line_chars + 1 > max_len && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        if !current.is_empty() {
            current.push('\n');
            current_chars += 1;
        }
        current.push_str(line);
        current_chars += line_chars;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::testfake::RecordingTransport;
    use crate::transport::DryRunTransport;

    // ---- split_message（旧 discord::gateway のテストを移植） ----

    #[test]
    fn s7_discord_nonce_is_stable_and_at_most_25_characters() {
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        assert_eq!(delivery_nonce(id), delivery_nonce(id));
        assert_eq!(delivery_nonce(id).len(), 25);
        assert!(delivery_nonce(id)
            .bytes()
            .all(|b| b.is_ascii_alphanumeric()));
    }

    #[tokio::test]
    async fn s7_long_delivery_persists_and_sends_exact_distinct_nonce_sequence() {
        let delivery_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let requests = prepare_say_requests(delivery_id, "100", &"あ".repeat(2500));
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].text.chars().count(), 2000);
        assert_eq!(requests[1].text.chars().count(), 500);
        assert_ne!(requests[0].nonce, requests[1].nonce);
        assert!(requests
            .iter()
            .all(|request| request.nonce.len() <= 25 && request.enforce_nonce));

        let prepared = serde_json::to_vec(&requests).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let ledger =
            opencrab_gate_client::emission::EmissionLedger::open(&temp.path().join("discord.db"))
                .unwrap();
        ledger
            .prepare(
                "binding",
                delivery_id,
                &"a".repeat(64),
                "at_most_once_indeterminate",
                &requests[0].nonce,
                &prepared,
                &"b".repeat(64),
                1,
                Some(2),
            )
            .unwrap();
        let persisted = ledger.get("binding", delivery_id).unwrap().unwrap();
        assert_eq!(persisted.prepared_request, prepared);
        let persisted_requests: Vec<PreparedSayRequest> =
            serde_json::from_slice(&persisted.prepared_request).unwrap();

        let rec = Arc::new(RecordingTransport::default());
        let transport: Arc<dyn DiscordTransport> = rec.clone();
        assert!(matches!(
            deliver_prepared_say(&transport, &persisted_requests).await,
            SayDelivery::Posted { .. }
        ));
        assert_eq!(
            rec.bodies(),
            requests.iter().map(|r| r.text.clone()).collect::<Vec<_>>()
        );
        assert_eq!(
            rec.nonces(),
            requests
                .iter()
                .map(|r| (r.nonce.clone(), true))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn split_message_short() {
        assert_eq!(split_message("hello", 2000), vec!["hello"]);
    }

    #[test]
    fn split_message_long() {
        let text = "a".repeat(2500);
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].len() <= 2000);
    }

    #[test]
    fn split_message_long_japanese_no_corruption() {
        // 2000文字超の日本語1行が文字境界で分割され、U+FFFDが混入しないこと。
        let text = "あ".repeat(2500);
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 2000);
        assert_eq!(chunks[1].chars().count(), 500);
        for chunk in &chunks {
            assert!(!chunk.contains('\u{FFFD}'), "no replacement characters");
            assert!(!chunk.is_empty(), "no empty chunks");
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn split_message_exact_boundary_no_empty_chunk() {
        let text = "a".repeat(200);
        let chunks = split_message(&text, 200);
        assert_eq!(chunks.len(), 1);
        assert!(chunks.iter().all(|c| !c.is_empty()));
    }

    #[test]
    fn split_message_multiline_line_preferred() {
        let lines: Vec<String> = (0..100)
            .map(|i| format!("Line {i}: some content here"))
            .collect();
        let text = lines.join("\n");
        let chunks = split_message(&text, 200);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 200);
        }
        // 行優先: 分割は行境界で起き、各チャンクを改行で連結すると原文へ戻る。
        assert_eq!(chunks.join("\n"), text);
    }

    // ---- split_for_discord（上限以下は verbatim 1 チャンク） ----

    #[test]
    fn split_for_discord_under_limit_is_single_verbatim_chunk() {
        // trailing newline を保つ（lines() で失わない）＝ fast path の verbatim 送信。
        let text = "line1\nline2\n";
        assert_eq!(split_for_discord(text), vec![text.to_string()]);
        // ちょうど 2000 字は 1 通。
        assert_eq!(split_for_discord(&"a".repeat(2000)).len(), 1);
    }

    #[test]
    fn split_for_discord_over_limit_splits() {
        assert!(split_for_discord(&"a".repeat(2001)).len() >= 2);
    }

    // ---- deliver_say ----

    #[tokio::test]
    async fn dry_run_say_is_posted_with_own_message_id() {
        let t: Arc<dyn DiscordTransport> = Arc::new(DryRunTransport);
        // dry-run でも自分の投稿 id を持つ（🏁 の付け先を QC が観測できるようにする）。
        match deliver_say(&t, "100", "hello").await {
            SayDelivery::Posted { message_id } => {
                assert!(
                    message_id.is_some(),
                    "dry-run say は自分の message id を返す"
                );
            }
            other => panic!("expected Posted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn short_say_sends_single_message_and_returns_its_id() {
        let rec = Arc::new(RecordingTransport::default());
        let t: Arc<dyn DiscordTransport> = rec.clone();
        let out = deliver_say(&t, "100", "hello").await;
        assert_eq!(rec.kinds(), vec!["say"], "2000字以下は 1 通");
        assert_eq!(
            out,
            SayDelivery::Posted {
                message_id: Some("1000".into())
            }
        );
    }

    #[tokio::test]
    async fn long_say_splits_sequentially_and_returns_last_chunk_id() {
        let rec = Arc::new(RecordingTransport::default());
        let t: Arc<dyn DiscordTransport> = rec.clone();
        // 2500 字の 1 行 → 2000 + 500 の 2 チャンク。
        let text = "a".repeat(2500);
        let out = deliver_say(&t, "100", &text).await;

        let bodies = rec.bodies();
        assert_eq!(bodies.len(), 2, "分割されて逐次送信される");
        assert_eq!(bodies[0].chars().count(), 2000);
        assert_eq!(bodies[1].chars().count(), 500);
        assert_eq!(bodies.concat(), text, "順序保証・欠落なし");
        // 最後のチャンクの id（連番: 2 回目 = 1001）が返る。
        assert_eq!(
            out,
            SayDelivery::Posted {
                message_id: Some("1001".into())
            }
        );
    }

    #[tokio::test]
    async fn long_say_fail_fast_stops_and_keeps_earlier_chunks() {
        // 3 チャンクになる本文で 2 番目（index=1）を Rejected にする。
        let rec = Arc::new(RecordingTransport {
            fail_at: Some(1),
            ..Default::default()
        });
        let t: Arc<dyn DiscordTransport> = rec.clone();
        let text = "b".repeat(5000); // 2000 * 2 + 1000 = 3 チャンク
        let out = deliver_say(&t, "100", &text).await;

        assert_eq!(out, SayDelivery::Failed("rejected".into()));
        // fail-fast: 既送 1 通 + 失敗 1 通 = 2 回で停止（3 通目は送らない）。
        assert_eq!(rec.bodies().len(), 2, "途中失敗で以降のチャンクは送らない");
    }

    #[tokio::test]
    async fn indeterminate_chunk_is_reported_as_failed() {
        let rec = Arc::new(RecordingTransport {
            fail_at: Some(0),
            indeterminate: true,
            ..Default::default()
        });
        let t: Arc<dyn DiscordTransport> = rec.clone();
        assert_eq!(
            deliver_say(&t, "100", &"c".repeat(2500)).await,
            SayDelivery::Failed("indeterminate".into())
        );
    }
}
