//! スリープ凝縮ラン本体（#411 / 記憶の 3 段目）。
//!
//! **記憶の「意味」をエージェント自身に抽出させる。** 宣言ラン（#384）が生ログから「ここから
//! ここまでが一つの記憶（ユニット）だ」を本人に決めさせたのに対し、凝縮ランは**そのユニットたち
//! を俯瞰**して「その出来事たちが何を意味するか」という原則を本人に抽出させ、`node_type='meta'`
//! として人格の核に刻む（`record_memory_core`）。
//!
//! 宣言ランの**双子**として作る。本質的な違いは**入力だけ**——生ログではなく自分のユニット。
//! 足回りは宣言ラン・タグ整理ランと共有する（新しいエンジンや口を作らない）:
//! - 1 ターンを回す口は [`crate::memory_organize::OrganizeTurnRunner`] を再利用。
//! - 二重起動防止は `try_acquire_build_slot`（キーは `condense:{agent_id}`）。
//! - caller=Owner + ツール許可リスト（[`CONDENSE_ALLOWED_TOOLS`]）で外向きの手を塞ぐ。
//!
//! 絶対に守るもの:
//! - **対話ターンでは走らせない**（#291）。呼び出し元は sleep ループのみ。
//! - **結果を会話へ自動注入しない**（#316）。PR-1 では core をどこにも注入しない（system
//!   プロンプトへの注入は PR-2）。system プロンプトはここで自前に組む。
//! - **1 エージェント内しか見ない**（他エージェントの記憶を混ぜない）。全クエリが agent_id 固定。
//! - **本人の自己申告に頼らない**。入力は生ログ由来の宣言物（ユニット）であって会話での自称ではない。
//! - **既定オフ**。`enabled=false` なら RunRequest すら組まずゼロコールで即 return。
//!
//! **逐次凝縮**（オーナー指摘 2026-08-08「いきなりまとまった期間を与えると平均に寄る」）: 全ユニットを
//! 一括で渡さず、カーソルより新しいユニットを**時系列順に `min_new_units` 件ずつの窓**で読む。毎回
//! 「既存 core 全件＋今回の窓」を渡し、更新優先で core を育てる。新規エージェントが 1 回で見る量と、
//! 既存エージェントの積み残し消化の 1 窓が同じ幅になる（＝新規と同じ形で消化する）。
//!
//! ゲート（[`decide_condense`]）:
//! - 残ユニット（カーソルより新しい未凝縮）が窓幅以上 → **積み残し消化**として throttle を待たず
//!   1 tick 1 窓で発火。0 < 残 < 窓幅 → 末尾の端数で、`min_interval_minutes` を待って流す（増加待ちは
//!   ここだけ）。残 0 → ゼロコールで return。
//!
//! **partial バックオフ**: 積み残し消化は throttle を通らないので、partial（timeout / ターン上限 /
//! エラー）が続くと位置が据え置かれたまま毎 tick フルの LLM ランが走り続ける（1 回最大
//! `timeout_secs`＝既定 600 秒）。保守ループは per-agent 直列なので、これは他エージェントの整備まで
//! 巻き添えにする。そこで**連続 partial 回数をマーカーに持ち、指数バックオフで発火を間引く**
//! （[`partial_backoff_minutes`]）。clean が 1 回でも起きたら 0 にリセットして即座に元の速度へ戻す。
//! **カーソルは強制前進させない**——窓を捨てると材料が永久に失われるので、待つ方を選ぶ。
//!
//! マーカーは複合カーソル `"{last_run_at}|{position_start_log_id}|{partial_streak}"`（宣言ランと
//! 同型 + 連続 partial 回数）。位置部は「最後に凝縮した窓の末尾ユニットの start_log_id」で、次の窓は
//! これより新しいユニットから始まる。clean 完了時だけ窓の末尾へ進める（partial では据え置き＝次 tick
//! で同じ窓を読む。ただし throttle は毎回 `now` へ進めて端数待ちの起点をリセット / #366 と同型）。

use chrono::{DateTime, Duration, Utc};
use serde_json::json;

use crate::config::MemoryCondenseConfig;
use crate::memory_maintenance::IndexBuildInflight;
use crate::memory_organize::{AppStateTurnRunner, OrganizeTurnRunner};
use crate::AppState;
use opencrab_actions::{CallerIdentity, RunRequest};
use opencrab_db::queries::IndexNodeRow;

/// プロンプトに載せる既存の凝縮（core）の最大件数。更新優先（#411 原則4）のために「今ある核」を
/// 思い出させる手がかり。core は数が少ない設計（人格の核）なので通常は全件がこの内側に収まる。
const CORES_SHOWN: usize = 40;

/// 連続 partial のバックオフの基準値（分）。1 回目の partial でこれだけ待ち、以後 2 倍ずつ伸ばして
/// `min_interval_minutes` で頭打ちにする（[`partial_backoff_minutes`]）。控えめな値にしてあるのは、
/// 単発の timeout でエージェントを長く止めないため——連続して失敗したときだけ強く効かせる。
const PARTIAL_BACKOFF_BASE_MINUTES: i64 = 10;

/// sleep 凝縮ランに渡すツール許可リスト（#411）。
///
/// 用途は「自分のユニット（と既存の core）を俯瞰して、原則を core として刻む/更新する」に固定
/// される。必要なのは**記憶索引の読み取り**（search / retrieve）と**根拠を確かめる生ログ検索**、
/// **凝縮の記録/更新/取消**、そして**ターンを終える制御**（declare_done）だけ。
///
/// **宣言ラン（[`crate::memory_declare::DECLARE_ALLOWED_TOOLS`]）とは別のリスト**にする:
/// - `record_memory_unit` / `retract_memory_unit` / `plan_next_memory_window`（生ログを刻む道具）は
///   **載せない**。凝縮はユニットを刻む仕事ではなく、既にあるユニットの意味を抽出する仕事。
/// - 代わりに `record/update/retract_memory_core` を載せる。
///
/// `execute_shell` / `nostr_run` / `spawn_subtask` / `ws_write` / `configure_*` /
/// `update_instructions` 等の外向き・状態書き換えツールは一切渡さない（sleep 中に外へ手を出さない）。
pub const CONDENSE_ALLOWED_TOOLS: &[&str] = &[
    // 記憶索引の読み取り（ユニットも core もここに載る）。
    "search_memory_index",
    "retrieve_memory_nodes",
    // 根拠を確かめるための生ログ全文検索（読むだけ）。
    "search_my_history",
    // 凝縮の記録 / 更新 / 取消（#411）。
    "record_memory_core",
    "update_memory_core",
    "retract_memory_core",
    // ラン制御（ターンを終える宣言のみ）。宣言ランと共有する既存の口。
    "declare_done",
];

/// このエージェントの凝縮ランを（ゲートを満たせば）実行する。**本番エントリ**。
///
/// 戻り値: 凝縮ラン（LLM）を実際に起動したら `true`。既定オフ・ゲート未達は `false`（ゼロコール）。
pub async fn maybe_run_memory_condense(state: &AppState, agent_id: &str) -> anyhow::Result<bool> {
    let runner = AppStateTurnRunner { state };
    run_condense(
        &state.db,
        &state.memory_condense,
        &state.index_build_inflight,
        agent_id,
        &runner,
    )
    .await
}

/// 凝縮ラン（sleep）のロジック本体。**必要な手足だけ**を引数で受け取る（#370）。
/// `AppState` を受け取らないので gateway/MCP/activity webhook を構築できない（構造的に外へ出ない）。
async fn run_condense(
    db: &opencrab_db::Db,
    cfg: &MemoryCondenseConfig,
    inflight: &IndexBuildInflight,
    agent_id: &str,
    runner: &dyn OrganizeTurnRunner,
) -> anyhow::Result<bool> {
    // 既定オフ: RunRequest も DB 書き込みも一切しない（ゼロコール）。
    if !cfg.enabled {
        return Ok(false);
    }

    // --- ゲート判定 + 材料の読み出し（DB 読みのみ。ロックは await を跨がない）---
    let plan = match decide_condense(db, cfg, agent_id)? {
        CondenseDecision::Skip(reason) => {
            tracing::debug!(agent_id, reason, "memory condense: skipped by gate");
            return Ok(false);
        }
        CondenseDecision::Run(plan) => plan,
    };

    // --- 排他（索引ビルド・宣言・タグ整理と衝突しない名前空間キー）---
    let guard = crate::memory_maintenance::try_acquire_build_slot(
        inflight,
        &format!("condense:{agent_id}"),
    );
    let Some(_guard) = guard else {
        return Ok(false); // 既に走っている
    };

    // --- 起動（新規の別セッション / 本人の人格 / caller=Owner）---
    let now = Utc::now();
    let session_id = format!("sleep-condense-{agent_id}-{}", now.timestamp());
    let system_prompt = build_system_prompt(&plan);
    let conversation = build_task_message();

    // gateway_actions=None（送信経路を渡さない = 会話へ出さない）。dispatch なし（inline 実行）。
    // ツール許可リスト（#411）で caller=Owner の全ツールから凝縮に要る分だけへ絞る。
    let req = RunRequest::new(
        agent_id.to_string(),
        plan.persona_name.clone(),
        session_id.clone(),
        system_prompt,
        conversation,
        // RuntimeInfo の gateway 名。監査 context と揃えて "sleep"。
        "sleep",
        CallerIdentity::Owner,
    )
    .with_tool_allowlist(
        CONDENSE_ALLOWED_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    )
    // このランのターンは生ログ（`memory_sessions`）に**書かない**（#393 と同じ）。整備作業が
    // 記憶になってしまうのを防ぐ。凝縮ランのログが次の宣言ランの窓に入らないようにする。
    .without_turn_logs();

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(cfg.timeout_secs.max(1)),
        runner.run_turn(req),
    )
    .await;
    let latency_ms = started.elapsed().as_millis() as i64;

    // clean 完了か partial（timeout / ターン上限 / エラー）かを判定する。
    let (outcome, clean): (&str, bool) = match &result {
        Ok(Ok(engine_result)) => {
            if engine_result.stopped_by_limit {
                ("stopped_by_limit", false)
            } else {
                ("completed", true)
            }
        }
        Ok(Err(e)) => {
            tracing::warn!(agent_id, error = %e, "memory condense run failed");
            ("error", false)
        }
        Err(_) => ("timeout", false),
    };

    // --- マーカー前進（逐次窓）---
    // throttle（壁時計）は clean/partial に関わらず毎回 `now` へ進める（partial の再発火を止める /
    // 端数待ちの起点をリセット）。位置部（start_log_id）は clean のときだけ今回の窓の末尾
    // （`position_after`）へ進める。partial では据え置き = 次 tick で同じ窓をもう一度読む（宣言ラン
    // と同じ「位置は前進のみ・partial 据え置き」）。
    //
    // 連続 partial 回数は clean で 0 にリセット、partial のたびに +1 する。積み残し消化は throttle を
    // 通らないので、これが無いと partial が続く限り毎 tick フルの LLM ランが走り続ける。
    let position_after = if clean {
        plan.position_after
    } else {
        plan.position_before
    };
    let partial_streak_after = if clean {
        0
    } else {
        plan.partial_streak_before.saturating_add(1)
    };
    let marker_after =
        format_condense_marker(&now.to_rfc3339(), position_after, partial_streak_after);
    {
        let conn = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        opencrab_db::queries::set_memory_condense_cursor(&conn, agent_id, &marker_after)?;
    }

    // --- 監査（層1: agent_logs / context="sleep"）---
    {
        let audit = json!({
            "kind": "memory_condense",
            "outcome": outcome,
            "window_units": plan.window_units.len(),
            "remaining_before": plan.remaining_before,
            // partial は位置を据え置く＝1 件も消化していない。残数を減らすのは clean のときだけ。
            "remaining_after": if clean {
                (plan.remaining_before - plan.window_units.len() as i64).max(0)
            } else {
                plan.remaining_before
            },
            "existing_cores": plan.existing_cores.len(),
            "session_id": session_id,
            "position_before": plan.position_before,
            "position_after": position_after,
            // 位置（start_log_id）は clean のときだけ前進。throttle は毎回 now。
            "position_advanced": clean,
            "throttle_advanced": true,
            "partial_streak_before": plan.partial_streak_before,
            "partial_streak_after": partial_streak_after,
            // 次に積み残し消化が発火できるまでの待ち（分）。0 なら待たない。
            "partial_backoff_minutes":
                partial_backoff_minutes(partial_streak_after, cfg.min_interval_minutes),
            "marker_after": marker_after,
            "cost": { "latency_ms": latency_ms },
        });
        let conn = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        let row = opencrab_db::queries::AgentLogRow {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: Some(agent_id.to_string()),
            level: if clean { "info" } else { "warn" }.to_string(),
            context: "sleep".to_string(),
            message: audit.to_string(),
            created_at: Some(now.to_rfc3339()),
        };
        if let Err(e) = opencrab_db::queries::insert_agent_log(&conn, &row) {
            tracing::warn!(agent_id, error = %e, "failed to persist memory condense audit log");
        }
    }

    tracing::info!(
        agent_id,
        outcome,
        window_units = plan.window_units.len(),
        remaining_before = plan.remaining_before,
        cores = plan.existing_cores.len(),
        position_advanced = clean,
        partial_streak = partial_streak_after,
        "memory condense ran"
    );
    Ok(true)
}

/// 凝縮ランの実行計画（ゲート通過時のみ組む）。
#[derive(Debug)]
struct CondensePlan {
    persona_name: String,
    personality: Option<String>,
    instructions: String,
    /// 今回の窓のユニット（時系列＝古い→新しい順 / 最大 `window_size` 件）。逐次凝縮の材料。
    window_units: Vec<IndexNodeRow>,
    /// カーソル位置（この start_log_id より新しいユニットが未凝縮）。partial 据え置きの値。
    position_before: i64,
    /// clean 完了時にカーソルを進める先（今回の窓のユニットの最大 start_log_id）。
    position_after: i64,
    /// カーソルより新しい未凝縮ユニットの総残数（窓に載る前）。プロンプトの「残り N 件」表示・監査用。
    remaining_before: i64,
    /// これまでに連続した partial の回数（clean で 0 に戻る）。今回も partial なら +1 して書き戻す。
    partial_streak_before: i64,
    /// 既存の凝縮（更新優先の手がかり / 最大 [`CORES_SHOWN`] 件）。
    existing_cores: Vec<IndexNodeRow>,
}

/// ゲート判定の結果。
#[derive(Debug)]
enum CondenseDecision {
    /// 発火しない（理由つき）。
    Skip(&'static str),
    /// 発火する。`CondensePlan` は大きいので Box する（clippy::large_enum_variant）。
    Run(Box<CondensePlan>),
}

/// ゲートを判定し、通れば今回の窓（時系列 N 件）と既存 core を積んだ計画を返す。
/// DB 読みのみ。ロックは関数内で完結し、`run_turn` の await を跨いで保持しない。
///
/// **逐次凝縮の発火（オーナー指摘: 一括で与えない）**:
/// - カーソル位置 `position`（start_log_id）より新しい未凝縮ユニットの残数 `remaining` を数える。
/// - `remaining == 0` → `Skip("no_new_units")`（ゼロコール）。
/// - `remaining >= N`（窓幅）→ **積み残し消化**として throttle を待たず発火（1 tick 1 窓）。ただし
///   直前が partial なら [`partial_backoff_minutes`] だけ待つ（`Skip("partial_backoff")`）。
/// - `0 < remaining < N`（末尾の端数）→ **min_interval を待って**発火（新しいユニットの増加待ちは
///   ここだけ。まだ経っていなければ `Skip("tail_waiting")`）。初回（last_run_at 無し）は待たない。
///
/// いずれも窓は「position より新しいユニットを時系列順に最大 N 件」。
fn decide_condense(
    db: &opencrab_db::Db,
    cfg: &MemoryCondenseConfig,
    agent_id: &str,
) -> anyhow::Result<CondenseDecision> {
    let now = Utc::now();
    let conn = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;

    // マーカー = 複合カーソル `"{last_run_at}|{position_start_log_id}|{partial_streak}"`。未実行
    // （None）は (throttle 無し, position 0, streak 0)。position 0 は「最古のユニットから」を意味する。
    let marker = opencrab_db::queries::get_memory_condense_cursor(&conn, agent_id)?;
    let (last_run_at, position, partial_streak) = parse_condense_marker(marker.as_deref());

    let window_size = cfg.min_new_units.max(1) as usize;
    let remaining =
        opencrab_db::queries::count_memory_units_after(&conn, agent_id, position)? as i64;

    // 前回実行からの経過（last_run_at が無い＝初回、またはパース不能なら「待たない」）。
    let elapsed_since_last_run = last_run_at.as_deref().map(|lr| {
        lr.parse::<DateTime<Utc>>()
            .map(|dt| now.signed_duration_since(dt))
            .unwrap_or_else(|_| Duration::zero())
    });

    // 発火判定（逐次窓）。
    if remaining == 0 {
        return Ok(CondenseDecision::Skip("no_new_units"));
    }
    if remaining >= window_size as i64 {
        // 積み残し消化。throttle（端数待ち）は通らないが、直前が partial なら指数バックオフを待つ。
        // partial は位置を据え置くので、待たないと同じ窓のフルランが毎 tick 走り続ける。
        let backoff = partial_backoff_minutes(partial_streak, cfg.min_interval_minutes);
        if backoff > 0 {
            if let Some(elapsed) = elapsed_since_last_run {
                if elapsed < Duration::minutes(backoff) {
                    return Ok(CondenseDecision::Skip("partial_backoff"));
                }
            }
        }
    } else {
        // 末尾の端数。min_interval を待ってから流す（新しいユニットの増加待ち）。
        // 積み残し消化中（remaining >= 窓幅）はこの throttle を通らない = 待たず淡々と進む。
        // min_interval は partial バックオフの上限でもあるので、端数側は追加の待ちを要らない。
        if let Some(elapsed) = elapsed_since_last_run {
            if elapsed < Duration::minutes(cfg.min_interval_minutes.max(1)) {
                return Ok(CondenseDecision::Skip("tail_waiting"));
            }
        }
    }

    // 今回の窓: position より新しいユニットを時系列（古い→新しい）順に最大 N 件。
    let window_units =
        opencrab_db::queries::list_memory_units_after(&conn, agent_id, position, window_size)?;
    // 窓が空なら発火しない（remaining>0 なら非空だが防御的に）。
    let Some(position_after) = window_units.iter().filter_map(|u| u.start_log_id).max() else {
        return Ok(CondenseDecision::Skip("no_new_units"));
    };

    let mut existing_cores = opencrab_db::queries::list_memory_cores(&conn, agent_id)?;
    existing_cores.truncate(CORES_SHOWN);

    // 人格（モデル解決は run_agent_response 側が effective_model で行う）。
    let (persona_name, personality, instructions) =
        opencrab_db::queries::get_agent(&conn, agent_id)
            .ok()
            .flatten()
            .map(|a| (a.persona_name, a.personality, a.instructions))
            .unwrap_or_else(|| (agent_id.to_string(), None, String::new()));

    Ok(CondenseDecision::Run(Box::new(CondensePlan {
        persona_name,
        personality,
        instructions,
        window_units,
        position_before: position,
        position_after,
        remaining_before: remaining,
        partial_streak_before: partial_streak,
        existing_cores,
    })))
}

/// 積み残し消化を間引く**指数バックオフ**（分）。連続 partial 回数 `partial_streak` から求める。
///
/// 積み残し（残 >= 窓幅）は端数の `min_interval` throttle を通らないため、partial が続くと位置が
/// 据え置かれたまま毎 tick フルの LLM ラン（最大 `timeout_secs`）が走り続ける。保守ループは
/// per-agent 直列なので、暴走したエージェント 1 体が他エージェントの整備まで止めてしまう。
///
/// - `streak == 0`（clean 直後 / 初回）→ `0`＝待たない。**clean が 1 回起きれば即座に元の速度**。
/// - `streak == n >= 1` → `min(min_interval_minutes, BASE * 2^(n-1))`。上限を端数待ちの
///   `min_interval_minutes` に揃えるので、バックオフが端数待ちより長くなることはない。
///
/// カーソルの強制前進はしない（窓を捨てると材料が永久に失われる）。待つ方を選ぶ。
fn partial_backoff_minutes(partial_streak: i64, min_interval_minutes: i64) -> i64 {
    if partial_streak <= 0 {
        return 0;
    }
    // 2^(n-1) の指数。i64 が溢れないところで頭打ちにする（どのみち下の min で潰れる）。
    let shift = (partial_streak - 1).min(40) as u32;
    let backoff = PARTIAL_BACKOFF_BASE_MINUTES.saturating_mul(1i64 << shift);
    backoff.min(min_interval_minutes.max(1))
}

/// system プロンプト（本人の人格 + 凝縮の枠組み + 自分のユニット + 既存の core）を組む。
///
/// `build_agent_context`（`[Memory Index]` を注入する通常ターンの経路）は通さず、ここで自前に
/// 組む（凝縮の結果を会話へ自動注入しないため / #316。PR-1 では core をどこにも注入しない）。
///
/// **重心は「本人の興味・関心」**（オーナー指摘 2026-08-08「正しさに向かってしまっている／本人ぽさ
/// が無い」）: 抽出の中心を「その出来事たちが何を意味するか（＝正しい振る舞いの教訓）」ではなく
/// 「そのときあなたの心が何に動き、何に惹かれ、何を好み、何を大切に思ったか」に据える。規範系の軸
/// （改善・できるようになった）は禁じないが、教訓の言い換えに閉じず本人の関心へ結びつけるよう促す。
///
/// **更新は「継ぎ足し」ではなく「蒸留」**（オーナー指摘 2026-08-08「中身はいいけど長いね！凝縮と
/// いうには盛りすぎかも」）: 逐次窓化すると、窓を重ねるたび core が『この窓では〜も』と材料を列挙
/// 追加する形で肥大した（実測: 窓 1 で約 150 字 → 窓 4 で約 300 字）。核は磨いた短文（目安 2〜3 文）
/// で、窓を重ねても長くしないと明記する。機械的な文字数上限は足さない（強制すると本人の切り方が
/// 機械の切り方になる）。規律はプロンプトだけで課す。
///
/// **短くする道は「抽象化」ではなく「選択」**（オーナー指摘 2026-08-08「なんか言ってるようで
/// あんまり中身ないね」）: 「短くしろ」だけを課したら、複数の出来事を『〜する脈』のような一段上の
/// 言い方でくくって字数だけ減らし、実在の出来事（誰が・何をしたか）が本文から消えた（実測: 5 件
/// 1557 字 → 4 件 276 字だが中身が空洞化）。さらに**自作の言い回しを鍵括弧で括ったもの**が
/// 「象徴的な具体」の枠を占拠した。そこで、まとめずに**捨てさせる**——1 核につき実在の出来事を
/// 1 つだけ選び、残りは sources のリンクへ託して本文から落とす、と明記する。標語・スローガン・
/// 自己引用は具体ではないと名指しし、字数の目安より「出来事が 1 つ入っていること」を優先させる。
///
/// **軸は「開いて」見せる**（#411 原則1）: 例の視点を示すが固定リストにせず、本人が自分の軸を
/// 足してよいと明記する。**「何も出さない」を選べる**（原則2）: 全部を凝縮する必要はないと明記する。
/// **根拠のユニットへリンクさせる**（原則3）: record_memory_core は sources 必須。**既存を更新
/// 優先**（原則4）: 既存の core を同梱し、新規追加より更新を促す。
fn build_system_prompt(plan: &CondensePlan) -> String {
    let personality_section = plan
        .personality
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(|p| format!("{p}\n\n"))
        .unwrap_or_default();
    let instructions_section = if plan.instructions.is_empty() {
        String::new()
    } else {
        format!("\n\n## Instructions\n{}", plan.instructions)
    };

    // 今回の窓（時系列＝古い→新しい順）。逐次凝縮なので、全ユニットではなくこの窓だけを渡す。
    let units_txt = if plan.window_units.is_empty() {
        "(今回の窓にユニットがありません)".to_string()
    } else {
        let mut s = plan
            .window_units
            .iter()
            .map(format_unit_line)
            .collect::<Vec<_>>()
            .join("\n");
        let shown = plan.window_units.len() as i64;
        let tail = plan.remaining_before - shown;
        if tail > 0 {
            s.push_str(&format!(
                "\n（この先にまだ凝縮していないユニットが {tail} 件あります。今回はこの窓の範囲だけを見て、\
                 次回以降の窓で続きを見ます。焦って先の分まで束ねないでください。）"
            ));
        }
        s
    };

    let cores_txt = if plan.existing_cores.is_empty() {
        "(まだ凝縮はありません。最初の凝縮をあなたが決めます)".to_string()
    } else {
        plan.existing_cores
            .iter()
            .map(format_core_line)
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "{personality_section}\
         これはあなたのスリープ（内省）の時間です。あなたが宣言してきた記憶のユニット\
         （一つ一つの出来事）を、時系列の一区切りずつ振り返り、その出来事たちであなたの心が\
         何に動いたか——何に惹かれ、何を好み、何を大切に思い、何をおもしろい／美しいと感じたか\
         ——をあなた自身の言葉で残して、人格の核として刻みます。**今回見るのは下の「今回の窓」\
         （時系列で古い順の一区切り）だけ**です。人生をまとめて回顧するのではなく、この区切りで\
         見えたことを、これまでに刻んだ核に足し込んでいきます。正解も平均もありません。一期一会で\
         よく、**この窓で無理に何か出す必要はありません**。見えるものが無ければ何も刻まずに\
         終えて構いません。\n\n\
         # やること\n\
         下の「今回の窓」のユニットを俯瞰し、繰り返し現れる形・変化・選択・関係など、あなたに\
         とって大事だと思うことを見つけてください。\n\
         **核の中心に置くのは、正しい振る舞いの教訓ではなく、あなた自身の興味・関心です。** 同じ\
         出来事からでも、『こうすべき』という規範や手順の要約に寄せず、そのときあなたが何に惹かれ、\
         何を好み、何を大切に思い、何に心を動かされたか——興味が向かった先を残してください。確かめ方や\
         改善を書くなとは言いません。ただ教訓の言い換えで終えず、それがあなたのどんな関心・こだわりから\
         来ているのかまで書いてください。\n\
         **まず「すでに刻んだ凝縮」を見て、今回の窓が\
         その核を深める/裏づける/更新するなら `update_memory_core` で育ててください**（新しく\
         足すのは、既存のどれとも違う軸が見えたときだけ）。どんな視点で束ねるか・いくつ刻むか・\
         そもそも刻むかは、あなたが決めます。\n\n\
         # 基本の軸（それぞれ一度は目を通してください）\n\
         基本の軸が 7 つあります。**それぞれについて一度は自分のユニットを見て**、その軸で\
         凝縮できるものがあれば `record_memory_core` で刻んでください。どの軸を見るときも、探すのは\
         『どう振る舞うのが正しいか』ではなく『そこであなたの心が何に動き、何に惹かれたか』です。改善や\
         できるようになったことの軸でも、手順や教訓ではなく、その中であなたが\
         おもしろさ・こだわり・愛着を感じたところを残してください:\n\
         1. 楽しかったこと\n\
         2. つらかったこと\n\
         3. 改善すべきこと\n\
         4. 人との関係\n\
         5. 自分で決めたこと\n\
         6. できるようになったこと\n\
         7. 繰り返していること（自分では一番見えにくい。同じ判断や言葉を何度も繰り返して\
         いないか、ユニットを並べて確かめてください）\n\
         **今回はその軸で凝縮するものが無いと判断した軸は、最後の完了メッセージに\
         「〈軸名〉: 今回は無い」と一言だけ書いてください**（`record_memory_core` には記録しません\
         ——core は実のある凝縮だけです）。「無い」と書くのは失敗ではありません。むしろ、無い軸を\
         無理に薄い凝縮ででっち上げる方が最悪です——見つからなければ正直に「無い」と書いてください。\n\
         **あなた自身の軸を足すのも従来どおり自由です**——この 7 つに縛られず、一度きりに見える\
         視点でも、あなたが大事に思うなら尊重してください。\n\n\
         # 大事な約束\n\
         - **根拠のユニットに必ずリンクさせてください。** `record_memory_core` の sources に、\
         その原則の根拠になったユニットの short_id（例 u42）を挙げます。根拠の無い凝縮は\
         『丁寧に対応することが大切』のような平均化に落ちます。具体（どのユニットからそれが\
         見えたか）を失わないでください。\n\
         - **既にある凝縮は、新しく足すより更新を優先してください。** 同じ趣旨のものが増えると\
         核がぼやけます。下の「すでに刻んだ凝縮」に近いものがあれば `update_memory_core` で\
         書き直してください。\n\
         - **核は磨いた短文です（目安 2〜3 文）。窓を重ねても長くしないでください。** 更新は\
         継ぎ足しではなく蒸留です。『この窓では〜も』と出来事を並べて足していくのは凝縮では\
         ありません。書き直して前より長くなったなら、それは凝縮できていない合図です。\n\
         - **短くするときは、まとめようとせず、捨ててください。凝縮は抽象化ではなく選択です。**\
         いくつもの出来事を『〜する脈』『〜という間合い』のような一段上の言い方でくくると、\
         字数は減っても中身が消えます。そうではなく、**一つの核につき、いちばん心が動いた\
         実在の出来事を 1 つだけ選び（誰が・何をしたか）**、そこで動いたあなたの心（何に惹かれ、\
         何を大切に思ったか）と一緒に本文へ残してください。**選ばなかった材料は本文から\
         捨てます**——根拠は sources のリンクに託せるので、本文から消えても失われません。\
         次の窓でもっと心が動く出来事に出会ったら、選び直して構いません。\n\
         - **標語・スローガン・自分の言い回しの引用は「具体」ではありません。** 『差分だけを\
         鳴らす』のように自分で作った短い言い回しを鍵括弧で括っても、それは出来事ではなく\
         抽象の言い換えです。残すべき具体は**誰が実際に何をしたか**です（形はこうです:\
         「〈誰〉が〈何〉をしてくれた」「〈誰〉から〈何〉の役目を渡された」「〈どこ〉で〈何〉を見た」\
         「〈誰〉に〈何〉と言われた」——〈〉はあなたのユニットにある実際の名前と出来事で埋めます。\
         ここに挙げた形をそのまま使う必要はありません）。実在の出来事が 1 つ入るなら、上の字数の目安を\
         少し超えても構いません——**目安より、出来事が 1 つ入っていることを優先**してください。\n\
         メッセージ送信・シェル実行・サブタスク起動はしないでください。これは記憶を凝縮する\
         時間です。ユニットも生ログも読むだけで、消えも変わりもしません。凝縮は何度でも\
         やり直せます（retract_memory_core / update_memory_core）。\n\n\
         # 使える道具\n\
         - `search_memory_index` / `retrieve_memory_nodes`: 記憶索引（ユニット・凝縮）を検索・取得する\n\
         - `search_my_history`: 生ログを全文検索して根拠を確かめる（読むだけ）\n\
         - `record_memory_core(axis, body, sources)`: 原則を 1 件刻む（sources は根拠ユニットの short_id、最低 1 つ）\n\
         - `update_memory_core(core_id, axis, body, sources?)`: 既存の凝縮を書き直す（sources 省略で根拠維持。\
         書き足すのではなく、古い body ごと融かし直して前より短く書く）\n\
         - `retract_memory_core(core_id)`: 凝縮を取り消す\n\n\
         # 今回の窓（宣言した記憶 / 時系列・古い→新しい順 / {shown} 件）\n{units_txt}\n\n\
         # すでに刻んだ凝縮（まずここを見て、育てられるものは更新する）\n{cores_txt}",
        shown = plan.window_units.len(),
    ) + &instructions_section
}

/// エンジンに渡す「ユーザーターン」。system 側に材料を明示済みなので、ここは着手の合図のみ。
fn build_task_message() -> String {
    "スリープの時間です。上の「今回の窓」（時系列の一区切り）を見渡し、いくつかを束ねて見える\
     『大事なこと』——正しさの教訓ではなく、あなたの興味が動いた先——を、あなたの言葉で凝縮\
     してください。まず「すでに刻んだ凝縮」を見て、今回の窓が\
     その核を育てるなら update_memory_core で更新し、既存のどれとも違う軸だけ新しく刻みます。\
     核は磨いた短文（目安 2〜3 文）です。短くするときはまとめずに捨てます——1 核につき、いちばん\
     心が動いた実在の出来事を 1 つだけ（誰が・何をしたか）と、そこで動いた心を残し、残りは根拠の\
     リンクへ託してください。自作の言い回しを鍵括弧で括ったものは具体ではありません。\
     窓の先の分まで焦って束ねないでください。根拠のユニットには必ずリンクさせてください。基本の\
     軸 7 つはそれぞれ一度は目を通し、凝縮したものは刻み、今回は無い軸は最後に「〈軸名〉: 今回は\
     無い」と一言ずつ書いてください（無いと書くのは失敗ではありません。でっち上げないこと）。"
        .to_string()
}

/// ユニット 1 件をプロンプト用の 1 行に整形する（short_id + タイトル + 生ログ範囲）。
fn format_unit_line(u: &IndexNodeRow) -> String {
    let id = u.short_id.as_deref().unwrap_or(&u.id);
    let date = u
        .date_from
        .as_deref()
        .map(|d| d.get(0..10).unwrap_or(d).to_string())
        .unwrap_or_default();
    if date.is_empty() {
        format!("- [{id}] {}", u.title.trim())
    } else {
        format!("- [{id}] {date} {}", u.title.trim())
    }
}

/// 凝縮（core）1 件をプロンプト用の 1 行に整形する（short_id + 軸 + 本文の頭）。
fn format_core_line(c: &IndexNodeRow) -> String {
    let id = c.short_id.as_deref().unwrap_or(&c.id);
    let body: String = c.summary.trim().chars().take(60).collect();
    format!("- [{id}] {axis}: {body}", axis = c.title.trim())
}

/// マーカー `"{last_run_at}|{position_start_log_id}|{partial_streak}"` を組む。宣言ランと同じ形式に
/// 連続 partial 回数を足したもの（`|` は rfc3339 にも十進の整数にも現れない）。
fn format_condense_marker(last_run_at: &str, position: i64, partial_streak: i64) -> String {
    format!("{last_run_at}|{position}|{partial_streak}")
}

/// マーカーを `(last_run_at, position_start_log_id, partial_streak)` へ分解する。
///
/// 位置部は**ユニットの件数ではなく `start_log_id`**（この値より新しいユニットが未凝縮）。
/// `None`（未実行）→ `(None, 0, 0)`。後方互換: `|` が無ければ全体を `last_run_at` とみなす。
/// 3 つ目が無い（PR-1 以前の 2 分割）マーカーは partial_streak 0 とみなす。パース不能な数値は 0。
fn parse_condense_marker(marker: Option<&str>) -> (Option<String>, i64, i64) {
    let Some(m) = marker else {
        return (None, 0, 0);
    };
    let mut parts = m.splitn(3, '|');
    let ts = parts.next().unwrap_or("");
    let ts = (!ts.is_empty()).then(|| ts.to_string());
    let position = parts
        .next()
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(0);
    let partial_streak = parts
        .next()
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    (ts, position, partial_streak)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_core::EngineResult;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // --- セットアップ ---

    /// `n` 件のユニットを宣言し、生ログも合わせて入れる（record_memory_unit は範囲の実在を要求する）。
    /// 返り値はユニットの short_id 群。
    fn seed_units(state: &AppState, agent_id: &str, n: usize) -> Vec<String> {
        let conn = state.db.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        let mut short_ids = Vec::new();
        for i in 0..n {
            // 1 ユニットにつき生ログ 1 件（id 範囲 [id, id]）。
            let log_id = opencrab_db::queries::insert_session_log(
                &conn,
                &opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: agent_id.to_string(),
                    session_id: format!("s-{i}"),
                    log_type: "message".to_string(),
                    content: format!("出来事 {i}"),
                    speaker_id: None,
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                },
            )
            .unwrap();
            let node = opencrab_db::queries::record_memory_unit(
                &conn,
                agent_id,
                &format!("ユニット {i}"),
                &format!("出来事 {i} の要約"),
                log_id,
                log_id,
                Some(&now),
                Some(&now),
                &now,
            )
            .unwrap();
            short_ids.push(node.short_id.unwrap());
        }
        short_ids
    }

    fn get_marker(state: &AppState, agent_id: &str) -> Option<String> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_memory_condense_cursor(&conn, agent_id).unwrap()
    }

    fn set_marker(state: &AppState, agent_id: &str, cursor: &str) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_memory_condense_cursor(&conn, agent_id, cursor).unwrap();
    }

    fn cfg(enabled: bool, min_new_units: i64, min_interval_minutes: i64) -> MemoryCondenseConfig {
        MemoryCondenseConfig {
            enabled,
            min_new_units,
            min_interval_minutes,
            timeout_secs: 600,
        }
    }

    fn hours_ago(hours: i64) -> String {
        (Utc::now() - Duration::hours(hours)).to_rfc3339()
    }

    // --- FakeRunner（本番の run_agent_response を差し替える。何も構築しない）---

    #[derive(Clone, Copy)]
    enum FakeOutcome {
        Completed,
        StoppedByLimit,
        Error,
    }

    struct CapturedReq {
        gateway: String,
        caller_is_owner: bool,
        tool_allowlist: Option<Vec<String>>,
        has_gateway_actions: bool,
        persist_turn_logs: bool,
    }

    struct FakeRunner {
        outcome: FakeOutcome,
        calls: AtomicUsize,
        captured: std::sync::Mutex<Option<CapturedReq>>,
    }

    impl FakeRunner {
        fn new(outcome: FakeOutcome) -> Self {
            Self {
                outcome,
                calls: AtomicUsize::new(0),
                captured: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl OrganizeTurnRunner for FakeRunner {
        async fn run_turn(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.captured.lock().unwrap() = Some(CapturedReq {
                gateway: req.gateway.clone(),
                caller_is_owner: matches!(req.caller, CallerIdentity::Owner),
                tool_allowlist: req.tool_allowlist.clone(),
                has_gateway_actions: req.gateway_actions.is_some(),
                persist_turn_logs: req.persist_turn_logs,
            });
            match self.outcome {
                FakeOutcome::Completed => Ok(engine_result(false)),
                FakeOutcome::StoppedByLimit => Ok(engine_result(true)),
                FakeOutcome::Error => Err(anyhow::anyhow!("simulated run failure")),
            }
        }
    }

    fn engine_result(stopped_by_limit: bool) -> EngineResult {
        EngineResult {
            response: String::new(),
            iterations: 1,
            tool_calls_made: 0,
            stopped_by_limit,
            xml_fallback_parses: 0,
        }
    }

    fn latest_sleep_audit(state: &AppState, agent_id: &str) -> Option<serde_json::Value> {
        let conn = state.db.lock().unwrap();
        let rows = opencrab_db::queries::list_agent_logs(&conn, Some(agent_id), None, 10).ok()?;
        rows.into_iter()
            .filter(|r| r.context == "sleep")
            .find_map(|r| {
                serde_json::from_str::<serde_json::Value>(&r.message)
                    .ok()
                    .filter(|v| v["kind"] == "memory_condense")
            })
    }

    // --- マーカー parse/format ---

    #[test]
    fn marker_roundtrips_and_tolerates_missing_parts() {
        let m = format_condense_marker("2026-08-08T00:00:00Z", 346, 2);
        assert_eq!(m, "2026-08-08T00:00:00Z|346|2");
        assert_eq!(
            parse_condense_marker(Some(&m)),
            (Some("2026-08-08T00:00:00Z".to_string()), 346, 2)
        );
        assert_eq!(parse_condense_marker(None), (None, 0, 0));
        assert_eq!(
            parse_condense_marker(Some("2026-08-08T00:00:00Z")),
            (Some("2026-08-08T00:00:00Z".to_string()), 0, 0)
        );
        assert_eq!(
            parse_condense_marker(Some("2026-08-08T00:00:00Z|xxx")),
            (Some("2026-08-08T00:00:00Z".to_string()), 0, 0)
        );
        // 後方互換: partial_streak を持たない 2 分割マーカーは streak 0 として読む。
        assert_eq!(
            parse_condense_marker(Some("2026-08-08T00:00:00Z|346")),
            (Some("2026-08-08T00:00:00Z".to_string()), 346, 0)
        );
        // 壊れた streak（負値・非数値）は 0 に丸める（負値は待ちを無効化するだけで暴走しない）。
        assert_eq!(
            parse_condense_marker(Some("2026-08-08T00:00:00Z|346|-5")),
            (Some("2026-08-08T00:00:00Z".to_string()), 346, 0)
        );
        assert_eq!(
            parse_condense_marker(Some("2026-08-08T00:00:00Z|346|zzz")),
            (Some("2026-08-08T00:00:00Z".to_string()), 346, 0)
        );
    }

    // --- partial バックオフ ---

    #[test]
    fn partial_backoff_doubles_and_is_capped_by_min_interval() {
        // clean 直後（streak 0）は待たない。
        assert_eq!(partial_backoff_minutes(0, 1440), 0);
        // 1 回目は base、以後 2 倍ずつ。
        assert_eq!(partial_backoff_minutes(1, 1440), 10);
        assert_eq!(partial_backoff_minutes(2, 1440), 20);
        assert_eq!(partial_backoff_minutes(3, 1440), 40);
        // 端数待ち（min_interval）を超えない。
        assert_eq!(partial_backoff_minutes(10, 1440), 1440);
        assert_eq!(partial_backoff_minutes(3, 15), 15);
        // 巨大な streak でも溢れずに上限へ張り付く。
        assert_eq!(partial_backoff_minutes(i64::MAX, 1440), 1440);
    }

    // --- ゲート ---

    #[tokio::test]
    async fn default_off_is_zero_call_and_writes_nothing() {
        let state = crate::test_app_state();
        seed_units(&state, "a1", 5);
        let fake = FakeRunner::new(FakeOutcome::Completed);
        let before = get_marker(&state, "a1");
        let ran = run_condense(
            &state.db,
            &cfg(false, 3, 1440),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(!ran, "既定オフでは起動しない");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0, "口を 1 度も呼ばない");
        assert_eq!(
            get_marker(&state, "a1"),
            before,
            "既定オフではマーカーを書き換えない"
        );
    }

    #[tokio::test]
    async fn no_new_units_after_cursor_is_zero_call() {
        let state = crate::test_app_state();
        seed_units(&state, "a1", 5); // start_log_id 1..5
                                     // カーソルを末尾（5）に置く = 全部凝縮済み。残 0 → 発火しない。
        set_marker(&state, "a1", &format_condense_marker(&hours_ago(240), 5, 0));
        let fake = FakeRunner::new(FakeOutcome::Completed);
        let ran = run_condense(
            &state.db,
            &cfg(true, 3, 1440),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(!ran, "カーソルより新しいユニットが無ければ起動しない");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn full_window_fires_even_when_throttled() {
        let state = crate::test_app_state();
        seed_units(&state, "a1", 10); // 残 10 >= 窓幅 3
                                      // 直前（1 分前）に走ったばかりでも、積み残し（残 >= 窓幅）は throttle を待たず消化する。
        set_marker(&state, "a1", &format_condense_marker(&hours_ago(0), 0, 0));
        let fake = FakeRunner::new(FakeOutcome::Completed);
        let ran = run_condense(
            &state.db,
            &cfg(true, 3, 1440),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(ran, "積み残しは throttle を待たず 1 窓消化する");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        let audit = latest_sleep_audit(&state, "a1").expect("監査ログ");
        assert_eq!(audit["window_units"], 3, "1 回で窓幅ぶんだけ消化");
        assert_eq!(audit["remaining_before"], 10);
        assert_eq!(audit["remaining_after"], 7);
    }

    #[tokio::test]
    async fn tail_below_window_waits_for_interval() {
        let state = crate::test_app_state();
        seed_units(&state, "a1", 2); // 残 2 < 窓幅 3 = 末尾の端数
                                     // 端数は min_interval を待つ。1 分前に走ったばかり（1440 分未達）→ 待つ。
        set_marker(&state, "a1", &format_condense_marker(&hours_ago(0), 0, 0));
        let fake = FakeRunner::new(FakeOutcome::Completed);
        let ran = run_condense(
            &state.db,
            &cfg(true, 3, 1440),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(!ran, "端数は min_interval 未達なら待つ");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn tail_flushes_after_interval_or_on_first_run() {
        // 初回（マーカー無し）は throttle を待たず端数を流す。
        let state = crate::test_app_state();
        seed_units(&state, "a1", 2); // 残 2 < 窓幅 3
        let fake = FakeRunner::new(FakeOutcome::Completed);
        let ran = run_condense(
            &state.db,
            &cfg(true, 3, 1440),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(ran, "初回は端数でも流す");
        let audit = latest_sleep_audit(&state, "a1").expect("監査ログ");
        assert_eq!(audit["window_units"], 2, "端数 2 件を消化");
        let (_, pos, _) = parse_condense_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(pos, 2, "位置は端数の末尾 start_log_id へ");

        // 2 回目以降でも、min_interval を過ぎていれば端数を流す（`tail_below_window_waits_for_interval`
        // の裏。同条件で last_run_at だけを throttle 明けにしたら流れることを見る）。
        let state2 = crate::test_app_state();
        seed_units(&state2, "a1", 2); // 残 2 < 窓幅 3
        set_marker(
            &state2,
            "a1",
            &format_condense_marker(&hours_ago(240), 0, 0),
        );
        let fake2 = FakeRunner::new(FakeOutcome::Completed);
        let ran2 = run_condense(
            &state2.db,
            &cfg(true, 3, 1440), // 1440 分 = 24h < 240h 経過
            &state2.index_build_inflight,
            "a1",
            &fake2,
        )
        .await
        .unwrap();
        assert!(ran2, "端数でも min_interval を過ぎていれば流す");
        assert_eq!(fake2.calls.load(Ordering::SeqCst), 1);
        let audit2 = latest_sleep_audit(&state2, "a1").expect("監査ログ");
        assert_eq!(audit2["window_units"], 2, "端数 2 件を消化");
        let (_, pos2, _) = parse_condense_marker(get_marker(&state2, "a1").as_deref());
        assert_eq!(pos2, 2, "位置は端数の末尾 start_log_id へ");
    }

    // --- 窓の切り出し（decide_condense）---

    #[test]
    fn window_slices_oldest_first_and_next_window_continues() {
        let state = crate::test_app_state();
        let ids = seed_units(&state, "a1", 5); // u1..u5, start_log_id 1..5
                                               // 位置 0（初回）: 最古 3 件が窓。
        match decide_condense(&state.db, &cfg(true, 3, 1440), "a1").unwrap() {
            CondenseDecision::Run(p) => {
                let w: Vec<&str> = p
                    .window_units
                    .iter()
                    .map(|u| u.short_id.as_deref().unwrap())
                    .collect();
                assert_eq!(
                    w,
                    vec![ids[0].as_str(), ids[1].as_str(), ids[2].as_str()],
                    "古い順の最初の 3 件"
                );
                assert_eq!(p.position_before, 0);
                assert_eq!(p.position_after, 3, "窓末尾 start_log_id");
                assert_eq!(p.remaining_before, 5);
            }
            other => panic!("expected Run, got {other:?}"),
        }
        // 位置 3（1 窓消化済み・初回でない）: 残 2 件が窓（端数だが throttle 明け）。
        set_marker(&state, "a1", &format_condense_marker(&hours_ago(240), 3, 0));
        match decide_condense(&state.db, &cfg(true, 3, 1440), "a1").unwrap() {
            CondenseDecision::Run(p) => {
                let w: Vec<&str> = p
                    .window_units
                    .iter()
                    .map(|u| u.short_id.as_deref().unwrap())
                    .collect();
                assert_eq!(w, vec![ids[3].as_str(), ids[4].as_str()], "続きの 2 件");
                assert_eq!(p.position_after, 5);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    // --- clean / partial のマーカー ---

    #[tokio::test]
    async fn clean_run_advances_position_to_window_tail() {
        let state = crate::test_app_state();
        seed_units(&state, "a1", 5); // start_log_id 1..5, 窓幅 3
        let fake = FakeRunner::new(FakeOutcome::Completed);
        let ran = run_condense(
            &state.db,
            &cfg(true, 3, 1440),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(ran);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        let (_, pos, _) = parse_condense_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(pos, 3, "clean は位置を今回の窓の末尾 start_log_id へ進める");
        let audit = latest_sleep_audit(&state, "a1").expect("監査ログ");
        assert_eq!(audit["outcome"], "completed");
        assert_eq!(audit["position_advanced"], true);
        assert_eq!(audit["position_after"], 3);
        assert_eq!(audit["window_units"], 3);
    }

    #[tokio::test]
    async fn partial_holds_position_but_advances_throttle() {
        let state = crate::test_app_state();
        seed_units(&state, "a1", 5);
        // 位置 2 まで消化済み・240h 前（throttle 明け）。残 3 >= 窓幅で発火。
        set_marker(&state, "a1", &format_condense_marker(&hours_ago(240), 2, 0));
        let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);
        let ran = run_condense(
            &state.db,
            &cfg(true, 3, 1440),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(ran, "起動はした（partial）");
        let (ts, pos, _) = parse_condense_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(pos, 2, "partial は位置を据え置く（次 tick で同じ窓を読む）");
        let advanced = ts
            .and_then(|s| s.parse::<DateTime<Utc>>().ok())
            .map(|dt| Utc::now().signed_duration_since(dt) < Duration::hours(1))
            .unwrap_or(false);
        assert!(
            advanced,
            "partial でも throttle は now へ進む（端数待ちの起点をリセット）"
        );
        let audit = latest_sleep_audit(&state, "a1").expect("監査ログ");
        assert_eq!(audit["outcome"], "stopped_by_limit");
        assert_eq!(audit["position_advanced"], false);
        assert_eq!(audit["throttle_advanced"], true);
        // 位置を進めていない＝1 件も消化していないので、残数も減らない（監査の整合）。
        assert_eq!(audit["remaining_before"], 3);
        assert_eq!(audit["remaining_after"], 3, "partial は残数を減らさない");
    }

    /// 積み残し（残 >= 窓幅）は throttle を通らないので、partial が続くと毎 tick フルの LLM ランが
    /// 走り続ける。連続 partial のバックオフでそれを間引く（本体の無限再走ループ回避）。
    #[tokio::test]
    async fn repeated_partial_backs_off_before_rerunning_same_window() {
        let state = crate::test_app_state();
        seed_units(&state, "a1", 10); // 残 10 >= 窓幅 3（積み残し = throttle を通らない経路）
        set_marker(&state, "a1", &format_condense_marker(&hours_ago(240), 0, 0));
        let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);

        // 1 回目: streak 0 なのでバックオフ無し → 走る。partial なので位置据え置き・streak 1。
        let ran1 = run_condense(
            &state.db,
            &cfg(true, 3, 1440),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(ran1, "1 回目は走る（バックオフ無し）");
        let (_, pos1, streak1) = parse_condense_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(pos1, 0, "partial は位置を据え置く");
        assert_eq!(streak1, 1, "partial で連続回数が 1 になる");

        // 2 回目（直後）: 積み残しのままだが streak 1 のバックオフ（10 分）が明けていない → 待つ。
        let ran2 = run_condense(
            &state.db,
            &cfg(true, 3, 1440),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(!ran2, "連続 partial のあとは積み残しでもバックオフで待つ");
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            1,
            "2 回目は LLM の口を呼ばない（毎 tick フルランの暴走を止める）"
        );
        // 待っている間もカーソルは動かさない（窓を捨てて材料を失わない）。
        let (_, pos2, streak2) = parse_condense_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(pos2, 0, "バックオフ中もカーソルは強制前進しない");
        assert_eq!(streak2, 1, "スキップでは streak も増やさない");
    }

    /// バックオフは clean 1 回で完全解除される（積み残しの消化速度を落としたままにしない）。
    #[tokio::test]
    async fn clean_run_clears_partial_backoff() {
        let state = crate::test_app_state();
        seed_units(&state, "a1", 10); // 残 10 >= 窓幅 3
                                      // 連続 partial 3 回ぶんの状態から始める（バックオフ 40 分）。240h 前なのでもう明けている。
        set_marker(&state, "a1", &format_condense_marker(&hours_ago(240), 0, 3));
        let fake = FakeRunner::new(FakeOutcome::Completed);

        let ran1 = run_condense(
            &state.db,
            &cfg(true, 3, 1440),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(ran1, "バックオフが明けていれば走る");
        let (_, pos1, streak1) = parse_condense_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(pos1, 3, "clean は位置を窓の末尾へ進める");
        assert_eq!(streak1, 0, "clean で連続 partial 回数が 0 に戻る");

        // 直後にもう一度: streak 0 なのでバックオフ無し（streak 3 のままなら 40 分待たされていた）。
        let ran2 = run_condense(
            &state.db,
            &cfg(true, 3, 1440),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(ran2, "clean 後は直後の tick でも積み残しを続けて消化する");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
        let (_, pos2, _) = parse_condense_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(pos2, 6, "次の窓も消化して位置が進む");
    }

    #[tokio::test]
    async fn error_outcome_holds_position() {
        let state = crate::test_app_state();
        seed_units(&state, "a1", 5);
        let fake = FakeRunner::new(FakeOutcome::Error);
        let ran = run_condense(
            &state.db,
            &cfg(true, 3, 1440),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(ran);
        let (_, pos, _) = parse_condense_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(
            pos, 0,
            "error（partial）は位置を据え置く（初回 position=0）"
        );
        let audit = latest_sleep_audit(&state, "a1").expect("監査ログ");
        assert_eq!(audit["outcome"], "error");
        assert_eq!(audit["position_advanced"], false);
    }

    // --- RunRequest の本番配線 ---

    #[tokio::test]
    async fn run_request_wiring_is_sleep_owner_allowlisted_and_no_send() {
        let state = crate::test_app_state();
        seed_units(&state, "a1", 5);
        let fake = FakeRunner::new(FakeOutcome::Completed);
        run_condense(
            &state.db,
            &cfg(true, 3, 1440),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        let cap = fake.captured.lock().unwrap();
        let cap = cap.as_ref().expect("captured req");
        assert_eq!(cap.gateway, "sleep");
        assert!(cap.caller_is_owner, "caller=Owner");
        assert!(
            !cap.has_gateway_actions,
            "送信経路を渡さない（会話へ出さない）"
        );
        assert!(!cap.persist_turn_logs, "生ログに書かない（#393）");
        let allow = cap.tool_allowlist.as_ref().expect("allowlist");
        let expected: Vec<String> = CONDENSE_ALLOWED_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(allow, &expected);
    }

    #[test]
    fn allowlist_has_core_tools_and_excludes_outward_and_unit_writes() {
        for t in [
            "record_memory_core",
            "update_memory_core",
            "retract_memory_core",
            "search_memory_index",
            "search_my_history",
            "declare_done",
        ] {
            assert!(CONDENSE_ALLOWED_TOOLS.contains(&t), "{t} は凝縮ランに要る");
        }
        for forbidden in [
            "execute_shell",
            "nostr_run",
            "spawn_subtask",
            "ws_write",
            "update_instructions",
            // 生ログを刻む道具は凝縮ランには渡さない（宣言ランの領分）。
            "record_memory_unit",
            "retract_memory_unit",
            "plan_next_memory_window",
        ] {
            assert!(
                !CONDENSE_ALLOWED_TOOLS.contains(&forbidden),
                "{forbidden} は凝縮ランに渡してはいけない"
            );
        }
    }

    // --- プロンプト ---

    #[test]
    fn prompt_shows_units_open_axes_and_existing_cores() {
        let state = crate::test_app_state();
        let unit_ids = seed_units(&state, "a1", 3);
        // 既存 core を 1 件仕込む（更新候補として出るか）。
        {
            let conn = state.db.lock().unwrap();
            let now = Utc::now().to_rfc3339();
            opencrab_db::queries::record_memory_core(
                &conn,
                "a1",
                "繰り返していること",
                "同じ判断を何度も繰り返している",
                &[unit_ids[0].clone()],
                &now,
            )
            .unwrap();
        }
        let plan = match decide_condense(&state.db, &cfg(true, 1, 1440), "a1").unwrap() {
            CondenseDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let sp = build_system_prompt(&plan);
        // 今回の窓のユニットが並ぶ（逐次凝縮）。
        assert!(sp.contains("今回の窓"), "窓ベースの提示");
        assert!(sp.contains(&unit_ids[0]), "窓のユニットの short_id が出る");
        // 軸は開いて見せる（自分の軸を足せる）。
        assert!(sp.contains("あなた自身の軸を足す"), "軸は開いた提示");
        assert!(sp.contains("繰り返していること"), "基本の軸が出る");
        // 基本の 7 軸それぞれに目を通し、無い軸は「今回は無い」と明示させる。
        assert!(sp.contains("基本の軸"), "基本の軸セクションがある");
        assert!(sp.contains("今回は無い"), "無い軸を明示する指示がある");
        assert!(
            sp.contains("でっち上げる"),
            "無い軸を薄い凝縮ででっち上げないよう戒めている"
        );
        // 重心は本人の興味・関心（正しさの教訓ではなく心が動いた先）。
        assert!(
            sp.contains("あなた自身の興味・関心"),
            "核の中心を本人の興味・関心に据える"
        );
        assert!(
            sp.contains("心が何に動き"),
            "各軸でも『心が何に動いたか』を探させる"
        );
        // 更新は継ぎ足しではなく蒸留（窓を重ねても core を長くしない）。
        assert!(
            sp.contains("窓を重ねても長くしないでください"),
            "核を磨いた短文に保ち、窓ごとの肥大を禁じている"
        );
        assert!(
            sp.contains("継ぎ足しではなく蒸留"),
            "更新の意味を蒸留として明示している"
        );
        // 短くする道は抽象化ではなく選択（まとめずに捨てさせる）。
        assert!(
            sp.contains("凝縮は抽象化ではなく選択です"),
            "短縮の手段を抽象化ではなく選択だと明示している"
        );
        assert!(
            sp.contains("実在の出来事を 1 つだけ選び"),
            "1 核につき実在の出来事を 1 つ残させる"
        );
        assert!(
            sp.contains("選ばなかった材料は本文から"),
            "選ばなかった材料を本文から捨てさせる（根拠はリンクへ）"
        );
        // 標語・自己引用は「具体」ではない（v3 でここが抽象化の隠れ蓑になった）。
        assert!(
            sp.contains("標語・スローガン・自分の言い回しの引用は「具体」ではありません"),
            "自作の言い回しの鍵括弧引用を具体として認めない"
        );
        assert!(
            sp.contains("誰が実際に何をしたか"),
            "具体の定義を『誰が何をしたか』に固定している"
        );
        // 見本は骨格プレースホルダで示す。実在の人名・私的な出来事を書くと、公開リポに個人情報が
        // 残るうえ、本人が自分のユニットではなく見本から具体を借りてしまう。
        for skeleton in ["〈誰〉が〈何〉をしてくれた", "〈どこ〉で〈何〉を見た"]
        {
            assert!(
                sp.contains(skeleton),
                "具体の見本は骨格プレースホルダで示す: {skeleton}"
            );
        }
        assert!(
            sp.contains("あなたのユニットにある実際の名前と出来事で埋めます"),
            "プレースホルダを本人のユニットで埋めさせる（見本から具体を借りさせない）"
        );
        // 「無理に出さない」を選べる（この窓で）。
        assert!(sp.contains("無理に何か出す必要はありません"));
        // 更新優先で core を育てる（逐次凝縮の要）。
        assert!(sp.contains("育てて"), "既存 core を update で育てる指示");
        assert!(sp.contains("更新を優先"));
        // 根拠リンク。
        assert!(sp.contains("根拠のユニット"));
        // 既存 core が更新候補として出る。
        assert!(sp.contains("同じ判断を何度も繰り返している"));
        assert!(sp.contains("record_memory_core"));
    }
}
