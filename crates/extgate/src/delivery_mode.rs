//! instance config の optional `delivery_mode`。欠落は `say`（WEBGATE §8.2）。
//! `kind_id` では分岐しない。

use opencrab_actions::DeliveryEffect;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    Say,
    ToolDriven,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryModeError {
    Invalid,
}

/// config bytes を読む。member 欠落は `say`。未知値・非 object は Invalid。
pub fn delivery_mode_from_config_bytes(bytes: &[u8]) -> Result<DeliveryMode, DeliveryModeError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| DeliveryModeError::Invalid)?;
    let obj = value.as_object().ok_or(DeliveryModeError::Invalid)?;
    match obj.get("delivery_mode") {
        None => Ok(DeliveryMode::Say),
        Some(Value::String(s)) if s == "say" => Ok(DeliveryMode::Say),
        Some(Value::String(s)) if s == "tool_driven" => Ok(DeliveryMode::ToolDriven),
        _ => Err(DeliveryModeError::Invalid),
    }
}

/// inbound 最終本文の say 抑止。`tool_driven` は Text だけ NoReply へ置換する。
pub fn adjust_inbound_effect(mode: DeliveryMode, effect: DeliveryEffect) -> DeliveryEffect {
    match (mode, effect) {
        (DeliveryMode::ToolDriven, DeliveryEffect::Text { .. }) => DeliveryEffect::NoReply,
        (_, other) => other,
    }
}

/// 自発配送を V3 say dispatcher へ渡すか。`tool_driven` は渡さない。
pub fn dispatches_v3_say(mode: DeliveryMode) -> bool {
    matches!(mode, DeliveryMode::Say)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_member_is_say() {
        assert_eq!(
            delivery_mode_from_config_bytes(b"{}").unwrap(),
            DeliveryMode::Say
        );
        assert_eq!(
            delivery_mode_from_config_bytes(br#"{"author_id":"owner"}"#).unwrap(),
            DeliveryMode::Say
        );
        assert_eq!(
            delivery_mode_from_config_bytes(br#"{"delivery_mode":"say"}"#).unwrap(),
            DeliveryMode::Say
        );
    }

    #[test]
    fn tool_driven_is_explicit() {
        assert_eq!(
            delivery_mode_from_config_bytes(br#"{"delivery_mode":"tool_driven"}"#).unwrap(),
            DeliveryMode::ToolDriven
        );
    }

    #[test]
    fn unknown_enum_is_invalid() {
        assert_eq!(
            delivery_mode_from_config_bytes(br#"{"delivery_mode":"banana"}"#),
            Err(DeliveryModeError::Invalid)
        );
        assert_eq!(
            delivery_mode_from_config_bytes(b"[]"),
            Err(DeliveryModeError::Invalid)
        );
        assert_eq!(
            delivery_mode_from_config_bytes(b"not-json"),
            Err(DeliveryModeError::Invalid)
        );
    }

    #[test]
    fn tool_driven_replaces_text_only() {
        let text = DeliveryEffect::Text {
            body: "hi".into(),
            stopped_by_limit: false,
            tool_calls_made: 0,
            iterations: 1,
        };
        assert_eq!(
            adjust_inbound_effect(DeliveryMode::ToolDriven, text.clone()),
            DeliveryEffect::NoReply
        );
        assert_eq!(adjust_inbound_effect(DeliveryMode::Say, text.clone()), text);
        assert_eq!(
            adjust_inbound_effect(DeliveryMode::ToolDriven, DeliveryEffect::NoReply),
            DeliveryEffect::NoReply
        );
        assert_eq!(
            adjust_inbound_effect(DeliveryMode::ToolDriven, DeliveryEffect::Empty),
            DeliveryEffect::Empty
        );
        assert_eq!(
            adjust_inbound_effect(
                DeliveryMode::ToolDriven,
                DeliveryEffect::Failed { error: "x".into() }
            ),
            DeliveryEffect::Failed { error: "x".into() }
        );
    }

    #[test]
    fn tool_driven_does_not_dispatch_v3_say() {
        assert!(!dispatches_v3_say(DeliveryMode::ToolDriven));
        assert!(dispatches_v3_say(DeliveryMode::Say));
    }
}
