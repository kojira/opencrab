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

    // --- 前進（前進のみ / 残りは次回）---
    // clean 完了時のみマーカーを worklist 末尾の (created_at, id) カーソルへ進める。
    //  - worklist 全件が範囲に収まった場合: 次回はこのカーソルより後の topic だけが対象。
    //  - 件数が N を超えた場合: N で切った残りはカーソルより後なので次回拾う。**索引ビルドは
    //    1 パスの全 topic に同一 created_at を刻むため、created_at 単体でなく id を副キーに
    //    持つカーソルにしている**（さもないと同着群の残余を恒久的に取りこぼす / #364 blocker）。
    // partial（timeout / ターン上限 / エラー）ではマーカーを進めない。タグ付与は PK 冪等
    // （`assign_topic_to_category`）なので、同じ範囲を次回に再挑戦しても重複しない。
    if clean {
        let conn = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        opencrab_db::queries::set_last_organize_at(&conn, agent_id, &plan.marker_advance_to)?;
    }

    // --- 監査（層1: agent_logs / context="sleep"）---
    // 層2（生プロンプト/生応答）は `run_agent_response` が LLM コールごとに llm_logs へ残す。
    {
        let audit = json!({
            "kind": "memory_organize",
            "outcome": outcome,
            "worklist_size": plan.worklist_size,
            "new_topic_count": plan.new_topic_count,
            "snapshot_log_id": plan.snapshot_log_id,
            "session_id": session_id,
            "marker_advanced": clean,
            "marker_advanced_to": if clean { Some(plan.marker_advance_to.clone()) } else { None },
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
#[derive(Debug)]
struct OrganizePlan {
    persona_name: String,
    personality: Option<String>,
    instructions: String,
    snapshot_log_id: i64,
    /// worklist（提示する topic）。`(created_at, id)` 昇順。
    worklist: Vec<IndexNodeRow>,
    worklist_size: usize,
    /// スナップショット以下の新規 topic 総数（N で切る前）。監査・ゲート表示用。
    new_topic_count: i64,
    /// 既存タグ（title, 付与件数）。プロンプトに現行の語彙として同梱する。
    tags: Vec<(String, i64)>,
    /// clean 完了時にマーカーへ刻む複合カーソル `"{created_at}|{id}"`（提示末尾の topic）。
    marker_advance_to: String,
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
        // 初回遭遇: now をシードして終了（既存履歴を「新規」に数えない）。id 部を持たない
        // 素の刻時でよい（次回 parse_cursor が `|` 無しを (now, "") と解釈する）。
        opencrab_db::queries::set_last_organize_at(&conn, agent_id, &now.to_rfc3339())?;
        return Ok(OrganizeDecision::Seeded);
    };
    // カーソルを (created_at, id) に分解する。日次ゲートは created_at 部だけを使う。
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

    // ゲート2: 下限（スナップショット以下の新規 topic 数）。
    let cursor = Some((since_ts.as_str(), since_id.as_str()));
    let new_topic_count =
        opencrab_db::queries::count_organize_topics(&conn, agent_id, cursor, snapshot_log_id)?;
    if new_topic_count < cfg.min_new_topics.max(1) {
        return Ok(OrganizeDecision::Skip("below_floor"));
    }

    // worklist（最大 N 件・(created_at, id) 昇順）。
    let worklist = opencrab_db::queries::list_organize_topics(
        &conn,
        agent_id,
        cursor,
        snapshot_log_id,
        cfg.max_topics.max(1),
    )?;
    let Some(last_row) = worklist.last() else {
        // 下限は満たすが LIMIT クエリで 0 件（理論上起きないが fail-safe）。
        return Ok(OrganizeDecision::Skip("empty_worklist"));
    };
    // マーカー前進先 = 提示した末尾の (created_at, id) カーソル。並び順が
    // `created_at ASC, id ASC` なので末尾が最大。同着 created_at 群を N で切っても、
    // id を副キーに持つカーソルが残余を次回へ引き継ぐ（取りこぼさない）。
    let marker_advance_to = format_cursor(&last_row.created_at, &last_row.id);

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
        tags,
        marker_advance_to,
    }))
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
         # 今回の対象（{size} 件 / スナップショット log_id={snap} 以下の新規 topic）\n\
         各行は `[短縮ID] タイトル — 要約` です。`短縮ID` を `topic_id` に渡してください。\n{worklist_txt}",
        size = plan.worklist_size,
        snap = plan.snapshot_log_id,
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
        // マーカー未設定（None）。初回遭遇は now をシードしてスキップ（既存を一気に対象化しない）。
        let d = decide_organize(&state, "a1", &cfg(true, 3, 2)).unwrap();
        assert!(matches!(d, OrganizeDecision::Seeded));
        assert!(
            get_marker(&state, "a1").is_some(),
            "初回でマーカーがシードされる"
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
        seed_topic(&state, "a1", "n0", &hours_ago(1), 10);
        let d = decide_organize(&state, "a1", &cfg(true, 3, 2)).unwrap();
        assert!(matches!(d, OrganizeDecision::Skip("below_floor")));
    }

    #[test]
    fn snapshot_upper_bound_excludes_topics_beyond_watermark() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 100);
        set_marker(&state, "a1", &hours_ago(48));
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
                // 前進先は提示した末尾の (created_at, id) 複合カーソル（= n3）。
                let last = plan.worklist.last().unwrap();
                assert_eq!(
                    plan.marker_advance_to,
                    format_cursor(&last.created_at, &last.id)
                );
                // 再解釈すると (created_at, id) に戻る（parse ⇄ format の一貫性）。
                let (ts, id) = parse_cursor(&plan.marker_advance_to);
                assert_eq!(ts, last.created_at);
                assert_eq!(id, last.id);
                // 既存タグの語彙がプロンプト材料に載る。
                assert!(plan.tags.iter().any(|(n, _)| n == "既存タグ"));
            }
            other => panic!("expected Run, got {other:?}"),
        }
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
            tags,
            marker_advance_to: "2026-08-02T00:00:00Z".to_string(),
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
        // スナップショットの明示
        assert!(sp.contains("log_id=100"));
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
