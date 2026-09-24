//! Platform-neutral co-agent authority carried across execution boundaries.

/// Gateway-projected evidence for one internal co-agent relationship.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationshipAuthority {
    pub co_agent_id: String,
    pub relationship_revision: u64,
}

/// The seven execution/emission boundaries required by the gateway ownership design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationBoundary {
    InitialModelTurn,
    QueueDequeueRetry,
    ToolInvocation,
    AutomaticContinuation,
    OperationDrivenContinuation,
    TimedSubtaskContinuation,
    OutboundDeliveryCommit,
}

impl AuthorizationBoundary {
    pub const ALL: [Self; 7] = [
        Self::InitialModelTurn,
        Self::QueueDequeueRetry,
        Self::ToolInvocation,
        Self::AutomaticContinuation,
        Self::OperationDrivenContinuation,
        Self::TimedSubtaskContinuation,
        Self::OutboundDeliveryCommit,
    ];
}

/// Revalidation callback. `false` fails closed before the covered side effect.
pub type AuthorizationCheck =
    std::sync::Arc<dyn Fn(AuthorizationBoundary) -> bool + Send + Sync + 'static>;
