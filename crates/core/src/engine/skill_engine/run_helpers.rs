use opencrab_llm_types::{
    ChatResponse, ContentPart, FinishReason, ImageUrl, Message, MessageContent, Role, ToolCall,
};

use super::super::{
    types::ToolDispatcher,
    xml_parser::{parse_xml_tool_calls, strip_function_calls_xml},
};

use crate::context_budget::{TokenLedger, TurnGovernor};
use crate::conversation_typed::TypedConversation;

pub(super) struct DispatchPartition<'a> {
    pub(super) dispatch_start: Option<usize>,
    /// Original batch index and tool name for each whole-batch inline cause.
    pub(super) forced_inline: Vec<(usize, &'a str)>,
}

pub(super) fn partition_tool_calls_for_dispatch<'a, F>(
    tool_calls: &'a [ToolCall],
    dispatcher: Option<&dyn ToolDispatcher>,
    mut is_action_allowed: F,
) -> DispatchPartition<'a>
where
    F: FnMut(&str) -> bool,
{
    let Some(dispatcher) = dispatcher else {
        return DispatchPartition {
            dispatch_start: None,
            forced_inline: Vec::new(),
        };
    };

    // Cache the predicate once per allowed tool, in source order. Besides avoiding
    // repeated policy calls, this keeps the first-dispatchable boundary and every
    // later inline cause based on the same classification pass.
    let dispatchable: Vec<bool> = tool_calls
        .iter()
        .map(|tool_call| {
            is_action_allowed(&tool_call.function.name)
                && dispatcher.should_dispatch(&tool_call.function.name)
        })
        .collect();

    let Some(first) = dispatchable.iter().position(|&can_dispatch| can_dispatch) else {
        return DispatchPartition {
            dispatch_start: None,
            forced_inline: Vec::new(),
        };
    };

    if dispatchable[first..]
        .iter()
        .all(|&can_dispatch| can_dispatch)
    {
        return DispatchPartition {
            dispatch_start: Some(first),
            forced_inline: Vec::new(),
        };
    }

    let forced_inline = tool_calls
        .iter()
        .enumerate()
        .filter(|(index, _)| *index > first && !dispatchable[*index])
        .map(|(index, tool_call)| (index, tool_call.function.name.as_str()))
        .collect();
    DispatchPartition {
        dispatch_start: None,
        forced_inline,
    }
}

pub(super) struct CallFailure {
    pub(super) code: String,
    pub(super) body: String,
}

pub(super) fn classify_call_failure(
    llm_result: &anyhow::Result<ChatResponse>,
    model: &str,
    max_output_tokens: Option<u32>,
) -> Option<CallFailure> {
    match llm_result {
        Err(e) => {
            let body = e.to_string();
            let code = if opencrab_llm_types::is_context_window_error(&body) {
                opencrab_llm_types::CONTEXT_WINDOW_EXCEEDED_ERROR_CODE.to_string()
            } else {
                "error".to_string()
            };
            Some(CallFailure { code, body })
        }
        Ok(resp) => {
            let finish_reason = resp.choices.first().and_then(|c| c.finish_reason.as_ref());
            if finish_reason == Some(&FinishReason::Length) {
                let body = format!(
                    "LLM 応答が出力トークン上限（model={}, max_output_tokens={:?}, \
                     completion_tokens={}）に達して切り捨てられました。切り捨てられた\
                     応答は最終回答として扱いません（fail loud / 継続生成は #676 方針に\
                     よりしない）。上限を上げるには model_pricing にそのモデルの \
                     max_output_tokens を登録し直してください。",
                    model, max_output_tokens, resp.usage.completion_tokens,
                );
                Some(CallFailure {
                    code: opencrab_llm_types::OUTPUT_TRUNCATED_ERROR_CODE.to_string(),
                    body,
                })
            } else if opencrab_llm_types::is_empty_response(resp) {
                let body = format!(
                    "LLM 応答が意味的に空でした（content がフィールド欠落／空文字／\
                     空白のみ、かつ tool_call 無し）。最終回答として扱いません（fail \
                     loud / リトライ・フォールバックは #706 方針によりしない）。\
                     model={model}, finish_reason={finish_reason:?}"
                );
                Some(CallFailure {
                    code: opencrab_llm_types::EMPTY_RESPONSE_ERROR_CODE.to_string(),
                    body,
                })
            } else {
                None
            }
        }
    }
}

pub(super) struct NormalizedResponse {
    pub(super) content: Option<String>,
    pub(super) tool_calls: Vec<ToolCall>,
    pub(super) xml_tool_count: usize,
}

pub(super) fn normalize_response(response: &ChatResponse) -> NormalizedResponse {
    let mut content = response.first_text().map(str::to_string);
    let mut tool_calls = response
        .first_message()
        .and_then(|m| m.tool_calls.clone())
        .unwrap_or_default();
    let mut xml_tool_count = 0;

    if tool_calls.is_empty() {
        if let Some(ref text) = content {
            if text.contains("<function_calls>") {
                let parsed = parse_xml_tool_calls(text);
                if !parsed.is_empty() {
                    xml_tool_count = parsed.len();
                    tool_calls = parsed;
                    let cleaned = strip_function_calls_xml(text);
                    content = if cleaned.is_empty() {
                        None
                    } else {
                        Some(cleaned)
                    };
                }
            }
        }
    }

    NormalizedResponse {
        content,
        tool_calls,
        xml_tool_count,
    }
}

pub(super) fn strip_continue_marker(content: Option<String>) -> (Option<String>, bool) {
    let Some(content) = content else {
        return (None, false);
    };
    if content.contains(crate::continue_marker::NO_REPLY_SENTINEL) {
        return (Some(content), false);
    }
    match crate::continue_marker::strip_trailing_continue(&content) {
        Some("") => (None, true),
        Some(body) => (Some(body.to_string()), true),
        None => (Some(content), false),
    }
}

pub(super) struct InitialTurn {
    pub(super) messages: Vec<Message>,
    pub(super) ledger: TokenLedger,
    pub(super) governor: Option<TurnGovernor>,
}

pub(super) fn initialize_turn(
    system_context: &str,
    user_message: &str,
    image_urls: &[String],
    typed_conversation: Option<&TypedConversation>,
    conversation_waters: (Option<usize>, Option<usize>),
) -> InitialTurn {
    // ユーザーメッセージ本文（画像があればマルチパート）。
    let user_content = if image_urls.is_empty() {
        MessageContent::Text(user_message.to_string())
    } else {
        let mut parts = vec![ContentPart::Text {
            text: user_message.to_string(),
        }];
        for url in image_urls {
            parts.push(ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: url.clone(),
                    detail: Some("auto".to_string()),
                },
            });
        }
        MessageContent::Multi(parts)
    };

    let messages = if let Some(tc) = typed_conversation {
        // #884 PR2: System context に（keep 時のみ）出力指示を後置し、context/snapshot ブロックと
        // typed history を順に並べる。現ターンのユーザー本文（テキスト）は typed history 末尾の
        // UserSpeech に既に含まれるため二重に積まない。
        let mut system = system_context.to_string();
        // #884 PR2 §9.4-1: 省略ポリシー説明は安定文言なので system に 1 回だけ置く。
        system.push_str("\n\n");
        system.push_str(crate::conversation_typed::OMISSION_POLICY_NOTE);
        if let Some(directive) = &tc.response_directive {
            system.push_str("\n\n");
            system.push_str(directive);
        }
        let mut msgs: Vec<Message> = Vec::with_capacity(tc.history.len() + 4);
        msgs.push(Message {
            role: Role::System,
            content: Some(MessageContent::Text(system)),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        });
        if let Some(cb) = &tc.context_block {
            msgs.push(cb.clone());
        }
        if let Some(sb) = &tc.snapshot_base {
            msgs.push(sb.clone());
        }
        msgs.extend(tc.history.iter().cloned());
        // 画像は session_logs に無く typed history に載らないので、ある時だけ末尾に画像 User を足す。
        if !image_urls.is_empty() {
            let mut parts: Vec<ContentPart> = Vec::new();
            for url in image_urls {
                parts.push(ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: url.clone(),
                        detail: Some("auto".to_string()),
                    },
                });
            }
            msgs.push(Message {
                role: Role::User,
                content: Some(MessageContent::Multi(parts)),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }
        // 保険: typed 会話が実質空（履歴も context も snapshot も無い）のときだけ、現ターン本文を User として置く。
        if tc.history.is_empty() && tc.context_block.is_none() && tc.snapshot_base.is_none() {
            msgs.push(Message {
                role: Role::User,
                content: Some(user_content.clone()),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }
        msgs
    } else {
        vec![
            Message {
                role: Role::System,
                content: Some(MessageContent::Text(system_context.to_string())),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: Some(user_content),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ]
    };

    let mut ledger = TokenLedger::new();
    ledger.record("system", system_context);
    ledger.record("user", user_message);
    let governor = if typed_conversation.is_some() {
        // #884 PR2: typed 経路はターン内圧縮を行わない（PR4 の governor 移行まで）。
        // apply_turn_budget は messages[1] を flat 履歴前提で切り詰めるため typed では無効化する。
        None
    } else {
        match conversation_waters {
            (Some(h), Some(l)) => {
                let mut gov = TurnGovernor::new(h, l);
                gov.inspect_turn_start(ledger.total());
                Some(gov)
            }
            _ => None,
        }
    };

    InitialTurn {
        messages,
        ledger,
        governor,
    }
}
