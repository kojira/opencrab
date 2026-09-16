use opencrab_gate_client::json::parse_object_no_dup;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::config::canonical_uuid;

pub const MAX_LINE: usize = 1_048_576;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Message { id: String, text: String },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordError {
    pub request_id: Option<String>,
    pub code: &'static str,
}

pub enum ReadRecord {
    Eof,
    Record(Result<Input, RecordError>),
}

pub async fn read_record<R: AsyncRead + Unpin>(reader: &mut R) -> ReadRecord {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0_u8];
        match reader.read_exact(&mut byte).await {
            Ok(_) => {
                bytes.push(byte[0]);
                if bytes.len() > MAX_LINE {
                    while byte[0] != b'\n' {
                        match reader.read_exact(&mut byte).await {
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    return ReadRecord::Record(Err(RecordError {
                        request_id: None,
                        code: "too_large",
                    }));
                }
                if byte[0] == b'\n' {
                    bytes.pop();
                    return ReadRecord::Record(parse_record(&bytes));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                if bytes.is_empty() {
                    return ReadRecord::Eof;
                }
                return ReadRecord::Record(Err(RecordError {
                    request_id: None,
                    code: "bad_request",
                }));
            }
            Err(_) => {
                return ReadRecord::Record(Err(RecordError {
                    request_id: None,
                    code: "bad_request",
                }));
            }
        }
    }
}

pub fn parse_record(bytes: &[u8]) -> Result<Input, RecordError> {
    let value = parse_object_no_dup(bytes).map_err(|_| bad(None))?;
    let request_id = trustworthy_id(&value);
    let object = value
        .as_object()
        .expect("duplicate-safe parser returns object");
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| bad(request_id.clone()))?;
    match kind {
        "message" => {
            if object.len() != 3 || !object.contains_key("id") || !object.contains_key("text") {
                return Err(bad(request_id));
            }
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .and_then(canonical_uuid)
                .ok_or_else(|| bad(None))?;
            let text = object
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| bad(Some(id.clone())))?;
            Ok(Input::Message {
                id,
                text: text.to_string(),
            })
        }
        "shutdown" => {
            if object.len() == 1 {
                Ok(Input::Shutdown)
            } else {
                Err(bad(None))
            }
        }
        _ => Err(bad(request_id)),
    }
}

fn trustworthy_id(value: &Value) -> Option<String> {
    value
        .get("id")
        .and_then(Value::as_str)
        .and_then(canonical_uuid)
}

fn bad(request_id: Option<String>) -> RecordError {
    RecordError {
        request_id,
        code: "bad_request",
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Ready {
        agent_id: String,
        session_id: String,
        state: &'static str,
    },
    Accepted {
        id: String,
        origin: String,
        seq: i64,
    },
    NotAdmitted {
        id: String,
    },
    Message {
        delivery_id: String,
        text: String,
        reply_origin: Option<String>,
    },
    Activity {
        activity_id: String,
        state: String,
        origin: Option<String>,
    },
    Completed {
        target: String,
    },
    CompletedNoReply {
        reply_origin: Option<String>,
    },
    TurnFailed {
        reply_origin: String,
    },
    Connection {
        state: &'static str,
    },
    Error {
        request_id: Option<String>,
        code: String,
        detail: Option<String>,
    },
    Closed {
        reason: &'static str,
    },
}

#[derive(Debug, Clone)]
pub enum Output {
    Event(Event),
    ReplLine(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

    #[test]
    fn parses_message_and_shutdown() {
        assert_eq!(
            parse_record(format!(r#"{{"type":"message","id":"{ID}","text":"hello"}}"#).as_bytes())
                .unwrap(),
            Input::Message {
                id: ID.into(),
                text: "hello".into()
            }
        );
        assert_eq!(
            parse_record(br#"{"type":"shutdown"}"#).unwrap(),
            Input::Shutdown
        );
    }

    #[test]
    fn rejects_duplicates_unknown_members_and_attachments() {
        assert!(parse_record(br#"{"type":"shutdown","type":"shutdown"}"#).is_err());
        assert!(parse_record(br#"{"type":"shutdown","extra":1}"#).is_err());
        let line = format!(r#"{{"type":"message","id":"{ID}","text":"x","attachments":[]}}"#);
        assert!(parse_record(line.as_bytes()).is_err());
    }

    #[test]
    fn keeps_only_trustworthy_request_id_on_error() {
        let line = format!(r#"{{"type":"message","id":"{ID}","text":""}}"#);
        assert_eq!(
            parse_record(line.as_bytes())
                .unwrap_err()
                .request_id
                .as_deref(),
            Some(ID)
        );
        let err = parse_record(br#"{"type":"message","id":"NO","text":"x"}"#).unwrap_err();
        assert_eq!(err.request_id, None);
    }

    #[tokio::test]
    async fn enforces_including_lf_size_and_recovers_next_record() {
        let mut bytes = vec![b'x'; MAX_LINE];
        bytes.extend_from_slice(b"\n{\"type\":\"shutdown\"}\n");
        let mut reader = &bytes[..];
        match read_record(&mut reader).await {
            ReadRecord::Record(Err(error)) => assert_eq!(error.code, "too_large"),
            _ => panic!("expected size error"),
        }
        match read_record(&mut reader).await {
            ReadRecord::Record(Ok(Input::Shutdown)) => {}
            _ => panic!("expected recovered record"),
        }
    }

    #[tokio::test]
    async fn accepts_exact_size_and_rejects_invalid_utf8_and_partial_eof() {
        let prefix = format!(r#"{{"type":"message","id":"{ID}","text":""#);
        let suffix = "\"}\n";
        let text_len = MAX_LINE - prefix.len() - suffix.len();
        let line = format!("{prefix}{}{suffix}", "x".repeat(text_len));
        assert_eq!(line.len(), MAX_LINE);
        let mut reader = line.as_bytes();
        assert!(matches!(
            read_record(&mut reader).await,
            ReadRecord::Record(Ok(Input::Message { .. }))
        ));

        let mut invalid: &[u8] = b"{\"type\":\"shutdown\",\"x\":\xff}\n";
        assert!(matches!(
            read_record(&mut invalid).await,
            ReadRecord::Record(Err(RecordError {
                code: "bad_request",
                ..
            }))
        ));
        let mut partial: &[u8] = b"{\"type\":\"shutdown\"}";
        assert!(matches!(
            read_record(&mut partial).await,
            ReadRecord::Record(Err(RecordError {
                code: "bad_request",
                ..
            }))
        ));
    }

    #[test]
    fn events_serialize_as_protocol_objects() {
        let value = serde_json::to_value(Event::Error {
            request_id: None,
            code: "disconnect".into(),
            detail: None,
        })
        .unwrap();
        assert_eq!(value["type"], "error");
        assert!(value.get("request_id").unwrap().is_null());
        assert!(value.get("detail").unwrap().is_null());
    }
}
