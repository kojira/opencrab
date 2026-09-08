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
//! ## 名簿と受理の経路（#159）
//! 指名できる相手（ロスター）と返信を受理できる相手は **同じ経路** でなければならない。
//! ずれていた頃は「別経路の co_agent へ依頼は飛ぶが、返信は受理ゲートで落ちる」状態に
//! なりえた。揃える向きは名簿を狭める側だけ（[`REVIEWER_PLATFORM`]）。
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
        class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::Blocked, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
/// 名簿は返信の受理ゲートと同じ経路（[`REVIEWER_PLATFORM`]）だけを引く（#159）。
/// 返信を受理できない相手を指名させないため。`<@id>` 形式の受理と「数値としてパース
/// できる id だけを採る」判定は据え置き。返り値はパース済み数値の文字列表現で、移設前
/// （`u64` を返して呼び出し側が `<@{id}>` に埋めていた）とバイト単位で同じメンションになる。
pub fn resolve_reviewer(
    conn: &rusqlite::Connection,
    delivery: &dyn TextDelivery,
    agent_id: &str,
    reviewer: &str,
) -> Result<String, String> {
    let reviewer = reviewer.trim();
    let co_agents =
        match opencrab_db::queries::list_co_agent_reviewers(conn, REVIEWER_PLATFORM, agent_id) {
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
        "(なし — trusted-users API で permission=co-agent + display_name を登録してください)"
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
/// Nostr は送信者識別子（pubkey）を信頼済みユーザー表の経路として持たない。
/// 対応が無い由来は `None` を返し、回収させない（fail-closed）。
fn trusted_platform_for(source: TranscriptSource) -> Option<&'static str> {
    match source {
        TranscriptSource::Discord => Some(opencrab_db::queries::TRUSTED_PLATFORM_DISCORD),
        TranscriptSource::Nostr => None,
        TranscriptSource::External => None,
    }
}

/// レビュアー名簿（ロスター）を引く経路（#159）。
///
/// **[`trusted_platform_for`] が返しうる経路と一致させること。** 名簿を絞らないままだと
/// 「別経路の co_agent を指名できる → 依頼は飛ぶ → 返信は受理ゲートで落ちる」という
/// 非対称になる（#159 で引き継いだ課題）。揃える向きは**名簿を狭める側だけ**:
/// 受理ゲートに別経路の判定を足すと、そこが権限の昇格経路になる。
///
/// 一致は `roster_platform_matches_the_harvestable_platforms` が固定する
/// （`TranscriptSource` に由来が増えたらそのテストがコンパイルできなくなる）。
pub(crate) const REVIEWER_PLATFORM: &str = opencrab_db::queries::TRUSTED_PLATFORM_DISCORD;

/// 受信した `[Peer Review]` 返信を requester の active タスクへ自動記録する（#58）。
///
/// ゲート（すべて満たす場合のみ記録）:
/// 1. marker が行頭にある
/// 2. 送信者がこのエージェントの登録済み co_agent（第三者・未信頼の偽 verdict を排除）
/// 3. active タスクに未回収のレビュー依頼がある（依頼していないレビューを誤記録しない）
///
/// 記録は追加処理: メッセージ自体はこの後通常どおり LLM にも流れる（会話には speech として残る）。
/// session_logs には重ねて記録しない（二重描画を避ける）。台帳へ書いた verdict は
/// **この受信で走るターンの** `[Task Ledger]` に載る（#156 S4 で受信フックを会話組み立ての
/// 前に置いたため。「次ターンに出る」と書いてあった移設前の記述は誤り）。
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
        .map(|u| u.permission == opencrab_db::queries::TrustedUserPermission::CoAgent)
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
#[path = "peer_review/tests/mod.rs"]
mod tests;
