fn parse_say(obj: &Value) -> Result<Say, FrameError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let binding_id = parse_uuid(&require_str(obj, "binding_id")?)?;
    let payload = obj.get("payload").cloned().ok_or(FrameError::BadRequest)?;
    if !payload.is_object() {
        return Err(FrameError::BadRequest);
    }
    say_reply_target(&payload)?;
    let payload_digest = parse_digest(&require_str(obj, "payload_digest")?)?;
    let delivery_guarantee = DeliveryGuarantee::parse(&require_str(obj, "delivery_guarantee")?)
        .ok_or(FrameError::BadRequest)?;
    let adapter_protocol_digest = parse_digest(&require_str(obj, "adapter_protocol_digest")?)?;
    Ok(Say {
        id,
        binding_id,
        payload,
        payload_digest,
        delivery_guarantee,
        adapter_protocol_digest,
    })
}

fn parse_delivery_ack(obj: &Value) -> Result<DeliveryAck, FrameError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let outcome = require_str(obj, "outcome")?;
    if !matches!(outcome.as_str(), "receipted" | "failed" | "indeterminate") {
        return Err(FrameError::BadRequest);
    }
    Ok(DeliveryAck { id, outcome })
}

