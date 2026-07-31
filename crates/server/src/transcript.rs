//! ターン/会話ログの転記サービス（#42）。
//!
//! 「`SessionLogRow` を組み立てて `insert_session_log` する」パターンが discord /
//! server に metadata を少しずつ変えて散在し、記録ポリシーの所有者が居なかった。
//! ターンイベント（ユーザー発言 / エージェント応答 / NO_REPLY / A2UI 応答）の
//! 行の形はこのモジュールが所有する。呼び出し側は行を組み立てず、型付きの
//! パラメータを渡す。
//!
//! 書き込みは全て best-effort（失敗は warn ログ、応答フローは止めない — #47）。
//! 各関数の行の形（agent_id / speaker_id / metadata のキー）は移行前の各サイトの
//! 形を正確に保存している。形を変えるときは会話ビルダー（build_conversation_string）
//! と web の消費側を確認すること。
//!
//! ## transport 中立化（#158 S3 / #156 S6）
//!
//! 以前は Discord 用の関数だけが `#[cfg(feature = "discord")]` 配下にあった。理由は
//! シグネチャが `opencrab_discord` の型（`DiscordReplyContext` / `InteractionRecord`）を
//! 引いていたことだけで、行の形自体は Nostr と同型だった。型を
//! `opencrab_actions::transcript` へ移したので機能フラグは不要になり、受信発話と
//! エージェント応答は **由来（[`TranscriptSource`]）を引数で受ける 1 つの関数**に統合した。
//!
//! `metadata_json` は統合前とバイト等価。根拠は 2 つ:
//!
//! - `source` / `triggered_by` の値は [`TranscriptSource`] と
//!   [`AgentReplyContext`] だけが持ち、統合前の文字列をそのまま出す（自由文字列で
//!   受けないので綴り違いが混入しない）。
//! - キーの有無は `Option` / 空判定で統合前と同じ条件に保つ（無い由来では出さない）。
//!   `serde_json` の `Map` は `preserve_order` 無効時 `BTreeMap` なので、出力は
//!   キー集合だけで決まり挿入順に依存しない。
//!
//! 下の `tests` が全経路の `metadata_json` をリテラル文字列で固定している。web の表示
//! （`SessionDetail` / `SessionCard`）は `source` の文字列比較で分岐しているため、
//! この不変条件が守られている限り web は無変更で動く。

use opencrab_actions::transcript::{
    AgentReplyContext, InboundMessageRecord, InteractionRecord, OutboundReplyRecord,
    TranscriptSource,
};
use opencrab_db::queries::{insert_session_log_best_effort, SessionLogRow};
use rusqlite::Connection;

/// 受信発言の記録をあきらめるまでの試行回数（#284 P0-3）。
///
/// 現実的な失敗は SQLITE_BUSY（同一 DB への並行書き込み）で、数十 ms 待てば通る。
/// ディスク満杯・権限のような回復しない失敗で長く粘っても意味は無いので 3 回で切り、
/// 呼び出し側にエスカレーションさせる。
const INBOUND_RECORD_ATTEMPTS: usize = 3;
/// 再試行の待機（n 回目の失敗後に `INBOUND_RETRY_DELAY * n` 待つ）。
const INBOUND_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// ゲートウェイから受信した発言（Discord / Nostr 共通）。記録できたら `true`。
///
/// 注意: 既存の慣習として `agent_id` 列には送信者IDが入る（発言の帰属が送信者）。
///
/// **ここだけは best-effort にしない**（#284 P0-3）。他の転記（応答・NO_REPLY）は
/// 落ちても会話は続くが、ユーザー発言が落ちるとその指示は**二度と会話履歴に現れず**、
/// エージェントは見ないまま応答する。実際に「オーナーの発言が DB に 1 件も無い」
/// 事故が起きており、当時のログには warn が 1 行残るだけだった。
/// 失敗は戻り値で呼び出し側へ返し、呼び出し側がオーナーへエスカレーションする。
pub fn record_inbound_message(
    conn: &Connection,
    source: TranscriptSource,
    record: &InboundMessageRecord<'_>,
) -> bool {
    let row = SessionLogRow {
        id: None,
        agent_id: record.sender_id.to_string(),
        session_id: record.session_id.to_string(),
        log_type: "speech".to_string(),
        content: record.text.to_string(),
        speaker_id: Some(record.sender_id.to_string()),
        turn_number: None,
        metadata_json: Some(inbound_metadata_json(source, record)),
        created_at: None,
    };
    for attempt in 1..=INBOUND_RECORD_ATTEMPTS {
        match opencrab_db::queries::insert_session_log(conn, &row) {
            Ok(_) => return true,
            Err(e) => {
                tracing::warn!(
                    session_id = %row.session_id,
                    attempt,
                    "inbound message insert failed: {e}"
                );
                if attempt < INBOUND_RECORD_ATTEMPTS {
                    std::thread::sleep(INBOUND_RETRY_DELAY * attempt as u32);
                }
            }
        }
    }
    tracing::error!(
        session_id = %row.session_id,
        attempts = INBOUND_RECORD_ATTEMPTS,
        "inbound user message could NOT be persisted; the agent will not see it"
    );
    false
}

/// NO_REPLY（沈黙の明示）。
pub fn record_agent_no_reply(conn: &Connection, agent_id: &str, session_id: &str) {
    insert_session_log_best_effort(
        conn,
        &SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "speech".to_string(),
            content: "NO_REPLY".to_string(),
            speaker_id: Some(agent_id.to_string()),
            turn_number: None,
            metadata_json: Some(serde_json::json!({"no_reply": true}).to_string()),
            created_at: None,
        },
    );
}

/// エージェントがゲートウェイへ返した応答（Discord / Nostr 共通）。
///
/// metadata の `triggered_by` 等の差分は [`AgentReplyContext`] で表す。
pub fn record_outbound_reply(
    conn: &Connection,
    source: TranscriptSource,
    record: &OutboundReplyRecord<'_>,
) {
    insert_session_log_best_effort(
        conn,
        &SessionLogRow {
            id: None,
            agent_id: record.agent_id.to_string(),
            session_id: record.session_id.to_string(),
            log_type: "speech".to_string(),
            content: record.text.to_string(),
            speaker_id: Some(record.agent_id.to_string()),
            turn_number: None,
            metadata_json: Some(outbound_metadata_json(source, record)),
            created_at: None,
        },
    );
}

/// REST 経由のエージェント応答（sessions.rs / agents_messages.rs 共通の形）。
///
/// ゲートウェイ経由の応答と違い `source` を持たない（統合前からそう）。
pub fn record_rest_agent_reply(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
    text: &str,
    iterations: usize,
    tool_calls_made: usize,
) {
    insert_session_log_best_effort(
        conn,
        &SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "speech".to_string(),
            content: text.to_string(),
            speaker_id: Some(agent_id.to_string()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({
                    "iterations": iterations,
                    "tool_calls_made": tool_calls_made,
                })
                .to_string(),
            ),
            created_at: None,
        },
    );
}

/// A2UI インタラクション応答の記録。
pub fn record_interaction_response(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
    record: &InteractionRecord<'_>,
) {
    insert_session_log_best_effort(
        conn,
        &SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "interaction_response".to_string(),
            content: record.content.to_string(),
            speaker_id: Some("system".to_string()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({
                    "interaction_id": record.interaction_id,
                    "surface_id": record.surface_id,
                    "action_name": record.action_name,
                    "component_id": record.component_id,
                    "responder_id": record.responder_id,
                })
                .to_string(),
            ),
            created_at: None,
        },
    );
}

/// 受信発話の `metadata_json`。
///
/// キーの有無は統合前と同じ条件に保つ: Discord は `channel_id`（Nostr には無い）、
/// Nostr は `pubkey`（Discord には無い）、`user_avatar_url` / `image_urls` は
/// 値があるときだけ出す。
fn inbound_metadata_json(source: TranscriptSource, record: &InboundMessageRecord<'_>) -> String {
    let mut meta = serde_json::json!({
        "source": source.inbound(),
        "user_name": record.sender_name,
    });
    if let Some(channel_id) = record.channel_id {
        meta["channel_id"] = serde_json::json!(channel_id);
    }
    if let Some(pubkey) = record.pubkey {
        meta["pubkey"] = serde_json::json!(pubkey);
    }
    if let Some(url) = record.avatar_url {
        meta["user_avatar_url"] = serde_json::json!(url);
    }
    if !record.image_urls.is_empty() {
        meta["image_urls"] = serde_json::json!(record.image_urls);
    }
    meta.to_string()
}

/// エージェント応答の `metadata_json`。
fn outbound_metadata_json(source: TranscriptSource, record: &OutboundReplyRecord<'_>) -> String {
    let mut meta = serde_json::json!({ "source": source.reply() });
    if let Some(channel_id) = record.channel_id {
        meta["channel_id"] = serde_json::json!(channel_id);
    }
    match &record.context {
        // 起動要因を記録しない由来（Nostr）: `source` だけの形を保つ。
        None => {}
        Some(AgentReplyContext::Direct { tool_calls_made }) => {
            meta["tool_calls_made"] = serde_json::json!(tool_calls_made);
        }
        Some(AgentReplyContext::SubtaskCompleted) => {
            meta["triggered_by"] = serde_json::json!("subtask_completed");
        }
        Some(AgentReplyContext::InteractionResponse { interaction_id }) => {
            meta["triggered_by"] = serde_json::json!("interaction_response");
            meta["interaction_id"] = serde_json::json!(interaction_id);
        }
    }
    meta.to_string()
}

/// `metadata_json` のバイト等価をリテラルで固定するスナップショットテスト（#158 S3）。
///
/// 期待値は統合前の各サイト（`record_discord_user_message` /
/// `record_discord_agent_reply` / `record_nostr_user_message` /
/// `record_nostr_agent_reply`）が書いていた JSON をそのまま写したもの。web の表示は
/// `source` の文字列で分岐するので、ここが割れたら web も直す必要がある。
#[cfg(test)]
mod tests {
    use super::*;

    fn discord_inbound<'a>(
        avatar_url: Option<&'a str>,
        image_urls: &'a [String],
    ) -> InboundMessageRecord<'a> {
        InboundMessageRecord {
            session_id: "sess-1",
            sender_id: "111",
            sender_name: "のすたろう",
            avatar_url,
            channel_id: Some("222"),
            pubkey: None,
            text: "こんにちは",
            image_urls,
        }
    }

    #[test]
    fn discord_inbound_metadata_is_byte_identical() {
        assert_eq!(
            inbound_metadata_json(TranscriptSource::Discord, &discord_inbound(None, &[])),
            r#"{"channel_id":"222","source":"discord","user_name":"のすたろう"}"#
        );
    }

    #[test]
    fn discord_inbound_metadata_keeps_avatar_and_images() {
        let images = [
            "https://cdn/a.png".to_string(),
            "https://cdn/b.png".to_string(),
        ];
        assert_eq!(
            inbound_metadata_json(
                TranscriptSource::Discord,
                &discord_inbound(Some("https://cdn/avatar.png"), &images)
            ),
            r#"{"channel_id":"222","image_urls":["https://cdn/a.png","https://cdn/b.png"],"source":"discord","user_avatar_url":"https://cdn/avatar.png","user_name":"のすたろう"}"#
        );
    }

    #[test]
    fn nostr_inbound_metadata_is_byte_identical() {
        let record = InboundMessageRecord {
            session_id: "sess-1",
            sender_id: "npub-hex",
            sender_name: "だれか",
            avatar_url: None,
            channel_id: None,
            pubkey: Some("npub-hex"),
            text: "よろしく",
            image_urls: &[],
        };
        assert_eq!(
            inbound_metadata_json(TranscriptSource::Nostr, &record),
            r#"{"pubkey":"npub-hex","source":"nostr","user_name":"だれか"}"#
        );
    }

    fn discord_reply(context: Option<AgentReplyContext<'_>>) -> OutboundReplyRecord<'_> {
        OutboundReplyRecord {
            agent_id: "agent-1",
            session_id: "sess-1",
            channel_id: Some("222"),
            text: "はい",
            context,
        }
    }

    #[test]
    fn discord_direct_reply_metadata_is_byte_identical() {
        assert_eq!(
            outbound_metadata_json(
                TranscriptSource::Discord,
                &discord_reply(Some(AgentReplyContext::Direct { tool_calls_made: 3 }))
            ),
            r#"{"channel_id":"222","source":"discord_response","tool_calls_made":3}"#
        );
    }

    #[test]
    fn discord_subtask_completed_reply_metadata_is_byte_identical() {
        assert_eq!(
            outbound_metadata_json(
                TranscriptSource::Discord,
                &discord_reply(Some(AgentReplyContext::SubtaskCompleted))
            ),
            r#"{"channel_id":"222","source":"discord_response","triggered_by":"subtask_completed"}"#
        );
    }

    #[test]
    fn discord_interaction_reply_metadata_is_byte_identical() {
        assert_eq!(
            outbound_metadata_json(
                TranscriptSource::Discord,
                &discord_reply(Some(AgentReplyContext::InteractionResponse {
                    interaction_id: "int-9"
                }))
            ),
            r#"{"channel_id":"222","interaction_id":"int-9","source":"discord_response","triggered_by":"interaction_response"}"#
        );
    }

    #[test]
    fn nostr_reply_metadata_is_byte_identical() {
        let record = OutboundReplyRecord {
            agent_id: "agent-1",
            session_id: "sess-1",
            channel_id: None,
            text: "はい",
            context: None,
        };
        assert_eq!(
            outbound_metadata_json(TranscriptSource::Nostr, &record),
            r#"{"source":"nostr_response"}"#
        );
    }

    /// 宛先の識別子（web の表示が参照する `channel_id`）を落としていないこと。
    #[test]
    fn discord_rows_keep_destination_id() {
        assert!(
            inbound_metadata_json(TranscriptSource::Discord, &discord_inbound(None, &[]))
                .contains(r#""channel_id":"222""#)
        );
        assert!(outbound_metadata_json(
            TranscriptSource::Discord,
            &discord_reply(Some(AgentReplyContext::Direct { tool_calls_made: 0 }))
        )
        .contains(r#""channel_id":"222""#));
    }

    /// 書き込みまで通した行の形（agent_id / speaker_id / log_type / metadata）。
    #[test]
    fn recorded_rows_match_pre_merge_shape() {
        let conn = opencrab_db::init_memory().unwrap();

        record_inbound_message(
            &conn,
            TranscriptSource::Discord,
            &discord_inbound(None, &[]),
        );
        record_outbound_reply(
            &conn,
            TranscriptSource::Discord,
            &discord_reply(Some(AgentReplyContext::SubtaskCompleted)),
        );

        let rows: Vec<(String, String, String, String)> = conn
            .prepare(
                "SELECT agent_id, speaker_id, log_type, metadata_json FROM memory_sessions \
                 WHERE session_id = 'sess-1' ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(
            rows,
            vec![
                (
                    "111".to_string(),
                    "111".to_string(),
                    "speech".to_string(),
                    r#"{"channel_id":"222","source":"discord","user_name":"のすたろう"}"#
                        .to_string(),
                ),
                (
                    "agent-1".to_string(),
                    "agent-1".to_string(),
                    "speech".to_string(),
                    r#"{"channel_id":"222","source":"discord_response","triggered_by":"subtask_completed"}"#
                        .to_string(),
                ),
            ]
        );
    }
}
