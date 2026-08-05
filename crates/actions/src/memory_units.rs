//! 記憶の単位（宣言）道具 4 つ（issue #379 #376 段階1）。
//!
//! エージェントが自分の生ログ（memory_sessions）を**俯瞰**し、**範囲を読み**、まとまりを
//! **宣言**する道具。宣言は `node_type='unit'` / `source_type='declared'` で
//! `memory_index_nodes` に載る（v30 で CHECK 拡張）。既存の time-series topic
//! （`node_type='topic'`）とは別 `node_type` なので、索引ビルド・タグ整理・月次ロールアップの
//! worklist へ**構造的に混ざらない**（#379 監査で確定）。
//!
//! 全て **TRUSTED_ONLY**（`bridge::TRUSTED_ONLY_ACTIONS`）で Nostr（caller=Agent）からは
//! list_tools に出ず dispatch でも拒否される。読み取り 2 つ（survey / read）は整理ラン用の
//! `ORGANIZE_ALLOWED_TOOLS` にも入る。記録 2 つ（record / retract）は段階2 まで入れない。
//!
//! #394 で 5 つ目 `plan_next_memory_window` を足した。宣言ランが 1 回に提示する窓（範囲の
//! 始まりと広さ）を**本人が決める**ための道具で、宣言ランからのみ使う。
//!
//! 有界化（687 発話の塊を一度に吐かせない）: `read_my_history` は行数 + 総文字数の
//! ハードキャップ + カーソル。`survey_my_history` はバケット数に上限。**生ログは読むだけ**。

use async_trait::async_trait;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult};

/// `read_my_history` の行数ハードキャップ。
const READ_ROW_CAP: usize = 200;
/// `read_my_history` の総文字数ハードキャップ（本文の累計）。
const READ_CHAR_CAP: usize = 40_000;
/// `survey_my_history` のバケット数上限（既定）。
const SURVEY_DEFAULT_MAX_BUCKETS: usize = 60;
/// `survey_my_history` のバケット数上限（これ以上は要求されても丸める）。
const SURVEY_HARD_MAX_BUCKETS: usize = 400;
/// `read_my_history` の `around` の既定半径。
const AROUND_DEFAULT_RADIUS: i64 = 20;
/// `read_my_history` の `around` の半径上限。
const AROUND_MAX_RADIUS: i64 = 100;

/// 生ログ読み取り道具（`survey_my_history` / `search_my_history`）の返り値が収まるべき
/// トークン予算。
///
/// #294 のツール結果キャップ（[`opencrab_core::tool_result_log::TOOL_RESULT_TOKEN_LIMIT`]
/// = 2,500 トークン）を超えると、返り値は**丸ごとメタ情報のスタブに差し替えられる**。
/// 宣言ラン（#376 段階2）は生ログを読むことが本体なのに、地図（survey）や検索結果が
/// 544 バイトのスタブに潰れてエージェントがオリエンテーション不能になり、成果ゼロで
/// 反復上限に達した（#386）。**道具側で必ず上限内に収める**のがこの予算の役目。
///
/// 上限そのものではなく 2 割引いた値にするのは、`data` の外側に乗る余白を確実に飲み込む
/// ため:
/// - `ActionResult` ラッパ（`{"success":..,"data":..,"error":..,"side_effects":..}`）が
///   ~20 トークン。
/// - `search_my_history` が付ける全文への導線 `note`（予算判定の**後**に足す）が ~90 トークン。
/// - トークン推定（tiktoken 近似）のぶれ。
///
/// 実測（#386 / 本番コピーの最大エージェント）で、この予算に収めた結果の**ラッパ込み**
/// トークンは survey ~2,020 / search ~2,110 に収まり、上限 2,500 に対し 400 弱の余白が残る。
/// ここに収めておけば、ラッパや note を被せても #294 のキャップに掛からない。
pub(crate) const HISTORY_RESULT_TOKEN_BUDGET: usize =
    opencrab_core::tool_result_log::TOOL_RESULT_TOKEN_LIMIT * 8 / 10;

/// 読み取り道具の返り値に添える per-result インライン上限（#386）。
///
/// #294 のキャップ（[`opencrab_core::tool_result_log::TOOL_RESULT_TOKEN_LIMIT`]）そのもの。
/// エージェントが「1 回の結果はこれを超えると本文が捨てられる」と知り、`est_tokens` /
/// `estimated_tokens` と突き合わせて範囲を刻めるよう、結果に `inline_limit_tokens` として
/// 添える。**新しい上限ではなく、既存のキャップの可視化**（総コンテキスト予算とは別物）。
pub(crate) const INLINE_LIMIT_TOKENS: usize =
    opencrab_core::tool_result_log::TOOL_RESULT_TOKEN_LIMIT;

/// `HistorySurvey`（地図）の serialize 後トークン数が `budget_tokens` に収まるよう、
/// **古いバケットから**落とす。
///
/// バケットは新しい順（`survey_my_history` が `ORDER BY bkt DESC`）に並ぶ。宣言・俯瞰で
/// 手がかりになるのは基本的に直近側なので、あふれたぶんは古い側から削り `truncated=true`
/// を立てる。集計メタ（`total_logs` / `total_sessions` / id 範囲 / `total_buckets`）は
/// **常に残す**ので、バケットを削っても「どれだけの履歴が、どの id 範囲に広がっているか」
/// は失われない（地図の骨格は保つ）。
///
/// hour 粒度 × 数百バケットのように、既定 clamp（[`SURVEY_HARD_MAX_BUCKETS`]）内でも
/// 46KB に達しうる（実測 #386）。バケット数の上限だけでは 1 バケットあたりのサイズが
/// 効いてこず**トークン上限を保証できない**ので、serialize 実測でここへ収める。
fn fit_survey_to_budget(survey: &mut opencrab_db::queries::HistorySurvey, budget_tokens: usize) {
    fn survey_tokens(s: &opencrab_db::queries::HistorySurvey) -> usize {
        let json = serde_json::to_string(s).unwrap_or_default();
        opencrab_core::tokens::estimate_tokens(&json)
    }
    if survey_tokens(survey) <= budget_tokens {
        return;
    }
    // 新しい順の全バケットを退避し、「先頭 keep 件（＝新しい側）だけ残す」最大の keep を
    // 二分探索する。バケットを減らすほど単調にトークンが減るので二分探索が効く。
    let all = std::mem::take(&mut survey.buckets);
    let (mut lo, mut hi) = (0usize, all.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        survey.buckets = all[..mid].to_vec();
        if survey_tokens(survey) <= budget_tokens {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    survey.buckets = all[..lo].to_vec();
    survey.returned_buckets = lo;
    if lo < all.len() {
        survey.truncated = true;
    }
}

/// `estimate_only` で範囲全体を走査するときの上限（この行数/文字数を超えたら概算に切替）。
/// 本番最大でも 1 エージェント 16k ログ程度なので、単一範囲はこの内に収まる。
const ESTIMATE_SCAN_ROW_CAP: usize = 100_000;
const ESTIMATE_SCAN_CHAR_CAP: usize = 200_000_000;

/// `SessionLogRow` の並びを、`read_my_history` が返すのと同じ形で serialize したときの
/// 推定トークン数（tiktoken 実測）。読み取りは範囲が有界なので概算せず実測する。
fn rows_tokens(rows: &[opencrab_db::queries::SessionLogRow]) -> usize {
    let json = serde_json::to_string(rows).unwrap_or_default();
    opencrab_core::tokens::estimate_tokens(&json)
}

/// 1 行の content を、その行だけで `budget_tokens` に収まる長さまで**文字境界で**切り詰め、
/// 打ち切ったと分かる marker を append する（#386）。
///
/// 単一の巨大ログ（巨大な tool_result 等）は行を減らしても収まらない。丸ごと #294 で潰れて
/// スタブになる（＝ ws_read の無い宣言ランでは二度と読めない）より、先頭を見せて「ここで
/// 切った・全文は範囲を狭めるか ws_read で」と伝える方が前へ進める。通常サイズの行はここへ
/// 来ない（[`fit_rows_to_budget`] が単一行超過のときだけ呼ぶ）。
fn truncate_row_content_to_budget(
    row: &mut opencrab_db::queries::SessionLogRow,
    budget_tokens: usize,
) {
    const MARKER: &str = "…[本文はここで打ち切り: この 1 行が inline_limit_tokens を超えています。\
                          全文は範囲（radius / id 窓）を狭めるか、ws_read で退避ファイルを読んでください]";
    // marker ぶんの余白を引いた予算に content を収める。
    let content_budget =
        budget_tokens.saturating_sub(opencrab_core::tokens::estimate_tokens(MARKER) + 16);
    let full = row.content.chars().count();
    let orig = row.content.clone();
    let fits = |n: usize| {
        let mut probe = row.clone();
        probe.content = orig.chars().take(n).collect();
        rows_tokens(std::slice::from_ref(&probe)) <= content_budget
    };
    let (mut lo, mut hi) = (0usize, full);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let kept: String = orig.chars().take(lo).collect();
    row.content = format!("{kept}{MARKER}");
}

/// 返却行を **per-result インライン上限**（`budget_tokens`）に収まるよう、末尾（新しい側）
/// から落とす。落としたら「続きの先頭 id」を返す（`next_from_id` に使う）。
///
/// `read_my_history` の DB クエリは行数 + 文字数でしか切っておらず、40,000 文字ぶんを
/// 返すと #294 の 2,500 トークン上限を優に超えて**丸ごと潰される**。ここでトークンでも
/// 切っておくと、1 ページが必ずインライン上限に収まり、続きは cursor で読める（#386）。
fn fit_rows_to_budget(
    rows: &mut Vec<opencrab_db::queries::SessionLogRow>,
    budget_tokens: usize,
) -> Option<i64> {
    if rows_tokens(rows) <= budget_tokens {
        return None;
    }
    let all = std::mem::take(rows);
    // 収まる最大の keep 件数を二分探索（先頭 keep 件＝古い側＝ id 昇順で残す）。
    let (mut lo, mut hi) = (0usize, all.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if rows_tokens(&all[..mid]) <= budget_tokens {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        // 先頭 1 行すら上限超（単一の巨大ログ）。その 1 行の content を切って必ず収める。
        let mut first = all[0].clone();
        truncate_row_content_to_budget(&mut first, budget_tokens);
        let next_from_id = all.get(1).and_then(|r| r.id);
        *rows = vec![first];
        return next_from_id;
    }
    let next_from_id = all.get(lo).and_then(|r| r.id);
    *rows = all[..lo].to_vec();
    next_from_id
}

/// 生ログを日/時/週で俯瞰する（地図）。
pub struct SurveyMyHistoryAction;

#[async_trait]
impl Action for SurveyMyHistoryAction {
    fn name(&self) -> &str {
        "survey_my_history"
    }

    fn description(&self) -> &str {
        "自分の生ログを日/時/週で俯瞰する（地図）。バケットごとに件数・セッション数・id 範囲・種別内訳・content 文字数・推定トークン数（est_tokens=概算）を返す。est_tokens は「その範囲を read_my_history で読むとおよそ何トークンか」の目安。1 ツール結果は inline_limit_tokens を超えると本文が捨てられるので、大きいバケットは範囲を絞って読む。地図自体は必ず上限内に収まる（大きすぎる古いバケットは truncated=true で落ちる）。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "granularity": {
                    "type": "string",
                    "enum": ["day", "hour", "week"],
                    "description": "集計粒度（既定: day）",
                    "default": "day"
                },
                "max_buckets": {
                    "type": "integer",
                    "description": format!("返すバケット数の上限（既定 {SURVEY_DEFAULT_MAX_BUCKETS} / 最大 {SURVEY_HARD_MAX_BUCKETS}）。新しいバケットから返す。"),
                    "default": SURVEY_DEFAULT_MAX_BUCKETS
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let granularity = match args["granularity"].as_str() {
            Some(g @ ("day" | "hour" | "week")) => g,
            Some(other) => {
                return ActionResult::error(&format!(
                    "granularity は day / hour / week のいずれか（受領: {other}）"
                ))
            }
            None => "day",
        };
        let max_buckets = args["max_buckets"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(SURVEY_DEFAULT_MAX_BUCKETS)
            .clamp(1, SURVEY_HARD_MAX_BUCKETS);

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        match opencrab_db::queries::survey_my_history(
            &conn,
            &ctx.agent_id,
            granularity,
            max_buckets,
        ) {
            Ok(mut survey) => {
                // #294 のツール結果キャップに丸ごと潰される前に、道具側で必ず上限内へ
                // 収める（#386）。あふれたら古いバケットから落とす（メタは残す）。
                fit_survey_to_budget(&mut survey, HISTORY_RESULT_TOKEN_BUDGET);
                match serde_json::to_value(&survey) {
                    Ok(mut v) => {
                        // per-result のインライン上限を地図に添える（#386）。エージェントが
                        // est_tokens とこの上限を突き合わせて「刻むか読むか」を決められる。
                        v["inline_limit_tokens"] = json!(INLINE_LIMIT_TOKENS);
                        ActionResult::success(v)
                    }
                    Err(e) => ActionResult::error(&format!("survey のシリアライズに失敗: {e}")),
                }
            }
            Err(e) => ActionResult::error(&format!("survey_my_history に失敗しました: {e}")),
        }
    }
}

/// 引数 `key` が「意味のある文字列」か（空文字・空白のみ・非文字列は「指定なし」）。
///
/// モデルは使わないキーも `""` で埋めてくるので、`is_some` では「指定あり」と
/// 誤判定してしまう（#388）。トリムして中身があるものだけを指定ありとみなす。
fn meaningful_str(args: &serde_json::Value, key: &str) -> bool {
    args.get(key)
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty())
}

/// 引数 `key` が「意味のある整数」か（`0`・非整数は「指定なし」）。
///
/// モデルは使わない id 系キーも `0` で埋めてくる（#388）。id は 1 始まりなので
/// `0` は範囲としての意味を持たない。0 は「指定なし」として扱う。
fn meaningful_int(args: &serde_json::Value, key: &str) -> bool {
    args.get(key)
        .and_then(|v| v.as_i64())
        .is_some_and(|n| n != 0)
}

/// 生ログを範囲指定で読む（有界: 行数 + 文字数キャップ + カーソル）。
pub struct ReadMyHistoryAction;

#[async_trait]
impl Action for ReadMyHistoryAction {
    fn name(&self) -> &str {
        "read_my_history"
    }

    fn description(&self) -> &str {
        "自分の生ログを範囲指定で読む。指定は次のいずれか 1 つ: session_id（セッション単位）/ from_id+to_id（id 範囲）/ around_id(+radius)（ある id の前後）/ from_time+to_time（時刻範囲）。1 回の結果は inline_limit_tokens を超えると本文が捨てられるので、トークン上限でも打ち切り、続きは next_from_id を cursor_from_id に渡して読む。取る前に大きさを知りたいときは estimate_only=true を渡すと、本文を返さず件数と推定トークン数（estimated_tokens）と fits を返す。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "セッション単位で読む" },
                "from_id": { "type": "integer", "description": "id 範囲の下端（to_id と対で使う）" },
                "to_id": { "type": "integer", "description": "id 範囲の上端（from_id と対で使う）" },
                "around_id": { "type": "integer", "description": "この id の前後を読む" },
                "radius": { "type": "integer", "description": format!("around_id の前後件数（既定 {AROUND_DEFAULT_RADIUS} / 最大 {AROUND_MAX_RADIUS}）") },
                "from_time": { "type": "string", "description": "時刻範囲の開始（RFC3339。to_time と対で使う）" },
                "to_time": { "type": "string", "description": "時刻範囲の終了（RFC3339。from_time と対で使う）" },
                "cursor_from_id": { "type": "integer", "description": "続きを読む: 前回の next_from_id をここに渡す" },
                "estimate_only": { "type": "boolean", "description": "true なら本文を返さず、この範囲の件数（range_total）と推定トークン数（estimated_tokens）と fits（inline_limit_tokens に収まるか）だけ返す。取る前に大きさを測るのに使う。" }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        use opencrab_db::queries::HistoryFilter;

        // 排他的にどの範囲指定が来ているかを判定する。
        //
        // 判定は「キーの有無」ではなく「値が意味を持つか」で行う（#388）。
        // gpt-5.6-sol 等のモデルはスキーマの全プロパティを毎回 `""` / `0` で埋めてくる。
        // presence（`is_some`）で数えると全モードが立ち、必ず「範囲は 1 つだけ」で
        // 拒否されて生ログを 1 行も読めなくなる（実験で 29 回連続拒否・成果ゼロ）。
        // 空文字列・0・null は「指定なし」として扱い、実際に意味を持つ値だけをモードとして数える。
        let has_session = meaningful_str(args, "session_id");
        let has_id_range = meaningful_int(args, "from_id") || meaningful_int(args, "to_id");
        let has_around = meaningful_int(args, "around_id");
        let has_time = meaningful_str(args, "from_time") || meaningful_str(args, "to_time");

        let mode_count = [has_session, has_id_range, has_around, has_time]
            .iter()
            .filter(|b| **b)
            .count();
        if mode_count == 0 {
            return ActionResult::error(
                "範囲指定が必要です: session_id / from_id+to_id / around_id / from_time+to_time のいずれか",
            );
        }
        if mode_count > 1 {
            return ActionResult::error(
                "範囲指定は 1 つだけにしてください（session_id / id 範囲 / around / 時刻範囲）。\
                 使わない範囲は 0 か空文字にしてください",
            );
        }

        let filter = if has_session {
            HistoryFilter::Session(args["session_id"].as_str().unwrap().to_string())
        } else if has_id_range {
            // 片側だけ意味を持つ id 範囲も素直に解釈する（#388 追補）。
            // from_id だけ → そこから先を、to_id だけ → そこまでを読む。指定の無い側
            // （`0`・空・非整数）は開いた端（`i64::MIN` / `i64::MAX`）にする。0 を境界として
            // 使うと `from_id:5, to_id:0` が [0,5] に正規化されて逆向きに読まれ、黙って
            // 空や見当違いが返り、エージェントが理由の分からないまま彷徨う（今日の失敗の形）。
            // 有界化は既存の行数・文字数・トークンのキャップが担う。範囲が本当に空でも
            // `range_total=0` が返るので「なぜ空か」は伝わる。
            let from_id = args["from_id"]
                .as_i64()
                .filter(|&v| v != 0)
                .unwrap_or(i64::MIN);
            let to_id = args["to_id"]
                .as_i64()
                .filter(|&v| v != 0)
                .unwrap_or(i64::MAX);
            HistoryFilter::IdRange { from_id, to_id }
        } else if has_around {
            let center_id = match args["around_id"].as_i64() {
                Some(v) => v,
                None => return ActionResult::error("around_id は integer で指定してください"),
            };
            let radius = args["radius"]
                .as_i64()
                .unwrap_or(AROUND_DEFAULT_RADIUS)
                .clamp(1, AROUND_MAX_RADIUS);
            HistoryFilter::Around { center_id, radius }
        } else {
            let from_time = match args["from_time"].as_str() {
                Some(v) if !v.is_empty() => v.to_string(),
                _ => {
                    return ActionResult::error(
                        "from_time と to_time を両方指定してください（RFC3339）",
                    )
                }
            };
            let to_time = match args["to_time"].as_str() {
                Some(v) if !v.is_empty() => v.to_string(),
                _ => {
                    return ActionResult::error(
                        "from_time と to_time を両方指定してください（RFC3339）",
                    )
                }
            };
            HistoryFilter::TimeRange { from_time, to_time }
        };

        let cursor_from_id = args["cursor_from_id"].as_i64();
        let estimate_only = args["estimate_only"].as_bool().unwrap_or(false);

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };

        // estimate_only: 本文を返さず、範囲全体の件数と推定トークン数だけ返す（#386）。
        // 取る前に「この範囲は inline 上限に収まるか / 何回に刻むか」を判断できる。
        if estimate_only {
            let page = match opencrab_db::queries::read_my_history(
                &conn,
                &ctx.agent_id,
                &filter,
                None, // カーソル無視 = 範囲全体
                ESTIMATE_SCAN_ROW_CAP,
                ESTIMATE_SCAN_CHAR_CAP,
            ) {
                Ok(p) => p,
                Err(e) => {
                    return ActionResult::error(&format!("read_my_history に失敗しました: {e}"))
                }
            };
            let scanned_tokens = rows_tokens(&page.rows);
            // 走査上限を超えた（page.truncated）ら、走査ぶんから全体を線形に外挿する。
            let estimated_tokens = if page.truncated && page.returned > 0 {
                ((scanned_tokens as u128 * page.range_total.max(0) as u128) / page.returned as u128)
                    as usize
            } else {
                scanned_tokens
            };
            let fits = estimated_tokens <= INLINE_LIMIT_TOKENS;
            let chunks = estimated_tokens.div_ceil(HISTORY_RESULT_TOKEN_BUDGET.max(1));
            let suggestion = if fits {
                "この範囲は 1 回で読める".to_string()
            } else {
                format!(
                    "この範囲は inline_limit_tokens を超える。範囲を約 {chunks} 分割する\
                     （id 窓や radius を狭める）か、cursor_from_id で刻んで読む"
                )
            };
            return ActionResult::success(json!({
                "estimate_only": true,
                "range_total": page.range_total,
                "estimated_tokens": estimated_tokens,
                "estimate_approximate": page.truncated,
                "inline_limit_tokens": INLINE_LIMIT_TOKENS,
                "fits": fits,
                "suggestion": suggestion,
            }));
        }

        match opencrab_db::queries::read_my_history(
            &conn,
            &ctx.agent_id,
            &filter,
            cursor_from_id,
            READ_ROW_CAP,
            READ_CHAR_CAP,
        ) {
            Ok(mut page) => {
                // DB クエリは行数 + 文字数でしか切っていない。40,000 文字ぶんは #294 の
                // 2,500 トークン上限を超えて丸ごと潰れるので、トークンでも切って 1 ページを
                // 必ず inline 上限内に収める（#386）。落としたぶんは cursor で続きを読める。
                if let Some(next) = fit_rows_to_budget(&mut page.rows, HISTORY_RESULT_TOKEN_BUDGET)
                {
                    page.truncated = true;
                    page.next_from_id = Some(next);
                    page.returned = page.rows.len();
                }
                match serde_json::to_value(&page) {
                    Ok(mut v) => {
                        v["estimated_tokens"] = json!(rows_tokens(&page.rows));
                        v["inline_limit_tokens"] = json!(INLINE_LIMIT_TOKENS);
                        ActionResult::success(v)
                    }
                    Err(e) => ActionResult::error(&format!("history のシリアライズに失敗: {e}")),
                }
            }
            Err(e) => ActionResult::error(&format!("read_my_history に失敗しました: {e}")),
        }
    }
}

/// 生ログの範囲 `[from_id, to_id]` を 1 つの記憶として宣言する。
pub struct RecordMemoryUnitAction;

#[async_trait]
impl Action for RecordMemoryUnitAction {
    fn name(&self) -> &str {
        "record_memory_unit"
    }

    fn description(&self) -> &str {
        "自分の生ログの範囲 [from_id, to_id] を『1 つの記憶』として宣言する。title 必須・summary 任意・tags 任意。生ログは消さず、宣言は retract_memory_unit で取り消せる。重なり（1 範囲が複数の宣言に属する）は許される。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["from_id", "to_id", "title"],
            "properties": {
                "from_id": { "type": "integer", "description": "範囲の下端（生ログの id）" },
                "to_id": { "type": "integer", "description": "範囲の上端（生ログの id）" },
                "title": { "type": "string", "description": "この記憶のタイトル（必須）" },
                "summary": { "type": "string", "description": "要約（任意）" },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "付けるタグ名（任意・複数可・無い名前は新設）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let from_id = match args["from_id"].as_i64() {
            Some(v) => v,
            None => return ActionResult::error("from_id は integer で指定してください"),
        };
        let to_id = match args["to_id"].as_i64() {
            Some(v) => v,
            None => return ActionResult::error("to_id は integer で指定してください"),
        };
        let title = match args["title"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("title is required"),
        };
        let summary = args["summary"].as_str().unwrap_or("").to_string();
        let tags: Vec<String> = args["tags"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        // 範囲にこのエージェントの生ログが実在するか確認し、date_from/date_to を埋める
        // （他エージェントの id や空範囲を宣言させない）。
        let meta = match opencrab_db::queries::log_range_meta(&conn, &ctx.agent_id, from_id, to_id) {
            Ok(Some(m)) => m,
            Ok(None) => {
                return ActionResult::error(
                    "指定範囲にこのエージェントの生ログがありません（from_id / to_id を確認してください）",
                )
            }
            Err(e) => return ActionResult::error(&format!("範囲の確認に失敗しました: {e}")),
        };
        let now = chrono::Utc::now().to_rfc3339();
        let node = match opencrab_db::queries::record_memory_unit(
            &conn,
            &ctx.agent_id,
            &title,
            &summary,
            from_id,
            to_id,
            Some(&meta.min_created_at),
            Some(&meta.max_created_at),
            &now,
        ) {
            Ok(n) => n,
            Err(e) => return ActionResult::error(&format!("記憶の宣言に失敗しました: {e}")),
        };

        // タグ（任意）。宣言ノード id を topic_id として既存タグ基盤へ付与する。
        // 付与失敗は宣言自体を無効にしない（宣言は成立済み・agent が付け直せる）。
        let mut tag_error: Option<String> = None;
        if !tags.is_empty() {
            if let Err(e) =
                opencrab_db::queries::tag_topic(&conn, &ctx.agent_id, &node.id, &tags, &now)
            {
                tag_error = Some(format!("タグ付けに失敗しました: {e}"));
            }
        }

        ActionResult::success(json!({
            "unit_id": node.id,
            "short_id": node.short_id,
            "title": node.title,
            "from_id": from_id,
            "to_id": to_id,
            "date_from": node.date_from,
            "date_to": node.date_to,
            "logs_in_range": meta.count,
            "tags": tags,
            "tag_error": tag_error,
        }))
    }
}

/// 宣言ユニットを取り消す（宣言ノード + FTS + member のみ削除。生ログは不変）。
pub struct RetractMemoryUnitAction;

#[async_trait]
impl Action for RetractMemoryUnitAction {
    fn name(&self) -> &str {
        "retract_memory_unit"
    }

    fn description(&self) -> &str {
        "record_memory_unit で宣言した記憶を取り消す。宣言ノードと FTS 行と付けたタグの付与だけを消す。生ログには触らない（何度でもやり直せる）。宣言ユニット以外は消せない。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["unit_id"],
            "properties": {
                "unit_id": {
                    "type": "string",
                    "description": "取り消す宣言ユニットの short_id またはフル node_id"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let unit_id = match args["unit_id"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("unit_id is required"),
        };
        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        match opencrab_db::queries::retract_memory_unit(&conn, &ctx.agent_id, &unit_id) {
            Ok(full_id) => ActionResult::success(json!({
                "retracted": true,
                "unit_id": full_id,
            })),
            Err(e) => ActionResult::error(&format!("宣言の取り消しに失敗しました: {e}")),
        }
    }
}

/// 宣言ランの窓の広さ（生ログ件数）として本人が指定できる**下限**（#394）。
///
/// これより狭くすると材料が薄すぎて宣言が抽象論に落ちる（#313 の実測: 20 件では固有名詞の
/// 無い抽象タグしか出なかった）。加えて 1 ラン当たりの前進量が小さくなりすぎ、日次ゲートの
/// もとで生ログの流入に追いつかなくなる。
pub const DECLARE_WINDOW_MIN: i64 = 50;

/// 宣言ランの窓の広さ（生ログ件数）として本人が指定できる**上限**（#394）。
///
/// 上限の理由は 1 コールのプロンプト肥大。本番実測で**窓 300 のとき最終コールが 73k
/// トークン**だった（窓の中身は本人が `read_my_history` で読み進めるので、コンテキストは
/// 窓の広さに概ね比例する）。倍の 600 でおよそ 150k 級となり、200k コンテキストの内側に
/// 収まる最後のあたりになる。ここを超えると窓を広げた結果としてターンが途中で潰れ、
/// partial（位置据え置き）になって前へ進まない。
///
/// 運用側が `memory_declare.max_logs` にこれより大きい値を設定している場合は、そちらが
/// 上限になる（本人の指定が運用の既定より狭められることは無い）。**その `max` を取れるのは
/// config を持つラン側（`memory_declare::decide_declare`）だけ**なので、この道具は上限で
/// 丸めず**希望をそのまま記録する**。ここで 600 に丸めてしまうと、`max_logs = 1000` の運用で
/// 本人が 1000 と表明した瞬間に窓が 1000 → 600 へ**狭まる**（黙っていれば 1000 のままだった）。
/// 下限（[`DECLARE_WINDOW_MIN`]）は config に依らないので、こちらは道具の側でも丸めてよい。
pub const DECLARE_WINDOW_MAX: i64 = 600;

/// 次回の宣言ランの窓（開始位置と広さ）を本人が決める。
///
/// #394: 宣言ランは「どこからどこまでが 1 つの記憶かは本人が決める」設計なのに、**窓の
/// 境界と広さだけは機械が固定で決めていた**（カーソルは宣言内容と無関係に窓の終端へ進む）。
/// この道具は本人の希望を DB（`agent_memory_index_config.memory_declare_window`）へ書く。
/// **希望であって決定ではない**: ランの側が前進の下限・上限へ丸めてから使う（本人任せに
/// すると宣言ゼロ・同じ位置の指定で同じ窓を永久に再取得するループに入る / #374）。
pub struct PlanNextMemoryWindowAction;

#[async_trait]
impl Action for PlanNextMemoryWindowAction {
    fn name(&self) -> &str {
        "plan_next_memory_window"
    }

    fn description(&self) -> &str {
        "次に自分へ提示される「今回の範囲」（窓）の始まりと広さを自分で決める。まだ続いている\
         出来事の末尾を次回へ回したいときは next_from_id にその先頭の生ログ id を渡す（そこから\
         先は次の窓にもう一度現れる）。窓の終端を越えて宣言したときも next_from_id を宣言の\
         次の id にすれば、次の窓が宣言済みと重ならない。材料が薄い/濃いと感じたら window_size で\
         次回以降の窓の広さ（生ログ件数）を変えられる（この設定は変えるまで残る）。どちらも希望\
         として記録され、必ず前へ進むように丸められる。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "next_from_id": {
                    "type": "integer",
                    "description": "次回の窓をこの生ログ id から始める（この id 以降は次回へ回す）。今回の範囲の中を指せば末尾を持ち越し、範囲の外（先）を指せば宣言済みの続きから始まる。指定しない（0）なら今回の範囲の終わりまで進む。"
                },
                "window_size": {
                    "type": "integer",
                    "description": format!("次回以降の窓に入れる生ログ件数（下限 {DECLARE_WINDOW_MIN} / 上限は既定 {DECLARE_WINDOW_MAX}。運用の設定がそれより広ければその値）。一度決めると変えるまで効き続ける。")
                },
                "note": {
                    "type": "string",
                    "description": "そう決めた理由（任意）。記録に残るだけで、機械は解釈しない。"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        // `0` / 空文字は「指定なし」として扱う（モデルは使わないキーも埋めてくる / #388）。
        let next_from_id = args["next_from_id"].as_i64().filter(|&v| v > 0);
        let requested_size = args["window_size"].as_i64().filter(|&v| v > 0);
        let note = args["note"]
            .as_str()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        if next_from_id.is_none() && requested_size.is_none() && note.is_none() {
            return ActionResult::error(
                "next_from_id / window_size / note のいずれかを指定してください",
            );
        }

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };

        // 既存の希望に**上書き**する（指定しなかった項目は残す）。window_size だけ変えたい
        // ときに、前に書いた next_from_id が消えないように。
        let mut pref = match opencrab_db::queries::get_memory_declare_window(&conn, &ctx.agent_id) {
            Ok(p) => p.unwrap_or_default(),
            Err(e) => return ActionResult::error(&format!("窓の希望の読み取りに失敗: {e}")),
        };
        if let Some(v) = next_from_id {
            pref.next_from_id = Some(v);
        }
        // 広さの**下限だけ**ここで丸める。下限は config に依らないので、丸めた値をその場で
        // 返せば本人が実際の設定を確認できる。**上限は丸めない**——上限は運用の `max_logs` と
        // の `max` で決まり（[`DECLARE_WINDOW_MAX`] の doc）、config を持つのはラン側だけ
        // だから。ここで 600 に丸めると `max_logs = 1000` の運用で本人が 1000 と表明した
        // 瞬間に窓が 1000 → 600 へ狭まる（黙っていれば 1000 のままだった）。
        let recorded_size = requested_size.map(|v| v.max(DECLARE_WINDOW_MIN));
        if let Some(v) = recorded_size {
            pref.window_size = Some(v);
        }
        if note.is_some() {
            pref.note = note;
        }
        pref.updated_at = Some(chrono::Utc::now().to_rfc3339());

        if let Err(e) =
            opencrab_db::queries::set_memory_declare_window(&conn, &ctx.agent_id, Some(&pref))
        {
            return ActionResult::error(&format!("窓の希望の保存に失敗: {e}"));
        }

        ActionResult::success(json!({
            "next_from_id": pref.next_from_id,
            "window_size": pref.window_size,
            "window_size_raised_to_min": requested_size.is_some() && requested_size != recorded_size,
            "window_size_min": DECLARE_WINDOW_MIN,
            "window_size_max_default": DECLARE_WINDOW_MAX,
            "note": pref.note,
            "applies_to": "next_run",
            "hint": format!(
                "next_from_id はこのランの終わりに一度だけ使われます（必ず前へ進むよう丸められます）。\
                 window_size は変えるまで効き続けます。広さの上限は既定 {DECLARE_WINDOW_MAX} 件\
                 （運用の設定がそれより広ければその値）で、実際に使われた広さは次回の\
                 「今回の範囲」に出ます。"
            ),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{ActionContext, CallerIdentity};
    use serde_json::json;

    fn test_context() -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: opencrab_db::Db::from_connection(conn),
            workspace: std::sync::Arc::new(ws),
            last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: std::sync::Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test-model".to_string(),
                active_model: None,
                available_providers: vec!["mock".to_string()],
                gateway: "test".to_string(),
            })),
            caller: CallerIdentity::Owner,
        };
        (dir, ctx)
    }

    fn seed_logs(ctx: &ActionContext, n: usize) {
        let conn = ctx.db.lock().unwrap();
        for i in 0..n {
            opencrab_db::queries::insert_session_log(
                &conn,
                &opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: "agent-1".to_string(),
                    session_id: "session-1".to_string(),
                    log_type: "message".to_string(),
                    content: format!("発話 {i}"),
                    speaker_id: None,
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                },
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn survey_empty_history() {
        let (_d, ctx) = test_context();
        let r = SurveyMyHistoryAction.execute(&json!({}), &ctx).await;
        assert!(r.success);
        let data = r.data.unwrap();
        assert_eq!(data["total_logs"], 0);
        assert_eq!(data["granularity"], "day");
    }

    #[tokio::test]
    async fn read_requires_a_range() {
        let (_d, ctx) = test_context();
        let r = ReadMyHistoryAction.execute(&json!({}), &ctx).await;
        assert!(!r.success);
    }

    #[tokio::test]
    async fn read_rejects_multiple_ranges() {
        let (_d, ctx) = test_context();
        let r = ReadMyHistoryAction
            .execute(&json!({"session_id": "s", "around_id": 1}), &ctx)
            .await;
        assert!(!r.success);
    }

    #[tokio::test]
    async fn record_read_and_retract_roundtrip() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 4);

        // record
        let r = RecordMemoryUnitAction
            .execute(
                &json!({"from_id": 1, "to_id": 4, "title": "所有権の話", "tags": ["Rust"]}),
                &ctx,
            )
            .await;
        assert!(r.success, "record failed: {:?}", r.error);
        let data = r.data.unwrap();
        let short_id = data["short_id"].as_str().unwrap().to_string();
        assert_eq!(data["logs_in_range"], 4);
        assert_eq!(data["tags"][0], "Rust");
        assert!(data["tag_error"].is_null());

        // read (id range)
        let rr = ReadMyHistoryAction
            .execute(&json!({"from_id": 1, "to_id": 4}), &ctx)
            .await;
        assert!(rr.success);
        assert_eq!(rr.data.unwrap()["returned"], 4);

        // retract by short_id
        let rt = RetractMemoryUnitAction
            .execute(&json!({"unit_id": short_id}), &ctx)
            .await;
        assert!(rt.success, "retract failed: {:?}", rt.error);
        assert_eq!(rt.data.unwrap()["retracted"], true);
    }

    #[tokio::test]
    async fn record_rejects_empty_range() {
        let (_d, ctx) = test_context();
        // 生ログが無いので範囲は空 → エラー。
        let r = RecordMemoryUnitAction
            .execute(&json!({"from_id": 1, "to_id": 4, "title": "x"}), &ctx)
            .await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("生ログ"));
    }

    #[tokio::test]
    async fn retract_rejects_missing_unit() {
        let (_d, ctx) = test_context();
        let r = RetractMemoryUnitAction
            .execute(&json!({"unit_id": "nope"}), &ctx)
            .await;
        assert!(!r.success);
    }

    // ---- survey の返り値を上限内に収める（#386）----

    use opencrab_db::queries::{HistoryBucket, HistorySurvey};

    /// 種別内訳を詰めた「太い」バケットを `n` 件持つ survey を作る（新しい順を模す）。
    fn fat_survey(n: usize) -> HistorySurvey {
        let mut type_counts = std::collections::BTreeMap::new();
        type_counts.insert("speech".to_string(), 1234i64);
        type_counts.insert("system".to_string(), 987);
        type_counts.insert("tool_result".to_string(), 456);
        type_counts.insert("tool_call".to_string(), 321);
        type_counts.insert("inner_voice".to_string(), 210);
        let buckets: Vec<HistoryBucket> = (0..n)
            .map(|i| HistoryBucket {
                // 先頭ほど「新しい」ことにする（fit は先頭 keep 件を残す）。
                bucket: format!("2026-08-{:02}T{:02}", (n - i) / 24 % 28 + 1, (n - i) % 24),
                log_count: 300,
                session_count: 7,
                min_id: (i as i64) * 300,
                max_id: (i as i64) * 300 + 299,
                content_chars: 90_000,
                est_tokens: 60_000,
                type_counts: type_counts.clone(),
            })
            .collect();
        HistorySurvey {
            granularity: "hour".to_string(),
            total_logs: 300 * n as i64,
            total_sessions: 91,
            min_id: Some(0),
            max_id: Some(300 * n as i64),
            total_content_chars: 90_000 * n as i64,
            total_est_tokens: 60_000 * n as i64,
            total_buckets: n as i64,
            returned_buckets: n,
            truncated: false,
            buckets,
        }
    }

    fn tokens_of(s: &HistorySurvey) -> usize {
        opencrab_core::tokens::estimate_tokens(&serde_json::to_string(s).unwrap())
    }

    /// 大量・太いバケットでも、fit 後は予算に収まり、ラッパ込みでも #294 の上限未満。
    /// 集計メタ（総数・id 範囲・total_buckets）は落とさず、新しい側のバケットを残す。
    #[test]
    fn survey_fit_bounds_tokens_and_keeps_meta() {
        let mut survey = fat_survey(400);
        let head_before = survey.buckets[0].bucket.clone();
        assert!(
            tokens_of(&survey) > HISTORY_RESULT_TOKEN_BUDGET,
            "前提が崩れている（既に予算内）: {}",
            tokens_of(&survey)
        );

        fit_survey_to_budget(&mut survey, HISTORY_RESULT_TOKEN_BUDGET);

        // survey 単体で予算内。
        assert!(
            tokens_of(&survey) <= HISTORY_RESULT_TOKEN_BUDGET,
            "over budget: {}",
            tokens_of(&survey)
        );
        // ActionResult ラッパを被せても #294 の上限（2,500）未満。
        let wrapped = serde_json::to_string(&ActionResult::success(
            serde_json::to_value(&survey).unwrap(),
        ))
        .unwrap();
        assert!(
            opencrab_core::tokens::estimate_tokens(&wrapped)
                < opencrab_core::tool_result_log::TOOL_RESULT_TOKEN_LIMIT,
            "wrapped over inline limit: {}",
            opencrab_core::tokens::estimate_tokens(&wrapped)
        );
        // 集計メタは残る（バケットを削っても地図の骨格は保つ）。
        assert_eq!(survey.total_logs, 300 * 400);
        assert_eq!(survey.total_buckets, 400);
        assert_eq!(survey.min_id, Some(0));
        assert!(survey.truncated, "削ったら truncated が立つ");
        assert!(!survey.buckets.is_empty(), "地図が空になってはいけない");
        assert!(survey.buckets.len() < 400, "実際に削れている");
        assert_eq!(survey.returned_buckets, survey.buckets.len());
        // 新しい側（先頭）から残す。
        assert_eq!(survey.buckets[0].bucket, head_before);
    }

    /// 既に予算内の小さい survey は 1 バケットも削らない（truncated も立てない）。
    #[test]
    fn survey_fit_leaves_small_survey_untouched() {
        let mut survey = fat_survey(3);
        assert!(tokens_of(&survey) <= HISTORY_RESULT_TOKEN_BUDGET);
        fit_survey_to_budget(&mut survey, HISTORY_RESULT_TOKEN_BUDGET);
        assert_eq!(survey.buckets.len(), 3);
        assert!(!survey.truncated);
    }

    /// Action 経由（本番と同じ serialize 路）でも、最大バケット要求で上限内に収まる。
    #[tokio::test]
    async fn survey_action_fits_even_at_max_buckets() {
        let (_d, ctx) = test_context();
        // hour 粒度で沢山のバケットを作る（各時に 1 件ずつ、500 時間ぶん）。
        {
            let conn = ctx.db.lock().unwrap();
            for h in 0..500 {
                let ts = format!(
                    "2026-{:02}-{:02}T{:02}:00:00Z",
                    1 + h / 700,
                    1 + (h / 24) % 28,
                    h % 24
                );
                let content = format!("発話 {h} ").repeat(3);
                conn.execute(
                    "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, created_at) VALUES (?1,?2,?3,?4,?5)",
                    rusqlite::params!["agent-1", format!("s{h}"), "speech", content, ts],
                )
                .unwrap();
            }
        }
        let r = SurveyMyHistoryAction
            .execute(&json!({"granularity": "hour", "max_buckets": 400}), &ctx)
            .await;
        assert!(r.success);
        let wrapped = serde_json::to_string(&r).unwrap();
        let tokens = opencrab_core::tokens::estimate_tokens(&wrapped);
        assert!(
            tokens < opencrab_core::tool_result_log::TOOL_RESULT_TOKEN_LIMIT,
            "survey action over inline limit: {tokens}"
        );
        // 地図として最低限（総数と最低 1 バケット）は残る。
        let data = r.data.unwrap();
        assert!(data["total_logs"].as_i64().unwrap() >= 500);
        assert!(!data["buckets"].as_array().unwrap().is_empty());
        // サイズ地図として est_tokens と per-result 上限が載る（#386）。
        assert!(data["total_est_tokens"].as_i64().unwrap() > 0);
        assert_eq!(data["inline_limit_tokens"], json!(INLINE_LIMIT_TOKENS));
        assert!(data["buckets"][0]["est_tokens"].as_i64().is_some());
    }

    // ---- read: 取る前に大きさを知る / 1 ページを上限内に収める（#386）----

    /// 大量の生ログを積む（各 content は長め）。
    fn seed_big_logs(ctx: &ActionContext, n: usize, chars_each: usize) {
        let conn = ctx.db.lock().unwrap();
        let body = "あ".repeat(chars_each);
        for i in 0..n {
            conn.execute(
                "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, created_at)
                 VALUES ('agent-1', 's1', 'speech', ?1, ?2)",
                rusqlite::params![
                    format!("{body} {i}"),
                    format!("2026-08-01T00:00:{:02}Z", i % 60)
                ],
            )
            .unwrap();
        }
    }

    /// estimate_only は本文を返さず、件数・推定トークン・fits を返す。
    #[tokio::test]
    async fn read_estimate_only_reports_size_without_bodies() {
        let (_d, ctx) = test_context();
        seed_big_logs(&ctx, 60, 400); // 60 件 × ~400 文字 → 明らかに 2,500 トークン超

        let r = ReadMyHistoryAction
            .execute(
                &json!({"from_id": 1, "to_id": 60, "estimate_only": true}),
                &ctx,
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["estimate_only"], true);
        assert_eq!(data["range_total"], 60);
        assert!(data["estimated_tokens"].as_i64().unwrap() > INLINE_LIMIT_TOKENS as i64);
        assert_eq!(data["fits"], false);
        assert_eq!(data["inline_limit_tokens"], json!(INLINE_LIMIT_TOKENS));
        // 本文（rows）は返さない。
        assert!(data.get("rows").is_none());
        // 推定は、実際に取ったときのサイズと大きく食い違わない（同オーダー）。
        let full = ReadMyHistoryAction
            .execute(&json!({"from_id": 1, "to_id": 60}), &ctx)
            .await;
        let full_wrapped = serde_json::to_string(&full).unwrap();
        // 実結果は 1 ページに収まっている（下のテストで担保）が、推定は全 60 件ぶんなので
        // 実結果より大きいはず（推定 > 1 ページ）。少なくとも推定は正の値。
        assert!(data["estimated_tokens"].as_i64().unwrap() > 0);
        assert!(!full_wrapped.is_empty());
    }

    /// 通常の read は 1 ページを必ず inline 上限内に収め、estimated_tokens と上限を添える。
    #[tokio::test]
    async fn read_page_fits_inline_limit_and_reports_tokens() {
        let (_d, ctx) = test_context();
        seed_big_logs(&ctx, 60, 400);

        let r = ReadMyHistoryAction
            .execute(&json!({"from_id": 1, "to_id": 60}), &ctx)
            .await;
        assert!(r.success);
        // ラッパ込み（LLM が見る本文）で #294 の上限未満＝潰れない。
        let wrapped = serde_json::to_string(&r).unwrap();
        assert!(
            opencrab_core::tokens::estimate_tokens(&wrapped)
                < opencrab_core::tool_result_log::TOOL_RESULT_TOKEN_LIMIT,
            "read page over inline limit: {}",
            opencrab_core::tokens::estimate_tokens(&wrapped)
        );
        let data = r.data.unwrap();
        // トークンで打ち切ったので続きがある。
        assert_eq!(data["truncated"], true);
        assert!(data["next_from_id"].as_i64().is_some());
        assert!(data["returned"].as_u64().unwrap() < 60);
        assert!(data["estimated_tokens"].as_i64().unwrap() > 0);
        assert_eq!(data["inline_limit_tokens"], json!(INLINE_LIMIT_TOKENS));
    }

    // ---- #388: モデルは全プロパティを埋める。値ベース判定で正しいモードに解決する ----
    //
    // 既存の単体テストは引数を「そのモードのキーだけ」明示的に組むので、この失敗を
    // 再現しない（だから実験 2 回まで見つからなかった）。ここでは gpt-5.6-sol の実際の
    // 癖——スキーマの全プロパティを毎回 `""` / `0` で埋める——を模したうえで、意味の
    // ある値を 1 つだけ足し、各モードが正しく解決することを確認する。

    /// モデルが毎回埋めてくる「全プロパティが空値」の引数（read_my_history のスキーマ全て）。
    fn all_props_filled() -> serde_json::Value {
        json!({
            "session_id": "",
            "from_id": 0,
            "to_id": 0,
            "around_id": 0,
            "from_time": "",
            "to_time": "",
            "estimate_only": false,
            "cursor_from_id": 0
        })
    }

    /// `base` に `over` のキーを上書きした引数を作る。
    fn with_override(mut base: serde_json::Value, over: serde_json::Value) -> serde_json::Value {
        let obj = base.as_object_mut().unwrap();
        for (k, v) in over.as_object().unwrap() {
            obj.insert(k.clone(), v.clone());
        }
        base
    }

    /// 全プロパティが空値で埋まった状態＋意味のある `around_id` だけ → around モードで動く。
    /// これが #388 で 29 回連続拒否された、実際の呼び出しの形。
    #[tokio::test]
    async fn read_all_props_filled_plus_around_works() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 20); // id 1..20

        let args = with_override(all_props_filled(), json!({"around_id": 10}));
        let r = ReadMyHistoryAction.execute(&args, &ctx).await;
        assert!(
            r.success,
            "全プロパティ埋め＋around が『範囲は1つだけ』で拒否された: {:?}",
            r.error
        );
        assert!(r.data.unwrap()["returned"].as_u64().unwrap() > 0);
    }

    /// 各モードについて、他のキーが全部空値で埋まっていても正しく解決すること。
    #[tokio::test]
    async fn read_each_mode_resolves_when_others_are_empty_valued() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 20); // id 1..20 / session-1

        // session モード。
        let r = ReadMyHistoryAction
            .execute(
                &with_override(all_props_filled(), json!({"session_id": "session-1"})),
                &ctx,
            )
            .await;
        assert!(r.success, "session モードが拒否された: {:?}", r.error);
        assert!(r.data.unwrap()["returned"].as_u64().unwrap() > 0);

        // id 範囲モード。
        let r = ReadMyHistoryAction
            .execute(
                &with_override(all_props_filled(), json!({"from_id": 5, "to_id": 15})),
                &ctx,
            )
            .await;
        assert!(r.success, "id 範囲モードが拒否された: {:?}", r.error);
        assert_eq!(r.data.unwrap()["returned"], 11);

        // around モード。
        let r = ReadMyHistoryAction
            .execute(
                &with_override(all_props_filled(), json!({"around_id": 10})),
                &ctx,
            )
            .await;
        assert!(r.success, "around モードが拒否された: {:?}", r.error);
        assert!(r.data.unwrap()["returned"].as_u64().unwrap() > 0);

        // 時刻範囲モード（seed の created_at は「今」なので広い範囲で確実に捕まえる）。
        let r = ReadMyHistoryAction
            .execute(
                &with_override(
                    all_props_filled(),
                    json!({
                        "from_time": "2000-01-01T00:00:00Z",
                        "to_time": "2100-01-01T00:00:00Z"
                    }),
                ),
                &ctx,
            )
            .await;
        assert!(r.success, "時刻範囲モードが拒否された: {:?}", r.error);
        assert!(r.data.unwrap()["returned"].as_u64().unwrap() > 0);
    }

    /// 全プロパティ埋め＋意味のある値ゼロ → 「範囲が必要」で拒否される（誤って通さない）。
    #[tokio::test]
    async fn read_all_props_filled_but_no_meaningful_value_is_rejected() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 4);
        let r = ReadMyHistoryAction.execute(&all_props_filled(), &ctx).await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("範囲指定が必要"));
    }

    /// 意味のある値が 2 つあれば従来どおり排他で拒否する（排他は維持する / #388）。
    #[tokio::test]
    async fn read_two_meaningful_ranges_still_rejected() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 20);
        let r = ReadMyHistoryAction
            .execute(
                &with_override(
                    all_props_filled(),
                    json!({"session_id": "session-1", "around_id": 10}),
                ),
                &ctx,
            )
            .await;
        assert!(!r.success);
        let err = r.error.unwrap();
        assert!(err.contains("1 つだけ"));
        // 全プロパティを埋めるモデルが 1 回で復帰できるよう「他をどう消すか」を示す。
        assert!(
            err.contains("0 か空文字"),
            "拒否メッセージに復帰方法が無い: {err}"
        );
    }

    /// 全プロパティ埋め＋around＋estimate_only=true → 範囲判定を抜けて estimate が返る。
    /// 実験では estimate_only を 5 回渡したが、range 判定が先に落ちて一度も発火しなかった。
    #[tokio::test]
    async fn read_all_props_filled_estimate_only_now_reaches_estimate() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 20);
        let args = with_override(
            all_props_filled(),
            json!({"around_id": 10, "estimate_only": true}),
        );
        let r = ReadMyHistoryAction.execute(&args, &ctx).await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["estimate_only"], true);
        assert!(data.get("rows").is_none());
    }

    // ---- #388 追補: 片側だけの id 範囲を空結果にせず素直に読む ----

    /// `from_id` だけ意味あり（`to_id` は空値）→ そこから先を読む。全プロパティ埋めでも動く。
    #[tokio::test]
    async fn read_id_range_from_only_reads_onward() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 20); // id 1..20

        let args = with_override(all_props_filled(), json!({"from_id": 15}));
        let r = ReadMyHistoryAction.execute(&args, &ctx).await;
        assert!(
            r.success,
            "from_id 片側指定が拒否/空になった: {:?}",
            r.error
        );
        let data = r.data.unwrap();
        // id 15..20 の 6 件。0 を境界に使うと逆向き（1..15）になり返り値が変わる。
        assert_eq!(data["range_total"], 6, "from 15 以降を読むべき");
        assert_eq!(data["returned"], 6);
    }

    /// `to_id` だけ意味あり（`from_id` は空値）→ そこまでを読む。全プロパティ埋めでも動く。
    #[tokio::test]
    async fn read_id_range_to_only_reads_up_to() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 20); // id 1..20

        let args = with_override(all_props_filled(), json!({"to_id": 5}));
        let r = ReadMyHistoryAction.execute(&args, &ctx).await;
        assert!(r.success, "to_id 片側指定が拒否/空になった: {:?}", r.error);
        let data = r.data.unwrap();
        // id 1..5 の 5 件。
        assert_eq!(data["range_total"], 5, "5 まで読むべき");
        assert_eq!(data["returned"], 5);
    }

    // ---- #394: 次回の窓（境界と広さ）を本人が決める ----

    fn pref(ctx: &ActionContext) -> Option<opencrab_db::queries::DeclareWindowPref> {
        let conn = ctx.db.lock().unwrap();
        opencrab_db::queries::get_memory_declare_window(&conn, &ctx.agent_id).unwrap()
    }

    /// 位置と広さを書ける。指定しなかった項目は**前の指定を消さない**（部分更新）。
    #[tokio::test]
    async fn plan_window_records_and_merges_fields() {
        let (_d, ctx) = test_context();

        // 位置だけ表明する。
        let r = PlanNextMemoryWindowAction
            .execute(
                &json!({"next_from_id": 23_600, "note": "この出来事はまだ続いている"}),
                &ctx,
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        let p = pref(&ctx).expect("希望が保存される");
        assert_eq!(p.next_from_id, Some(23_600));
        assert_eq!(p.window_size, None);
        assert_eq!(p.note.as_deref(), Some("この出来事はまだ続いている"));
        assert!(p.updated_at.is_some());

        // 広さだけ表明しても、位置は消えない（部分更新）。
        let r = PlanNextMemoryWindowAction
            .execute(&json!({"window_size": 450}), &ctx)
            .await;
        assert!(r.success, "{:?}", r.error);
        let p = pref(&ctx).unwrap();
        assert_eq!(p.next_from_id, Some(23_600), "位置が消えてはいけない");
        assert_eq!(p.window_size, Some(450));
    }

    /// 広さの**下限だけ**道具が丸め、**上限は丸めない**（上限は運用の `max_logs` との `max` で
    /// 決まり、config を持つのはラン側だけ）。ここで 600 に丸めると、`max_logs` を 600 超に
    /// している運用で本人が表明した瞬間に窓が**狭まる**（黙っていれば広いままだった）。
    #[tokio::test]
    async fn plan_window_raises_to_min_but_does_not_cap_at_max() {
        let (_d, ctx) = test_context();

        // 上限より大きい希望は**そのまま記録する**（ラン側が config を見て丸める）。
        let r = PlanNextMemoryWindowAction
            .execute(&json!({"window_size": DECLARE_WINDOW_MAX + 400}), &ctx)
            .await;
        assert!(r.success);
        let data = r.data.unwrap();
        assert_eq!(data["window_size"], json!(DECLARE_WINDOW_MAX + 400));
        assert_eq!(data["window_size_raised_to_min"], json!(false));
        assert_eq!(
            pref(&ctx).unwrap().window_size,
            Some(DECLARE_WINDOW_MAX + 400),
            "道具が上限で丸めると、広い max_logs の運用で窓がむしろ狭まる"
        );
        // 上限の既定は伝える（本人が「そのまま通る」と誤解しないように）。
        assert_eq!(data["window_size_max_default"], json!(DECLARE_WINDOW_MAX));

        // 下限は config に依らないので道具が丸め、丸めたことを伝える。
        let r = PlanNextMemoryWindowAction
            .execute(&json!({"window_size": 1}), &ctx)
            .await;
        assert!(r.success);
        let data = r.data.unwrap();
        assert_eq!(data["window_size"], json!(DECLARE_WINDOW_MIN));
        assert_eq!(data["window_size_raised_to_min"], json!(true));
        assert_eq!(pref(&ctx).unwrap().window_size, Some(DECLARE_WINDOW_MIN));

        // 範囲内はそのまま。
        let r = PlanNextMemoryWindowAction
            .execute(&json!({"window_size": 200}), &ctx)
            .await;
        let data = r.data.unwrap();
        assert_eq!(data["window_size"], json!(200));
        assert_eq!(data["window_size_raised_to_min"], json!(false));
    }

    /// 全プロパティを空値で埋めてくるモデル（#388 の癖）は「指定なし」として拒否する。
    /// `0` を位置として呑むと、本人が意図しない巻き戻しの希望が立ってしまう。
    #[tokio::test]
    async fn plan_window_rejects_all_empty_values() {
        let (_d, ctx) = test_context();
        let r = PlanNextMemoryWindowAction
            .execute(
                &json!({"next_from_id": 0, "window_size": 0, "note": "  "}),
                &ctx,
            )
            .await;
        assert!(!r.success);
        assert_eq!(pref(&ctx), None, "何も書かれてはいけない");
    }

    /// 範囲が本当に該当なしなら、空であること（range_total=0）が返る（「なぜ空か」を伝える）。
    #[tokio::test]
    async fn read_id_range_genuinely_empty_reports_zero_total() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 20); // id 1..20

        // 100..200 には生ログが無い（両側とも意味を持つが該当なし）。
        let args = with_override(all_props_filled(), json!({"from_id": 100, "to_id": 200}));
        let r = ReadMyHistoryAction.execute(&args, &ctx).await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(
            data["range_total"], 0,
            "該当なしは range_total=0 で明示する"
        );
        assert_eq!(data["returned"], 0);
    }
}
