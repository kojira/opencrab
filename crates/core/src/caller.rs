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

impl CallerIdentity {
    /// 信頼度の序列。`bridge::policy_allows` の owner_only / trusted_only と同じ区分
    /// （owner > trusted{co_agent, trusted_user} > agent）を数値化しただけで、新しい
    /// 権限概念ではない。
    fn trust_level(&self) -> u8 {
        match self {
            CallerIdentity::Owner => 2,
            CallerIdentity::CoAgent { .. } | CallerIdentity::TrustedUser => 1,
            CallerIdentity::Agent => 0,
        }
    }

    /// このターンの呼び出し元が、`spawner`（subtask を spawn した親ターンの呼び出し元 /
    /// `SpawnedSubtask.caller`）が生み出した subtask を管理してよいか。
    ///
    /// 停止（`cancel_subtask`）と、親からの進捗代理報告（`report_progress` の親経路）で
    /// 使う。呼び出し元の信頼度が spawner 以上のときだけ許す。セッションを agent 単位で
    /// 1 本にした（#323）結果、「親セッション一致」だけでは素の Agent ターンから Owner 由来の
    /// subtask を触れてしまう（旧 per-相手 セッションでは別セッションで構造的に不可能だった）。
    /// それを塞ぐための判定（#331）。
    ///
    /// **subtask 本人（depth>=1 の自己申告 = 自セッション一致）には適用しない**（呼び出し側で
    /// self 経路を先に許可する）。ここへ適用するとサブエージェント自身の進捗報告が壊れる。
    pub fn can_manage_subtask_of(&self, spawner: &CallerIdentity) -> bool {
        self.trust_level() >= spawner.trust_level()
    }
}
