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

