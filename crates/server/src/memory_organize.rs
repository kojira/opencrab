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
use crate::AppState;
use opencrab_actions::{CallerIdentity, RunRequest};
use opencrab_db::queries::IndexNodeRow;

/// 1 回の worklist に載せる 1 topic あたりの要約の最大文字数（プロンプト肥大の抑制）。
/// 実測平均は要約 102 字（#313）。振れ幅を吸収しつつ上限を持たせる。
const SUMMARY_MAX_CHARS: usize = 240;

/// このエージェントの整理ランを（ゲートを満たせば）実行する。
///
/// 戻り値: 整理ラン（LLM）を実際に起動したら `true`。既定オフ・ゲート未達・初回シードは
/// `false`（＝ LLM ゼロコール）。
pub async fn maybe_run_memory_organize(state: &AppState, agent_id: &str) -> anyhow::Result<bool> {
    let cfg = state.memory_organize.clone();
    // 既定オフ: ここで即 return する。RunRequest も DB 書き込みも一切しない（ゼロコール）。
    if !cfg.enabled {
        return Ok(false);
    }

    // --- ゲート判定 + worklist 組み立て（DB 読みのみ。ロックは await を跨がない）---
    let plan = match decide_organize(state, agent_id, &cfg)? {
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
        &state.index_build_inflight,
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
    let req = RunRequest::new(
        agent_id.to_string(),
        plan.persona_name.clone(),
        session_id.clone(),
        system_prompt,
        conversation,
        // RuntimeInfo の gateway 名。監査 context と揃えて "sleep"。
        "sleep",
        CallerIdentity::Owner,
    );

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(cfg.timeout_secs.max(1)),
        crate::process::run_agent_response(state, req),
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

    // --- 前進（前進のみ / 残りは次回 / 2 軸を独立に）---
    // clean 完了時のみ、提示した各軸のマーカーを末尾の (created_at, id) カーソルへ進める。
    //  - **新規側**（`last_organize_at` / 昇順）: 提示末尾より後を次回の対象に。新規が 0 件でも
    //    `now` まで進めるので、過去分だけの日でも日次ゲートが throttle され続ける。
    //  - **遡り側**（`organize_backlog_cursor` / 降順）: 過去分を提示したときだけ、提示した中で
    //    最も古い (created_at, id) より古い分を次回の対象に。**索引ビルドは 1 パスの全 topic に
    //    同一 created_at を刻むため、created_at 単体でなく id を副キーに持つカーソルにしている**
    //    （降順側でも同着群の残余を取りこぼさない / #364 blocker と同型）。
    // partial（timeout / ターン上限 / エラー）ではどちらのマーカーも進めない。タグ付与は PK 冪等
    // （`assign_topic_to_category`）なので、同じ範囲を次回に再挑戦しても重複しない。
    {
        let conn = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
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
            "new_marker_advanced_to": if clean { Some(plan.new_marker_advance_to.clone()) } else { None },
            "backlog_marker_advanced_to": if clean { plan.backlog_marker_advance_to.clone() } else { None },
            "cost": { "latency_ms": latency_ms },
        });
        let conn = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
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
    /// clean 完了時に**新規側マーカー**へ刻む複合カーソル `"{created_at}|{id}"`。
    /// 新規を提示したなら提示末尾（最新）の topic、新規が 0 件なら `now`（＝新規側は
    /// 今回時点まで追いついた）。後者により、過去分だけを消化する日でもマーカーが `now`
    /// へ進み、日次ゲート（`now - last_organize_at`）が正しく throttle される。
    new_marker_advance_to: String,
    /// clean 完了時に**遡り側マーカー**へ刻む複合カーソル。過去分を提示したときのみ
    /// `Some`（提示末尾＝提示した中で最も古い `(created_at, id)`）。0 件なら `None`（据え置き）。
    backlog_marker_advance_to: Option<String>,
}

/// ゲート判定の結果。
#[derive(Debug)]
enum OrganizeDecision {
    /// 発火しない（理由つき）。
    Skip(&'static str),
    /// 初回遭遇: `now` をマーカーにシードして今回はスキップ（既存の全 topic を一気に
    /// 対象化しない）。次回以降、シード後に増えた topic が下限に達したら発火する。
    Seeded,
    /// 発火する。
    Run(OrganizePlan),
}

/// ゲート（日次 + 下限）を判定し、通れば worklist と人格を積んだ計画を返す。
///
/// DB 読みのみ（初回シードの 1 write を除く）。ロックは関数内で完結し、`run_agent_response`
/// の await を跨いで保持しない。
fn decide_organize(
    state: &AppState,
    agent_id: &str,
    cfg: &MemoryOrganizeConfig,
) -> anyhow::Result<OrganizeDecision> {
    let now = Utc::now();
    let conn = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("db lock: {e}"))?;

    // ゲート1: 日次 + 初回シード。
    let last_at = opencrab_db::queries::get_last_organize_at(&conn, agent_id)?;
    let Some(last_at) = last_at else {
        // 初回遭遇: **両軸のマーカーを now にシード**して終了（既存履歴を「新規」に数えない）。
        // id 部を持たない素の刻時でよい（次回 parse_cursor が `|` 無しを (now, "") と解釈する）。
        // 新規側は now より後を「新規」に、遡り側は now より前を「過去分」に分ける境界になる。
        let now_s = now.to_rfc3339();
        opencrab_db::queries::set_last_organize_at(&conn, agent_id, &now_s)?;
        opencrab_db::queries::set_organize_backlog_cursor(&conn, agent_id, &now_s)?;
        return Ok(OrganizeDecision::Seeded);
    };
    // 新規側カーソルを (created_at, id) に分解する。日次ゲートは created_at 部だけを使う。
    // 新規側マーカーは clean 完了ごとに（新規が無い日でも）`now` 付近まで前進するので、
    // これを throttle の基準に使える（過去分だけ進む日も含めて日次に保たれる / #365）。
    let (since_ts, since_id) = parse_cursor(&last_at);
    let elapsed = since_ts
        .parse::<DateTime<Utc>>()
        .map(|dt| now.signed_duration_since(dt))
        .unwrap_or_else(|_| Duration::zero());
    if elapsed < Duration::hours(cfg.min_interval_hours.max(1)) {
        return Ok(OrganizeDecision::Skip("interval_not_elapsed"));
    }

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
    // 新規側マーカー前進先 = 提示末尾（最新）の (created_at, id)。新規が 0 件なら `now`
    // （新規側は今回時点まで追いついた）。並び順が `created_at ASC, id ASC` なので末尾が最大。
    // 同着 created_at 群を N で切っても id 副キーで残余を次回へ引き継ぐ（取りこぼさない）。
    let new_marker_advance_to = new_worklist
        .last()
        .map(|r| format_cursor(&r.created_at, &r.id))
        .unwrap_or_else(|| now.to_rfc3339());

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
    Ok(OrganizeDecision::Run(OrganizePlan {
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
    }))
}

/// clean 完了時のみ 2 軸のマーカーを前進させる（partial では**進めない** / #364 と同じ流儀）。
///
/// - 新規側（`last_organize_at`）: 常に前進（新規 0 件の日でも `now` まで）。日次 throttle の基準。
/// - 遡り側（`organize_backlog_cursor`）: 過去分を提示したときだけ前進（先頭到達なら据え置き＝止まる）。
fn advance_markers(
    conn: &rusqlite::Connection,
    agent_id: &str,
    plan: &OrganizePlan,
    clean: bool,
) -> anyhow::Result<()> {
    if !clean {
        return Ok(());
    }
    opencrab_db::queries::set_last_organize_at(conn, agent_id, &plan.new_marker_advance_to)?;
    if let Some(backlog_to) = &plan.backlog_marker_advance_to {
        opencrab_db::queries::set_organize_backlog_cursor(conn, agent_id, backlog_to)?;
    }
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
         メッセージ送信・シェル実行・サブタスク起動はしないでください。これは記憶整理の時間です。\n\n\
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

/// 文字（char）境界で安全に切り詰める。超過時は末尾に `…` を付ける。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ゲート判定（decide_organize）用のセットアップ ---

    fn cfg(enabled: bool, max_topics: i64, min_new: i64) -> MemoryOrganizeConfig {
        MemoryOrganizeConfig {
            enabled,
            max_topics,
            min_new_topics: min_new,
            min_interval_hours: 24,
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
        let d = decide_organize(&state, "a1", &cfg(true, 3, 2)).unwrap();
        assert!(matches!(d, OrganizeDecision::Seeded));
        assert!(
            get_marker(&state, "a1").is_some(),
            "初回で新規側マーカーがシードされる"
        );
        assert!(
            get_backlog_marker(&state, "a1").is_some(),
            "初回で遡り側マーカーもシードされる（2軸）"
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
        let d = decide_organize(&state, "a1", &cfg(true, 3, 2)).unwrap();
        assert!(matches!(d, OrganizeDecision::Skip("interval_not_elapsed")));
    }

    #[test]
    fn floor_gate_blocks_when_too_few_new_topics() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        // マーカーは 48h 前（間隔は通る）。新規 topic は 1 件だけ（下限 2 未満）。
        set_marker(&state, "a1", &hours_ago(48));
        disable_backlog(&state, "a1"); // 過去分が無い日を模す（新規側の下限だけを見る）。
        seed_topic(&state, "a1", "n0", &hours_ago(1), 10);
        let d = decide_organize(&state, "a1", &cfg(true, 3, 2)).unwrap();
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
        let d = decide_organize(&state, "a1", &cfg(true, 10, 2)).unwrap();
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
        let d = decide_organize(&state, "a1", &cfg(true, 3, 2)).unwrap();
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
                assert_eq!(
                    plan.new_marker_advance_to,
                    format_cursor(&last.created_at, &last.id)
                );
                // 再解釈すると (created_at, id) に戻る（parse ⇄ format の一貫性）。
                let (ts, id) = parse_cursor(&plan.new_marker_advance_to);
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
        let d = decide_organize(&state, "a1", &cfg(true, 2, 2)).unwrap();
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
        let d = decide_organize(&state, "a1", &cfg(true, 5, 2)).unwrap();
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
        let d = decide_organize(&state, "a1", &cfg(true, 5, 2)).unwrap();
        match d {
            OrganizeDecision::Run(plan) => {
                assert_eq!(plan.new_topic_count, 0, "新規は無い");
                assert_eq!(plan.new_presented, 0);
                assert_eq!(plan.backlog_presented, 3, "過去分だけで発火・進行する");
                // 新規側マーカーは now 付近まで進む（過去分だけの日でも日次 throttle を保つ）。
                assert!(
                    plan.new_marker_advance_to.parse::<DateTime<Utc>>().is_ok(),
                    "新規側マーカーは素の刻時（now）"
                );
                assert!(plan.backlog_marker_advance_to.is_some());
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
        let d = decide_organize(&state, "a1", &cfg(true, 5, 2)).unwrap();
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
        let plan1 = match decide_organize(&state, "a1", &cfg(true, 2, 2)).unwrap() {
            OrganizeDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let ids1: Vec<String> = plan1.worklist.iter().map(|t| t.id.clone()).collect();
        assert_eq!(ids1, vec!["old280", "old290"]);
        // clean 完了として両軸のマーカーを前進させる（タグは付けていない）。run1 で新規側は
        // `now` へ進む（過去分だけの日でも throttle を保つ設計）。
        {
            let conn = state.db.lock().unwrap();
            advance_markers(&conn, "a1", &plan1, true).unwrap();
        }
        // 翌日を模す: 新規側マーカーを 48h 前へ戻して日次ゲートを開ける（遡りカーソルは
        // run1 の前進位置のまま = 別軸なので影響しない）。
        set_marker(&state, "a1", &hours_ago(48));
        // run2: 次は old300 だけ（提示済みの old280/old290 は二度と出ない）。
        let plan2 = match decide_organize(&state, "a1", &cfg(true, 2, 2)).unwrap() {
            OrganizeDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let ids2: Vec<String> = plan2.worklist.iter().map(|t| t.id.clone()).collect();
        assert_eq!(ids2, vec!["old300"], "提示済みを拾い直さない（未タグでも）");
    }

    /// partial（clean でない）ではどちらのマーカーも進めない。
    #[test]
    fn partial_run_does_not_advance_markers() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48));
        set_backlog_marker(&state, "a1", &hours_ago(240));
        for h in [300, 290, 280] {
            seed_topic(&state, "a1", &format!("old{h}"), &hours_ago(h), 10 + h);
        }
        let plan = match decide_organize(&state, "a1", &cfg(true, 2, 2)).unwrap() {
            OrganizeDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let new_before = get_marker(&state, "a1");
        let backlog_before = get_backlog_marker(&state, "a1");
        // partial: clean=false → 進めない。
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
        // clean=true → 両軸が計画どおり進む。
        {
            let conn = state.db.lock().unwrap();
            advance_markers(&conn, "a1", &plan, true).unwrap();
        }
        assert_eq!(
            get_marker(&state, "a1").as_deref(),
            Some(plan.new_marker_advance_to.as_str())
        );
        assert_eq!(
            get_backlog_marker(&state, "a1"),
            plan.backlog_marker_advance_to
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
            new_marker_advance_to: "2026-08-02T00:00:00Z".to_string(),
            backlog_marker_advance_to: None,
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
    fn truncate_chars_respects_char_boundary_and_ellipsis() {
        // マルチバイトでもパニックせず、char 単位で切れる。
        let s = "あいうえおかきくけこ"; // 10 文字
        let out = truncate_chars(s, 4);
        assert_eq!(out, "あいうえ…");
        // 上限以下はそのまま（… を付けない）。
        assert_eq!(truncate_chars("abc", 5), "abc");
    }

    #[test]
    fn topic_line_omits_dash_when_summary_empty() {
        let line = format_topic_line(&topic("id1", "t1", "無要約", ""));
        assert_eq!(line, "- [t1] 無要約");
    }
}
