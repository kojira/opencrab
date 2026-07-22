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

use opencrab_db::queries::{insert_session_log_best_effort, SessionLogRow};
use rusqlite::Connection;

/// Discord ユーザー発言。
///
/// 注意: 既存の慣習として `agent_id` 列には送信者IDが入る（発言の帰属が送信者）。
#[cfg(feature = "discord")]
#[allow(clippy::too_many_arguments)]
pub fn record_discord_user_message(
    conn: &Connection,
    session_id: &str,
    sender_id: &str,
    sender_name: &str,
    avatar_url: Option<&str>,
    channel_id: &str,
    text: &str,
    image_urls: &[String],
) {
    let mut meta = serde_json::json!({
        "source": "discord",
        "channel_id": channel_id,
        "user_name": sender_name,
    });
    if let Some(url) = avatar_url {
        meta["user_avatar_url"] = serde_json::json!(url);
    }
    if !image_urls.is_empty() {
        meta["image_urls"] = serde_json::json!(image_urls);
    }
    insert_session_log_best_effort(
        conn,
        &SessionLogRow {
            id: None,
            agent_id: sender_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "speech".to_string(),
            content: text.to_string(),
            speaker_id: Some(sender_id.to_string()),
            turn_number: None,
            metadata_json: Some(meta.to_string()),
            created_at: None,
        },
    );
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

/// エージェントの Discord 応答（metadata の triggered_by 差分は呼び出し元の型で表現）。
#[cfg(feature = "discord")]
pub fn record_discord_agent_reply(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
    channel_id: &str,
    text: &str,
    context: &opencrab_discord::DiscordReplyContext<'_>,
) {
    let meta = match context {
        opencrab_discord::DiscordReplyContext::Direct { tool_calls_made } => serde_json::json!({
            "source": "discord_response",
            "channel_id": channel_id,
            "tool_calls_made": tool_calls_made,
        }),
        opencrab_discord::DiscordReplyContext::SubtaskCompleted => serde_json::json!({
            "source": "discord_response",
            "channel_id": channel_id,
            "triggered_by": "subtask_completed",
        }),
        opencrab_discord::DiscordReplyContext::InteractionResponse { interaction_id } => {
            serde_json::json!({
                "source": "discord_response",
                "channel_id": channel_id,
                "triggered_by": "interaction_response",
                "interaction_id": interaction_id,
            })
        }
    };
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
            metadata_json: Some(meta.to_string()),
            created_at: None,
        },
    );
}

/// Nostr 受信イベント（投稿者発言）。agent_id 列には送信者 pubkey が入る（discord と同様の慣習）。
pub fn record_nostr_user_message(
    conn: &Connection,
    session_id: &str,
    sender_pubkey: &str,
    sender_name: &str,
    text: &str,
) {
    let meta = serde_json::json!({
        "source": "nostr",
        "user_name": sender_name,
        "pubkey": sender_pubkey,
    });
    insert_session_log_best_effort(
        conn,
        &SessionLogRow {
            id: None,
            agent_id: sender_pubkey.to_string(),
            session_id: session_id.to_string(),
            log_type: "speech".to_string(),
            content: text.to_string(),
            speaker_id: Some(sender_pubkey.to_string()),
            turn_number: None,
            metadata_json: Some(meta.to_string()),
            created_at: None,
        },
    );
}

/// エージェントの Nostr 返信。
pub fn record_nostr_agent_reply(conn: &Connection, agent_id: &str, session_id: &str, text: &str) {
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
            metadata_json: Some(serde_json::json!({"source": "nostr_response"}).to_string()),
            created_at: None,
        },
    );
}

/// REST 経由のエージェント応答（sessions.rs / agents_messages.rs 共通の形）。
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
#[cfg(feature = "discord")]
pub fn record_interaction_response(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
    record: &opencrab_discord::InteractionRecord<'_>,
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
