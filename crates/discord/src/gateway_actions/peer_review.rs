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
use opencrab_gateway::{
    GatewayActionResult, GatewayCallContext, PEER_REVIEW_REPLY_MARKER, PEER_REVIEW_REQUEST_MARKER,
};

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
    /// 指名レビュアーの Discord user id（メンション用）。
    pub mention: Option<u64>,
}

/// 1通目 = ヘッダ、2通目以降 = `part X/N` + RAW content（切り詰めない）。
pub(crate) fn build_peer_review_messages(
    header: &PeerReviewHeader<'_>,
    content: &str,
    limit: usize,
) -> Vec<String> {
    let parts = build_part_messages(content, limit);
    let part_count = parts.len();

    // ヘッダは1通（2000 chars 上限）に収める: 可変長フィールドは切り詰める。
    // メンションは marker の後ろに置く（レビュアー側の starts-with 判定を壊さない）。
    let mention = header
        .mention
        .map(|id| format!(" <@{id}>"))
        .unwrap_or_default();
    let mut head = String::new();
    match header.task {
        Some((task_id, _, _)) => head.push_str(&format!(
            "{PEER_REVIEW_REQUEST_MARKER}{mention} from {} — task #{task_id}\n",
            truncate_chars(header.agent_name, 100),
        )),
        None => head.push_str(&format!(
            "{PEER_REVIEW_REQUEST_MARKER}{mention} from {} — no active task\n",
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

/// `[Peer Review]` 返信のパース結果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PeerReviewVerdict {
    /// 0.0-1.0 に clamp 済み。抽出できなければ None。
    pub score: Option<f64>,
    pub gaps: Vec<String>,
    pub summary: String,
}

/// text 中で `[Peer Review]` marker が行頭（markdown 装飾は許容）に現れる位置を返す。
///
/// debounce がレビュアーの前置きと verdict を1メッセージに結合することがあるため、
/// 先頭だけでなく各行の行頭を見る。行の途中の言及（レビュー対象の diff 等）は無視する。
fn find_reply_marker(text: &str) -> Option<usize> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let stripped = line.trim_start_matches(|c: char| {
            c.is_whitespace() || c == '*' || c == '_' || c == '#' || c == '>'
        });
        if stripped.starts_with(PEER_REVIEW_REPLY_MARKER) {
            return Some(offset + (line.len() - stripped.len()));
        }
        offset += line.len();
    }
    None
}

/// `[Peer Review]` を行頭に含むメッセージから score / gaps / summary を抽出する。
///
/// レビュアーは LLM なので形式ゆれに寛容にパースする（フィールド欠落でも Some を返す）。
/// marker を行頭に含まないメッセージは None。
pub(crate) fn parse_peer_review_reply(text: &str) -> Option<PeerReviewVerdict> {
    let marker_pos = find_reply_marker(text)?;
    let body = &text[marker_pos + PEER_REVIEW_REPLY_MARKER.len()..];
    let lower = body.to_ascii_lowercase();

    // フィールドキーはコロン必須で照合する（"no gaps found" のような本文中の
    // 単語をフィールド開始と誤認して gaps を捏造しないため）
    // score: の後の最初の数値（"0.8", "0.8/1.0", "0.8 (…)" 等の先頭数値を拾う）
    let score = lower.find("score:").and_then(|pos| {
        let after = &body[pos + "score:".len()..];
        let after = after.trim_start();
        let num: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        num.parse::<f64>().ok().map(|v| v.clamp(0.0, 1.0))
    });

    // gaps: から summary:（または末尾）まで
    let gaps = match lower.find("gaps:") {
        Some(pos) => {
            let after = &body[pos + "gaps:".len()..];
            let after = after.trim_start_matches(' ');
            let end = after
                .to_ascii_lowercase()
                .find("summary:")
                .unwrap_or(after.len());
            // インライン形式（"Gaps: none, Summary: ..."）の区切りカンマ等を落とす
            let strip = |s: &str| {
                s.trim_matches(|c: char| c.is_whitespace() || c == ',' || c == ';')
                    .to_string()
            };
            let section = strip(&after[..end]);
            if section.eq_ignore_ascii_case("none") || section.is_empty() {
                Vec::new()
            } else {
                // "- x" 行のリスト、または改行区切りのインライン
                let items: Vec<String> = section
                    .lines()
                    .map(|l| strip(l.trim().trim_start_matches('-')))
                    .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("none"))
                    .collect();
                items
            }
        }
        None => Vec::new(),
    };

    // summary: の後（無ければ本文先頭 200 chars をフォールバック）
    let summary = match lower.find("summary:") {
        Some(pos) => {
            let after = &body[pos + "summary:".len()..];
            after.trim().to_string()
        }
        None => truncate_chars(body.trim(), 200),
    };

    Some(PeerReviewVerdict {
        score,
        gaps,
        summary,
    })
}

/// reviewer 指定（display_name または Discord user id）を Discord user id に解決する。
///
/// **登録済みの co_agent のみ**解決する: 表示名一致を先に見て（数値の表示名も扱える）、
/// 次に id 一致。未登録の任意 id は受け付けない（LLM の幻覚 id によるゴーストメンション防止）。
/// 未解決の場合は Err に登録済みレビュアーの一覧文字列を返す。
pub(crate) fn resolve_reviewer(
    conn: &rusqlite::Connection,
    agent_id: &str,
    reviewer: &str,
) -> Result<u64, String> {
    let reviewer = reviewer.trim();
    let co_agents = match opencrab_db::queries::list_co_agent_reviewers(conn, agent_id) {
        Ok(rows) => rows,
        Err(e) => {
            warn!("resolve_reviewer: roster query failed: {e}");
            return Err(
                "(レビュアー一覧の取得に失敗しました — 後で再試行してください)".to_string(),
            );
        }
    };
    // 表示名一致を優先（数値の表示名が id 解釈に食われないように）
    if let Some(matched) = co_agents
        .iter()
        .find(|u| !u.display_name.is_empty() && u.display_name.eq_ignore_ascii_case(reviewer))
    {
        if let Ok(id) = matched.discord_user_id.parse::<u64>() {
            return Ok(id);
        }
    }
    // `<@123>` / `123` 形式は登録済み id とのみ照合
    let bare = reviewer
        .trim_start_matches("<@")
        .trim_end_matches('>')
        .trim();
    if bare.parse::<u64>().is_ok() {
        if let Some(matched) = co_agents.iter().find(|u| u.discord_user_id == bare) {
            if let Ok(id) = matched.discord_user_id.parse::<u64>() {
                return Ok(id);
            }
        }
    }
    let available = if co_agents.is_empty() {
        "(なし — trusted-users API で permission=co_agent + display_name を登録してください)"
            .to_string()
    } else {
        co_agents
            .iter()
            .map(|u| {
                if u.display_name.is_empty() {
                    u.discord_user_id.clone()
                } else {
                    format!("{} (<@{}>)", u.display_name, u.discord_user_id)
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    Err(available)
}

/// パース済み verdict をタスク台帳の progress 文字列に整形する。
pub(crate) fn format_peer_review_progress(verdict: &PeerReviewVerdict, reviewer: &str) -> String {
    let score = verdict
        .score
        .map(|s| format!("{s:.2}"))
        .unwrap_or_else(|| "n/a".to_string());
    let gaps = if verdict.gaps.is_empty() {
        "none".to_string()
    } else {
        truncate_chars(&verdict.gaps.join("; "), 800)
    };
    format!(
        "[peer review] score {score} (from {reviewer}): {}; gaps: {gaps}",
        truncate_chars(&verdict.summary, 300),
    )
}

/// active タスクに「未回収のレビュー依頼」があるか判定する。
///
/// 直近の進捗を新しい順に見て、`[peer review]`（受領記録）より後に
/// `[peer review requested]` があれば未回収。これにより:
/// - 依頼していないタスクには第三者間のレビューが記録されない
///   （同一チャンネルの別 bot 同士のレビューを誤記録しない）
/// - 1依頼につき1件だけ記録される（同文の連投は2件目以降スキップ）
fn has_outstanding_review_request(conn: &rusqlite::Connection, task_id: i64) -> bool {
    let recent =
        opencrab_db::queries::list_recent_task_progress(conn, task_id, 30).unwrap_or_default();
    for entry in recent.iter().rev() {
        if entry.content.starts_with("[peer review requested]") {
            return true;
        }
        if entry.content.starts_with("[peer review]") {
            return false;
        }
    }
    false
}

/// 受信した `[Peer Review]` 返信を requester の active タスクへ自動記録する（#58）。
///
/// ゲート（すべて満たす場合のみ記録）:
/// 1. marker が行頭にある
/// 2. 送信者がこのエージェントの登録済み co_agent（第三者・未信頼の偽 verdict を排除）
/// 3. active タスクに未回収のレビュー依頼がある（依頼していないレビューを誤記録しない）
///
/// 記録は追加処理: メッセージ自体はこの後通常どおり LLM にも流れる（会話には speech として残る）。
/// session_logs には重ねて記録しない（二重描画を避ける。台帳経由で次ターンの [Task Ledger] に出る）。
/// 記録した場合 true を返す。
pub(crate) fn record_peer_review_reply(
    db: &opencrab_db::Db,
    agent_id: &str,
    session_id: &str,
    sender_id: &str,
    sender_name: &str,
    text: &str,
) -> bool {
    let Some(verdict) = parse_peer_review_reply(text) else {
        return false;
    };
    let Ok(conn) = db.lock() else {
        warn!("record_peer_review_reply: DB lock failed, review not recorded");
        return false;
    };
    // 送信者ゲート: 登録済み co_agent のみ
    let is_co_agent = opencrab_db::queries::get_trusted_user(&conn, sender_id, agent_id)
        .map(|u| u.permission == "co_agent")
        .unwrap_or(false);
    if !is_co_agent {
        tracing::debug!(
            agent_id = %agent_id,
            sender_id = %sender_id,
            "peer review reply from non-co_agent sender — skipping auto-record"
        );
        return false;
    }
    let Some(task) = opencrab_db::queries::get_active_task_for_session(&conn, agent_id, session_id)
        .ok()
        .flatten()
    else {
        tracing::debug!(
            agent_id = %agent_id,
            session_id = %session_id,
            "peer review reply received but no active task — skipping auto-record"
        );
        return false;
    };
    if !has_outstanding_review_request(&conn, task.id) {
        tracing::debug!(
            agent_id = %agent_id,
            task_id = task.id,
            "peer review reply but no outstanding request on active task — skipping auto-record"
        );
        return false;
    }
    let content = format_peer_review_progress(&verdict, sender_name);
    match opencrab_db::queries::insert_task_progress(&conn, task.id, "progress", &content) {
        Ok(_) => {
            tracing::info!(
                agent_id = %agent_id,
                task_id = task.id,
                score = ?verdict.score,
                reviewer = %sender_name,
                "peer review reply auto-recorded to task ledger"
            );
            true
        }
        Err(e) => {
            warn!("record_peer_review_reply: ledger record failed: {e}");
            false
        }
    }
}

impl DiscordGatewayActions {
    pub(crate) async fn execute_request_peer_review(
        &self,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // セッション必須（fail-closed）: 台帳記録・返信回収がセッションに紐づくため、
        // セッション文脈の無い実行は "" で黙って進まず明示エラーにする（#36）。
        let session_id =
            match ctx.session_id.as_deref() {
                Some(s) if !s.is_empty() => s,
                _ => return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(
                        "request_peer_review はセッション文脈でのみ実行できます（session_id 不明）"
                            .to_string(),
                    ),
                },
            };
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) if !c.trim().is_empty() => c,
            _ => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(
                        "contentパラメータが必要です（レビュー対象のRAWコンテンツ）".to_string(),
                    ),
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
        let reviewer = args
            .get("reviewer")
            .and_then(|v| v.as_str())
            .filter(|r| !r.trim().is_empty());
        // agent 表示名・active タスク・レビュアー解決を1ロックスコープで（await 前に drop）
        let (agent_name, task, mention) = {
            match self.db.lock() {
                Ok(conn) => {
                    let name = opencrab_db::queries::get_agent(&conn, &ctx.agent_id)
                        .ok()
                        .flatten()
                        .map(|a| a.name)
                        .unwrap_or_else(|| ctx.agent_id.clone());
                    let task = {
                        opencrab_db::queries::get_active_task_for_session(
                            &conn,
                            &ctx.agent_id,
                            session_id,
                        )
                        .ok()
                        .flatten()
                    };
                    // reviewer 解決: 数値なら user id、それ以外は登録済み co_agent の
                    // display_name 一致（大文字小文字無視）
                    let mention = match reviewer {
                        None => None,
                        Some(r) => match resolve_reviewer(&conn, &ctx.agent_id, r) {
                            Ok(id) => Some(id),
                            Err(available) => {
                                return GatewayActionResult {
                                    success: false,
                                    data: None,
                                    error: Some(format!(
                                        "reviewer '{r}' が見つかりません。登録済みのピアレビュアー: {available}"
                                    )),
                                }
                            }
                        },
                    };
                    (name, task, mention)
                }
                Err(e) => {
                    warn!("request_peer_review: DB lock failed, sending without task info: {e}");
                    (ctx.agent_id.clone(), None, None)
                }
            }
        };

        let header = PeerReviewHeader {
            agent_name: &agent_name,
            task: task
                .as_ref()
                .map(|t| (t.id, t.goal.as_str(), t.contract.as_deref())),
            instructions,
            mention,
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
                error!("request_peer_review: send failed after {i}/{total} messages sent: {e}");
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
            mention: None,
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
            mention: None,
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
            mention: None,
        };
        let msgs = build_peer_review_messages(&header, "x", DISCORD_CHUNK_LIMIT);
        assert!(msgs[0].contains("task #3"));
        assert!(msgs[0].contains("goal: g"));
        assert!(!msgs[0].contains("contract:"));
    }

    #[test]
    fn header_includes_mention_after_marker() {
        let header = PeerReviewHeader {
            agent_name: "a",
            task: None,
            instructions: None,
            mention: Some(1234567890),
        };
        let msgs = build_peer_review_messages(&header, "x", DISCORD_CHUNK_LIMIT);
        // starts-with 判定を壊さないよう、メンションは marker の後ろ
        assert!(msgs[0].starts_with("[Peer Review Request] <@1234567890> from a"));
    }

    #[test]
    fn parse_reply_full_form() {
        let v = parse_peer_review_reply(
            "[Peer Review] score: 0.75\ngaps:\n- tests not run\n- no error handling\nsummary: solid but unverified",
        )
        .unwrap();
        assert_eq!(v.score, Some(0.75));
        assert_eq!(v.gaps, vec!["tests not run", "no error handling"]);
        assert_eq!(v.summary, "solid but unverified");
    }

    #[test]
    fn parse_reply_inline_and_variants() {
        // インライン gaps、score にスラッシュ形式、大文字
        let v = parse_peer_review_reply(
            "  [Peer Review] Score: 0.9/1.0, Gaps: none, Summary: looks good",
        )
        .unwrap();
        assert_eq!(v.score, Some(0.9));
        assert!(v.gaps.is_empty());
        assert_eq!(v.summary, "looks good");

        // score 欠落 → None、summary 欠落 → 本文フォールバック
        let v = parse_peer_review_reply("[Peer Review] this looks fine to me").unwrap();
        assert_eq!(v.score, None);
        assert!(v.summary.contains("looks fine"));

        // 1.0 超は clamp
        let v = parse_peer_review_reply("[Peer Review] score: 8.5 summary: s").unwrap();
        assert_eq!(v.score, Some(1.0));
    }

    #[test]
    fn parse_reply_rejects_non_marker() {
        assert!(parse_peer_review_reply("just chatting about [Peer Review] stuff").is_none());
        assert!(parse_peer_review_reply("[Peer Review Request] from a").is_none());
    }

    #[test]
    fn parse_reply_finds_marker_after_preamble_and_markdown() {
        // debounce がレビュアーの前置きと verdict を結合するケース
        let v = parse_peer_review_reply(
            "Looking at it now.\n[Peer Review] score: 0.8, gaps: none, summary: fine",
        )
        .unwrap();
        assert_eq!(v.score, Some(0.8));

        // markdown 装飾付き行頭
        let v = parse_peer_review_reply("**[Peer Review]** score: 0.5 summary: hm").unwrap();
        assert_eq!(v.score, Some(0.5));

        // 行の途中の言及は依然として無視
        assert!(parse_peer_review_reply("the diff mentions [Peer Review] in prose").is_none());

        // 本文中の "gaps" という単語をフィールド開始と誤認しない（コロン必須）
        let v = parse_peer_review_reply("[Peer Review] score: 1.0 summary: no gaps found").unwrap();
        assert!(v.gaps.is_empty());
        assert_eq!(v.summary, "no gaps found");
    }

    #[test]
    fn format_progress_bounds_and_labels() {
        let v = PeerReviewVerdict {
            score: Some(0.4),
            gaps: vec!["a".repeat(600), "b".to_string()],
            summary: "needs work".to_string(),
        };
        let s = format_peer_review_progress(&v, "crab-b");
        assert!(s.starts_with("[peer review] score 0.40 (from crab-b): needs work"));
        assert!(s.chars().count() < 1300, "progress entry must stay bounded");

        let v = PeerReviewVerdict {
            score: None,
            gaps: vec![],
            summary: "s".to_string(),
        };
        assert!(format_peer_review_progress(&v, "r").contains("score n/a"));
    }

    #[test]
    fn resolve_reviewer_registered_only() {
        let conn = opencrab_db::init_memory().unwrap();
        opencrab_db::queries::add_trusted_user(
            &conn,
            "row-1",
            "agent-1",
            "42",
            "co_agent",
            "owner",
            "2026-01-01",
            "Crab B",
        )
        .unwrap();
        // 数値の display_name（id 解釈に食われないこと）
        opencrab_db::queries::add_trusted_user(
            &conn,
            "row-2",
            "agent-1",
            "77",
            "co_agent",
            "owner",
            "2026-01-01",
            "2026",
        )
        .unwrap();
        // co_agent でない行はロスター外
        opencrab_db::queries::add_trusted_user(
            &conn,
            "row-3",
            "agent-1",
            "44",
            "trusted_user",
            "owner",
            "2026-01-01",
            "Human",
        )
        .unwrap();

        // display_name 一致（大文字小文字無視）が最優先
        assert_eq!(resolve_reviewer(&conn, "agent-1", "crab b").unwrap(), 42);
        assert_eq!(resolve_reviewer(&conn, "agent-1", "2026").unwrap(), 77);
        // 登録済み id / <@id> 形式
        assert_eq!(resolve_reviewer(&conn, "agent-1", "42").unwrap(), 42);
        assert_eq!(resolve_reviewer(&conn, "agent-1", "<@42>").unwrap(), 42);
        // 未登録の任意 id は拒否（幻覚 id のゴーストメンション防止）
        let err = resolve_reviewer(&conn, "agent-1", "999").unwrap_err();
        assert!(err.contains("Crab B"));
        // 非 co_agent はロスター外
        let err = resolve_reviewer(&conn, "agent-1", "Human").unwrap_err();
        assert!(err.contains("Crab B"));
        assert!(!err.contains("Human"));
    }

    #[test]
    fn record_reply_gates_and_writes() {
        let db = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        let task_id = {
            let conn = db.lock().unwrap();
            // 送信者 "42" をこのエージェントの co_agent として登録
            opencrab_db::queries::add_trusted_user(
                &conn,
                "row-1",
                "a1",
                "42",
                "co_agent",
                "owner",
                "2026-01-01",
                "Crab B",
            )
            .unwrap();
            let task_id =
                opencrab_db::queries::insert_task_ledger(&conn, "a1", "s1", "goal", None).unwrap();
            // 未回収のレビュー依頼を記録
            opencrab_db::queries::insert_task_progress(
                &conn,
                task_id,
                "progress",
                "[peer review requested] posted to channel 1 (1 parts)",
            )
            .unwrap();
            task_id
        };
        let reply = "[Peer Review] score: 0.6\ngaps:\n- missing tests\nsummary: incomplete";

        // marker 無し → 記録しない
        assert!(!record_peer_review_reply(
            &db, "a1", "s1", "42", "crab-b", "hello"
        ));
        // 未登録送信者（co_agent でない）→ 記録しない
        assert!(!record_peer_review_reply(
            &db, "a1", "s1", "99", "stranger", reply
        ));
        // active タスクの無いセッション → 記録しない
        assert!(!record_peer_review_reply(
            &db, "a1", "other", "42", "crab-b", reply
        ));
        // 正常系
        assert!(record_peer_review_reply(
            &db, "a1", "s1", "42", "crab-b", reply
        ));
        // 依頼が回収済みになったので、追加の返信は記録しない（1依頼1記録）
        assert!(!record_peer_review_reply(
            &db, "a1", "s1", "42", "crab-b", reply
        ));

        let conn = db.lock().unwrap();
        let progress = opencrab_db::queries::list_recent_task_progress(&conn, task_id, 10).unwrap();
        assert_eq!(progress.len(), 2); // requested + received
        assert!(progress[1]
            .content
            .contains("[peer review] score 0.60 (from crab-b)"));
        assert!(progress[1].content.contains("missing tests"));
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
            mention: None,
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
            mention: None,
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
