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
        size_src = if plan.preferred_window_size.is_some() {
            "（あなたが決めた広さ）"
        } else {
            "（既定の広さ）"
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
mod tests {
    use super::*;
    use opencrab_actions::Action;
    use opencrab_core::EngineResult;
    use opencrab_db::queries::DeclareWindowPref;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // --- セットアップ ---

    /// state の DB に生ログを 1 件入れて、その id を返す。
    fn seed_log(state: &AppState, agent_id: &str, session_id: &str, content: &str) -> i64 {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                log_type: "message".to_string(),
                content: content.to_string(),
                speaker_id: None,
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap()
    }

    /// `n` 件の生ログを 1 セッションに入れ、id のリストを返す。
    fn seed_logs(state: &AppState, agent_id: &str, session_id: &str, n: usize) -> Vec<i64> {
        (0..n)
            .map(|i| seed_log(state, agent_id, session_id, &format!("発話 {i}")))
            .collect()
    }

    fn get_marker(state: &AppState, agent_id: &str) -> Option<String> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_memory_declare_cursor(&conn, agent_id).unwrap()
    }

    fn set_marker(state: &AppState, agent_id: &str, cursor: &str) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_memory_declare_cursor(&conn, agent_id, cursor).unwrap();
    }

    fn cfg(enabled: bool, max_logs: i64, min_new: i64) -> MemoryDeclareConfig {
        MemoryDeclareConfig {
            enabled,
            max_logs,
            min_new_logs: min_new,
            min_interval_minutes: 1440,
            timeout_secs: 600,
        }
    }

    fn hours_ago(hours: i64) -> String {
        (Utc::now() - Duration::hours(hours)).to_rfc3339()
    }

    fn minutes_ago(minutes: i64) -> String {
        (Utc::now() - Duration::minutes(minutes)).to_rfc3339()
    }

    // --- マーカー parse/format ---

    #[test]
    fn marker_roundtrips_and_tolerates_missing_parts() {
        let m = format_marker("2026-08-05T00:00:00Z", 4242);
        assert_eq!(m, "2026-08-05T00:00:00Z|4242");
        assert_eq!(
            parse_marker(Some(&m)),
            (Some("2026-08-05T00:00:00Z".to_string()), 4242)
        );
        // 未実行（None）→ (None, 0)。
        assert_eq!(parse_marker(None), (None, 0));
        // `|` 無し（旧形式・素の刻時）→ 位置 0。
        assert_eq!(
            parse_marker(Some("2026-08-05T00:00:00Z")),
            (Some("2026-08-05T00:00:00Z".to_string()), 0)
        );
        // 位置がパース不能なら 0（壊れたマーカーで先頭からやり直す・落ちない）。
        assert_eq!(
            parse_marker(Some("2026-08-05T00:00:00Z|xxx")),
            (Some("2026-08-05T00:00:00Z".to_string()), 0)
        );
    }

    // --- ゲート判定（decide_declare）---

    #[test]
    fn first_run_fires_from_beginning() {
        let state = crate::test_app_state();
        let ids = seed_logs(&state, "a1", "s1", 5);
        // マーカー未設定（None）: throttle 無し・cursor=0 で先頭から窓を組む。
        let d = decide_declare(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        match d {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.cursor_id, 0, "初回は先頭（cursor=0）");
                assert_eq!(plan.window.log_count, 3, "窓は max_logs=3 で有界");
                assert_eq!(plan.window.from_id, Some(ids[0]), "窓は最古から");
                assert_eq!(plan.window.to_id, Some(ids[2]));
                assert_eq!(plan.window.total_remaining, 5, "未宣言の総数は 5");
                // clean 前進先は窓末尾（to_id）。
                assert_eq!(plan.window.to_id, Some(ids[2]));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn interval_gate_blocks_when_recent() {
        let state = crate::test_app_state();
        seed_logs(&state, "a1", "s1", 5);
        // 1h 前に走った（24h 未満）→ throttle。
        set_marker(&state, "a1", &format_marker(&hours_ago(1), 0));
        let d = decide_declare(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        assert!(matches!(d, DeclareDecision::Skip("interval_not_elapsed")));
    }

    /// 間隔ゲートは**分単位**（#390）。既定 1440 分は 24 時間ゲートのまま（現行挙動を維持）で、
    /// config で分を指定するとその間隔で発火する。0 は無効化ではなく 1 分に丸める。
    #[test]
    fn interval_gate_is_minutes_with_24h_default() {
        let state = crate::test_app_state();
        seed_logs(&state, "a1", "s1", 5);
        assert_eq!(
            MemoryDeclareConfig::default().min_interval_minutes,
            1440,
            "既定は 1440 分 = 24 時間（現行挙動）"
        );

        // 既定（1440 分）: 23h 前では通らない。
        let mut c = cfg(true, 3, 2);
        assert_eq!(c.min_interval_minutes, 1440);
        set_marker(&state, "a1", &format_marker(&minutes_ago(23 * 60), 0));
        assert!(matches!(
            decide_declare(&state.db, &c, "a1").unwrap(),
            DeclareDecision::Skip("interval_not_elapsed")
        ));

        // 10 分に詰めると、同じマーカーでも発火する。
        c.min_interval_minutes = 10;
        assert!(matches!(
            decide_declare(&state.db, &c, "a1").unwrap(),
            DeclareDecision::Run(_)
        ));
        // 5 分前 < 10 分 → まだ弾かれる（分の刻みが効いている）。
        set_marker(&state, "a1", &format_marker(&minutes_ago(5), 0));
        assert!(matches!(
            decide_declare(&state.db, &c, "a1").unwrap(),
            DeclareDecision::Skip("interval_not_elapsed")
        ));

        // 0 でもゲートは外れない（1 分に丸める）: 直前に走った直後は弾かれ、2 分後は通る。
        c.min_interval_minutes = 0;
        set_marker(
            &state,
            "a1",
            &format_marker(&(Utc::now() - Duration::seconds(10)).to_rfc3339(), 0),
        );
        assert!(matches!(
            decide_declare(&state.db, &c, "a1").unwrap(),
            DeclareDecision::Skip("interval_not_elapsed")
        ));
        set_marker(&state, "a1", &format_marker(&minutes_ago(2), 0));
        assert!(matches!(
            decide_declare(&state.db, &c, "a1").unwrap(),
            DeclareDecision::Run(_)
        ));
    }

    #[test]
    fn floor_gate_blocks_below_min() {
        let state = crate::test_app_state();
        seed_logs(&state, "a1", "s1", 3);
        // 間隔は通る（48h 前）。未宣言は 3 件で下限 5 未満 → skip。
        set_marker(&state, "a1", &format_marker(&hours_ago(48), 0));
        let d = decide_declare(&state.db, &cfg(true, 10, 5), "a1").unwrap();
        assert!(matches!(d, DeclareDecision::Skip("below_floor")));
    }

    #[test]
    fn empty_history_skips() {
        let state = crate::test_app_state();
        // ログ 0 件 → total_remaining 0 < 下限 → skip（発火しない）。
        let d = decide_declare(&state.db, &cfg(true, 10, 1), "a1").unwrap();
        assert!(matches!(d, DeclareDecision::Skip("below_floor")));
    }

    #[test]
    fn window_starts_after_cursor_and_carries_survey_and_units() {
        let state = crate::test_app_state();
        let ids = seed_logs(&state, "a1", "s1", 8);
        // cursor を 3 件目に置く（間隔は通る）。窓は 4 件目以降。
        set_marker(&state, "a1", &format_marker(&hours_ago(48), ids[2]));
        // 既存宣言を 1 つ作っておく（プロンプト材料に載る）。
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::record_memory_unit(
                &conn,
                "a1",
                "既存の宣言",
                "",
                ids[0],
                ids[1],
                None,
                None,
                "2026-08-01T00:00:00Z",
            )
            .unwrap();
        }
        let d = decide_declare(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        match d {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.cursor_id, ids[2]);
                assert_eq!(plan.window.from_id, Some(ids[3]), "cursor の次から");
                assert_eq!(plan.window.to_id, Some(ids[5]), "max_logs=3 で有界");
                assert_eq!(plan.window.total_remaining, 5, "cursor 以降の未宣言は 5");
                // 地図（集計）が載る。
                assert_eq!(plan.survey.total_logs, 8);
                // 既存宣言が載る（1 件）。
                assert_eq!(plan.recent_units.len(), 1);
                assert_eq!(plan.recent_units[0].title, "既存の宣言");
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    // --- プロンプト ---

    #[test]
    fn system_prompt_has_map_window_tools_but_no_log_bodies() {
        let state = crate::test_app_state();
        seed_logs(&state, "a1", "s1", 5);
        let plan = match decide_declare(&state.db, &cfg(true, 3, 2), "a1").unwrap() {
            DeclareDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let sp = build_system_prompt(&plan);
        // 地図（集計）が載る。
        assert!(sp.contains("あなたの記憶の地図"));
        assert!(sp.contains("総ログ 5 件"));
        // 今回の窓が載る。
        assert!(sp.contains("今回の範囲"));
        // 宣言の道具名が載る。
        assert!(sp.contains("record_memory_unit"));
        assert!(sp.contains("read_my_history"));
        // 生ログ本文（"発話 N"）は**渡さない**（要約を渡すと読まない / #313）。
        assert!(!sp.contains("発話 0"), "生ログ本文がプロンプトに漏れている");
        assert!(!sp.contains("発話 4"), "生ログ本文がプロンプトに漏れている");
    }

    // --- 本番のラン構築を通さない全経路テスト（#370 の構造を共有）---

    enum FakeOutcome {
        Completed,
        StoppedByLimit,
        Error,
    }

    struct FakeRunner {
        outcome: FakeOutcome,
        calls: AtomicUsize,
        captured: std::sync::Mutex<Option<CapturedReq>>,
        /// ターン中に本人が `plan_next_memory_window` を呼んだ状況を模す（#394）。
        /// 道具は DB へ書くだけなので、ここで同じ列へ書けば本番と同じ経路を通る。
        writes_pref: Option<(opencrab_db::Db, DeclareWindowPref)>,
    }

    struct CapturedReq {
        gateway: String,
        caller_is_owner: bool,
        tool_allowlist: Option<Vec<String>>,
        has_gateway_actions: bool,
        persist_turn_logs: bool,
    }

    impl FakeRunner {
        fn new(outcome: FakeOutcome) -> Self {
            Self {
                outcome,
                calls: AtomicUsize::new(0),
                captured: std::sync::Mutex::new(None),
                writes_pref: None,
            }
        }

        /// ターン中に本人が窓の希望を表明する版（#394）。
        fn with_pref(mut self, state: &AppState, pref: DeclareWindowPref) -> Self {
            self.writes_pref = Some((state.db.clone(), pref));
            self
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
            if let Some((db, pref)) = &self.writes_pref {
                let conn = db.lock().unwrap();
                opencrab_db::queries::set_memory_declare_window(&conn, "a1", Some(pref)).unwrap();
            }
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
            dispatched_subtasks: 0,
        }
    }

    /// ゲートが通る状態に DB を整える（間隔 OK / 下限以上のログ）。to_id を返す。
    fn seed_passing_gate(state: &AppState) -> i64 {
        let ids = seed_logs(state, "a1", "s1", 5);
        set_marker(state, "a1", &format_marker(&hours_ago(48), 0));
        *ids.last().unwrap()
    }

    fn latest_sleep_audit(state: &AppState) -> Option<serde_json::Value> {
        let conn = state.db.lock().unwrap();
        let rows = opencrab_db::queries::list_agent_logs(&conn, Some("a1"), None, 10).ok()?;
        rows.into_iter()
            .filter(|r| r.context == "sleep")
            .find_map(|r| {
                serde_json::from_str::<serde_json::Value>(&r.message)
                    .ok()
                    .filter(|v| v["kind"] == "memory_declare")
            })
    }

    #[tokio::test]
    async fn default_off_is_zero_call_and_writes_nothing() {
        let state = crate::test_app_state();
        seed_passing_gate(&state);
        // 既定オフ用にマーカーを消す（seed_passing_gate が立てるので上書きで None にはできない;
        // 代わりに enabled=false で decide に入らないことを確認する）。
        let fake = FakeRunner::new(FakeOutcome::Completed);
        let before = get_marker(&state, "a1");
        let ran = run_declare(
            &state.db,
            &cfg(false, 3, 2),
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
    async fn clean_run_advances_marker() {
        let state = crate::test_app_state();
        let to_id = seed_passing_gate(&state);
        let fake = FakeRunner::new(FakeOutcome::Completed);
        let ran = run_declare(
            &state.db,
            &cfg(true, 3, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(ran);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1, "ターンは 1 回");
        // clean → 位置が提示窓の末尾（3 件目）へ進む。max_logs=3 なので to は ids[2]。
        let marker = get_marker(&state, "a1").expect("マーカーが立つ");
        let (_, cursor) = parse_marker(Some(&marker));
        assert_eq!(cursor, to_id - 2, "窓末尾（3 件目）へ前進");
        let audit = latest_sleep_audit(&state).expect("監査ログ");
        assert_eq!(audit["outcome"], "completed");
        assert_eq!(audit["position_advanced"], true);
        assert_eq!(audit["throttle_advanced"], true);
    }

    /// partial（ターン上限）: **位置は据え置き・throttle は now へ前進**。その結果、次 tick は
    /// 日次ゲートで弾かれ、同じ窓で再発火しない（#366 と同型の暴走防止 / 無人の連続失敗を止める）。
    #[tokio::test]
    async fn partial_holds_position_but_advances_throttle() {
        let state = crate::test_app_state();
        seed_passing_gate(&state); // marker = "{48h前}|0"
        let (before_run, before_pos) = parse_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(before_pos, 0);
        let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);
        let ran = run_declare(
            &state.db,
            &cfg(true, 3, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(ran, "起動はした（partial でも true）");
        let (after_run, after_pos) = parse_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(after_pos, 0, "partial では位置を進めない（据え置き）");
        // throttle は now へ進んだ（48h 前より新しい）。
        let before_dt = before_run.unwrap().parse::<DateTime<Utc>>().unwrap();
        let after_dt = after_run.unwrap().parse::<DateTime<Utc>>().unwrap();
        assert!(after_dt > before_dt, "partial でも throttle は now へ進む");
        let audit = latest_sleep_audit(&state).expect("監査ログ");
        assert_eq!(audit["outcome"], "stopped_by_limit");
        assert_eq!(audit["position_advanced"], false);
        assert_eq!(audit["throttle_advanced"], true);
        // 次 tick は日次ゲートで弾かれる（10 分後の再発火を止める）。
        let d = decide_declare(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        assert!(
            matches!(d, DeclareDecision::Skip("interval_not_elapsed")),
            "partial 直後は throttle で弾かれ再発火しない"
        );
    }

    /// error（run 自体の失敗）も partial と同じ: 位置据え置き・throttle 前進・次 tick はゲート。
    #[tokio::test]
    async fn error_holds_position_but_advances_throttle() {
        let state = crate::test_app_state();
        seed_passing_gate(&state);
        let (before_run, _) = parse_marker(get_marker(&state, "a1").as_deref());
        let fake = FakeRunner::new(FakeOutcome::Error);
        run_declare(
            &state.db,
            &cfg(true, 3, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        let (after_run, after_pos) = parse_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(after_pos, 0, "error では位置を進めない");
        let before_dt = before_run.unwrap().parse::<DateTime<Utc>>().unwrap();
        let after_dt = after_run.unwrap().parse::<DateTime<Utc>>().unwrap();
        assert!(after_dt > before_dt, "error でも throttle は now へ進む");
        let audit = latest_sleep_audit(&state).expect("監査ログ");
        assert_eq!(audit["outcome"], "error");
        assert_eq!(audit["position_advanced"], false);
        // 次 tick はゲートで弾かれる。
        let d = decide_declare(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        assert!(matches!(d, DeclareDecision::Skip("interval_not_elapsed")));
    }

    /// マーカーが前進し、次回は次の窓を提示する（提示済みを二度出さない = 無限ループしない）。
    #[tokio::test]
    async fn marker_progresses_across_runs_without_repeat() {
        let state = crate::test_app_state();
        let ids = seed_logs(&state, "a1", "s1", 6);
        set_marker(&state, "a1", &format_marker(&hours_ago(48), 0));

        // run1: 先頭 3 件（ids[0..3]）を提示 → clean で cursor=ids[2]。
        let fake1 = FakeRunner::new(FakeOutcome::Completed);
        run_declare(
            &state.db,
            &cfg(true, 3, 1),
            &state.index_build_inflight,
            "a1",
            &fake1,
        )
        .await
        .unwrap();
        let (_, c1) = parse_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(c1, ids[2]);

        // 翌日を模す（throttle を開ける）。位置はそのまま。
        set_marker(&state, "a1", &format_marker(&hours_ago(48), ids[2]));

        // run2: 次の窓（ids[3..6]）を提示することを decide で確認。
        let d = decide_declare(&state.db, &cfg(true, 3, 1), "a1").unwrap();
        match d {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.window.from_id, Some(ids[3]), "提示済みを跨いで次から");
                assert_eq!(plan.window.to_id, Some(ids[5]));
                assert_eq!(plan.window.total_remaining, 3);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 口に渡る `RunRequest` が本番配線を保つ: gateway="sleep" / caller=Owner /
    /// ツール許可リスト=DECLARE_ALLOWED_TOOLS / 送信経路（gateway_actions）なし。
    #[tokio::test]
    async fn run_request_carries_expected_wiring() {
        let state = crate::test_app_state();
        seed_passing_gate(&state);
        let fake = FakeRunner::new(FakeOutcome::Completed);
        run_declare(
            &state.db,
            &cfg(true, 3, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        let captured = fake.captured.lock().unwrap();
        let req = captured.as_ref().expect("ターンが回れば記録される");
        assert_eq!(req.gateway, "sleep");
        assert!(req.caller_is_owner, "caller は Owner");
        assert!(
            !req.has_gateway_actions,
            "送信経路（会話への出口）は渡さない"
        );
        let expected: Vec<String> = DECLARE_ALLOWED_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(req.tool_allowlist.as_ref(), Some(&expected));
        // #393: 整備作業のターンは生ログに残さない（残すと次の宣言ランの材料になる）。
        assert!(
            !req.persist_turn_logs,
            "宣言ランのターンは memory_sessions に記録しない"
        );
    }

    /// 許可リストの内容（経路1）: 宣言に要る道具は入り、外向き・タグ整理の道具は入らない。
    #[test]
    fn declare_allowlist_includes_record_excludes_outward_and_tag_tools() {
        let allowed = [
            "survey_my_history",
            "read_my_history",
            "search_my_history",
            "record_memory_unit",
            "retract_memory_unit",
            "declare_done",
        ];
        for a in allowed {
            assert!(
                DECLARE_ALLOWED_TOOLS.contains(&a),
                "宣言に必要な {a} が許可リストに無い"
            );
        }
        // 外向き・状態書き換え・タグ整理（別ラン）の道具は入っていない。
        let forbidden = [
            "execute_shell",
            "nostr_run",
            "spawn_subtask",
            "ws_write",
            "ws_delete",
            "configure_self",
            "update_instructions",
            "tag_topic",
            "untag_topic",
            "merge_tags",
            "browse_memory_index",
            "search_memory_index",
            "retrieve_memory_nodes",
        ];
        for f in forbidden {
            assert!(
                !DECLARE_ALLOWED_TOOLS.contains(&f),
                "許可リストに入ってはならない {f} が入っている"
            );
        }
    }

    /// 回帰（#379/#383 の構造的分離を段階2 でも固定）: 宣言ユニット（node_type='unit'）は
    /// タグ整理ランの worklist（node_type='topic' / source_type='session_log' を pin）に混ざらない。
    #[test]
    fn declared_units_do_not_mix_into_tag_worklist() {
        let state = crate::test_app_state();
        let ids = seed_logs(&state, "a1", "s1", 4);
        let conn = state.db.lock().unwrap();
        // 生ログ由来の topic を 2 件（タグ整理ランの worklist 対象）。
        for (i, end) in [ids[1], ids[3]].into_iter().enumerate() {
            opencrab_db::queries::insert_index_node(
                &conn,
                &IndexNodeRow {
                    id: format!("topic-{i}"),
                    agent_id: "a1".to_string(),
                    parent_id: None,
                    node_type: "topic".to_string(),
                    source_type: "session_log".to_string(),
                    title: format!("topic {i}"),
                    summary: "s".to_string(),
                    start_log_id: None,
                    end_log_id: Some(end),
                    source_session_id: None,
                    date_from: None,
                    date_to: None,
                    depth: 3,
                    child_count: 0,
                    token_count: 0,
                    created_at: "2026-08-01T00:00:00Z".to_string(),
                    updated_at: "2026-08-01T00:00:00Z".to_string(),
                    short_id: Some(format!("t{i}")),
                    keywords_json: "[]".to_string(),
                    summary_refreshed_at: None,
                },
            )
            .unwrap();
        }
        // 宣言ユニットを 1 件（node_type='unit'）。
        opencrab_db::queries::record_memory_unit(
            &conn,
            "a1",
            "宣言",
            "",
            ids[0],
            ids[3],
            None,
            None,
            "2026-08-02T00:00:00Z",
        )
        .unwrap();

        // タグ整理ランの発火下限クエリは topic だけを数える（unit は混ざらない）。
        let cursor = Some(("1970-01-01T00:00:00Z", ""));
        let n =
            opencrab_db::queries::count_organize_topics(&conn, "a1", cursor, 1_000_000).unwrap();
        assert_eq!(
            n, 2,
            "worklist に宣言ユニットが混ざった（topic 2 件のはず）"
        );
        let worklist =
            opencrab_db::queries::list_organize_topics(&conn, "a1", cursor, 1_000_000, 50).unwrap();
        assert!(
            worklist.iter().all(|t| t.node_type == "topic"),
            "worklist に node_type='unit' が現れた"
        );
        // 一方 list_memory_units には宣言ユニットが 1 件だけ出る。
        let units = opencrab_db::queries::list_memory_units(&conn, "a1").unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].node_type, "unit");
    }

    // ---- #394: 窓の境界と広さを本人が決める（前進の保証つき）----

    fn cursor_of(state: &AppState) -> i64 {
        parse_marker(get_marker(state, "a1").as_deref()).1
    }

    fn get_pref(state: &AppState) -> Option<DeclareWindowPref> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_memory_declare_window(&conn, "a1").unwrap()
    }

    /// 位置の希望だけを持つ `DeclareWindowPref`。
    fn want_next_from(id: i64) -> DeclareWindowPref {
        DeclareWindowPref {
            next_from_id: Some(id),
            ..Default::default()
        }
    }

    /// 宣言ランの中で道具を呼ぶときと同じ `ActionContext`（caller=Owner / gateway="sleep" /
    /// 同じ DB）。窓の道具は DB しか触らないので、これで本番と同じ経路を通せる。
    fn declare_tool_ctx(state: &AppState) -> (tempfile::TempDir, opencrab_actions::ActionContext) {
        let dir = tempfile::TempDir::new().unwrap();
        let workspace = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = opencrab_actions::ActionContext {
            caller: CallerIdentity::Owner,
            agent_id: "a1".to_string(),
            agent_name: "a1".to_string(),
            session_id: Some("sleep-declare-a1-1".to_string()),
            db: state.db.clone(),
            workspace: std::sync::Arc::new(workspace),
            last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: std::sync::Arc::new(std::sync::Mutex::new(
                opencrab_actions::RuntimeInfo {
                    default_model: "mock:test".to_string(),
                    active_model: None,
                    available_providers: vec!["mock".to_string()],
                    gateway: "sleep".to_string(),
                },
            )),
        };
        (dir, ctx)
    }

    /// throttle だけ開けて（位置はそのまま）もう 1 ラン回す。翌日 / 次の tick を模す。
    async fn run_again(state: &AppState, c: &MemoryDeclareConfig, fake: &FakeRunner) {
        set_marker(
            state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(state)),
        );
        run_declare(&state.db, c, &state.index_build_inflight, "a1", fake)
            .await
            .unwrap();
    }

    /// ゲートが通る状態に `n` 件の生ログを積む（cursor=0 / 間隔 OK）。id を返す。
    fn seed_window(state: &AppState, n: usize) -> Vec<i64> {
        let ids = seed_logs(state, "a1", "s1", n);
        set_marker(state, "a1", &format_marker(&hours_ago(48), 0));
        ids
    }

    /// **前進の保証(1)**: 何も表明しない（宣言ゼロ相当）ラン。従来どおり窓の終端へ進む。
    #[tokio::test]
    async fn no_request_advances_to_window_end() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 9);
        let fake = FakeRunner::new(FakeOutcome::Completed);
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(cursor_of(&state), ids[8], "希望が無ければ窓の終端へ");
    }

    /// **前進の保証(2)**: 本人が現在位置以下（＝巻き戻し）を指定しても、必ず前へ進む。
    /// これを落とすと同じ窓を永久に再取得するループに入る（#374）。
    #[tokio::test]
    async fn request_at_or_below_cursor_still_advances() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 9);
        // cursor は 0。「次は id 1 から」＝ 1 件も進めない要求。
        let fake =
            FakeRunner::new(FakeOutcome::Completed).with_pref(&state, want_next_from(ids[0]));
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        // 下限（提示窓 9 件の 1/3 = 3 件目）まで引き上げられる。
        assert_eq!(cursor_of(&state), ids[2], "下限まで必ず前進する");
        assert!(cursor_of(&state) > 0, "前進していない（無限ループの入口）");

        // 次のランは進んだ先から始まる（同じ窓を再取得しない）。
        set_marker(
            &state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(&state)),
        );
        match decide_declare(&state.db, &cfg(true, 9, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(plan.window.from_id, Some(ids[3])),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// **前進の保証(3)**: 0 や負の指定（モデルが空値で埋めた形）でも下限まで進む。
    #[tokio::test]
    async fn nonsense_request_still_advances() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 9);
        let fake = FakeRunner::new(FakeOutcome::Completed).with_pref(
            &state,
            DeclareWindowPref {
                next_from_id: Some(-42),
                ..Default::default()
            },
        );
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(cursor_of(&state), ids[2], "壊れた指定でも下限まで前進する");
    }

    /// 本来の用途: **続いている出来事の末尾を次回へ回す**。窓の途中を指せばそこから次回に現れる。
    #[tokio::test]
    async fn request_inside_window_rolls_the_tail_over() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 9);
        // 「id ids[5] から先はまだ続いているので次回に回したい」
        let fake =
            FakeRunner::new(FakeOutcome::Completed).with_pref(&state, want_next_from(ids[5]));
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(cursor_of(&state), ids[4], "指した id の 1 つ手前で止まる");

        // 翌日: 回した末尾（ids[5..]）がちゃんともう一度現れる（従来は二度と現れなかった）。
        set_marker(
            &state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(&state)),
        );
        match decide_declare(&state.db, &cfg(true, 9, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.window.from_id, Some(ids[5]), "末尾が次の窓に戻る");
                assert_eq!(plan.window.to_id, Some(ids[8]));
            }
            other => panic!("expected Run, got {other:?}"),
        }
        let audit = latest_sleep_audit(&state).expect("監査ログ");
        assert_eq!(audit["requested_next_from_id"], json!(ids[5]));
        assert_eq!(audit["position"], json!(ids[4]));
    }

    /// **上限**: 窓の終端を大きく越える指定でも、2 窓ぶんより先へは飛ばない（未読を丸ごと
    /// 飛ばして二度と窓に入らないのを防ぐ）。
    #[tokio::test]
    async fn request_far_beyond_window_is_capped() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 40);
        let fake =
            FakeRunner::new(FakeOutcome::Completed).with_pref(&state, want_next_from(999_999));
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        // 窓は 9 件。上限は 2 窓ぶん = 18 件目。
        assert_eq!(cursor_of(&state), ids[17], "上限（2 窓ぶん）で止まる");
        // 残りは失われていない。
        set_marker(
            &state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(&state)),
        );
        match decide_declare(&state.db, &cfg(true, 9, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(plan.window.total_remaining, 22),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 窓の終端を**少しだけ**越えた指定（越境して宣言した続きから）はそのまま通る。
    #[tokio::test]
    async fn request_just_past_window_end_is_honored() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 20);
        // 窓は 9 件（ids[0..9]）。本人は ids[10] まで宣言したので「次は ids[11] から」。
        let fake =
            FakeRunner::new(FakeOutcome::Completed).with_pref(&state, want_next_from(ids[11]));
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(cursor_of(&state), ids[10], "越境した宣言のぶんは重複しない");
    }

    /// **partial では据え置き**（既存の挙動を壊さない）。位置の希望はランで使い切って消える。
    #[tokio::test]
    async fn partial_holds_position_even_with_request_and_consumes_it() {
        let state = crate::test_app_state();
        seed_window(&state, 9);
        let fake = FakeRunner::new(FakeOutcome::StoppedByLimit).with_pref(
            &state,
            DeclareWindowPref {
                next_from_id: Some(5),
                window_size: Some(120),
                ..Default::default()
            },
        );
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(cursor_of(&state), 0, "partial では位置を進めない");
        let pref = get_pref(&state).expect("希望の行は残る");
        assert_eq!(pref.next_from_id, None, "位置の希望はランで使い切る");
        assert_eq!(pref.window_size, Some(120), "広さは残る（sticky）");
    }

    /// 位置の希望は clean でも使い切る（過去の指定が後のランを引き戻し続けない）。
    #[tokio::test]
    async fn position_request_is_consumed_after_clean_run() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 9);
        let fake =
            FakeRunner::new(FakeOutcome::Completed).with_pref(&state, want_next_from(ids[5]));
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        // 位置しか書かれていなかったので、使い切ると希望そのものが空になる（列は NULL へ戻る）。
        assert_eq!(get_pref(&state).and_then(|p| p.next_from_id), None);
        assert_eq!(get_pref(&state), None, "空になった希望は NULL へ戻す");

        // 2 回目（希望なし）は窓の終端まで進む＝古い指定が生き残っていない。
        set_marker(
            &state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(&state)),
        );
        let fake2 = FakeRunner::new(FakeOutcome::Completed);
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake2,
        )
        .await
        .unwrap();
        assert_eq!(cursor_of(&state), ids[8]);
    }

    /// `note` は位置と**同じ寿命**で消費される。残すと、以後すべてのランの監査 `window_note` に
    /// 同じ文字列が出続け、「このランで本人がこう書いた」と誤読される。
    #[tokio::test]
    async fn note_is_consumed_together_with_the_position() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 20);
        let fake = FakeRunner::new(FakeOutcome::Completed).with_pref(
            &state,
            DeclareWindowPref {
                next_from_id: Some(ids[5]),
                note: Some("この出来事はまだ続いている".to_string()),
                ..Default::default()
            },
        );
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        // 書かれたランの監査には出る。
        assert_eq!(
            latest_sleep_audit(&state).unwrap()["window_note"],
            json!("この出来事はまだ続いている")
        );
        assert_eq!(
            get_pref(&state).and_then(|p| p.note),
            None,
            "note は残らない"
        );

        // 次のラン（本人は何も書いていない）の監査には出ない。
        let fake2 = FakeRunner::new(FakeOutcome::Completed);
        run_again(&state, &cfg(true, 9, 1), &fake2).await;
        assert_eq!(
            latest_sleep_audit(&state).unwrap()["window_note"],
            json!(null),
            "過去のランの note が後のランの監査に出続けている"
        );
    }

    /// **自力での回復**: 本人が広げた結果ターンが毎回潰れると、位置が 1 件も進まないまま発火し
    /// 続ける（ターンが潰れる状況では本人が道具を呼ぶ余地も無い）。partial が
    /// [`MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET`] 回連続したら、広さの希望を捨てて config の
    /// 既定へ戻す。**N-1 回では戻らない**（一時的な遅延・失敗で本人の設定を消さない）。
    #[tokio::test]
    async fn consecutive_partials_reset_preferred_window_size() {
        assert_eq!(MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET, 3, "以下は N=3 前提");
        let state = crate::test_app_state();
        seed_window(&state, 400);
        // config の既定は 100。本人はそれより**広い** 300 を表明する（＝安全弁の対象）。
        let c = cfg(true, 100, 1);

        // 本人が道具で広さを表明する（実際に通る経路）。
        let (_dir, ctx) = declare_tool_ctx(&state);
        let r = opencrab_actions::memory_units::PlanNextMemoryWindowAction
            .execute(&json!({"window_size": 300}), &ctx)
            .await;
        assert!(r.success, "{:?}", r.error);

        // 1 回目・2 回目の partial では戻さない（連続を数えるだけ）。
        for expected_streak in 1..MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET {
            let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);
            run_again(&state, &c, &fake).await;
            let pref = get_pref(&state).expect("希望は残る");
            assert_eq!(
                pref.window_size,
                Some(300),
                "{expected_streak} 回目の partial で本人の設定が消えた"
            );
            assert_eq!(pref.partial_streak, Some(expected_streak));
            let audit = latest_sleep_audit(&state).unwrap();
            assert_eq!(audit["partial_streak"], json!(expected_streak));
            assert_eq!(audit["window_size_auto_reset"], json!(false));
        }

        // N 回目で既定へ戻す。
        let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);
        run_again(&state, &c, &fake).await;
        assert_eq!(
            get_pref(&state).and_then(|p| p.window_size),
            None,
            "N 回連続の partial でも本人の広さが残っている（自力で回復できない）"
        );
        assert_eq!(get_pref(&state).and_then(|p| p.partial_streak), None);
        let audit = latest_sleep_audit(&state).unwrap();
        assert_eq!(
            audit["window_size_auto_reset"],
            json!(true),
            "自動で戻したことが監査から分からない"
        );

        // 次のランは config の既定の広さで走る（throttle だけ開ける）。
        set_marker(
            &state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(&state)),
        );
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.window_size, 100, "config の既定へ戻っていない");
                assert_eq!(plan.preferred_window_size, None);
            }
            other => panic!("expected Run, got {other:?}"),
        }

        // 恒久的な禁止ではない: 本人が呼べばまた広げられる。
        let r = opencrab_actions::memory_units::PlanNextMemoryWindowAction
            .execute(&json!({"window_size": 250}), &ctx)
            .await;
        assert!(r.success);
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(plan.window_size, 250),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// **狭める方向の表明は巻き添えにしない**（#394 のオーナー要件「密に拾う個性 → 濃い範囲では
    /// 窓を縮めて丁寧に見たい」）。
    ///
    /// 既定より狭い設定は partial の原因になり得ない（timeout / ターン上限は広い窓の側で起きる）。
    /// ここで破棄すると、窓は既定へ**広がって**状況を悪化させる方向へ動く。`clean` は
    /// `completed` だけが真で LLM 側の一時障害も 1 回として数えるので、消化中
    /// （`min_interval_minutes = 1`）はプロバイダの不調だけで連続が伸びる——現実に踏む。
    #[tokio::test]
    async fn narrower_than_default_preference_is_never_auto_reset() {
        let state = crate::test_app_state();
        seed_window(&state, 400);
        let c = cfg(true, 100, 1); // 既定 100 に対して本人は 60（狭める方向）

        let (_dir, ctx) = declare_tool_ctx(&state);
        let r = opencrab_actions::memory_units::PlanNextMemoryWindowAction
            .execute(&json!({"window_size": 60}), &ctx)
            .await;
        assert!(r.success, "{:?}", r.error);

        // N を超えて partial が続いても破棄しない。連続も数えない。
        for _ in 0..MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET + 2 {
            let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);
            run_again(&state, &c, &fake).await;
            let pref = get_pref(&state).expect("希望は残る");
            assert_eq!(
                pref.window_size,
                Some(60),
                "狭める方向の設定を機械が取り上げた（窓が既定へ広がってしまう）"
            );
            assert_eq!(pref.partial_streak, None, "対象外なのに連続を数えている");
            assert_eq!(
                latest_sleep_audit(&state).unwrap()["window_size_auto_reset"],
                json!(false)
            );
        }
        // 次のランも本人の 60 のまま。
        set_marker(
            &state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(&state)),
        );
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(plan.window_size, 60),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// **本人の自己修正は巻き添えにしない**。希望はターンの**後**に読むので、連続が N-1 まで
    /// 来た状態で本人がターン中に「広すぎたので狭くする」と表明し、そのターンも partial に
    /// 落ちても、**いま書いたばかりの狭い値ごと**破棄されてはいけない。
    #[tokio::test]
    async fn self_correction_during_the_turn_is_not_swept_away() {
        let state = crate::test_app_state();
        seed_window(&state, 400);
        let c = cfg(true, 100, 1);
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::set_memory_declare_window(
                &conn,
                "a1",
                Some(&DeclareWindowPref {
                    window_size: Some(300),
                    // 既に N-1 回連続している状態から始める。
                    partial_streak: Some(MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET - 1),
                    ..Default::default()
                }),
            )
            .unwrap();
        }

        // このターンの中で本人が既定以下へ狭め、ターン自体は partial に終わる。
        let fake = FakeRunner::new(FakeOutcome::StoppedByLimit).with_pref(
            &state,
            DeclareWindowPref {
                window_size: Some(80),
                partial_streak: Some(MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET - 1),
                ..Default::default()
            },
        );
        run_again(&state, &c, &fake).await;

        let pref = get_pref(&state).expect("希望は残る");
        assert_eq!(
            pref.window_size,
            Some(80),
            "本人が書いたばかりの狭い値が N 回目として破棄された"
        );
        assert_eq!(
            pref.partial_streak, None,
            "既定以下になったので連続は切れる"
        );
        assert_eq!(
            latest_sleep_audit(&state).unwrap()["window_size_auto_reset"],
            json!(false)
        );
    }

    /// clean が 1 回通れば連続は切れる（間に成功が挟まれば本人の設定は消えない）。
    #[tokio::test]
    async fn clean_run_breaks_the_partial_streak() {
        let state = crate::test_app_state();
        seed_window(&state, 800);
        // 既定 100 より広い 300（＝安全弁の対象）でないと、そもそも連続を数えない。
        let c = cfg(true, 100, 1);
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::set_memory_declare_window(
                &conn,
                "a1",
                Some(&DeclareWindowPref {
                    window_size: Some(300),
                    ..Default::default()
                }),
            )
            .unwrap();
        }

        // partial × (N-1) → clean → partial × (N-1)。どこにも N 連続は無い。
        for outcome in [
            FakeOutcome::StoppedByLimit,
            FakeOutcome::Error,
            FakeOutcome::Completed,
            FakeOutcome::StoppedByLimit,
            FakeOutcome::Error,
        ] {
            let fake = FakeRunner::new(outcome);
            run_again(&state, &c, &fake).await;
        }
        assert_eq!(
            get_pref(&state).and_then(|p| p.window_size),
            Some(300),
            "clean を挟んでいるのに本人の設定が消えた"
        );
        assert_eq!(
            get_pref(&state).and_then(|p| p.partial_streak),
            Some(2),
            "clean 後の連続だけが数えられているはず"
        );
    }

    /// 広さを表明していないエージェントでは連続を数えない（戻す先が無い＝仕事が無い）。
    /// 希望の行を作らないので、道具を一度も使っていない DB は NULL のまま。
    #[tokio::test]
    async fn partials_without_a_preference_do_not_create_state() {
        let state = crate::test_app_state();
        seed_window(&state, 200);
        let c = cfg(true, 100, 1);
        for _ in 0..MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET + 1 {
            let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);
            run_again(&state, &c, &fake).await;
        }
        assert_eq!(get_pref(&state), None, "希望なしの行を作ってはいけない");
        assert_eq!(
            latest_sleep_audit(&state).unwrap()["partial_streak"],
            json!(0)
        );
    }

    /// **窓の広さ**: 本人の表明が次の窓に効き、上下限へ丸められる。表明が無ければ config の既定
    /// のまま（既定値は変えない）。
    #[test]
    fn preferred_window_size_resizes_next_window_within_bounds() {
        let state = crate::test_app_state();
        seed_window(&state, 200);
        let c = cfg(true, 100, 1);

        // 表明なし: config の既定（100）。既定が下限 50 を下回る設定でも丸めない。
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.window_size, 100);
                assert_eq!(plan.window.log_count, 100);
                assert_eq!(plan.preferred_window_size, None);
            }
            other => panic!("expected Run, got {other:?}"),
        }
        match decide_declare(&state.db, &cfg(true, 20, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(
                plan.window_size, 20,
                "表明が無ければ config の既定をそのまま使う（既定値を変えない）"
            ),
            other => panic!("expected Run, got {other:?}"),
        }

        // 広げる（薄かったので次はもっと広く）。
        let set = |size: i64| {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::set_memory_declare_window(
                &conn,
                "a1",
                Some(&DeclareWindowPref {
                    window_size: Some(size),
                    ..Default::default()
                }),
            )
            .unwrap();
        };
        set(150);
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.window_size, 150);
                assert_eq!(plan.window.log_count, 150, "実際の窓が広がる");
                assert_eq!(plan.preferred_window_size, Some(150));
            }
            other => panic!("expected Run, got {other:?}"),
        }

        // 狭める。
        set(60);
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(plan.window.log_count, 60),
            other => panic!("expected Run, got {other:?}"),
        }

        // 上限・下限で丸める（プロンプトが肥大しない / 前進が止まらない）。
        set(100_000);
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(
                plan.window_size,
                opencrab_actions::memory_units::DECLARE_WINDOW_MAX
            ),
            other => panic!("expected Run, got {other:?}"),
        }
        set(1);
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(
                plan.window_size,
                opencrab_actions::memory_units::DECLARE_WINDOW_MIN
            ),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 運用が `max_logs` を上限より大きく設定している場合、本人の表明でそれより狭められることは
    /// あっても、上限のせいで運用の設定より狭くなることは無い。
    ///
    /// **道具（`plan_next_memory_window`）経由で**表明する——本人が実際に通る経路。DB を直接
    /// 叩くと、道具が上限で丸めてしまう実装でもこのテストは通ってしまい、doc の約束
    /// （`DECLARE_WINDOW_MAX` の doc）との食い違いを検出できない。
    #[tokio::test]
    async fn preferred_window_size_ceiling_never_undercuts_config_via_tool() {
        let state = crate::test_app_state();
        seed_window(&state, 60);
        let big = opencrab_actions::memory_units::DECLARE_WINDOW_MAX + 400;

        let (_dir, ctx) = declare_tool_ctx(&state);
        let r = opencrab_actions::memory_units::PlanNextMemoryWindowAction
            .execute(&json!({"window_size": big}), &ctx)
            .await;
        assert!(r.success, "{:?}", r.error);

        // 運用が上限より広い枠を既定にしている: 本人が同じ値を表明しても窓は狭まらない。
        match decide_declare(&state.db, &cfg(true, big, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(
                plan.window_size, big,
                "本人が表明した瞬間に窓が運用の既定より狭まってはいけない"
            ),
            other => panic!("expected Run, got {other:?}"),
        }
        // 運用が既定（上限より狭い）なら、同じ表明が上限で丸められる。
        match decide_declare(&state.db, &cfg(true, 100, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(
                plan.window_size,
                opencrab_actions::memory_units::DECLARE_WINDOW_MAX
            ),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 道具経由で位置と広さを表明し、**ラン → 次の窓**まで通す（本人が実際に通る経路の一気通貫）。
    #[tokio::test]
    async fn tool_expressed_window_flows_through_a_real_run() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 200);
        let (_dir, ctx) = declare_tool_ctx(&state);
        let r = opencrab_actions::memory_units::PlanNextMemoryWindowAction
            .execute(
                &json!({"next_from_id": ids[40], "window_size": 80, "note": "まだ続いている"}),
                &ctx,
            )
            .await;
        assert!(r.success, "{:?}", r.error);

        // 窓 60 のランが clean で終わると、カーソルは道具で指した 1 つ手前へ。
        let fake = FakeRunner::new(FakeOutcome::Completed);
        run_declare(
            &state.db,
            &cfg(true, 60, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(cursor_of(&state), ids[39]);
        let audit = latest_sleep_audit(&state).expect("監査ログ");
        assert_eq!(audit["window_note"], json!("まだ続いている"));

        // 次のランの窓は、道具で表明した広さ 80（config の 60 ではない）で組まれる。
        set_marker(
            &state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(&state)),
        );
        match decide_declare(&state.db, &cfg(true, 60, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.window.from_id, Some(ids[40]), "指した id から再開する");
                assert_eq!(plan.window_size, 80);
                assert_eq!(plan.window.log_count, 80);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 丸めの下限・上限は**窓と同じ時点**で決まり、生ログの件数で測られる。
    #[test]
    fn position_bounds_are_measured_in_rows() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 40);
        match decide_declare(&state.db, &cfg(true, 9, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.min_position, ids[2], "窓 9 件の 1/3 = 3 件目");
                assert_eq!(plan.max_position, ids[17], "窓 9 件の 2 倍 = 18 件目");
                assert!(plan.min_position <= plan.window.to_id.unwrap());
                assert!(plan.max_position >= plan.window.to_id.unwrap());
            }
            other => panic!("expected Run, got {other:?}"),
        }
        // 生ログが 2 窓ぶんに満たないときは、上限は「あるだけ」（最後の id）。
        let state2 = crate::test_app_state();
        let ids2 = seed_window(&state2, 12);
        match decide_declare(&state2.db, &cfg(true, 9, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(plan.max_position, ids2[11]),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// プロンプトに「窓は自分で調整できる」ことが**書いてある**（道具を足しても説明が無ければ
    /// 使われない）。今回の広さと、丸めの範囲も示す。
    #[test]
    fn system_prompt_explains_window_control() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 9);
        let plan = match decide_declare(&state.db, &cfg(true, 9, 1), "a1").unwrap() {
            DeclareDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let sp = build_system_prompt(&plan);
        assert!(sp.contains("plan_next_memory_window"), "道具名が無い");
        assert!(
            sp.contains("範囲の切り方もあなたが決められます"),
            "説明が無い"
        );
        assert!(sp.contains("next_from_id"), "持ち越しの指定方法が無い");
        assert!(sp.contains("window_size"), "広さの変え方が無い");
        // 今回の広さ（9 件）と既定/本人の別。
        assert!(sp.contains("いまの設定は 9 件です（既定の広さ）"));
        // 丸めの範囲は next_from_id として指せる値（＝位置 + 1）で示す。
        assert!(sp.contains(&format!("id {} 〜 {}", ids[2] + 1, ids[8] + 1)));
    }

    /// **約束と実装が一致していること**: 広さは sticky だが、機械が既定へ戻すことがある。
    /// プロンプトはその条件（既定より広い / N 回連続）まで書き、狭めた設定は戻らないと言う。
    ///
    /// 回数は**実装の定数から組む**ので、`MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET` を変えれば
    /// 文面も追随する（片方だけ変わって食い違うことがない）。
    #[test]
    fn system_prompt_promise_matches_the_auto_reset_rule() {
        let state = crate::test_app_state();
        seed_window(&state, 9);
        let plan = match decide_declare(&state.db, &cfg(true, 9, 1), "a1").unwrap() {
            DeclareDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let sp = build_system_prompt(&plan);
        // sticky であることは引き続き言う。
        assert!(sp.contains("一度決めると変えるまで効き続けます"));
        // ただし機械が戻すことがある、という但し書きが同じ場所にある。
        assert!(
            sp.contains(&format!(
                "{MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET} 回続いたら"
            )),
            "自動で戻す条件（回数）が実装の定数と結びついていない: {sp}"
        );
        assert!(
            sp.contains("既定の広さへ自動で戻します"),
            "自動で戻すことが書かれていない"
        );
        // 戻す対象は「既定より広げた設定」だけ、という条件まで書く（狭めた設定は戻らない）。
        assert!(sp.contains("既定より広げた設定"), "対象の条件が無い");
        assert!(
            sp.contains("狭めた設定はそのままです"),
            "対象外の明示が無い"
        );
    }

    /// 残ログが窓より少ないとき、プロンプトの 2 つの件数（提示した実数と設定値）が
    /// **矛盾して読めない**こと。設定 100 / 残り 9 件で「9 件」と「100 件」が並ぶ形。
    #[test]
    fn system_prompt_distinguishes_actual_range_from_configured_size() {
        let state = crate::test_app_state();
        seed_window(&state, 9);
        let plan = match decide_declare(&state.db, &cfg(true, 100, 1), "a1").unwrap() {
            DeclareDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        assert_eq!(plan.window.log_count, 9, "実際に提示できるのは 9 件");
        assert_eq!(plan.window_size, 100, "設定は 100 件");
        let sp = build_system_prompt(&plan);
        // 提示した範囲は実数で書く。
        assert!(sp.contains("今回の範囲（未宣言"));
        assert!(sp.contains("/ 9 件 /"));
        // 広さは「設定」として書き分け、実数がこれより少なくなり得ることを添える。
        assert!(sp.contains("いまの設定は 100 件です"));
        assert!(
            sp.contains("これより少なくなります"),
            "設定値と実数がずれ得ることの説明が無い"
        );
        // 「今回は 100 件」という、提示した実数と読める書き方をしていない。
        assert!(!sp.contains("今回は 100 件"));
    }
}
