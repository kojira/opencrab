use opencrab_llm_types::{ContentPart, ImageUrl, Message, MessageContent, Role};

use crate::context_budget::{TokenLedger, TurnGovernor};
use crate::conversation_typed::TypedConversation;

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
