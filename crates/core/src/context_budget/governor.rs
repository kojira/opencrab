//! ターン統御（#826-B）。
//!
//! 圧縮の正時はターン終了直後。開始は組立と検査のみ。途中は超過時だけ刈る。
//! 派生スナップショットは行追加のみ。

use rusqlite::Connection;

use super::checkpoint::{
    parse_checkpoint_event, select_checkpoint_lane, CheckpointLane, ContextCheckpoint,
    CHECKPOINT_EVENT_TYPE,
};
use super::compact::{
    compact_to_low_water, should_compact, CompactItem, CompactLane, CompactOutcome, CompactPhase,
};
use super::ledger::TokenLedger;
use crate::conversation::format_single_log;

/// 統御が出すイベント。テストはこれの順を見る。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GovernorEvent {
    Inspect {
        phase: CompactPhase,
        tokens: usize,
    },
    CompactFired {
        phase: CompactPhase,
        before: usize,
        after: usize,
    },
    SnapshotWritten {
        through_log_id: i64,
        token_count: usize,
    },
    CheckpointEmpty,
    LowWaterUnreachable,
}

/// 1 セッションのターン統御。
#[derive(Debug, Clone)]
pub struct TurnGovernor {
    pub conversation_high: usize,
    pub conversation_low: usize,
    events: Vec<GovernorEvent>,
}

impl TurnGovernor {
    pub fn new(conversation_high: usize, conversation_low: usize) -> Self {
        Self {
            conversation_high,
            conversation_low,
            events: Vec::new(),
        }
    }

    pub fn events(&self) -> &[GovernorEvent] {
        &self.events
    }

    /// ターン開始: 組立結果を検査するだけ。ここでは刈らない。
    pub fn inspect_turn_start(&mut self, tokens: usize) {
        self.events.push(GovernorEvent::Inspect {
            phase: CompactPhase::TurnStart,
            tokens,
        });
    }

    /// append 境界: 台帳合計だけを見て、超過なら刈る。
    pub fn inspect_append(
        &mut self,
        ledger: &TokenLedger,
        items: &[CompactItem],
        checkpoint: &CheckpointLane,
    ) -> Option<CompactOutcome> {
        let tokens = ledger.total();
        self.events.push(GovernorEvent::Inspect {
            phase: CompactPhase::MidTurn,
            tokens,
        });
        if !should_compact(tokens, self.conversation_high) {
            return None;
        }
        Some(self.fire(CompactPhase::MidTurn, items, checkpoint))
    }

    /// append 後の総量が高水位を超えたとき、合成 user 文字列だけを低水位まで刈る。
    ///
    /// `other_tokens` は system / 構造化 tool など user 以外。user 車線の水位から引く。
    pub fn compact_user_on_append(
        &mut self,
        ledger_total: usize,
        user_items: &[CompactItem],
        checkpoint: &CheckpointLane,
        other_tokens: usize,
    ) -> Option<CompactOutcome> {
        self.events.push(GovernorEvent::Inspect {
            phase: CompactPhase::MidTurn,
            tokens: ledger_total,
        });
        if !should_compact(ledger_total, self.conversation_high) {
            return None;
        }
        let high = self.conversation_high.saturating_sub(other_tokens);
        let low = self.conversation_low.saturating_sub(other_tokens);
        let outcome = compact_to_low_water(user_items, checkpoint, high, low);
        self.events.push(GovernorEvent::CompactFired {
            phase: CompactPhase::MidTurn,
            before: ledger_total,
            after: other_tokens.saturating_add(outcome.after_tokens),
        });
        Some(outcome)
    }

    /// ターン終了直後の正時。超過していれば刈り、スナップショットを追記する。
    pub fn finish_turn(
        &mut self,
        conn: &Connection,
        session_id: &str,
        items: &[CompactItem],
        checkpoint: &CheckpointLane,
        through_log_id: i64,
    ) -> Result<CompactOutcome, anyhow::Error> {
        let before = items.iter().map(|i| i.tokens).sum::<usize>() + checkpoint.tokens();
        let outcome = if should_compact(before, self.conversation_high) {
            self.fire(CompactPhase::TurnEnd, items, checkpoint)
        } else {
            CompactOutcome {
                fired: false,
                before_tokens: before,
                after_tokens: before,
                text: compact_to_low_water(
                    items,
                    checkpoint,
                    self.conversation_high,
                    self.conversation_low,
                )
                .text,
                through_log_id: Some(through_log_id),
                checkpoint_empty: checkpoint.is_empty(),
                low_water_unreachable: false,
                exhausted: false,
            }
        };
        if outcome.checkpoint_empty {
            self.events.push(GovernorEvent::CheckpointEmpty);
            tracing::info!(
                target: "context_budget_check",
                session_id,
                reason = "checkpoint_empty",
                "context_checkpoint empty after compact"
            );
        }
        if outcome.low_water_unreachable {
            self.events.push(GovernorEvent::LowWaterUnreachable);
        }
        persist_snapshot(
            conn,
            session_id,
            &outcome.text,
            through_log_id,
            outcome.after_tokens,
        )?;
        self.events.push(GovernorEvent::SnapshotWritten {
            through_log_id,
            token_count: outcome.after_tokens,
        });
        Ok(outcome)
    }

    fn fire(
        &mut self,
        phase: CompactPhase,
        items: &[CompactItem],
        checkpoint: &CheckpointLane,
    ) -> CompactOutcome {
        let outcome = compact_to_low_water(
            items,
            checkpoint,
            self.conversation_high,
            self.conversation_low,
        );
        self.events.push(GovernorEvent::CompactFired {
            phase,
            before: outcome.before_tokens,
            after: outcome.after_tokens,
        });
        outcome
    }
}

/// スナップショット＋水位印より後の差分を組み立てる。開始時 compact はしない。
pub fn assemble_from_snapshot(
    conn: &Connection,
    session_id: &str,
    agent_id: &str,
) -> Result<AssembledConversation, anyhow::Error> {
    let snap = opencrab_db::queries::latest_conversation_snapshot(conn, session_id)?;
    let logs = match &snap {
        Some(s) => {
            opencrab_db::queries::list_session_logs_after(conn, session_id, s.through_log_id)?
        }
        None => opencrab_db::queries::list_session_logs_by_session(conn, session_id)?,
    };
    let logs = crate::conversation::retain_conversation_logs(logs);
    let delta = logs
        .iter()
        .map(format_single_log)
        .collect::<Vec<_>>()
        .join("\n");
    let text = match &snap {
        Some(s) if delta.is_empty() => s.compacted_conversation.clone(),
        Some(s) => format!("{}\n{delta}", s.compacted_conversation),
        None if delta.is_empty() => crate::conversation::NO_MESSAGES_MARKER.to_string(),
        None => delta,
    };
    let mut ledger = TokenLedger::new();
    ledger.record("assembled", &text);
    let through = logs
        .iter()
        .rev()
        .find_map(|l| l.id)
        .or_else(|| snap.as_ref().map(|s| s.through_log_id))
        .unwrap_or(0);
    let (items, checkpoint) = items_from_logs(conn, session_id, agent_id, &logs)?;
    Ok(AssembledConversation {
        text,
        tokens: ledger.total(),
        through_log_id: through,
        items,
        checkpoint,
        ledger,
    })
}

/// 組立結果。
#[derive(Debug, Clone)]
pub struct AssembledConversation {
    pub text: String,
    pub tokens: usize,
    pub through_log_id: i64,
    pub items: Vec<CompactItem>,
    pub checkpoint: CheckpointLane,
    pub ledger: TokenLedger,
}

/// 正本ログから車線付き単位とチェックポイントを作る。各行は 1 回だけ測る。
pub fn items_from_logs(
    conn: &Connection,
    session_id: &str,
    agent_id: &str,
    logs: &[opencrab_db::queries::SessionLogRow],
) -> Result<(Vec<CompactItem>, CheckpointLane), anyhow::Error> {
    let all = if logs.is_empty() {
        let raw = opencrab_db::queries::list_session_logs_by_session(conn, session_id)?;
        crate::conversation::retain_conversation_logs(raw)
    } else {
        logs.to_vec()
    };
    let explicit = all.iter().rev().find_map(|log| {
        if log.log_type == "system" {
            parse_checkpoint_event(&log.content)
        } else {
            None
        }
    });
    let assistant = all.iter().rev().find_map(|log| {
        if log.log_type == "speech" && log.speaker_id.as_deref() == Some(agent_id) {
            Some(log.content.as_str())
        } else {
            None
        }
    });
    let checkpoint = select_checkpoint_lane(explicit.as_ref(), assistant);

    let newest_user: Vec<usize> = all
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, log)| {
            log.log_type == "speech" && log.speaker_id.as_deref().is_some_and(|s| s != agent_id)
        })
        .take(5)
        .map(|(i, _)| i)
        .collect();
    let recent_tail = all.len().saturating_sub(8);

    let mut items = Vec::new();
    let mut ledger = TokenLedger::new();
    for (i, log) in all.iter().enumerate() {
        if log.log_type == "system" && parse_checkpoint_event(&log.content).is_some() {
            continue;
        }
        let text = format_single_log(log);
        let key = format!("log:{}", log.id.unwrap_or(i as i64));
        let tokens = ledger.record(&key, &text);
        let lane = if newest_user.contains(&i) || i >= recent_tail {
            CompactLane::RecentVerbatim
        } else if log.log_type == "tool_call" || log.log_type == "tool_result" {
            CompactLane::Echoable
        } else {
            CompactLane::OldHistory
        };
        items.push(CompactItem {
            key,
            tokens,
            text,
            lane,
            log_id: log.id,
            must_keep: newest_user.contains(&i),
        });
    }
    let _ = CHECKPOINT_EVENT_TYPE;
    Ok((items, checkpoint))
}

fn persist_snapshot(
    conn: &Connection,
    session_id: &str,
    compacted: &str,
    through_log_id: i64,
    token_count: usize,
) -> Result<i64, anyhow::Error> {
    let row = opencrab_db::queries::ConversationSnapshotRow {
        id: None,
        session_id: session_id.to_string(),
        compacted_conversation: compacted.to_string(),
        through_log_id,
        token_count: token_count as i64,
        created_at: None,
    };
    opencrab_db::queries::insert_conversation_snapshot(conn, &row)
}

/// 過大な明示更新は旧値を残して失敗させる。
pub fn apply_explicit_checkpoint(
    previous: Option<&ContextCheckpoint>,
    incoming: ContextCheckpoint,
) -> Result<ContextCheckpoint, &'static str> {
    if incoming.exceeds_cap() {
        return Err("checkpoint_oversize");
    }
    let _ = previous;
    Ok(incoming)
}
