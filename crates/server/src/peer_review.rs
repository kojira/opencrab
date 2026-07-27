//! ピアレビュー依頼ツール `request_peer_review` の gateway 非依存な実体（#157 S7）。
//!
//! LOOPS 原則 II（自己採点させるな / #49 phase 2）の機構が Discord gateway
//! （`crates/discord/src/gateway_actions/peer_review.rs`）にしか無かったため、Discord 経由の
//! ターンでしか露出しなかった（#157 の残件）。ここへ移すことで、素テキストの配送口
//! （[`TextDelivery`]）を提供する transport すべてで同じ実装が使える。
//!
//! 自分の成果物（diff / 出力 / トレース）を **要約せず RAW のまま** 投稿し、別ベクトルの
//! bot にレビューを依頼する。レビュアー側の応答規約（`[Peer Review Request]` には
//! NO_REPLY せず `[Peer Review]` で応答する）は system prompt
//! （`crate::process::build_agent_context`）に定義されている。
//!
//! ## transport に残したもの
//! [`TextDelivery`] の 4 メソッドだけ: 宛先トークンの検査 / メンション記法 /
//! 1 通の上限 / 送信そのもの。**分割の仕方と部分失敗の勘定は汎用層に残す**
//! （抽象越しにすると「N/M 通送信済み」が失われやすいため意図的にこちら側）。
//!
//! ## 返信の回収（#156 S4 でここへ合流）
//! `[Peer Review]` 返信の解析と台帳への自動記録は、以前は Discord gateway 側にあり
//! Discord の受信ループ 1 箇所からしか呼ばれなかった。共通の受信フック
//! （[`opencrab_actions::AgentRuntime::on_inbound_message`]）ができたので、このファイルの
//! 後半（[`harvest_inbound_reply`]）へ移設した。依頼が書く目印と回収が探す目印が
//! 同じファイルで突き合わせられる。
//!
//! ## 不変条件（移設で壊してはならないもの）
//! - **セッション必須（fail-closed）**: `session_id` が無い/空なら明示エラー（#36）。
//! - **幻覚 id への誤送信防止**: レビュアーは**登録済みの co_agent のみ**から解決し、
//!   未登録の任意 id は拒否する（[`resolve_reviewer`]）。
//! - **本文の長さ上限**: [`MAX_REVIEW_CONTENT_CHARS`]。
//! - **分割送信の途中失敗を明示**: 「N/M 通送信済み」を error 文言に載せる。
//! - **レスポンス JSON のキーと全エラー文言は移設前と 1 バイトも変えない**
//!   （リテラルで固定するテストがこのファイルの末尾にある）。

use opencrab_core::llm_text::truncate_chars;
use opencrab_core::text_delivery::TextDelivery;
use opencrab_gateway::{
    GatewayActionDef, GatewayActionResult, GatewayCallContext, PEER_REVIEW_REPLY_MARKER,
    PEER_REVIEW_REQUEST_MARKER,
};
use serde_json::json;
use tracing::{error, warn};

use opencrab_actions::{build_part_messages, InboundMessageRecord, TranscriptSource};

/// content の上限（chars）。配送先のレート制限（Discord なら ~5通/5秒/チャンネル）の
/// 1ウィンドウに収まる分割数（ヘッダ+6 part 程度）に抑える。超える場合はワークスペースに
/// 保存して discord_send_file を使う。
pub const MAX_REVIEW_CONTENT_CHARS: usize = 12_000;

/// ヘッダに描画する goal / contract / instructions の上限（chars）。
/// ヘッダは1通に収める必要がある（Discord 上限 2000 chars）ため、フィールドを切り詰める。
/// 全文はレビュー対象の content 側や台帳にあるので、ここは案内で足りる。
const HEADER_FIELD_MAX_CHARS: usize = 300;

/// `request_peer_review` のツール定義。
///
/// 名前・引数スキーマ・description は移設前（Discord gateway、#158 S2 適用後）から
/// 1 バイトも変えない（文言の見直しは移設の範囲外 — 変えるなら別 issue）。
///
/// **description に transport 前提を書き戻さないこと**（#158 S2 / #218）。移設で
/// この定義は Nostr / web / REST / 定期実行の全ターンに露出するため、`[Discord context]`
/// のような存在しない文脈を参照させると幻覚宛先への誤投稿を招く。同様にレビュアーは
/// **表示名のみ**を渡させる（メンション記法の組み立ては transport の責務）。
/// 再発防止は `definition_is_stable` の transport 中立チェックが担う。
pub fn request_peer_review_definition() -> GatewayActionDef {
    GatewayActionDef {
        name: "request_peer_review".to_string(),
        description: "自分の成果物（diff・実行結果・トレース等）を、同じチャンネルにいる別のBot（別モデル）にピアレビューしてもらうため、レビュー依頼をDiscordチャンネルへ投稿する。contentは要約せずRAWのまま part X/N で分割送信される。レビュアーは [Peer Review] で始まる返信（score 0.0-1.0 / gaps / summary）を返す想定。activeタスクがあればタスク台帳に [peer review requested] を自動記録する。".to_string(),
        parameters: json!({
            "type": "object",
            "required": ["content"],
            "properties": {
                "content": {
                    "type": "string",
                    "description": "レビュー対象のRAWコンテンツ（diff・出力・トレース等）。要約せずそのまま渡すこと。上限12000文字（超える場合はワークスペースに保存してdiscord_send_fileで添付する）。"
                },
                "channel_id": {
                    "type": "string",
                    "description": "投稿先の宛先ID（省略可）。**通常は省略する** — 省略時は今のやりとりの返信先へ投稿される。今の会話とは別の宛先へ送りたいときだけ指定すること（推測した識別子を渡してはならない）。"
                },
                "instructions": {
                    "type": "string",
                    "description": "レビュアーに重点的に見てほしい観点（省略可）。"
                },
                "reviewer": {
                    "type": "string",
                    "description": "指名したいレビュアー（省略可）。システムプロンプトの Peer Reviewers 一覧にある表示名を渡す。指定するとヘッダにメンションが付く。"
                }
            }
        }),
    }
}

/// レビュー依頼ヘッダの構成要素。
pub struct PeerReviewHeader<'a> {
    pub agent_name: &'a str,
    /// (task_id, goal, contract)
    pub task: Option<(i64, &'a str, Option<&'a str>)>,
    pub instructions: Option<&'a str>,
    /// 指名レビュアーへのメンション（**transport の記法で組んだ文字列**）。
    /// 記法そのものは [`TextDelivery::mention`] が持つ（汎用層は `<@id>` を知らない）。
    pub mention: Option<&'a str>,
}

/// 1通目 = ヘッダ、2通目以降 = `part X/N` + RAW content（切り詰めない）。
pub fn build_peer_review_messages(
    header: &PeerReviewHeader<'_>,
    content: &str,
    limit: usize,
) -> Vec<String> {
    let parts = build_part_messages(content, limit);
    let part_count = parts.len();

    // ヘッダは1通（2000 chars 上限）に収める: 可変長フィールドは切り詰める。
    // メンションは marker の後ろに置く（レビュアー側の starts-with 判定を壊さない）。
    let mention = header.mention.map(|m| format!(" {m}")).unwrap_or_default();
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

/// reviewer 指定（display_name または transport のユーザー id）を id 文字列に解決する。
///
/// **登録済みの co_agent のみ**解決する: 表示名一致を先に見て（数値の表示名も扱える）、
/// 次に id 一致。未登録の任意 id は受け付けない（LLM の幻覚 id によるゴーストメンション防止）。
/// 未解決の場合は Err に登録済みレビュアーの一覧文字列を返す。
///
/// 名簿のキー空間（`trusted_users.user_id` と `<@id>` 形式の受理、および
/// 「数値としてパースできる id だけを採る」判定）は **#214 の担当なので現状維持**。
/// 返り値はパース済み数値の文字列表現で、移設前（`u64` を返して呼び出し側が
/// `<@{id}>` に埋めていた）とバイト単位で同じメンションになる。
pub fn resolve_reviewer(
    conn: &rusqlite::Connection,
    delivery: &dyn TextDelivery,
    agent_id: &str,
    reviewer: &str,
) -> Result<String, String> {
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
        if let Ok(id) = matched.user_id.parse::<u64>() {
            return Ok(id.to_string());
        }
    }
    // `<@123>` / `123` 形式は登録済み id とのみ照合
    let bare = reviewer
        .trim_start_matches("<@")
        .trim_end_matches('>')
        .trim();
    if bare.parse::<u64>().is_ok() {
        if let Some(matched) = co_agents.iter().find(|u| u.user_id == bare) {
            if let Ok(id) = matched.user_id.parse::<u64>() {
                return Ok(id.to_string());
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
                    u.user_id.clone()
                } else {
                    // メンション記法は transport の責務（汎用層は `<@…>` を組まない）。
                    format!("{} ({})", u.display_name, delivery.mention(&u.user_id))
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    Err(available)
}

/// ピアレビュー依頼の投稿先を解決する（#158 S1）。
///
/// 引数 `channel_id` が最優先。未指定（または空）なら実行文脈の返信先
/// （`GatewayCallContext.reply_target` = gateway 不透明 token。Discord では channel id の
/// 数値文字列）へフォールバックする。**両方無ければ空文字で送らず明示エラー**
/// （fail-closed）。宛先を明示した呼び出しの挙動は従来どおり（既定値が増えるだけ）。
///
/// 引数が JSON 数値でも受け付ける: 移設前は Discord gateway の `normalize_id_args` が
/// 実行直前に `*_id` の整数を文字列化していたため、モデルが `channel_id: 123` と
/// 渡しても通っていた。合成 gateway にはその正規化が無いので、ここで同じ吸収を行う。
fn resolve_review_channel(
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> Result<String, String> {
    if let Some(id) = args.get("channel_id").and_then(id_arg_to_string) {
        return Ok(id);
    }
    ctx.reply_target
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.to_string())
        .ok_or_else(|| "channel_idパラメータが必要です（実行文脈に返信先がありません）".to_string())
}

/// ID 引数を文字列として読む（空文字/空白は未指定として扱う）。
///
/// JSON 数値は精度を保ったまま文字列化する（スノーフレークは 2^53 超で f64 では壊れるが、
/// serde_json は整数リテラルを u64/i64 で保持する）。
fn id_arg_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        serde_json::Value::Number(_) => v
            .as_u64()
            .map(|u| u.to_string())
            .or_else(|| v.as_i64().map(|i| i.to_string())),
        _ => None,
    }
}

fn fail(error: String) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(error),
    }
}

/// `request_peer_review` の実体（gateway 非依存）。
///
/// 手順は移設前と同一: セッション検査 → content 検査 → 長さ検査 → 宛先解決 → 宛先検査
/// → 表示名/タスク/レビュアー解決（1 ロックスコープ）→ 分割送信 → 台帳記録。
pub async fn request_peer_review(
    db: &opencrab_db::Db,
    delivery: &dyn TextDelivery,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    // セッション必須（fail-closed）: 台帳記録・返信回収がセッションに紐づくため、
    // セッション文脈の無い実行は "" で黙って進まず明示エラーにする（#36）。
    let session_id = match ctx.session_id.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => {
            return fail(
                "request_peer_review はセッション文脈でのみ実行できます（session_id 不明）"
                    .to_string(),
            )
        }
    };
    let content = match args.get("content").and_then(|v| v.as_str()) {
        Some(c) if !c.trim().is_empty() => c,
        _ => return fail("contentパラメータが必要です（レビュー対象のRAWコンテンツ）".to_string()),
    };
    if content.chars().count() > MAX_REVIEW_CONTENT_CHARS {
        return fail(format!(
            "contentが{MAX_REVIEW_CONTENT_CHARS}文字を超えています — ワークスペースにファイルとして保存し discord_send_file で添付した上で、contentには要点とファイル名を書いてください"
        ));
    }
    let target = match resolve_review_channel(args, ctx) {
        Ok(id) => id,
        Err(error) => return fail(error),
    };
    // 宛先トークンの妥当性は transport が判定する（Discord なら数値スノーフレーク）。
    if let Err(error) = delivery.validate_target(&target) {
        return fail(error);
    }
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
        match db.lock() {
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
                    Some(r) => match resolve_reviewer(&conn, delivery, &ctx.agent_id, r) {
                        Ok(id) => Some(delivery.mention(&id)),
                        Err(available) => {
                            return fail(format!(
                                "reviewer '{r}' が見つかりません。登録済みのピアレビュアー: {available}"
                            ))
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
        mention: mention.as_deref(),
    };
    let messages = build_peer_review_messages(&header, content, delivery.chunk_limit());
    let total = messages.len();
    let parts = total - 1;

    for (i, message) in messages.iter().enumerate() {
        if let Err(e) = delivery.send_text(&target, message).await {
            error!("request_peer_review: send failed after {i}/{total} messages sent: {e}");
            return fail(format!(
                "ピアレビュー依頼の送信に失敗（{i}/{total} 通送信済みの時点で失敗）: {e}。\
                 投稿済みの依頼は不完全です。チャンネルに取り消しの一言を送ってから、必要なら再依頼してください。"
            ));
        }
    }

    // 台帳へ記録（best-effort: 失敗しても送信成功は返す）
    let ledger_recorded = if let Some(task) = &task {
        let focus = instructions
            .map(|i| format!(" — focus: {i}"))
            .unwrap_or_default();
        match db.lock() {
            Ok(conn) => opencrab_db::queries::insert_task_progress(
                &conn,
                task.id,
                "progress",
                &format!(
                    "[peer review requested] posted to channel {target} ({parts} parts){focus}"
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
            "channel_id": target,
            "parts": parts,
            "task_id": task.as_ref().map(|t| t.id),
            "ledger_recorded": ledger_recorded,
            "message": "ピアレビュー依頼を投稿しました。[Peer Review] で始まる返信を待ってください。",
        })),
        error: None,
    }
}

// ===========================================================================
// 返信の回収（#156 S4 で `crates/discord/src/gateway_actions/peer_review.rs` から移設）
// ===========================================================================
//
// 解析・3 つのゲート・台帳への記録は元から transport のライブラリ依存がゼロで、
// Discord に置かれていた理由は「呼び出し口が Discord の受信ループしか無かった」こと
// だけだった。共通の受信フック（`AgentRuntime::on_inbound_message`）ができたので、
// 依頼側（上）と**同じファイル**へ置く。目印の噛み合わせ（依頼が書くものと回収が
// 探すもの）が 1 ファイルで読めるようにするため:
// - 依頼が投稿する本文の先頭   … [`PEER_REVIEW_REQUEST_MARKER`]（`[Peer Review Request]`）
// - 依頼が台帳へ書く進捗       … `[peer review requested] ...`（`post_peer_review` 内）
// - 回収が本文に探す目印       … [`PEER_REVIEW_REPLY_MARKER`]（`[Peer Review]`）
// - 回収が台帳へ書く進捗       … `[peer review] score ...`（`format_peer_review_progress`）
//   → 未回収判定は「`[peer review requested]` が `[peer review]` より新しいか」。
//     `[Peer Review Request]` は `[Peer Review]` で始まらない（`]` の位置が違う）ため、
//     依頼メッセージ自体が返信として回収されることはない（テストで固定）。

/// `[Peer Review]` 返信のパース結果。
#[derive(Debug, Clone, PartialEq)]
struct PeerReviewVerdict {
    /// 0.0-1.0 に clamp 済み。抽出できなければ None。
    score: Option<f64>,
    gaps: Vec<String>,
    summary: String,
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
fn parse_peer_review_reply(text: &str) -> Option<PeerReviewVerdict> {
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

/// パース済み verdict をタスク台帳の progress 文字列に整形する。
fn format_peer_review_progress(verdict: &PeerReviewVerdict, reviewer: &str) -> String {
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

/// 受信フック（[`opencrab_actions::AgentRuntime::on_inbound_message`]）の購読者本体。
///
/// 受信メッセージがピアレビュー返信なら requester の台帳へ回収する。返信でなければ
/// 何もしない（受信 1 件につき 1 回呼ばれる前提の best-effort）。
///
/// 送信者識別子がどの経路の空間に属するかは [`TranscriptSource`] から引き、**経路の
/// 列を持たない由来では回収しない**（fail-closed）。ここを「とりあえず discord で引く」
/// にすると、別経路の識別子が偶然一致した相手の verdict を受理してしまう。経路ごとの
/// キー空間の分離自体は #159 の残作業。
pub(crate) fn harvest_inbound_reply(
    db: &opencrab_db::Db,
    source: TranscriptSource,
    agent_id: &str,
    record: &InboundMessageRecord<'_>,
) -> bool {
    let Some(platform) = trusted_platform_for(source) else {
        tracing::debug!(
            agent_id = %agent_id,
            "inbound hook: 信頼済みユーザーの経路が未定義の由来 — ピアレビュー回収をスキップ (#159)"
        );
        return false;
    };
    record_peer_review_reply(
        db,
        platform,
        agent_id,
        record.session_id,
        record.sender_id,
        record.sender_name,
        record.text,
    )
}

/// 受信の由来を、信頼済みユーザー表（`trusted_users.platform`）のキー空間へ対応づける。
///
/// Nostr は送信者識別子（pubkey）を信頼済みユーザー表の経路として持たない（#159 の
/// 残作業）。対応が無い由来は `None` を返し、回収させない。
fn trusted_platform_for(source: TranscriptSource) -> Option<&'static str> {
    match source {
        TranscriptSource::Discord => Some(opencrab_db::queries::TRUSTED_PLATFORM_DISCORD),
        TranscriptSource::Nostr => None,
    }
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
fn record_peer_review_reply(
    db: &opencrab_db::Db,
    platform: &str,
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
    // 送信者ゲート: 登録済み co_agent のみ。sender_id を申告した経路の空間で引く
    // （#214 で入った platform 列。経路の分離の残りは #159）。
    let is_co_agent = opencrab_db::queries::get_trusted_user(&conn, platform, sender_id, agent_id)
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use opencrab_gateway::GatewayCaller;
    use std::sync::Mutex;

    /// Discord の配送口と同じ規約（数値宛先 / `<@id>` / 1900 chars）を持つフェイク。
    /// 送信を記録するだけで、`fail_at` を指定すると N 通目で失敗する。
    struct FakeDelivery {
        sent: Mutex<Vec<(String, String)>>,
        /// 0-origin の添字。この通数目の送信で失敗させる。
        fail_at: Option<usize>,
    }

    impl FakeDelivery {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                fail_at: None,
            }
        }
        fn failing_at(i: usize) -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                fail_at: Some(i),
            }
        }
        fn count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl TextDelivery for FakeDelivery {
        fn validate_target(&self, target: &str) -> Result<(), String> {
            if target.parse::<u64>().is_ok() {
                Ok(())
            } else {
                Err(format!("無効なchannel_id: {target}"))
            }
        }
        fn mention(&self, user_id: &str) -> String {
            format!("<@{user_id}>")
        }
        fn chunk_limit(&self) -> usize {
            1900
        }
        async fn send_text(&self, target: &str, text: &str) -> Result<(), String> {
            if self.fail_at == Some(self.count()) {
                return Err("transport down".to_string());
            }
            self.sent
                .lock()
                .unwrap()
                .push((target.to_string(), text.to_string()));
            Ok(())
        }
    }

    const CHUNK_LIMIT: usize = 1900;

    fn ctx_with_session() -> GatewayCallContext {
        GatewayCallContext::new(GatewayCaller::Agent, "agent-a").with_session_id("sess-1")
    }

    // ---- #158 S1: 宛先の解決 ----

    /// #158 S1: 宛先を省略したら実行文脈の返信先（Discord では channel id の数値文字列）
    /// が使われる。
    #[test]
    fn resolve_channel_falls_back_to_ctx_reply_target() {
        let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-a")
            .with_reply_target(Some("111222333".to_string()));
        let args = json!({"content": "diff"});
        let resolved = resolve_review_channel(&args, &ctx).unwrap();
        assert_eq!(resolved, "111222333");
    }

    /// #158 S1 非退行: 宛先を明示したら文脈の返信先より優先される（既定値が増えるだけ）。
    #[test]
    fn resolve_channel_prefers_explicit_argument() {
        let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-a")
            .with_reply_target(Some("111222333".to_string()));
        let args = json!({"content": "diff", "channel_id": "999"});
        let resolved = resolve_review_channel(&args, &ctx).unwrap();
        assert_eq!(resolved, "999", "引数の宛先が文脈より優先される");
    }

    /// #158 S1: 引数も文脈も宛先を持たないなら "" で送らず明示エラー（fail-closed）。
    #[test]
    fn resolve_channel_fails_closed_without_any_target() {
        let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-a");
        let args = json!({"content": "diff"});
        let err = resolve_review_channel(&args, &ctx).unwrap_err();
        assert!(
            err.starts_with("channel_idパラメータが必要です"),
            "既存の文言に揃える: {err}"
        );
    }

    /// 空文字は宛先として扱わない（"" のまま送らない = fail-closed の担保）。
    #[test]
    fn resolve_channel_treats_blank_as_unspecified() {
        let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-a")
            .with_reply_target(Some("444".to_string()));
        let blank_args = json!({"content": "diff", "channel_id": "  "});
        let resolved = resolve_review_channel(&blank_args, &ctx).unwrap();
        assert_eq!(resolved, "444");

        let blank_ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-a")
            .with_reply_target(Some("".to_string()));
        let args = json!({"content": "diff"});
        assert!(resolve_review_channel(&args, &blank_ctx).is_err());
    }

    /// 移設前は Discord gateway の `normalize_id_args` が JSON 数値の `*_id` を文字列化
    /// していた。合成 gateway にはその正規化が無いので、宛先解決側で同じ吸収を行う
    /// （モデルは channel_id を数値で渡してくることが多い）。
    #[test]
    fn resolve_channel_accepts_numeric_channel_id() {
        let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-a");
        // 2^53 を超えるスノーフレークでも精度を落とさない。
        let args = json!({"content": "diff", "channel_id": 1234567890123456789_u64});
        let resolved = resolve_review_channel(&args, &ctx).unwrap();
        assert_eq!(resolved, "1234567890123456789");
    }

    // ---- ヘッダ組み立て ----

    #[test]
    fn header_includes_task_and_instructions() {
        let header = PeerReviewHeader {
            agent_name: "crab-a",
            task: Some((12, "ship feature", Some("tests green"))),
            instructions: Some("check the error handling"),
            mention: None,
        };
        let msgs = build_peer_review_messages(&header, "diff content", CHUNK_LIMIT);
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
        let msgs = build_peer_review_messages(&header, "x", CHUNK_LIMIT);
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
        let msgs = build_peer_review_messages(&header, "x", CHUNK_LIMIT);
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
            mention: Some("<@1234567890>"),
        };
        let msgs = build_peer_review_messages(&header, "x", CHUNK_LIMIT);
        // starts-with 判定を壊さないよう、メンションは marker の後ろ
        assert!(msgs[0].starts_with("[Peer Review Request] <@1234567890> from a"));
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
        let msgs = build_peer_review_messages(&header, "x", CHUNK_LIMIT);
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
        let msgs = build_peer_review_messages(&header, &content, CHUNK_LIMIT);
        let parts = &msgs[1..];
        assert!(parts.len() >= 3);
        // 各チャンクは limit + "part X/N\n" プレフィクス以内
        for (i, p) in parts.iter().enumerate() {
            let prefix = format!("part {}/{}\n", i + 1, parts.len());
            assert!(p.starts_with(&prefix));
            let body = &p[prefix.len()..];
            assert!(body.chars().count() <= CHUNK_LIMIT);
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

    // ---- レビュアー解決（幻覚 id への誤送信防止） ----

    #[test]
    fn resolve_reviewer_registered_only() {
        let conn = opencrab_db::init_memory().unwrap();
        let delivery = FakeDelivery::new();
        opencrab_db::queries::add_trusted_user(
            &conn,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
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
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
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
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
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
        assert_eq!(
            resolve_reviewer(&conn, &delivery, "agent-1", "crab b").unwrap(),
            "42"
        );
        assert_eq!(
            resolve_reviewer(&conn, &delivery, "agent-1", "2026").unwrap(),
            "77"
        );
        // 登録済み id / <@id> 形式
        assert_eq!(
            resolve_reviewer(&conn, &delivery, "agent-1", "42").unwrap(),
            "42"
        );
        assert_eq!(
            resolve_reviewer(&conn, &delivery, "agent-1", "<@42>").unwrap(),
            "42"
        );
        // 未登録の任意 id は拒否（幻覚 id のゴーストメンション防止）
        let err = resolve_reviewer(&conn, &delivery, "agent-1", "999").unwrap_err();
        assert!(err.contains("Crab B"));
        // 一覧のメンション記法は transport が組む
        assert!(err.contains("Crab B (<@42>)"), "{err}");
        // 非 co_agent はロスター外
        let err = resolve_reviewer(&conn, &delivery, "agent-1", "Human").unwrap_err();
        assert!(err.contains("Crab B"));
        assert!(!err.contains("Human"));
    }

    #[test]
    fn resolve_reviewer_lists_registration_hint_when_roster_is_empty() {
        let conn = opencrab_db::init_memory().unwrap();
        let delivery = FakeDelivery::new();
        let err = resolve_reviewer(&conn, &delivery, "agent-1", "nobody").unwrap_err();
        assert_eq!(
            err,
            "(なし — trusted-users API で permission=co_agent + display_name を登録してください)"
        );
    }

    // ---- 実体（引数検査・送信・台帳） ----

    fn db_with_agent() -> opencrab_db::Db {
        let db = opencrab_db::Db::memory().unwrap();
        {
            let conn = db.lock().unwrap();
            opencrab_db::queries::add_trusted_user(
                &conn,
                opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
                "row-1",
                "agent-a",
                "42",
                "co_agent",
                "owner",
                "2026-01-01",
                "Crab B",
            )
            .unwrap();
        }
        db
    }

    /// **エラー文言をリテラルで固定する**（移設でバイトが変わっていないこと）。
    #[tokio::test]
    async fn error_messages_are_byte_stable() {
        let db = db_with_agent();
        let d = FakeDelivery::new();

        // セッション必須（fail-closed）
        let no_session = GatewayCallContext::new(GatewayCaller::Agent, "agent-a");
        let r = request_peer_review(&db, &d, &json!({"content": "diff"}), &no_session).await;
        assert_eq!(
            r.error.unwrap(),
            "request_peer_review はセッション文脈でのみ実行できます（session_id 不明）"
        );
        // 空文字の session_id も同じく拒否する。
        let blank = GatewayCallContext::new(GatewayCaller::Agent, "agent-a").with_session_id("");
        let r = request_peer_review(&db, &d, &json!({"content": "diff"}), &blank).await;
        assert!(!r.success);

        let ctx = ctx_with_session();
        // content 未指定
        let r = request_peer_review(&db, &d, &json!({"channel_id": "123"}), &ctx).await;
        assert_eq!(
            r.error.unwrap(),
            "contentパラメータが必要です（レビュー対象のRAWコンテンツ）"
        );
        // 空白だけの content も未指定扱い
        let r =
            request_peer_review(&db, &d, &json!({"content": "  ", "channel_id": "1"}), &ctx).await;
        assert!(!r.success);

        // 長さ上限
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": "x".repeat(12_001), "channel_id": "123"}),
            &ctx,
        )
        .await;
        assert_eq!(
            r.error.unwrap(),
            "contentが12000文字を超えています — ワークスペースにファイルとして保存し discord_send_file で添付した上で、contentには要点とファイル名を書いてください"
        );
        // 上限ちょうどは通る（境界）
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": "x".repeat(12_000), "channel_id": "123"}),
            &ctx,
        )
        .await;
        assert!(r.success, "{:?}", r.error);

        // 宛先なし
        let r = request_peer_review(&db, &d, &json!({"content": "diff"}), &ctx).await;
        assert_eq!(
            r.error.unwrap(),
            "channel_idパラメータが必要です（実行文脈に返信先がありません）"
        );

        // 宛先が transport の形式に合わない（文言は transport が組む）
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": "diff", "channel_id": "not-a-number"}),
            &ctx,
        )
        .await;
        assert_eq!(r.error.unwrap(), "無効なchannel_id: not-a-number");

        // レビュアー未登録（幻覚 id）
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": "diff", "channel_id": "1", "reviewer": "999"}),
            &ctx,
        )
        .await;
        assert_eq!(
            r.error.unwrap(),
            "reviewer '999' が見つかりません。登録済みのピアレビュアー: Crab B (<@42>)"
        );
    }

    /// 成功時のレスポンス JSON のキーと固定文言。
    #[tokio::test]
    async fn success_payload_shape_is_stable() {
        let db = db_with_agent();
        let d = FakeDelivery::new();
        let ctx = ctx_with_session();
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": "diff", "channel_id": "555", "reviewer": "Crab B"}),
            &ctx,
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["channel_id"], "555");
        assert_eq!(data["parts"], 1);
        assert_eq!(data["task_id"], serde_json::Value::Null);
        assert_eq!(data["ledger_recorded"], false);
        assert_eq!(
            data["message"],
            "ピアレビュー依頼を投稿しました。[Peer Review] で始まる返信を待ってください。"
        );

        // ヘッダ + part 1/1 の 2 通、宛先はそのまま、メンションは transport の記法。
        let sent = d.sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, "555");
        assert!(sent[0]
            .1
            .starts_with("[Peer Review Request] <@42> from agent-a"));
        assert_eq!(sent[1].1, "part 1/1\ndiff");
    }

    /// active タスクがあれば台帳へ `[peer review requested]` を記録し、`task_id` を返す。
    #[tokio::test]
    async fn records_the_request_in_the_task_ledger() {
        let db = db_with_agent();
        let d = FakeDelivery::new();
        let task_id = {
            let conn = db.lock().unwrap();
            opencrab_db::queries::insert_task_ledger(&conn, "agent-a", "sess-1", "goal", None)
                .unwrap()
        };
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": "diff", "channel_id": "7", "instructions": "look here"}),
            &ctx_with_session(),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["task_id"], task_id);
        assert_eq!(data["ledger_recorded"], true);

        let conn = db.lock().unwrap();
        let progress = opencrab_db::queries::list_recent_task_progress(&conn, task_id, 10).unwrap();
        assert_eq!(progress.len(), 1);
        assert_eq!(
            progress[0].content,
            "[peer review requested] posted to channel 7 (1 parts) — focus: look here"
        );
    }

    /// **分割送信の途中失敗を明示する**（抽象越しに失われやすい情報。落とさない）。
    #[tokio::test]
    async fn partial_send_failure_reports_how_many_went_out() {
        let db = db_with_agent();
        // ヘッダ + 3 part = 4 通。3 通目（0-origin で 2）で失敗させる。
        let d = FakeDelivery::failing_at(2);
        let content = "あ".repeat(1900 * 2 + 10);
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": content, "channel_id": "9"}),
            &ctx_with_session(),
        )
        .await;
        assert!(!r.success);
        assert_eq!(
            r.error.unwrap(),
            "ピアレビュー依頼の送信に失敗（2/4 通送信済みの時点で失敗）: transport down。\
             投稿済みの依頼は不完全です。チャンネルに取り消しの一言を送ってから、必要なら再依頼してください。"
        );
        assert_eq!(d.count(), 2, "失敗前の 2 通だけが出ている");
    }

    /// 台帳記録は依頼が**未回収**であることの根拠になる（返信の自動記録ゲート）。
    /// 記録に失敗しても送信の成功は返す（best-effort）。
    #[tokio::test]
    async fn ledger_failure_does_not_fail_the_send() {
        let db = db_with_agent();
        let d = FakeDelivery::new();
        // task が無い（= 記録対象なし）ケース: ledger_recorded=false でも success。
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": "diff", "channel_id": "1"}),
            &ctx_with_session(),
        )
        .await;
        assert!(r.success);
        assert_eq!(r.data.unwrap()["ledger_recorded"], false);
        assert_eq!(d.count(), 2);
    }

    #[test]
    fn definition_is_stable() {
        let def = request_peer_review_definition();
        assert_eq!(def.name, "request_peer_review");
        assert!(def.description.starts_with("自分の成果物（diff・実行結果・トレース等）を、同じチャンネルにいる別のBot（別モデル）に"));
        assert_eq!(def.parameters["required"], json!(["content"]));
        let props = def.parameters["properties"].as_object().unwrap();
        let mut keys: Vec<&str> = props.keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["channel_id", "content", "instructions", "reviewer"]
        );
    }

    /// **#158 S2（#218）の成果を守る**: このツールは移設で全ターン（Nostr / web / REST /
    /// 定期実行）に露出するため、引数の説明文に transport 前提を書き戻してはならない。
    ///
    /// - `[Discord context]` を参照させると、その文脈が存在しないターンでモデルに宛先の
    ///   出所を教えることになり、幻覚した宛先への誤投稿を招く。
    /// - メンション記法（`<@`）を露出すると、名簿に無い識別子を組み立てて渡させる。
    ///   レビュアーは**表示名のみ**（記法の組み立ては transport の責務）。
    ///
    /// 説明文の先頭だけを見る [`definition_is_stable`] ではこの退行を検出できない
    /// （実際に rebase で 1 度巻き戻った）。ここは**全 description の本文**を見る。
    #[test]
    fn definition_text_stays_transport_neutral() {
        let def = request_peer_review_definition();
        let mut texts: Vec<String> = vec![def.description.clone()];
        for (name, prop) in def.parameters["properties"].as_object().unwrap() {
            let d = prop["description"].as_str().unwrap_or_default();
            assert!(!d.is_empty(), "{name} の説明文が空");
            texts.push(d.to_string());
        }
        for t in &texts {
            assert!(
                !t.contains("Discord context"),
                "存在しない文脈（[Discord context]）を参照させてはならない（#158 S2）: {t}"
            );
            assert!(
                !t.contains("<@"),
                "メンション記法は transport の責務。説明文に露出させない（#158 S2）: {t}"
            );
        }
        // 宛先は「省略が既定」であることを明示し続ける（#158 S1/S2）。
        let channel = def.parameters["properties"]["channel_id"]["description"]
            .as_str()
            .unwrap();
        assert!(channel.contains("通常は省略する"), "{channel}");
        assert!(
            channel.contains("推測した識別子を渡してはならない"),
            "{channel}"
        );
        // レビュアーは表示名のみ（transport のユーザー識別子を渡させない）。
        let reviewer = def.parameters["properties"]["reviewer"]["description"]
            .as_str()
            .unwrap();
        assert!(reviewer.contains("表示名を渡す"), "{reviewer}");
        assert!(!reviewer.contains("user id"), "{reviewer}");
    }

    // ---- 返信の回収（#156 S4 で Discord gateway から移設。解析 / 整形 / 3 つのゲート）

    /// 回収の便宜関数（移設前の 6 引数の呼び出し形をテスト内で保つ）。
    fn record_discord_reply(
        db: &opencrab_db::Db,
        agent_id: &str,
        session_id: &str,
        sender_id: &str,
        sender_name: &str,
        text: &str,
    ) -> bool {
        record_peer_review_reply(
            db,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
            agent_id,
            session_id,
            sender_id,
            sender_name,
            text,
        )
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

    /// **依頼側と回収側の目印が噛み合っていること**（#157 の依頼側 / #156 S4 の回収側）。
    ///
    /// 依頼が投稿する本文の先頭は [`PEER_REVIEW_REQUEST_MARKER`]、回収が探すのは
    /// [`PEER_REVIEW_REPLY_MARKER`]。前者が後者で始まってしまうと、依頼メッセージ自体が
    /// 「返信」として回収され、依頼した瞬間に verdict が捏造される。
    #[test]
    fn request_marker_is_not_harvested_as_a_reply() {
        assert!(!PEER_REVIEW_REQUEST_MARKER.starts_with(PEER_REVIEW_REPLY_MARKER));
        // 依頼側が実際に組み立てるヘッダ（`post_peer_review` と同じ形）でも回収されない。
        let header = format!("{PEER_REVIEW_REQUEST_MARKER}<@42> from crab-a — task #7\n");
        assert!(parse_peer_review_reply(&header).is_none());
        // 逆向き: 回収が探す目印は依頼側の目印の前置ではない（未回収判定も同様に
        // `[peer review requested]` が `[peer review]` で始まらないことに依存する）。
        assert!(!"[peer review requested] posted".starts_with("[peer review]"));
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

    /// 回収の 3 つのゲート（marker / 登録済み co_agent / 未回収の依頼）と 1 依頼 1 記録。
    #[test]
    fn record_reply_gates_and_writes() {
        let db = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        let task_id = {
            let conn = db.lock().unwrap();
            // 送信者 "42" をこのエージェントの co_agent として登録
            opencrab_db::queries::add_trusted_user(
                &conn,
                opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
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
        assert!(!record_discord_reply(
            &db, "a1", "s1", "42", "crab-b", "hello"
        ));
        // 未登録送信者（co_agent でない）→ 記録しない
        assert!(!record_discord_reply(
            &db, "a1", "s1", "99", "stranger", reply
        ));
        // active タスクの無いセッション → 記録しない
        assert!(!record_discord_reply(
            &db, "a1", "other", "42", "crab-b", reply
        ));
        // 正常系
        assert!(record_discord_reply(&db, "a1", "s1", "42", "crab-b", reply));
        // 依頼が回収済みになったので、追加の返信は記録しない（1依頼1記録）
        assert!(!record_discord_reply(
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

    /// 同じ送信者識別子でも、**登録された経路が違えば解決しない**（fail-closed / #214）。
    ///
    /// 由来（[`TranscriptSource`]）から経路を引くので、経路の列を持たない由来
    /// （Nostr）では回収しない。「とりあえず discord で引く」に戻すとこのテストが落ちる。
    #[test]
    fn harvest_resolves_sender_in_the_route_that_declared_it() {
        let db = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        {
            let conn = db.lock().unwrap();
            opencrab_db::queries::add_trusted_user(
                &conn,
                opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
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
            opencrab_db::queries::insert_task_progress(
                &conn,
                task_id,
                "progress",
                "[peer review requested] posted to channel 1 (1 parts)",
            )
            .unwrap();
        }
        let reply = "[Peer Review] score: 0.6 summary: ok";
        let record = InboundMessageRecord {
            session_id: "s1",
            sender_id: "42",
            sender_name: "crab-b",
            avatar_url: None,
            channel_id: Some("1"),
            pubkey: None,
            text: reply,
            image_urls: &[],
        };
        // 経路の列を持たない由来 → 回収しない（識別子が偶然一致しても受理しない）
        assert!(!harvest_inbound_reply(
            &db,
            TranscriptSource::Nostr,
            "a1",
            &record
        ));
        assert!(trusted_platform_for(TranscriptSource::Nostr).is_none());
        // Discord は登録済み経路 → 回収する
        assert!(harvest_inbound_reply(
            &db,
            TranscriptSource::Discord,
            "a1",
            &record
        ));
    }
}
