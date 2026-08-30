//! ターン統御（#826-B）。
//!
//! 圧縮の正時はターン終了直後。開始は組立と検査のみ（超過だけ途中扱い）。
//! 途中は超過時だけ刈る。派生スナップショットは行追加のみ。
//! 本番経路はすべてここを通り、発火は [`take_governor_events`] で観測できる。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use super::compact::{
    compact_to_low_water, should_compact, CompactItem, CompactLane, CompactOutcome, CompactPhase,
};
use super::ledger::TokenLedger;
use crate::conversation::format_single_log_with_echo;

thread_local! {
    static EVENT_SINK: RefCell<Vec<GovernorEvent>> = const { RefCell::new(Vec::new()) };
}

/// 統御が出すイベント。本番経路がここへ積む。テストはこれの順を見る。
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
    LowWaterUnreachable,
}

/// テストが本番発火点の順を取る。取るたびに空になる。
pub fn take_governor_events() -> Vec<GovernorEvent> {
    EVENT_SINK.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

fn emit(ev: GovernorEvent) {
    EVENT_SINK.with(|s| s.borrow_mut().push(ev));
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

    fn push(&mut self, ev: GovernorEvent) {
        emit(ev.clone());
        self.events.push(ev);
    }

    /// ターン開始: 組立結果を検査するだけ。ここでは刈らない。
    pub fn inspect_turn_start(&mut self, tokens: usize) {
        self.push(GovernorEvent::Inspect {
            phase: CompactPhase::TurnStart,
            tokens,
        });
    }

    /// 開始時に高水位を越えていたときだけ、途中超過と同じ圧縮を走らせる。
    pub fn compact_start_if_over(
        &mut self,
        tokens: usize,
        items: &[CompactItem],
    ) -> Option<CompactOutcome> {
        self.inspect_turn_start(tokens);
        if !should_compact(tokens, self.conversation_high) {
            return None;
        }
        Some(self.fire(CompactPhase::MidTurn, items))
    }

    /// append 後の総量が高水位を超えたとき、合成 user 文字列だけを低水位まで刈る。
    ///
    /// `other_tokens` は system / 構造化 tool など user 以外。user 車線の水位から引く。
    pub fn compact_user_on_append(
        &mut self,
        ledger_total: usize,
        user_items: &[CompactItem],
        other_tokens: usize,
    ) -> Option<CompactOutcome> {
        self.push(GovernorEvent::Inspect {
            phase: CompactPhase::MidTurn,
            tokens: ledger_total,
        });
        if !should_compact(ledger_total, self.conversation_high) {
            return None;
        }
        let high = self.conversation_high.saturating_sub(other_tokens);
        let low = self.conversation_low.saturating_sub(other_tokens);
        let outcome = compact_to_low_water(user_items, high, low);
        self.push(GovernorEvent::CompactFired {
            phase: CompactPhase::MidTurn,
            before: ledger_total,
            after: other_tokens.saturating_add(outcome.after_tokens),
        });
        Some(outcome)
    }

    /// ターン終了直後の正時。超過していれば刈り、スナップショットを追記する。
    ///
    /// 非発火時は `assembled_text`（スナップショット＋差分の全文）を書く。
    /// 差分 items だけを書いて履歴を捨てない。
    pub fn finish_turn(
        &mut self,
        conn: &Connection,
        session_id: &str,
        items: &[CompactItem],
        through_log_id: i64,
        assembled_text: &str,
    ) -> Result<CompactOutcome, anyhow::Error> {
        let before = crate::tokens::estimate_tokens(assembled_text);
        let outcome = if should_compact(before, self.conversation_high) {
            let mut fired = self.fire(CompactPhase::TurnEnd, items);
            fired.before_tokens = before;
            fired
        } else {
            CompactOutcome {
                fired: false,
                before_tokens: before,
                after_tokens: before,
                text: assembled_text.to_string(),
                through_log_id: Some(through_log_id),
                low_water_unreachable: false,
                exhausted: false,
            }
        };
        if outcome.low_water_unreachable {
            self.push(GovernorEvent::LowWaterUnreachable);
        }
        persist_snapshot(
            conn,
            session_id,
            &outcome.text,
            through_log_id,
            outcome.after_tokens,
        )?;
        self.push(GovernorEvent::SnapshotWritten {
            through_log_id,
            token_count: outcome.after_tokens,
        });
        Ok(outcome)
    }

    fn fire(&mut self, phase: CompactPhase, items: &[CompactItem]) -> CompactOutcome {
        let outcome = compact_to_low_water(items, self.conversation_high, self.conversation_low);
        self.push(GovernorEvent::CompactFired {
            phase,
            before: outcome.before_tokens,
            after: outcome.after_tokens,
        });
        outcome
    }
}

/// スナップショット＋水位印より後の差分を組み立てる。開始時 compact はしない。
///
/// `items` は正本の全ログから作る。
/// refs を作り、自分の表示名（agents.name）を引いて設定する（row295c: 自分行を UUID でなく
/// 名前で出す）。名前引きに失敗しても refs は使える（speaker_label が agent_id へフォールバック）。
fn build_conversation_refs(
    conn: &Connection,
    logs: &[opencrab_db::queries::SessionLogRow],
    agent_id: &str,
) -> crate::conversation::ConversationRefs {
    let mut refs = crate::conversation::ConversationRefs::build(logs, agent_id);
    if let Ok(Some(agent)) = opencrab_db::queries::get_agent(conn, agent_id) {
        refs.set_agent_name(agent.name);
    }
    refs
}

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
    let all = crate::conversation::retain_conversation_logs(
        opencrab_db::queries::list_session_logs_by_session(conn, session_id)?,
    );
    let completed_for_read = completed_tool_call_ids(&all);
    // §9A: u/e/c 短縮参照は全履歴の初出順で採番し、snapshot 境界を跨いでも安定させる。
    let refs = build_conversation_refs(conn, &all, agent_id);
    let delta = logs
        .iter()
        .map(|l| format_single_log_with_echo(l, Some(&completed_for_read), Some(&refs)))
        .collect::<Vec<_>>()
        .join("\n");
    // #826 snapshot は旧レンダリング済みテキストの blob で、§9A/refs 描画経路を通らずそのまま
    // 連結される。read 時に表示剥がし（メタ行除去・生識別子短縮）を適用して legacy 残存を消す。
    // 触るのは派生キャッシュの読み出しだけで正本 session_logs は書き換えない。剥がしは冪等なので
    // 新形式（既に §9A）には無影響。次の finish_turn で剥がし済みが再永続化され snapshot は自己治癒する。
    let text = match &snap {
        Some(s) => {
            let base =
                crate::conversation::strip_inbound_meta_for_display(&s.compacted_conversation);
            if delta.is_empty() {
                base
            } else {
                format!("{base}\n{delta}")
            }
        }
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
    let items = items_from_logs(conn, session_id, agent_id, &all)?;
    Ok(AssembledConversation {
        text,
        tokens: ledger.total(),
        through_log_id: through,
        items,
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
    pub ledger: TokenLedger,
}

/// 正本ログから車線付き単位を作る。各行は 1 回だけ測る。
///
/// tool_call と対応 result は同じ [`super::compact::ExchangeGroup`] になる。
pub fn items_from_logs(
    conn: &Connection,
    session_id: &str,
    agent_id: &str,
    logs: &[opencrab_db::queries::SessionLogRow],
) -> Result<Vec<CompactItem>, anyhow::Error> {
    let all = if logs.is_empty() {
        let raw = opencrab_db::queries::list_session_logs_by_session(conn, session_id)?;
        crate::conversation::retain_conversation_logs(raw)
    } else {
        logs.to_vec()
    };

    let newest_user: HashSet<usize> = all
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

    let completed_ids = completed_tool_call_ids(&all);
    let refs = build_conversation_refs(conn, &all, agent_id);
    let groups = partition_exchange_groups(&all, agent_id);

    let mut items = Vec::new();
    let mut ledger = TokenLedger::new();
    for (gid, idxs) in groups {
        let unresolved = group_is_unresolved(&all, &idxs, &completed_ids);
        let in_recent = idxs
            .iter()
            .any(|&i| newest_user.contains(&i) || i >= recent_tail);
        for i in idxs {
            let log = &all[i];
            let text = format_single_log_with_echo(log, Some(&completed_ids), Some(&refs));
            let key = format!("log:{}", log.id.unwrap_or(i as i64));
            let tokens = ledger.record(&key, &text);
            let must_keep = newest_user.contains(&i) || unresolved;
            let lane = if must_keep || in_recent {
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
                must_keep,
                group_id: Some(gid),
            });
        }
    }
    Ok(items)
}

fn completed_tool_call_ids(logs: &[opencrab_db::queries::SessionLogRow]) -> HashSet<String> {
    logs.iter()
        .filter(|l| l.log_type == "tool_result" || l.log_type == "tool_cancelled")
        .filter_map(|l| tool_call_id_from_meta(l.metadata_json.as_deref()))
        .collect()
}

fn tool_call_id_from_meta(meta: Option<&str>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(meta?).ok()?;
    v.get("tool_call_id")
        .and_then(|x| x.as_str())
        .map(ToOwned::to_owned)
}

fn tool_call_ids_from_log(log: &opencrab_db::queries::SessionLogRow) -> Vec<String> {
    let Some(meta) = log.metadata_json.as_deref() else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(meta) else {
        return Vec::new();
    };
    let Some(raw) = v.get("tool_calls_json").and_then(|x| x.as_str()) else {
        return Vec::new();
    };
    let Ok(arr) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    arr.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.get("id")
                        .and_then(|id| id.as_str())
                        .map(ToOwned::to_owned)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn group_is_unresolved(
    logs: &[opencrab_db::queries::SessionLogRow],
    idxs: &[usize],
    completed: &HashSet<String>,
) -> bool {
    idxs.iter().any(|&i| {
        logs[i].log_type == "tool_call"
            && tool_call_ids_from_log(&logs[i])
                .iter()
                .any(|id| !completed.contains(id))
    })
}

/// assistant said + 直後の tool_call/result を 1 group にする。user speech は単独 group。
fn partition_exchange_groups(
    logs: &[opencrab_db::queries::SessionLogRow],
    agent_id: &str,
) -> Vec<(u64, Vec<usize>)> {
    let mut call_to_group: HashMap<String, u64> = HashMap::new();
    let mut groups: Vec<(u64, Vec<usize>)> = Vec::new();
    let mut open_group: Option<u64> = None;
    let mut next_id = 1u64;

    for (i, log) in logs.iter().enumerate() {
        let is_user =
            log.log_type == "speech" && log.speaker_id.as_deref().is_some_and(|s| s != agent_id);
        let is_assistant = log.log_type == "speech" && log.speaker_id.as_deref() == Some(agent_id);

        if is_user {
            let gid = next_id;
            next_id += 1;
            groups.push((gid, vec![i]));
            open_group = None;
            continue;
        }
        if is_assistant {
            let gid = next_id;
            next_id += 1;
            groups.push((gid, vec![i]));
            open_group = Some(gid);
            continue;
        }
        if log.log_type == "tool_call" {
            let gid = open_group.unwrap_or_else(|| {
                let g = next_id;
                next_id += 1;
                groups.push((g, Vec::new()));
                open_group = Some(g);
                g
            });
            if let Some((_, idxs)) = groups.iter_mut().find(|(id, _)| *id == gid) {
                idxs.push(i);
            }
            for cid in tool_call_ids_from_log(log) {
                call_to_group.insert(cid, gid);
            }
            continue;
        }
        if log.log_type == "tool_result" || log.log_type == "tool_cancelled" {
            let gid = tool_call_id_from_meta(log.metadata_json.as_deref())
                .and_then(|id| call_to_group.get(&id).copied())
                .or(open_group)
                .unwrap_or_else(|| {
                    let g = next_id;
                    next_id += 1;
                    groups.push((g, Vec::new()));
                    g
                });
            if let Some((_, idxs)) = groups.iter_mut().find(|(id, _)| *id == gid) {
                idxs.push(i);
            }
            continue;
        }
        let gid = next_id;
        next_id += 1;
        groups.push((gid, vec![i]));
        open_group = None;
    }
    groups
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
