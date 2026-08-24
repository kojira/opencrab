//! Discord kind 行の runtime upsert と起動列挙。secret 値は読まない・返さない。

use crate::{Result, Store};
use opencrab_port::{GateInstanceId, IngressDiscovery, OriginScope};
use rusqlite::{params, Connection};

const DISCORD_KIND: &str = "discord";
const TOKEN_NAME: &str = "discord_bot_token";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscordLaunchDecision {
    pub instance_id: GateInstanceId,
    pub label: String,
    pub start: bool,
}

pub fn upsert_discord_kind_on(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO gate_kinds(kind_id,protocol_major,origin_scope,ingress_discovery)
         VALUES(?1,2,?2,?3)
         ON CONFLICT(kind_id) DO UPDATE SET protocol_major=2,origin_scope=?2,ingress_discovery=?3",
        params![
            DISCORD_KIND,
            OriginScope::KindAddress.as_wire(),
            IngressDiscovery::Membership.as_wire()
        ],
    )?;
    Ok(())
}

/// present && enabled の discord instance。dedicated かつ secret 非空だけ start=true。
/// `shared:*` は token 非空でも start=false（v15 §8 未実装）。値は SELECT しない。
pub fn discord_launch_decisions_on(conn: &Connection) -> Result<Vec<DiscordLaunchDecision>> {
    let mut stmt = conn.prepare(
        "SELECT gi.instance_id, gi.label,
                CASE WHEN sv.value IS NOT NULL AND length(sv.value) > 0 THEN 1 ELSE 0 END
         FROM gate_instances gi
         JOIN gate_instance_revisions r
           ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
         LEFT JOIN secret_values sv
           ON sv.secret_set_id=r.secret_set_id AND sv.name=?1
         WHERE gi.kind_id=?2 AND r.present=1 AND r.enabled=1
         ORDER BY gi.instance_id",
    )?;
    let rows = stmt.query_map(params![TOKEN_NAME, DISCORD_KIND], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (instance, label, token_present) = row?;
        let shared = label.starts_with("shared:");
        out.push(DiscordLaunchDecision {
            instance_id: GateInstanceId::parse(instance)
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
            label,
            start: token_present != 0 && !shared,
        });
    }
    Ok(out)
}

pub fn discord_launch_decisions_read_only(
    path: impl AsRef<std::path::Path>,
) -> Result<Vec<DiscordLaunchDecision>> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.execute_batch("BEGIN")?;
    let rows = discord_launch_decisions_on(&conn);
    conn.execute_batch("ROLLBACK")?;
    rows
}

impl Store {
    pub fn upsert_discord_kind(&self) -> Result<()> {
        upsert_discord_kind_on(&self.c())
    }

    pub fn discord_launch_decisions(&self) -> Result<Vec<DiscordLaunchDecision>> {
        discord_launch_decisions_on(&self.c())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256;
    use opencrab_port::GateKindId;

    fn discord_kind() -> GateKindId {
        GateKindId::parse("discord".to_string()).unwrap()
    }

    #[test]
    fn upsert_discord_kind_is_idempotent_and_membership() {
        let store = Store::new_in_memory().unwrap();
        store.upsert_discord_kind().unwrap();
        store.upsert_discord_kind().unwrap();
        let row: (i64, String, String) = store
            .c()
            .query_row(
                "SELECT protocol_major,origin_scope,ingress_discovery FROM gate_kinds WHERE kind_id='discord'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(row.0, 2);
        assert_eq!(row.1, OriginScope::KindAddress.as_wire());
        assert_eq!(row.2, IngressDiscovery::Membership.as_wire());
        let count: i64 = store
            .c()
            .query_row(
                "SELECT COUNT(*) FROM gate_kinds WHERE kind_id='discord'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn launch_decisions_start_dedicated_with_token_refuse_shared() {
        let store = Store::new_in_memory().unwrap();
        store.upsert_discord_kind().unwrap();
        let dedicated =
            GateInstanceId::parse("018f0000-0000-7000-8000-000000000031".to_string()).unwrap();
        let shared =
            GateInstanceId::parse("018f0000-0000-7000-8000-000000000032".to_string()).unwrap();
        let empty =
            GateInstanceId::parse("018f0000-0000-7000-8000-000000000033".to_string()).unwrap();
        store
            .install_gate_instance_revision(
                &dedicated,
                &discord_kind(),
                "dedicated:discord:Zg",
                Some(1),
                1,
                true,
                OriginScope::KindAddress,
                IngressDiscovery::Membership,
                "gate-config/discord/v1",
                b"{}",
                1,
            )
            .unwrap();
        store
            .install_gate_instance_revision(
                &shared,
                &discord_kind(),
                "shared:discord",
                None,
                1,
                true,
                OriginScope::KindAddress,
                IngressDiscovery::Membership,
                "gate-config/discord/v1",
                b"{}",
                1,
            )
            .unwrap();
        store
            .install_gate_instance_revision(
                &empty,
                &discord_kind(),
                "dedicated:discord:YQ",
                Some(2),
                1,
                true,
                OriginScope::KindAddress,
                IngressDiscovery::Membership,
                "gate-config/discord/v1",
                b"{}",
                1,
            )
            .unwrap();
        let conn = store.c();
        conn.execute(
            "INSERT INTO secret_sets(secret_set_id,revision,scope,created_at) VALUES('set-d',1,'gate-instance:d',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO secret_sets(secret_set_id,revision,scope,created_at) VALUES('set-s',1,'gate-instance:s',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO secret_sets(secret_set_id,revision,scope,created_at) VALUES('set-e',1,'gate-instance:e',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE gate_instance_revisions SET secret_set_id='set-d' WHERE instance_id=?1",
            params![dedicated.as_str()],
        )
        .unwrap();
        conn.execute(
            "UPDATE gate_instance_revisions SET secret_set_id='set-s' WHERE instance_id=?1",
            params![shared.as_str()],
        )
        .unwrap();
        conn.execute(
            "UPDATE gate_instance_revisions SET secret_set_id='set-e' WHERE instance_id=?1",
            params![empty.as_str()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO secret_values(secret_set_id,name,value,at_rest_format,value_digest)
             VALUES('set-d','discord_bot_token',x'616263','source-plaintext',?1)",
            params![sha256(b"abc")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO secret_values(secret_set_id,name,value,at_rest_format,value_digest)
             VALUES('set-s','discord_bot_token',x'646566','source-plaintext',?1)",
            params![sha256(b"def")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO secret_values(secret_set_id,name,value,at_rest_format,value_digest)
             VALUES('set-e','discord_bot_token',x'','source-plaintext',?1)",
            params![sha256(b"")],
        )
        .unwrap();
        drop(conn);
        let rows = store.discord_launch_decisions().unwrap();
        assert_eq!(
            rows,
            vec![
                DiscordLaunchDecision {
                    instance_id: dedicated,
                    label: "dedicated:discord:Zg".into(),
                    start: true,
                },
                DiscordLaunchDecision {
                    instance_id: shared,
                    label: "shared:discord".into(),
                    start: false,
                },
                DiscordLaunchDecision {
                    instance_id: empty,
                    label: "dedicated:discord:YQ".into(),
                    start: false,
                },
            ]
        );
    }
}
