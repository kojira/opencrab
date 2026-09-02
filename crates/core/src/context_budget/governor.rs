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
    // 並行バッチの spawn 受理（同一 subtask を call ごとに重複記録）は初出だけ残す（row295 item4）。
    let mut seen_spawns = std::collections::HashSet::new();
    let delta = logs
        .iter()
        .filter(|l| match crate::conversation::spawn_ack_subtask_id(l) {
            Some(sid) => seen_spawns.insert(sid),
            None => true,
        })
        .map(|l| {
            let text = format_single_log_with_echo(l, Some(&completed_for_read), Some(&refs));
            // row318 検知器: delta 描画に生識別子が残る＝描画器のバグ。fail-loud に WARN（本番では
            // 短縮形が出ているはずなのでここは鳴らない）。凍結 snapshot blob はこの対象外。
            // #847/row339: speech 本文（利用者・全話者の自由記述）は**原文のまま**描画するので
            // 検知対象から外す（構造ヘッダ行は引き続き見る）。full スクラブ基準で本文を見ると利用者が
            // UUID/npub/64hex 等を書いた瞬間に偽 WARN が出て、本物の描画器バグ WARN をマスクする。
            if let Some(line) = crate::conversation::leaked_identifier_in_render(l, &text) {
                tracing::warn!(
                    session_id,
                    log_id = l.id.unwrap_or(0),
                    log_type = %l.log_type,
                    leaked = %line,
                    "delta 描画に生識別子が残存（描画器バグ・短縮形が出ていない）"
                );
            }
            text
        })
        .collect::<Vec<_>>()
        .join("\n");
    // #826 snapshot は凍結済みテキストの blob で、§9A/refs 描画経路を通らずそのまま連結される。
    // row339: 世代ゲートで復元する。新規凍結（v2 マーカー付き）は生成元が既にクリーン（構造=u/e/c/s・
    // 本文=原文）なのでスクラブせず本文原文のまま復元する（本文の UUID/npub/64hex を再マスクしない＝
    // compaction 後も「本文原文」裁定を守る）。マーカー無しの legacy blob（載せ替え前の歴史データ・
    // 構造に生識別子混在）だけ従来どおり read 時スクラブする。触るのは派生キャッシュの読み出しだけで
    // 正本 session_logs は書き換えない。次の finish_turn で v2 マーカー付きで再永続化され snapshot は自己治癒する。
    let text = match &snap {
        Some(s) => {
            let base = crate::conversation::restore_frozen_snapshot(&s.compacted_conversation);
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
    if std::env::var("OC_TYPED_SHADOW").is_ok() {
        // #884 PR2 (PR1 レビュー追補): flat 側トークン（直上の assembled ledger）と typed の
        // wire トークンを 1 行に併記し、§8.1 の比較記録を本番ログで取れるようにする。
        let flat_tokens = ledger.total();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let refs = build_conversation_refs(conn, &all, agent_id);
            let completed = completed_tool_call_ids(&all);
            let derived =
                crate::conversation_typed::derive_items(&all, &refs, &completed, agent_id);
            let assembled = crate::conversation_typed::assemble_typed_messages(&derived.items);
            let wire: String = assembled
                .history
                .iter()
                .filter_map(|message| serde_json::to_string(message).ok())
                .collect();
            let typed_wire_tokens = crate::tokens::estimate_tokens(&wire);
            tracing::debug!(
                session_id,
                typed_items = derived.diagnostics.item_count,
                unpaired = derived.diagnostics.unpaired_call_count,
                opaque = derived.diagnostics.opaque_event_count,
                flat_tokens,
                typed_wire_tokens,
                "typed shadow (OC_TYPED_SHADOW)"
            );
        }));
    }
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
    // 並行バッチの spawn 受理（同一 subtask を重複記録）は初出だけ items に載せる（row295 item4）。
    // これで圧縮発火時の snapshot にも二重表示が残らない。
    let mut seen_spawns = HashSet::new();
    for (gid, idxs) in groups {
        let unresolved = group_is_unresolved(&all, &idxs, &completed_ids);
        let in_recent = idxs
            .iter()
            .any(|&i| newest_user.contains(&i) || i >= recent_tail);
        for i in idxs {
            let log = &all[i];
            if let Some(sid) = crate::conversation::spawn_ack_subtask_id(log) {
                if !seen_spawns.insert(sid) {
                    continue;
                }
            }
            let text = format_single_log_with_echo(log, Some(&completed_ids), Some(&refs));
            // row318 検知器（items 経路も同じ描画器を通る）。#847: speech 本文は対象外（構造ヘッダは見る）。
            if let Some(line) = crate::conversation::leaked_identifier_in_render(log, &text) {
                tracing::warn!(
                    session_id,
                    log_id = log.id.unwrap_or(0),
                    log_type = %log.log_type,
                    leaked = %line,
                    "items 描画に生識別子が残存（描画器バグ・短縮形が出ていない）"
                );
            }
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
    // row339: 生成元の描画は既にクリーン（構造=u/e/c/s 短縮参照・本文=原文）。v2 マーカーを付けて
    // read 時 full スクラブをスキップさせ、本文原文を compaction 後も保つ（[`restore_frozen_snapshot`]）。
    let row = opencrab_db::queries::ConversationSnapshotRow {
        id: None,
        session_id: session_id.to_string(),
        compacted_conversation: crate::conversation::frozen_snapshot_with_marker(compacted),
        through_log_id,
        token_count: token_count as i64,
        created_at: None,
    };
    opencrab_db::queries::insert_conversation_snapshot(conn, &row)
}
