use super::*;
use crate::CallerIdentity;

fn levels(cs: &[CallerIdentity]) -> Vec<u8> {
    cs.iter().map(|c| c.trust_level()).collect()
}

fn owner() -> CallerIdentity {
    CallerIdentity::Owner
}
fn co_agent() -> CallerIdentity {
    CallerIdentity::CoAgent {
        agent_id: "agent-a".to_string(),
    }
}
fn external() -> CallerIdentity {
    CallerIdentity::Agent
}

/// Q13: 現行 `consecutive_groups` の 4 ケースを core 側で固定する。
#[test]
fn trust_groups_match_current_consecutive_privilege_split() {
    assert_eq!(
        consecutive_trust_groups(&levels(&[owner(), external(), co_agent()])),
        vec![(0, 1), (1, 1), (2, 1)],
        "owner→外部→co_agent は 2,0,2 で 3 グループ"
    );
    assert_eq!(
        consecutive_trust_groups(&levels(&[owner(), co_agent(), external()])),
        vec![(0, 2), (2, 1)],
        "owner→co_agent は同権限(2)で合流"
    );
    assert_eq!(
        consecutive_trust_groups(&levels(&[owner(), owner()])),
        vec![(0, 2)],
        "同一 owner 連続は 1 グループ"
    );
    assert_eq!(
        consecutive_trust_groups(&levels(&[external(), external(), owner()])),
        vec![(0, 2), (2, 1)],
        "外部連続は 1 グループ、owner は別"
    );
}

/// 内容のある最後だけがトリガー。空グループは run しない。
#[test]
fn record_only_flags_trigger_latest_with_content() {
    // 同権限 2 通 + 末尾空 → 2 通目がトリガー（index 1）。
    let flags = plan_record_only_flags(&[1, 1, 1], &[true, true, false]);
    assert_eq!(flags, vec![true, false, true]);

    // 権限が違う 2 通はどちらもトリガー。
    let flags = plan_record_only_flags(&[2, 1], &[true, true]);
    assert_eq!(flags, vec![false, false]);

    // 全部空 → 全部 record-only（run 0）。
    let flags = plan_record_only_flags(&[2, 2], &[false, false]);
    assert_eq!(flags, vec![true, true]);
}

#[test]
fn dm_gate_drops_untrusted_sender() {
    assert_eq!(
        admit_inbound_message(true, false),
        Err(InboundMessageDrop::DmNotTrusted)
    );
    // 非 DM は事前ゲートを見ない。
    assert!(admit_inbound_message(false, false).is_ok());
}

#[test]
fn agent_gate_drops_untrusted_dm_and_non_whitelist() {
    assert_eq!(
        admit_inbound_agent(true, false, true),
        Err(InboundAgentDrop::DmNotTrustedForAgent)
    );
    assert_eq!(
        admit_inbound_agent(false, true, false),
        Err(InboundAgentDrop::ChannelNotWhitelisted)
    );
    assert!(admit_inbound_agent(true, true, false).is_ok());
    assert!(admit_inbound_agent(false, false, true).is_ok());
}

fn event<'a>(sender: &'a str, channel: &'a str, guild: &'a str) -> NormalizedInboundEvent<'a> {
    NormalizedInboundEvent {
        sender_id: sender,
        channel_id: channel,
        guild_id: guild,
    }
}

fn work<'a>(sender: &'a str, channel: &'a str, guild: &'a str) -> InboundWork<'a> {
    InboundWork {
        event: event(sender, channel, guild),
        has_content: true,
        kind_label: "",
        author_key: sender,
    }
}

fn accept_one(
    lookups: &InboundLookups<'_>,
    item: InboundWork<'_>,
    owner: &str,
    agents: &[String],
) -> Result<AdmittedInbound, InboundDrop> {
    let mut out = None;
    accept_inbound::<()>(
        &[item],
        owner,
        agents,
        lookups,
        None,
        |_| (),
        |_, adm| out = Some(adm.clone()),
        |_, _, _| {},
    )?;
    Ok(out.expect("通した件は on_admitted される"))
}

#[test]
fn accept_inbound_admits_and_resolves_caller() {
    let caller = CallerIdentity::Owner;
    let resolve = |_: &str, _: &[String], _: &str| caller.clone();
    let lookups = InboundLookups {
        resolve_caller: &resolve,
        dm_allowed_any: &|_, _, _| true,
        dm_allowed: &|_, _, _| true,
        channel_whitelisted: &|_, _| true,
    };
    let plan = accept_one(&lookups, work("u1", "ch", "g1"), "owner", &["a".into()]).unwrap();
    assert_eq!(plan.caller, CallerIdentity::Owner);
    assert_eq!(plan.admitted_agent_ids, vec!["a".to_string()]);
    assert!(plan.agent_drops.is_empty());
}

#[test]
fn accept_inbound_drops_untrusted_dm_per_agent() {
    let caller = CallerIdentity::TrustedUser;
    let resolve = |_: &str, _: &[String], _: &str| caller.clone();
    let lookups = InboundLookups {
        resolve_caller: &resolve,
        dm_allowed_any: &|_, _, _| true,
        dm_allowed: &|_, _, _| false,
        channel_whitelisted: &|_, _| true,
    };
    let plan = accept_one(&lookups, work("u1", "ch", ""), "owner", &["a".into()]).unwrap();
    assert_eq!(
        plan.agent_drop("a"),
        Some(InboundAgentDrop::DmNotTrustedForAgent)
    );
    assert!(plan.admitted_agent_ids.is_empty());
}

#[test]
fn accept_inbound_drops_non_whitelisted_channel() {
    let caller = CallerIdentity::Agent;
    let resolve = |_: &str, _: &[String], _: &str| caller.clone();
    let lookups = InboundLookups {
        resolve_caller: &resolve,
        dm_allowed_any: &|_, _, _| false,
        dm_allowed: &|_, _, _| false,
        channel_whitelisted: &|_, _| false,
    };
    let plan = accept_one(&lookups, work("u1", "ch", "g1"), "owner", &["a".into()]).unwrap();
    assert_eq!(
        plan.agent_drop("a"),
        Some(InboundAgentDrop::ChannelNotWhitelisted)
    );
    assert!(plan.admitted_agent_ids.is_empty());
}

#[test]
fn accept_inbound_drops_untrusted_dm_for_single_item() {
    let caller = CallerIdentity::Agent;
    let resolve = |_: &str, _: &[String], _: &str| caller.clone();
    let lookups = InboundLookups {
        resolve_caller: &resolve,
        dm_allowed_any: &|_, _, _| false,
        dm_allowed: &|_, _, _| false,
        channel_whitelisted: &|_, _| true,
    };
    let work = InboundWork {
        event: event("u1", "ch", ""),
        has_content: true,
        kind_label: "メンション",
        author_key: "u1",
    };
    let err = accept_inbound::<()>(
        &[work],
        "owner",
        &["a".into()],
        &lookups,
        None,
        |_| (),
        |_, _| {},
        |_, _, _| {},
    )
    .unwrap_err();
    assert_eq!(err, InboundDrop::Message(InboundMessageDrop::DmNotTrusted));
}

#[test]
fn accept_inbound_session_split_runs_latest_with_content() {
    let caller = CallerIdentity::TrustedUser;
    let resolve = |_: &str, _: &[String], _: &str| caller.clone();
    let lookups = InboundLookups {
        resolve_caller: &resolve,
        dm_allowed_any: &|_, _, _| true,
        dm_allowed: &|_, _, _| true,
        channel_whitelisted: &|_, _| true,
    };
    let items = [
        InboundWork {
            event: event("u1", "ch", "g1"),
            has_content: true,
            kind_label: "",
            author_key: "u1",
        },
        InboundWork {
            event: event("u2", "ch", "g1"),
            has_content: true,
            kind_label: "",
            author_key: "u2",
        },
        InboundWork {
            event: event("u3", "ch", "g1"),
            has_content: false,
            kind_label: "",
            author_key: "u3",
        },
    ];
    let mut admitted = Vec::new();
    let mut runs = Vec::new();
    let mut reads = Vec::new();
    accept_inbound::<()>(
        &items,
        "owner",
        &["a".into()],
        &lookups,
        None,
        |_| (),
        |i, _| admitted.push(i),
        |i, _, read| {
            runs.push(i);
            reads.push(read.to_vec());
        },
    )
    .unwrap();
    assert_eq!(admitted, vec![0, 1, 2]);
    assert_eq!(runs, vec![1], "同権限 3 通の内容あり最後だけ run");
    assert_eq!(
        reads,
        vec![vec![0, 1, 2]],
        "トリガーのターン文脈は同グループの通した全件（record-only 含む）"
    );
}

#[tokio::test]
async fn accept_inbound_watch_holds_non_immediate() {
    let caller = CallerIdentity::Owner;
    let resolve = |_: &str, _: &[String], _: &str| caller.clone();
    let lookups = InboundLookups {
        resolve_caller: &resolve,
        dm_allowed_any: &|_, _, _| true,
        dm_allowed: &|_, _, _| true,
        channel_whitelisted: &|_, _| true,
    };
    let owner = std::collections::HashSet::from(["aa".to_string()]);
    let followees = std::collections::HashSet::new();
    let empty = std::collections::HashSet::new();
    let allow = WatchAllowSets {
        followees: &followees,
        owner: &owner,
        co_agents: &empty,
        trusted_users: &empty,
    };
    let fire = PrivilegeFire::new(|_items: Vec<(usize, CallerIdentity)>| async {});
    let work = InboundWork {
        event: NormalizedInboundEvent {
            sender_id: "aa",
            channel_id: "nostr-a",
            guild_id: "nostr",
        },
        has_content: true,
        kind_label: "リポスト",
        author_key: "aa",
    };
    let mut held = Vec::new();
    let mut admitted = Vec::new();
    let mut runs = Vec::new();
    accept_inbound(
        &[work],
        "",
        &["a".into()],
        &lookups,
        Some(WatchAccept {
            policy_json: "{}",
            interval_secs: 60,
            allow,
            owner: &owner,
            followees: &followees,
            privilege: Some(&fire),
        }),
        |i| {
            held.push(i);
            i
        },
        |i, _| admitted.push(i),
        |i, _, _| runs.push(i),
    )
    .unwrap();
    assert_eq!(held, vec![0]);
    assert!(admitted.is_empty());
    assert!(runs.is_empty());
    assert_eq!(fire.held_intervals(), vec![60]);
    assert_eq!(fire.held_len(), 1);
}

/// row 116-117: record-only は on_run されない。同グループのトリガーの read にだけ入る。
#[test]
fn accept_inbound_record_only_is_read_with_the_trigger_not_alone() {
    let caller = CallerIdentity::TrustedUser;
    let resolve = |_: &str, _: &[String], _: &str| caller.clone();
    let lookups = InboundLookups {
        resolve_caller: &resolve,
        dm_allowed_any: &|_, _, _| true,
        dm_allowed: &|_, _, _| true,
        channel_whitelisted: &|_, _| true,
    };
    let items = [
        InboundWork {
            event: event("u1", "ch", "g1"),
            has_content: true,
            kind_label: "",
            author_key: "u1",
        },
        InboundWork {
            event: event("u2", "ch", "g1"),
            has_content: true,
            kind_label: "",
            author_key: "u2",
        },
    ];
    let mut runs = Vec::new();
    let mut reads = Vec::new();
    accept_inbound::<()>(
        &items,
        "owner",
        &["a".into()],
        &lookups,
        None,
        |_| (),
        |_, _| {},
        |i, _, read| {
            runs.push(i);
            reads.push(read.to_vec());
        },
    )
    .unwrap();
    assert_eq!(runs, vec![1], "内容あり最後だけがトリガー");
    assert_eq!(
        reads,
        vec![vec![0, 1]],
        "record-only の 0 もこのターンで読む"
    );
}

/// row 116-117: チャンネル whitelist 落ちは agent_drop。メッセージは通るので
/// `on_run` の read には入る。👀 を付けないのはゲートが `agent_drop` を見たとき。
#[test]
fn accept_inbound_whitelist_drop_is_agent_drop_not_message_drop() {
    let caller = CallerIdentity::Agent;
    let resolve = |_: &str, _: &[String], _: &str| caller.clone();
    let lookups = InboundLookups {
        resolve_caller: &resolve,
        dm_allowed_any: &|_, _, _| true,
        dm_allowed: &|_, _, _| true,
        channel_whitelisted: &|_, _| false,
    };
    let items = [InboundWork {
        event: event("u1", "ch", "g1"),
        has_content: true,
        kind_label: "",
        author_key: "u1",
    }];
    let mut plan = None;
    accept_inbound::<()>(
        &items,
        "owner",
        &["a".into()],
        &lookups,
        None,
        |_| (),
        |_, adm| plan = Some(adm.clone()),
        |_, _, _| {},
    )
    .unwrap();
    let plan = plan.expect("メッセージ自体は通る");
    assert_eq!(
        plan.agent_drop("a"),
        Some(InboundAgentDrop::ChannelNotWhitelisted)
    );
    assert!(plan.admitted_agent_ids.is_empty());
}
