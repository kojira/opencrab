#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Receipted,
    Failed,
    Indeterminate,
}

impl InstanceClient {
    /// Report a durable gateway-ledger terminal result. Core commits its matching outcome before
    /// returning `DeliveryAcknowledged`; callers must not invoke this before the gateway store
    /// transaction commits.
    pub async fn complete_delivery(
        &self,
        delivery_id: &str,
        outcome: DeliveryOutcome,
    ) -> Result<(), CommandError> {
        let frame = match outcome {
            DeliveryOutcome::Receipted => ok_frame(delivery_id),
            DeliveryOutcome::Failed => err_frame(delivery_id, "external_rejected", None),
            DeliveryOutcome::Indeterminate => err_frame(delivery_id, "indeterminate", None),
        };
        if send_frame(self, frame).await {
            Ok(())
        } else {
            Err(CommandError::Disconnected)
        }
    }

}
