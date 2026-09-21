use anyhow::Result;
use opencrab_llm_types::{ContentPart, Message, MessageContent, Role};

use crate::conversation::{CONVERSATION_HISTORY_END, CONVERSATION_HISTORY_START};

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

fn conversation_history_range(text: &str) -> Option<(usize, usize)> {
    if text.matches(CONVERSATION_HISTORY_START).count() != 1
        || text.matches(CONVERSATION_HISTORY_END).count() != 1
    {
        return None;
    }
    let start = text.find(CONVERSATION_HISTORY_START)? + CONVERSATION_HISTORY_START.len();
    let end = text[start..].find(CONVERSATION_HISTORY_END)? + start;
    Some((start, end))
}

fn compactable_user_text(text: &str) -> &str {
    conversation_history_range(text)
        .map(|(start, end)| text[start..end].trim_matches('\n'))
        .unwrap_or(text)
}

fn rebuild_user_text(original: &str, compacted: &str) -> String {
    let Some((start, end)) = conversation_history_range(original) else {
        return compacted.to_string();
    };
    format!(
        "{}\n{}\n{}",
        &original[..start],
        compacted.trim_matches('\n'),
        &original[end..]
    )
}

/// Append one canonically rendered visible event to the request's single bounded history.
///
/// Structured provider messages remain in their original slots; only their visible conversation
/// counterparts are rebuilt here. Refuse malformed/multiple boundaries rather than guessing.
pub(super) fn append_bounded_history_block(
    messages: &mut [Message],
    ledger: &mut crate::context_budget::TokenLedger,
    block: &str,
) -> bool {
    let mut bounded_message = None;
    let mut starts = 0;
    let mut ends = 0;
    for (index, message) in messages.iter().enumerate() {
        if message.role != Role::User {
            continue;
        }
        let text = message_plain_text(message);
        let message_starts = text.matches(CONVERSATION_HISTORY_START).count();
        let message_ends = text.matches(CONVERSATION_HISTORY_END).count();
        if message_starts > 0 || message_ends > 0 {
            bounded_message = Some(index);
        }
        starts += message_starts;
        ends += message_ends;
    }
    if starts != 1 || ends != 1 {
        return false;
    }
    let Some(message) = bounded_message.and_then(|index| messages.get_mut(index)) else {
        return false;
    };
    let text = match message.content.as_mut() {
        Some(MessageContent::Text(text)) => text,
        Some(MessageContent::Multi(parts)) => {
            let Some(ContentPart::Text { text }) = parts.iter_mut().find(|part| {
                matches!(
                    part,
                    ContentPart::Text { text }
                        if text.contains(CONVERSATION_HISTORY_START)
                            && text.contains(CONVERSATION_HISTORY_END)
                )
            }) else {
                return false;
            };
            text
        }
        Some(MessageContent::Image { .. }) | None => return false,
    };
    let Some((_, end)) = conversation_history_range(text) else {
        return false;
    };
    let separator = if text[..end].ends_with('\n') {
        ""
    } else {
        "\n"
    };
    text.insert_str(end, &format!("{separator}{block}\n"));
    ledger.record("user", text);
    true
}

/// Append to the canonical history, creating its single boundary around the initial user text
/// only when no boundary markers exist anywhere in the request.
pub(super) fn append_or_create_bounded_history_block(
    messages: &mut [Message],
    ledger: &mut crate::context_budget::TokenLedger,
    block: &str,
) -> Result<bool> {
    if append_bounded_history_block(messages, ledger, block) {
        return Ok(true);
    }

    let starts = messages
        .iter()
        .filter(|message| message.role == Role::User)
        .map(message_plain_text)
        .map(|text| text.matches(CONVERSATION_HISTORY_START).count())
        .sum::<usize>();
    let ends = messages
        .iter()
        .filter(|message| message.role == Role::User)
        .map(message_plain_text)
        .map(|text| text.matches(CONVERSATION_HISTORY_END).count())
        .sum::<usize>();
    if starts != 0 || ends != 0 {
        return Err(anyhow::anyhow!(
            "malformed or multiple <conversation_history> boundaries: found {starts} opening and {ends} closing markers"
        ));
    }

    let Some(user) = messages
        .iter_mut()
        .find(|message| message.role == Role::User)
    else {
        return Ok(false);
    };
    let text = match user.content.as_mut() {
        Some(MessageContent::Text(text)) => text,
        Some(MessageContent::Multi(parts)) => {
            let Some(ContentPart::Text { text }) = parts
                .iter_mut()
                .find(|part| matches!(part, ContentPart::Text { .. }))
            else {
                return Ok(false);
            };
            text
        }
        Some(MessageContent::Image { .. }) | None => return Ok(false),
    };
    let original = text.trim_matches('\n');
    *text = if original.is_empty() {
        format!("{CONVERSATION_HISTORY_START}\n{block}\n{CONVERSATION_HISTORY_END}")
    } else {
        format!("{CONVERSATION_HISTORY_START}\n{original}\n{block}\n{CONVERSATION_HISTORY_END}")
    };
    ledger.record("user", text);
    Ok(true)
}

pub(super) fn user_line_items(messages: &[Message]) -> Vec<crate::context_budget::CompactItem> {
    use crate::context_budget::{CompactItem, CompactLane, TokenLedger};
    let Some(user) = messages.get(1) else {
        return Vec::new();
    };
    let text = message_plain_text(user);
    let blocks = split_user_blocks(compactable_user_text(&text));
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
    let user_text = messages.get(1).map(message_plain_text).unwrap_or_default();
    let compactable_tokens = crate::tokens::estimate_tokens(compactable_user_text(&user_text));
    let fixed_user_tokens = user_tokens.saturating_sub(compactable_tokens);
    let other = ledger
        .total()
        .saturating_sub(user_tokens)
        .saturating_add(fixed_user_tokens)
        .saturating_add(reserved);
    let items = user_line_items(messages);
    let Some(outcome) =
        gov.compact_user_on_append(ledger.total().saturating_add(reserved), &items, other)
    else {
        return Ok(());
    };
    if outcome.fired {
        let rebuilt = rebuild_user_text(&user_text, &outcome.text);
        if let Some(user) = messages.get_mut(1) {
            user.content = Some(MessageContent::Text(rebuilt.clone()));
        }
        ledger.record("user", &rebuilt);
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

#[cfg(test)]
mod tests {
    use super::conversation_history_range;

    #[test]
    fn conversation_history_range_rejects_two_complete_blocks() {
        let input = "<conversation_history>\n[u1]:\nfirst\n</conversation_history>\n<conversation_history>\n[u2]:\nsecond\n</conversation_history>";

        assert_eq!(conversation_history_range(input), None);
    }
}
