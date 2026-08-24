//! provider — 本物の推論を、`Engine` の seam の向こうに置く（詳細§01・§05）。
//!
//! `EchoEngine`（同クレート）はこだまを返す差し替え実装。ここはその**同じ口**の向こうに、HTTPS で
//! ストリーミングするプロバイダを繋ぐ。core は `Engine` trait しか知らない——本物か echo かは app が
//! **設定で選ぶ**（フォールバックではない・§15）。鍵が無ければ選ばれないだけで、ビルドもテストも通る。
//!
//! **TLS は自前で張らない**（オーナー判断）: 既存 opencrab の実戦のプロバイダ層と同じく `reqwest` に
//! 終端させる。`https://api.anthropic.com` を直に叩ける。ローカルの偽サーバは平文でも自己署名 TLS でも
//! 立てられ、どちらでもアイドル上限が効くことを検証する（本物の TLS 経路の担保・§05）。
//!
//! **アイドルの上限はチャンク間で効く**（§05）: プロバイダから断片が届くたびに `chunks.chunk()` を叩く。
//! core の `infer_with_idle_cap` がそれで計測を取り直すので、長く流れている生成は切られず、止まった生成
//! だけが切れる——本物の TLS 経路でも同じ。**総時間では切らない。** 上限に達すると core が infer の future
//! を捨て、進行中のリクエストは中断される（ソケットが閉じる。ランタイムの性質）。
//!
//! **落とし方の境界**（§15）: 外から来たもの＝プロバイダの応答・接続も含めて、壊れ・失敗は
//! `EngineError` を返す（core は死なない）。近いものへ寄せない・echo へ逃がさない・黙って再試行しない・
//! 既定値で埋めない。既存 opencrab から持ってきた際、`unwrap_or(json!({}))`（壊れた引数を空に潰す）・
//! `Err(_) => None`（壊れた SSE を握り潰す）の類は**落とした**。
//!
//! **持ってきたもの / 落としたもの**（既存 opencrab `crates/llm` のプロバイダ層から。core の型は 1 つも
//! 持ち込まない）:
//!   - `reqwest` による HTTPS 終端（TLS を書かない）。
//!   - SSE の**行バッファ**（チャンク境界を跨ぐ結合・マルチバイト UTF-8 の保護・`sse.rs` の line_stream）。
//!   - Anthropic のイベント解釈（`content_block_delta`/`tool_use`/`message_delta.stop_reason`）とモデル ID。
//!   - `input_schema` 付きのツール宣言と、ツール列への `cache_control`（プロンプトキャッシュ）。
//!   - 落とした: 黙って再試行・既定値埋め・失敗の握り潰し（§15）。総時間で切る形（アイドルは core が握る）。

use opencrab_port::{
    Block, ChunkSink, Context, EffectSpec, Engine, EngineError, InferOutput, MsgRole, Part,
    ToolCallSpec,
};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Deterministic response scripts exposed by the explicit `mock` provider.
///
/// The mock is a real `Engine` implementation selected at the same provider boundary as the
/// network providers. It is never selected implicitly: both `OPENCRAB_LLM_PROVIDER=mock` and a
/// known `OPENCRAB_MOCK_LLM_SCRIPT` value are required.
#[derive(Clone, Copy)]
enum MockScript {
    Reply,
    History,
    NoReply,
    PrefixedNoReply,
    ToolThenReply,
    PlaintextToolSettledReply,
    ClockBatch,
    AnswerDirect,
    ShellThenRead,
    AnswerThenNoReply,
    ShellFailThenRead,
    /// #810: 成功の大きい結果を退避したあと、後続ターンで core-bg-read する実形。
    ShellOffloadThenRead,
    /// #810 QC 形: 同一ターン反復で、切り離し直後（settle 前）に core-bg-read する。
    ShellThenBgReadBeforeSettle,
    /// QC 形（#796）: PROGRESS + reply + 末尾空セグメント。空 Spoke を公開しないことの検査用。
    ProgressReplyTrailingEmpty,
}

pub const MOCK_MODEL: &str = "mock";

const OFFLOAD_MARKER_LINE: &str = "OFFLOAD-LINE-0000-XXXXXXXXXXXXXXXXXXXXXXXX";

/// 退避案内の読み方レシピから activity を取る（`core-bg-read（activity=N・…）`）。
fn activity_id_from_offload_notice(rendered: &str) -> Option<i64> {
    digits_after(rendered, "core-bg-read（activity=")
}

/// 同一ターンの切り離し通知から activity を取る（`背景へ移した（活動 N）`）。
fn activity_id_from_detached_notice(text: &str) -> Option<i64> {
    digits_after(text, "背景へ移した（活動 ")
}

fn digits_after(text: &str, mark: &str) -> Option<i64> {
    let rest = text.split(mark).nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

fn tool_result_texts(ctx: &Context) -> Vec<(String, bool)> {
    ctx.history
        .iter()
        .flat_map(|message| {
            message.content.iter().filter_map(|block| match block {
                Block::ToolResult {
                    content, is_error, ..
                } => {
                    let text: String = content
                        .iter()
                        .filter_map(|part| match part {
                            Part::Text(t) => Some(t.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    Some((text, *is_error))
                }
                _ => None,
            })
        })
        .collect()
}

/// Provider-level deterministic engine for process E2E tests and offline deployments.
pub struct MockEngine {
    script: MockScript,
}

impl MockEngine {
    fn from_env() -> Result<Self, String> {
        let value = match std::env::var("OPENCRAB_MOCK_LLM_SCRIPT") {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => {
                return Err(
                    "OPENCRAB_MOCK_LLM_SCRIPT is required when OPENCRAB_LLM_PROVIDER=mock"
                        .to_string(),
                )
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err("OPENCRAB_MOCK_LLM_SCRIPT is not valid Unicode".to_string())
            }
        };
        let script = match value.as_str() {
            "reply" => MockScript::Reply,
            "history" => MockScript::History,
            "no_reply" => MockScript::NoReply,
            "prefixed_no_reply" => MockScript::PrefixedNoReply,
            "tool_then_reply" => MockScript::ToolThenReply,
            "plaintext_tool_settled_reply" => MockScript::PlaintextToolSettledReply,
            "clock_batch" => MockScript::ClockBatch,
            "answer_direct" => MockScript::AnswerDirect,
            "shell_then_read" => MockScript::ShellThenRead,
            "answer_then_no_reply" => MockScript::AnswerThenNoReply,
            "shell_fail_then_read" => MockScript::ShellFailThenRead,
            "shell_offload_then_read" => MockScript::ShellOffloadThenRead,
            "shell_then_bg_read_before_settle" => MockScript::ShellThenBgReadBeforeSettle,
            "progress_reply_trailing_empty" => MockScript::ProgressReplyTrailingEmpty,
            other => return Err(format!("unknown OPENCRAB_MOCK_LLM_SCRIPT: {other}")),
        };
        Ok(Self { script })
    }
}

#[async_trait::async_trait]
impl Engine for MockEngine {
    fn emits_tool_calls(&self) -> bool {
        !matches!(
            self.script,
            MockScript::PlaintextToolSettledReply
                | MockScript::ShellThenRead
                | MockScript::ShellFailThenRead
        )
    }

    fn model(&self) -> &str {
        MOCK_MODEL
    }

    async fn infer(&self, ctx: &Context, chunks: &ChunkSink) -> Result<InferOutput, EngineError> {
        chunks.chunk();
        let say = |text: &str| InferOutput {
            effects: vec![EffectSpec::say(text)],
            tool_calls: vec![],
            done: true,
        };
        match self.script {
            MockScript::Reply => {
                // Nostr E2E の合成 mention では、通常の即応ターンに着火発言が会話ログと区別して
                // 明示されたことまで mock 境界で検査する。通常の provider unit context は対象外。
                if ctx.rendered.contains("synthetic mention for reply") {
                    let trigger_block = ctx
                        .rendered
                        .split_once("=== 即応の発端 ===\n")
                        .and_then(|(_, rest)| rest.split_once("\n\n").map(|(block, _)| block));
                    if !trigger_block.is_some_and(|block| {
                        block.contains("synthetic mention for reply") && block.contains("[1]")
                    }) {
                        return Err(EngineError(
                            "mock reply turn did not receive the explicit triggering utterance"
                                .to_string(),
                        ));
                    }
                }
                Ok(say("mock reply"))
            }
            MockScript::History => {
                if !ctx.rendered.contains("synthetic history question") {
                    return Ok(say("mock history seed acknowledged"));
                }
                if !ctx.rendered.contains("synthetic history seed")
                    || !ctx.rendered.contains("mock history seed acknowledged")
                {
                    return Err(EngineError(
                        "mock history turn did not receive the previously read conversation"
                            .to_string(),
                    ));
                }
                Ok(say("mock remembered synthetic history seed"))
            }
            MockScript::NoReply => Ok(say("NO_REPLY")),
            MockScript::PrefixedNoReply => Ok(say("mock internal reasoning\nNO_REPLY")),
            MockScript::ToolThenReply => {
                let result = ctx.history.iter().find_map(|message| {
                    message.content.iter().find_map(|block| match block {
                        Block::ToolResult {
                            content, is_error, ..
                        } => Some((content, *is_error)),
                        _ => None,
                    })
                });
                match result {
                    None => Ok(InferOutput {
                        effects: vec![],
                        tool_calls: vec![ToolCallSpec {
                            id: "mock-tool-call-1".to_string(),
                            name: "core-child-list".to_string(),
                            args: serde_json::json!({}),
                        }],
                        done: false,
                    }),
                    Some((_, true)) => Err(EngineError(
                        "mock tool_then_reply received an error tool result".to_string(),
                    )),
                    Some((content, false)) if content.is_empty() => Err(EngineError(
                        "mock tool_then_reply received an empty tool result".to_string(),
                    )),
                    Some((_, false)) => Ok(say("mock reply after tool result")),
                }
            }
            MockScript::PlaintextToolSettledReply => {
                let settled = ctx.rendered.contains("=== 決着の対応 ===");
                if !settled {
                    return Ok(say("nostr-whoami::{}\nNO_REPLY"));
                }
                if !ctx
                    .rendered
                    .contains("synthetic mention for plaintext_tool_settled_reply")
                {
                    return Err(EngineError(
                        "mock plaintext settled turn did not receive the originating request"
                            .to_string(),
                    ));
                }
                if !ctx.rendered.contains("npub1") {
                    return Err(EngineError(
                        "mock plaintext settled turn did not receive the tool result".to_string(),
                    ));
                }
                if !ctx.rendered.contains("受理ツール: nostr-whoami args={}") {
                    return Err(EngineError(
                        "mock plaintext settled turn did not receive the accepted tool call"
                            .to_string(),
                    ));
                }
                Ok(say("mock reply after settled plaintext tool result"))
            }
            MockScript::ClockBatch => {
                let has_first = ctx.rendered.contains("synthetic batched first");
                let has_second = ctx.rendered.contains("synthetic batched second");
                if has_first || has_second {
                    if !(has_first && has_second) {
                        return Err(EngineError(
                            "mock batch turn did not receive both same-standing events".to_string(),
                        ));
                    }
                    Ok(say("mock batched pair"))
                } else if ctx.rendered.contains("synthetic immediate clock probe") {
                    Ok(say("mock immediate clock reply"))
                } else {
                    Err(EngineError(
                        "mock clock script received an unknown turn".to_string(),
                    ))
                }
            }
            MockScript::AnswerDirect => Ok(say("synthetic-direct-answer")),
            MockScript::ShellThenRead => {
                let settled = ctx.rendered.contains("=== 決着の対応 ===");
                if !settled {
                    return Ok(say(r#"core-shell::{"argv":["date"]}"#));
                }
                let result = ctx
                    .rendered
                    .split("結果:")
                    .nth(1)
                    .map(str::trim)
                    .filter(|body| !body.is_empty());
                match result {
                    Some(body) => Ok(say(&format!("synthetic-shell-result {body}"))),
                    None => Err(EngineError(
                        "mock shell_then_read settled turn did not receive the tool result"
                            .to_string(),
                    )),
                }
            }
            MockScript::AnswerThenNoReply => Ok(say("synthetic-mixed-answer\nNO_REPLY")),
            MockScript::ShellFailThenRead => {
                let settled = ctx.rendered.contains("=== 決着の対応 ===");
                if !settled {
                    return Ok(say(r#"core-shell::{"argv":["ls"]}"#));
                }
                let result = ctx
                    .rendered
                    .split("結果:")
                    .nth(1)
                    .map(str::trim)
                    .filter(|body| !body.is_empty());
                match result {
                    Some(body) => Ok(say(&format!("synthetic-shell-failure {body}"))),
                    None => Err(EngineError(
                        "mock shell_fail_then_read settled turn did not receive the failure reason"
                            .to_string(),
                    )),
                }
            }
            MockScript::ShellOffloadThenRead => {
                // 実形（#810 / QC）: native tool。ターン N で大きい shell を切り離し、
                // settle 後の後続ターンが core-bg-read {"activity":N} だけを呼ぶ（行指定なし＝既定）。
                // 同じターンの tool_result（背景へ移した）では読まない。
                if ctx.history.iter().any(|message| {
                    message.content.iter().any(|block| match block {
                        Block::ToolResult {
                            content, is_error, ..
                        } => {
                            !*is_error
                                && content.iter().any(|part| match part {
                                    Part::Text(t) => t.contains(OFFLOAD_MARKER_LINE),
                                    _ => false,
                                })
                        }
                        _ => false,
                    })
                }) {
                    return Ok(say(&format!(
                        "synthetic-offload-read {OFFLOAD_MARKER_LINE}"
                    )));
                }
                if ctx.rendered.contains("=== 決着の対応 ===") {
                    let Some(activity) = activity_id_from_offload_notice(&ctx.rendered) else {
                        return Err(EngineError(
                            "mock shell_offload_then_read settled turn did not receive an offload recipe"
                                .to_string(),
                        ));
                    };
                    return Ok(InferOutput {
                        effects: vec![],
                        tool_calls: vec![ToolCallSpec {
                            id: "mock-bg-read".to_string(),
                            name: "core-bg-read".to_string(),
                            args: serde_json::json!({ "activity": activity }),
                        }],
                        done: false,
                    });
                }
                Ok(InferOutput {
                    effects: vec![],
                    tool_calls: vec![ToolCallSpec {
                        id: "mock-shell-offload".to_string(),
                        name: "core-shell".to_string(),
                        args: serde_json::json!({
                            "argv": [
                                "awk",
                                "BEGIN{for(i=0;i<500;i++) printf \"OFFLOAD-LINE-%04d-XXXXXXXXXXXXXXXXXXXXXXXX\\n\", i}"
                            ]
                        }),
                    }],
                    done: false,
                })
            }
            MockScript::ShellThenBgReadBeforeSettle => {
                // QC 形（#810）: 同一ターン反復。detach の tool_result を見て、settle を待たずに
                // core-bg-read する。公開本文は読みの実メッセージ（走行中／畳み込み誤り）を復唱する。
                if let Some(body) =
                    tool_result_texts(ctx)
                        .into_iter()
                        .find_map(|(text, is_error)| {
                            if is_error
                                && (text.contains("まだ決着していない")
                                    || text.contains("あなたの活動ではない")
                                    || text.contains("退避されていない"))
                            {
                                Some(text)
                            } else {
                                None
                            }
                        })
                {
                    return Ok(say(&format!("synthetic-inprogress-read {body}")));
                }
                if let Some(activity) = tool_result_texts(ctx)
                    .iter()
                    .find_map(|(text, _)| activity_id_from_detached_notice(text))
                {
                    return Ok(InferOutput {
                        effects: vec![],
                        tool_calls: vec![ToolCallSpec {
                            id: "mock-bg-read-before-settle".to_string(),
                            name: "core-bg-read".to_string(),
                            args: serde_json::json!({ "activity": activity }),
                        }],
                        done: false,
                    });
                }
                Ok(InferOutput {
                    effects: vec![],
                    tool_calls: vec![ToolCallSpec {
                        id: "mock-shell-before-settle".to_string(),
                        name: "core-shell".to_string(),
                        args: serde_json::json!({ "argv": ["sleep", "30"] }),
                    }],
                    done: false,
                })
            }
            MockScript::ProgressReplyTrailingEmpty => {
                Ok(say("PROGRESS::読み込み中\n\nreply:1:synthetic-qc-reply\n"))
            }
        }
    }
}

/// SSE の 1 イベントの意味（プロバイダごとに解釈が違うので値で返す）。転送側（`stream_request`）が
/// 断片を跨いで積む——このenumは 1 イベント＝1 値で、状態を持たない。
pub enum Delta {
    /// 本文の断片。
    Text(String),
    /// ツール呼び出しブロックの開始（`content_block_start`）。index で以後の入力断片を紐づける。
    /// `id` は Anthropic の `tool_use.id`——結果を対にして返すために保つ（§05）。
    ToolStart {
        index: u64,
        id: String,
        name: String,
    },
    /// ツール入力（JSON）の断片（`input_json_delta`）。index のブロックへ積む。
    ToolInput { index: u64, partial_json: String },
    /// 生成の終わり方（`message_delta.stop_reason`）。`tool_use` ならターンは続く（§07）。
    StopReason(String),
    /// 生成の終わり（`message_stop`）。
    Done,
    /// ストリームの本文（200 応答）の中で外から届いた失敗（Responses API の `error` イベント等）。
    /// HTTP ステータスでは表に出ない在中のエラーを握り潰さず、転送が `EngineError` に写す（§15）。
    Error(String),
    /// この系には関係ないイベント（開始・使用量など）。
    Ignore,
}

/// チャット系プロバイダの「線の組み方」だけを差し替える口。
///
/// **プロバイダを 1 つ足すときに触るのはここ 1 実装 + 選択（`engine_from_env`）の 1 アーム**——
/// 転送・ストリーミング・チャンクの叩き（`HttpSseEngine`）は共通で、プロバイダごとには書かない。
#[async_trait::async_trait]
pub trait ChatProvider: Send + Sync {
    /// POST の経路（例: `/v1/messages`）。base_url に続けて繋ぐ。
    fn path(&self) -> String;
    /// 追加ヘッダ（認証など）。鍵はここで注入する。同期に組めるものはここへ。
    fn headers(&self) -> Vec<(String, String)>;
    /// 転送が実際に載せるヘッダ。**認証の準備が非同期な口**（OAuth トークンの取得など）は
    /// ここを上書きする。既定は同期 `headers()` をそのまま返す——だから Anthropic/OpenAI は触らない。
    ///
    /// これが「HTTP 固定を緩める」ための穴: 鍵の取得が非同期でも、転送（`HttpSseEngine`）を
    /// 分岐させず・既存プロバイダを触らず 1 つ差せる。失敗は `EngineError`（近いものへ寄せない・§15）。
    async fn prepared_headers(&self) -> Result<Vec<(String, String)>, EngineError> {
        Ok(self.headers())
    }
    /// 予算の物差しにする実効モデル名（§06）。`HttpSseEngine` がこれを `Engine::model()` として core へ
    /// 見せ、core が起動時に `context_window × ratio` で会話予算を確定する。
    fn model(&self) -> &str;
    /// このプロバイダが画像を tool_result で受けるか（DESIGN-images §6）。既定 `true`（Anthropic/OpenAI
    /// chat は image ブロックを受ける）。tool 出力に画像を載せられない wire（Responses の
    /// function_call_output はテキストのみ）は `false` を返す——core が core-look をメニューから落とす。
    fn accepts_images(&self) -> bool {
        true
    }
    /// 文脈からリクエスト本体（JSON）を組む。`stream:true` を含めること。
    fn build_body(&self, ctx: &Context) -> serde_json::Value;
    /// SSE の 1 イベントの `data:`（JSON）を解釈し、そこに含まれる断片を**順に**返す。
    ///
    /// **1 イベントが複数の断片を含み得る**ので `Vec` で返す（Anthropic は 1 イベント 1 断片だが、
    /// OpenAI 形式は 1 つの `chat.completion.chunk` に tool_call の開始＋入力＋終了理由を**まとめて**
    /// 載せる）。近いものへ寄せず・握り潰さず、含まれる分だけをそのまま並べる（§15）。関係ないイベントは空。
    fn parse_event(&self, data: &serde_json::Value) -> Vec<Delta>;
}

/// Anthropic Messages API（ストリーミング・SSE）のプロバイダ。
///
/// 参照（実戦のプロバイダ層から）: `content_block_delta.delta.text` を積み、`tool_use` ブロックは
/// `content_block_start`（name）＋ `input_json_delta`（partial_json）で組み、`message_delta.stop_reason`
/// を見て `message_stop` で終わる。版ヘッダ `anthropic-version: 2023-06-01`、鍵は `x-api-key`。
pub struct AnthropicProvider {
    model: String,
    api_key: String,
    max_tokens: u64,
}

impl AnthropicProvider {
    pub fn new(model: impl Into<String>, api_key: impl Into<String>, max_tokens: u64) -> Self {
        AnthropicProvider {
            model: model.into(),
            api_key: api_key.into(),
            max_tokens,
        }
    }
}

impl ChatProvider for AnthropicProvider {
    fn model(&self) -> &str {
        &self.model
    }
    fn path(&self) -> String {
        "/v1/messages".to_string()
    }
    fn headers(&self) -> Vec<(String, String)> {
        vec![
            ("content-type".into(), "application/json".into()),
            ("x-api-key".into(), self.api_key.clone()),
            ("anthropic-version".into(), "2023-06-01".into()),
        ]
    }
    fn build_body(&self, ctx: &Context) -> serde_json::Value {
        // 会話はターンの中で積み上がる（§05）。最初の user メッセージ（＝場のログから 1 度組んだ rendered）に、
        // 積んだ `history` を **native の tool_use/tool_result ブロック**で続ける。テキストに混ぜない。
        let mut messages: Vec<serde_json::Value> = vec![serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": ctx.rendered}],
        })];
        for m in &ctx.history {
            let role = match m.role {
                MsgRole::User => "user",
                MsgRole::Assistant => "assistant",
            };
            let content: Vec<serde_json::Value> = m
                .content
                .iter()
                .map(|b| match b {
                    Block::Text(t) => serde_json::json!({"type": "text", "text": t}),
                    Block::ToolUse { id, name, input } => serde_json::json!({
                        "type": "tool_use", "id": id, "name": name, "input": input,
                    }),
                    Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => serde_json::json!({
                        "type": "tool_result", "tool_use_id": tool_use_id,
                        // マルチパート（DESIGN-images §4）: Anthropic の tool_result content は
                        // text / image ブロックの配列で持てる。画像は base64 の image ブロックへ写す。
                        "content": anthropic_tool_result_content(content), "is_error": is_error,
                    }),
                })
                .collect();
            messages.push(serde_json::json!({"role": role, "content": content}));
        }
        // 増分キャッシュ: 会話の末尾ブロックに cache_control を置く（増えた分だけ足すので、安定プレフィックスが
        // 反復ごとにキャッシュから読める＝丸ごと再処理しない・§05。実戦のプロバイダ層と同じ wire 形）。
        if let Some(last) = messages.last_mut() {
            if let Some(arr) = last["content"].as_array_mut() {
                if let Some(b) = arr.last_mut() {
                    b["cache_control"] = serde_json::json!({"type": "ephemeral"});
                }
            }
        }
        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "stream": true,
            "messages": messages,
        });
        // system は top-level（block 配列）へ載せるだけ——core が組んだものをそのまま置く。末尾ブロックに
        // cache_control を置き、ターン跨ぎで安定な prefix をキャッシュする（設計の system 末尾 breakpoint）。
        // 空（非 Agent ターン等）のときは付けない（線に空 system を載せない）。
        if !ctx.system.is_empty() {
            body["system"] = serde_json::json!([{
                "type": "text",
                "text": ctx.system,
                "cache_control": {"type": "ephemeral"},
            }]);
        }
        // 道具を宣言する（§10）。宣言しない道具をモデルは呼べない。core が `check` を通したものだけが
        // `ctx.tools` に載る（§09）。ツール列の末尾に cache_control を置く（ツールループの各反復で同じ
        // ツール列を再送するので、安定プレフィックスをキャッシュできる。実戦のプロバイダ層と同じ wire 形）。
        if !ctx.tools.is_empty() {
            let mut tools: Vec<serde_json::Value> = ctx
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.params,
                    })
                })
                .collect();
            if let Some(last) = tools.last_mut() {
                last["cache_control"] = serde_json::json!({"type": "ephemeral"});
            }
            body["tools"] = serde_json::json!(tools);
        }
        body
    }
    fn parse_event(&self, data: &serde_json::Value) -> Vec<Delta> {
        // Anthropic は 1 イベント＝1 断片。単一の Delta を組んで 1 要素の Vec で返す（seam は Vec・上の trait）。
        let one = match data.get("type").and_then(|x| x.as_str()) {
            Some("content_block_start") => {
                let block = data.get("content_block");
                let is_tool =
                    block.and_then(|b| b.get("type")).and_then(|x| x.as_str()) == Some("tool_use");
                match (
                    is_tool,
                    data.get("index").and_then(|x| x.as_u64()),
                    block.and_then(|b| b.get("id")).and_then(|x| x.as_str()),
                    block.and_then(|b| b.get("name")).and_then(|x| x.as_str()),
                ) {
                    (true, Some(index), Some(id), Some(name)) => Delta::ToolStart {
                        index,
                        id: id.to_string(),
                        name: name.to_string(),
                    },
                    _ => Delta::Ignore, // text ブロックの開始などは無視（本文は content_block_delta で来る）
                }
            }
            Some("content_block_delta") => {
                let d = data.get("delta");
                match d.and_then(|x| x.get("type")).and_then(|x| x.as_str()) {
                    Some("text_delta") => {
                        match d.and_then(|x| x.get("text")).and_then(|x| x.as_str()) {
                            Some(t) => Delta::Text(t.to_string()),
                            None => Delta::Ignore,
                        }
                    }
                    Some("input_json_delta") => match (
                        data.get("index").and_then(|x| x.as_u64()),
                        d.and_then(|x| x.get("partial_json"))
                            .and_then(|x| x.as_str()),
                    ) {
                        (Some(index), Some(pj)) => Delta::ToolInput {
                            index,
                            partial_json: pj.to_string(),
                        },
                        _ => Delta::Ignore,
                    },
                    _ => Delta::Ignore,
                }
            }
            Some("message_delta") => {
                match data
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|x| x.as_str())
                {
                    Some(r) => Delta::StopReason(r.to_string()),
                    None => Delta::Ignore,
                }
            }
            Some("message_stop") => Delta::Done,
            Some("error") => Delta::Error(provider_error_message(data)),
            _ => Delta::Ignore,
        };
        vec![one]
    }
}

/// OpenAI 互換の Chat Completions API（ストリーミング・SSE）のプロバイダ。
///
/// **プロバイダを 1 つ足す = この 1 実装 ＋ `engine_from_env` の 1 アーム**（転送・チャンクの叩き・
/// アイドル上限は `HttpSseEngine` が共通で持つ）。この環境の実モデルは OpenAI 形式で出ている
/// （`/v1/chat/completions`・`claude-*-*` のモデル ID）ので、変換の橋を外に立てずに直に届く。
///
/// 線の形（Anthropic との違いだけ）:
///   - path は `/v1/chat/completions`、鍵は `Authorization: Bearer`（あるときだけ付ける・偽/ローカルは不要）。
///   - messages は role/content の平坦形。tool 往復は assistant の `tool_calls` と role=`tool`（`tool_call_id`）へ写す。
///   - tool 宣言は `[{type:"function", function:{name,description,parameters}}]`。
///   - SSE は `choices[].delta.content`（本文）と `choices[].delta.tool_calls[]`（id/name/arguments）を運び、
///     `choices[].finish_reason` で終わる。**1 チャンクに開始＋入力＋終了理由がまとまって来る**ので、
///     `parse_event` は複数の Delta を返す（seam を Vec にした理由）。`tool_calls` は core が続行と読む
///     `tool_use` へ写す（終了理由の語彙を core 側に合わせる・§07）。
pub struct OpenAiProvider {
    model: String,
    api_key: String,
    max_tokens: u64,
}

impl OpenAiProvider {
    pub fn new(model: impl Into<String>, api_key: impl Into<String>, max_tokens: u64) -> Self {
        OpenAiProvider {
            model: model.into(),
            api_key: api_key.into(),
            max_tokens,
        }
    }
}

impl ChatProvider for OpenAiProvider {
    fn model(&self) -> &str {
        &self.model
    }
    fn path(&self) -> String {
        "/v1/chat/completions".to_string()
    }
    fn headers(&self) -> Vec<(String, String)> {
        let mut h = vec![("content-type".into(), "application/json".into())];
        // 鍵は要求しない——空なら付けない（ローカルの偽/橋は鍵を見ない。本物は Bearer で送る・§15）。
        if !self.api_key.is_empty() {
            h.push(("authorization".into(), format!("Bearer {}", self.api_key)));
        }
        h
    }
    fn build_body(&self, ctx: &Context) -> serde_json::Value {
        // system は先頭の role=system メッセージへ載せるだけ（core が組んだものをそのまま置く）。
        // 空（非 Agent ターン等）のときは付けない。
        let mut messages: Vec<serde_json::Value> = vec![];
        if !ctx.system.is_empty() {
            messages.push(serde_json::json!({"role": "system", "content": ctx.system}));
        }
        // 最初の user メッセージ（＝場のログから 1 度組んだ rendered）に、積んだ history を平坦形で続ける。
        messages.push(serde_json::json!({
            "role": "user", "content": ctx.rendered,
        }));
        for m in &ctx.history {
            match m.role {
                MsgRole::User => {
                    // user 側のブロック: text は role=user、tool_result は role=tool（tool_call_id で対にする）。
                    let mut text = String::new();
                    for b in &m.content {
                        match b {
                            Block::Text(t) => {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                            Block::ToolResult {
                                tool_use_id,
                                content,
                                ..
                            } => {
                                messages.push(serde_json::json!({
                                    // マルチパート（DESIGN-images §4）: 画像は image_url の data URI へ写す。
                                    // テキストだけなら従来どおり文字列（後方互換）。
                                    "role": "tool", "tool_call_id": tool_use_id,
                                    "content": openai_tool_result_content(content),
                                }));
                            }
                            Block::ToolUse { .. } => {} // user 側に tool_use は来ない
                        }
                    }
                    if !text.is_empty() {
                        messages.push(serde_json::json!({"role": "user", "content": text}));
                    }
                }
                MsgRole::Assistant => {
                    // assistant 側: text は content、tool_use は tool_calls へ（同じ 1 メッセージに載せる）。
                    let mut text = String::new();
                    let mut tool_calls: Vec<serde_json::Value> = vec![];
                    for b in &m.content {
                        match b {
                            Block::Text(t) => {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                            Block::ToolUse { id, name, input } => {
                                tool_calls.push(serde_json::json!({
                                    "id": id, "type": "function",
                                    "function": {"name": name, "arguments": input.to_string()},
                                }));
                            }
                            Block::ToolResult { .. } => {} // assistant 側に tool_result は来ない
                        }
                    }
                    let mut msg = serde_json::json!({"role": "assistant"});
                    if !text.is_empty() {
                        msg["content"] = serde_json::json!(text);
                    }
                    if !tool_calls.is_empty() {
                        msg["tool_calls"] = serde_json::json!(tool_calls);
                    }
                    messages.push(msg);
                }
            }
        }
        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "stream": true,
            "messages": messages,
        });
        // 道具を宣言する（§10）。宣言しない道具をモデルは呼べない。core が `check` を通したものだけが載る。
        if !ctx.tools.is_empty() {
            let tools: Vec<serde_json::Value> = ctx
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.params,
                        },
                    })
                })
                .collect();
            body["tools"] = serde_json::json!(tools);
        }
        body
    }
    fn parse_event(&self, data: &serde_json::Value) -> Vec<Delta> {
        // OpenAI: chat.completion.chunk。choices[0] を見る（この系は 1 choice）。
        if data.get("error").is_some() || data.get("type").and_then(|x| x.as_str()) == Some("error")
        {
            return vec![Delta::Error(provider_error_message(data))];
        }
        let choice = match data
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
        {
            Some(c) => c,
            None => return vec![], // 使用量だけの行など choices が無いイベントは無視
        };
        let mut out: Vec<Delta> = vec![];
        let delta = choice.get("delta");
        // 本文の断片。
        if let Some(text) = delta
            .and_then(|d| d.get("content"))
            .and_then(|x| x.as_str())
        {
            if !text.is_empty() {
                out.push(Delta::Text(text.to_string()));
            }
        }
        // tool_call の断片。1 エントリに id/name と arguments がまとまって来ることも、arguments だけ
        // 後続で来ることもある——両方を落とさず並べる（id/name があれば開始、arguments があれば入力）。
        if let Some(tcs) = delta
            .and_then(|d| d.get("tool_calls"))
            .and_then(|x| x.as_array())
        {
            for tc in tcs {
                let index = tc.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                let id = tc.get("id").and_then(|x| x.as_str());
                let name = tc.pointer("/function/name").and_then(|x| x.as_str());
                if let (Some(id), Some(name)) = (id, name) {
                    out.push(Delta::ToolStart {
                        index,
                        id: id.to_string(),
                        name: name.to_string(),
                    });
                }
                if let Some(args) = tc.pointer("/function/arguments").and_then(|x| x.as_str()) {
                    if !args.is_empty() {
                        out.push(Delta::ToolInput {
                            index,
                            partial_json: args.to_string(),
                        });
                    }
                }
            }
        }
        // 終了理由。OpenAI に message_stop は無く finish_reason が終端印。`tool_calls` は core が続行と読む
        // `tool_use` へ写す（§07・終了理由の語彙を core 側に合わせる。近いものへ寄せるのではなく、同義の写像）。
        if let Some(fr) = choice.get("finish_reason").and_then(|x| x.as_str()) {
            let mapped = if fr == "tool_calls" { "tool_use" } else { fr };
            out.push(Delta::StopReason(mapped.to_string()));
            out.push(Delta::Done);
        }
        out
    }
}

/// ChatGPT のサブスクで動く推論（`gpt-5.6-luna` ほか）を、`codex/responses`（OpenAI Responses API・
/// ストリーミング SSE）で叩くプロバイダ。**codex の実装は読まず・持ち込まず**、既存 opencrab の運用が
/// 設定コメントに残した判断だけを持ってきた: 「codex サブプロセスは中で勝手にツールを実行して系の
/// 権限判定・記録・上限を素通りさせる」ので使わない、代わりに ChatGPT の口を **HTTP で直に**叩く。
///
/// **なぜこの口が方針に適うか（オーナー方針 1「自分で外に手を出さない相手だけ」）**:
/// この経路は `type:"function"` の道具だけを宣言し、モデルは `function_call`（＝**申し出**）を返す。
/// 実際に呼ぶのは core（`authorize_tool` → `invoke_or_detach`）。サーバ側で実行される道具（`web_search`
/// 等）は**宣言しない**——だから権限判定も記録も上限もこの口を通る。codex が落ちるのはここ。
///
/// 認証は ChatGPT の OAuth トークン（設定 `auth_file = ~/.codex/auth.json`。このファイルは ChatGPT の
/// 資格情報の置き場であって codex の実装ではない）。トークンは**毎推論ごとに読み直す**ので、外部（本番
/// opencrab など）がリフレッシュした新トークンをそのまま拾う。失効していれば**その場で正直に失敗**する
/// （既定値で埋めない・黙って再試行しない・§15）。自前の OAuth リフレッシュは持ち込まない——それには
/// codex 由来の公開 client_id が要り、方針「codex は参考にもしない」に触れるため（穴として報告する）。
pub struct ChatGptProvider {
    model: String,
    auth_file: String,
    reasoning_effort: Option<String>,
}

impl ChatGptProvider {
    pub fn new(model: impl Into<String>, auth_file: impl Into<String>) -> Self {
        ChatGptProvider {
            model: model.into(),
            auth_file: expand_tilde(&auth_file.into()),
            reasoning_effort: Some("low".to_string()),
        }
    }

    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort.filter(|s| !s.is_empty());
        self
    }

    /// auth.json を読み、`tokens.access_token` を返す（毎推論ごとに読み直す＝外部リフレッシュを拾う）。
    fn load_access_token(&self) -> Result<String, EngineError> {
        let content = std::fs::read_to_string(&self.auth_file)
            .map_err(|e| EngineError(format!("chatgpt auth file {}: {e}", self.auth_file)))?;
        let v: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| EngineError(format!("chatgpt auth.json is not JSON: {e}")))?;
        v["tokens"]["access_token"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| EngineError("chatgpt auth.json: tokens.access_token not found".into()))
    }
}

/// 本物の fetch（DESIGN-images §3 の seam の reqwest 実装）。core-look / core-read が使う。
/// TLS は reqwest に終端させる（provider と同じ・自前で張らない）。**自分でタイムアウトとサイズ上限を
/// 掛ける**（§3・core は結果だけ見る）——上限超過は `FetchError` で fail loud（core が理由を返す）。
pub struct ReqwestFetcher {
    client: reqwest::Client,
    /// ダウンロードの打ち切り上限（バイト）。**これはプロバイダの画像受理上限に合わせて調整する app 層の
    /// 政策**であって、core（プロトコル）には持たせない。実測で挟む——ここは安全側の初期値。
    byte_cap: usize,
}

impl ReqwestFetcher {
    pub fn new() -> ReqwestFetcher {
        // 8MB: Anthropic/OpenAI の画像受理上限（実測で調整）に対する安全側の初期値。タイムアウトは
        // 停止した取得でターンを長く握らないための歯止め（fetch はターン内同期・§3）。
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("reqwest client");
        ReqwestFetcher {
            client,
            byte_cap: 8 * 1024 * 1024,
        }
    }
}

impl Default for ReqwestFetcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl opencrab_port::Fetcher for ReqwestFetcher {
    async fn fetch(&self, url: &str) -> Result<opencrab_port::Fetched, opencrab_port::FetchError> {
        use opencrab_port::{FetchError, Fetched};
        // 取得先を http(s) に限る（file:// 等でローカルを読ませない・安全側）。
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(FetchError(format!("http(s) ではない URL: {url}")));
        }
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| FetchError(format!("接続に失敗: {e}")))?;
        if !resp.status().is_success() {
            return Err(FetchError(format!("HTTP {}", resp.status().as_u16())));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        // 断片ごとに上限を掛けながら読む（Content-Length を偽っても実バイトで打ち切る）。
        let mut resp = resp;
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| FetchError(format!("受信に失敗: {e}")))?
        {
            if bytes.len() + chunk.len() > self.byte_cap {
                return Err(FetchError(format!(
                    "大きすぎる（上限 {} バイトを超えた）",
                    self.byte_cap
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Fetched {
            content_type,
            bytes,
        })
    }
}

// ---- tool_result のマルチパート → 各 wire への写像（DESIGN-images §4）----

/// 標準 base64（パディングつき）で符号化する（外部依存なし・画像の data 化用）。
/// `base64url_decode` の対の役割だが、こちらは wire へ載せる標準アルファベット（`+/`）。
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Anthropic の tool_result content（text / image ブロックの配列）へ写す。画像は base64 の image source。
fn anthropic_tool_result_content(parts: &[Part]) -> serde_json::Value {
    let arr: Vec<serde_json::Value> = parts
        .iter()
        .map(|p| match p {
            Part::Text(t) => serde_json::json!({"type": "text", "text": t}),
            Part::ImageBytes { media_type, data } => serde_json::json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": base64_encode(data)},
            }),
        })
        .collect();
    serde_json::Value::Array(arr)
}

/// OpenAI chat の tool メッセージ content へ写す。テキストだけなら文字列（後方互換）、画像を含むなら
/// text / image_url（data URI）の配列。
fn openai_tool_result_content(parts: &[Part]) -> serde_json::Value {
    let has_image = parts.iter().any(|p| matches!(p, Part::ImageBytes { .. }));
    if !has_image {
        return serde_json::Value::String(parts_to_text(parts));
    }
    let arr: Vec<serde_json::Value> = parts
        .iter()
        .map(|p| match p {
            Part::Text(t) => serde_json::json!({"type": "text", "text": t}),
            Part::ImageBytes { media_type, data } => serde_json::json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{};base64,{}", media_type, base64_encode(data))},
            }),
        })
        .collect();
    serde_json::Value::Array(arr)
}

/// テキストパートだけを連結する（画像を載せられない wire・Responses の function_call_output 用）。
fn parts_to_text(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.as_str()),
            Part::ImageBytes { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// base64url（パディング任意）を復号する。JWT のペイロード取り出し用（外部依存なし）。
fn base64url_decode(input: &str) -> Result<Vec<u8>, EngineError> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let (mut buf, mut bits): (u32, u32) = (0, 0);
    for &c in input.as_bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = val(c).ok_or_else(|| EngineError("invalid base64url in JWT".into()))? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

/// JWT のペイロード（2 番目のセグメント）を JSON として取り出す。
fn jwt_payload(token: &str) -> Result<serde_json::Value, EngineError> {
    let seg = token
        .split('.')
        .nth(1)
        .ok_or_else(|| EngineError("chatgpt token is not a JWT".into()))?;
    let bytes = base64url_decode(seg)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| EngineError(format!("chatgpt token payload not JSON: {e}")))
}

/// JWT クレームから `chatgpt-account-id` を取り出す（Responses API のヘッダに要る）。
fn extract_account_id(token: &str) -> Result<String, EngineError> {
    let payload = jwt_payload(token)?;
    payload["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| EngineError("chatgpt token: chatgpt_account_id claim not found".into()))
}

/// access_token が失効しているか（`exp` を 60 秒前倒しで判定）。読めなければ失効扱いにせず送る
/// （判定不能を失敗に化けさせない——実際の可否はサーバの応答で決まる）。
fn token_expired(token: &str) -> bool {
    let Ok(payload) = jwt_payload(token) else {
        return false;
    };
    let Some(exp) = payload["exp"].as_i64() else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    exp - 60 <= now
}

/// 先頭の `~` を `HOME` に展開する。
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest)
    } else if path == "~" {
        std::env::var("HOME").unwrap_or_default()
    } else {
        path.to_string()
    }
}

#[async_trait::async_trait]
impl ChatProvider for ChatGptProvider {
    fn model(&self) -> &str {
        &self.model
    }
    // Responses の function_call_output はテキストのみ——画像を tool_result で運べないので core-look を
    // 出さない（DESIGN-images §6・「宣言しても呼べないものを渡さない」）。
    fn accepts_images(&self) -> bool {
        false
    }
    fn path(&self) -> String {
        // base_url（`https://chatgpt.com/backend-api`）に続けて叩く。
        "/codex/responses".to_string()
    }
    fn headers(&self) -> Vec<(String, String)> {
        // 認証以外の固定ヘッダ（accept は転送が付ける）。Responses API の版と発信元印。
        vec![
            ("content-type".into(), "application/json".into()),
            ("openai-beta".into(), "responses=experimental".into()),
            ("originator".into(), "pi".into()),
        ]
    }
    async fn prepared_headers(&self) -> Result<Vec<(String, String)>, EngineError> {
        // 毎回読み直す（外部リフレッシュを拾う）。失効・不備はその場で正直に失敗（§15）。
        let token = self.load_access_token()?;
        if token_expired(&token) {
            return Err(EngineError(
                "chatgpt access token expired — refresh ~/.codex/auth.json (外部で更新すれば次の推論で拾う)"
                    .into(),
            ));
        }
        let account_id = extract_account_id(&token)?;
        let mut h = self.headers();
        h.push(("authorization".into(), format!("Bearer {token}")));
        h.push(("chatgpt-account-id".into(), account_id));
        Ok(h)
    }
    fn build_body(&self, ctx: &Context) -> serde_json::Value {
        // Responses API の入力列。最初の user（＝場のログから 1 度組んだ rendered）に history を続ける（§05）。
        // 道具の往復は Responses の形（function_call / function_call_output）で対にして入れる——テキストに混ぜない。
        let mut input: Vec<serde_json::Value> = vec![serde_json::json!({
            "role": "user", "content": ctx.rendered,
        })];
        for m in &ctx.history {
            match m.role {
                MsgRole::User => {
                    let mut text = String::new();
                    for b in &m.content {
                        match b {
                            Block::Text(t) => {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                            Block::ToolResult {
                                tool_use_id,
                                content,
                                ..
                            } => {
                                input.push(serde_json::json!({
                                    "type": "function_call_output",
                                    "call_id": tool_use_id,
                                    // function_call_output はテキストのみ。この provider は accepts_images=false
                                    // なので core-look は来ない——テキストパートを連結する（DESIGN-images §4）。
                                    "output": parts_to_text(content),
                                }));
                            }
                            Block::ToolUse { .. } => {} // user 側に tool_use は来ない
                        }
                    }
                    if !text.is_empty() {
                        input.push(serde_json::json!({"role": "user", "content": text}));
                    }
                }
                MsgRole::Assistant => {
                    let mut text = String::new();
                    let mut calls: Vec<serde_json::Value> = vec![];
                    for b in &m.content {
                        match b {
                            Block::Text(t) => {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                            Block::ToolUse {
                                id,
                                name,
                                input: args,
                            } => {
                                calls.push(serde_json::json!({
                                    "type": "function_call",
                                    "call_id": id,
                                    "name": name,
                                    "arguments": args.to_string(),
                                }));
                            }
                            Block::ToolResult { .. } => {} // assistant 側に tool_result は来ない
                        }
                    }
                    if !text.is_empty() {
                        input.push(serde_json::json!({"role": "assistant", "content": text}));
                    }
                    // function_call は別アイテムとして並べる（Responses API の形）。
                    input.extend(calls);
                }
            }
        }
        let mut body = serde_json::json!({
            "model": self.model,
            "store": false,
            "stream": true,
            "input": input,
            "text": {"verbosity": "medium"},
            "tool_choice": "auto",
            "parallel_tool_calls": true,
        });
        // system は Responses API の `instructions` へ載せるだけ（core が組んだものをそのまま置く）。
        // 空（非 Agent ターン等）のときは付けない。
        if !ctx.system.is_empty() {
            body["instructions"] = serde_json::json!(ctx.system);
        }
        if let Some(effort) = self.reasoning_effort.as_deref() {
            body["reasoning"] = serde_json::json!({"effort": effort});
        }
        // 道具を宣言する（§10）。**`type:"function"` のみ**——サーバ側で実行される道具は宣言しない
        // （申し出だけを返させ、実行は core・オーナー方針 1）。core が check を通したものだけが載る。
        if !ctx.tools.is_empty() {
            let tools: Vec<serde_json::Value> = ctx
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.params,
                    })
                })
                .collect();
            body["tools"] = serde_json::json!(tools);
        }
        body
    }
    fn parse_event(&self, data: &serde_json::Value) -> Vec<Delta> {
        // Responses API はイベント種別を data の `type` が持つ（実戦のプロバイダ層の解釈と同じ）。
        match data.get("type").and_then(|x| x.as_str()) {
            // 本文の断片。
            Some("response.output_text.delta") => {
                match data.get("delta").and_then(|x| x.as_str()) {
                    Some(t) if !t.is_empty() => vec![Delta::Text(t.to_string())],
                    _ => vec![],
                }
            }
            // 完成した出力アイテム。function_call なら 1 イベントに name+call_id+arguments が揃って来る。
            // 開始（id/name）と入力（arguments）を同じ index で並べる（転送が対にして積む）。
            Some("response.output_item.done") | Some("response.output_item.completed") => {
                let item = data.get("item");
                let is_call = item.and_then(|i| i.get("type")).and_then(|x| x.as_str())
                    == Some("function_call");
                if !is_call {
                    return vec![];
                }
                let index = data
                    .get("output_index")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                let id = item
                    .and_then(|i| i.get("call_id").or_else(|| i.get("id")))
                    .and_then(|x| x.as_str());
                let name = item.and_then(|i| i.get("name")).and_then(|x| x.as_str());
                // arguments は文字列（JSON）で来る。無ければ空（転送が空を {} と解釈する）。
                let args = item
                    .and_then(|i| i.get("arguments"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                match (id, name) {
                    (Some(id), Some(name)) => vec![
                        Delta::ToolStart {
                            index,
                            id: id.to_string(),
                            name: name.to_string(),
                        },
                        Delta::ToolInput {
                            index,
                            partial_json: args.to_string(),
                        },
                    ],
                    _ => vec![],
                }
            }
            // 生成の終わり。output に function_call があればターンは続く（core が道具を呼ぶ・§07）。
            Some("response.completed") | Some("response.done") => {
                let has_call = data
                    .pointer("/response/output")
                    .and_then(|o| o.as_array())
                    .map(|arr| {
                        arr.iter().any(|it| {
                            it.get("type").and_then(|x| x.as_str()) == Some("function_call")
                        })
                    })
                    .unwrap_or(false);
                if has_call {
                    vec![Delta::StopReason("tool_use".to_string()), Delta::Done]
                } else {
                    vec![Delta::Done]
                }
            }
            // 200 応答の本文内で届いた失敗。握り潰さず Error にする（転送が EngineError に写す・§15）。
            Some("error") => {
                vec![Delta::Error(provider_error_message(data))]
            }
            _ => vec![],
        }
    }
}

/// HTTPS で SSE をストリーミングする `Engine` 実装（プロバイダ非依存の転送）。
///
/// TLS は `reqwest` に終端させる（自前で張らない）。テストは自己署名の根を信頼した `reqwest::Client` を
/// 差し込んで、本物の TLS 経路でアイドル上限が効くことを確かめる。
pub struct HttpSseEngine {
    base_url: String,
    provider: Box<dyn ChatProvider>,
    client: reqwest::Client,
}

impl HttpSseEngine {
    /// 既定のクライアント（webpki の根を信頼・rustls）。本番はこれで `https://api.anthropic.com` を叩く。
    pub fn new(base_url: impl Into<String>, provider: Box<dyn ChatProvider>) -> Self {
        HttpSseEngine {
            base_url: base_url.into(),
            provider,
            client: reqwest::Client::new(),
        }
    }

    /// クライアントを差し込む（テストが自己署名の根を信頼したクライアントを渡す）。
    pub fn with_client(
        base_url: impl Into<String>,
        provider: Box<dyn ChatProvider>,
        client: reqwest::Client,
    ) -> Self {
        HttpSseEngine {
            base_url: base_url.into(),
            provider,
            client,
        }
    }
}

#[async_trait::async_trait]
impl Engine for HttpSseEngine {
    fn model(&self) -> &str {
        // 予算の物差しは provider が持つモデル名（§06）。core が起動時に store の context_window で予算を確定する。
        self.provider.model()
    }

    fn accepts_images(&self) -> bool {
        self.provider.accepts_images()
    }

    async fn infer(&self, ctx: &Context, chunks: &ChunkSink) -> Result<InferOutput, EngineError> {
        let body = self.provider.build_body(ctx).to_string();
        // 外から来たもの（接続・応答）はどれも EngineError に写す（core は死なない・§15）。
        stream_request(
            &self.client,
            &self.base_url,
            self.provider.as_ref(),
            &body,
            chunks,
        )
        .await
    }
}

/// 設定（環境変数）から本物のプロバイダを選ぶ。設定が無ければ `Ok(None)`——app は echo に落ち着く。
/// 明示値が未知なら `Err` で起動を止め、echo へフォールバックしない。**プロバイダを足すときはここに 1 アーム**。
pub fn engine_from_env() -> Result<Option<Arc<dyn Engine>>, String> {
    let configured = match std::env::var("OPENCRAB_LLM_PROVIDER") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err("OPENCRAB_LLM_PROVIDER is not valid Unicode".to_string())
        }
    };
    let engine = match configured.as_deref() {
        Some("mock") => Some(Arc::new(MockEngine::from_env()?) as Arc<dyn Engine>),
        Some("anthropic") => {
            // 本番は https。手順書に合わせ、既定は本物の API。偽サーバのときだけ base を差し替える。
            let base = std::env::var("OPENCRAB_LLM_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
            // モデル ID は設定で渡す（手順書参照）。既定は現行の Claude（`claude-opus-5`）。
            let model =
                std::env::var("OPENCRAB_LLM_MODEL").unwrap_or_else(|_| "claude-opus-5".to_string());
            // 鍵は要求しない——無ければ空で組む（ビルド・テストは通る。実行時に本物を選べば API が拒否する）。
            let api_key = std::env::var("OPENCRAB_LLM_API_KEY")
                .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
                .unwrap_or_default();
            let max_tokens = std::env::var("OPENCRAB_LLM_MAX_TOKENS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(4096);
            Some(Arc::new(HttpSseEngine::new(
                base,
                Box::new(AnthropicProvider::new(model, api_key, max_tokens)),
            )) as Arc<dyn Engine>)
        }
        Some("openai") => {
            // OpenAI 互換の Chat Completions。既定は本物の OpenAI。この環境の橋（OpenAI 形式で Claude を
            // 出す）へ向けるときは `OPENCRAB_LLM_BASE_URL` と `OPENCRAB_LLM_MODEL` を差し替える（手順書参照）。
            let base = std::env::var("OPENCRAB_LLM_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com".to_string());
            let model =
                std::env::var("OPENCRAB_LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
            // 鍵は要求しない——無ければ空で組む（ローカルの橋は鍵を見ない。本物は Bearer で送る）。
            let api_key = std::env::var("OPENCRAB_LLM_API_KEY")
                .or_else(|_| std::env::var("OPENAI_API_KEY"))
                .unwrap_or_default();
            let max_tokens = std::env::var("OPENCRAB_LLM_MAX_TOKENS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(4096);
            Some(Arc::new(HttpSseEngine::new(
                base,
                Box::new(OpenAiProvider::new(model, api_key, max_tokens)),
            )) as Arc<dyn Engine>)
        }
        Some("chatgpt") => {
            // ChatGPT のサブスクで動く口（`gpt-5.6-luna` ほか）。本番は chatgpt.com のバックエンド。
            // 偽/ローカルサーバに向けるときだけ base を差し替える。
            let base = std::env::var("OPENCRAB_LLM_BASE_URL")
                .unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string());
            // 既定モデルは運用の既定（config の default_model）に合わせて `gpt-5.6-luna`。
            let model =
                std::env::var("OPENCRAB_LLM_MODEL").unwrap_or_else(|_| "gpt-5.6-luna".to_string());
            // 認証は ChatGPT の OAuth トークン（既定の置き場。codex の実装ではなく資格情報ファイル）。
            let auth_file = std::env::var("OPENCRAB_CHATGPT_AUTH_FILE")
                .unwrap_or_else(|_| "~/.codex/auth.json".to_string());
            // 推論強度は運用の既定に合わせて `low`（空文字で外せる）。
            let effort = std::env::var("OPENCRAB_LLM_REASONING_EFFORT")
                .ok()
                .or_else(|| Some("low".to_string()));
            let provider = ChatGptProvider::new(model, auth_file).with_reasoning_effort(effort);
            Some(Arc::new(HttpSseEngine::new(base, Box::new(provider))) as Arc<dyn Engine>)
        }
        Some("cursor") => {
            // 平文専用の頭（`cursor-agent` CLI・子プロセス）。HTTP ではないので `HttpSseEngine` には
            // 乗らず、`CursorEngine` が `Engine` を直に実装する（同クレート `cursor`）。
            let model = std::env::var("OPENCRAB_LLM_MODEL")
                .unwrap_or_else(|_| "cursor-grok-4.6-high".to_string());
            // バイナリ名はインストールでゆれる（cursor-agent / cursor / agent）。
            let binary = std::env::var("OPENCRAB_CURSOR_BINARY")
                .unwrap_or_else(|_| "cursor-agent".to_string());
            // `--sandbox` は既定 enabled（最安全）。`--plan` と直交する多層防御。
            let sandbox =
                std::env::var("OPENCRAB_CURSOR_SANDBOX").unwrap_or_else(|_| "enabled".to_string());
            // 鍵は要求しない——無ければ `cursor-agent login` 済みのアンビエント認証に任せる。
            let api_key = std::env::var("OPENCRAB_LLM_API_KEY")
                .or_else(|_| std::env::var("CURSOR_API_KEY"))
                .ok()
                .filter(|s| !s.is_empty());
            Some(Arc::new(crate::cursor::CursorEngine::new(
                model, binary, sandbox, api_key,
            )) as Arc<dyn Engine>)
        }
        // 未設定だけが echo を選ぶ。未知・空の明示値は近いものへ寄せない（§15）。
        None => None,
        Some(other) => return Err(format!("unknown OPENCRAB_LLM_PROVIDER: {other}")),
    };
    Ok(engine)
}

// ---- HTTP/SSE の転送（TLS は reqwest が終端。断片ごとに chunk() を叩く）----

/// POST してストリームを読み、断片が届くたび `chunks.chunk()` を叩きながら組み立てる。
///
/// 断片（reqwest が socket から読めたバイト）が届くたびに `chunk()` を叩くので、アイドルの上限は
/// **チャンク間**で効く（総時間ではない・§05）。止まった生成では `chunk()` が返らず、core が上限で切る
/// （infer の future を捨て、reqwest のリクエストが中断される）。
///
/// SSE は**行**で処理する（実戦の `sse.rs` の line_stream を持ち込み）: バイトをバッファし、`\n` 境界の
/// 完全な行だけを取り出す。1 チャンクに複数の `data:` があっても落とさず、チャンク境界を跨いだ行や
/// マルチバイト UTF-8 が壊れない。
async fn stream_request(
    client: &reqwest::Client,
    base_url: &str,
    provider: &dyn ChatProvider,
    body: &str,
    chunks: &ChunkSink,
) -> Result<InferOutput, EngineError> {
    let url = format!("{base_url}{}", provider.path());
    let mut req = client
        .post(&url)
        .header("accept", "text/event-stream")
        .body(body.to_string());
    // 認証の準備が非同期な口（OAuth）も差せるよう、載せるヘッダは prepared_headers から取る。
    // 既定は同期 headers() をそのまま返す（Anthropic/OpenAI は変わらない）。失敗は EngineError（§15）。
    for (k, v) in provider.prepared_headers().await? {
        req = req.header(k, v);
    }
    let mut resp = req
        .send()
        .await
        .map_err(|e| EngineError(format!("provider request: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        // 失敗は失敗として返す（近いものへ寄せない・握り潰さない・§15）。本文があれば添える。
        let detail = resp.text().await.unwrap_or_default();
        let detail = detail.chars().take(400).collect::<String>();
        return Err(EngineError(format!(
            "provider http status {}: {detail}",
            status.as_u16()
        )));
    }

    let mut buf: Vec<u8> = Vec::new();
    let mut acc = String::new();
    // index → (id, name, 積み上げ中の入力 JSON 文字列)。tool_use ブロックを跨いで組む。
    let mut tools: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
    let mut stop_reason: Option<String> = None;
    let mut done = false;
    // 200 応答の本文内で外から届いた失敗（`Delta::Error`）。握り潰さず、ループを抜けて EngineError に写す（§15）。
    let mut in_stream_error: Option<String> = None;

    // 断片を読む。届くたび chunk()（アイドルの計測が取り直される・§05）。
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| EngineError(format!("provider stream: {e}")))?
    {
        chunks.chunk();
        buf.extend_from_slice(&chunk);
        // バッファから \n 境界の完全な行だけを取り出す（チャンク跨ぎ・マルチバイト保護）。
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim();
            for delta in parse_sse_line(line, provider)? {
                apply_delta(
                    delta,
                    &mut acc,
                    &mut tools,
                    &mut stop_reason,
                    &mut done,
                    &mut in_stream_error,
                );
            }
        }
        if done || in_stream_error.is_some() {
            break;
        }
    }

    // 本文内の失敗は失敗として返す（近いものへ寄せない・握り潰さない・§15）。
    if let Some(msg) = in_stream_error {
        return Err(EngineError(format!("provider stream error: {msg}")));
    }
    require_terminal_event(done)?;

    // ツール呼び出しを組み立てる。壊れた JSON は握り潰さず失敗を返す（§15）。空入力は {}（正しい解釈）。
    let mut tool_calls: Vec<ToolCallSpec> = Vec::new();
    for (_index, (id, name, json)) in tools {
        let trimmed = json.trim();
        let args: serde_json::Value = if trimmed.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(trimmed)
                .map_err(|e| EngineError(format!("provider tool input JSON invalid: {e}")))?
        };
        tool_calls.push(ToolCallSpec { id, name, args });
    }

    // 本文があれば say にする。ツールだけのときは say を出さない（§08: 出す効果はある分だけ）。
    let effects = if acc.is_empty() {
        vec![]
    } else {
        vec![EffectSpec::say(acc)]
    };
    // stop_reason == tool_use ならターンは続く（core が道具を呼び、結果を積んで再推論する・§07）。
    // それ以外（end_turn / stop_sequence / max_tokens / 不明）は完了。
    let done_turn = stop_reason.as_deref() != Some("tool_use");

    Ok(InferOutput {
        effects,
        tool_calls,
        done: done_turn,
    })
}

/// SSE の 1 行を解釈する。`data:` 行だけを見る（`event:` 行・コメント・空行は無視）。
/// `data:` の中身が壊れた JSON なら握り潰さず失敗を返す（外から来たもの・§15）。`[DONE]` は終端印。
fn parse_sse_line(line: &str, provider: &dyn ChatProvider) -> Result<Vec<Delta>, EngineError> {
    let data = match line.strip_prefix("data:") {
        Some(d) => d.trim(),
        None => return Ok(vec![]), // event: 行・keep-alive コメント・空行
    };
    if data.is_empty() {
        return Ok(vec![]);
    }
    if data == "[DONE]" {
        return Ok(vec![Delta::Done]);
    }
    let v: serde_json::Value = serde_json::from_str(data)
        .map_err(|e| EngineError(format!("provider SSE data is not JSON: {e}")))?;
    Ok(provider.parse_event(&v))
}

/// provider ごとの error event から、人が調べられる本文を一つの規則で取り出す。
/// 既知の message 欄が無い場合だけ固定文にし、未知 event 全体を error 扱いにはしない。
fn provider_error_message(data: &serde_json::Value) -> String {
    data.get("message")
        .and_then(|x| x.as_str())
        .or_else(|| data.pointer("/error/message").and_then(|x| x.as_str()))
        .unwrap_or("unknown error")
        .to_string()
}

fn require_terminal_event(done: bool) -> Result<(), EngineError> {
    if done {
        Ok(())
    } else {
        Err(EngineError(
            "provider stream ended before terminal event".to_string(),
        ))
    }
}

/// 断片を積む。転送側の状態はここだけが触る。
fn apply_delta(
    delta: Delta,
    acc: &mut String,
    tools: &mut BTreeMap<u64, (String, String, String)>,
    stop_reason: &mut Option<String>,
    done: &mut bool,
    err: &mut Option<String>,
) {
    match delta {
        Delta::Text(t) => acc.push_str(&t),
        Delta::ToolStart { index, id, name } => {
            tools.insert(index, (id, name, String::new()));
        }
        Delta::ToolInput {
            index,
            partial_json,
        } => {
            if let Some((_id, _name, json)) = tools.get_mut(&index) {
                json.push_str(&partial_json);
            }
            // 開始を見ていない index への入力は捨てる（started を見ていない断片・プロトコル§05 と同じ姿勢）。
        }
        Delta::StopReason(r) => *stop_reason = Some(r),
        Delta::Done => *done = true,
        // 最初の 1 件だけ保つ（以降のノイズで上書きしない）。転送がこれを EngineError に写す（§15）。
        Delta::Error(m) => {
            if err.is_none() {
                *err = Some(m);
            }
        }
        Delta::Ignore => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only_say(output: &InferOutput) -> &str {
        output.effects[0]
            .content
            .text
            .as_deref()
            .expect("mock Say text")
    }

    #[tokio::test]
    async fn mock_scripts_are_deterministic_and_tool_script_requires_a_result() {
        let (chunks, _rx) = ChunkSink::channel();
        let ctx = Context::default();

        let reply = MockEngine {
            script: MockScript::Reply,
        }
        .infer(&ctx, &chunks)
        .await
        .unwrap();
        assert_eq!(only_say(&reply), "mock reply");

        let history = MockEngine {
            script: MockScript::History,
        };
        let seed = history.infer(&ctx, &chunks).await.unwrap();
        assert_eq!(only_say(&seed), "mock history seed acknowledged");
        let history_ctx = Context {
            rendered: "[1] Synthetic: synthetic history seed\n[2] Agent: mock history seed acknowledged\n[3] Synthetic: synthetic history question\n"
                .to_string(),
            ..Context::default()
        };
        let answer = history.infer(&history_ctx, &chunks).await.unwrap();
        assert_eq!(only_say(&answer), "mock remembered synthetic history seed");

        let no_reply = MockEngine {
            script: MockScript::NoReply,
        }
        .infer(&ctx, &chunks)
        .await
        .unwrap();
        assert_eq!(only_say(&no_reply), "NO_REPLY");

        let prefixed = MockEngine {
            script: MockScript::PrefixedNoReply,
        }
        .infer(&ctx, &chunks)
        .await
        .unwrap();
        assert_eq!(only_say(&prefixed), "mock internal reasoning\nNO_REPLY");

        let engine = MockEngine {
            script: MockScript::ToolThenReply,
        };
        let first = engine.infer(&ctx, &chunks).await.unwrap();
        assert!(!first.done);
        assert_eq!(first.tool_calls.len(), 1);
        assert_eq!(first.tool_calls[0].name, "core-child-list");

        let with_result = Context {
            history: vec![opencrab_port::Message {
                role: MsgRole::User,
                content: vec![Block::ToolResult {
                    tool_use_id: "mock-tool-call-1".to_string(),
                    content: vec![Part::text("synthetic result")],
                    is_error: false,
                }],
            }],
            ..Context::default()
        };
        let second = engine.infer(&with_result, &chunks).await.unwrap();
        assert!(second.done);
        assert_eq!(only_say(&second), "mock reply after tool result");

        let plaintext = MockEngine {
            script: MockScript::PlaintextToolSettledReply,
        };
        assert!(!plaintext.emits_tool_calls());
        let first = plaintext.infer(&ctx, &chunks).await.unwrap();
        assert_eq!(only_say(&first), "nostr-whoami::{}\nNO_REPLY");
        let settled_ctx = Context {
            rendered: "=== 決着の対応 ===\n活動 #2\n発端入力: #1..#1\nsynthetic mention for plaintext_tool_settled_reply\n受理ツール: nostr-whoami args={}\n結果:\nnpub1synthetic"
                .to_string(),
            ..Context::default()
        };
        let second = plaintext.infer(&settled_ctx, &chunks).await.unwrap();
        assert_eq!(
            only_say(&second),
            "mock reply after settled plaintext tool result"
        );

        let clock = MockEngine {
            script: MockScript::ClockBatch,
        };
        let immediate_ctx = Context {
            rendered: "synthetic immediate clock probe".to_string(),
            ..Context::default()
        };
        let immediate = clock.infer(&immediate_ctx, &chunks).await.unwrap();
        assert_eq!(only_say(&immediate), "mock immediate clock reply");
        let batched_ctx = Context {
            rendered: "synthetic batched first\nsynthetic batched second".to_string(),
            ..Context::default()
        };
        let batched = clock.infer(&batched_ctx, &chunks).await.unwrap();
        assert_eq!(only_say(&batched), "mock batched pair");

        let answer_direct = MockEngine {
            script: MockScript::AnswerDirect,
        };
        let direct = answer_direct.infer(&ctx, &chunks).await.unwrap();
        assert_eq!(only_say(&direct), "synthetic-direct-answer");
        assert!(
            !only_say(&direct).contains("NO_REPLY"),
            "answer_direct must never emit NO_REPLY"
        );

        let shell = MockEngine {
            script: MockScript::ShellThenRead,
        };
        assert!(!shell.emits_tool_calls());
        let first_shell = shell.infer(&ctx, &chunks).await.unwrap();
        assert_eq!(only_say(&first_shell), r#"core-shell::{"argv":["date"]}"#);
        let settled_shell_ctx = Context {
            rendered: "=== 決着の対応 ===\n活動 #2\n発端入力: #1..#1\n受理ツール: core-shell args={\"argv\":[\"date\"]}\n結果:\nMon Aug 24 01:00:00 UTC 2026\n"
                .to_string(),
            ..Context::default()
        };
        let second_shell = shell.infer(&settled_shell_ctx, &chunks).await.unwrap();
        assert!(
            only_say(&second_shell).contains("synthetic-shell-result"),
            "second turn must recite the result: {}",
            only_say(&second_shell)
        );
        assert!(
            only_say(&second_shell).contains("Mon Aug 24 01:00:00 UTC 2026"),
            "second turn must include the Settled body: {}",
            only_say(&second_shell)
        );

        let mixed = MockEngine {
            script: MockScript::AnswerThenNoReply,
        };
        let mixed_out = mixed.infer(&ctx, &chunks).await.unwrap();
        assert_eq!(only_say(&mixed_out), "synthetic-mixed-answer\nNO_REPLY");
        assert!(
            only_say(&mixed_out).contains("synthetic-mixed-answer"),
            "answer_then_no_reply must emit the substantive body"
        );
        assert!(
            only_say(&mixed_out).contains("NO_REPLY"),
            "answer_then_no_reply must emit the NO_REPLY sentinel"
        );

        let shell_fail = MockEngine {
            script: MockScript::ShellFailThenRead,
        };
        assert!(!shell_fail.emits_tool_calls());
        let first_fail = shell_fail.infer(&ctx, &chunks).await.unwrap();
        assert_eq!(only_say(&first_fail), r#"core-shell::{"argv":["ls"]}"#);
        let settled_fail_ctx = Context {
            rendered: "=== 決着の対応 ===\n活動 #2\n発端入力: #1..#1\n受理ツール: core-shell args={\"argv\":[\"ls\"]}\n結果:\nコマンド «ls» は許可されていない\n"
                .to_string(),
            ..Context::default()
        };
        let second_fail = shell_fail.infer(&settled_fail_ctx, &chunks).await.unwrap();
        assert!(
            only_say(&second_fail).contains("synthetic-shell-failure"),
            "second turn must recite the failure: {}",
            only_say(&second_fail)
        );
        assert!(
            only_say(&second_fail).contains("許可されていない"),
            "second turn must include the failure reason: {}",
            only_say(&second_fail)
        );
        assert!(
            !only_say(&second_fail).contains("NO_REPLY"),
            "settled failure turn must reply like shell_then_read: {}",
            only_say(&second_fail)
        );

        let offload = MockEngine {
            script: MockScript::ShellOffloadThenRead,
        };
        assert!(offload.emits_tool_calls());
        let first_offload = offload.infer(&ctx, &chunks).await.unwrap();
        assert_eq!(first_offload.tool_calls.len(), 1);
        assert_eq!(first_offload.tool_calls[0].name, "core-shell");
        let settled_offload_ctx = Context {
            rendered: "=== 決着の対応 ===\n活動 activity=20\n受理ツール: core-shell args={\"argv\":[\"awk\"]}\n結果:\n活動 #20 が完了した（成功）。結果が大きいので退避した。読むには core-bg-read（activity=20・start_line・line_count）を呼ぶ。\n"
                .to_string(),
            ..Context::default()
        };
        let second_offload = offload.infer(&settled_offload_ctx, &chunks).await.unwrap();
        assert_eq!(second_offload.tool_calls.len(), 1);
        assert_eq!(second_offload.tool_calls[0].name, "core-bg-read");
        assert_eq!(
            second_offload.tool_calls[0].args,
            serde_json::json!({ "activity": 20 })
        );
        let read_ctx = Context {
            rendered: settled_offload_ctx.rendered.clone(),
            history: vec![opencrab_port::Message {
                role: MsgRole::User,
                content: vec![Block::ToolResult {
                    tool_use_id: "mock-bg-read".to_string(),
                    content: vec![Part::text(format!(
                        "活動 #20 の退避 1〜5 行目（全 500 行）:\n{OFFLOAD_MARKER_LINE}\n"
                    ))],
                    is_error: false,
                }],
            }],
            ..Context::default()
        };
        let third_offload = offload.infer(&read_ctx, &chunks).await.unwrap();
        assert!(
            only_say(&third_offload).contains("synthetic-offload-read"),
            "third turn must recite the offload slice: {}",
            only_say(&third_offload)
        );
        assert!(
            only_say(&third_offload).contains(OFFLOAD_MARKER_LINE),
            "third turn must include the offload marker: {}",
            only_say(&third_offload)
        );

        let inprogress = MockEngine {
            script: MockScript::ShellThenBgReadBeforeSettle,
        };
        assert!(inprogress.emits_tool_calls());
        let first_inprogress = inprogress.infer(&ctx, &chunks).await.unwrap();
        assert_eq!(first_inprogress.tool_calls.len(), 1);
        assert_eq!(first_inprogress.tool_calls[0].name, "core-shell");
        assert_eq!(
            first_inprogress.tool_calls[0].args,
            serde_json::json!({ "argv": ["sleep", "30"] })
        );
        let detached_ctx = Context {
            history: vec![opencrab_port::Message {
                role: MsgRole::User,
                content: vec![Block::ToolResult {
                    tool_use_id: "mock-shell-before-settle".to_string(),
                    content: vec![Part::text("背景へ移した（活動 20）")],
                    is_error: false,
                }],
            }],
            ..Context::default()
        };
        let second_inprogress = inprogress.infer(&detached_ctx, &chunks).await.unwrap();
        assert_eq!(second_inprogress.tool_calls.len(), 1);
        assert_eq!(second_inprogress.tool_calls[0].name, "core-bg-read");
        assert_eq!(
            second_inprogress.tool_calls[0].args,
            serde_json::json!({ "activity": 20 })
        );
        let read_inprogress_ctx = Context {
            history: vec![opencrab_port::Message {
                role: MsgRole::User,
                content: vec![Block::ToolResult {
                    tool_use_id: "mock-bg-read-before-settle".to_string(),
                    content: vec![Part::text(
                        "失敗: 活動 #20 はまだ決着していない（退避は決着後）",
                    )],
                    is_error: true,
                }],
            }],
            ..Context::default()
        };
        let third_inprogress = inprogress
            .infer(&read_inprogress_ctx, &chunks)
            .await
            .unwrap();
        assert!(
            only_say(&third_inprogress).contains("synthetic-inprogress-read"),
            "same-turn read must recite the in-progress state: {}",
            only_say(&third_inprogress)
        );
        assert!(
            only_say(&third_inprogress).contains("まだ決着していない"),
            "same-turn read must include the in-progress message: {}",
            only_say(&third_inprogress)
        );

        let trailing = MockEngine {
            script: MockScript::ProgressReplyTrailingEmpty,
        };
        let trailing_out = trailing.infer(&ctx, &chunks).await.unwrap();
        assert_eq!(
            only_say(&trailing_out),
            "PROGRESS::読み込み中\n\nreply:1:synthetic-qc-reply\n"
        );
        assert!(
            only_say(&trailing_out).ends_with('\n'),
            "trailing empty segment must be present in the raw script body"
        );
    }

    fn only_error(deltas: Vec<Delta>) -> String {
        match deltas.as_slice() {
            [Delta::Error(message)] => message.clone(),
            _ => panic!("expected exactly one error delta"),
        }
    }

    #[test]
    fn known_provider_error_events_are_not_ignored() {
        let anthropic = AnthropicProvider::new("m", "", 64);
        assert_eq!(
            only_error(anthropic.parse_event(&serde_json::json!({
                "type": "error",
                "error": {"message": "anthropic sentinel"}
            }))),
            "anthropic sentinel"
        );

        let openai = OpenAiProvider::new("m", "", 64);
        assert_eq!(
            only_error(openai.parse_event(&serde_json::json!({
                "error": {"message": "openai sentinel"}
            }))),
            "openai sentinel"
        );

        let responses = ChatGptProvider::new("m", "/unused-auth.json");
        assert_eq!(
            only_error(responses.parse_event(&serde_json::json!({
                "type": "error",
                "error": {"message": "responses sentinel"}
            }))),
            "responses sentinel"
        );
    }

    #[test]
    fn done_sentinel_is_an_explicit_terminal_event() {
        let provider = OpenAiProvider::new("m", "", 64);
        let deltas = parse_sse_line("data: [DONE]", &provider).unwrap();
        assert!(matches!(deltas.as_slice(), [Delta::Done]));
    }

    #[test]
    fn eof_without_terminal_event_is_an_error() {
        assert!(require_terminal_event(true).is_ok());
        let message = require_terminal_event(false).unwrap_err().0;
        assert_eq!(message, "provider stream ended before terminal event");
    }
}
