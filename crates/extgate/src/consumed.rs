//! #935/#930/#933「消費済み入力」登録簿と build 初投入の read/consume 実装。
//!
//! ある入力（said・subtask 完了）を **LLM に渡した（プロンプトへ入れた）** とき、その入力のための
//! 独立ターン／resume を起こさない（消費済み入力＝二重処理しない）。判定材料を **per-session の単一
//! 構造体** [`ConsumedInputs`] に集約する（旧: `folded_seqs`／`consumed_completions`／
//! `injected_watermark` の 3 集合を 1 つへ・in-memory・新 DB テーブルなし）。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;

use rusqlite::params;

use crate::registry::ExtgateState;

/// #935: build 初投入判定で 1 回に走査する行数の上限（暴走安全弁・超過分は次ターンで拾う）。
const BUILD_CONSUMED_POLL_LIMIT: usize = 128;

/// per-session の「消費済み入力」登録簿（単一構造体）。
///
/// - `folded_seqs`（#930/#933）: 走行中畳み込み／build 初投入で LLM に渡した said の
///   `external_origins.seq`。その said 自身の独立ターンは dequeue 時に `is_folded` なら skip。**実際に
///   fold/初投入した seq だけ**を持つ非消費集合（`contains` 判定・二重 take に免疫）。OnlySpeaker 畳み込み
///   で別話者の未 fold said を over-skip しないため、スカラ高水位ではなく seq 集合。肥大は `prune_folded_below`
///   （dequeue した seq 未満＝FIFO で処理済み）で防ぐ。
/// - `completions`（#935 c3）: build で初描画された subtask 完了の subtask_id。その完了の resume が
///   `run_v3_said_less_turn` 頭で `is_completion` なら skip。seq（i64・prune 対象）と id（String）は型が
///   違うため同一 session エントリ内で別コレクション（登録簿は 1 つ・prune は seq のみに意味を持つ）。
/// - `watermark`（#935 a/b）: プロンプトへ初投入した最終 log id。build で「これより後」の said/完了が初投入。
#[derive(Default)]
pub(crate) struct ConsumedInputs {
    inner: Mutex<HashMap<String, SessionEntry>>,
}

#[derive(Default)]
struct SessionEntry {
    folded_seqs: BTreeSet<i64>,
    completions: HashSet<String>,
    watermark: Option<i64>,
}

impl ConsumedInputs {
    pub fn mark_folded_seq(&self, session_id: &str, seq: i64) {
        if let Ok(mut m) = self.inner.lock() {
            m.entry(session_id.to_string())
                .or_default()
                .folded_seqs
                .insert(seq);
        }
    }

    pub fn is_folded(&self, session_id: &str, seq: i64) -> bool {
        self.inner
            .lock()
            .ok()
            .map(|m| {
                m.get(session_id)
                    .is_some_and(|e| e.folded_seqs.contains(&seq))
            })
            .unwrap_or(false)
    }

    pub fn prune_folded_below(&self, session_id: &str, below: i64) {
        if let Ok(mut m) = self.inner.lock() {
            if let Some(e) = m.get_mut(session_id) {
                e.folded_seqs = e.folded_seqs.split_off(&below);
                if e.folded_seqs.is_empty() && e.completions.is_empty() && e.watermark.is_none() {
                    m.remove(session_id);
                }
            }
        }
    }

    pub fn mark_completion(&self, session_id: &str, subtask_id: &str) {
        if let Ok(mut m) = self.inner.lock() {
            m.entry(session_id.to_string())
                .or_default()
                .completions
                .insert(subtask_id.to_string());
        }
    }

    pub fn is_completion(&self, session_id: &str, subtask_id: &str) -> bool {
        self.inner
            .lock()
            .ok()
            .map(|m| {
                m.get(session_id)
                    .is_some_and(|e| e.completions.contains(subtask_id))
            })
            .unwrap_or(false)
    }

    /// 未設定なら `init` で初期化して返す（初回ターンの初投入判定の基準）。
    pub fn watermark_or_init(&self, session_id: &str, init: i64) -> i64 {
        self.inner
            .lock()
            .map(|mut m| {
                m.entry(session_id.to_string())
                    .or_default()
                    .watermark
                    .get_or_insert(init)
                    .to_owned()
            })
            .unwrap_or(init)
    }

    /// watermark を単調前進させる（`id` が現在値より大きいときだけ）。
    pub fn advance_watermark(&self, session_id: &str, id: i64) {
        if let Ok(mut m) = self.inner.lock() {
            let w = &mut m.entry(session_id.to_string()).or_default().watermark;
            if w.map(|cur| id > cur).unwrap_or(true) {
                *w = Some(id);
            }
        }
    }
}

/// #935 (a)/(b): read state（👀）を emit しつつ、その said の `external_origins.seq` を消費済み化する
/// 共有実装。走行中畳み込み（R2b・`on_read_origin`）と build 初投入（R2c）の両方がここを通る
/// （read 発火・mark の単一実装）。
pub(crate) async fn emit_read_and_consume_said(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    session_id: &str,
    origin: &str,
) {
    let activity_id = uuid::Uuid::new_v4().to_string();
    crate::listen::emit_activity(
        state,
        instance_id,
        binding_id,
        &activity_id,
        "read",
        Some(origin),
        None,
    )
    .await;
    if let Some(seq) = crate::inbound::seq_for_origin(state, binding_id, origin) {
        state.mark_folded_seq(session_id, seq);
    }
}

/// #935 (c3): resume の発端完了が既に消費済み（別ターンの build で描画済み）なら true（skip）。
/// started/typing/LLM を出す前に呼ぶ。heartbeat（`completion` が None）は常に false。
pub(crate) fn resume_should_skip(
    state: &ExtgateState,
    session_id: &str,
    completion: Option<&str>,
) -> bool {
    let Some(cid) = completion else {
        return false;
    };
    if state.is_consumed_completion(session_id, cid) {
        tracing::info!(
            session_id = %session_id,
            subtask_id = %cid,
            "skip resume: subtask completion already consumed by an earlier build (#935 c3)"
        );
        return true;
    }
    false
}

/// #935/#925: resume/heartbeat ターン頭の共通前処理。発端完了が consumed なら **None（skip）**＝
/// started/typing/LLM を出さない。そうでなければ `started`（origin=None・👀 なし）を emit し、build で
/// 初描画される他の完了・初投入 said を consumed 化して **その started の activity_id** を返す（呼び出し側は
/// 対の `ended` に同じ id を使う）。`own_completion` はこの resume 自身の発端完了（heartbeat は None）で、
/// mark 対象から除く（(b) 発端 skip と同型）。
pub(crate) async fn resume_prelude(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    session_id: &str,
    agent_id: &str,
    own_completion: Option<&str>,
) -> Option<String> {
    if resume_should_skip(state, session_id, own_completion) {
        return None;
    }
    let activity_id = uuid::Uuid::new_v4().to_string();
    crate::listen::emit_activity(
        state,
        instance_id,
        binding_id,
        &activity_id,
        "started",
        None,
        None,
    )
    .await;
    mark_build_consumed_inputs(
        state,
        instance_id,
        binding_id,
        session_id,
        agent_id,
        None,
        own_completion,
    )
    .await;
    Some(activity_id)
}

/// #935 (a)/(b): 発端の無いターン（heartbeat 等）の watermark 初期化用「ターン開始時点の最新 log id」。
fn session_max_log_id(state: &Arc<ExtgateState>, session_id: &str) -> i64 {
    state
        .db
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT COALESCE(MAX(id), 0) FROM memory_sessions WHERE session_id = ?1",
                params![session_id],
                |r| r.get::<_, i64>(0),
            )
            .ok()
        })
        .unwrap_or(0)
}

/// #935 (a)/(b): 発端 said の memory_sessions log id（初回ターンの watermark 初期化用）。**発端より後**に
/// 届いた行は初回ターンでも初投入＝read+consumed にし、発端より前の古い履歴には read を出さない。
fn origin_log_id(state: &Arc<ExtgateState>, session_id: &str, origin: &str) -> Option<i64> {
    let conn = state.db.lock().ok()?;
    conn.query_row(
        "SELECT id FROM memory_sessions
         WHERE session_id = ?1 AND json_extract(metadata_json, '$.external_origin') = ?2
         ORDER BY id DESC LIMIT 1",
        params![session_id, origin],
        |r| r.get::<_, i64>(0),
    )
    .ok()
}

/// #935 (c3): watermark `after_id` より後の subtask 完了（system 行・`speaker_id IS NULL`・
/// `metadata_json.type = "subtask_completed"`）の (subtask_id, log_id) を古い順に返す。
fn build_drawn_completions(
    state: &Arc<ExtgateState>,
    session_id: &str,
    after_id: i64,
) -> Vec<(String, i64)> {
    let Ok(conn) = state.db.lock() else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, content FROM memory_sessions
         WHERE session_id = ?1 AND log_type = 'system' AND speaker_id IS NULL AND id > ?2
         ORDER BY id ASC",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map(params![session_id, after_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for row in rows.flatten() {
        let (id, content) = row;
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
            if v.get("type").and_then(|t| t.as_str()) == Some("subtask_completed") {
                if let Some(sid) = v.get("subtask_id").and_then(|s| s.as_str()) {
                    out.push((sid.to_string(), id));
                }
            }
        }
    }
    out
}

/// #935 (a)/(b)/(c3): ターン build で初めてプロンプトへ入る行を「消費済み入力」として登録する。
///
/// - 前ターン終了後に届き未投入だった said（発端以外）に read+origin を出し seq を consumed 化
///   （発端は started 済み・read しない = (b) 発端 skip）。
/// - 初描画される subtask 完了を consumed 化（`own_completion`＝この resume 自身の発端完了は除く）。
/// - watermark（初投入判定）をターン跨ぎで持ち、処理後に最大 log id へ前進。初回（未設定）は**発端行の
///   log id**（said 発端はその said 行・無ければ開始時最新 id）で初期化＝発端より後の入力は初回でも初投入。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn mark_build_consumed_inputs(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    session_id: &str,
    agent_id: &str,
    turn_origin: Option<&str>,
    own_completion: Option<&str>,
) {
    let init = turn_origin
        .and_then(|o| origin_log_id(state, session_id, o))
        .unwrap_or_else(|| session_max_log_id(state, session_id));
    let w = state.injected_watermark_or_init(session_id, init);
    let mut max_id = w;

    // W より後の user-speech（build に載る初投入 said）を古い順に。発端は read しない。
    let rows = {
        let Ok(conn) = state.db.lock() else {
            return;
        };
        opencrab_db::queries::list_user_speech_logs_after(
            &conn,
            session_id,
            agent_id,
            w,
            None,
            BUILD_CONSUMED_POLL_LIMIT,
        )
        .unwrap_or_default()
    };
    for row in &rows {
        if let Some(id) = row.id {
            max_id = max_id.max(id);
        }
        let Some(origin) = row
            .metadata_json
            .as_deref()
            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .and_then(|v| {
                v.get("external_origin")
                    .and_then(|o| o.as_str())
                    .map(|s| s.to_string())
            })
        else {
            continue;
        };
        if Some(origin.as_str()) == turn_origin {
            continue; // 発端は started・read しない（(b) 発端 skip）
        }
        emit_read_and_consume_said(state, instance_id, binding_id, session_id, &origin).await;
    }

    // 初描画される subtask 完了を consumed 化（own_completion は除く＝resume 発端 skip）。
    for (cid, id) in build_drawn_completions(state, session_id, w) {
        max_id = max_id.max(id);
        if Some(cid.as_str()) == own_completion {
            continue;
        }
        state.mark_consumed_completion(session_id, &cid);
    }

    state.advance_injected_watermark(session_id, max_id);
}

#[cfg(test)]
mod tests {
    use crate::registry::ExtgateState;

    fn test_state() -> ExtgateState {
        ExtgateState::new(
            opencrab_db::Db::memory().unwrap(),
            crate::OperatorToken::from_bytes("t"),
        )
    }

    // #933 不変(i): is_folded は fold した seq **だけ** 真（未 fold は偽）。
    #[test]
    fn is_folded_only_for_marked_seqs() {
        let s = test_state();
        s.mark_folded_seq("sess", 5);
        s.mark_folded_seq("sess", 9);
        assert!(s.is_folded("sess", 5));
        assert!(s.is_folded("sess", 9));
        assert!(!s.is_folded("sess", 7), "未 fold の 7 は skip されない");
        assert!(!s.is_folded("sess", 3), "未 fold の 3 は skip されない");
    }

    // #933 不変(ii): is_folded は非消費（同じ seq を何度照会しても真のまま）。
    #[test]
    fn is_folded_is_non_consuming() {
        let s = test_state();
        s.mark_folded_seq("sess", 7);
        for _ in 0..3 {
            assert!(s.is_folded("sess", 7), "7 は何度照会しても skip 対象のまま");
        }
        assert!(!s.is_folded("sess", 8), "8 は独立ターンを起こす");
    }

    // #933 不変(iii): 複数 said 同時畳み込みで 34,35 とも skip 対象（取りこぼさない）。
    #[test]
    fn two_said_fold_both_marked() {
        let s = test_state();
        s.mark_folded_seq("sess", 34);
        s.mark_folded_seq("sess", 35);
        assert!(s.is_folded("sess", 34), "seq34 は skip 対象");
        assert!(s.is_folded("sess", 35), "seq35 は skip 対象");
    }

    // #933 修正2（R1・最重要）: OnlySpeaker 畳み込みでの over-skip 防止。
    #[test]
    fn unfolded_other_speaker_seq_not_skipped_even_if_higher_folded() {
        let s = test_state();
        s.mark_folded_seq("sess", 41);
        assert!(
            !s.is_folded("sess", 40),
            "未 fold の B(40)は独立ターンを起こす（lost 0）"
        );
        assert!(s.is_folded("sess", 41), "fold 済みの A(41)は skip");
    }

    // #933: prune は dequeue した seq 未満を掃除するが、未 fold の判定は変わらない（over-skip なし）。
    #[test]
    fn prune_below_keeps_unfolded_not_skipped() {
        let s = test_state();
        s.mark_folded_seq("sess", 41);
        s.prune_folded_below("sess", 40);
        assert!(
            !s.is_folded("sess", 40),
            "prune 後も未 fold の 40 は skip されない"
        );
        assert!(s.is_folded("sess", 41), "41 は残る（40 以上）");
        s.prune_folded_below("sess", 42);
        assert!(
            !s.is_folded("sess", 41),
            "41 の said は dequeue 済み＝prune で掃除"
        );
    }

    // #933 不変(iv): session ごとに独立。
    #[test]
    fn is_folded_is_per_session() {
        let s = test_state();
        s.mark_folded_seq("a", 10);
        assert!(s.is_folded("a", 10));
        assert!(!s.is_folded("b", 10), "別 session は未 fold");
    }

    // #935 (c3): 完了 id の consumed 記録は per-session・非消費。
    #[test]
    fn consumed_completion_is_per_session_and_non_consuming() {
        let s = test_state();
        s.mark_consumed_completion("a", "sub-1");
        for _ in 0..3 {
            assert!(s.is_consumed_completion("a", "sub-1"));
        }
        assert!(!s.is_consumed_completion("a", "sub-2"));
        assert!(
            !s.is_consumed_completion("b", "sub-1"),
            "別 session は未消費"
        );
    }

    // #935 (a/b): watermark は init で初期化され、単調前進のみ。
    #[test]
    fn watermark_inits_once_and_advances_monotonically() {
        let s = test_state();
        assert_eq!(s.injected_watermark_or_init("sess", 100), 100);
        assert_eq!(
            s.injected_watermark_or_init("sess", 999),
            100,
            "初期化は 1 回"
        );
        s.advance_injected_watermark("sess", 50);
        assert_eq!(s.injected_watermark_or_init("sess", 0), 100, "後退しない");
        s.advance_injected_watermark("sess", 150);
        assert_eq!(s.injected_watermark_or_init("sess", 0), 150, "前進する");
    }
}
