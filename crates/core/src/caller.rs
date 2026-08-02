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
    ///
    /// `CoAgent` と `TrustedUser` を同列（=1）に置いているため、この判定だけでは両者は
    /// 互いの subtask を管理できてしまう。ただし本判定を使う subtask ゲート（Nostr の
    /// 受信ターン）では現状どちらの caller も生成されない（受信は Owner / Agent のみ）ので、
    /// この同列扱いは実害が出ない。将来 CoAgent / TrustedUser をこの経路へ流す場合は、
    /// 両者を分離すべきか改めて検討すること。
    pub fn trust_level(&self) -> u8 {
        match self {
            CallerIdentity::Owner => 2,
            CallerIdentity::CoAgent { .. } | CallerIdentity::TrustedUser => 1,
            CallerIdentity::Agent => 0,
        }
    }

    /// スキルの **作成者 caller** を DB へ格納する短いタグ（trust class の射影）。
    ///
    /// `skills.created_caller` に入れる。`CoAgent { agent_id }` の agent_id は落として
    /// trust class（owner / trusted / agent）だけ残す。読み出し時のゲート
    /// （[`Self::may_exercise_skill`]）は trust_level しか見ないため、これで十分。
    /// 新しい権限概念ではなく、既存 [`CallerIdentity`] の trust_level の射影にすぎない。
    pub fn skill_origin_tag(&self) -> &'static str {
        match self {
            CallerIdentity::Owner => "owner",
            CallerIdentity::CoAgent { .. } | CallerIdentity::TrustedUser => "trusted",
            CallerIdentity::Agent => "agent",
        }
    }

    /// `skills.created_caller` タグ（[`Self::skill_origin_tag`]）を trust_level へ戻す。
    ///
    /// `None`（＝記録の無い既存スキル）と未知タグは **Owner 相当**として扱う
    /// （legacy grandfather）。この PR より前に作られた 58 個はすべて `created_caller`
    /// が NULL なので、Owner ターンからも従来どおり読める（＝既存が壊れない）。
    pub fn skill_origin_trust(tag: Option<&str>) -> u8 {
        match tag {
            Some("agent") => CallerIdentity::Agent.trust_level(),
            Some("trusted") => CallerIdentity::TrustedUser.trust_level(),
            // "owner" / None / 未知タグ = legacy grandfather → Owner 相当
            _ => CallerIdentity::Owner.trust_level(),
        }
    }

    /// このターンの呼び出し元（`self`）が、`origin_tag` の caller が作ったスキルの
    /// **本文（行動指針）を読んで実行に移して**よいか（`read_skill` のゲート / #335）。
    ///
    /// 許可条件は「**このターンの信頼度が作成者を超えないこと**」。より強いターン
    /// （例: caller=Owner の heartbeat）が、弱いターン（例: 外部 Nostr の caller=Agent）で
    /// 仕込まれたスキルの本文を借りて owner 権限のローカル操作へ届く confused deputy を
    /// 塞ぐ。逆向き（弱いターンが強いスキルを読む）は許すが、実際のアクションは
    /// dispatch 側の caller ゲート（`policy_allows`）で弾かれるため昇格は起きない。
    pub fn may_exercise_skill(&self, origin_tag: Option<&str>) -> bool {
        self.trust_level() <= CallerIdentity::skill_origin_trust(origin_tag)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_origin_tag_projects_trust_class() {
        assert_eq!(CallerIdentity::Owner.skill_origin_tag(), "owner");
        assert_eq!(CallerIdentity::TrustedUser.skill_origin_tag(), "trusted");
        assert_eq!(
            CallerIdentity::CoAgent {
                agent_id: "x".into()
            }
            .skill_origin_tag(),
            "trusted"
        );
        assert_eq!(CallerIdentity::Agent.skill_origin_tag(), "agent");
    }

    #[test]
    fn null_and_unknown_origin_grandfather_to_owner() {
        assert_eq!(
            CallerIdentity::skill_origin_trust(None),
            CallerIdentity::Owner.trust_level()
        );
        assert_eq!(
            CallerIdentity::skill_origin_trust(Some("legacy-unknown")),
            CallerIdentity::Owner.trust_level()
        );
    }

    #[test]
    fn may_exercise_blocks_stronger_turn_from_weaker_skill() {
        // Owner ターンは agent 作成スキルを実行できない（confused deputy 封じ）。
        assert!(!CallerIdentity::Owner.may_exercise_skill(Some("agent")));
        // Owner ターンは owner 作成 / legacy(NULL) は実行できる。
        assert!(CallerIdentity::Owner.may_exercise_skill(Some("owner")));
        assert!(CallerIdentity::Owner.may_exercise_skill(None));
        // 弱いターンは強いスキルを読めるが、昇格はしない（実アクションは dispatch で弾かれる）。
        assert!(CallerIdentity::Agent.may_exercise_skill(Some("owner")));
        assert!(CallerIdentity::Agent.may_exercise_skill(Some("agent")));
    }
}
