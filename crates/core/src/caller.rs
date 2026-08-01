//! run の呼び出し元識別子。
//!
//! 権限ゲート（`opencrab_actions::bridge::policy_allows`）が owner_only / trusted_only の
//! ツールを list_tools からも dispatch からも落とす判定に使う。
//!
//! **ここ（core）に置いてある理由**: A2UI の保留状態
//! （[`crate::a2ui::PendingInteraction`]）が「その UI を描いた run の呼び出し元」を
//! 保持する（#298 / #302）。core は `opencrab-actions` にも `opencrab-gateway` にも
//! 依存できない（両者が core に依存している）ので、共通の型はここにしか置けない。
//! `opencrab_actions::CallerIdentity` はこの型の再エクスポートで、
//! `opencrab_gateway::GatewayCaller` からの写像は `opencrab-gateway` 側の
//! `From` 実装が担う。

/// 呼び出し元の識別子
#[derive(Debug, Clone, PartialEq)]
pub enum CallerIdentity {
    Owner,
    Agent,
    CoAgent { agent_id: String },
    TrustedUser,
}
