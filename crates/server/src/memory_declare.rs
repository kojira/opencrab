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
//! - **位置は前進のみ**（partial では位置を進めない）。clean 完了時だけ提示窓の末尾へ進める。
//!   ただし **throttle（日次ゲート用の壁時計）は clean/partial に関わらず毎回 `now` へ進める**
//!   （partial で据え置くと同じ窓のまま tick 毎に再発火して LLM を呼び続けるため / #366 と同型）。
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
    // ラン制御（ターンを終える宣言のみ）
    "declare_done",
];

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
    let position = if clean {
        plan.window.to_id.unwrap_or(plan.cursor_id)
    } else {
        plan.cursor_id
    };
    let marker_after = format_marker(&now.to_rfc3339(), position);
    {
        let conn = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        opencrab_db::queries::set_memory_declare_cursor(&conn, agent_id, &marker_after)?;
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
    /// 位置を据え置く値として使う（clean は `window.to_id` へ進む）。
    cursor_id: i64,
}

/// ゲート判定の結果。
#[derive(Debug)]
enum DeclareDecision {
    /// 発火しない（理由つき）。
    Skip(&'static str),
    /// 発火する。`DeclarePlan` は大きいので Box する（clippy::large_enum_variant）。
    Run(Box<DeclarePlan>),
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

    // 未宣言の窓（マーカーより新しい生ログを id 昇順で最大 max_logs 件）。
    let window =
        opencrab_db::queries::declare_window(&conn, agent_id, cursor_id, cfg.max_logs.max(1))?;

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
         メッセージ送信・シェル実行・サブタスク起動はしないでください。これは記憶を宣言する時間です。\
         生ログは読むだけで、消えも変わりもしません。宣言は何度でもやり直せます。\n\
         【サイズの約束】1 回のツール結果が inline_limit_tokens（約 2,500 トークン）を超えると本文は捨てられます。\
         地図の est_tokens や read_my_history の estimated_tokens を見て、大きい範囲は id 窓を狭めるか cursor_from_id で刻んで読んでください。\n\n\
         # あなたの記憶の地図（生ログ全体の分布・day 粒度）\n{survey_txt}\n\n\
         # 今回の範囲（未宣言 / id {from}〜{to} / {count} 件 / {span}）\n\
         この範囲の生ログには、まだあなたの記憶の単位が宣言されていません。ここを読んで、あなたに\
         とっての「一つの記憶」を宣言してください。セッションの切れ目・話題の切れ目・気持ちの切れ目、\
         どれを単位にするかはあなた次第です。（この範囲のセッション数の目安: {sessions}）\n\n\
         # すでに宣言した記憶（最近のもの）\n{units_txt}",
        count = w.log_count,
        sessions = w.session_count,
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
    use opencrab_core::EngineResult;
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
}
