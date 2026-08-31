//! operator と gateway が共有する配置。HTTP bind は無い。**秘密（bot token）は載せない**
//! （設計 §1.3・§3.1: canonical config は bot の nonsecret identity / intents / transport 設定だけ）。

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Placement {
    pub core_socket: String,
    pub instances: Vec<InstancePlacement>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstancePlacement {
    pub instance_id: String,
    pub revision: u64,
    /// この agent の whitelisted channel session（`discord-{agent}-{guild}-{channel}`）の集合。
    /// gateway はこれらの say を drain し bind を待つ。実受理は core の ack が正（binding_for_address）。
    pub addresses: Vec<String>,
    pub config_b64: String,
}

/// instance の非秘密 config。bot token は含めない（env 注入のみ）。
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceConfig {
    /// core `agents.id`。受信 Discord message の (guild,channel) から binding address を組む。
    pub agent_id: String,
    /// bot 自身の Discord user id（10進 snowflake）。自分の投稿除外と hello の author identity。
    pub self_bot_id: String,
    /// bot の表示名（非秘密・任意）。
    #[serde(default)]
    pub name: Option<String>,
    /// application id（非秘密・任意）。
    #[serde(default)]
    pub application_id: Option<String>,
    /// 配送モード（say | tool_driven）。
    #[serde(default)]
    pub delivery_mode: Option<String>,
    /// system reaction（受理／完了／失敗）の絵文字。profile data（D17-12・instance config_b64）で
    /// 上書きし、無ければ現行と同じ既定値（👀🏁❌）。core に Discord 語彙は足さない。
    #[serde(default)]
    pub system_reactions: SystemReactions,
}

/// gateway が自動付与する system reaction の絵文字集合（設計 v17 §6.3・D17-12）。
///
/// - `accepted`（👀）: said を core が受理した時点で発端メッセージへ付ける（受理サイン＝gateway の責務）。
/// - `completed`（🏁）: 発端メッセージへの返信（say）を配送し終えた時点で付ける。
/// - `failed`（❌）: 発端メッセージへの返信配送が失敗した時点で付ける。
/// - `no_reply`（🤐）: ターンが沈黙（say 無し）で終えた時点で発端メッセージへ付ける
///   （`CompletedNoReply` の reply_origin が Single のときだけ・裁定A で真の沈黙だけに立つ）。
#[derive(Debug, Clone, Deserialize)]
pub struct SystemReactions {
    #[serde(default = "default_accepted")]
    pub accepted: String,
    #[serde(default = "default_completed")]
    pub completed: String,
    #[serde(default = "default_failed")]
    pub failed: String,
    #[serde(default = "default_no_reply")]
    pub no_reply: String,
}

fn default_accepted() -> String {
    "👀".to_string()
}
fn default_completed() -> String {
    "🏁".to_string()
}
fn default_failed() -> String {
    "❌".to_string()
}
fn default_no_reply() -> String {
    "🤐".to_string()
}

impl Default for SystemReactions {
    fn default() -> Self {
        Self {
            accepted: default_accepted(),
            completed: default_completed(),
            failed: default_failed(),
            no_reply: default_no_reply(),
        }
    }
}

impl Placement {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let place: Placement = serde_json::from_str(&text)?;
        place.validate()?;
        Ok(place)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.core_socket.is_empty() || !self.core_socket.starts_with('/') {
            anyhow::bail!("core_socket must be an absolute path");
        }
        if self.instances.is_empty() {
            anyhow::bail!("instances must be nonempty");
        }
        let mut seen = std::collections::BTreeSet::new();
        for inst in &self.instances {
            if inst.revision == 0 {
                anyhow::bail!("revision must be positive");
            }
            if inst.addresses.is_empty() {
                anyhow::bail!("addresses must be nonempty");
            }
            if inst.addresses.iter().any(|a| a.is_empty()) {
                anyhow::bail!("address must be nonempty");
            }
            parse_uuid(&inst.instance_id)?;
            if !seen.insert(inst.instance_id.clone()) {
                anyhow::bail!("duplicate instance_id is a double live; refuse startup");
            }
            let bytes = decode_config_b64(&inst.config_b64)?;
            let cfg = parse_instance_config(&bytes)?;
            validate_instance_config(&cfg)?;
        }
        Ok(())
    }
}

pub fn decode_config_b64(config_b64: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(config_b64)
        .map_err(|_| anyhow::anyhow!("config_b64 must be standard padded base64"))
}

pub fn config_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_lower(&Sha256::digest(bytes))
}

pub fn parse_instance_config(bytes: &[u8]) -> anyhow::Result<InstanceConfig> {
    let cfg: InstanceConfig = serde_json::from_slice(bytes)
        .map_err(|e| anyhow::anyhow!("instance config is not valid JSON object: {e}"))?;
    validate_instance_config(&cfg)?;
    Ok(cfg)
}

fn validate_instance_config(cfg: &InstanceConfig) -> anyhow::Result<()> {
    if cfg.agent_id.trim().is_empty() {
        anyhow::bail!("agent_id must be nonempty");
    }
    if !is_snowflake(&cfg.self_bot_id) {
        anyhow::bail!("self_bot_id must be a nonempty decimal snowflake");
    }
    if cfg
        .name
        .as_deref()
        .map(str::trim)
        .is_some_and(|n| n.is_empty())
    {
        anyhow::bail!("name, if present, must be nonempty");
    }
    match cfg.delivery_mode.as_deref() {
        None | Some("say") | Some("tool_driven") => {}
        Some(_) => anyhow::bail!("delivery_mode must be say or tool_driven"),
    }
    let sr = &cfg.system_reactions;
    for (label, emoji) in [
        ("accepted", &sr.accepted),
        ("completed", &sr.completed),
        ("failed", &sr.failed),
        ("no_reply", &sr.no_reply),
    ] {
        if emoji.trim().is_empty() {
            anyhow::bail!("system_reactions.{label} must be a nonempty emoji");
        }
    }
    Ok(())
}

/// 10進 snowflake（u64 に収まる非空数字列）。生 ID は会話へ出さないが config の妥当性検査には使う。
pub fn is_snowflake(raw: &str) -> bool {
    !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) && raw.parse::<u64>().is_ok()
}

pub fn parse_uuid(raw: &str) -> anyhow::Result<String> {
    let parsed = uuid::Uuid::parse_str(raw)?;
    let canonical = parsed.to_string();
    if canonical != raw {
        anyhow::bail!("instance_id must be canonical lowercase UUID");
    }
    Ok(canonical)
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> serde_json::Value {
        serde_json::json!({
            "agent_id": "agent-x",
            "self_bot_id": "111111111111111111",
            "name": "crab",
        })
    }

    fn encode(v: &serde_json::Value) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(v).unwrap())
    }

    #[test]
    fn valid_placement_passes_and_has_no_token_field() {
        let p = Placement {
            core_socket: "/tmp/g.sock".into(),
            instances: vec![InstancePlacement {
                instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
                revision: 1,
                addresses: vec!["discord-agent-x-100-200".into()],
                config_b64: encode(&sample_config()),
            }],
        };
        p.validate().unwrap();
        // config には token フィールドが無い（あっても serde が無視する＝InstanceConfig に無い）。
        let cfg =
            parse_instance_config(&decode_config_b64(&encode(&sample_config())).unwrap()).unwrap();
        assert_eq!(cfg.agent_id, "agent-x");
        assert_eq!(cfg.self_bot_id, "111111111111111111");
    }

    #[test]
    fn empty_addresses_fail_loud() {
        let p = Placement {
            core_socket: "/tmp/g.sock".into(),
            instances: vec![InstancePlacement {
                instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
                revision: 1,
                addresses: vec![],
                config_b64: encode(&sample_config()),
            }],
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn non_snowflake_self_bot_id_fails() {
        let mut v = sample_config();
        v["self_bot_id"] = serde_json::json!("not-a-number");
        assert!(parse_instance_config(&serde_json::to_vec(&v).unwrap()).is_err());
    }

    #[test]
    fn dm_address_with_empty_guild_component_is_accepted() {
        // DM は guild 成分が空（discord-{agent}--{channel}）。address 非空なら通す（設計 §3.2/D17-03）。
        let p = Placement {
            core_socket: "/tmp/g.sock".into(),
            instances: vec![InstancePlacement {
                instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
                revision: 1,
                addresses: vec!["discord-agent-x--200".into()],
                config_b64: encode(&sample_config()),
            }],
        };
        p.validate().unwrap();
    }

    #[test]
    fn system_reactions_default_to_legacy_emojis() {
        // config に system_reactions が無ければ現行既定（👀🏁❌）。
        let cfg = parse_instance_config(&serde_json::to_vec(&sample_config()).unwrap()).unwrap();
        assert_eq!(cfg.system_reactions.accepted, "👀");
        assert_eq!(cfg.system_reactions.completed, "🏁");
        assert_eq!(cfg.system_reactions.failed, "❌");
        assert_eq!(cfg.system_reactions.no_reply, "🤐");
    }

    #[test]
    fn system_reactions_can_be_overridden_via_profile_data() {
        let mut v = sample_config();
        v["system_reactions"] = serde_json::json!({
            "accepted": "🫡", "completed": "✅", "failed": "💥", "no_reply": "🙊",
        });
        let cfg = parse_instance_config(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(cfg.system_reactions.accepted, "🫡");
        assert_eq!(cfg.system_reactions.completed, "✅");
        assert_eq!(cfg.system_reactions.failed, "💥");
        assert_eq!(cfg.system_reactions.no_reply, "🙊");
    }

    #[test]
    fn partial_system_reactions_fall_back_per_field() {
        // 一部だけ指定 → 残りは既定。
        let mut v = sample_config();
        v["system_reactions"] = serde_json::json!({ "completed": "✅" });
        let cfg = parse_instance_config(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(cfg.system_reactions.accepted, "👀");
        assert_eq!(cfg.system_reactions.completed, "✅");
        assert_eq!(cfg.system_reactions.failed, "❌");
        assert_eq!(cfg.system_reactions.no_reply, "🤐");
    }

    #[test]
    fn empty_system_reaction_emoji_fails() {
        let mut v = sample_config();
        v["system_reactions"] = serde_json::json!({ "accepted": "" });
        assert!(parse_instance_config(&serde_json::to_vec(&v).unwrap()).is_err());
    }

    #[test]
    fn bad_delivery_mode_fails() {
        let mut v = sample_config();
        v["delivery_mode"] = serde_json::json!("weird");
        assert!(parse_instance_config(&serde_json::to_vec(&v).unwrap()).is_err());
    }

    #[test]
    fn duplicate_instance_id_is_double_live() {
        let inst = InstancePlacement {
            instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
            revision: 1,
            addresses: vec!["discord-agent-x-100-200".into()],
            config_b64: encode(&sample_config()),
        };
        let p = Placement {
            core_socket: "/tmp/g.sock".into(),
            instances: vec![inst.clone(), inst],
        };
        assert!(p.validate().is_err());
    }
}
