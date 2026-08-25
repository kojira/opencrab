//! セッション watch の場ポリシー（載せ替え工程 5-a / §4.6）。
//!
//! ゲートはイベントの形だけを見て即時転送 / 束ねする。「誰か」と
//! 即応 / 権限毎デバウンスはここ（core）が決める。
//!
//! `policy_json == '{}'` の実行時は AGREED 逐語:
//! オーナー npub とフォロイーのリプライ / メンション / リアクションには即反応。
//! リポストは AGREED の即応集合に無い → 即応せず watch 間隔でデバウンス。
//! CoAgent 等価は権限ゲートの話であり、即応の「誰の投稿か」を co_agent に拡張しない。

use std::collections::{HashMap, HashSet};

use crate::session_inbound::{
    plan_inbound, InboundIdentity, InboundMessageDrop, InboundPlan, NormalizedInboundEvent,
};
use crate::CallerIdentity;

/// ゲートが付ける機械的種別ラベルの閉集合（§1.2.1）。
pub const WATCH_KIND_LABELS: &[&str] =
    &["リプライ", "メンション", "リポスト", "リアクション", "長文"];

/// `'{}'` のとき AGREED が即応する種別。リポストは含めない。
pub const AGREED_IMMEDIATE_KINDS: &[&str] = &["リプライ", "メンション", "リアクション"];

/// 権限クラスのキー（`CallerIdentity` の variant 名）。
pub const POLICY_CLASS_KEYS: &[&str] = &["Owner", "CoAgent", "TrustedUser", "Agent"];

/// `'{}'` 用の「誰の投稿か」（AGREED 即応の standing）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchAuthorStanding {
    Owner,
    Followee,
    Other,
}

/// ポリシー判定の結果。間隔は秒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchTurnDecision {
    Immediate,
    Debounce { interval_secs: u64 },
}

/// `policy_json` の読み取り / 形の誤り。欠けたクラスを補完しない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPolicyError {
    InvalidJson(String),
    MissingClass(String),
    UnknownClass(String),
    UnknownImmediateKind(String),
    InvalidDebounce(String),
}

impl std::fmt::Display for SessionPolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson(s) => write!(f, "policy_json が JSON として読めない: {s}"),
            Self::MissingClass(s) => write!(f, "policy_json に権限クラス {s} が無い（補完しない）"),
            Self::UnknownClass(s) => write!(f, "policy_json に未知の権限クラス {s}"),
            Self::UnknownImmediateKind(s) => {
                write!(f, "policy_json.immediate が閉集合外: {s}")
            }
            Self::InvalidDebounce(s) => write!(f, "policy_json.debounce_secs が不正: {s}"),
        }
    }
}

impl std::error::Error for SessionPolicyError {}

/// 1 権限クラス分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassPolicy {
    pub debounce_secs: u64,
    pub immediate: HashSet<String>,
}

/// 非空 `policy_json`。4 クラスすべて必須。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWatchPolicy {
    pub classes: HashMap<String, ClassPolicy>,
}

/// `CallerIdentity` → policy_json のクラスキー。
pub fn caller_policy_key(caller: &CallerIdentity) -> &'static str {
    match caller {
        CallerIdentity::Owner => "Owner",
        CallerIdentity::CoAgent { .. } => "CoAgent",
        CallerIdentity::TrustedUser => "TrustedUser",
        CallerIdentity::Agent => "Agent",
    }
}

/// 許可集合（フォロイー ∪ owner ∪ co_agent ∪ trusted_users / #698）。
///
/// 材料は呼び出し側が渡す。ここは照合だけ（ゲートに「誰か」を残さない）。
#[derive(Debug, Clone)]
pub struct WatchAllowSets<'a> {
    pub followees: &'a HashSet<String>,
    pub owner: &'a HashSet<String>,
    pub co_agents: &'a HashSet<String>,
    pub trusted_users: &'a HashSet<String>,
}

impl WatchAllowSets<'_> {
    pub fn is_allowed(&self, author_key: &str) -> bool {
        self.followees.contains(author_key)
            || self.owner.contains(author_key)
            || self.co_agents.contains(author_key)
            || self.trusted_users.contains(author_key)
    }
}

/// AGREED 即応の standing。owner を先に見る。co_agent は Other（拡張しない）。
pub fn watch_author_standing(
    author_key: &str,
    owner: &HashSet<String>,
    followees: &HashSet<String>,
) -> WatchAuthorStanding {
    if owner.contains(author_key) {
        WatchAuthorStanding::Owner
    } else if followees.contains(author_key) {
        WatchAuthorStanding::Followee
    } else {
        WatchAuthorStanding::Other
    }
}

/// `policy_json` を読む。`'{}'` は `None`（未設定）。非空は 4 クラス必須。
pub fn parse_session_watch_policy(
    policy_json: &str,
) -> Result<Option<SessionWatchPolicy>, SessionPolicyError> {
    let value: serde_json::Value = serde_json::from_str(policy_json)
        .map_err(|e| SessionPolicyError::InvalidJson(e.to_string()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| SessionPolicyError::InvalidJson("object ではない".into()))?;
    if obj.is_empty() {
        return Ok(None);
    }
    for key in obj.keys() {
        if !POLICY_CLASS_KEYS.contains(&key.as_str()) {
            return Err(SessionPolicyError::UnknownClass(key.clone()));
        }
    }
    let mut classes = HashMap::new();
    for &key in POLICY_CLASS_KEYS {
        let entry = obj
            .get(key)
            .ok_or_else(|| SessionPolicyError::MissingClass(key.to_string()))?;
        classes.insert(key.to_string(), parse_class_policy(key, entry)?);
    }
    Ok(Some(SessionWatchPolicy { classes }))
}

fn parse_class_policy(
    key: &str,
    value: &serde_json::Value,
) -> Result<ClassPolicy, SessionPolicyError> {
    let obj = value
        .as_object()
        .ok_or_else(|| SessionPolicyError::InvalidJson(format!("{key} が object ではない")))?;
    let debounce = obj.get("debounce_secs").ok_or_else(|| {
        SessionPolicyError::InvalidDebounce(format!("{key}: debounce_secs が無い"))
    })?;
    let debounce_secs = debounce.as_u64().ok_or_else(|| {
        SessionPolicyError::InvalidDebounce(format!("{key}: debounce_secs は 0 以上の整数"))
    })?;
    let immediate_val = obj.get("immediate").ok_or_else(|| {
        SessionPolicyError::UnknownImmediateKind(format!("{key}: immediate が無い"))
    })?;
    let arr = immediate_val.as_array().ok_or_else(|| {
        SessionPolicyError::UnknownImmediateKind(format!("{key}: immediate は配列"))
    })?;
    let mut immediate = HashSet::new();
    for item in arr {
        let label = item.as_str().ok_or_else(|| {
            SessionPolicyError::UnknownImmediateKind(format!(
                "{key}: immediate 要素が文字列ではない"
            ))
        })?;
        if !WATCH_KIND_LABELS.contains(&label) {
            return Err(SessionPolicyError::UnknownImmediateKind(label.to_string()));
        }
        immediate.insert(label.to_string());
    }
    Ok(ClassPolicy {
        debounce_secs,
        immediate,
    })
}

/// 即応するか / 何秒デバウンスするかを決める（core）。
///
/// `watch_interval_secs` は watch 行の必須間隔。`'{}'` の非即応側に使う。
/// 0 をここに渡さない（呼び出し側が watch 行を読めないなら起動エラー）。
pub fn decide_watch_turn(
    policy_json: &str,
    standing: WatchAuthorStanding,
    caller: &CallerIdentity,
    kind_label: &str,
    watch_interval_secs: u64,
) -> Result<WatchTurnDecision, SessionPolicyError> {
    if watch_interval_secs == 0 {
        return Err(SessionPolicyError::InvalidDebounce(
            "watch interval_secs が 0（既定値は使わない）".into(),
        ));
    }
    match parse_session_watch_policy(policy_json)? {
        None => Ok(decide_agreed_default(
            standing,
            kind_label,
            watch_interval_secs,
        )),
        Some(policy) => {
            let key = caller_policy_key(caller);
            let class = policy
                .classes
                .get(key)
                .ok_or_else(|| SessionPolicyError::MissingClass(key.to_string()))?;
            if class.immediate.contains(kind_label) {
                return Ok(WatchTurnDecision::Immediate);
            }
            if class.debounce_secs == 0 {
                return Ok(WatchTurnDecision::Immediate);
            }
            Ok(WatchTurnDecision::Debounce {
                interval_secs: class.debounce_secs,
            })
        }
    }
}

fn decide_agreed_default(
    standing: WatchAuthorStanding,
    kind_label: &str,
    watch_interval_secs: u64,
) -> WatchTurnDecision {
    let agreed_who = matches!(
        standing,
        WatchAuthorStanding::Owner | WatchAuthorStanding::Followee
    );
    if agreed_who && AGREED_IMMEDIATE_KINDS.contains(&kind_label) {
        WatchTurnDecision::Immediate
    } else {
        WatchTurnDecision::Debounce {
            interval_secs: watch_interval_secs,
        }
    }
}

/// watch inbound の plan。`plan_inbound`（4-a/4-b と同じ口）で caller を決める。
///
/// #698 許可集合外は `Err(InboundMessageDrop)` 相当として `None` の許可落ちを返す。
/// Discord の DM 事前ゲートとは別（Nostr DM はゲートが破棄済み）。
pub fn plan_watch_inbound<I: InboundIdentity>(
    identity: &I,
    event: &NormalizedInboundEvent<'_>,
    owner_id: &str,
    agent_ids: &[String],
    author_key: &str,
    allow: &WatchAllowSets<'_>,
) -> Result<InboundPlan, WatchInboundDrop> {
    if !allow.is_allowed(author_key) {
        return Err(WatchInboundDrop::NotAllowed);
    }
    plan_inbound(identity, event, owner_id, agent_ids).map_err(WatchInboundDrop::Inbound)
}

/// watch 経路で落とす理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchInboundDrop {
    NotAllowed,
    Inbound(InboundMessageDrop),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_set(hex: &str) -> HashSet<String> {
        HashSet::from([hex.to_string()])
    }

    fn followees(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn empty_policy_is_unset() {
        assert!(parse_session_watch_policy("{}").unwrap().is_none());
    }

    #[test]
    fn empty_policy_agreed_immediate_for_owner_and_followee() {
        for kind in AGREED_IMMEDIATE_KINDS {
            assert_eq!(
                decide_watch_turn(
                    "{}",
                    WatchAuthorStanding::Owner,
                    &CallerIdentity::Owner,
                    kind,
                    60
                )
                .unwrap(),
                WatchTurnDecision::Immediate,
                "owner {kind}"
            );
            assert_eq!(
                decide_watch_turn(
                    "{}",
                    WatchAuthorStanding::Followee,
                    &CallerIdentity::Agent,
                    kind,
                    60
                )
                .unwrap(),
                WatchTurnDecision::Immediate,
                "followee {kind}"
            );
        }
    }

    #[test]
    fn empty_policy_repost_is_not_immediate() {
        assert_eq!(
            decide_watch_turn(
                "{}",
                WatchAuthorStanding::Owner,
                &CallerIdentity::Owner,
                "リポスト",
                90
            )
            .unwrap(),
            WatchTurnDecision::Debounce { interval_secs: 90 }
        );
        assert_eq!(
            decide_watch_turn(
                "{}",
                WatchAuthorStanding::Followee,
                &CallerIdentity::Agent,
                "リポスト",
                90
            )
            .unwrap(),
            WatchTurnDecision::Debounce { interval_secs: 90 }
        );
    }

    #[test]
    fn empty_policy_does_not_extend_immediate_to_co_agent() {
        assert_eq!(
            watch_author_standing("co1", &owner_set("owner1"), &followees(&[])),
            WatchAuthorStanding::Other
        );
        assert_eq!(
            decide_watch_turn(
                "{}",
                WatchAuthorStanding::Other,
                &CallerIdentity::CoAgent {
                    agent_id: "co".into()
                },
                "リプライ",
                60
            )
            .unwrap(),
            WatchTurnDecision::Debounce { interval_secs: 60 }
        );
    }

    #[test]
    fn empty_policy_trusted_and_stranger_debounce() {
        assert_eq!(
            decide_watch_turn(
                "{}",
                WatchAuthorStanding::Other,
                &CallerIdentity::TrustedUser,
                "リプライ",
                120
            )
            .unwrap(),
            WatchTurnDecision::Debounce { interval_secs: 120 }
        );
        assert_eq!(
            decide_watch_turn(
                "{}",
                WatchAuthorStanding::Other,
                &CallerIdentity::Agent,
                "メンション",
                120
            )
            .unwrap(),
            WatchTurnDecision::Debounce { interval_secs: 120 }
        );
    }

    #[test]
    fn empty_policy_rejects_zero_interval() {
        let err = decide_watch_turn(
            "{}",
            WatchAuthorStanding::Other,
            &CallerIdentity::Agent,
            "長文",
            0,
        )
        .unwrap_err();
        assert!(matches!(err, SessionPolicyError::InvalidDebounce(_)));
    }

    fn full_policy(immediate_owner: &[&str], debounce_agent: u64) -> String {
        serde_json::json!({
            "Owner": { "debounce_secs": 0, "immediate": immediate_owner },
            "CoAgent": { "debounce_secs": 0, "immediate": ["リプライ"] },
            "TrustedUser": { "debounce_secs": 120, "immediate": [] },
            "Agent": { "debounce_secs": debounce_agent, "immediate": [] },
        })
        .to_string()
    }

    #[test]
    fn nonempty_policy_requires_all_four_classes() {
        let missing = r#"{"Owner":{"debounce_secs":0,"immediate":[]}}"#;
        assert!(matches!(
            parse_session_watch_policy(missing),
            Err(SessionPolicyError::MissingClass(_))
        ));
    }

    #[test]
    fn nonempty_policy_rejects_unknown_class_and_kind() {
        let unknown = r#"{
            "Owner":{"debounce_secs":0,"immediate":[]},
            "CoAgent":{"debounce_secs":0,"immediate":[]},
            "TrustedUser":{"debounce_secs":0,"immediate":[]},
            "Agent":{"debounce_secs":0,"immediate":[]},
            "Guest":{"debounce_secs":0,"immediate":[]}
        }"#;
        assert!(matches!(
            parse_session_watch_policy(unknown),
            Err(SessionPolicyError::UnknownClass(_))
        ));
        let bad_kind = r#"{
            "Owner":{"debounce_secs":0,"immediate":["タイムライン"]},
            "CoAgent":{"debounce_secs":0,"immediate":[]},
            "TrustedUser":{"debounce_secs":0,"immediate":[]},
            "Agent":{"debounce_secs":0,"immediate":[]}
        }"#;
        assert!(matches!(
            parse_session_watch_policy(bad_kind),
            Err(SessionPolicyError::UnknownImmediateKind(_))
        ));
    }

    #[test]
    fn nonempty_policy_uses_class_immediate_and_debounce() {
        let json = full_policy(&["リプライ", "リポスト"], 300);
        assert_eq!(
            decide_watch_turn(
                &json,
                WatchAuthorStanding::Owner,
                &CallerIdentity::Owner,
                "リポスト",
                60
            )
            .unwrap(),
            WatchTurnDecision::Immediate
        );
        assert_eq!(
            decide_watch_turn(
                &json,
                WatchAuthorStanding::Other,
                &CallerIdentity::Agent,
                "リプライ",
                60
            )
            .unwrap(),
            WatchTurnDecision::Debounce { interval_secs: 300 }
        );
        // debounce_secs=0 かつ immediate 外 → 待ち 0 = 即応（値を発明しない）
        assert_eq!(
            decide_watch_turn(
                &json,
                WatchAuthorStanding::Owner,
                &CallerIdentity::Owner,
                "長文",
                60
            )
            .unwrap(),
            WatchTurnDecision::Immediate
        );
    }

    #[test]
    fn standing_owner_wins_over_followee() {
        let owner = owner_set("pk");
        let fol = followees(&["pk"]);
        assert_eq!(
            watch_author_standing("pk", &owner, &fol),
            WatchAuthorStanding::Owner
        );
    }

    #[test]
    fn allow_set_is_union() {
        let followee_set = followees(&["f1"]);
        let owner = owner_set("o1");
        let co = followees(&["c1"]);
        let trusted = followees(&["t1"]);
        let sets = WatchAllowSets {
            followees: &followee_set,
            owner: &owner,
            co_agents: &co,
            trusted_users: &trusted,
        };
        assert!(sets.is_allowed("f1"));
        assert!(sets.is_allowed("o1"));
        assert!(sets.is_allowed("c1"));
        assert!(sets.is_allowed("t1"));
        assert!(!sets.is_allowed("stranger"));
    }

    struct MockIdentity {
        caller: CallerIdentity,
    }

    impl InboundIdentity for MockIdentity {
        fn resolve_caller(
            &self,
            _sender_id: &str,
            _agent_ids: &[String],
            _owner_id: &str,
        ) -> CallerIdentity {
            self.caller.clone()
        }
        fn dm_allowed_any(&self, _s: &str, _a: &[String], _o: &str) -> bool {
            true
        }
        fn dm_allowed(&self, _s: &str, _a: &str, _o: &str) -> bool {
            true
        }
        fn is_channel_whitelisted_for_agent(&self, _c: &str, _a: &str) -> bool {
            true
        }
    }

    #[test]
    fn plan_watch_inbound_uses_plan_inbound_and_allow() {
        let followees = followees(&["auth"]);
        let empty = HashSet::new();
        let allow = WatchAllowSets {
            followees: &followees,
            owner: &empty,
            co_agents: &empty,
            trusted_users: &empty,
        };
        let id = MockIdentity {
            caller: CallerIdentity::Agent,
        };
        let ev = NormalizedInboundEvent {
            sender_id: "auth",
            channel_id: "nostr-a",
            guild_id: "nostr",
        };
        let plan = plan_watch_inbound(&id, &ev, "owner", &["a".into()], "auth", &allow).unwrap();
        assert_eq!(plan.caller, CallerIdentity::Agent);
        assert_eq!(plan.admitted_agent_ids, vec!["a".to_string()]);
        assert_eq!(
            plan_watch_inbound(&id, &ev, "owner", &["a".into()], "stranger", &allow).unwrap_err(),
            WatchInboundDrop::NotAllowed
        );
    }
}
