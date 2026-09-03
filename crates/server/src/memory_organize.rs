//! スリープ整理ラン本体（#313 段階3 / #361）。
//!
//! **システムが分類するのをやめ、エージェント自身に道具を渡して整理させる。** メンテナンス
//! ループ（`memory_maintenance`）の ①〜③（索引ビルド / backfill / ロールアップ）で索引が
//! 確定したあと、⑥ としてここが走る。エージェント本人が**新規の別セッション**・**本人の人格**
//! で、自分の記憶（topic）に自分のやり方でタグを付け／統合する。
//!
//! 既存の仕組みに乗る（新しいエンジンを作らない）:
//! - 起動は `process::run_agent_response`（subtask / heartbeat と同じ headless 経路）。
//! - caller は **Owner**（heartbeat と同じ前例）。タグ道具は `TRUSTED_ONLY` なので Owner で通る。
//! - 監査は 2 層: `llm_logs`（`run_agent_response` が各 LLM コールで自動永続化）+
//!   `agent_logs`（`context="sleep"` の構造化サマリをこのモジュールが書く）。
//!
//! 絶対に守るもの（#361）:
//! - **対話ターンでは走らせない**（#291）。呼び出し元は sleep ループのみ。
//! - **結果を会話へ自動注入しない**（#316）。system プロンプトはここで自前に組むので
//!   `[Memory Index]` の注入経路（`build_agent_context`）は通らない。タグが次回以降の
//!   `[Memory Index]` の `Categories:` 行に出るのは段階1/2 で入った既存挙動で、ここは触らない。
//! - **1 エージェント内しか見ない**（他エージェントの記憶を混ぜない）。全クエリが `agent_id` 固定。
//! - **既定オフ**。`enabled=false` なら RunRequest すら組まずゼロコールで即 return。

use chrono::{DateTime, Duration, Utc};
use serde_json::json;

use crate::config::MemoryOrganizeConfig;
use crate::memory_maintenance::IndexBuildInflight;
use crate::AppState;
use opencrab_actions::{CallerIdentity, RunRequest};
use opencrab_core::llm_text::truncate_chars;
use opencrab_core::EngineResult;
use opencrab_db::queries::IndexNodeRow;

/// 整理ランが「エージェント的な1ターンを回す」ために必要とする**唯一の手足**（#370）。
///
/// 整理ラン（sleep）のロジック本体（ゲート判定・worklist 組み立て・マーカー前進・partial の
/// 扱い・監査）は、外へ出る口も LLM も**持たない**。唯一「1 ターンを実際に走らせて結果を得る」
/// 部分だけをこの狭い口に切り出す。
///
/// - **本番**は [`AppStateTurnRunner`]（`run_agent_response` を呼ぶ実装）を渡す。ラン構築一式
///   （dispatcher / gateway スロット / MCP / activity webhook sink / metrics / LLM client /
///   engine）はこの実装の**内側**にだけ存在する。
/// - **テスト**は結果（[`EngineResult`]）を差し替えるフェイクを渡す。フェイクは何も構築しない
///   ので、webhook も gateway も MCP も LLM も**そもそも sleep の依存に入らない**（隔離実験の
///   つもりが本番 Discord へ飛んだ #370 の再発を、症状の個別封じではなく構造で防ぐ）。
///
/// タイムアウトは呼び出し側（[`run_organize`]）が sleep ポリシーとして被せる。ここは「1 ターンを
/// 走らせる」ことだけに責務を絞る。
#[async_trait::async_trait]
pub trait OrganizeTurnRunner: Send + Sync {
    /// 与えた [`RunRequest`] で 1 ターンを走らせ、結果を返す。`Err` は run 自体の失敗。
    async fn run_turn(&self, req: RunRequest) -> anyhow::Result<EngineResult>;
}

/// 本番の [`OrganizeTurnRunner`]。`run_agent_response`（本番のラン構築経路）へ委譲する。
///
/// この型より外側（sleep ロジック）は `AppState` を持たないため、gateway/MCP/webhook を
/// 構築する術がない。ラン構築が必要とする `state` はこの実装の中だけに閉じ込める。
pub struct AppStateTurnRunner<'a> {
    pub state: &'a AppState,
}

#[async_trait::async_trait]
impl OrganizeTurnRunner for AppStateTurnRunner<'_> {
    async fn run_turn(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
        crate::process::run_agent_response(self.state, req).await
    }
}

/// 1 回の worklist に載せる 1 topic あたりの要約の最大文字数（プロンプト肥大の抑制）。
/// 実測平均は要約 102 字（#313）。振れ幅を吸収しつつ上限を持たせる。
const SUMMARY_MAX_CHARS: usize = 240;

/// sleep 整理ランに渡すツール許可リスト（#368）。
///
/// **眠っている間に外へ手が出せる状態にしない。** 整理ランの用途は「自分の記憶を読んで
/// タグを付ける／統合する」に固定されているので、必要なのは**記憶の読み取り**と**タグ操作**、
/// そして**ターンを終える最小限のラン制御**だけ。`execute_shell` / `nostr_run`（外向き投稿）/
/// `spawn_subtask` / `ws_write` / `ws_delete` / `configure_*` / `update_instructions` 等は
/// 一切渡さない。
///
/// この許可リストは `RunRequest.tool_allowlist` 経由で `BridgedExecutor` に載り、可視性
/// （`list_tools`）と実行（`dispatch_inner`）の**両方**を、**全スロット**（dispatcher /
/// gateway own = `SystemGatewayActions` / MCP）にわたって絞る。既存の caller ゲート
/// （`tool_policy`。タグ道具は `TRUSTED_ONLY`）は弱めず、その**上に重ねる**。
///
/// 内訳:
/// - 読み取り: `browse_memory_index` / `search_memory_index` / `retrieve_memory_nodes` /
///   `search_my_history`（対象 topic の中身をもっと知りたいときに引く）。
/// - タグ操作: `tag_topic` / `untag_topic` / `merge_tags`（整理の本体）。
/// - ラン制御: `declare_done`（そのターンを終える宣言）。整理ランは system プロンプトで
///   「終わったら観点を一言残す」と促しており、モデルは通常ツール無しの最終テキストで
///   自然終了するが、`declare_done` は「これ以上やることが無い」を明示する既存の終了シグナル
///   なので載せる（外向きの副作用は無い / `CORE_INLINE_ACTIONS`）。他のラン制御
///   （`report_progress` / `spawn_subtask` / `cancel_subtask`）は subtask ライフサイクル用で、
///   整理ラン（inline・非 subtask）には不要なので入れない。
pub const ORGANIZE_ALLOWED_TOOLS: &[&str] = &[
    // 読み取り
    "browse_memory_index",
    "search_memory_index",
    "retrieve_memory_nodes",
    "search_my_history",
    // 記憶の単位（宣言）の読み取り 2 つ（#379 #376 段階1）。整理ランが生ログを俯瞰・範囲読み
    // できるようにする。記録 2 つ（record / retract）は宣言ラン（段階2）で別途載せる。
    "survey_my_history",
    "read_my_history",
    // タグ操作
    "tag_topic",
    "untag_topic",
    "merge_tags",
    // ラン制御（ターンを終える宣言のみ）
    "declare_done",
];

/// このエージェントの整理ランを（ゲートを満たせば）実行する。**本番エントリ**。
///
/// 本番のラン構築（`run_agent_response`）を [`AppStateTurnRunner`] に閉じ込め、sleep の
/// ロジック本体は [`run_organize`] に委譲する。sleep 本体は `AppState` を持たないので、
/// gateway/MCP/webhook を構築する術がない（#370）。
///
/// 戻り値: 整理ラン（LLM）を実際に起動したら `true`。既定オフ・ゲート未達・初回シードは
/// `false`（＝ LLM ゼロコール）。
pub async fn maybe_run_memory_organize(state: &AppState, agent_id: &str) -> anyhow::Result<bool> {
    let runner = AppStateTurnRunner { state };
    run_organize(
        &state.db,
        &state.memory_organize,
        &state.index_build_inflight,
        agent_id,
        &runner,
    )
    .await
}

/// 整理ラン（sleep）のロジック本体。**必要な手足だけ**を引数で受け取る（#370）:
/// DB・設定・二重起動スロット・1 ターンを回す [`OrganizeTurnRunner`]。
///
/// `AppState` を受け取らないので、この関数からは gateway/MCP/activity webhook を構築できない
/// （構造的に外へ出ない）。1 ターンを走らせる部分だけを `runner` に委ね、本番は
/// `run_agent_response` 実装、テストは結果差し替えのフェイクを渡す。これにより本番のラン構築を
/// 通さずにゲート判定・worklist 組み立て・マーカー前進/据え置き・partial の扱いを単体検証できる。
async fn run_organize(
    db: &opencrab_db::Db,
    cfg: &MemoryOrganizeConfig,
    inflight: &IndexBuildInflight,
    agent_id: &str,
    runner: &dyn OrganizeTurnRunner,
) -> anyhow::Result<bool> {
    // 既定オフ: ここで即 return する。RunRequest も DB 書き込みも一切しない（ゼロコール）。
    if !cfg.enabled {
        return Ok(false);
    }

    // --- ゲート判定 + worklist 組み立て（DB 読みのみ。ロックは await を跨がない）---
    let plan = match decide_organize(db, cfg, agent_id)? {
        OrganizeDecision::Skip(reason) => {
            tracing::debug!(agent_id, reason, "memory organize: skipped by gate");
            return Ok(false);
        }
        OrganizeDecision::Seeded => {
            tracing::debug!(agent_id, "memory organize: seeded marker (first encounter)");
            return Ok(false);
        }
        OrganizeDecision::Run(plan) => plan,
    };

    // --- 排他（索引ビルドと衝突しない名前空間キー）---
    // 整理ランは sleep ループからしか呼ばれない（対話ターン非経由）ので実質競合しないが、
    // ①増分ビルドや skill 棚卸しと同じスロット機構で二重起動を防ぐ。
    let guard = crate::memory_maintenance::try_acquire_build_slot(
        inflight,
        &format!("organize:{agent_id}"),
    );
    let Some(_guard) = guard else {
        return Ok(false); // 既に走っている
    };

    // --- 起動（新規の別セッション / 本人の人格 / caller=Owner）---
    let now = Utc::now();
    let session_id = format!("sleep-organize-{agent_id}-{}", now.timestamp());
    let system_prompt = build_system_prompt(&plan);
    let conversation = build_task_message(&plan);

    // gateway_actions=None（送信経路を渡さない = 会話へ出さない）。dispatch なし
    // （ツールは inline 実行。background subtask 化しない）。
    //
    // ツール許可リスト（#368）: caller=Owner なので放置すると Owner の全ツール
    // （`execute_shell` / `nostr_run` / `ws_write` / `configure_*` / `update_instructions` …）が
    // 届く。整理ランは「眠っている」内向きのランなので、記憶の読み取り・タグ操作・ターン終了
    // 宣言だけに絞る（`ORGANIZE_ALLOWED_TOOLS`）。可視性と実行の両方を全スロットで絞る。
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
        ORGANIZE_ALLOWED_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    )
    // 宣言ランと同じく、整理ランのターンも生ログ（`memory_sessions`）に書かない（#393）。
    // 整備作業は本人の生きた体験ではなく、記憶の材料にしない。
    .without_turn_logs();

    let started = std::time::Instant::now();
    // タイムアウトは sleep ポリシー（「どこまで待つか」）としてここで被せる。1 ターンを走らせる
    // 実体は `runner` に委ねる（本番＝run_agent_response / テスト＝フェイク）。
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
            tracing::warn!(agent_id, error = %e, "memory organize run failed");
            ("error", false)
        }
        Err(_) => ("timeout", false),
    };

    // --- 前進（前進のみ / 残りは次回 / 位置 2 軸 + throttle 刻時）---
    // clean 完了時のみ前進する（詳細は `advance_markers`）:
    //  - **新規側**（`last_organize_at` / 昇順）: **提示した新規 topic があるときだけ**末尾へ。0 件なら
    //    据え置き（壁時計へは飛ばさない = snapshot 外の取り残しを追い越さない / #365 レビュー修正）。
    //  - **遡り側**（`organize_backlog_cursor` / 降順）: 過去分を提示したときだけ、提示した中で
    //    最も古い (created_at, id) より古い分を次回の対象に。**索引ビルドは 1 パスの全 topic に
    //    同一 created_at を刻むため、created_at 単体でなく id を副キーに持つカーソルにしている**
    //    （降順側でも同着群の残余を取りこぼさない / #364 blocker と同型）。
    //  - **throttle**（`organize_last_run_at`）: 常に `now`。位置と分離して日次ゲートを支える。
    // partial（timeout / ターン上限 / エラー）ではどれも進めない。タグ付与は PK 冪等
    // （`assign_topic_to_category`）なので、同じ範囲を次回に再挑戦しても重複しない。
    {
        let conn = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        advance_markers(&conn, agent_id, &plan, clean)?;
    }

    // --- 監査（層1: agent_logs / context="sleep"）---
    // 層2（生プロンプト/生応答）は `run_agent_response` が LLM コールごとに llm_logs へ残す。
    {
        let audit = json!({
            "kind": "memory_organize",
            "outcome": outcome,
            "worklist_size": plan.worklist_size,
            "new_topic_count": plan.new_topic_count,
            "new_presented": plan.new_presented,
            "backlog_presented": plan.backlog_presented,
            "backlog_remaining": plan.backlog_remaining,
            "snapshot_log_id": plan.snapshot_log_id,
            "session_id": session_id,
            "marker_advanced": clean,
            "new_marker_advanced_to": if clean { plan.new_marker_advance_to.clone() } else { None },
            "backlog_marker_advanced_to": if clean { plan.backlog_marker_advance_to.clone() } else { None },
            "last_run_at": if clean { Some(plan.run_at.clone()) } else { None },
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
            tracing::warn!(agent_id, error = %e, "failed to persist memory organize audit log");
        }
    }

    tracing::info!(
        agent_id,
        outcome,
        worklist = plan.worklist_size,
        marker_advanced = clean,
        "memory organize ran"
    );
    Ok(true)
}

/// 整理ランの実行計画（ゲート通過時のみ組む）。
///
/// worklist は **新規（前進 / 昇順）を優先し、枠が余ったら過去分（遡り / 降順）で埋める**
/// （#365）。合計は `max_topics` を超えない。2 つの軸のマーカーは独立に前進させる。
#[derive(Debug)]
struct OrganizePlan {
    persona_name: String,
    personality: Option<String>,
    instructions: String,
    snapshot_log_id: i64,
    /// worklist（提示する topic）。前半が新規（`(created_at, id)` 昇順）、後半が過去分
    /// （遡り / `(created_at, id)` 降順）。
    worklist: Vec<IndexNodeRow>,
    worklist_size: usize,
    /// スナップショット以下の新規 topic 総数（N で切る前）。監査・ゲート表示用。
    new_topic_count: i64,
    /// 提示した新規の件数（前半）。
    new_presented: usize,
    /// 提示した過去分の件数（後半 / 遡り）。
    backlog_presented: usize,
    /// 遡り側の残数（提示分を除く前の残り。監査・先頭到達の把握用）。
    backlog_remaining: i64,
    /// 既存タグ（title, 付与件数）。プロンプトに現行の語彙として同梱する。
    tags: Vec<(String, i64)>,
    /// clean 完了時に**新規側マーカー**（位置 / `last_organize_at`）へ刻む複合カーソル
    /// `"{created_at}|{id}"`。**実際に提示した新規 topic の末尾（最新）のときだけ `Some`**。
    /// 新規 0 件なら `None`（据え置き）。**壁時計 `now` へは絶対に飛ばさない** — 非トランザクションな
    /// ビルドが途中失敗して `end_log_id > watermark`（snapshot 外）の topic を残したとき、その
    /// `created_at`（`now` より前）を追い越して恒久ロスするため（#365 レビュー修正 / #364 と同型）。
    new_marker_advance_to: Option<String>,
    /// clean 完了時に**遡り側マーカー**へ刻む複合カーソル。過去分を提示したときのみ
    /// `Some`（提示末尾＝提示した中で最も古い `(created_at, id)`）。0 件なら `None`（据え置き）。
    backlog_marker_advance_to: Option<String>,
    /// clean 完了時に**日次 throttle 用刻時**（`organize_last_run_at`）へ刻む壁時計。位置マーカーと
    /// 分離することで、位置は「見た topic」までしか進めず（安全）、時刻は毎回 `now` へ進める
    /// （静かな日でも tick 毎起動しない）を両立させる。
    run_at: String,
}

/// ゲート判定の結果。
#[derive(Debug)]
enum OrganizeDecision {
    /// 発火しない（理由つき）。
    Skip(&'static str),
    /// 初回遭遇: `now` をマーカーにシードして今回はスキップ（既存の全 topic を一気に
    /// 対象化しない）。次回以降、シード後に増えた topic が下限に達したら発火する。
    Seeded,
    /// 発火する。`OrganizePlan` は大きいので Box して enum の variant 間サイズ差を抑える
    /// （clippy::large_enum_variant）。
    Run(Box<OrganizePlan>),
}

/// ゲート（日次 + 下限）を判定し、通れば worklist と人格を積んだ計画を返す。
///
/// DB 読みのみ（初回シードの 1 write を除く）。ロックは関数内で完結し、`run_agent_response`
/// の await を跨いで保持しない。
fn decide_organize(
    db: &opencrab_db::Db,
    cfg: &MemoryOrganizeConfig,
    agent_id: &str,
) -> anyhow::Result<OrganizeDecision> {
    let now = Utc::now();
    let conn = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;

    // ゲート1: 日次 + 初回シード。
    let last_at = opencrab_db::queries::get_last_organize_at(&conn, agent_id)?;
    let Some(last_at) = last_at else {
        // 初回遭遇: **3 マーカーを now にシード**して終了（既存履歴を「新規」に数えない）。
        // id 部を持たない素の刻時でよい（次回 parse_cursor が `|` 無しを (now, "") と解釈する）。
        // 新規側は now より後を「新規」に、遡り側は now より前を「過去分」に分ける境界になる。
        // throttle（organize_last_run_at）も now を刻んで最初の 1 回を throttle する。
        let now_s = now.to_rfc3339();
        opencrab_db::queries::set_last_organize_at(&conn, agent_id, &now_s)?;
        opencrab_db::queries::set_organize_backlog_cursor(&conn, agent_id, &now_s)?;
        opencrab_db::queries::set_organize_last_run_at(&conn, agent_id, &now_s)?;
        return Ok(OrganizeDecision::Seeded);
    };
    // 日次ゲートは**位置マーカーではなく throttle 用刻時**（organize_last_run_at）で判定する。
    // 位置（新規側カーソル）は「見た topic」までしか進まず、静かな日には過去へ留まるため
    // throttle の基準に使えない（tick 毎起動になる）。刻時は clean 完了ごとに `now` へ進む。
    // 移行 DB（段階3/3b で先に有効化・本列 NULL）は last_organize_at の created_at 部へ
    // フォールバックする（旧挙動 / 本番は未有効化なので通らない）。
    let last_run_ts = opencrab_db::queries::get_organize_last_run_at(&conn, agent_id)?
        .unwrap_or_else(|| parse_cursor(&last_at).0);
    let elapsed = last_run_ts
        .parse::<DateTime<Utc>>()
        .map(|dt| now.signed_duration_since(dt))
        .unwrap_or_else(|_| Duration::zero());
    if elapsed < Duration::minutes(cfg.min_interval_minutes.max(1)) {
        return Ok(OrganizeDecision::Skip("interval_not_elapsed"));
    }
    let (since_ts, since_id) = parse_cursor(&last_at);

    // スナップショット（①〜③で最新化済みの索引の上端）。
    let snapshot_log_id = opencrab_db::queries::get_index_watermark(&conn, agent_id)?
        .map(|w| w.last_indexed_log_id)
        .unwrap_or(0);

    // --- 新規側（前進 / 昇順）を優先で組む ---
    let cursor = Some((since_ts.as_str(), since_id.as_str()));
    let new_topic_count =
        opencrab_db::queries::count_organize_topics(&conn, agent_id, cursor, snapshot_log_id)?;
    let budget = cfg.max_topics.max(1);
    let new_worklist = opencrab_db::queries::list_organize_topics(
        &conn,
        agent_id,
        cursor,
        snapshot_log_id,
        budget,
    )?;
    let new_presented = new_worklist.len();
    // 新規側マーカー前進先 = **実際に提示した新規 topic の末尾（最新）の (created_at, id) だけ**。
    // 新規 0 件なら `None`（据え置き）— 壁時計 `now` へは飛ばさない（snapshot 外に取り残された
    // topic を追い越して恒久ロスするため / #365 レビュー）。並び順が `created_at ASC, id ASC`
    // なので末尾が最大。同着 created_at 群を N で切っても id 副キーで残余を次回へ引き継ぐ。
    let new_marker_advance_to = new_worklist
        .last()
        .map(|r| format_cursor(&r.created_at, &r.id));

    // --- 枠が余ったら過去分（遡り / 降順）で埋める ---
    // 遡りカーソルは新規側と**別軸**。未シードなら now を境界にシードする（初回遭遇では
    // 上でシード済み。ここに来るのは段階3 で先に有効化された移行 DB のみ）。
    let backlog_cursor_raw =
        match opencrab_db::queries::get_organize_backlog_cursor(&conn, agent_id)? {
            Some(c) => c,
            None => {
                let now_s = now.to_rfc3339();
                opencrab_db::queries::set_organize_backlog_cursor(&conn, agent_id, &now_s)?;
                now_s
            }
        };
    let (before_ts, before_id) = parse_cursor(&backlog_cursor_raw);
    let backlog_remaining = opencrab_db::queries::count_organize_backlog_topics(
        &conn,
        agent_id,
        (&before_ts, &before_id),
        snapshot_log_id,
    )?;
    let remaining_budget = budget - new_presented as i64;
    let backlog_worklist = if remaining_budget > 0 {
        opencrab_db::queries::list_organize_backlog_topics(
            &conn,
            agent_id,
            (&before_ts, &before_id),
            snapshot_log_id,
            remaining_budget,
        )?
    } else {
        Vec::new()
    };
    let backlog_presented = backlog_worklist.len();
    // 遡り側マーカー前進先 = 提示末尾（提示した中で最も古い / 降順の末尾）の (created_at, id)。
    // 過去分を提示したときのみ刻む。0 件なら据え置き（先頭到達なら二度と進めない＝止まる）。
    let backlog_marker_advance_to = backlog_worklist
        .last()
        .map(|r| format_cursor(&r.created_at, &r.id));

    // ゲート2: 発火判定。**新規が下限に達する**か、または**過去分の消化余地がある**なら
    // 発火する。過去分だけの日（新規 0）でも消化が進むようにするため、下限は新規側だけを
    // 塞ぐ（過去分があれば通す / #365 受け入れ条件）。両方とも無いときだけスキップ。
    if new_topic_count < cfg.min_new_topics.max(1) && backlog_worklist.is_empty() {
        return Ok(OrganizeDecision::Skip("below_floor_no_backlog"));
    }

    // 提示順は「新規 → 過去分」（新規優先）。合計は budget 以下。
    let mut worklist = new_worklist;
    worklist.extend(backlog_worklist);

    // 人格（モデル解決は run_agent_response 側が effective_model で行うのでここでは不要）。
    let (persona_name, personality, instructions) =
        opencrab_db::queries::get_agent(&conn, agent_id)
            .ok()
            .flatten()
            .map(|a| (a.persona_name, a.personality, a.instructions))
            .unwrap_or_else(|| (agent_id.to_string(), None, String::new()));

    // 既存タグ（現行の語彙）。件数つきで見せる（統合判断の材料）。
    let tag_nodes = opencrab_db::queries::list_top_level_categories(&conn, agent_id)?;
    let counts = opencrab_db::queries::count_category_members(&conn, agent_id)?;
    let tags: Vec<(String, i64)> = tag_nodes
        .iter()
        .map(|n| (n.title.clone(), counts.get(&n.id).copied().unwrap_or(0)))
        .collect();

    let worklist_size = worklist.len();
    Ok(OrganizeDecision::Run(Box::new(OrganizePlan {
        persona_name,
        personality,
        instructions,
        snapshot_log_id,
        worklist,
        worklist_size,
        new_topic_count,
        new_presented,
        backlog_presented,
        backlog_remaining,
        tags,
        new_marker_advance_to,
        backlog_marker_advance_to,
        run_at: now.to_rfc3339(),
    })))
}

/// clean 完了時のみ、位置マーカー（2 軸）と throttle 刻時を前進させる（partial では**進めない**
/// / #364 と同じ流儀）。
///
/// - 新規側（`last_organize_at`）: **提示した新規 topic があるときだけ**前進（末尾へ）。0 件なら
///   据え置き。壁時計へは飛ばさない（snapshot 外の取り残しを追い越さない / #365）。
/// - 遡り側（`organize_backlog_cursor`）: 過去分を提示したときだけ前進（先頭到達なら据え置き＝止まる）。
/// - throttle（`organize_last_run_at`）: **常に** `now` を刻む。位置と分離して日次ゲートを支える。
fn advance_markers(
    conn: &rusqlite::Connection,
    agent_id: &str,
    plan: &OrganizePlan,
    clean: bool,
) -> anyhow::Result<()> {
    if !clean {
        return Ok(());
    }
    if let Some(new_to) = &plan.new_marker_advance_to {
        opencrab_db::queries::set_last_organize_at(conn, agent_id, new_to)?;
    }
    if let Some(backlog_to) = &plan.backlog_marker_advance_to {
        opencrab_db::queries::set_organize_backlog_cursor(conn, agent_id, backlog_to)?;
    }
    // 位置の前進有無に関わらず throttle は毎回進める（静かな日でも tick 毎起動しない）。
    opencrab_db::queries::set_organize_last_run_at(conn, agent_id, &plan.run_at)?;
    Ok(())
}

/// system プロンプト（本人の人格 + 整理の枠組み + 現行タグ + worklist）を組む。
///
/// `build_agent_context`（`[Memory Index]` を注入する通常ターンの経路）は通さず、ここで
/// 自前に組む。整理の結果を会話へ自動注入しないため（#316）。
fn build_system_prompt(plan: &OrganizePlan) -> String {
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

    let tags_txt = if plan.tags.is_empty() {
        "(まだタグはありません。最初のタグをあなたが決めます)".to_string()
    } else {
        plan.tags
            .iter()
            .map(|(name, n)| format!("- {name}（{n}件）"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let worklist_txt = plan
        .worklist
        .iter()
        .map(format_topic_line)
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "{personality_section}\
         これはあなたのスリープ（内省）の時間です。あなた自身の記憶を、あなたの人格・関心で\
         整理します。正解や平均に合わせる必要はありません。一期一会でよく、決定的である必要も\
         ありません。あなたが大事に思うまとまりを、あなたのやり方でタグにしてください。\n\n\
         # やること\n\
         下の「今回の対象」に挙げた記憶（topic）を見て、あなたの関心に沿ってタグを付けてください。\
         1 つの topic に複数のタグを付けてよいし、付けないという判断もあり得ます。似たタグが\
         増えて散らかってきたと感じたら統合してください。この範囲の外の記憶には手を出さないでください。\n\n\
         # 使える道具\n\
         - `browse_memory_index` / `search_memory_index` / `retrieve_memory_nodes` / `search_my_history`: \
         記憶を読む（対象の中身をもっと知りたいときに引く）\n\
         - `tag_topic(topic_id, tags[])`: topic にタグを付ける（無いタグ名はその場で新設）\n\
         - `untag_topic(topic_id, tag)`: topic からタグ 1 個を外す\n\
         - `merge_tags(from, into)`: 2 つのタグを統合する（実質リネームにもなる）\n\
         メッセージ送信・シェル実行・サブタスク起動はしないでください。これは記憶整理の時間です。\n\
         【サイズの約束】1 回のツール結果が inline_limit_tokens（約 2,500 トークン）を超えると本文は捨てられます。\
         survey_my_history の est_tokens や read_my_history の estimated_tokens（estimate_only=true で本文なしに測れる）を見て、大きい範囲は狭めて読んでください。\n\n\
         # 現在のタグ（あなたの語彙・付与件数）\n{tags_txt}\n\n\
         # 今回の対象（{size} 件 / あなたの記憶（topic））\n\
         最近の分と、まだ見ていない過去の分が混ざっています。どれもあなた自身の記憶です。\
         各行は `[短縮ID] タイトル — 要約` です。`短縮ID` を `topic_id` に渡してください。\n{worklist_txt}",
        size = plan.worklist_size,
    ) + &instructions_section
}

/// エンジンに渡す「ユーザーターン」。system 側に対象を明示済みなので、ここは着手の合図のみ。
fn build_task_message(plan: &OrganizePlan) -> String {
    format!(
        "スリープ整理の時間です。上に挙げた {} 件の記憶を、あなたの関心に沿ってタグ付け・統合してください。\
         終わったら、どういう観点で整理したかを一言だけ残してください。",
        plan.worklist_size
    )
}

/// worklist の 1 行を `[短縮ID] タイトル — 要約` で組む（要約は上限で切り詰め）。
fn format_topic_line(t: &IndexNodeRow) -> String {
    let id = t.short_id.as_deref().unwrap_or(&t.id);
    let title = t.title.trim();
    let summary = truncate_chars(t.summary.trim(), SUMMARY_MAX_CHARS);
    if summary.is_empty() {
        format!("- [{id}] {title}")
    } else {
        format!("- [{id}] {title} — {summary}")
    }
}

/// マーカー（`last_organize_at`）の複合カーソル `"{created_at}|{id}"` を組む。
///
/// `created_at`（rfc3339）にも `id`（`topic-{agent}-{session}-{first}-{last}` 等）にも `|`
/// は現れないので、最初の `|` を区切りに使える。
fn format_cursor(created_at: &str, id: &str) -> String {
    format!("{created_at}|{id}")
}

/// マーカーを `(created_at, id)` へ分解する。`|` が無ければ全体を `created_at` とみなし
/// `id` は空（初回シードした素の刻時や、旧形式との後方互換）。
fn parse_cursor(marker: &str) -> (String, String) {
    match marker.split_once('|') {
        Some((ts, id)) => (ts.to_string(), id.to_string()),
        None => (marker.to_string(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // --- ゲート判定（decide_organize）用のセットアップ ---

    fn cfg(enabled: bool, max_topics: i64, min_new: i64) -> MemoryOrganizeConfig {
        MemoryOrganizeConfig {
            enabled,
            max_topics,
            min_new_topics: min_new,
            min_interval_minutes: 1440,
            timeout_secs: 600,
        }
    }

    /// state の DB に watermark を刻む（スナップショット上端）。
    fn set_watermark(state: &AppState, agent_id: &str, last_log_id: i64) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_index_watermark(
            &conn,
            &opencrab_db::queries::WatermarkRow {
                agent_id: agent_id.to_string(),
                last_indexed_log_id: last_log_id,
                last_indexed_at: "2026-08-03T00:00:00Z".to_string(),
                total_nodes: 0,
            },
        )
        .unwrap();
    }

    /// state の DB に topic を 1 件入れる。
    fn seed_topic(state: &AppState, agent_id: &str, id: &str, created_at: &str, end_log_id: i64) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::insert_index_node(
            &conn,
            &IndexNodeRow {
                id: id.to_string(),
                agent_id: agent_id.to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: format!("題 {id}"),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: Some(end_log_id),
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 3,
                child_count: 0,
                token_count: 0,
                created_at: created_at.to_string(),
                updated_at: created_at.to_string(),
                short_id: Some(id.to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }

    fn set_marker(state: &AppState, agent_id: &str, ts: &str) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_last_organize_at(&conn, agent_id, ts).unwrap();
    }

    fn get_marker(state: &AppState, agent_id: &str) -> Option<String> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_last_organize_at(&conn, agent_id).unwrap()
    }

    fn set_backlog_marker(state: &AppState, agent_id: &str, cursor: &str) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_organize_backlog_cursor(&conn, agent_id, cursor).unwrap();
    }

    fn get_backlog_marker(state: &AppState, agent_id: &str) -> Option<String> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_organize_backlog_cursor(&conn, agent_id).unwrap()
    }

    /// throttle 用刻時（`organize_last_run_at`）を刻む。日次ゲートを開け閉めするテストで使う。
    fn set_last_run(state: &AppState, agent_id: &str, ts: &str) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_organize_last_run_at(&conn, agent_id, ts).unwrap();
    }

    fn get_last_run(state: &AppState, agent_id: &str) -> Option<String> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_organize_last_run_at(&conn, agent_id).unwrap()
    }

    /// 遡り側マーカーを最古（epoch）に置く。過去分（`created_at < epoch`）は 0 件になるので、
    /// 新規側だけを見たいゲート/worklist テストで遡りの影響を消せる。
    const EPOCH: &str = "1970-01-01T00:00:00Z";
    fn disable_backlog(state: &AppState, agent_id: &str) {
        set_backlog_marker(state, agent_id, EPOCH);
    }

    /// 現在から `hours` 時間前の rfc3339。
    fn hours_ago(hours: i64) -> String {
        (Utc::now() - Duration::hours(hours)).to_rfc3339()
    }

    /// 現在から `minutes` 分前の rfc3339。
    fn minutes_ago(minutes: i64) -> String {
        (Utc::now() - Duration::minutes(minutes)).to_rfc3339()
    }

    #[tokio::test]
    async fn default_off_is_zero_call_and_writes_nothing() {
        let mut state = crate::test_app_state();
        state.memory_organize = cfg(false, 3, 2);
        // ゲートが通る材料を揃えても、既定オフなら decide にすら入らない。
        set_watermark(&state, "a1", 1000);
        for i in 0..5 {
            seed_topic(&state, "a1", &format!("n{i}"), &hours_ago(1), 10 + i);
        }
        let ran = maybe_run_memory_organize(&state, "a1").await.unwrap();
        assert!(!ran, "既定オフでは起動しない");
        // decide に入っていれば初回シードでマーカーが立つはず。立っていない＝ゼロコールの証跡。
        assert_eq!(get_marker(&state, "a1"), None, "既定オフでは DB を書かない");
    }

    #[test]
    fn first_encounter_seeds_marker_and_skips() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        for i in 0..5 {
            seed_topic(&state, "a1", &format!("n{i}"), &hours_ago(1), 10 + i);
        }
        // マーカー未設定（None）。初回遭遇は両軸を now にシードしてスキップ（既存を一気に対象化しない）。
        let d = decide_organize(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        assert!(matches!(d, OrganizeDecision::Seeded));
        assert!(
            get_marker(&state, "a1").is_some(),
            "初回で新規側マーカーがシードされる"
        );
        assert!(
            get_backlog_marker(&state, "a1").is_some(),
            "初回で遡り側マーカーもシードされる（2軸）"
        );
        assert!(
            get_last_run(&state, "a1").is_some(),
            "初回で throttle 刻時もシードされる"
        );
    }

    #[test]
    fn interval_gate_blocks_when_recent() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        for i in 0..5 {
            seed_topic(&state, "a1", &format!("n{i}"), &hours_ago(1), 10 + i);
        }
        set_marker(&state, "a1", &hours_ago(1)); // 1h 前 = 24h 未満
        let d = decide_organize(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        assert!(matches!(d, OrganizeDecision::Skip("interval_not_elapsed")));
    }

    /// 間隔ゲートは**分単位**（#390）。既定 1440 分は 24 時間ゲートのまま（現行挙動を維持）で、
    /// config で分を指定するとその間隔で発火する。0 は無効化ではなく 1 分に丸める。
    #[test]
    fn interval_gate_is_minutes_with_24h_default() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48)); // 位置（新規/過去の境界）は開けておく
        disable_backlog(&state, "a1"); // 新規側の間隔ゲートだけを見る
        seed_topic(&state, "a1", "n1", &hours_ago(3), 10);
        seed_topic(&state, "a1", "n2", &hours_ago(2), 11);
        assert_eq!(
            MemoryOrganizeConfig::default().min_interval_minutes,
            1440,
            "既定は 1440 分 = 24 時間（現行挙動）"
        );

        // 既定（1440 分）: throttle 刻時が 23h 前では通らない。
        let mut c = cfg(true, 10, 2);
        assert_eq!(c.min_interval_minutes, 1440);
        set_last_run(&state, "a1", &minutes_ago(23 * 60));
        assert!(matches!(
            decide_organize(&state.db, &c, "a1").unwrap(),
            OrganizeDecision::Skip("interval_not_elapsed")
        ));

        // 10 分に詰めると、同じ刻時でも発火する。
        c.min_interval_minutes = 10;
        assert!(matches!(
            decide_organize(&state.db, &c, "a1").unwrap(),
            OrganizeDecision::Run(_)
        ));
        // 5 分前 < 10 分 → まだ弾かれる（分の刻みが効いている）。
        set_last_run(&state, "a1", &minutes_ago(5));
        assert!(matches!(
            decide_organize(&state.db, &c, "a1").unwrap(),
            OrganizeDecision::Skip("interval_not_elapsed")
        ));

        // 0 でもゲートは外れない（1 分に丸める）: 直後は弾かれ、2 分後は通る。
        c.min_interval_minutes = 0;
        set_last_run(
            &state,
            "a1",
            &(Utc::now() - Duration::seconds(10)).to_rfc3339(),
        );
        assert!(matches!(
            decide_organize(&state.db, &c, "a1").unwrap(),
            OrganizeDecision::Skip("interval_not_elapsed")
        ));
        set_last_run(&state, "a1", &minutes_ago(2));
        assert!(matches!(
            decide_organize(&state.db, &c, "a1").unwrap(),
            OrganizeDecision::Run(_)
        ));
    }

    #[test]
    fn floor_gate_blocks_when_too_few_new_topics() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        // マーカーは 48h 前（間隔は通る）。新規 topic は 1 件だけ（下限 2 未満）。
        set_marker(&state, "a1", &hours_ago(48));
        disable_backlog(&state, "a1"); // 過去分が無い日を模す（新規側の下限だけを見る）。
        seed_topic(&state, "a1", "n0", &hours_ago(1), 10);
        let d = decide_organize(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        assert!(matches!(
            d,
            OrganizeDecision::Skip("below_floor_no_backlog")
        ));
    }

    #[test]
    fn snapshot_upper_bound_excludes_topics_beyond_watermark() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 100);
        set_marker(&state, "a1", &hours_ago(48));
        disable_backlog(&state, "a1"); // 新規側の snapshot 上端だけを見る（遡りは別テスト）。
                                       // snapshot 内 2 件 + snapshot 超過 2 件。超過分は下限にも worklist にも入らない。
        seed_topic(&state, "a1", "in1", &hours_ago(3), 50);
        seed_topic(&state, "a1", "in2", &hours_ago(2), 80);
        seed_topic(&state, "a1", "out1", &hours_ago(1), 200);
        seed_topic(&state, "a1", "out2", &hours_ago(1), 300);
        let d = decide_organize(&state.db, &cfg(true, 10, 2), "a1").unwrap();
        match d {
            OrganizeDecision::Run(plan) => {
                assert_eq!(plan.new_topic_count, 2, "snapshot 超過は数えない");
                let ids: Vec<&str> = plan.worklist.iter().map(|t| t.id.as_str()).collect();
                assert_eq!(ids, vec!["in1", "in2"]);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn gate_passes_builds_bounded_worklist_and_marker_boundary() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48));
        disable_backlog(&state, "a1"); // このテストは新規側の bounded worklist と前進先を見る。
                                       // 既存タグを 1 つ用意（プロンプト同梱の語彙）。tag_topic はタグノードを新設する
                                       // （付与先 topic の実在はここでは問わない — 語彙の存在だけ用意する）。
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::tag_topic(
                &conn,
                "a1",
                "seedtopic",
                &["既存タグ".to_string()],
                "2026-08-01T00:00:00Z",
            )
            .unwrap();
        }
        // worklist 対象を 5 件（created_at 昇順）。
        for i in 1..=5 {
            let ts = (Utc::now() - Duration::hours(10 - i)).to_rfc3339();
            seed_topic(&state, "a1", &format!("n{i}"), &ts, 10 + i);
        }
        let d = decide_organize(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        match d {
            OrganizeDecision::Run(plan) => {
                assert_eq!(plan.new_topic_count, 5, "下限判定は全 5 件");
                assert_eq!(plan.worklist_size, 3, "worklist は N=3 で bounded");
                assert_eq!(plan.new_presented, 3, "全て新規（過去分は無効化済み）");
                assert_eq!(plan.backlog_presented, 0, "過去分は 0 件");
                // 遡り側の枠は新規で埋まった（budget=3 を新規が使い切った）。
                assert!(plan.backlog_marker_advance_to.is_none(), "遡り側は据え置き");
                // 新規側の前進先は提示した末尾の (created_at, id) 複合カーソル（= n3）。
                let last = plan.worklist.last().unwrap();
                let advance = plan
                    .new_marker_advance_to
                    .as_deref()
                    .expect("新規を提示したので Some");
                assert_eq!(advance, format_cursor(&last.created_at, &last.id));
                // 再解釈すると (created_at, id) に戻る（parse ⇄ format の一貫性）。
                let (ts, id) = parse_cursor(advance);
                assert_eq!(ts, last.created_at);
                assert_eq!(id, last.id);
                // 既存タグの語彙がプロンプト材料に載る。
                assert!(plan.tags.iter().any(|(n, _)| n == "既存タグ"));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    // --- 過去分の遡り消化（#365 段階3b）---

    /// 新規が枠を使い切ったら過去分は 0 件（新規優先）。
    #[test]
    fn new_fills_budget_leaves_no_room_for_backlog() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48)); // 間隔は通る
        set_backlog_marker(&state, "a1", &hours_ago(240)); // 過去分の境界（10日前）
                                                           // 新規 3 件（境界 48h より後）。
        for i in 1..=3 {
            seed_topic(&state, "a1", &format!("n{i}"), &hours_ago(10 - i), 50 + i);
        }
        // 過去分も 3 件（境界 240h より古い）。
        for i in 1..=3 {
            seed_topic(
                &state,
                "a1",
                &format!("old{i}"),
                &hours_ago(300 - i),
                10 + i,
            );
        }
        // budget=2 を新規が使い切る。
        let d = decide_organize(&state.db, &cfg(true, 2, 2), "a1").unwrap();
        match d {
            OrganizeDecision::Run(plan) => {
                assert_eq!(plan.new_presented, 2, "新規が budget=2 を使い切る");
                assert_eq!(plan.backlog_presented, 0, "枠が無いので過去分は 0 件");
                assert_eq!(plan.worklist_size, 2);
                assert!(plan.backlog_marker_advance_to.is_none(), "遡り側は据え置き");
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 枠が余ったら過去分で埋める（新規 → 過去分の順 / 合計 <= max_topics）。
    #[test]
    fn backlog_fills_leftover_after_new() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48));
        set_backlog_marker(&state, "a1", &hours_ago(240));
        // 新規 2 件（昇順 n1<n2）。
        seed_topic(&state, "a1", "n1", &hours_ago(5), 50);
        seed_topic(&state, "a1", "n2", &hours_ago(3), 60);
        // 過去分 4 件（270/280/290/300h 前）。
        for h in [270, 280, 290, 300] {
            seed_topic(&state, "a1", &format!("old{h}"), &hours_ago(h), 10 + h);
        }
        // budget=5: 新規 2 + 過去分 3（残り 1 は次回）。
        let d = decide_organize(&state.db, &cfg(true, 5, 2), "a1").unwrap();
        match d {
            OrganizeDecision::Run(plan) => {
                assert_eq!(plan.new_presented, 2);
                assert_eq!(plan.backlog_presented, 3, "残り枠 3 を過去分で埋める");
                assert_eq!(plan.worklist_size, 5, "合計は max_topics 以下");
                // 提示順は新規 → 過去分。先頭 2 件が新規。
                let ids: Vec<&str> = plan.worklist.iter().map(|t| t.id.as_str()).collect();
                assert_eq!(&ids[0..2], &["n1", "n2"], "新規が先");
                // 過去分は遡り（降順）: 270 → 280 → 290。
                assert_eq!(&ids[2..5], &["old270", "old280", "old290"]);
                // 遡り側マーカーは提示した中で最も古い old290 へ進む（old300 は次回）。
                let oldest = plan.worklist.last().unwrap();
                assert_eq!(oldest.id, "old290");
                assert_eq!(
                    plan.backlog_marker_advance_to.as_deref(),
                    Some(format_cursor(&oldest.created_at, &oldest.id).as_str())
                );
                assert_eq!(plan.backlog_remaining, 4, "遡り残数は提示前の 4 件");
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 新規が無い日でも過去分が進む（下限は新規側だけを塞ぐ）。
    #[test]
    fn no_new_day_still_progresses_backlog() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48)); // 間隔は通る / 新規は 0 件
        set_backlog_marker(&state, "a1", &hours_ago(240));
        for h in [300, 290, 280] {
            seed_topic(&state, "a1", &format!("old{h}"), &hours_ago(h), 10 + h);
        }
        // 新規 0（下限 2 未満）だが過去分があるので発火する。
        let d = decide_organize(&state.db, &cfg(true, 5, 2), "a1").unwrap();
        match d {
            OrganizeDecision::Run(plan) => {
                assert_eq!(plan.new_topic_count, 0, "新規は無い");
                assert_eq!(plan.new_presented, 0);
                assert_eq!(plan.backlog_presented, 3, "過去分だけで発火・進行する");
                // 新規側マーカーは**据え置き**（新規 0 件では壁時計 now へ飛ばさない / 恒久ロス防止）。
                assert!(
                    plan.new_marker_advance_to.is_none(),
                    "新規 0 件では新規側を進めない（None）"
                );
                // 遡り側は進む。日次 throttle は throttle 刻時（run_at）が担う。
                assert!(plan.backlog_marker_advance_to.is_some());
                assert!(
                    plan.run_at.parse::<DateTime<Utc>>().is_ok(),
                    "throttle 刻時は now"
                );
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 遡りが先頭（最古）に到達したら止まる: 過去分 0 かつ新規 0 ならスキップ（無限に走らない）。
    #[test]
    fn backlog_head_reached_and_no_new_skips() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48));
        set_backlog_marker(&state, "a1", EPOCH); // 遡りカーソルが先頭 → 過去分 0
                                                 // 過去 topic はあるが、全て境界（epoch）より新しい＝もう遡る先が無い。
        for h in [300, 290] {
            seed_topic(&state, "a1", &format!("old{h}"), &hours_ago(h), 10 + h);
        }
        let d = decide_organize(&state.db, &cfg(true, 5, 2), "a1").unwrap();
        assert!(matches!(
            d,
            OrganizeDecision::Skip("below_floor_no_backlog")
        ));
    }

    /// 同じ topic を毎日拾い直さない（タグを付けなかったものも含めて / 位置マーカーで進む）。
    #[test]
    fn backlog_does_not_repick_presented_topics_across_runs() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48));
        set_backlog_marker(&state, "a1", &hours_ago(240));
        for h in [300, 290, 280] {
            seed_topic(&state, "a1", &format!("old{h}"), &hours_ago(h), 10 + h);
        }
        // run1: budget=2 → 遡り降順で old280, old290 を提示（タグ付けは一切しない）。
        let plan1 = match decide_organize(&state.db, &cfg(true, 2, 2), "a1").unwrap() {
            OrganizeDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let ids1: Vec<String> = plan1.worklist.iter().map(|t| t.id.clone()).collect();
        assert_eq!(ids1, vec!["old280", "old290"]);
        // clean 完了として前進させる（タグは付けていない）。run1 は過去分だけなので新規側は
        // 据え置き、遡り側と throttle 刻時が進む。
        {
            let conn = state.db.lock().unwrap();
            advance_markers(&conn, "a1", &plan1, true).unwrap();
        }
        // 翌日を模す: throttle 刻時を 48h 前へ戻して日次ゲートを開ける（遡りカーソルは
        // run1 の前進位置のまま = 別軸なので影響しない）。
        set_last_run(&state, "a1", &hours_ago(48));
        // run2: 次は old300 だけ（提示済みの old280/old290 は二度と出ない）。
        let plan2 = match decide_organize(&state.db, &cfg(true, 2, 2), "a1").unwrap() {
            OrganizeDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let ids2: Vec<String> = plan2.worklist.iter().map(|t| t.id.clone()).collect();
        assert_eq!(ids2, vec!["old300"], "提示済みを拾い直さない（未タグでも）");
    }

    /// partial（clean でない）では 2 軸マーカーも throttle 刻時も進めない。clean では全て進む。
    #[test]
    fn partial_run_does_not_advance_markers() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48));
        set_backlog_marker(&state, "a1", &hours_ago(240));
        set_last_run(&state, "a1", &hours_ago(48));
        // 新規 2 件 + 過去分（両軸が進む計画にして、どちらも partial で止まることを見る）。
        seed_topic(&state, "a1", "n1", &hours_ago(5), 50);
        seed_topic(&state, "a1", "n2", &hours_ago(3), 60);
        for h in [300, 290] {
            seed_topic(&state, "a1", &format!("old{h}"), &hours_ago(h), 10 + h);
        }
        let plan = match decide_organize(&state.db, &cfg(true, 5, 2), "a1").unwrap() {
            OrganizeDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        assert!(plan.new_marker_advance_to.is_some(), "新規を提示（Some）");
        assert!(
            plan.backlog_marker_advance_to.is_some(),
            "過去分を提示（Some）"
        );
        let new_before = get_marker(&state, "a1");
        let backlog_before = get_backlog_marker(&state, "a1");
        let run_before = get_last_run(&state, "a1");
        // partial: clean=false → 何も進めない。
        {
            let conn = state.db.lock().unwrap();
            advance_markers(&conn, "a1", &plan, false).unwrap();
        }
        assert_eq!(
            get_marker(&state, "a1"),
            new_before,
            "partial で新規側は不変"
        );
        assert_eq!(
            get_backlog_marker(&state, "a1"),
            backlog_before,
            "partial で遡り側は不変"
        );
        assert_eq!(
            get_last_run(&state, "a1"),
            run_before,
            "partial で throttle 刻時も不変"
        );
        // clean=true → 2 軸 + throttle が計画どおり進む。
        {
            let conn = state.db.lock().unwrap();
            advance_markers(&conn, "a1", &plan, true).unwrap();
        }
        assert_eq!(
            get_marker(&state, "a1"),
            plan.new_marker_advance_to,
            "clean で新規側が進む"
        );
        assert_eq!(
            get_backlog_marker(&state, "a1"),
            plan.backlog_marker_advance_to,
            "clean で遡り側が進む"
        );
        assert_eq!(
            get_last_run(&state, "a1").as_deref(),
            Some(plan.run_at.as_str()),
            "clean で throttle 刻時が now へ進む"
        );
    }

    /// 回帰（#365 レビュー / #364 と同型）: 非トランザクションなビルドが途中失敗して
    /// `end_log_id > watermark`（snapshot 外）の topic を残した状態で、過去分により整理ランが
    /// 発火し clean 完了しても、その topic を**新規側が恒久ロスしない**こと。
    ///
    /// 初版（新規 0 件で新規側を壁時計 `now` へ飛ばす）ではこのテストが落ちる:
    /// `new_marker_advance_to` が `Some(now)` になり（`is_none()` で失敗）、仮に進めれば run2 で
    /// stale が新規側カーソルに追い越されて worklist から消える。
    #[test]
    fn stale_topic_beyond_watermark_not_lost_on_zero_new_day() {
        let state = crate::test_app_state();
        // 位置・throttle を 48h 前に（間隔は通る）。遡り境界は 10 日前。
        set_marker(&state, "a1", &hours_ago(48));
        set_last_run(&state, "a1", &hours_ago(48));
        set_backlog_marker(&state, "a1", &hours_ago(240));
        // ビルドが step5(topic commit) 後 step7(watermark 更新) 前に失敗した状態を模す:
        // topic は commit 済みだが end_log_id=200 > watermark=100（snapshot 外）。created_at は現在より前。
        set_watermark(&state, "a1", 100);
        seed_topic(&state, "a1", "stale", &hours_ago(2), 200);
        // 過去分（遡り発火のトリガ）。end_log_id は watermark(100) 内にして snapshot に入れる
        // （stale だけが snapshot 外という状況を作る）。
        seed_topic(&state, "a1", "old300", &hours_ago(300), 10);
        seed_topic(&state, "a1", "old290", &hours_ago(290), 11);
        // run1: stale は snapshot 外で除外 → 新規 0。過去分で発火（下限 1 で run2 の 1 件でも発火）。
        let plan1 = match decide_organize(&state.db, &cfg(true, 5, 1), "a1").unwrap() {
            OrganizeDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        assert_eq!(plan1.new_presented, 0, "stale は snapshot 外なので新規 0");
        assert!(
            plan1.new_marker_advance_to.is_none(),
            "新規 0 では新規側を壁時計へ飛ばさない（恒久ロス防止の肝）"
        );
        {
            let conn = state.db.lock().unwrap();
            advance_markers(&conn, "a1", &plan1, true).unwrap();
        }
        // ビルド再開で watermark が追いつく（stale が snapshot 内へ）。翌日を模す。
        set_watermark(&state, "a1", 300);
        set_last_run(&state, "a1", &hours_ago(48));
        // run2: stale が新規側で拾える（恒久ロスしない）。
        let plan2 = match decide_organize(&state.db, &cfg(true, 5, 1), "a1").unwrap() {
            OrganizeDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let ids: Vec<&str> = plan2.worklist.iter().map(|t| t.id.as_str()).collect();
        assert!(
            ids.contains(&"stale"),
            "snapshot 外だった topic が新規側から恒久ロスした: {ids:?}"
        );
    }

    // --- プロンプト組み立て ---

    fn topic(id: &str, short: &str, title: &str, summary: &str) -> IndexNodeRow {
        IndexNodeRow {
            id: id.to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: "session_log".to_string(),
            title: title.to_string(),
            summary: summary.to_string(),
            start_log_id: None,
            end_log_id: Some(10),
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 3,
            child_count: 0,
            token_count: 0,
            created_at: "2026-08-01T00:00:00Z".to_string(),
            updated_at: "2026-08-01T00:00:00Z".to_string(),
            short_id: Some(short.to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        }
    }

    fn plan_with(worklist: Vec<IndexNodeRow>, tags: Vec<(String, i64)>) -> OrganizePlan {
        let worklist_size = worklist.len();
        OrganizePlan {
            persona_name: "テスト太郎".to_string(),
            personality: Some("あなたは慎重で記録魔です。".to_string()),
            instructions: String::new(),
            snapshot_log_id: 100,
            worklist,
            worklist_size,
            new_topic_count: worklist_size as i64,
            new_presented: worklist_size,
            backlog_presented: 0,
            backlog_remaining: 0,
            tags,
            new_marker_advance_to: Some("2026-08-02T00:00:00Z".to_string()),
            backlog_marker_advance_to: None,
            run_at: "2026-08-02T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn system_prompt_includes_persona_worklist_tags_and_tool_names() {
        let plan = plan_with(
            vec![
                topic("id1", "t42", "送金の設計", "手数料の扱いを議論した"),
                topic("id2", "t43", "Nostr 連携", "リレー選定の話"),
            ],
            vec![("お金".to_string(), 3), ("技術".to_string(), 7)],
        );
        let sp = build_system_prompt(&plan);
        // 人格が載る
        assert!(sp.contains("あなたは慎重で記録魔です。"));
        // worklist が短縮IDつきで載る
        assert!(sp.contains("[t42] 送金の設計 — 手数料の扱いを議論した"));
        assert!(sp.contains("[t43] Nostr 連携"));
        // 現行タグが件数つきで載る
        assert!(sp.contains("お金（3件）"));
        assert!(sp.contains("技術（7件）"));
        // タグ道具の名が載る（発散させないための道具の明示）
        assert!(sp.contains("tag_topic"));
        assert!(sp.contains("merge_tags"));
        // 新規と過去分が混ざりうる旨の明示（#365）。
        assert!(sp.contains("過去の分が混ざっています"));
    }

    #[test]
    fn system_prompt_handles_no_tags() {
        let plan = plan_with(
            vec![topic("id1", "t1", "初めての記憶", "最初の一歩")],
            vec![],
        );
        let sp = build_system_prompt(&plan);
        assert!(sp.contains("まだタグはありません"));
    }

    #[test]
    fn cursor_roundtrips_and_tolerates_bare_timestamp() {
        // format → parse で往復する。
        let m = format_cursor("2026-08-03T00:00:00Z", "topic-a1-s-000");
        assert_eq!(m, "2026-08-03T00:00:00Z|topic-a1-s-000");
        assert_eq!(
            parse_cursor(&m),
            (
                "2026-08-03T00:00:00Z".to_string(),
                "topic-a1-s-000".to_string()
            )
        );
        // `|` 無し（初回シードの素の刻時 / 旧形式）は id 空で解釈する。
        assert_eq!(
            parse_cursor("2026-08-03T00:00:00Z"),
            ("2026-08-03T00:00:00Z".to_string(), String::new())
        );
    }

    #[test]
    fn topic_line_omits_dash_when_summary_empty() {
        let line = format_topic_line(&topic("id1", "t1", "無要約", ""));
        assert_eq!(line, "- [t1] 無要約");
    }

    // --- 整理ランのツール許可リスト（#368 / 実測）---

    /// MCP スロット検証用: `mcp__` 名前空間の外部ツールを 1 つ定義するモック。
    /// 整理ランは depth 0 なので本番でも MCP が注入されうる。許可リストが MCP スロットも
    /// 覆うことを実測する。
    struct MockMcpSlot;

    #[async_trait::async_trait]
    impl opencrab_gateway::GatewayActions for MockMcpSlot {
        fn definitions(&self) -> Vec<opencrab_gateway::GatewayActionDef> {
            vec![opencrab_gateway::GatewayActionDef {
                name: "mcp__ext__send".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "external send".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }]
        }
        async fn execute(
            &self,
            name: &str,
            _args: &serde_json::Value,
            _ctx: &opencrab_gateway::GatewayCallContext,
        ) -> opencrab_gateway::GatewayActionResult {
            opencrab_gateway::GatewayActionResult {
                success: true,
                data: Some(serde_json::json!({ "reached": name })),
                error: None,
            }
        }
    }

    /// 整理ランが**実際に受け取る合成 executor**を、`process::run_agent_response` の run
    /// 構築と同じ配線で組む（dispatcher core + config 駆動の execute_shell + gateway own =
    /// `SystemGatewayActions` + MCP スロット）。`with_allowlist=true` で
    /// `ORGANIZE_ALLOWED_TOOLS` を載せる（整理ランと同じ）。
    fn build_organize_executor(
        state: &AppState,
        with_allowlist: bool,
    ) -> opencrab_actions::BridgedExecutor {
        // dispatcher: core アクション + config 駆動の execute_shell。
        let mut dispatcher = opencrab_actions::ActionDispatcher::new();
        let tools_cfg = opencrab_actions::tools::ToolsConfig {
            enabled: true,
            shell: Some(opencrab_actions::tools::ShellToolConfig {
                enabled: true,
                allowed_commands: vec!["echo".to_string()],
                ..Default::default()
            }),
        };
        opencrab_actions::register_tools_from_config(&tools_cfg, &mut dispatcher);

        let ws_path = std::env::temp_dir().join(format!("organize-tools-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws_path).unwrap();
        let workspace = opencrab_core::workspace::Workspace::from_root(&ws_path).unwrap();

        // 整理ランと同じ caller=Owner。放置すると Owner の全ツールが届く前提を再現する。
        let ctx = opencrab_actions::ActionContext {
            caller: CallerIdentity::Owner,
            agent_id: "a1".to_string(),
            agent_name: "a1".to_string(),
            session_id: Some("sleep-organize-a1-1".to_string()),
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

        // gateway own = SystemGatewayActions（configure_* / nostr_run / spawn_subtask を own で持つ）。
        // depth 0 なので本番でもこの合成 gateway がそのまま渡る（sub-engine の絞りは通らない）。
        let system_actions: std::sync::Arc<dyn opencrab_gateway::GatewayActions> =
            std::sync::Arc::new(crate::system_actions::SystemGatewayActions::new(
                state.clone(),
                None,
                None,
                None,
            ));

        let mut bridged = opencrab_actions::BridgedExecutor::new(dispatcher, ctx)
            .with_depth(0)
            .with_gateway_actions(system_actions)
            .with_mcp_actions(std::sync::Arc::new(MockMcpSlot));
        if with_allowlist {
            bridged = bridged.with_tool_allowlist(Some(
                ORGANIZE_ALLOWED_TOOLS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ));
        }
        bridged
    }

    /// 整理ランに渡すツールセットは、記憶の読み取り・タグ操作・終了宣言だけ。
    /// **眠っている間に外へ手が出る**ツール（`execute_shell` / `nostr_run` / `spawn_subtask` /
    /// `ws_write` / `ws_delete` / `configure_*` / `update_instructions`）は 3 経路すべてで塞ぐ:
    ///   経路1: `run_allows` 相当（許可リスト定数の内容）
    ///   経路2: `list_tools`（可視性）
    ///   経路3: `dispatch`（実行）
    #[tokio::test]
    async fn organize_run_tool_allowlist_excludes_outward_tools() {
        // list_tools / execute は ActionExecutor トレイト経由。
        use opencrab_core::ActionExecutor;
        let state = crate::test_app_state();

        // 眠っている間に外へ手が出る／状態を書き換えるツール（全スロットにまたがる）。
        // nostr_run は露出撤去済み（返信は say 一本 / #840）なので外向きツール表からは外す
        // （own 定義に無く「許可リスト無しでは届くはず」の対照が成り立たない）。
        // #654: configure_nostr の定義は nostr feature 依存（#651）。off では定義が無く対照が
        // 空論になるので、期待値も同じ cfg で組む（feature off でも他の外向きツールは全経路で
        // 塞がることを引き続き固定する）。nostr off では下の push が cfg で消え mut が不要になる。
        #[cfg_attr(not(feature = "nostr"), allow(unused_mut))]
        let mut forbidden = vec![
            "execute_shell",          // dispatcher（config 駆動）
            "ws_write",               // dispatcher core
            "ws_delete",              // dispatcher core
            "update_instructions",    // dispatcher core（owner 専用の指示書書き換え）
            "spawn_subtask",          // gateway own
            "configure_llm_provider", // gateway own
            "configure_self",         // gateway own
            "configure_mcp_server",   // gateway own
            "mcp__ext__send",         // MCP スロット
        ];
        #[cfg(feature = "nostr")]
        {
            forbidden.push("configure_nostr"); // gateway own
        }
        // 整理に要る読み取り・タグ・終了宣言。
        let allowed = [
            "browse_memory_index",
            "search_memory_index",
            "retrieve_memory_nodes",
            "search_my_history",
            "tag_topic",
            "untag_topic",
            "merge_tags",
            "declare_done",
        ];

        // 経路1: 許可リスト定数そのものの内容。
        for f in forbidden.iter() {
            assert!(
                !ORGANIZE_ALLOWED_TOOLS.contains(f),
                "許可リストに外向きツール {f} が入っている"
            );
        }
        for a in allowed {
            assert!(
                ORGANIZE_ALLOWED_TOOLS.contains(&a),
                "許可リストに {a} が無い（整理に必要）"
            );
        }

        // --- 対照: 許可リスト無し（None）なら Owner の全ツールが届く（危険の再現） ---
        let unrestricted = build_organize_executor(&state, false);
        // #923: allowlist の可視性は narrowing 前の policy＋allowlist 層で検証する（list_tools は
        // depth0 で常時集合に絞るため、allowlist 契約は effective_tool_definitions で見る）。
        let base: Vec<String> = unrestricted
            .effective_tool_definitions()
            .into_iter()
            .map(|t| t.definition.name)
            .collect();
        for f in forbidden.iter() {
            assert!(
                base.contains(&f.to_string()),
                "許可リスト無しでは {f} が届くはず（許可リストが効いている証跡の対照）: {base:?}"
            );
        }

        // --- 整理ラン（許可リスト有り） ---
        let executor = build_organize_executor(&state, true);

        // 経路2: 可視性（policy＋allowlist 層）。許可外は 1 つも出ない。
        let visible: Vec<String> = executor
            .effective_tool_definitions()
            .into_iter()
            .map(|t| t.definition.name)
            .collect();
        for f in forbidden.iter() {
            assert!(
                !visible.contains(&f.to_string()),
                "整理ランの list_tools に外向きツール {f} が出ている: {visible:?}"
            );
        }
        for a in allowed {
            assert!(
                visible.contains(&a.to_string()),
                "整理ランの list_tools に {a} が無い: {visible:?}"
            );
        }

        // 経路3: dispatch（実行）。許可外は構造的拒否で、実装へは届かない。
        for f in forbidden.iter().copied() {
            let r = executor.execute(f, &serde_json::json!({})).await;
            assert!(!r.success, "整理ランで {f} の実行が成功してはならない");
            let err = r.error.unwrap_or_default();
            assert!(
                err.starts_with(opencrab_actions::REJECTION_CODE_PREFIX),
                "整理ランで {f} は構造的拒否であるべき: {err}"
            );
            assert!(
                r.data.get("reached").is_none(),
                "整理ランで {f} が実装へ届いてはならない"
            );
        }

        // 実測の記録（推定でなく実際に受け取るツール名。テスト出力に残す）。
        let mut dump = visible.clone();
        dump.sort();
        eprintln!("[#368] 整理ランが実際に受け取るツール: {dump:?}");
    }

    // --- 本番のラン構築を通さない全経路テスト（#370）---
    //
    // `run_organize` は `AppState` を受け取らず、1 ターンを回す口（`OrganizeTurnRunner`）だけを
    // 外から受ける。テストは結果を差し替えるフェイクを渡す。フェイクは `run_agent_response` を
    // 呼ばないので、webhook も gateway も MCP も LLM も**一切構築されない**（構造的に sleep の依存に
    // 入らない = 隔離実験が本番へ飛んだ #370 の再発を症状封じでなく構造で防ぐ）。ゲート → 実行 →
    // clean/partial 判定 → マーカー前進/据え置き → 監査、までを LLM ゼロコールで検証する。

    enum FakeOutcome {
        Completed,
        StoppedByLimit,
        Error,
    }

    /// テスト用の [`OrganizeTurnRunner`]。受け取った `RunRequest` の要点を記録し、設定した結果を
    /// 返すだけ。何も構築しない（外向きの口は一切現れない）。
    struct FakeRunner {
        outcome: FakeOutcome,
        calls: AtomicUsize,
        captured: std::sync::Mutex<Option<CapturedReq>>,
    }

    /// フェイクが観測した `RunRequest` の要点（本番配線が保たれているかの検証用）。
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
            last_posting_utterance_id: None,
            last_generation_had_continuation_speech: false,
            xml_fallback_parses: 0,
        }
    }

    /// ゲートが「新規を提示して発火」する状態に DB を整える（新規側だけを見る）。
    fn seed_passing_gate(state: &AppState) {
        set_watermark(state, "a1", 1000);
        set_marker(state, "a1", &hours_ago(48)); // last_organize_at（新規/過去の境界）
        set_backlog_marker(state, "a1", EPOCH); // 過去分は out（新規側だけ見る）
        set_last_run(state, "a1", &hours_ago(48)); // 日次 throttle を開ける（48h > 24h）
                                                   // 新規（境界より後）を下限（min_new）以上そろえる。順は created_at ASC で n1 → n2。
        seed_topic(state, "a1", "n1", &hours_ago(5), 50);
        seed_topic(state, "a1", "n2", &hours_ago(3), 60);
    }

    /// context="sleep" の最新監査 message を JSON で返す。
    fn latest_sleep_audit(state: &AppState) -> Option<serde_json::Value> {
        let conn = state.db.lock().unwrap();
        let rows = opencrab_db::queries::list_agent_logs(&conn, Some("a1"), None, 10).ok()?;
        rows.into_iter()
            .find(|r| r.context == "sleep")
            .and_then(|r| serde_json::from_str(&r.message).ok())
    }

    /// clean 完了: マーカーが前進し、監査に completed が残る。本番のラン構築は一切通らない。
    #[tokio::test]
    async fn clean_run_advances_markers_without_production_build() {
        let state = crate::test_app_state();
        seed_passing_gate(&state);
        let fake = FakeRunner::new(FakeOutcome::Completed);

        let ran = run_organize(
            &state.db,
            &cfg(true, 5, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();

        assert!(ran, "ゲート通過 → 起動して true");
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            1,
            "ターンは 1 回だけ回る"
        );
        // clean → 新規側マーカーが提示末尾（最新 = n2）へ前進する。
        let marker = get_marker(&state, "a1").expect("新規側マーカー");
        assert!(
            marker.contains("n2"),
            "clean で新規側マーカーが提示末尾(n2)へ前進する: {marker}"
        );
        let audit = latest_sleep_audit(&state).expect("監査ログが書かれる");
        assert_eq!(audit["outcome"], "completed");
        assert_eq!(audit["marker_advanced"], true);
    }

    /// partial（ターン上限）: マーカーは据え置き、監査は stopped_by_limit。差し替えた結果だけで
    /// partial 経路を検証できる（LLM もラン構築も不要）。
    #[tokio::test]
    async fn stopped_by_limit_run_holds_markers() {
        let state = crate::test_app_state();
        seed_passing_gate(&state);
        let before = get_marker(&state, "a1");
        let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);

        let ran = run_organize(
            &state.db,
            &cfg(true, 5, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();

        assert!(ran, "起動はした（partial でも true）");
        assert_eq!(
            get_marker(&state, "a1"),
            before,
            "partial（ターン上限）では新規側マーカーを進めない"
        );
        let audit = latest_sleep_audit(&state).expect("監査ログが書かれる");
        assert_eq!(audit["outcome"], "stopped_by_limit");
        assert_eq!(audit["marker_advanced"], false);
    }

    /// run 自体が Err: マーカーは据え置き、監査は error。エラー経路も差し替えで検証できる。
    #[tokio::test]
    async fn errored_run_holds_markers() {
        let state = crate::test_app_state();
        seed_passing_gate(&state);
        let before = get_marker(&state, "a1");
        let fake = FakeRunner::new(FakeOutcome::Error);

        let ran = run_organize(
            &state.db,
            &cfg(true, 5, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();

        assert!(ran, "起動はした（error でも true）");
        assert_eq!(
            get_marker(&state, "a1"),
            before,
            "error では新規側マーカーを進めない"
        );
        let audit = latest_sleep_audit(&state).expect("監査ログが書かれる");
        assert_eq!(audit["outcome"], "error");
    }

    /// 口に渡る `RunRequest` が本番配線を保っていること（#368/#369 を壊していない）:
    /// gateway="sleep" / caller=Owner / ツール許可リスト=ORGANIZE_ALLOWED_TOOLS /
    /// 送信経路（gateway_actions）なし。
    #[tokio::test]
    async fn run_request_carries_expected_wiring() {
        let state = crate::test_app_state();
        seed_passing_gate(&state);
        let fake = FakeRunner::new(FakeOutcome::Completed);

        run_organize(
            &state.db,
            &cfg(true, 5, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();

        let captured = fake.captured.lock().unwrap();
        let req = captured.as_ref().expect("ターンが回れば記録される");
        assert_eq!(req.gateway, "sleep", "RuntimeInfo の gateway 名");
        assert!(req.caller_is_owner, "caller は Owner");
        assert!(
            !req.has_gateway_actions,
            "送信経路（会話への出口）は渡さない"
        );
        let expected: Vec<String> = ORGANIZE_ALLOWED_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            req.tool_allowlist.as_ref(),
            Some(&expected),
            "#369 のツール許可リストがそのまま載る"
        );
        // #393: 整備作業のターンは生ログに残さない（残すと宣言ランの材料になる）。
        assert!(
            !req.persist_turn_logs,
            "整理ランのターンは memory_sessions に記録しない"
        );
    }

    /// 既定オフ: ゲートに入る前にゼロコールで返る。**口（LLM）は 1 度も呼ばれない**。
    #[tokio::test]
    async fn disabled_never_calls_the_runner() {
        let state = crate::test_app_state();
        // ゲートが通る材料を揃えても、既定オフなら口を呼ばない。
        seed_passing_gate(&state);
        let fake = FakeRunner::new(FakeOutcome::Completed);

        let ran = run_organize(
            &state.db,
            &cfg(false, 5, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();

        assert!(!ran, "既定オフでは起動しない");
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            0,
            "既定オフでは 1 ターンも回さない（LLM ゼロコール）"
        );
    }
}
