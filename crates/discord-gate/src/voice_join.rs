//! join/leave の権限と guild fail-closed。
//!
//! 本体 `voice_actions.rs` と同じ順序: 権限は voice 有無より**手前**。
//! guild は Discord セッションから復元し、欠けたら fail-closed。
//! DM / 非 Discord は拒否。shared 起動拒否は launch 側の既存ガードのまま。

/// 本体 `GatewayCaller` の voice 許可集合に対応する。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoiceCaller {
    Owner,
    OwnerEquivalent,
    Trusted,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinVoicePlan {
    pub guild_id: u64,
    pub vc_channel_id: u64,
    pub text_channel_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoiceJoinDeny {
    Permission(&'static str),
    VoiceDisabled(&'static str),
    NotDiscordSession(&'static str),
    Dm(&'static str),
    MissingChannelId(&'static str),
}

pub fn voice_caller_allowed(caller: &VoiceCaller) -> bool {
    matches!(
        caller,
        VoiceCaller::Owner | VoiceCaller::OwnerEquivalent | VoiceCaller::Trusted
    )
}

fn permission_message(action: &str) -> &'static str {
    match action {
        "leave" => "leave_voice_channel requires owner, co_agent, or trusted_user",
        _ => "join_voice_channel requires owner, co_agent, or trusted_user",
    }
}

/// `channel_id` は文字列または数値。本体と同じ。
pub fn parse_vc_channel_id(channel_id: Option<&str>, channel_id_u64: Option<u64>) -> Option<u64> {
    channel_id
        .and_then(|s| s.parse::<u64>().ok())
        .or(channel_id_u64)
}

/// join: 権限 → voice 有無 → Discord セッション / guild fail-closed → channel_id。
pub fn evaluate_join_voice(
    caller: &VoiceCaller,
    voice_available: bool,
    session_guild: Option<&str>,
    session_text_channel: Option<&str>,
    vc_channel_id: Option<u64>,
    text_channel_override: Option<&str>,
) -> Result<JoinVoicePlan, VoiceJoinDeny> {
    if !voice_caller_allowed(caller) {
        return Err(VoiceJoinDeny::Permission(permission_message("join")));
    }
    if !voice_available {
        return Err(VoiceJoinDeny::VoiceDisabled(
            "voice 機能が無効です（config.toml の [voice] enabled = true が必要）",
        ));
    }
    let Some(guild_str) = session_guild.filter(|value| !value.is_empty()) else {
        return Err(VoiceJoinDeny::NotDiscordSession(
            "join_voice_channel は Discord セッション文脈でのみ実行できます",
        ));
    };
    let Ok(guild_id) = guild_str.parse::<u64>() else {
        return Err(VoiceJoinDeny::Dm("DM からは VC に参加できません"));
    };
    let Some(vc_channel_id) = vc_channel_id else {
        return Err(VoiceJoinDeny::MissingChannelId(
            "join_voice_channel: 'channel_id'（VCのID）が必要です",
        ));
    };
    let text_channel_id = text_channel_override
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| session_text_channel.map(str::to_string))
        .ok_or(VoiceJoinDeny::NotDiscordSession(
            "join_voice_channel は Discord セッション文脈でのみ実行できます",
        ))?;
    Ok(JoinVoicePlan {
        guild_id,
        vc_channel_id,
        text_channel_id,
    })
}

/// leave: 権限 → voice 有無 → Discord セッション / guild fail-closed。
pub fn evaluate_leave_voice(
    caller: &VoiceCaller,
    voice_available: bool,
    session_guild: Option<&str>,
) -> Result<u64, VoiceJoinDeny> {
    if !voice_caller_allowed(caller) {
        return Err(VoiceJoinDeny::Permission(permission_message("leave")));
    }
    if !voice_available {
        return Err(VoiceJoinDeny::VoiceDisabled("voice 機能が無効です"));
    }
    let Some(guild_id) = session_guild.and_then(|g| g.parse::<u64>().ok()) else {
        return Err(VoiceJoinDeny::NotDiscordSession(
            "leave_voice_channel は Discord セッション文脈でのみ実行できます",
        ));
    };
    Ok(guild_id)
}

impl VoiceJoinDeny {
    pub fn is_permission(&self) -> bool {
        matches!(self, Self::Permission(_))
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Permission(m)
            | Self::VoiceDisabled(m)
            | Self::NotDiscordSession(m)
            | Self::Dm(m)
            | Self::MissingChannelId(m) => m,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(
        caller: VoiceCaller,
        voice: bool,
        guild: Option<&str>,
    ) -> Result<JoinVoicePlan, VoiceJoinDeny> {
        evaluate_join_voice(&caller, voice, guild, Some("222"), Some(333), None)
    }

    #[test]
    fn voice_actions_reject_non_trusted_callers_before_voice_availability() {
        let caller = VoiceCaller::Other;
        let joined = join(caller.clone(), false, Some("111"));
        assert!(
            joined
                .as_ref()
                .err()
                .is_some_and(VoiceJoinDeny::is_permission),
            "join が権限より先に voice 無効で止まっている: {joined:?}"
        );
        let left = evaluate_leave_voice(&caller, false, Some("111"));
        assert!(
            left.as_ref()
                .err()
                .is_some_and(VoiceJoinDeny::is_permission),
            "leave が権限より先に voice 無効で止まっている: {left:?}"
        );
    }

    #[test]
    fn voice_actions_let_owner_equivalent_and_trusted_user_past_the_gate() {
        for caller in [
            VoiceCaller::Owner,
            VoiceCaller::OwnerEquivalent,
            VoiceCaller::Trusted,
        ] {
            let joined = join(caller.clone(), false, Some("111"));
            assert!(
                !joined
                    .as_ref()
                    .err()
                    .is_some_and(VoiceJoinDeny::is_permission),
                "join が {caller:?} を権限で弾いている: {joined:?}"
            );
            assert!(
                matches!(joined, Err(VoiceJoinDeny::VoiceDisabled(m)) if m.contains("voice")),
                "{caller:?}: ゲートの先（voice 無効）まで届いていない: {joined:?}"
            );
            let left = evaluate_leave_voice(&caller, false, Some("111"));
            assert!(
                !left
                    .as_ref()
                    .err()
                    .is_some_and(VoiceJoinDeny::is_permission),
                "leave が {caller:?} を権限で弾いている: {left:?}"
            );
        }
    }

    #[test]
    fn join_guild_is_fail_closed_and_dm_is_rejected() {
        let allowed = VoiceCaller::Owner;
        assert!(matches!(
            evaluate_join_voice(&allowed, true, None, Some("222"), Some(333), None),
            Err(VoiceJoinDeny::NotDiscordSession(_))
        ));
        assert!(matches!(
            evaluate_join_voice(&allowed, true, Some("dm"), Some("222"), Some(333), None),
            Err(VoiceJoinDeny::Dm(_))
        ));
        let plan = evaluate_join_voice(
            &allowed,
            true,
            Some("111"),
            Some("222"),
            Some(333),
            Some("444"),
        )
        .unwrap();
        assert_eq!(plan.guild_id, 111);
        assert_eq!(plan.vc_channel_id, 333);
        assert_eq!(plan.text_channel_id, "444");
    }
}
