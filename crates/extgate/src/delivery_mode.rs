//! 旧 instance config の optional `delivery_mode` 互換層。
//! 現行契約では `delivery_mode` で本文配送を切り替えない。

use opencrab_actions::DeliveryEffect;
use serde_json::Value;

use crate::operations::FinalDelivery;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    Say,
    ToolDriven,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryModeError {
    Invalid,
}

pub fn delivery_mode_from_final_delivery(final_delivery: FinalDelivery) -> DeliveryMode {
    match final_delivery {
        FinalDelivery::Automatic => DeliveryMode::Say,
        FinalDelivery::OperationDriven => DeliveryMode::ToolDriven,
    }
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

/// inbound 最終本文は delivery-mode 互換値に関係なく配送対象にする。
pub fn adjust_inbound_effect(_mode: DeliveryMode, effect: DeliveryEffect) -> DeliveryEffect {
    effect
}

/// 自発配送は delivery-mode 互換値に関係なく V3 say dispatcher へ渡す。
pub fn dispatches_v3_say(_mode: DeliveryMode) -> bool {
    true
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
    fn legacy_modes_do_not_suppress_text() {
        let text = DeliveryEffect::Text {
            body: "hi".into(),
            stopped_by_limit: false,
            tool_calls_made: 0,
            iterations: 1,
        };
        assert_eq!(
            adjust_inbound_effect(DeliveryMode::ToolDriven, text.clone()),
            text
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
    fn legacy_modes_dispatch_v3_say() {
        assert!(dispatches_v3_say(DeliveryMode::ToolDriven));
        assert!(dispatches_v3_say(DeliveryMode::Say));
    }
}
