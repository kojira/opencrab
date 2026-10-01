//! ツール結果の画像（`ActionResult.images`）を、そのターンが終わるまで毎回の LLM 呼び出しに
//! 見せる（D-1060）。
//!
//! 画像は最初に載せる request で、そのイテレーションで積んだ全ツール結果（と走行中の
//! 新着）の後ろへ user メッセージ 1 本として置く（Anthropic の「tool_use 直後は
//! tool_result」の順序を崩さない）。ターン内の会話（`messages`）には同じ位置に本文だけの
//! 注記を残し、request の写しを組むたびにその位置を画像つきへ差し替える。位置が変わらない
//! のでキャッシュの前方一致を崩さない。次のターンへは持ち越さない。台帳には注記だけを数える。

use opencrab_llm_types::{ContentPart, ImageUrl, Message, MessageContent, Role};

use crate::context_budget::TokenLedger;
use crate::engine::types::ChatRequest;

const SHOWN_LABEL: &str = "view_image の画像";
const SHOWN_NOTE: &str = "[view_image の画像（このターンの間は表示される）]";

#[derive(Default)]
pub(super) struct ViewImages {
    collected: Vec<String>,
    /// `messages` 内の注記の位置と、そこへ差し込む画像。
    seated: Vec<(usize, Vec<String>)>,
}

fn user_message(content: MessageContent) -> Message {
    Message {
        role: Role::User,
        content: Some(content),
        name: None,
        function_call: None,
        tool_calls: None,
        tool_call_id: None,
    }
}

fn image_message(urls: &[String]) -> Message {
    let mut parts = vec![ContentPart::Text {
        text: SHOWN_LABEL.to_string(),
    }];
    parts.extend(urls.iter().map(|url| ContentPart::ImageUrl {
        image_url: ImageUrl {
            url: url.clone(),
            detail: Some("auto".to_string()),
        },
    }));
    user_message(MessageContent::Multi(parts))
}

impl ViewImages {
    pub(super) fn collect(&mut self, images: &[String]) {
        self.collected.extend(images.iter().cloned());
    }

    /// 次の request に載せる messages を返す。新しく集めた画像があれば `messages` へ注記を
    /// 積み、返す写しでは各注記の位置を画像つきの user メッセージにする。
    pub(super) fn seat(
        &mut self,
        messages: &mut Vec<Message>,
        ledger: &mut TokenLedger,
    ) -> Vec<Message> {
        if !self.collected.is_empty() {
            messages.push(user_message(MessageContent::Text(SHOWN_NOTE.to_string())));
            ledger.record(format!("view_image:{}", messages.len()), SHOWN_NOTE);
            self.seated
                .push((messages.len() - 1, std::mem::take(&mut self.collected)));
        }
        let mut request_messages = messages.clone();
        for (index, urls) in &self.seated {
            if let Some(slot) = request_messages.get_mut(*index) {
                if slot.text_content() == Some(SHOWN_NOTE) {
                    *slot = image_message(urls);
                }
            }
        }
        request_messages
    }

    /// ログ用に data URL の本体を省略した写しを作る（LLM へ送る `request` は変えない）。
    pub(super) fn redact_for_log(&self, request: &ChatRequest) -> ChatRequest {
        redact_for_log(request)
    }
}

fn redact_url(url: &mut String) {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, payload)) = rest.split_once(',') {
            *url = format!("data:{meta},<omitted {} bytes>", payload.len());
        }
    }
}

/// ログ用に data URL の本体を省略した写しを作る。
fn redact_for_log(request: &ChatRequest) -> ChatRequest {
    let mut copy = request.clone();
    for message in &mut copy.messages {
        match &mut message.content {
            Some(MessageContent::Multi(parts)) => {
                for part in parts {
                    if let ContentPart::ImageUrl { image_url } = part {
                        redact_url(&mut image_url.url);
                    }
                }
            }
            Some(MessageContent::Image { image_url, .. }) => redact_url(&mut image_url.url),
            _ => {}
        }
    }
    copy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seat_keeps_images_for_the_turn_and_redacts_log_copy() {
        let mut messages = vec![Message::system("s"), Message::user("u")];
        let mut ledger = TokenLedger::new();
        let mut view = ViewImages::default();
        view.collect(&["data:image/png;base64,AAAABBBB".to_string()]);
        let request = ChatRequest {
            model: "m".to_string(),
            messages: view.seat(&mut messages, &mut ledger),
            functions: None,
            function_call: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            stream: None,
            metadata: Default::default(),
            agent_id: None,
            reasoning_effort: None,
        };
        let logged = view.redact_for_log(&request);
        let sent = serde_json::to_string(&request).unwrap();
        let log = serde_json::to_string(&logged).unwrap();
        assert!(sent.contains("data:image/png;base64,AAAABBBB"));
        assert!(!log.contains("AAAABBBB"));
        assert!(log.contains("data:image/png;base64,<omitted 8 bytes>"));
        assert_eq!(messages[2].text_content(), Some(SHOWN_NOTE));
        let again = view.seat(&mut messages, &mut ledger);
        assert_eq!(messages.len(), 3, "no second note for the same images");
        assert!(serde_json::to_string(&again[2])
            .unwrap()
            .contains("data:image/png;base64,AAAABBBB"));
    }
}
