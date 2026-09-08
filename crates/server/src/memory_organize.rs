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
    include!("memory_organize/tests/mod.rs");
}
