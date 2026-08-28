//! 到達点チェックポイント（#825 / #826-B）。
//!
//! schema は `{ confirmed: [], position, next }`。専用上限 1000 token。
//! 明示更新を優先し、無ければ直近 assistant speech を逐語コピーする（要約しない）。

use serde::{Deserialize, Serialize};

use crate::tokens::estimate_tokens;

/// 到達点チェックポイントの専用上限（token）。
pub const CHECKPOINT_TOKEN_CAP: usize = 1_000;

/// 空のときの注入マーカー。
pub const CHECKPOINT_EMPTY_MARKER: &str = "[context_checkpoint: empty]";

/// typed system event の `type` 値。
pub const CHECKPOINT_EVENT_TYPE: &str = "context_checkpoint";

/// 到達点チェックポイント。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCheckpoint {
    #[serde(default)]
    pub confirmed: Vec<String>,
    #[serde(default)]
    pub position: String,
    #[serde(default)]
    pub next: String,
}

impl ContextCheckpoint {
    pub fn to_canonical_json(&self) -> String {
        serde_json::to_string(self).expect("ContextCheckpoint は常に JSON 化できる")
    }

    pub fn token_count(&self) -> usize {
        estimate_tokens(&self.to_canonical_json())
    }

    pub fn exceeds_cap(&self) -> bool {
        self.token_count() > CHECKPOINT_TOKEN_CAP
    }
}

/// 不可侵レーンに置く一件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointLane {
    Explicit(ContextCheckpoint),
    AssistantSpeech { text: String },
    Empty,
}

impl CheckpointLane {
    pub fn render(&self) -> String {
        match self {
            Self::Explicit(cp) => format!(
                "[context_checkpoint]\n{}",
                serde_json::to_string_pretty(cp).unwrap_or_else(|_| cp.to_canonical_json())
            ),
            Self::AssistantSpeech { text } => text.clone(),
            Self::Empty => CHECKPOINT_EMPTY_MARKER.to_string(),
        }
    }

    pub fn tokens(&self) -> usize {
        estimate_tokens(&self.render())
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }
}

/// 明示更新を優先し、無ければ assistant speech を逐語コピー。どちらも無ければ Empty。
pub fn select_checkpoint_lane(
    explicit: Option<&ContextCheckpoint>,
    latest_assistant_speech: Option<&str>,
) -> CheckpointLane {
    if let Some(cp) = explicit {
        return CheckpointLane::Explicit(cp.clone());
    }
    if let Some(text) = latest_assistant_speech {
        if !text.is_empty() {
            return CheckpointLane::AssistantSpeech {
                text: text.to_string(),
            };
        }
    }
    CheckpointLane::Empty
}

/// session_logs の system 本文から明示チェックポイントを読む。
pub fn parse_checkpoint_event(content: &str) -> Option<ContextCheckpoint> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    if value.get("type").and_then(|v| v.as_str()) != Some(CHECKPOINT_EVENT_TYPE) {
        return None;
    }
    Some(ContextCheckpoint {
        confirmed: value
            .get("confirmed")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        position: value
            .get("position")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        next: value
            .get("next")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// 明示更新を session_logs へ書く本文。
pub fn checkpoint_event_body(cp: &ContextCheckpoint) -> String {
    serde_json::json!({
        "type": CHECKPOINT_EVENT_TYPE,
        "confirmed": cp.confirmed,
        "position": cp.position,
        "next": cp.next,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_roundtrip_and_cap() {
        let cp = ContextCheckpoint {
            confirmed: vec!["step-1".into()],
            position: "waiting".into(),
            next: "confirm".into(),
        };
        let parsed = parse_checkpoint_event(&checkpoint_event_body(&cp)).unwrap();
        assert_eq!(parsed, cp);
        assert!(!cp.exceeds_cap());
        let huge = ContextCheckpoint {
            confirmed: vec!["x".repeat(8_000)],
            position: "p".into(),
            next: "n".into(),
        };
        assert!(huge.exceeds_cap());
        assert!(huge.token_count() > CHECKPOINT_TOKEN_CAP);
    }

    #[test]
    fn fallback_is_byte_copy_not_summary() {
        let speech = "確認した: ファイルは /tmp/a にある。";
        let lane = select_checkpoint_lane(None, Some(speech));
        match &lane {
            CheckpointLane::AssistantSpeech { text } => assert_eq!(text, speech),
            other => panic!("逐語コピーであるべき: {other:?}"),
        }
        assert_eq!(lane.render(), speech, "speech はヘッダ無しの逐語コピー");
        assert_eq!(select_checkpoint_lane(None, None), CheckpointLane::Empty);
        assert_eq!(CheckpointLane::Empty.render(), CHECKPOINT_EMPTY_MARKER);
    }

    #[test]
    fn explicit_wins_over_speech() {
        let cp = ContextCheckpoint {
            confirmed: vec!["ok".into()],
            position: "here".into(),
            next: "there".into(),
        };
        let lane = select_checkpoint_lane(Some(&cp), Some("ignored speech"));
        assert!(matches!(lane, CheckpointLane::Explicit(_)));
        assert!(!lane.render().contains("ignored speech"));
    }
}
