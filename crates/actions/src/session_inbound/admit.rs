use super::debounce::PrivilegeFire;
use super::normalize::NormalizedInboundEvent;
use crate::session_watch_policy::{
    watch_author_standing, watch_hold_interval_secs, SessionPolicyError, WatchAllowSets,
};
use crate::CallerIdentity;

/// メッセージ全体を落とす理由（DM 事前ゲート）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundMessageDrop {
    /// どのエージェントも送信者を信頼していない。
    DmNotTrusted,
}

/// エージェント 1 体分を落とす理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundAgentDrop {
    DmNotTrustedForAgent,
    ChannelNotWhitelisted,
}

/// 誰か・権限の照会。3 アダプタ型は置かない。計算本体は runner の関数を渡す。
pub struct InboundLookups<'a> {
    pub resolve_caller: &'a dyn Fn(&str, &[String], &str) -> CallerIdentity,
    pub dm_allowed_any: &'a dyn Fn(&str, &[String], &str) -> bool,
    pub dm_allowed: &'a dyn Fn(&str, &str, &str) -> bool,
    pub channel_whitelisted: &'a dyn Fn(&str, &str) -> bool,
}

/// [`accept_inbound`] が 1 件分として受ける正規化イベント。
#[derive(Debug, Clone, Copy)]
pub struct InboundWork<'a> {
    pub event: NormalizedInboundEvent<'a>,
    pub has_content: bool,
    pub kind_label: &'a str,
    pub author_key: &'a str,
}

/// watch 束の追加材料。無ければ対話系（discord / web）。
pub struct WatchAccept<'a, T> {
    pub policy_json: &'a str,
    pub interval_secs: u64,
    pub allow: WatchAllowSets<'a>,
    pub owner: &'a std::collections::HashSet<String>,
    pub followees: &'a std::collections::HashSet<String>,
    /// `Some` = 対話系（core が権限デバウンスし時限発火）。`None` = 機械束ね flush（抱えない）。
    pub privilege: Option<&'a PrivilegeFire<T>>,
}

/// 通した 1 件。ゲートはこれを見て配送（ログ）する。ターン対象は `on_run` だけ。
/// ターンの文脈に含めた件は `on_run` の第 3 引数（読んだ事実。判定中間値ではない）。
#[derive(Debug, Clone)]
pub struct AdmittedInbound {
    pub caller: CallerIdentity,
    pub admitted_agent_ids: Vec<String>,
    pub agent_drops: Vec<(String, InboundAgentDrop)>,
}

impl AdmittedInbound {
    pub fn agent_drop(&self, agent_id: &str) -> Option<InboundAgentDrop> {
        self.agent_drops
            .iter()
            .find(|(id, _)| id == agent_id)
            .map(|(_, reason)| *reason)
    }
}

/// 束全体を落とす理由（1 件の対話系で DM 不信頼のときだけ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundDrop {
    Message(InboundMessageDrop),
    Policy(SessionPolicyError),
}

impl std::fmt::Display for InboundDrop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(d) => write!(f, "{d:?}"),
            Self::Policy(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for InboundDrop {}

/// DM の事前ゲート（「いずれかのエージェントが信頼していれば通す」）。
///
/// [`accept_inbound`] が [`InboundLookups::dm_allowed_any`] の結果を渡す。非 DM は常に通す。
///
/// ゲートは呼ばない。[`accept_inbound`] が内部で使う。
fn admit_inbound_message(is_dm: bool, dm_allowed_any: bool) -> Result<(), InboundMessageDrop> {
    if is_dm && !dm_allowed_any {
        Err(InboundMessageDrop::DmNotTrusted)
    } else {
        Ok(())
    }
}

/// エージェント個別の権限ゲート（DM 個別信頼 / チャンネル whitelist）。
///
/// ゲートは呼ばない。[`accept_inbound`] が内部で使う。
fn admit_inbound_agent(
    is_dm: bool,
    dm_allowed: bool,
    channel_whitelisted: bool,
) -> Result<(), InboundAgentDrop> {
    if is_dm {
        if !dm_allowed {
            return Err(InboundAgentDrop::DmNotTrustedForAgent);
        }
        return Ok(());
    }
    if !channel_whitelisted {
        return Err(InboundAgentDrop::ChannelNotWhitelisted);
    }
    Ok(())
}

fn admit_one(
    lookups: &InboundLookups<'_>,
    event: &NormalizedInboundEvent<'_>,
    owner_id: &str,
    agent_ids: &[String],
) -> Result<AdmittedInbound, InboundMessageDrop> {
    let is_dm = event.guild_id.is_empty();
    admit_inbound_message(
        is_dm,
        (lookups.dm_allowed_any)(event.sender_id, agent_ids, owner_id),
    )?;
    let caller = (lookups.resolve_caller)(event.sender_id, agent_ids, owner_id);
    let mut admitted_agent_ids = Vec::new();
    let mut agent_drops = Vec::new();
    for agent_id in agent_ids {
        match admit_inbound_agent(
            is_dm,
            (lookups.dm_allowed)(event.sender_id, agent_id, owner_id),
            (lookups.channel_whitelisted)(event.channel_id, agent_id),
        ) {
            Ok(()) => admitted_agent_ids.push(agent_id.clone()),
            Err(reason) => agent_drops.push((agent_id.clone(), reason)),
        }
    }
    Ok(AdmittedInbound {
        caller,
        admitted_agent_ids,
        agent_drops,
    })
}

/// 唯一の inbound 入り口。ゲートは正規化イベントの束を 1 回投げる。
///
/// - 対話系（`watch` 無し）: trust 分割（Q13）。`on_admitted` は通した件、`on_run` はトリガー。
///   `on_run` の第 3 引数は、そのターンの文脈に含めた受信の元 index（record-only を含む）。
/// - watch 即時（`privilege` あり）: 許可集合・standing・権限デバウンス。抱えた件は
///   [`PrivilegeFire`] が時限発火する（callback しない）。発火時の件が文脈。
/// - watch 束 flush（`privilege` 無し）: 許可集合。通した最後だけ `on_run`（文脈は通した全件）。
///
/// `take_hold` は権限デバウンスに抱えるときだけ呼ばれる。
#[allow(clippy::too_many_arguments)]
pub fn accept_inbound<T: Send + 'static>(
    items: &[InboundWork<'_>],
    owner_id: &str,
    agent_ids: &[String],
    lookups: &InboundLookups<'_>,
    watch: Option<WatchAccept<'_, T>>,
    mut take_hold: impl FnMut(usize) -> T,
    mut on_admitted: impl FnMut(usize, &AdmittedInbound),
    mut on_run: impl FnMut(usize, &AdmittedInbound, &[usize]),
) -> Result<(), InboundDrop> {
    let mut admitted: Vec<(usize, AdmittedInbound)> = Vec::new();
    let mut trust_at: Vec<Option<u8>> = vec![None; items.len()];
    for (i, item) in items.iter().enumerate() {
        if let Some(w) = watch.as_ref() {
            if !w.allow.is_allowed(item.author_key) {
                continue;
            }
        }
        let plan = match admit_one(lookups, &item.event, owner_id, agent_ids) {
            Ok(p) => p,
            Err(drop) => {
                if items.len() == 1 && watch.is_none() {
                    return Err(InboundDrop::Message(drop));
                }
                if watch.is_none() {
                    trust_at[i] = Some(
                        (lookups.resolve_caller)(item.event.sender_id, agent_ids, owner_id)
                            .trust_level(),
                    );
                }
                continue;
            }
        };
        trust_at[i] = Some(plan.caller.trust_level());
        if let Some(w) = watch.as_ref() {
            if let Some(fire) = w.privilege {
                let standing = watch_author_standing(item.author_key, w.owner, w.followees);
                let hold = watch_hold_interval_secs(
                    w.policy_json,
                    standing,
                    &plan.caller,
                    item.kind_label,
                    w.interval_secs,
                )
                .map_err(InboundDrop::Policy)?;
                if let Some(secs) = hold {
                    fire.hold(take_hold(i), plan.caller.clone(), secs);
                    continue;
                }
            }
        }
        admitted.push((i, plan));
    }

    let run_flags = if watch.as_ref().is_some_and(|w| w.privilege.is_none()) {
        let mut flags = vec![false; admitted.len()];
        if let Some(last) = flags.last_mut() {
            *last = true;
        }
        flags
    } else if watch.is_none() {
        let levels: Vec<u8> = trust_at
            .iter()
            .map(|t| t.expect("対話系の全件は admit または DM drop で trust を書いている"))
            .collect();
        let has_content: Vec<bool> = items.iter().map(|item| item.has_content).collect();
        let record_only = plan_record_only_flags(&levels, &has_content);
        admitted.iter().map(|(i, _)| !record_only[*i]).collect()
    } else {
        vec![true; admitted.len()]
    };

    for (j, (i, adm)) in admitted.iter().enumerate() {
        on_admitted(*i, adm);
        if run_flags[j] {
            let read = turn_read_indices(*i, &admitted, watch.as_ref(), &trust_at);
            on_run(*i, adm, &read);
        }
    }
    Ok(())
}

/// ターン 1 本の文脈に含める受信の元 index。
///
/// 対話系: トリガーと同じ連続同権限グループのうち、通した件（record-only 含む）。
/// watch 束 flush: 通した全件。watch 即時: その 1 件。
fn turn_read_indices<T>(
    trigger: usize,
    admitted: &[(usize, AdmittedInbound)],
    watch: Option<&WatchAccept<'_, T>>,
    trust_at: &[Option<u8>],
) -> Vec<usize> {
    if watch.is_some_and(|w| w.privilege.is_none()) {
        return admitted.iter().map(|(i, _)| *i).collect();
    }
    if watch.is_some() {
        return vec![trigger];
    }
    let levels: Vec<u8> = trust_at
        .iter()
        .map(|t| t.expect("対話系の全件は admit または DM drop で trust を書いている"))
        .collect();
    let groups = consecutive_trust_groups(&levels);
    let Some(&(start, len)) = groups
        .iter()
        .find(|&&(s, l)| trigger >= s && trigger < s + l)
    else {
        return vec![trigger];
    };
    let end = start + len;
    admitted
        .iter()
        .map(|(i, _)| *i)
        .filter(|&i| i >= start && i < end)
        .collect()
}

/// 連続した同一 trust_level の並びを `(開始 index, 長さ)` に切る。
///
/// 到着順を保ち、隣が変わったところで切る。移設前の `message_loop::consecutive_groups` と同一。
/// trust_level 分割後のターン本数と各 caller を現行どおり固定する（RULINGS Q13）。
pub fn consecutive_trust_groups(levels: &[u8]) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut start = 0;
    while start < levels.len() {
        let mut end = start + 1;
        while end < levels.len() && levels[end] == levels[start] {
            end += 1;
        }
        groups.push((start, end - start));
        start = end;
    }
    groups
}

/// グループごとに「内容のある最後」だけ run トリガー、他は record-only。
///
/// `true` = 記録だけ（run しない）。現行フラッシュの `record_only_flags` と同一。
/// `levels` と `has_content` の長さが違うときは panic する（呼ばれ方の契約。
/// 長さ不一致はバグなので黙って空を返さない）。
pub fn plan_record_only_flags(levels: &[u8], has_content: &[bool]) -> Vec<bool> {
    if levels.len() != has_content.len() {
        panic!(
            "plan_record_only_flags: levels ({}) and has_content ({}) length mismatch",
            levels.len(),
            has_content.len()
        );
    }
    let groups = consecutive_trust_groups(levels);
    let mut record_only = vec![true; levels.len()];
    for &(start, len) in &groups {
        if let Some(rel) = has_content[start..start + len].iter().rposition(|c| *c) {
            record_only[start + rel] = false;
        }
    }
    record_only
}

#[cfg(test)]
#[path = "admit_tests.rs"]
mod tests;
