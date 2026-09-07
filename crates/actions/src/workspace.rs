use async_trait::async_trait;
use opencrab_core::tool_result_log::READ_TOOL_RESULT_TOKEN_LIMIT;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult, SideEffect};

pub struct WsReadAction;

#[async_trait]
impl Action for WsReadAction {
    fn name(&self) -> &str {
        "ws_read"
    }

    fn description(&self) -> &str {
        "ワークスペース内のファイルを読み取る。既定で先頭から 2,000 行を返す（省略時も 1 ページ目）。\
         start_line で読み始める行、line_count で行数を指定できる。grep が返す行番号をそのまま \
         start_line に渡せる。続きがあれば has_more=true と next_line が付くので、next_line を \
         start_line に入れて辿る。1 行は最大 2,000 文字で切られ、切られた行の末尾には \
         ` …⟨+M文字⟩`（M は切り捨てた文字数）が付く（その行の残りは再取得できない）。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "読み取るファイルのパス（ワークスペースルートからの相対パス）"
                },
                "start_line": {
                    "type": "integer",
                    "description": "読み始める行（1 始まり。grep が返す行番号をそのまま渡せる）。省略時は先頭。has_more が true のとき next_line を渡すと続きを読める。",
                    "default": 1
                },
                "line_count": {
                    "type": "integer",
                    "description": format!("返す行数（省略時はトークン予算まで）。1 行は最大 {WS_READ_MAX_LINE_CHARS} 文字で切られ、切られた行の末尾には ` …⟨+M文字⟩`（M は切り捨てた文字数）が付く。")
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let path = match args["path"].as_str() {
            Some(p) => p.to_string(),
            None => return ActionResult::error("path is required"),
        };
        // start_line / line_count のどちらかが与えられたら行範囲読み。無ければ従来どおり全文
        // （後方互換）。旧 offset / limit（バイト）は未知キーとして無視され、全文読みへ落ちる。
        let start_line = args["start_line"].as_u64().unwrap_or(1).max(1) as usize;
        // 0 行要求は無意味。1 に丸めて、next_line が start_line から進まない暴走ページングを防ぐ。
        // #707: 既定 2,000 行（Claude Code の Read と同じ）。省略時に全文を返して退避する形を
        // やめ、範囲指定の有無に関わらず「1 ページ目」を返す。続きは has_more / next_line。
        let line_count = Some(
            args["line_count"]
                .as_u64()
                .map(|n| (n as usize).max(1))
                .unwrap_or(WS_READ_DEFAULT_LINES),
        );

        // 読み取り・計数・組み上げ・トークナイズを **すべて `spawn_blocking` の中** で行う
        // （マルチエージェントの async executor を同期 CPU で塞がない / #567）。返すのは有界な
        // 結果 JSON だけで、巨大な全文（実測 509MB）を executor 側へ持ち帰らない。
        let ws = ctx.workspace.clone();
        match tokio::task::spawn_blocking(move || {
            compute_ws_read(&ws, &path, start_line, line_count)
        })
        .await
        {
            Ok(Ok(v)) => ActionResult::success(v),
            Ok(Err(e)) => ActionResult::error(&e.to_string()),
            Err(e) => ActionResult::error(&format!("read task failed: {e}")),
        }
    }
}

/// `ws_read` の本体（同期・`spawn_blocking` 内で走る）。行範囲読みは単一 open の逐次読みで
/// ページを組み、有界な結果 JSON を返す。
///
/// **行範囲読みは全ファイル長ぶんの走査も全文読みもしない。** 規模（`total_bytes`）は
/// `metadata().len()`（O(1)）で得る。本文は [`Workspace::line_reader`] が 1 回 open した窓の
/// 中で `start_line` から 1 行ずつ、各行 [`WS_READ_MAX_LINE_CHARS`] 文字までを積み、標識込みの
/// 累計が [`RANGE_CONTENT_TOKEN_CEILING`] に達する直前で止める（返り値は必ず inline 上限未満＝
/// 再退避しない / #564 の自己ループを断つ）。二分探索もバイト境界補正も無い（#617）。範囲指定
/// なしの全文読みだけは従来どおり全体を読む（後方互換）。
///
/// [`Workspace::line_reader`]: opencrab_core::workspace::Workspace::line_reader
fn compute_ws_read(
    ws: &opencrab_core::workspace::Workspace,
    path: &str,
    start_line: usize,
    line_count: Option<usize>,
) -> anyhow::Result<serde_json::Value> {
    // 行範囲読み: 単一 open の逐次読みで `start_line` から 1 行ずつページに積む。各行は
    // WS_READ_MAX_LINE_CHARS 文字で切り（超過は ` …⟨+M文字⟩` の標識を付ける）、標識込みの累計
    // がトークン上限に達する直前で止める。二分探索もバイト境界補正も無い（#617）。
    //
    // 予算は**行トークンの走行合計**で判定する。累積 content を毎行トークナイズし直すと、ページ
    // 確定までに O(n²) のトークナイズが走る（149 行で ~740KB を tiktoken に通していた）。各 piece
    // を 1 度だけ数えて足すと O(n) になる。部分文字列を別々に数えた和は必ず全体の真値以上（分割で
    // トークンは減らない）で、`RANGE_CONTENT_TOKEN_CEILING` の −400 余白に収まるので、過小評価で
    // 再退避が起きることはない（[`tokens_reach_limit`] の窓和と同じ安全側 / #576）。改行結合ぶんも
    // 1 トークン上限として足す（`\n` は 1 トークンだが、跨ぐ結合で増えることはあっても減らない）。
    let (mut reader, total_bytes) = ws.line_reader(path, start_line, WS_READ_MAX_LINE_CHARS)?;
    let mut content = String::new();
    let mut used_tokens = 0usize;
    let mut first_line: Option<usize> = None;
    let mut next_line: Option<usize> = None;
    let mut lines_taken = 0usize;
    while let Some(line) = reader.next_line()? {
        // line_count が与えられていれば行数でも頭打ちにする（トークン予算とどちらか早い方）。
        if line_count.is_some_and(|lc| lines_taken >= lc) {
            next_line = Some(line.number); // まだ続きがある
            break;
        }
        let piece = if line.overflow_chars > 0 {
            format!("{} …⟨+{}文字⟩", line.text, line.overflow_chars)
        } else {
            line.text
        };
        // この行を足したときの上限側トークン見積り（改行結合ぶん +1）。
        let joiner = if content.is_empty() { 0 } else { 1 };
        let piece_tokens = opencrab_core::tokens::estimate_tokens(&piece) + joiner;
        // 予算判定。ページに 1 行も無いうちは無条件で 1 行返す（最低 1 行保証: 単独で予算超過の
        // 行でも 512 文字に切って返し next_line を前進させる。ここを空返しにすると next_line が
        // start_line から進まず、同じ行を読み続ける暴走ページングになる / #567 の趣旨を行版で保つ）。
        if !content.is_empty() && used_tokens + piece_tokens > RANGE_CONTENT_TOKEN_CEILING {
            next_line = Some(line.number); // この行は含めず、次回ここから読み直す
            break;
        }
        if content.is_empty() {
            content = piece;
        } else {
            content.push('\n');
            content.push_str(&piece);
        }
        used_tokens += piece_tokens;
        first_line.get_or_insert(line.number);
        lines_taken += 1;
    }

    // 出力の estimated_tokens は確定したページ本文（有界）を 1 度だけ正確に数える（O(n) 1 回）。
    let estimated_tokens = opencrab_core::tokens::estimate_tokens(&content);
    let mut out = json!({
        "path": path,
        "content": content,
        "total_bytes": total_bytes,
        "start_line": first_line.unwrap_or(start_line),
        "has_more": next_line.is_some(),
        "estimated_tokens": estimated_tokens,
        "inline_limit_tokens": READ_TOOL_RESULT_TOKEN_LIMIT,
    });
    if let Some(n) = next_line {
        out["next_line"] = json!(n);
    }
    Ok(out)
}

/// 行範囲読みで返す本文のトークン上限。結果 JSON 全体（本文＋メタ情報の封筒）が inline 上限
/// [`TOOL_RESULT_TOKEN_LIMIT`] を超えて再退避される（#564 の自己ループ）ことを構造的に防ぐため、
/// 封筒ぶんの余白を引いた保守値にする。封筒は keys＋数値＋`path`＋標識で実測数百トークン以内
/// なので、余裕を持って 400 引く。
const RANGE_CONTENT_TOKEN_CEILING: usize = READ_TOOL_RESULT_TOKEN_LIMIT - 400;

/// 行範囲読みで 1 行あたりに返す最大文字数（`char` 数）。超える行は切って ` …⟨+M文字⟩` を付ける。
///
/// 単位は**文字数**。テキストを扱うので文字で切れば境界補正が要らない（`chars().take(n)` で済む）。
///
/// 値 2,000（#707）は **Claude Code の Read ツールと同じ基準**。同じ仕事をする道具の刻み方が
/// 桁で違う理由が無い（オーナー指摘）。旧値 512 は当時のページ天井 2,100 トークンに単一行を
/// 収めるための逆算値で、天井が上がったので外れる（2,000 × 4 = 8,000 < 29,600 なので、最悪
/// 密度でも単一行が天井に収まる性質は保つ＝最低 1 行保証）。
const WS_READ_MAX_LINE_CHARS: usize = 2_000;

/// `line_count` 省略時に返す行数（#707）。**Claude Code の Read と同じ 2,000 行**。
///
/// 以前は省略時に**全文**を返していた。全文が上限を超えると `sanitize_tool_result` が中身を
/// 別ファイルへ複製して退避するため、読むたびに `workspace/tmp` が増えていた（本番実測:
/// 4,255 ファイル・270MB）。**元がファイルなのだから複製に意味が無い**（オーナー指摘）。
/// 省略時も 1 ページ目として返せば退避は起きず、参照は**元のファイル名がそのまま**使える。
const WS_READ_DEFAULT_LINES: usize = 2_000;

pub struct WsWriteAction;

#[async_trait]
impl Action for WsWriteAction {
    fn name(&self) -> &str {
        "ws_write"
    }

    fn description(&self) -> &str {
        "ワークスペース内にファイルを書き込む"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "書き込むファイルのパス（ワークスペースルートからの相対パス）"
                },
                "content": {
                    "type": "string",
                    "description": "ファイルの内容"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let path = match args["path"].as_str() {
            Some(p) => p,
            None => return ActionResult::error("path is required"),
        };
        let content = match args["content"].as_str() {
            Some(c) => c,
            None => return ActionResult::error("content is required"),
        };

        match ctx.workspace.write(path, content).await {
            Ok(_) => ActionResult::success(json!({
                "path": path,
                "written": true,
            }))
            .with_side_effect(SideEffect::FileWritten {
                path: path.to_string(),
            }),
            Err(e) => ActionResult::error(&e.to_string()),
        }
    }
}

pub struct WsEditAction;

#[async_trait]
impl Action for WsEditAction {
    fn name(&self) -> &str {
        "ws_edit"
    }

    fn description(&self) -> &str {
        "ワークスペース内のファイルを差分編集する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path", "old_string", "new_string"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "編集するファイルのパス"
                },
                "old_string": {
                    "type": "string",
                    "description": "置換対象の文字列（ユニークである必要がある）"
                },
                "new_string": {
                    "type": "string",
                    "description": "置換後の文字列"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let path = match args["path"].as_str() {
            Some(p) => p,
            None => return ActionResult::error("path is required"),
        };
        let old = match args["old_string"].as_str() {
            Some(o) => o,
            None => return ActionResult::error("old_string is required"),
        };
        let new = match args["new_string"].as_str() {
            Some(n) => n,
            None => return ActionResult::error("new_string is required"),
        };

        match ctx.workspace.edit(path, old, new).await {
            Ok(_) => ActionResult::success(json!({
                "path": path,
                "edited": true,
            })),
            Err(e) => ActionResult::error(&e.to_string()),
        }
    }
}

pub struct WsListAction;

#[async_trait]
impl Action for WsListAction {
    fn name(&self) -> &str {
        "ws_list"
    }

    fn description(&self) -> &str {
        "ワークスペース内のディレクトリを一覧表示する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "一覧表示するディレクトリのパス（デフォルト: ルート）",
                    "default": ""
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let path = args["path"].as_str().unwrap_or("");

        match ctx.workspace.list(path).await {
            Ok(entries) => {
                let entries_json: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|e| {
                        json!({
                            "name": e.name,
                            "is_dir": e.is_dir,
                            "size": e.size,
                        })
                    })
                    .collect();
                ActionResult::success(json!({
                    "path": path,
                    "entries": entries_json,
                }))
            }
            Err(e) => ActionResult::error(&e.to_string()),
        }
    }
}

pub struct WsDeleteAction;

#[async_trait]
impl Action for WsDeleteAction {
    fn name(&self) -> &str {
        "ws_delete"
    }

    fn description(&self) -> &str {
        "ワークスペース内のファイルまたはディレクトリを削除する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "削除するファイルまたはディレクトリのパス"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let path = match args["path"].as_str() {
            Some(p) => p,
            None => return ActionResult::error("path is required"),
        };

        match ctx.workspace.delete(path).await {
            Ok(_) => ActionResult::success(json!({
                "path": path,
                "deleted": true,
            })),
            Err(e) => ActionResult::error(&e.to_string()),
        }
    }
}

pub struct WsMkdirAction;

#[async_trait]
impl Action for WsMkdirAction {
    fn name(&self) -> &str {
        "ws_mkdir"
    }

    fn description(&self) -> &str {
        "ワークスペース内にディレクトリを作成する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "作成するディレクトリのパス"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let path = match args["path"].as_str() {
            Some(p) => p,
            None => return ActionResult::error("path is required"),
        };

        match ctx.workspace.mkdir(path).await {
            Ok(_) => ActionResult::success(json!({
                "path": path,
                "created": true,
            })),
            Err(e) => ActionResult::error(&e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests;
