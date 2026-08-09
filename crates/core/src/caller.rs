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
    /// **co_agent は owner 等価**（オーナー指示 2026-08-10 / #485。#330「ローカルの
    /// シェル / ファイル操作は owner のみ・co_agent / trusted にも開けない」を**覆す**）。
    ///
    /// > エージェント同士で進める時にオーナーのパーミッションが必要になって、全然
    /// > 自律的に前に進めない。等価にして。オーナーができる指示は全部できていい。
    ///
    /// **この判定が owner ゲートの唯一の源。** owner を要求する全レイヤ —
    /// L1 bridge policy（`opencrab_actions::bridge` の `caller_is_owner`）/
    /// L3 Action（`update_instructions`）/ L4 server ハンドラ（`configure_llm_provider` /
    /// `manage_allowed_commands` / `configure_nostr` / `configure_self` /
    /// `configure_mcp_server` / `add_allowed_command` / `remove_allowed_command` /
    /// ハートビート指示更新）/ L5 webhook 設定（set/disable）/ L6 voice（VC 参加/退出）—
    /// はすべてこの述語を通す。[`Self::trust_level`] / [`Self::skill_origin_tag`] も
    /// これに追従する。ここを唯一の源にすることで「同じ owner 判定が 2 箇所にあって
    /// 食い違う」事故を防ぐ。
    ///
    /// **widening のみ**（owner ゲートを緩めるだけで代替ゲートを足さない）。
    /// **`Agent` は不変** — 外部の未信頼ユーザー由来ターン（caller=Agent）からは従来
    /// どおり owner/trusted のツールを全て塞ぐ（意図せぬ自律実行 = #240 の再来を防ぐ）。
    pub fn is_owner_equivalent(&self) -> bool {
        matches!(self, CallerIdentity::Owner | CallerIdentity::CoAgent { .. })
    }

    /// 信頼度の序列。`bridge::policy_allows` の owner_only / trusted_only と同じ区分
    /// （owner{owner, co_agent} > trusted_user > agent）を数値化しただけで、新しい権限
    /// 概念ではない。
    ///
    /// co_agent が owner と同順位（=2）であることの唯一の源は
    /// [`Self::is_owner_equivalent`]（#485）。ここもそれへ追従させ、owner 等価の判定を
    /// 2 箇所に分けない。#485 より前は co_agent と trusted_user が同列（=1）だったが、
    /// co_agent を owner と同列（=2）へ引き上げた（[`Self::can_manage_subtask_of`] で
    /// co_agent が owner の subtask を管理できるようにするため）。trusted_user は据え置き。
    pub fn trust_level(&self) -> u8 {
        // owner / co_agent は同順位（最上位 = 2）。唯一の源は is_owner_equivalent。
        if self.is_owner_equivalent() {
            return 2;
        }
        match self {
            CallerIdentity::TrustedUser => 1,
            CallerIdentity::Agent => 0,
            // owner 等価（Owner / CoAgent）は上で return 済み。variant 追加時の網羅性
            // 検査のためだけに残す（実行時にはここへ来ない）。
            CallerIdentity::Owner | CallerIdentity::CoAgent { .. } => 2,
        }
    }

    /// スキルの **作成者 caller** を DB へ格納する短いタグ（trust class の射影）。
    ///
    /// `skills.created_caller` に入れる。`CoAgent { agent_id }` の agent_id は落として
    /// trust class（owner / trusted / agent）だけ残す。読み出し時のゲート
    /// （[`Self::may_exercise_skill`]）は trust_level しか見ないため、これで十分。
    /// 新しい権限概念ではなく、既存 [`CallerIdentity`] の trust_level の射影にすぎない。
    ///
    /// #485 で co_agent を owner 等価にしたため、co_agent 作成スキルは `"owner"` タグに
    /// なる（[`Self::is_owner_equivalent`] に追従）。owner ターンからも読める。
    pub fn skill_origin_tag(&self) -> &'static str {
        // co_agent は owner 等価（#485）→ owner タグに揃える。唯一の源は is_owner_equivalent。
        if self.is_owner_equivalent() {
            return "owner";
        }
        match self {
            CallerIdentity::TrustedUser => "trusted",
            CallerIdentity::Agent => "agent",
            // owner 等価は上で return 済み（網羅性検査のためだけに残す）。
            CallerIdentity::Owner | CallerIdentity::CoAgent { .. } => "owner",
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

    fn co_agent() -> CallerIdentity {
        CallerIdentity::CoAgent {
            agent_id: "x".into(),
        }
    }

    #[test]
    fn skill_origin_tag_projects_trust_class() {
        assert_eq!(CallerIdentity::Owner.skill_origin_tag(), "owner");
        assert_eq!(CallerIdentity::TrustedUser.skill_origin_tag(), "trusted");
        // #485: co_agent は owner 等価 → owner タグ（旧: "trusted"）。
        assert_eq!(co_agent().skill_origin_tag(), "owner");
        assert_eq!(CallerIdentity::Agent.skill_origin_tag(), "agent");
    }

    /// #485: co_agent は owner 等価。ここが緩むと全 owner ゲートが同時に破れる（唯一の源）。
    #[test]
    fn co_agent_is_owner_equivalent() {
        assert!(CallerIdentity::Owner.is_owner_equivalent());
        assert!(co_agent().is_owner_equivalent());
        // agent / trusted_user は owner 等価ではない（widening の境界）。
        assert!(!CallerIdentity::Agent.is_owner_equivalent());
        assert!(!CallerIdentity::TrustedUser.is_owner_equivalent());
    }

    /// #485: trust_level も owner 等価に追従（co_agent = owner = 2 > trusted_user = 1 > agent = 0）。
    #[test]
    fn trust_level_follows_owner_equivalence() {
        assert_eq!(CallerIdentity::Owner.trust_level(), 2);
        assert_eq!(co_agent().trust_level(), 2);
        assert_eq!(CallerIdentity::TrustedUser.trust_level(), 1);
        assert_eq!(CallerIdentity::Agent.trust_level(), 0);
    }

    /// #485: co_agent は owner の subtask を管理できる（協働の要）。agent はできない。
    #[test]
    fn co_agent_can_manage_owner_subtask() {
        assert!(co_agent().can_manage_subtask_of(&CallerIdentity::Owner));
        assert!(!CallerIdentity::Agent.can_manage_subtask_of(&CallerIdentity::Owner));
        // trusted_user は owner の subtask を管理できない（据え置き）。
        assert!(!CallerIdentity::TrustedUser.can_manage_subtask_of(&CallerIdentity::Owner));
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
