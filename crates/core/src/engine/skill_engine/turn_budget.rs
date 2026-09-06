use anyhow::Result;
use opencrab_llm_types::{ContentPart, Message, MessageContent};

pub(super) fn message_plain_text(msg: &Message) -> String {
    match &msg.content {
        Some(MessageContent::Text(t)) => t.clone(),
        Some(MessageContent::Multi(parts)) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(MessageContent::Image { .. }) | None => String::new(),
    }
}

fn split_user_blocks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        let new_block = line.starts_with('[')
            && line.contains("]:")
            && !line.starts_with("[tool_call]")
            && !line.starts_with("[tool_result]")
            && !line.starts_with("[id=")
            && !line.starts_with("[old_history_summary]")
            && !line.starts_with("[echo]");
        if new_block && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(line);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.into_iter().filter(|b| !b.trim().is_empty()).collect()
}

fn is_toolish_user_block(block: &str) -> bool {
    block.contains("[tool_call]")
        || block.contains("[tool_result]")
        || block.contains("[system:")
        || block.contains("[subtask_completed")
}

pub(super) fn user_line_items(messages: &[Message]) -> Vec<crate::context_budget::CompactItem> {
    use crate::context_budget::{CompactItem, CompactLane, TokenLedger};
    let Some(user) = messages.get(1) else {
        return Vec::new();
    };
    let text = message_plain_text(user);
    let blocks = split_user_blocks(&text);
    let tail = blocks.len().saturating_sub(8);
    let newest_speech: std::collections::HashSet<usize> = blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| !is_toolish_user_block(b))
        .map(|(i, _)| i)
        .rev()
        .take(5)
        .collect();
    let mut ledger = TokenLedger::new();
    let mut gid = 1u64;
    let mut last_tool_gid: Option<u64> = None;
    blocks
        .into_iter()
        .enumerate()
        .map(|(i, block)| {
            let is_tool = is_toolish_user_block(&block);
            let group_id = if is_tool {
                match last_tool_gid {
                    Some(g) => g,
                    None => {
                        let g = gid;
                        gid += 1;
                        last_tool_gid = Some(g);
                        g
                    }
                }
            } else {
                last_tool_gid = None;
                let g = gid;
                gid += 1;
                g
            };
            let key = format!("user:{i}");
            let tokens = ledger.record(&key, &block);
            let keep_speech = newest_speech.contains(&i);
            CompactItem {
                key,
                tokens,
                text: block,
                lane: if keep_speech || (i >= tail && !is_tool) {
                    CompactLane::RecentVerbatim
                } else if is_tool {
                    CompactLane::Echoable
                } else {
                    CompactLane::OldHistory
                },
                log_id: Some(i as i64),
                must_keep: keep_speech,
                group_id: Some(group_id),
            }
        })
        .collect()
}

pub(super) fn apply_turn_budget(
    gov: &mut Option<crate::context_budget::TurnGovernor>,
    ledger: &mut crate::context_budget::TokenLedger,
    messages: &mut [Message],
    reserved: usize,
) -> Result<(), anyhow::Error> {
    let Some(gov) = gov.as_mut() else {
        return Ok(());
    };
    let user_tokens = ledger
        .items()
        .iter()
        .find(|i| i.key == "user")
        .map(|i| i.tokens)
        .unwrap_or(0);
    // `reserved` は「これから載せる本文」の見積り。会話単体は高水位未満でも、
    // 本文を足すと超えるなら先に刈って残り枠を空ける。収まらなくてもここでは
    // 止めない（結果は残り枠へ切り詰めて必ず載せる）。
    let other = ledger
        .total()
        .saturating_sub(user_tokens)
        .saturating_add(reserved);
    let items = user_line_items(messages);
    let Some(outcome) =
        gov.compact_user_on_append(ledger.total().saturating_add(reserved), &items, other)
    else {
        return Ok(());
    };
    if outcome.fired {
        if let Some(user) = messages.get_mut(1) {
            user.content = Some(MessageContent::Text(outcome.text.clone()));
        }
        ledger.record_tokens("user", outcome.after_tokens);
    }
    Ok(())
}

fn remaining_conversation(
    gov: &Option<crate::context_budget::TurnGovernor>,
    ledger: &crate::context_budget::TokenLedger,
) -> Option<usize> {
    gov.as_ref()
        .map(|g| g.conversation_high.saturating_sub(ledger.total()))
}

fn result_exceeds_limit(result_json: &str, limit: usize) -> bool {
    result_json.len() >= limit && crate::tokens::tokens_reach_limit(result_json, limit)
}

/// 結果を載せる前に必要なら圧縮し、残り枠へ切り詰めた本文を返す。turn は止めない。
pub(super) fn seat_tool_result(
    gov: &mut Option<crate::context_budget::TurnGovernor>,
    ledger: &mut crate::context_budget::TokenLedger,
    messages: &mut [Message],
    tool_name: &str,
    result_json: &str,
    cap: impl FnOnce(Option<usize>) -> String,
) -> Result<String, anyhow::Error> {
    apply_turn_budget(gov, ledger, messages, 0)?;
    let remaining = remaining_conversation(gov, ledger);
    let tentative = crate::tool_result_log::append_limit_for_tool(tool_name, remaining);
    if result_exceeds_limit(result_json, tentative) {
        apply_turn_budget(
            gov,
            ledger,
            messages,
            crate::tool_result_log::inline_limit_for_tool(tool_name),
        )?;
    }
    Ok(cap(remaining_conversation(gov, ledger)))
}
