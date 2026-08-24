//! 宣言済み 8 operation の計画。songbird / Discord HTTP は main が実行する。
//!
//! join/leave は本体 `voice_actions.rs` と同じ権限順（権限 → voice 有無 → session）。
//! auth の入力は grant / standing を `VoiceCaller` へ写したもの。

use serde_json::{json, Value};

use crate::declared_discord_operations;
use crate::voice_join::{
    evaluate_join_voice, evaluate_leave_voice, parse_vc_channel_id, JoinVoicePlan, VoiceCaller,
    VoiceJoinDeny,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscordOpPlan {
    ListGuilds,
    ListChannels {
        guild_id: u64,
    },
    CreateChannel {
        guild_id: u64,
        name: String,
        parent_id: Option<u64>,
        topic: Option<String>,
        reason: Option<String>,
    },
    CreateWebhook {
        channel_id: u64,
        name: String,
    },
    AddReaction {
        channel_id: u64,
        message_id: u64,
        emoji: String,
    },
    SendFile {
        channel_id: u64,
        file_path: String,
        caption: String,
        filename: Option<String>,
    },
    JoinVoice(JoinVoicePlan),
    LeaveVoice {
        guild_id: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscordOpDeny {
    UnknownName(String),
    Voice(VoiceJoinDeny),
    BadArgs(&'static str),
    /// §8.1: channel→place が複数。policy は推測しない。
    PolicyAmbiguous,
}

impl DiscordOpDeny {
    pub fn message(&self) -> String {
        match self {
            Self::UnknownName(name) => format!("undeclared gate operation: {name}"),
            Self::Voice(deny) => deny.message().to_string(),
            Self::BadArgs(message) => (*message).to_string(),
            Self::PolicyAmbiguous => "policy_ambiguous".to_string(),
        }
    }
}

/// 本体 `discord_ops` list_guilds 行。`member_count` は常に null。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedGuild {
    pub id: String,
    pub name: String,
}

/// 本体 list_channels 行 + §8.1 policy join 入力。text 以外は envelope が落とす。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedChannel {
    pub id: String,
    pub name: String,
    pub is_text: bool,
    pub policy: ChannelPolicy,
}

/// §1.3 / §8.1: 0件=hard default、1件=本体3値、複数=`policy_ambiguous`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelPolicy {
    HardDefault,
    Resolved {
        readable: bool,
        writable: bool,
        whitelisted: bool,
    },
    Ambiguous,
}

/// standing / grant role の wire → 本体 GatewayCaller 相当。
pub fn voice_caller_from_role(role: Option<&str>) -> VoiceCaller {
    match role {
        Some("owner") => VoiceCaller::Owner,
        Some("owner_equivalent") | Some("co_agent") => VoiceCaller::OwnerEquivalent,
        Some("trusted") | Some("trusted_user") => VoiceCaller::Trusted,
        _ => VoiceCaller::Other,
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn arg_id(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
    })
}

/// `name` は宣言 8 件だけ。それ以外は fail loud。
pub fn plan_discord_operation(
    name: &str,
    args: &Value,
    caller: &VoiceCaller,
    voice_available: bool,
    session_guild: Option<&str>,
    session_text: Option<&str>,
) -> Result<DiscordOpPlan, DiscordOpDeny> {
    if !declared_discord_operations().contains(&name) {
        return Err(DiscordOpDeny::UnknownName(name.to_string()));
    }
    match name {
        "discord_list_guilds" => Ok(DiscordOpPlan::ListGuilds),
        "discord_list_channels" => {
            let guild_id = arg_id(args, "guild_id")
                .ok_or(DiscordOpDeny::BadArgs("guild_idパラメータが必要です"))?;
            Ok(DiscordOpPlan::ListChannels { guild_id })
        }
        "discord_create_channel" => {
            let guild_id = arg_id(args, "guild_id")
                .ok_or(DiscordOpDeny::BadArgs("guild_idパラメータが必要です"))?;
            let name = arg_str(args, "name")
                .ok_or(DiscordOpDeny::BadArgs("nameパラメータが必要です"))?
                .to_string();
            Ok(DiscordOpPlan::CreateChannel {
                guild_id,
                name,
                parent_id: arg_id(args, "parent_id"),
                topic: arg_str(args, "topic").map(str::to_string),
                reason: arg_str(args, "reason").map(str::to_string),
            })
        }
        "discord_create_webhook" => {
            let channel_id = arg_id(args, "channel_id")
                .ok_or(DiscordOpDeny::BadArgs("channel_idパラメータが必要です"))?;
            Ok(DiscordOpPlan::CreateWebhook {
                channel_id,
                name: arg_str(args, "name")
                    .unwrap_or("opencrab-subtask")
                    .to_string(),
            })
        }
        "discord_add_reaction" => {
            let channel_id = arg_id(args, "channel_id")
                .ok_or(DiscordOpDeny::BadArgs("channel_idパラメータが必要です"))?;
            let message_id = arg_id(args, "message_id")
                .ok_or(DiscordOpDeny::BadArgs("message_idパラメータが必要です"))?;
            let emoji = arg_str(args, "emoji")
                .ok_or(DiscordOpDeny::BadArgs("emojiパラメータが必要です"))?
                .to_string();
            Ok(DiscordOpPlan::AddReaction {
                channel_id,
                message_id,
                emoji,
            })
        }
        "discord_send_file" => {
            let channel_id = arg_id(args, "channel_id")
                .ok_or(DiscordOpDeny::BadArgs("channel_idパラメータが必要です"))?;
            let file_path = arg_str(args, "file_path")
                .ok_or(DiscordOpDeny::BadArgs("file_pathパラメータが必要です"))?
                .to_string();
            Ok(DiscordOpPlan::SendFile {
                channel_id,
                file_path,
                caption: arg_str(args, "caption").unwrap_or("").to_string(),
                filename: arg_str(args, "filename").map(str::to_string),
            })
        }
        "join_voice_channel" => {
            let vc = parse_vc_channel_id(arg_str(args, "channel_id"), arg_id(args, "channel_id"));
            evaluate_join_voice(
                caller,
                voice_available,
                session_guild,
                session_text,
                vc,
                arg_str(args, "text_channel_id"),
            )
            .map(DiscordOpPlan::JoinVoice)
            .map_err(DiscordOpDeny::Voice)
        }
        "leave_voice_channel" => evaluate_leave_voice(caller, voice_available, session_guild)
            .map(|guild_id| DiscordOpPlan::LeaveVoice { guild_id })
            .map_err(DiscordOpDeny::Voice),
        other => Err(DiscordOpDeny::UnknownName(other.to_string())),
    }
}

pub fn join_success_json(plan: &JoinVoicePlan) -> Value {
    json!({
        "status": "joined",
        "guild_id": plan.guild_id.to_string(),
        "vc_channel_id": plan.vc_channel_id.to_string(),
        "text_channel_id": plan.text_channel_id,
        "note": "音声はユーザーごとに文字起こしされ、このチャンネルの会話として届きます。返信は自動で読み上げられます。",
    })
}

pub fn leave_success_json() -> Value {
    json!({"status": "left"})
}

/// 本体 `execute_list_guilds` data。
pub fn list_guilds_success_json(guilds: &[ListedGuild]) -> Value {
    let list: Vec<Value> = guilds
        .iter()
        .map(|guild| {
            json!({
                "id": guild.id,
                "name": guild.name,
                "member_count": Value::Null,
            })
        })
        .collect();
    let count = list.len();
    json!({"guilds": list, "count": count})
}

/// 本体 `execute_list_channels` data + §8.1 `policy_ambiguous`。
pub fn list_channels_success_json(
    guild_id: &str,
    channels: &[ListedChannel],
) -> Result<Value, DiscordOpDeny> {
    let mut list = Vec::new();
    for channel in channels {
        if !channel.is_text {
            continue;
        }
        let (readable, writable, whitelisted) = match channel.policy {
            ChannelPolicy::Ambiguous => return Err(DiscordOpDeny::PolicyAmbiguous),
            ChannelPolicy::HardDefault => (true, true, false),
            ChannelPolicy::Resolved {
                readable,
                writable,
                whitelisted,
            } => (readable, writable, whitelisted),
        };
        list.push(json!({
            "id": channel.id,
            "name": channel.name,
            "kind": "text",
            "readable": readable,
            "writable": writable,
            "whitelisted": whitelisted,
        }));
    }
    let count = list.len();
    Ok(json!({
        "guild_id": guild_id,
        "channels": list,
        "count": count,
    }))
}

/// 本体 `execute_discord_create_channel` data。
pub fn create_channel_success_json(
    id: &str,
    name: &str,
    guild_id: &str,
    parent_id: Option<&str>,
) -> Value {
    json!({
        "id": id,
        "name": name,
        "guild_id": guild_id,
        "parent_id": parent_id,
        "url": format!("https://discord.com/channels/{guild_id}/{id}"),
        "message": format!("チャンネル {name} を作成しました"),
    })
}

/// 本体 `execute_discord_create_webhook` data。
pub fn create_webhook_success_json(
    channel_id: &str,
    webhook_id: &str,
    name: &str,
    url: &str,
) -> Value {
    json!({
        "channel_id": channel_id,
        "webhook_id": webhook_id,
        "name": name,
        "url": url,
        "message": "webhookを作成しました。このurlをspawn_subtask.webhook.urlに渡せます。",
    })
}

/// 本体 `execute_discord_add_reaction` data。
pub fn add_reaction_success_json(channel_id: &str, message_id: &str, emoji: &str) -> Value {
    json!({
        "channel_id": channel_id,
        "message_id": message_id,
        "emoji": emoji,
        "message": format!("リアクション {emoji} を追加しました"),
    })
}

/// 本体 `execute_send_file` data。
pub fn send_file_success_json(channel_id: &str, file: &str) -> Value {
    json!({
        "channel_id": channel_id,
        "file": file,
        "message": format!("ファイル {file} を送信しました"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_declared_names_are_accepted_and_unknown_is_loud() {
        for name in declared_discord_operations() {
            let args = match *name {
                "discord_list_channels" | "discord_create_channel" => {
                    json!({"guild_id": "1", "name": "x"})
                }
                "discord_create_webhook" | "discord_add_reaction" | "discord_send_file" => {
                    json!({
                        "channel_id": "2",
                        "message_id": "3",
                        "emoji": "👍",
                        "file_path": "a.png"
                    })
                }
                "join_voice_channel" => json!({"channel_id": "333"}),
                _ => json!({}),
            };
            let planned = plan_discord_operation(
                name,
                &args,
                &VoiceCaller::Owner,
                true,
                Some("111"),
                Some("222"),
            );
            assert!(planned.is_ok(), "{name}: {planned:?}");
        }
        let err = plan_discord_operation(
            "not_a_tool",
            &json!({}),
            &VoiceCaller::Owner,
            true,
            Some("111"),
            Some("222"),
        )
        .unwrap_err();
        assert!(matches!(err, DiscordOpDeny::UnknownName(_)));
    }

    #[test]
    fn join_leave_keep_permission_before_voice() {
        let denied = plan_discord_operation(
            "join_voice_channel",
            &json!({"channel_id": "333"}),
            &VoiceCaller::Other,
            false,
            Some("111"),
            Some("222"),
        )
        .unwrap_err();
        assert!(
            matches!(denied, DiscordOpDeny::Voice(ref v) if v.is_permission()),
            "{denied:?}"
        );
        let voice_off = plan_discord_operation(
            "leave_voice_channel",
            &json!({}),
            &VoiceCaller::Trusted,
            false,
            Some("111"),
            Some("222"),
        )
        .unwrap_err();
        assert!(
            matches!(voice_off, DiscordOpDeny::Voice(ref v) if !v.is_permission()),
            "{voice_off:?}"
        );
    }

    #[test]
    fn grant_roles_map_to_body_callers() {
        assert_eq!(voice_caller_from_role(Some("owner")), VoiceCaller::Owner);
        assert_eq!(
            voice_caller_from_role(Some("owner_equivalent")),
            VoiceCaller::OwnerEquivalent
        );
        assert_eq!(
            voice_caller_from_role(Some("trusted")),
            VoiceCaller::Trusted
        );
        assert_eq!(voice_caller_from_role(Some("agent")), VoiceCaller::Other);
        assert_eq!(voice_caller_from_role(None), VoiceCaller::Other);
    }

    fn object_keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn non_join_envelopes_use_body_keys() {
        let guilds = list_guilds_success_json(&[ListedGuild {
            id: "1".into(),
            name: "g".into(),
        }]);
        assert_eq!(object_keys(&guilds), ["count", "guilds"]);
        assert_eq!(
            object_keys(&guilds["guilds"][0]),
            ["id", "member_count", "name"]
        );

        let hard = list_channels_success_json(
            "11",
            &[
                ListedChannel {
                    id: "21".into(),
                    name: "general".into(),
                    is_text: true,
                    policy: ChannelPolicy::HardDefault,
                },
                ListedChannel {
                    id: "22".into(),
                    name: "voice".into(),
                    is_text: false,
                    policy: ChannelPolicy::HardDefault,
                },
            ],
        )
        .unwrap();
        assert_eq!(object_keys(&hard), ["channels", "count", "guild_id"]);
        assert_eq!(
            object_keys(&hard["channels"][0]),
            ["id", "kind", "name", "readable", "whitelisted", "writable"]
        );
        assert_eq!(hard["channels"][0]["kind"], "text");
        assert_eq!(hard["count"], 1);

        let resolved = list_channels_success_json(
            "11",
            &[ListedChannel {
                id: "21".into(),
                name: "general".into(),
                is_text: true,
                policy: ChannelPolicy::Resolved {
                    readable: false,
                    writable: true,
                    whitelisted: true,
                },
            }],
        )
        .unwrap();
        assert_eq!(
            object_keys(&resolved["channels"][0]),
            ["id", "kind", "name", "readable", "whitelisted", "writable"]
        );

        let ambiguous = list_channels_success_json(
            "11",
            &[ListedChannel {
                id: "21".into(),
                name: "general".into(),
                is_text: true,
                policy: ChannelPolicy::Ambiguous,
            }],
        )
        .unwrap_err();
        assert_eq!(ambiguous, DiscordOpDeny::PolicyAmbiguous);
        assert_eq!(ambiguous.message(), "policy_ambiguous");

        let created = create_channel_success_json("31", "room", "11", None);
        assert_eq!(
            object_keys(&created),
            ["guild_id", "id", "message", "name", "parent_id", "url"]
        );
        assert_eq!(created["url"], "https://discord.com/channels/11/31");

        let webhook = create_webhook_success_json("21", "41", "hook", "https://example.invalid/w");
        assert_eq!(
            object_keys(&webhook),
            ["channel_id", "message", "name", "url", "webhook_id"]
        );

        let reaction = add_reaction_success_json("21", "51", "👍");
        assert_eq!(
            object_keys(&reaction),
            ["channel_id", "emoji", "message", "message_id"]
        );

        let file = send_file_success_json("21", "a.png");
        assert_eq!(object_keys(&file), ["channel_id", "file", "message"]);
    }
}
