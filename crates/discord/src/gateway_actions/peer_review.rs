//! ピアレビュー依頼アクション（LOOPS 原則 II: 自己採点させるな — #49 phase 2）。
//!
//! 自分の成果物（diff / 出力 / トレース）を **要約せず RAW のまま** Discord チャンネルへ
//! 投稿し、別ベクトルの bot にレビューを依頼する。self-evaluator（同モデル・同システム）
//! では捕まえられないフレーミング/設計の誤りを、別の視点が捕まえる。
//!
//! レビュアー側の応答規約（`[Peer Review Request]` には NO_REPLY せず `[Peer Review]` で
//! 応答する）は system prompt（server/process.rs build_agent_context）に定義されている。

use serde_json::json;
use serenity::all::{ChannelId, CreateMessage};
use tracing::{error, warn};

use opencrab_core::llm_text::truncate_chars;
use opencrab_gateway::{GatewayActionResult, PEER_REVIEW_REQUEST_MARKER};

use super::webhook::build_part_messages;
use super::DiscordGatewayActions;

/// content の上限（chars）。Discord のレート制限（~5通/5秒/チャンネル）の1ウィンドウに
/// 収まる分割数（ヘッダ+6 part 程度）に抑える。超える場合はワークスペースに保存して
/// discord_send_file を使う。
pub(crate) const MAX_REVIEW_CONTENT_CHARS: usize = 12_000;

/// ヘッダに描画する goal / contract / instructions の上限（chars）。
/// ヘッダは1通に収める必要がある（Discord 上限 2000 chars）ため、フィールドを切り詰める。
/// 全文はレビュー対象の content 側や台帳にあるので、ここは案内で足りる。
const HEADER_FIELD_MAX_CHARS: usize = 300;

/// レビュー依頼ヘッダの構成要素。
pub(crate) struct PeerReviewHeader<'a> {
    pub agent_name: &'a str,
    /// (task_id, goal, contract)
    pub task: Option<(i64, &'a str, Option<&'a str>)>,
    pub instructions: Option<&'a str>,
}

/// 1通目 = ヘッダ、2通目以降 = `part X/N` + RAW content（切り詰めない）。
pub(crate) fn build_peer_review_messages(
    header: &PeerReviewHeader<'_>,
    content: &str,
    limit: usize,
) -> Vec<String> {
    let parts = build_part_messages(content, limit);
    let part_count = parts.len();

    // ヘッダは1通（2000 chars 上限）に収める: 可変長フィールドは切り詰める
    let mut head = String::new();
    match header.task {
        Some((task_id, _, _)) => head.push_str(&format!(
            "{PEER_REVIEW_REQUEST_MARKER} from {} — task #{task_id}\n",
            truncate_chars(header.agent_name, 100),
        )),
        None => head.push_str(&format!(
            "{PEER_REVIEW_REQUEST_MARKER} from {} — no active task\n",
            truncate_chars(header.agent_name, 100),
        )),
    }
    if let Some((_, goal, contract)) = header.task {
        head.push_str(&format!(
            "goal: {}\n",
            truncate_chars(goal, HEADER_FIELD_MAX_CHARS)
        ));
        if let Some(contract) = contract.filter(|c| !c.trim().is_empty()) {
            head.push_str(&format!(
                "contract: {}\n",
                truncate_chars(contract, HEADER_FIELD_MAX_CHARS)
            ));
        }
    }
    if let Some(instructions) = header.instructions.filter(|i| !i.trim().is_empty()) {
        head.push_str(&format!(
            "instructions: {}\n",
            truncate_chars(instructions, HEADER_FIELD_MAX_CHARS)
        ));
    }
    head.push_str(&format!(
        "Please review the raw content in the following part 1/{part_count}..{part_count}/{part_count} messages with fresh eyes.\n\
         Reply with ONE message starting with [Peer Review] containing: score: <0.0-1.0>, gaps: <concrete list or none>, summary: <one sentence>. Judge on evidence, not confidence.\n\
         parts: {part_count}"
    ));

    let mut msgs = Vec::with_capacity(part_count + 1);
    msgs.push(head);
    msgs.extend(parts);
    msgs
}

impl DiscordGatewayActions {
    pub(crate) async fn execute_request_peer_review(
        &self,
        args: &serde_json::Value,
    ) -> GatewayActionResult {
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) if !c.trim().is_empty() => c,
            _ => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("contentパラメータが必要です（レビュー対象のRAWコンテンツ）".to_string()),
                }
            }
        };
        if content.chars().count() > MAX_REVIEW_CONTENT_CHARS {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!(
                    "contentが{MAX_REVIEW_CONTENT_CHARS}文字を超えています — ワークスペースにファイルとして保存し discord_send_file で添付した上で、contentには要点とファイル名を書いてください"
                )),
            };
        }
        let channel_id_str = match args.get("channel_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("channel_idパラメータが必要です".to_string()),
                }
            }
        };
        let channel_id: u64 = match channel_id_str.parse() {
            Ok(id) => id,
            Err(_) => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("無効なchannel_id: {channel_id_str}")),
                }
            }
        };
        let instructions = args
            .get("instructions")
            .and_then(|v| v.as_str())
            .filter(|i| !i.trim().is_empty());
        let session_id = args
            .get("__session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // agent 表示名と active タスクを1ロックスコープで解決（await 前に drop）
        let (agent_name, task) = {
            match self.db.lock() {
                Ok(conn) => {
                    let name = opencrab_db::queries::get_agent(&conn, &self.agent_id)
                        .ok()
                        .flatten()
                        .map(|a| a.name)
                        .unwrap_or_else(|| self.agent_id.clone());
                    let task = if session_id.is_empty() {
                        None
                    } else {
                        opencrab_db::queries::get_active_task_for_session(
                            &conn,
                            &self.agent_id,
                            session_id,
                        )
                        .ok()
                        .flatten()
                    };
                    (name, task)
                }
                Err(e) => {
                    warn!("request_peer_review: DB lock failed, sending without task info: {e}");
                    (self.agent_id.clone(), None)
                }
            }
        };

        let header = PeerReviewHeader {
            agent_name: &agent_name,
            task: task
                .as_ref()
                .map(|t| (t.id, t.goal.as_str(), t.contract.as_deref())),
            instructions,
        };
        let messages =
            build_peer_review_messages(&header, content, super::webhook::DISCORD_CHUNK_LIMIT);
        let total = messages.len();
        let parts = total - 1;

        for (i, message) in messages.iter().enumerate() {
            if let Err(e) = ChannelId::new(channel_id)
                .send_message(&self.http, CreateMessage::new().content(message))
                .await
            {
                error!(
                    "request_peer_review: send failed after {i}/{total} messages sent: {e}"
                );
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!(
                        "ピアレビュー依頼の送信に失敗（{i}/{total} 通送信済みの時点で失敗）: {e}。\
                         投稿済みの依頼は不完全です。チャンネルに取り消しの一言を送ってから、必要なら再依頼してください。"
                    )),
                };
            }
        }

        // 台帳へ記録（best-effort: 失敗しても送信成功は返す）
        let ledger_recorded = if let Some(task) = &task {
            let focus = instructions
                .map(|i| format!(" — focus: {i}"))
                .unwrap_or_default();
            match self.db.lock() {
                Ok(conn) => opencrab_db::queries::insert_task_progress(
                    &conn,
                    task.id,
                    "progress",
                    &format!(
                        "[peer review requested] posted to channel {channel_id} ({parts} parts){focus}"
                    ),
                )
                .map(|_| true)
                .unwrap_or_else(|e| {
                    warn!("request_peer_review: ledger record failed: {e}");
                    false
                }),
                Err(_) => false,
            }
        } else {
            false
        };

        GatewayActionResult {
            success: true,
            data: Some(json!({
                "channel_id": channel_id_str,
                "parts": parts,
                "task_id": task.as_ref().map(|t| t.id),
                "ledger_recorded": ledger_recorded,
                "message": "ピアレビュー依頼を投稿しました。[Peer Review] で始まる返信を待ってください。",
            })),
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway_actions::webhook::DISCORD_CHUNK_LIMIT;

    #[test]
    fn header_includes_task_and_instructions() {
        let header = PeerReviewHeader {
            agent_name: "crab-a",
            task: Some((12, "ship feature", Some("tests green"))),
            instructions: Some("check the error handling"),
        };
        let msgs = build_peer_review_messages(&header, "diff content", DISCORD_CHUNK_LIMIT);
        assert_eq!(msgs.len(), 2);
        let head = &msgs[0];
        assert!(head.starts_with("[Peer Review Request] from crab-a — task #12"));
        assert!(head.contains("goal: ship feature"));
        assert!(head.contains("contract: tests green"));
        assert!(head.contains("instructions: check the error handling"));
        assert!(head.contains("score: <0.0-1.0>"));
        assert!(head.contains("parts: 1"));
        assert_eq!(msgs[1], "part 1/1\ndiff content");
    }

    #[test]
    fn header_without_task_or_contract() {
        let header = PeerReviewHeader {
            agent_name: "crab-a",
            task: None,
            instructions: None,
        };
        let msgs = build_peer_review_messages(&header, "x", DISCORD_CHUNK_LIMIT);
        assert!(msgs[0].contains("no active task"));
        assert!(!msgs[0].contains("goal:"));
        assert!(!msgs[0].contains("instructions:"));

        // contract が空文字列なら contract 行は出ない
        let header = PeerReviewHeader {
            agent_name: "crab-a",
            task: Some((3, "g", Some("  "))),
            instructions: None,
        };
        let msgs = build_peer_review_messages(&header, "x", DISCORD_CHUNK_LIMIT);
        assert!(msgs[0].contains("task #3"));
        assert!(msgs[0].contains("goal: g"));
        assert!(!msgs[0].contains("contract:"));
    }

    #[test]
    fn header_stays_within_discord_limit_with_long_fields() {
        // goal 2000 / contract 4000 / instructions 無制限でもヘッダは1通(2000 chars)に収まる
        let goal = "g".repeat(2000);
        let contract = "c".repeat(4000);
        let instructions = "i".repeat(5000);
        let header = PeerReviewHeader {
            agent_name: "very-long-agent-name-agent",
            task: Some((99, goal.as_str(), Some(contract.as_str()))),
            instructions: Some(instructions.as_str()),
        };
        let msgs = build_peer_review_messages(&header, "x", DISCORD_CHUNK_LIMIT);
        assert!(
            msgs[0].chars().count() <= 2000,
            "header must fit one Discord message, got {}",
            msgs[0].chars().count()
        );
        // 切り詰めが起きていることの確認
        assert!(msgs[0].contains("…"));
    }

    #[test]
    fn long_japanese_content_chunks_losslessly() {
        let content = "日本語のレビュー対象コンテンツ。".repeat(300); // 4800 chars
        let header = PeerReviewHeader {
            agent_name: "a",
            task: None,
            instructions: None,
        };
        let msgs = build_peer_review_messages(&header, &content, DISCORD_CHUNK_LIMIT);
        let parts = &msgs[1..];
        assert!(parts.len() >= 3);
        // 各チャンクは limit + "part X/N\n" プレフィクス以内
        for (i, p) in parts.iter().enumerate() {
            let prefix = format!("part {}/{}\n", i + 1, parts.len());
            assert!(p.starts_with(&prefix));
            let body = &p[prefix.len()..];
            assert!(body.chars().count() <= DISCORD_CHUNK_LIMIT);
        }
        // 結合で原文復元（要約・切り詰めが無い）
        let reassembled: String = parts
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let prefix = format!("part {}/{}\n", i + 1, parts.len());
                p[prefix.len()..].to_string()
            })
            .collect();
        assert_eq!(reassembled, content);
        // ヘッダの parts 数が一致
        assert!(msgs[0].contains(&format!("parts: {}", parts.len())));
    }
}
