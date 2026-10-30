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
        session_key: &str,
        request: &ProvisionalRequest,
    ) -> Result<(), ProvisionalError> {
        self.provisional_ready().await?;
        let id = self.next_id();
        self.provisional_request(&id, provisional_frame(&id, session_key, request))
            .await
    }

    /// Aborts a provisional run. Idempotent; once acknowledged, no partial of the run follows.
    pub async fn provisional_cancel(
        &self,
        session_key: &str,
        provisional_id: &str,
    ) -> Result<(), ProvisionalError> {
        self.provisional_ready().await?;
        let id = self.next_id();
        self.provisional_request(&id, provisional_cancel_frame(&id, session_key, provisional_id))
        .await
    }

    /// D-1096: stops the running turn of `session_key`'s session. `delivered` is the part of the
    /// agent's last utterance that reached the other side; core records it. Acknowledged once
    /// the turn has stopped and the cut is recorded (also when no turn was running).
    pub async fn interrupt(
        &self,
        session_key: &str,
        delivered: Option<&str>,
    ) -> Result<(), ProvisionalError> {
        self.provisional_ready().await?;
        let id = self.next_id();
        self.provisional_request(&id, interrupt_frame(&id, session_key, delivered))
            .await
    }

    async fn provisional_ready(&self) -> Result<(), ProvisionalError> {
        if self.inner.lock().await.closed {
            return Err(ProvisionalError::NotReady);
        }
        Ok(())
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
            Ok(Ok(
                SaidOutcome::NotAdmitted | SaidOutcome::Disconnected | SaidOutcome::Opened { .. },
            ))
            | Ok(Err(_)) => {
                Err(ProvisionalError::Disconnected)
            }
            Err(_) => {
                self.inner.lock().await.pending_said.remove(id);
                Err(ProvisionalError::Disconnected)
            }
        }
    }
}
