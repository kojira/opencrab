//! スリープ宣言ラン本体（#384 / #376 段階2）。
//!
//! **記憶の「単位」をエージェント自身に決めさせる。** 段階1（#379 / PR #383）で道具は入った
//! が、まだ誰も使っていなかった。ここが sleep 中にそれを走らせる: エージェント本人が新規の
//! 別セッション・本人の人格で、自分の生ログ（`memory_sessions`）を俯瞰し、「ここからここまでが
//! 一つの記憶だ」と宣言する（`record_memory_unit`）。機械が刻んだ索引の区切りではなく、本人が
//! 意味の切れ目を決める。
//!
//! **タグ整理ラン（[`crate::memory_organize`]）とは別ラン**にする（#376 の設計）。入力（生ログ
//! vs topic）も進捗マーカー（本モジュールの単一カーソル vs あちらの 3 列）も別物だから。ただし
//! **足回りは共有する**（新しいエンジンや口を作らない）:
//! - 1 ターンを回す口は [`crate::memory_organize::OrganizeTurnRunner`] を再利用（本番＝
//!   `run_agent_response` / テスト＝結果差し替えフェイク。#370 の隔離構造をそのまま使う）。
//! - 二重起動防止は `try_acquire_build_slot`（キーは `declare:{agent_id}`）。
//! - caller=Owner + ツール許可リスト（[`DECLARE_ALLOWED_TOOLS`]）で外向きの手を塞ぐ。
//!
//! 絶対に守るもの（#384）:
//! - **対話ターンでは走らせない**（#291）。呼び出し元は sleep ループのみ。
//! - **結果を会話へ自動注入しない**（#316）。system プロンプトはここで自前に組むので
//!   `[Memory Index]` の注入経路（`build_agent_context`）は通らない。
//! - **1 エージェント内しか見ない**（他エージェントの記憶を混ぜない）。全クエリが `agent_id` 固定。
//! - **生ログを消さない・変更しない**（読むだけ / 宣言は派生ノードで `retract` 可逆）。
//! - **位置は前進のみ**（partial では位置を進めない）。clean 完了時だけ前へ進める。
//!   ただし **throttle（日次ゲート用の壁時計）は clean/partial に関わらず毎回 `now` へ進める**
//!   （partial で据え置くと同じ窓のまま tick 毎に再発火して LLM を呼び続けるため / #366 と同型）。
//! - **窓の境界と広さも本人が決める**（#394）。「どこからどこまでが一つの記憶か」を本人が決める
//!   設計なのに、窓だけは機械が固定で切っていた。`plan_next_memory_window` で次回の開始位置
//!   （＝オーバーラップ）と窓の広さを表明でき、ランの側はそれを**前進の下限
//!   （[`MIN_ADVANCE_DIVISOR`]）と上限（[`MAX_ADVANCE_WINDOWS`]）へ丸めてから**使う。
//!   丸めがあるので、宣言ゼロ・指定なし・現在位置以下の指定でも**必ず前進する**。
//! - **既定オフ**。`enabled=false` なら RunRequest すら組まずゼロコールで即 return。

use chrono::{DateTime, Duration, Utc};
use serde_json::json;

use crate::config::MemoryDeclareConfig;
use crate::memory_maintenance::IndexBuildInflight;
use crate::memory_organize::{AppStateTurnRunner, OrganizeTurnRunner};
use crate::AppState;
use opencrab_actions::{CallerIdentity, RunRequest};
use opencrab_db::queries::{DeclareWindow, HistorySurvey, IndexNodeRow};

/// 地図（`survey_my_history`）としてプロンプトに同梱するバケット数の上限。
/// day 粒度で概ね直近 1 か月ぶん。俯瞰の手がかりで、全量はエージェントが自分で引く。
const SURVEY_BUCKETS: usize = 30;

/// プロンプトに載せる「すでに宣言した記憶」の最大件数（本人の直近の宣言を思い出す手がかり）。
const RECENT_UNITS_SHOWN: usize = 15;

/// sleep 宣言ランに渡すツール許可リスト（#384）。
///
/// 宣言ランの用途は「自分の生ログを俯瞰・範囲読みして、まとまりを宣言する」に固定される。
/// 必要なのは**生ログの読み取り**（survey / read / search）と**宣言の記録/取り消し**、そして
/// **ターンを終える最小限のラン制御**（declare_done）だけ。
///
/// **タグ整理ラン（[`crate::memory_organize::ORGANIZE_ALLOWED_TOOLS`]）とは別のリスト**にする:
/// - あちらには無い `record_memory_unit` / `retract_memory_unit`（段階1 で allowlist へ入れず
///   段階2 のこのランで初めて渡す / #379）を載せる。
/// - あちらの `tag_topic` / `untag_topic` / `merge_tags`（機械が作った topic への分類）や
///   `browse/search/retrieve_memory_index`（機械の索引を見る）は**載せない**。宣言は生ログを
///   直接読んで切れ目を決める仕事で、機械の索引の区切りに引きずられないため。
///
/// `execute_shell` / `nostr_run` / `spawn_subtask` / `ws_write` / `configure_*` /
/// `update_instructions` 等の外向き・状態書き換えツールは一切渡さない。この許可リストは
/// `RunRequest.tool_allowlist` 経由で可視性（`list_tools`）と実行（`dispatch_inner`）の**両方**を、
/// **全スロット**（dispatcher / gateway own / MCP）にわたって絞る。既存の caller ゲート
/// （記録2つは `TRUSTED_ONLY`）は弱めず、その**上に重ねる**。
pub const DECLARE_ALLOWED_TOOLS: &[&str] = &[
    // 生ログの俯瞰・範囲読み・全文検索（読むだけ）
    "survey_my_history",
    "read_my_history",
    "search_my_history",
    // 記憶の単位の記録 / 取り消し（段階2 で初めて渡す）
    "record_memory_unit",
    "retract_memory_unit",
    // 次回の窓（境界と広さ）を本人が決める（#394）
    "plan_next_memory_window",
    // ラン制御（ターンを終える宣言のみ）
    "declare_done",
];

/// 本人が窓の位置を指定しても、clean 完了時には**最低でも提示窓の何分の 1 かは必ず前へ進む**
/// ——その分母（#394）。
///
/// カーソルを完全に本人任せにすると、宣言ゼロ・同じ位置の指定・現在位置以下の指定で**同じ窓を
/// 永久に再取得するループ**に入る（#374 で実際に踏んだ罠）。かといって「1 件でも進めば良い」に
/// すると、窓 300 に対して 1 件ずつしか進まないラン（＝実質ループ）を止められない。提示した窓の
/// 1/3 を下限にすると、**どんな指定でも 1 つの窓は最悪 3 ラン（日次なら 3 日）で必ず抜ける**一方、
/// 「続いている出来事の末尾を次回へ回す」用途には窓の 2/3 まで使える。
const MIN_ADVANCE_DIVISOR: i64 = 3;

/// 本人が指定できるカーソルの**上限**を、提示窓の何倍の件数までにするか（#394）。
///
/// `record_memory_unit` は窓に縛られないので、本人は窓の終端を越えた範囲を宣言できる。その分を
/// 次の窓から外すには終端より先を指せる必要がある。一方で桁違いの値（総ログ数を越える id 等）を
/// そのまま呑むと、**読んでいない生ログを丸ごと飛ばして二度と窓に入らない**。窓 1 つぶんの
/// 越境（＝合計 2 窓ぶん）まで許せば「越えて宣言した続きから」は成立し、それ以上の飛ばしは起きない。
const MAX_ADVANCE_WINDOWS: i64 = 2;

/// partial（timeout / ターン上限 / エラー）がこの回数**連続**したら、本人が表明した窓の広さを
/// 破棄して config の既定へ戻す（#394）。**戻す対象は「既定より広い」表明だけ**——狭める方向の
/// 表明（#394 のオーナー要件「濃い範囲では窓を縮めて丁寧に見たい」）は、partial の原因になり得
/// ないので機械が取り上げない。
///
/// 広さは sticky なので、本人が広げすぎてターンが毎回潰れると**位置が 1 件も進まないまま
/// 発火し続ける**。ターンが潰れる状況では `plan_next_memory_window` を呼ぶ余地も無いので、
/// 放っておくと本人が自分で狭めるまで抜けられない（自力での回復が保証されない）。
///
/// 3 にする理由: 1 回の partial は珍しくない（LLM の一時的な遅延・失敗でも起きる）ので、
/// 1 や 2 で戻すと本人の設定が些細な揺らぎで消える。一方、日次（既定 1440 分）なら 3 日、
/// バックログ消化（`min_interval_minutes = 1` / maintenance tick 既定 600 秒）でも 30 分ほどで
/// 回復するので、空回りが数時間に伸びることは無い。**clean が 1 回通れば連続は切れる**。
const MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET: i64 = 3;

/// このエージェントの宣言ランを（ゲートを満たせば）実行する。**本番エントリ**。
///
/// 本番のラン構築（`run_agent_response`）を [`AppStateTurnRunner`] に閉じ込め、宣言ランの
/// ロジック本体は [`run_declare`] に委譲する（#370 の構造をタグ整理ランと共有）。
///
/// 戻り値: 宣言ラン（LLM）を実際に起動したら `true`。既定オフ・ゲート未達は `false`
/// （＝ LLM ゼロコール）。
pub async fn maybe_run_memory_declare(state: &AppState, agent_id: &str) -> anyhow::Result<bool> {
    let runner = AppStateTurnRunner { state };
    run_declare(
        &state.db,
        &state.memory_declare,
        &state.index_build_inflight,
        agent_id,
        &runner,
    )
    .await
}

/// 宣言ラン（sleep）のロジック本体。**必要な手足だけ**を引数で受け取る（#370）:
/// DB・設定・二重起動スロット・1 ターンを回す [`OrganizeTurnRunner`]。
///
/// `AppState` を受け取らないので、この関数からは gateway/MCP/activity webhook を構築できない
/// （構造的に外へ出ない）。1 ターンを走らせる部分だけを `runner` に委ね、本番は
/// `run_agent_response` 実装、テストは結果差し替えのフェイクを渡す。
async fn run_declare(
    db: &opencrab_db::Db,
    cfg: &MemoryDeclareConfig,
    inflight: &IndexBuildInflight,
    agent_id: &str,
    runner: &dyn OrganizeTurnRunner,
) -> anyhow::Result<bool> {
    // 既定オフ: ここで即 return する。RunRequest も DB 書き込みも一切しない（ゼロコール）。
    if !cfg.enabled {
        return Ok(false);
    }

    // --- ゲート判定 + 窓組み立て（DB 読みのみ。ロックは await を跨がない）---
    let plan = match decide_declare(db, cfg, agent_id)? {
        DeclareDecision::Skip(reason) => {
            tracing::debug!(agent_id, reason, "memory declare: skipped by gate");
            return Ok(false);
        }
        DeclareDecision::Run(plan) => plan,
    };

    // --- 排他（索引ビルド・タグ整理と衝突しない名前空間キー）---
    let guard =
        crate::memory_maintenance::try_acquire_build_slot(inflight, &format!("declare:{agent_id}"));
    let Some(_guard) = guard else {
        return Ok(false); // 既に走っている
    };

    // --- 起動（新規の別セッション / 本人の人格 / caller=Owner）---
    let now = Utc::now();
    let session_id = format!("sleep-declare-{agent_id}-{}", now.timestamp());
    let system_prompt = build_system_prompt(&plan);
    let conversation = build_task_message(&plan);

    // gateway_actions=None（送信経路を渡さない = 会話へ出さない）。dispatch なし（inline 実行）。
    // ツール許可リスト（#384）で caller=Owner の全ツールから宣言に要る分だけへ絞る。
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
        DECLARE_ALLOWED_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    )
    // このランのターンは生ログ（`memory_sessions`）に**書かない**（#393）。書くと 1 ラン
    // 35〜65 行を生産し、それが次の宣言ランの窓に入って「記憶を整理した」という記憶を
    // 作り始める（実際に本番で起きた）。整備作業は本人の生きた体験ではない。
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
            tracing::warn!(agent_id, error = %e, "memory declare run failed");
            ("error", false)
        }
        Err(_) => ("timeout", false),
    };

    // --- 前進（前進のみ / 位置 + throttle を 1 列に刻む）---
    // **throttle（壁時計）は clean/partial に関わらず毎回 `now` へ進める。位置は clean のときだけ
    // 提示窓の末尾（`to_id`）へ進める（partial では据え置き）。** 複合カーソル 1 列の位置部と
    // throttle 部を別々に扱う。
    //
    // なぜ partial でも throttle を進めるか（#366 と同じ理由でタグ整理ランが位置と throttle を
    // 分離したのと同型）: 日次ゲートは throttle 部で判定する。partial（timeout / ターン上限 /
    // エラー）で throttle を据え置くと、位置も進まないため**次の maintenance tick（既定 600 秒）で
    // 同じ窓のまま再発火**し、clean が 1 回通るまで 10 分おきに LLM を呼び続ける（無人・夜間の
    // 暴走）。throttle を `now` へ進めれば、次 tick は日次ゲートで弾かれ、翌日に同じ窓を再挑戦する。
    //
    // 位置を進めるのは clean のときだけ（**提示したら進める**＝本人が意図的に宣言しなかった範囲を
    // 毎回拾い直さない / 一期一会）。**位置を進めないと無限ループ**（#374）だが、それは clean 側で
    // 必ず `to_id`（> 現カーソル）へ前進することで塞ぐ。partial で位置据え置きでも throttle が翌日
    // まで再発火を止めるので暴走しない。record は範囲不変なので、翌日 clean で重複宣言してもユニットが
    // 増えるだけで壊れず、本人が retract できる。
    //
    // **窓の終端は既定であって決定ではない**（#394）。本人がターン中に
    // `plan_next_memory_window(next_from_id=...)` で「次はここから」を表明していれば、その
    // 手前（`next_from_id - 1`）を位置にする。ただし必ず `[min_position, max_position]` へ
    // 丸める（下限＝提示窓の 1/3 は必ず進む・上限＝2 窓ぶんより先へは飛ばない）。指定が無い・
    // 宣言ゼロ・現在位置以下の指定は、いずれもこの丸めで下限以上へ引き上げられる＝**必ず前進**。
    let requested = {
        let conn = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        opencrab_db::queries::get_memory_declare_window(&conn, agent_id)?
    };
    let requested_next_from_id = requested.as_ref().and_then(|p| p.next_from_id);
    let requested_note = requested.as_ref().and_then(|p| p.note.clone());
    let position = if clean {
        match requested_next_from_id {
            // 次の窓が `next_from_id` から始まる ⇔ カーソル（提示し終えた末尾）はその 1 つ手前。
            Some(next_from) => {
                (next_from.saturating_sub(1)).clamp(plan.min_position, plan.max_position)
            }
            None => plan.window.to_id.unwrap_or(plan.cursor_id),
        }
    } else {
        plan.cursor_id
    };
    let marker_after = format_marker(&now.to_rfc3339(), position);

    // 窓の希望の後始末（#394）。
    //
    // - **位置（`next_from_id`）と理由（`note`）はこのランで使い切る**（clean / partial を
    //   問わず消す）。位置を残すと、次の窓を見てもいない過去の指定が後のランのカーソルを
    //   引き戻し続ける。`note` は「その位置をそう決めた理由」なので寿命は位置と同じ——残すと
    //   以後すべてのランの監査 `window_note` に同じ文字列が出続け、「このランで本人がこう
    //   書いた」と誤読される。
    // - **広さ（`window_size`）は sticky**（本人が上書きするまで効く）。ただし partial が
    //   続いたら自動で手放す（下記）。
    let mut after = requested.clone().unwrap_or_default();
    after.next_from_id = None;
    after.note = None;

    // **partial が続いたら本人の広さを既定へ戻す**（自力で回復できない状態を作らない）。
    //
    // 広さは sticky なので、本人が広げすぎてターンが毎回潰れると、位置が 1 件も進まないまま
    // 発火し続ける。しかもターンが潰れる状況では `plan_next_memory_window` を呼ぶ余地も
    // 無いので、**本人が自分で狭めるまで抜けられない**。日次（既定 1440 分）なら軽微だが、
    // バックログ消化では `min_interval_minutes = 1` で回すため maintenance tick ごと
    // （既定 600 秒）に発火し、数時間ぶん空回りする。
    //
    // 数える対象は「**次のランで config の既定より広くなる**表明があるとき」だけ。この安全弁の
    // 目的は「広げすぎて毎回ターンが潰れる状態からの回復」なので、既定以下の設定を機械が取り
    // 上げる理由が無い。むしろ:
    // - 本人は既定より**狭い**値も表明できる（#394 のオーナー要件「密に拾う個性 → 濃い範囲では
    //   窓を縮めて丁寧に見たい」）。狭い設定を破棄すると窓は既定へ**広がる**——partial の原因
    //   （timeout / ターン上限）は広い窓の側で起きるので、原因でないものを取り上げて悪化させる。
    // - `clean` は `completed` だけが真で、LLM 側の一時障害（`error`）も 1 回として数える。
    //   消化中は `min_interval_minutes = 1` なので、プロバイダが数十分不調なだけで連続が伸びる。
    // - 本人がターン中に「広すぎたので狭くする」と自己修正した場合、`requested` はターンの
    //   **後**に読むので、その狭い値がそのまま次の判定に入る。既定以下なので数えられず、
    //   書いたばかりの値が巻き添えで消えることも無い。
    //
    // 判定に使う広さは `decide_declare` と**同じ丸め**（[`effective_window_size`]）を通す。
    // clean が 1 回通れば連続は切れる。戻すのは希望の破棄だけで、次に本人が
    // `plan_next_memory_window` を呼べばまた広げられる（恒久的に禁止しない）。
    let mut window_size_auto_reset = false;
    let widened_beyond_default = after.window_size.is_some()
        && effective_window_size(after.window_size, cfg) > cfg.max_logs.max(1);
    if clean || !widened_beyond_default {
        after.partial_streak = None;
    } else {
        let streak = after.partial_streak.unwrap_or(0).saturating_add(1);
        if streak >= MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET {
            after.window_size = None;
            after.partial_streak = None;
            window_size_auto_reset = true;
        } else {
            after.partial_streak = Some(streak);
        }
    }
    let partial_streak_after = after.partial_streak.unwrap_or(0);

    {
        let conn = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        opencrab_db::queries::set_memory_declare_cursor(&conn, agent_id, &marker_after)?;

        // 中身が空になったら列ごと NULL へ戻す（「希望なし」と同じ状態にする）。
        let after_opt = (after != Default::default()).then_some(&after);
        if after_opt != requested.as_ref() {
            opencrab_db::queries::set_memory_declare_window(&conn, agent_id, after_opt)?;
        }
    }

    // --- 監査（層1: agent_logs / context="sleep"）---
    // 層2（生プロンプト/生応答）は `run_agent_response` が LLM コールごとに llm_logs へ残す。
    {
        let audit = json!({
            "kind": "memory_declare",
            "outcome": outcome,
            "cursor_before": plan.cursor_id,
            "window_from_id": plan.window.from_id,
            "window_to_id": plan.window.to_id,
            "window_log_count": plan.window.log_count,
            "window_session_count": plan.window.session_count,
            "total_remaining": plan.window.total_remaining,
            "session_id": session_id,
            // 窓の広さ（本人の希望か config 既定か）と、位置の希望・丸めの範囲（#394）。
            "window_size": plan.window_size,
            "window_size_preferred": plan.preferred_window_size,
            "requested_next_from_id": requested_next_from_id,
            "position": position,
            "position_min": plan.min_position,
            "position_max": plan.max_position,
            "window_note": requested_note,
            // partial の連続と、それによる広さの自動リセット（#394）。`true` なら次のランは
            // config の既定の広さで走る（本人が再び表明すればまた広がる）。
            "partial_streak": partial_streak_after,
            "window_size_auto_reset": window_size_auto_reset,
            // 位置は clean のときだけ前進。throttle は毎回 now（partial の再発火を止める）。
            "position_advanced": clean,
            "throttle_advanced": true,
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
            tracing::warn!(agent_id, error = %e, "failed to persist memory declare audit log");
        }
    }

    tracing::info!(
        agent_id,
        outcome,
        window = plan.window.log_count,
        marker_advanced = clean,
        "memory declare ran"
    );
    Ok(true)
}

/// 宣言ランの実行計画（ゲート通過時のみ組む）。
#[derive(Debug)]
struct DeclarePlan {
    persona_name: String,
    personality: Option<String>,
    instructions: String,
    /// 今回提示する未宣言の窓（地図＝集計のみ。本文は含めない）。
    window: DeclareWindow,
    /// 生ログ全体の地図（day 粒度）。プロンプトに同梱する俯瞰の手がかり。
    survey: HistorySurvey,
    /// すでに宣言した記憶（新しい順 / 最大 [`RECENT_UNITS_SHOWN`] 件）。本人の直近の手癖を示す。
    recent_units: Vec<IndexNodeRow>,
    /// 現在のマーカー位置（生ログ id）。プロンプトの「ここまで宣言済み」の表示・partial 時に
    /// 位置を据え置く値として使う（clean は既定で `window.to_id` へ進む）。
    cursor_id: i64,
    /// 今回の窓の広さ（生ログ件数）。本人の希望（sticky）があればそれ、無ければ config の
    /// `max_logs`。プロンプトにも出して、本人が「広い/狭い」を判断できるようにする（#394）。
    window_size: i64,
    /// 本人が既に表明している窓の広さ（sticky）。未表明なら `None`（＝ config の既定で走っている）。
    preferred_window_size: Option<i64>,
    /// 既定の窓の広さ（＝ `cfg.max_logs.max(1)`。表明が無いときに使う値）。自動リセットの
    /// 判定 `effective > cfg.max_logs.max(1)`（#394 の `widened_beyond_default`）が比べる
    /// のとまさに同じ値。本人が表明済みだと size は自分の値しか出ないので、既定を併記して
    /// 「自分の設定が既定より広いか＝自動リセットが自分に掛かるか」を本人が判定できるようにする（#399）。
    default_window_size: i64,
    /// clean 完了時にカーソルを置ける**下限**（＝ここまでは必ず前進する / #394）。
    /// 提示窓の `1/MIN_ADVANCE_DIVISOR` 件目の生ログ id。窓が小さければ窓の終端。
    min_position: i64,
    /// clean 完了時にカーソルを置ける**上限**（#394）。提示窓の `MAX_ADVANCE_WINDOWS` 倍の
    /// 件数ぶん先の生ログ id。それより先に生ログが無ければ最後の id。
    max_position: i64,
}

/// ゲート判定の結果。
#[derive(Debug)]
enum DeclareDecision {
    /// 発火しない（理由つき）。
    Skip(&'static str),
    /// 発火する。`DeclarePlan` は大きいので Box する（clippy::large_enum_variant）。
    Run(Box<DeclarePlan>),
}

/// 本人の表明（`preferred`）を、そのランで**実際に使う窓の広さ**へ丸める（#394）。
///
/// **config の既定は変えない**——本人が表明したときだけ、その値を上下限へ丸めて使う（未表明の
/// エージェントは従来どおり `max_logs` そのままで走る）。上限は運用の設定より狭くならないよう
/// `max` を取る（`max_logs` を [`opencrab_actions::memory_units::DECLARE_WINDOW_MAX`] 超に
/// 設定した運用を勝手に絞らない / 表明した瞬間に窓が狭まるのを防ぐ）。
///
/// 窓を組むとき（[`decide_declare`]）と、partial の連続を数えるかどうかの判定（[`run_declare`]）
/// の**両方**がここを通る。別々に丸めると、判定が実際の広さとずれる。
fn effective_window_size(preferred: Option<i64>, cfg: &MemoryDeclareConfig) -> i64 {
    match preferred {
        Some(v) => v.clamp(
            opencrab_actions::memory_units::DECLARE_WINDOW_MIN,
            opencrab_actions::memory_units::DECLARE_WINDOW_MAX.max(cfg.max_logs),
        ),
        None => cfg.max_logs.max(1),
    }
}

/// ゲート（日次 throttle + 下限）を判定し、通れば窓・地図・人格を積んだ計画を返す。
///
/// DB 読みのみ。ロックは関数内で完結し、`run_agent_response` の await を跨いで保持しない。
fn decide_declare(
    db: &opencrab_db::Db,
    cfg: &MemoryDeclareConfig,
    agent_id: &str,
) -> anyhow::Result<DeclareDecision> {
    let now = Utc::now();
    let conn = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;

    // マーカー = 複合カーソル `"{last_run_at}|{cursor_id}"`。未実行（None）は (throttle 無し, 0)。
    let marker = opencrab_db::queries::get_memory_declare_cursor(&conn, agent_id)?;
    let (last_run_at, cursor_id) = parse_marker(marker.as_deref());

    // ゲート1: 日次 throttle。last_run_at が無ければ（初回）throttle は掛からない。
    if let Some(lr) = &last_run_at {
        let elapsed = lr
            .parse::<DateTime<Utc>>()
            .map(|dt| now.signed_duration_since(dt))
            .unwrap_or_else(|_| Duration::zero());
        if elapsed < Duration::minutes(cfg.min_interval_minutes.max(1)) {
            return Ok(DeclareDecision::Skip("interval_not_elapsed"));
        }
    }

    let pref = opencrab_db::queries::get_memory_declare_window(&conn, agent_id)?;
    let preferred_window_size = pref.as_ref().and_then(|p| p.window_size);
    let window_size = effective_window_size(preferred_window_size, cfg);

    // 未宣言の窓（マーカーより新しい生ログを id 昇順で最大 window_size 件）。
    let window = opencrab_db::queries::declare_window(&conn, agent_id, cursor_id, window_size)?;

    // ゲート2: 発火の下限。マーカーより新しい未宣言ログが下限に達しないと発火しない
    // （薄い材料で走らせない / #313 の実測: 20 件では抽象タグしか出なかった）。0 件もここで弾く。
    if window.total_remaining < cfg.min_new_logs.max(1) {
        return Ok(DeclareDecision::Skip("below_floor"));
    }
    // total_remaining >= 下限 >= 1 なのでマーカーより新しいログが必ず存在し、窓は非空
    // （from_id / to_id は Some）。防御的に None なら発火しない（clean 前進先が無いため）。
    if window.to_id.is_none() {
        return Ok(DeclareDecision::Skip("below_floor"));
    }

    // カーソルを置ける下限・上限（#394）。**窓と同じ時点で決める**（ターン中に増えた生ログに
    // 影響されないため）。id の差ではなく**生ログの件数**で測る（id は全エージェント共通の採番
    // なので、1 エージェントぶんの間隔は疎ら）。
    //
    // 窓が非空（`to_id` が Some）なのは上のゲートで確定しているので、両方とも必ず Some になる。
    // 防御的に None のときは窓の終端（＝従来の挙動）へ倒す。
    let window_end = window.to_id.unwrap_or(cursor_id);
    // 切り上げ除算（`i64::div_ceil` は unstable なので手で書く。log_count >= 0）。
    let min_advance = ((window.log_count + MIN_ADVANCE_DIVISOR - 1) / MIN_ADVANCE_DIVISOR).max(1);
    let min_position =
        opencrab_db::queries::nth_log_id_after(&conn, agent_id, cursor_id, min_advance)?
            .unwrap_or(window_end);
    let max_position = opencrab_db::queries::nth_log_id_after(
        &conn,
        agent_id,
        cursor_id,
        window.log_count.saturating_mul(MAX_ADVANCE_WINDOWS).max(1),
    )?
    .unwrap_or(window_end);

    // 地図（生ログ全体の分布 / day 粒度）。集計のみ＝本文は渡さない。
    let survey = opencrab_db::queries::survey_my_history(&conn, agent_id, "day", SURVEY_BUCKETS)?;

    // すでに宣言した記憶（新しい順）。本人の直近の宣言を思い出す手がかり。
    let mut recent_units = opencrab_db::queries::list_memory_units(&conn, agent_id)?;
    recent_units.truncate(RECENT_UNITS_SHOWN);

    // 人格（モデル解決は run_agent_response 側が effective_model で行うのでここでは不要）。
    let (persona_name, personality, instructions) =
        opencrab_db::queries::get_agent(&conn, agent_id)
            .ok()
            .flatten()
            .map(|a| (a.persona_name, a.personality, a.instructions))
            .unwrap_or_else(|| (agent_id.to_string(), None, String::new()));

    Ok(DeclareDecision::Run(Box::new(DeclarePlan {
        persona_name,
        personality,
        instructions,
        window,
        survey,
        recent_units,
        cursor_id,
        window_size,
        preferred_window_size,
        // 既定 = 未表明時に使う値。effective_window_size の None 枝と同式（#399）。
        default_window_size: cfg.max_logs.max(1),
        min_position,
        max_position,
    })))
}

/// system プロンプト（本人の人格 + 宣言の枠組み + 地図 + 今回の窓 + 既存宣言）を組む。
///
/// `build_agent_context`（`[Memory Index]` を注入する通常ターンの経路）は通さず、ここで
/// 自前に組む（宣言の結果を会話へ自動注入しないため / #316）。
///
/// **要約（本文）は渡さない。** 地図（集計）と窓の範囲だけ渡し、本文はエージェントが
/// `read_my_history` で自分で読む。要約を渡すと本人が読まない（読み取りツール 0 回）ことが
/// #313 の実測で分かっているため。ただし読むことは**強制しない**（読むか / どこで切るかは本人の判断）。
fn build_system_prompt(plan: &DeclarePlan) -> String {
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

    let survey_txt = render_survey(&plan.survey);
    let units_txt = if plan.recent_units.is_empty() {
        "(まだ宣言はありません。最初の宣言をあなたが決めます)".to_string()
    } else {
        plan.recent_units
            .iter()
            .map(format_unit_line)
            .collect::<Vec<_>>()
            .join("\n")
    };

    let w = &plan.window;
    let from = w.from_id.unwrap_or(plan.cursor_id);
    let to = w.to_id.unwrap_or(plan.cursor_id);
    let span = match (&w.date_from, &w.date_to) {
        (Some(f), Some(t)) => format!("{f} 〜 {t}"),
        _ => "(不明)".to_string(),
    };

    format!(
        "{personality_section}\
         これはあなたのスリープ（内省）の時間です。あなた自身の生ログ（あなたが実際に交わした\
         言葉）を見渡して、「ここからここまでが自分にとって一つの記憶だ」と思うまとまりを、\
         あなた自身が決めて宣言します。機械が刻んだ区切りに合わせる必要はありません。正解も\
         平均もありません。一期一会でよく、全部を宣言する必要もありません。\n\n\
         # やること\n\
         下の「今回の範囲」は、あなたがまだ記憶の単位を宣言していない生ログです。地図（下の\
         集計）を手がかりに、気になったところを `read_my_history` で読み、まとまりだと感じた\
         範囲を `record_memory_unit` で宣言してください。どこで切るか・いくつ宣言するか・そもそも\
         読むかどうかは、あなたが決めます。\n\n\
         # 使える道具\n\
         - `survey_my_history`: 生ログを日/時/週で俯瞰する（地図）。各バケットに est_tokens（概算トークン数）が付く\n\
         - `read_my_history`: 範囲を指定して生ログの中身を読む（session_id / id 範囲 / around / 時刻範囲）。取る前に estimate_only=true で大きさを測れる\n\
         - `search_my_history`: 生ログを全文検索する（関連する場面を探す）\n\
         - `record_memory_unit(from_id, to_id, title, summary?, tags?)`: 範囲を一つの記憶として宣言する\n\
         - `retract_memory_unit(unit_id)`: 宣言を取り消す\n\
         - `plan_next_memory_window(next_from_id?, window_size?, note?)`: 次回の範囲の始まりと広さを決める\n\
         メッセージ送信・シェル実行・サブタスク起動はしないでください。これは記憶を宣言する時間です。\
         生ログは読むだけで、消えも変わりもしません。宣言は何度でもやり直せます。\n\
         【サイズの約束】1 回のツール結果が inline_limit_tokens（約 2,500 トークン）を超えると本文は捨てられます。\
         地図の est_tokens や read_my_history の estimated_tokens を見て、大きい範囲は id 窓を狭めるか cursor_from_id で刻んで読んでください。\n\n\
         # あなたの記憶の地図（生ログ全体の分布・day 粒度）\n{survey_txt}\n\n\
         # 今回の範囲（未宣言 / id {from}〜{to} / {count} 件 / {span}）\n\
         この範囲の生ログには、まだあなたの記憶の単位が宣言されていません。ここを読んで、あなたに\
         とっての「一つの記憶」を宣言してください。セッションの切れ目・話題の切れ目・気持ちの切れ目、\
         どれを単位にするかはあなた次第です。（この範囲のセッション数の目安: {sessions}）\n\n\
         # 範囲の切り方もあなたが決められます\n\
         この「今回の範囲」は初期値にすぎません。`plan_next_memory_window` で次回に持ち越せます。\n\
         - **まだ続いている出来事**が範囲の途中から始まっているなら、そこで宣言せず\
         `plan_next_memory_window(next_from_id=その先頭の id)` を呼んでください。そこから先は\
         次回もう一度この範囲に現れます（呼ばなければ、宣言しなかった末尾は二度と現れません）。\n\
         - 範囲の**終わりを越えて**宣言したときも、`next_from_id` を宣言の続きの id にすれば、\
         次回が宣言済みと重なりません。\n\
         - 位置の指定は必ず前へ進むよう丸められます（今回は id {min_pos} 〜 {max_pos} の範囲に\
         収まります）。この「必ず進む」量は範囲の広さに比例するので、広くするほど、次回に回さず\
         その場で通り過ぎる件数も増えます。\n\
         - **範囲の広さ自体**も変えられます。いまの設定は {size} 件です{size_src}（未宣言の\
         生ログがそれより少ないときは、上の「今回の範囲」の件数はこれより少なくなります）。\
         材料が薄くて出来事が拾いきれないと感じたら `window_size` を大きく、濃すぎて丁寧に\
         見られないと感じたら小さくしてください（下限 {size_min} 件 / 上限は既定 {size_max} 件）。\
         一度決めると変えるまで効き続けます。ただし**既定より広げた設定のまま、ターンが途中で\
         潰れる（時間切れ・反復上限・エラー）ことが {reset_n} 回続いたら、既定の広さへ自動で\
         戻します**（そのときは自分で広げ直せます）。狭めた設定はそのままです。\n\
         どちらも義務ではありません。今のままで良ければ呼ばなくて構いません。\n\n\
         # すでに宣言した記憶（最近のもの）\n{units_txt}",
        count = w.log_count,
        sessions = w.session_count,
        min_pos = plan.min_position.saturating_add(1),
        max_pos = plan.max_position.saturating_add(1),
        size = plan.window_size,
        // 表明済みだと size は自分の値しか出ないので、既定を併記して「自分の設定が既定より
        // 広いか＝上の自動リセットが自分に掛かるか」を本人が判定できるようにする（#399）。
        // 未表明のときは size がそのまま既定なので併記しない（足す情報は最小に留める）。
        size_src = if plan.preferred_window_size.is_some() {
            format!("（あなたが決めた広さ／既定は {} 件）", plan.default_window_size)
        } else {
            "（既定の広さ）".to_string()
        },
        size_min = opencrab_actions::memory_units::DECLARE_WINDOW_MIN,
        size_max = opencrab_actions::memory_units::DECLARE_WINDOW_MAX,
        // 約束の文面を実装の定数から組む（片方だけ変えても食い違わない / #394）。
        reset_n = MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET,
    ) + &instructions_section
}

/// エンジンに渡す「ユーザーターン」。system 側に対象を明示済みなので、ここは着手の合図のみ。
fn build_task_message(plan: &DeclarePlan) -> String {
    let w = &plan.window;
    let from = w.from_id.unwrap_or(plan.cursor_id);
    let to = w.to_id.unwrap_or(plan.cursor_id);
    format!(
        "スリープの時間です。上の「今回の範囲」（id {from}〜{to} の未宣言ログ {count} 件）を見て、\
         あなたにとって一つの記憶だと感じるまとまりを宣言してください。読むか・どこで切るか・\
         いくつ宣言するかはあなたが決めます。終わったら、どういう視点でまとまりを見たかを\
         一言だけ残してください。",
        count = w.log_count,
    )
}

/// 地図（`HistorySurvey`）を集計テーブルとして描く（本文は含めない）。
fn render_survey(s: &HistorySurvey) -> String {
    let mut lines = vec![format!(
        "総ログ {total} 件 / 総セッション {sessions} / id {min}〜{max}{trunc}",
        total = s.total_logs,
        sessions = s.total_sessions,
        min = s
            .min_id
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into()),
        max = s
            .max_id
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into()),
        trunc = if s.truncated {
            format!(
                "（新しい {} バケットのみ表示 / 全 {} バケット）",
                s.returned_buckets, s.total_buckets
            )
        } else {
            String::new()
        },
    )];
    for b in &s.buckets {
        lines.push(format!(
            "- {bucket}: {logs} 件 / {sessions} セッション（id {min}〜{max} / 約 {est} トークン）",
            bucket = b.bucket,
            logs = b.log_count,
            sessions = b.session_count,
            min = b.min_id,
            max = b.max_id,
            est = b.est_tokens,
        ));
    }
    lines.join("\n")
}

/// 既存宣言の 1 行を `[短縮ID] タイトル（id from-to）` で描く。
fn format_unit_line(u: &IndexNodeRow) -> String {
    let id = u.short_id.as_deref().unwrap_or(&u.id);
    let from = u
        .start_log_id
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".into());
    let to = u
        .end_log_id
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".into());
    format!("- [{id}] {title}（id {from}-{to}）", title = u.title.trim())
}

/// マーカー `"{last_run_at}|{cursor_id}"` を組む。`last_run_at`（rfc3339）にも十進の
/// `cursor_id` にも `|` は現れないので、最初の `|` を区切りに使える。
fn format_marker(last_run_at: &str, cursor_id: i64) -> String {
    format!("{last_run_at}|{cursor_id}")
}

/// マーカーを `(last_run_at, cursor_id)` へ分解する。`None`（未実行）→ `(None, 0)`。
/// `|` が無ければ全体を `last_run_at` とみなし cursor は 0（後方互換）。パース不能な位置は 0。
fn parse_marker(marker: Option<&str>) -> (Option<String>, i64) {
    let Some(m) = marker else {
        return (None, 0);
    };
    match m.split_once('|') {
        Some((ts, id)) => {
            let ts = (!ts.is_empty()).then(|| ts.to_string());
            (ts, id.parse::<i64>().unwrap_or(0))
        }
        None => {
            let ts = (!m.is_empty()).then(|| m.to_string());
            (ts, 0)
        }
    }
}

#[cfg(test)]
#[path = "memory_declare/tests/mod.rs"]
mod tests;
