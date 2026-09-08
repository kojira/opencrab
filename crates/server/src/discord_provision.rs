//! Discord V3 gate instance / binding の core 側敷設と点火（ignition）。
//!
//! `crates/server/src/nostr_provision.rs` と `crates/nostr/src/ingress.rs` の**最小ミラー**。
//! ingress は V3-only。既存の generic `gate_instances` / `gate_bindings` だけで表現し、
//! core に Discord 固有の enum / DB 列 / 語彙は足さない。
//!
//! **秘密（bot token）はこのモジュールに一切載せない**。token は `agent_discord_config.bot_token` に
//! あり、点火オーケストレータ（[`ignite_discord_instances`]）が spawn クロージャへ**別引数**で渡す。
//! spawn 実装（main.rs）は token を子プロセスの env（`DISCORD_BOT_TOKEN`）へ注入する（argv 禁止）。
//! `self_bot_id` は `agent_discord_config.bot_user_id`（各 bot 自身の認証済み接続＝`get_current_user`
//! だけが書く）。未接続で空なら V3 config を組めないので当該 agent は fail-loud で skip する。

use anyhow::{bail, Context, Result};
use opencrab_db::queries::{create_gate_binding_in_tx, get_session, CreateGateBindingError};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscordIngress;

impl DiscordIngress {
    pub const V3: Self = Self;

    /// Only an explicit `v3` value is accepted. Missing and retired modes fail closed.
    pub fn parse(raw: &str) -> Option<Self> {
        (raw.trim() == "v3").then_some(Self)
    }

    pub fn as_str(self) -> &'static str {
        "v3"
    }

    pub fn provisions_instance(self) -> bool {
        true
    }

    pub fn provisions_binding(self) -> bool {
        true
    }
}

const DNS_NS: uuid::Uuid = uuid::Uuid::NAMESPACE_DNS;

/// agent ごと決定的 instance_id（Nostr の `nostr_instance_id` と同型）。
pub fn discord_instance_id(agent_id: &str) -> String {
    uuid::Uuid::new_v5(
        &DNS_NS,
        format!("opencrab:discord:instance:{agent_id}").as_bytes(),
    )
    .to_string()
}

/// binding_id（address ごと決定的）。
pub fn discord_binding_id(agent_id: &str, address: &str) -> String {
    uuid::Uuid::new_v5(
        &DNS_NS,
        format!("opencrab:discord:binding:{agent_id}:{address}").as_bytes(),
    )
    .to_string()
}

/// 会話 session アドレス。既存の `discord-{agent}-{guild}-{channel}` session を再利用する
/// （新規 session は作らない・§9.1「既存 session 再利用」）。
pub fn discord_address(agent_id: &str, guild_id: &str, channel_id: &str) -> String {
    format!("discord-{agent_id}-{guild_id}-{channel_id}")
}

/// instance の非秘密 config（`crates/discord-gateway/src/config.rs` の `InstanceConfig` 形）。
/// bot token は**絶対に載せない**（env 注入のみ）。
pub fn discord_instance_config_bytes(
    agent_id: &str,
    self_bot_id: &str,
    name: &str,
) -> Result<Vec<u8>> {
    let name = name.trim();
    if name.is_empty() {
        bail!("agents.name が空（Discord instance config を組めない）");
    }
    let self_bot_id = self_bot_id.trim();
    if self_bot_id.is_empty() {
        bail!("agent_discord_config.bot_user_id が空（bot 自身の接続=get_current_user が書くまで V3 点火不可）");
    }
    Ok(serde_json::to_vec(&serde_json::json!({
        "agent_id": agent_id,
        "self_bot_id": self_bot_id,
        // 返信/投稿は say 一本（Discord profile: DESIGN-DISCORD-GATE §10.3）。
        "delivery_mode": "say",
        "name": name,
    }))?)
}

/// [`provision_discord_gate`] の結果。placement 構築に必要な非秘密値だけを持つ。
#[derive(Debug, Clone)]
pub struct DiscordProvisionResult {
    pub instance_id: String,
    pub revision: u64,
    pub config_b64: String,
    pub addresses: Vec<String>,
}

/// instance（常に）+ binding（`provision_bindings=true`＝`v3` のとき）を 1 トランザクションで敷く。
///
/// Nostr の `provision_nostr_gate` / `provision_nostr_instance` を畳んだ最小版。binding は
/// **既存 session を再利用**し、session 不在・membership 不一致は fail-loud（発明しない）。
pub fn provision_discord_gate(
    conn: &mut Connection,
    agent_id: &str,
    self_bot_id: &str,
    name: &str,
    addresses: &[String],
    provision_bindings: bool,
    now: i64,
) -> Result<DiscordProvisionResult> {
    let instance_id = discord_instance_id(agent_id);
    let config_bytes = discord_instance_config_bytes(agent_id, self_bot_id, name)?;
    let config_b64 = opencrab_extgate::encode_config_b64(&config_bytes);
    let digest = opencrab_extgate::config_digest(&config_bytes);

    let subject_id: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = ?1",
            params![agent_id],
            |r| r.get(0),
        )
        .with_context(|| format!("agent {agent_id} の subject_id が無い"))?;

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    // ---- instance 行の upsert ----
    let existing = tx
        .query_row(
            "SELECT kind_id, subject_id, deleted_at, revision FROM gate_instances WHERE instance_id = ?1",
            params![instance_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    let revision = match existing {
        Some((_, _, Some(_), _)) => bail!("discord instance {instance_id} は削除済み"),
        Some((kind, subject, None, _)) if kind != "discord" || subject != subject_id => {
            bail!("discord instance {instance_id} が別 kind/subject で存在する")
        }
        Some((_, _, None, rev)) => {
            tx.execute(
                "UPDATE gate_instances
                 SET config_b64 = ?2, config_digest = ?3, updated_at = ?4
                 WHERE instance_id = ?1",
                params![instance_id, config_b64, digest, now],
            )?;
            u64::try_from(rev).context("revision")?
        }
        None => {
            tx.execute(
                "INSERT INTO gate_instances (
                    instance_id, kind_id, subject_id, revision, enabled,
                    config_b64, config_digest, created_at, updated_at, deleted_at
                 ) VALUES (?1, 'discord', ?2, 1, 1, ?3, ?4, ?5, ?5, NULL)",
                params![instance_id, subject_id, config_b64, digest, now],
            )?;
            1
        }
    };

    // ---- binding（v3 のみ）----
    if provision_bindings {
        for address in addresses {
            if get_session(&tx, address)?.is_none() {
                bail!("session {address} が無い（V3 binding は既存 session を再利用する）");
            }
            let binding_id = discord_binding_id(agent_id, address);
            let already: Option<String> = tx
                .query_row(
                    "SELECT address FROM gate_bindings WHERE binding_id = ?1",
                    params![binding_id],
                    |r| r.get(0),
                )
                .optional()?;
            match already {
                Some(addr) if &addr == address => {}
                Some(addr) => {
                    bail!("binding {binding_id} は別 address {addr} で存在する")
                }
                None => match create_gate_binding_in_tx(
                    &tx,
                    &binding_id,
                    &instance_id,
                    address,
                    address,
                    now,
                ) {
                    Ok(()) => {}
                    Err(CreateGateBindingError::Conflict) => {
                        bail!("binding address {address} の membership / 占有が一致しない")
                    }
                    Err(CreateGateBindingError::Store(e)) => return Err(e),
                },
            }
        }
    }

    tx.commit()?;
    Ok(DiscordProvisionResult {
        instance_id,
        revision,
        config_b64,
        addresses: addresses.to_vec(),
    })
}

fn agent_name(conn: &Connection, agent_id: &str) -> Result<String> {
    conn.query_row(
        "SELECT name FROM agents WHERE agent_id = ?1",
        params![agent_id],
        |r| r.get(0),
    )
    .with_context(|| format!("agent {agent_id} の name が無い"))
}

/// gateway プロセスを起こすための placement（非秘密）。bot token は含めない（別引数で渡す）。
#[derive(Debug, Clone)]
pub struct DiscordPlacementPlan {
    pub agent_id: String,
    pub instance_id: String,
    pub revision: u64,
    pub addresses: Vec<String>,
    pub config_b64: String,
}

/// spawn クロージャ。`(plan, bot_token)`。token は env 注入用で、**ログに出さないこと**。
pub type DiscordSpawnFn<'a> = dyn Fn(&DiscordPlacementPlan, &str) -> Result<()> + Send + Sync + 'a;

/// 点火の集計（起動時ログ / テスト用）。
#[derive(Debug, Default, Clone)]
pub struct DiscordIgniteReport {
    /// provision まで到達した agent_id。
    pub provisioned: Vec<String>,
    /// spawn クロージャが Ok を返した agent_id。
    pub spawned: Vec<String>,
}

/// server 起動時、enabled な各 agent の V3 instance/binding を敷き、
/// `spawn` クロージャで discord-gateway プロセスを起こす。
/// 前提を満たさない agent（bot_user_id / bot_token 未設定・whitelist channel 0・provision 失敗）は
/// **黙って通さず warn して skip** する（1 agent の失敗で全体を止めない）。
pub fn ignite_discord_instances(
    conn: &mut Connection,
    ingress: DiscordIngress,
    now: i64,
    spawn: &DiscordSpawnFn,
) -> Result<DiscordIgniteReport> {
    let mut report = DiscordIgniteReport::default();

    let configs = opencrab_db::queries::list_enabled_agent_discord_configs(conn)?;
    info!(
        ingress = ingress.as_str(),
        candidates = configs.len(),
        "discord V3 点火開始"
    );

    for cfg in configs {
        let agent_id = cfg.agent_id.clone();
        let self_bot_id = opencrab_db::queries::get_agent_discord_bot_user_id(conn, &agent_id)?;
        if self_bot_id.trim().is_empty() {
            bail!("enabled Discord agent {agent_id} has no bot_user_id");
        }
        if cfg.bot_token.trim().is_empty() {
            bail!("enabled Discord agent {agent_id} has no bot_token");
        }
        let name = agent_name(conn, &agent_id)
            .with_context(|| format!("enabled Discord agent {agent_id} has no valid name"))?;
        let channels = opencrab_db::queries::list_channel_configs_by_agent(conn, &agent_id)
            .with_context(|| {
                format!("cannot read channels for enabled Discord agent {agent_id}")
            })?;
        let addresses: Vec<String> = channels
            .iter()
            .filter(|c| c.whitelisted)
            .map(|c| discord_address(&agent_id, &c.guild_id, &c.channel_id))
            .collect();
        if addresses.is_empty() {
            bail!("enabled Discord agent {agent_id} has no whitelisted channel");
        }
        let res =
            provision_discord_gate(conn, &agent_id, &self_bot_id, &name, &addresses, true, now)
                .with_context(|| format!("Discord V3 provision failed for {agent_id}"))?;
        report.provisioned.push(agent_id.clone());
        let plan = DiscordPlacementPlan {
            agent_id: agent_id.clone(),
            instance_id: res.instance_id,
            revision: res.revision,
            addresses: res.addresses,
            config_b64: res.config_b64,
        };
        spawn(&plan, &cfg.bot_token)
            .with_context(|| format!("discord-gateway spawn failed for {agent_id}"))?;
        info!(
            agent_id = %agent_id,
            instance_id = %plan.instance_id,
            revision = plan.revision,
            addresses = plan.addresses.len(),
            "discord-gateway spawn"
        );
        report.spawned.push(agent_id);
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_db::queries::{
        insert_agent_session_in_tx, insert_session_in_tx, upsert_agent, AgentDiscordConfigRow,
        AgentRow, ChannelConfigRow,
    };
    use std::sync::Mutex;

    fn seed_agent(conn: &Connection, agent_id: &str, name: &str) {
        upsert_agent(
            conn,
            &AgentRow {
                agent_id: agent_id.into(),
                name: name.into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "p".into(),
                personality: None,
                instructions: String::new(),
                heartbeat_instructions: String::new(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .unwrap();
    }

    fn seed_discord_config(conn: &Connection, agent_id: &str, token: &str, enabled: bool) {
        opencrab_db::queries::upsert_agent_discord_config(
            conn,
            &AgentDiscordConfigRow {
                agent_id: agent_id.into(),
                bot_token: token.into(),
                owner_discord_id: "222".into(),
                enabled,
            },
        )
        .unwrap();
    }

    fn seed_channel(conn: &Connection, agent_id: &str, guild: &str, channel: &str, wl: bool) {
        opencrab_db::queries::upsert_channel_config(
            conn,
            &ChannelConfigRow {
                channel_id: channel.into(),
                agent_id: agent_id.into(),
                guild_id: guild.into(),
                channel_name: "c".into(),
                readable: true,
                writable: true,
                whitelisted: wl,
                heartbeat_enabled: false,
                heartbeat_interval_secs: None,
                heartbeat_instructions: String::new(),
            },
        )
        .unwrap();
    }

    /// spawn 呼び出しを記録する fake（実プロセス spawn は行わない）。
    #[derive(Default)]
    struct FakeSpawner {
        calls: Mutex<Vec<(String, String)>>, // (agent_id, bot_token)
    }
    impl FakeSpawner {
        fn as_fn(&self) -> impl Fn(&DiscordPlacementPlan, &str) -> Result<()> + '_ {
            move |plan: &DiscordPlacementPlan, token: &str| {
                self.calls
                    .lock()
                    .unwrap()
                    .push((plan.agent_id.clone(), token.to_string()));
                Ok(())
            }
        }
    }

    #[test]
    fn ingress_accepts_only_explicit_v3() {
        assert_eq!(DiscordIngress::parse("v3"), Some(DiscordIngress::V3));
        for retired in ["", "legacy", "v3_shadow", "bogus"] {
            assert_eq!(DiscordIngress::parse(retired), None, "accepted {retired:?}");
        }
        assert!(DiscordIngress::V3.provisions_instance());
        assert!(DiscordIngress::V3.provisions_binding());
    }

    #[test]
    fn config_bytes_has_no_secret_and_is_say() {
        let raw = discord_instance_config_bytes("a1", "111", "くらぶ").unwrap();
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("\"delivery_mode\":\"say\""), "{text}");
        assert!(text.contains("\"self_bot_id\":\"111\""), "{text}");
        assert!(text.contains("くらぶ"));
        assert!(!text.to_lowercase().contains("token"));
        assert!(!text.contains("bot_token"));
    }

    #[test]
    fn empty_bot_user_id_is_fail_loud() {
        let err = discord_instance_config_bytes("a1", "  ", "n").unwrap_err();
        assert!(err.to_string().contains("bot_user_id"), "{err}");
    }

    /// (b') v3: 既存 session があれば instance + binding を敷き spawn が呼ばれる。
    #[test]
    fn v3_provisions_binding_over_existing_session() {
        let mut conn = opencrab_db::init_memory().unwrap();
        seed_agent(&conn, "a1", "crab");
        seed_discord_config(&conn, "a1", "tok", true);
        opencrab_db::queries::set_agent_discord_bot_user_id(&conn, "a1", "111").unwrap();
        seed_channel(&conn, "a1", "500", "600", true);
        // 既存 session を用意（V3 binding は既存 session を再利用する）。
        let addr = discord_address("a1", "500", "600");
        {
            let tx = conn.transaction().unwrap();
            insert_session_in_tx(&tx, &addr, &addr, "2026-01-01T00:00:00Z").unwrap();
            insert_agent_session_in_tx(&tx, "a1", &addr).unwrap();
            tx.commit().unwrap();
        }

        let spawner = FakeSpawner::default();
        let f = spawner.as_fn();
        let report = ignite_discord_instances(&mut conn, DiscordIngress::V3, 9, &f).unwrap();
        assert_eq!(report.spawned, vec!["a1".to_string()]);
        let bindings: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM gate_bindings WHERE address = ?1",
                params![addr],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bindings, 1, "v3 は既存 session に binding を敷く");
    }

    #[test]
    fn missing_bot_user_id_fails_loudly() {
        let mut conn = opencrab_db::init_memory().unwrap();
        seed_agent(&conn, "a1", "crab");
        seed_discord_config(&conn, "a1", "tok", true);
        // bot_user_id は既定 ""（未接続）。
        seed_channel(&conn, "a1", "500", "600", true);

        let spawner = FakeSpawner::default();
        let f = spawner.as_fn();
        let err = ignite_discord_instances(&mut conn, DiscordIngress::V3, 1, &f).unwrap_err();
        assert!(err.to_string().contains("bot_user_id"), "{err:#}");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM gate_instances", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn no_whitelisted_channel_fails_loudly() {
        let mut conn = opencrab_db::init_memory().unwrap();
        seed_agent(&conn, "a1", "crab");
        seed_discord_config(&conn, "a1", "tok", true);
        opencrab_db::queries::set_agent_discord_bot_user_id(&conn, "a1", "111").unwrap();
        seed_channel(&conn, "a1", "500", "600", false); // whitelisted=false

        let spawner = FakeSpawner::default();
        let f = spawner.as_fn();
        let err = ignite_discord_instances(&mut conn, DiscordIngress::V3, 1, &f).unwrap_err();
        assert!(err.to_string().contains("whitelisted channel"), "{err:#}");
    }
}
