#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Say {
    pub id: String,
    pub binding_id: String,
    pub payload: Value,
    pub payload_digest: String,
    pub delivery_guarantee: DeliveryGuarantee,
    pub adapter_protocol_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryAck {
    pub id: String,
    pub outcome: String,
}

