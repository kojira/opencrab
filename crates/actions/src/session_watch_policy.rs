//! セッション watch ポリシー（載せ替え工程 5-a / §4.6）。
//!
//! ゲートはイベントの形だけを見て即時転送 / 束ねする。「誰か」と
//! 即応 / 権限毎デバウンスは [`crate::session_inbound::accept_inbound`] が決める。
//! このモジュールはポリシーの読み取りと、core 内部の間隔計算だけを持つ。
//!
//! `policy_json == '{}'` の実行時は AGREED 逐語:
//! オーナー npub とフォロイーのリプライ / メンション / リアクションには即反応。
//! リポストは AGREED の即応集合に無い → 即応せず watch 間隔でデバウンス。
//! CoAgent 等価は権限ゲートの話であり、即応の「誰の投稿か」を co_agent に拡張しない。

use std::collections::{HashMap, HashSet};

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

/// 権限デバウンスの秒。`None` は即応。ゲートは呼ばない（`accept_inbound` が内部で使う）。
///
/// `watch_interval_secs` は watch 行の必須間隔。`'{}'` の非即応側に使う。
/// 0 をここに渡さない（呼び出し側が watch 行を読めないなら起動エラー）。
pub(crate) fn watch_hold_interval_secs(
    policy_json: &str,
    standing: WatchAuthorStanding,
    caller: &CallerIdentity,
    kind_label: &str,
    watch_interval_secs: u64,
) -> Result<Option<u64>, SessionPolicyError> {
    if watch_interval_secs == 0 {
        return Err(SessionPolicyError::InvalidDebounce(
            "watch interval_secs が 0（既定値は使わない）".into(),
        ));
    }
    match parse_session_watch_policy(policy_json)? {
        None => Ok(agreed_hold_secs(standing, kind_label, watch_interval_secs)),
        Some(policy) => {
            let key = caller_policy_key(caller);
            let class = policy
                .classes
                .get(key)
                .ok_or_else(|| SessionPolicyError::MissingClass(key.to_string()))?;
            if class.immediate.contains(kind_label) || class.debounce_secs == 0 {
                return Ok(None);
            }
            Ok(Some(class.debounce_secs))
        }
    }
}

fn agreed_hold_secs(
    standing: WatchAuthorStanding,
    kind_label: &str,
    watch_interval_secs: u64,
) -> Option<u64> {
    let agreed_who = matches!(
        standing,
        WatchAuthorStanding::Owner | WatchAuthorStanding::Followee
    );
    if agreed_who && AGREED_IMMEDIATE_KINDS.contains(&kind_label) {
        None
    } else {
        Some(watch_interval_secs)
    }
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
                watch_hold_interval_secs(
                    "{}",
                    WatchAuthorStanding::Owner,
                    &CallerIdentity::Owner,
                    kind,
                    60
                )
                .unwrap(),
                None,
                "owner {kind}"
            );
            assert_eq!(
                watch_hold_interval_secs(
                    "{}",
                    WatchAuthorStanding::Followee,
                    &CallerIdentity::Agent,
                    kind,
                    60
                )
                .unwrap(),
                None,
                "followee {kind}"
            );
        }
    }

    #[test]
    fn empty_policy_repost_is_not_immediate() {
        assert_eq!(
            watch_hold_interval_secs(
                "{}",
                WatchAuthorStanding::Owner,
                &CallerIdentity::Owner,
                "リポスト",
                90
            )
            .unwrap(),
            Some(90)
        );
        assert_eq!(
            watch_hold_interval_secs(
                "{}",
                WatchAuthorStanding::Followee,
                &CallerIdentity::Agent,
                "リポスト",
                90
            )
            .unwrap(),
            Some(90)
        );
    }

    #[test]
    fn empty_policy_does_not_extend_immediate_to_co_agent() {
        assert_eq!(
            watch_author_standing("co1", &owner_set("owner1"), &followees(&[])),
            WatchAuthorStanding::Other
        );
        assert_eq!(
            watch_hold_interval_secs(
                "{}",
                WatchAuthorStanding::Other,
                &CallerIdentity::CoAgent {
                    agent_id: "co".into()
                },
                "リプライ",
                60
            )
            .unwrap(),
            Some(60)
        );
    }

    #[test]
    fn empty_policy_trusted_and_stranger_debounce() {
        assert_eq!(
            watch_hold_interval_secs(
                "{}",
                WatchAuthorStanding::Other,
                &CallerIdentity::TrustedUser,
                "リプライ",
                120
            )
            .unwrap(),
            Some(120)
        );
        assert_eq!(
            watch_hold_interval_secs(
                "{}",
                WatchAuthorStanding::Other,
                &CallerIdentity::Agent,
                "メンション",
                120
            )
            .unwrap(),
            Some(120)
        );
    }

    #[test]
    fn empty_policy_rejects_zero_interval() {
        let err = watch_hold_interval_secs(
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
            watch_hold_interval_secs(
                &json,
                WatchAuthorStanding::Owner,
                &CallerIdentity::Owner,
                "リポスト",
                60
            )
            .unwrap(),
            None
        );
        assert_eq!(
            watch_hold_interval_secs(
                &json,
                WatchAuthorStanding::Other,
                &CallerIdentity::Agent,
                "リプライ",
                60
            )
            .unwrap(),
            Some(300)
        );
        // debounce_secs=0 かつ immediate 外 → 待ち 0 = 即応（値を発明しない）
        assert_eq!(
            watch_hold_interval_secs(
                &json,
                WatchAuthorStanding::Owner,
                &CallerIdentity::Owner,
                "長文",
                60
            )
            .unwrap(),
            None
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
}
