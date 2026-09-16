use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::jsonl::{Event, Input, Output};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Ignore,
    Input(Input),
    Help,
    Status,
    Error(String),
}

pub fn parse_line(line: &str) -> Line {
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.is_empty() {
        return Line::Ignore;
    }
    match line {
        ":help" => Line::Help,
        ":status" => Line::Status,
        ":quit" => Line::Input(Input::Shutdown),
        command if command.starts_with("::") => Line::Input(message(&command[1..])),
        command if command.starts_with(':') => Line::Error(format!("unknown command: {command}")),
        text => Line::Input(message(text)),
    }
}

fn message(text: &str) -> Input {
    let id = uuid::Uuid::new_v4().to_string();
    Input::Message {
        id,
        text: text.to_string(),
    }
}

pub fn help() -> &'static str {
    ":help commands | :status connection | :quit exit | ::text sends :text; one nonempty line is one message"
}

pub async fn write_output<W: AsyncWrite + Unpin>(
    writer: &mut W,
    output: &Output,
    agent_id: &str,
) -> std::io::Result<()> {
    let line = match output {
        Output::ReplLine(line) => Some(safe_text(line)),
        Output::Event(event) => render_event(event, agent_id),
    };
    if let Some(line) = line {
        writer.write_all(b"\r\x1b[2K").await?;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    } else {
        writer.write_all(b"\r\x1b[2K").await?;
    }
    if !matches!(output, Output::Event(Event::Closed { .. })) {
        writer.write_all(b"you> ").await?;
    }
    writer.flush().await
}

fn render_event(event: &Event, agent_id: &str) -> Option<String> {
    match event {
        Event::Ready {
            agent_id,
            session_id,
            ..
        } => Some(format!(
            "Connected: {} / {}",
            safe_text(agent_id),
            safe_text(session_id)
        )),
        Event::Accepted { seq, .. } => Some(format!("accepted #{seq}")),
        Event::NotAdmitted { .. } => Some("not admitted".into()),
        Event::Message { text, .. } => {
            Some(format!("{}> {}", safe_text(agent_id), safe_text(text)))
        }
        Event::Activity { state, .. } if state == "read" => Some("read".into()),
        Event::Activity { state, .. } if state == "started" => Some("thinking…".into()),
        Event::Activity { .. } | Event::Completed { .. } => None,
        Event::CompletedNoReply { .. } => Some("(no reply)".into()),
        Event::TurnFailed { .. } => Some("(turn failed)".into()),
        Event::Connection { state } => Some((*state).into()),
        Event::Error { code, .. } => Some(format!("error: {}", safe_text(code))),
        Event::Closed { .. } => Some("closed".into()),
    }
}

pub fn safe_text(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' | '\t' => result.push(character),
            character if character.is_control() => {
                use std::fmt::Write;
                let _ = write!(result, "\\u{{{:04x}}}", character as u32);
            }
            character => result.push(character),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_and_escape_are_distinct_from_messages() {
        assert_eq!(parse_line(""), Line::Ignore);
        assert_eq!(parse_line(":help"), Line::Help);
        assert!(matches!(parse_line(":quit"), Line::Input(Input::Shutdown)));
        match parse_line("::hello") {
            Line::Input(Input::Message { text, .. }) => assert_eq!(text, ":hello"),
            _ => panic!("expected message"),
        }
        assert!(matches!(parse_line(":wat"), Line::Error(_)));
    }

    #[test]
    fn neutralizes_terminal_control_characters() {
        assert_eq!(safe_text("a\u{1b}[31m\u{7}b"), "a\\u{001b}[31m\\u{0007}b");
        assert_eq!(safe_text("a\nb\tc"), "a\nb\tc");
    }

    #[tokio::test]
    async fn renderer_serializes_event_and_redraws_prompt() {
        let mut bytes = Vec::new();
        write_output(
            &mut bytes,
            &Output::Event(Event::Message {
                delivery_id: "d".into(),
                text: "hello".into(),
                reply_origin: None,
            }),
            "agent-a",
        )
        .await
        .unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "\r\x1b[2Kagent-a> hello\nyou> "
        );
    }
}
