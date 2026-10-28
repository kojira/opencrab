/// D-1085: why a `provisional`, `provisional_cancel` (or D-1096 `interrupt`) request was not
/// acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionalError {
    NotReady,
    Rejected { code: String },
    Disconnected,
}

impl InstanceClient {
    /// Starts a provisional run for text this gateway expects to commit as a realtime said with
    /// the same inputs and `SaidContext.provisional_id`. Its partials arrive as
    /// `LiveEvent::Partial` with `provisional_id`; nothing is recorded or delivered until the said
    /// promotes it.
    pub async fn provisional(
        &self,
        address: &str,
        request: &ProvisionalRequest,
    ) -> Result<(), ProvisionalError> {
        let binding_id = self.provisional_binding(address).await?;
        let id = self.next_id();
        self.provisional_request(&id, provisional_frame(&id, &binding_id, request))
            .await
    }

    /// Aborts a provisional run. Idempotent; once acknowledged, no partial of the run follows.
    pub async fn provisional_cancel(
        &self,
        address: &str,
        provisional_id: &str,
    ) -> Result<(), ProvisionalError> {
        let binding_id = self.provisional_binding(address).await?;
        let id = self.next_id();
        self.provisional_request(
            &id,
            provisional_cancel_frame(&id, &binding_id, provisional_id),
        )
        .await
    }

    /// D-1096: stops the running turn of `address`'s session. `delivered` is the part of the
    /// agent's last utterance that reached the other side; core records it. Acknowledged once
    /// the turn has stopped and the cut is recorded (also when no turn was running).
    pub async fn interrupt(
        &self,
        address: &str,
        delivered: Option<&str>,
    ) -> Result<(), ProvisionalError> {
        let binding_id = self.provisional_binding(address).await?;
        let id = self.next_id();
        self.provisional_request(&id, interrupt_frame(&id, &binding_id, delivered))
            .await
    }

    async fn provisional_binding(&self, address: &str) -> Result<String, ProvisionalError> {
        let inner = self.inner.lock().await;
        if inner.closed {
            return Err(ProvisionalError::NotReady);
        }
        inner
            .acknowledged
            .get(address)
            .cloned()
            .ok_or(ProvisionalError::NotReady)
    }

    async fn provisional_request(&self, id: &str, frame: Value) -> Result<(), ProvisionalError> {
        let (tx, rx) = oneshot::channel();
        self.inner.lock().await.pending_said.insert(
            id.to_string(),
            PendingSaid {
                kind: PendingKind::Command,
                reply: tx,
            },
        );
        if !send_frame(self, frame).await {
            self.inner.lock().await.pending_said.remove(id);
            return Err(ProvisionalError::Disconnected);
        }
        match tokio::time::timeout(SAID_TIMEOUT, rx).await {
            Ok(Ok(SaidOutcome::Accepted { .. })) => Ok(()),
            Ok(Ok(SaidOutcome::WireErr { code, .. })) => Err(ProvisionalError::Rejected { code }),
            Ok(Ok(SaidOutcome::NotAdmitted | SaidOutcome::Disconnected)) | Ok(Err(_)) => {
                Err(ProvisionalError::Disconnected)
            }
            Err(_) => {
                self.inner.lock().await.pending_said.remove(id);
                Err(ProvisionalError::Disconnected)
            }
        }
    }
}
