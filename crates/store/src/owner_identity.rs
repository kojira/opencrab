//! Owner ID 日次経路の store コマンド（DESIGN-OWNER-IDENTITY）。
//!
//! `add_identity` の ON CONFLICT steal は呼ばない。衝突は fail loud。
//! 昇格の合成（revision → identity 解決 → standing）は同一 TX。

use crate::{reconcile_subject_routes_on, runtime_uuid_v7, sha256, Result, Store};
use opencrab_port::{
    GateInstanceId, GateKindId, IngressDiscovery, OriginScope, PlaceId, RoutePurpose, Standing,
    SubjectId,
};
use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::{json, Value};

#[derive(Debug)]
pub enum OwnerIdentityError {
    Store(rusqlite::Error),
    InstanceUnknown,
    IdentityConflict,
    AmbiguousIdentity,
}

impl From<rusqlite::Error> for OwnerIdentityError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

impl std::fmt::Display for OwnerIdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "{error}"),
            Self::InstanceUnknown => write!(f, "gate instance unknown"),
            Self::IdentityConflict => {
                write!(f, "gate identity already bound to another subject")
            }
            Self::AmbiguousIdentity => {
                write!(f, "owner external id maps to multiple subjects")
            }
        }
    }
}

/// PATCH 省略 = keep。`Set("")` はクリア（拒否しない）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnerExternalChange {
    Keep,
    Set(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateOwnerProjection {
    pub instance_id: GateInstanceId,
    pub kind_id: GateKindId,
    pub present: bool,
    pub enabled: bool,
    pub running: bool,
    pub owner_external_id: String,
    pub has_secret: bool,
    pub config_bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerPrincipalOutcome {
    pub instance_id: GateInstanceId,
    pub owner_external_id: String,
    pub revision: u64,
    pub elevated: Option<SubjectId>,
}

fn standing_sql(standing: Standing) -> &'static str {
    match standing {
        Standing::Owner => "owner",
        Standing::Trusted => "trusted",
        Standing::Unknown => "unknown",
    }
}

fn parse_kind(kind: String) -> Result<GateKindId> {
    GateKindId::parse(kind).map_err(|_| rusqlite::Error::InvalidQuery)
}

fn parse_instance(id: String) -> Result<GateInstanceId> {
    GateInstanceId::parse(id).map_err(|_| rusqlite::Error::InvalidQuery)
}

struct ActiveRevision {
    revision: i64,
    present: bool,
    enabled: bool,
    schema: String,
    bytes: Vec<u8>,
    secret_set: Option<String>,
}

fn read_active_revision(
    tx: &Transaction<'_>,
    instance: &GateInstanceId,
) -> Result<Option<ActiveRevision>> {
    tx.query_row(
        "SELECT r.revision,r.present,r.enabled,r.config_schema_id,r.config_bytes,r.secret_set_id
         FROM gate_instances gi
         JOIN gate_instance_revisions r
           ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
         WHERE gi.instance_id=?1",
        params![instance.as_str()],
        |row| {
            Ok(ActiveRevision {
                revision: row.get(0)?,
                present: row.get::<_, i64>(1)? != 0,
                enabled: row.get::<_, i64>(2)? != 0,
                schema: row.get(3)?,
                bytes: row.get(4)?,
                secret_set: row.get(5)?,
            })
        },
    )
    .optional()
}

fn owner_from_bytes(bytes: &[u8]) -> Result<String> {
    let parsed: Value = serde_json::from_slice(bytes).map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(parsed
        .get("owner_external_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string())
}

fn with_owner_external_id(bytes: &[u8], owner_external_id: &str) -> Result<Vec<u8>> {
    let mut parsed: Value =
        serde_json::from_slice(bytes).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let object = parsed
        .as_object_mut()
        .ok_or(rusqlite::Error::InvalidQuery)?;
    object.insert(
        "owner_external_id".into(),
        Value::String(owner_external_id.to_string()),
    );
    serde_json::to_vec(&parsed).map_err(|_| rusqlite::Error::InvalidQuery)
}

fn identity_on_instance(
    tx: &Transaction<'_>,
    instance: &GateInstanceId,
    external_id: &str,
) -> Result<Option<SubjectId>> {
    tx.query_row(
        "SELECT subject_id FROM gate_subject_identities
         WHERE instance_id=?1 AND external_id=?2",
        params![instance.as_str(), external_id],
        |row| row.get(0),
    )
    .optional()
}

fn participant_subjects(tx: &Transaction<'_>, instance: &GateInstanceId) -> Result<Vec<SubjectId>> {
    let (owner, bytes): (Option<i64>, Vec<u8>) = tx.query_row(
        "SELECT gi.owner_subject_id,r.config_bytes
         FROM gate_instances gi
         JOIN gate_instance_revisions r
           ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
         WHERE gi.instance_id=?1",
        params![instance.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut subjects = Vec::new();
    if let Some(owner) = owner {
        subjects.push(owner);
    }
    if let Ok(parsed) = serde_json::from_slice::<Value>(&bytes) {
        if let Some(ids) = parsed.get("agent_ids").and_then(Value::as_array) {
            for id in ids {
                if let Some(agent) = id.as_str() {
                    if let Ok(subject) = agent.parse::<i64>() {
                        if !subjects.contains(&subject) {
                            subjects.push(subject);
                        }
                    }
                }
            }
        }
    }
    Ok(subjects)
}

fn materialized_places(tx: &Transaction<'_>, instance: &GateInstanceId) -> Result<Vec<PlaceId>> {
    let mut stmt = tx.prepare(
        "SELECT DISTINCT place_id FROM gate_bindings WHERE instance_id=?1 ORDER BY place_id",
    )?;
    let rows = stmt.query_map(params![instance.as_str()], |row| row.get(0))?;
    rows.collect()
}

fn reconcile_owner_principal_on(
    tx: &Transaction<'_>,
    instance: &GateInstanceId,
    old_subjects: &[SubjectId],
    new_subjects: &[SubjectId],
) -> Result<()> {
    let kind: String = tx.query_row(
        "SELECT kind_id FROM gate_instances WHERE instance_id=?1",
        params![instance.as_str()],
        |row| row.get(0),
    )?;
    let kind = parse_kind(kind)?;
    let mut union = old_subjects.to_vec();
    for subject in new_subjects {
        if !union.contains(subject) {
            union.push(*subject);
        }
    }
    union.sort_unstable();
    let places = materialized_places(tx, instance)?;
    for subject in union {
        for place in &places {
            let binding: Option<String> = tx
                .query_row(
                    "SELECT binding_id FROM gate_bindings
                     WHERE instance_id=?1 AND place_id=?2 ORDER BY binding_id LIMIT 1",
                    params![instance.as_str(), place],
                    |row| row.get(0),
                )
                .optional()?;
            let purposes = [RoutePurpose::inbound(), RoutePurpose::outbound()];
            reconcile_subject_routes_on(tx, subject, *place, &kind, binding.as_deref(), &purposes)?;
        }
    }
    Ok(())
}

fn set_subject_standing_on(
    tx: &Transaction<'_>,
    subject: SubjectId,
    standing: Standing,
) -> Result<()> {
    let n = tx.execute(
        "UPDATE subjects SET standing=?1 WHERE id=?2",
        params![standing_sql(standing), subject],
    )?;
    if n != 1 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    Ok(())
}

fn register_gate_identity_on(
    tx: &Transaction<'_>,
    instance: &GateInstanceId,
    external_id: &str,
    subject: SubjectId,
    display_name: Option<&str>,
) -> std::result::Result<(), OwnerIdentityError> {
    if let Some(existing) = identity_on_instance(tx, instance, external_id)? {
        if existing != subject {
            return Err(OwnerIdentityError::IdentityConflict);
        }
        tx.execute(
            "UPDATE gate_subject_identities SET display_name=?1
             WHERE instance_id=?2 AND external_id=?3",
            params![display_name, instance.as_str(), external_id],
        )?;
        return Ok(());
    }
    tx.execute(
        "INSERT INTO gate_subject_identities(instance_id,external_id,subject_id,display_name)
         VALUES(?1,?2,?3,?4)",
        params![instance.as_str(), external_id, subject, display_name],
    )?;
    Ok(())
}

fn resolve_owner_subject(
    tx: &Transaction<'_>,
    instance: &GateInstanceId,
    external_id: &str,
) -> std::result::Result<Option<SubjectId>, OwnerIdentityError> {
    if external_id.is_empty() {
        return Ok(None);
    }
    if let Some(subject) = identity_on_instance(tx, instance, external_id)? {
        return Ok(Some(subject));
    }
    let kind: String = tx.query_row(
        "SELECT kind_id FROM gate_instances WHERE instance_id=?1",
        params![instance.as_str()],
        |row| row.get(0),
    )?;
    let mut stmt = tx.prepare(
        "SELECT DISTINCT i.subject_id FROM gate_subject_identities i
         JOIN gate_instances gi ON gi.instance_id=i.instance_id
         WHERE gi.kind_id=?1 AND i.external_id=?2 ORDER BY i.subject_id",
    )?;
    let found = stmt
        .query_map(params![kind, external_id], |row| row.get(0))?
        .collect::<Result<Vec<SubjectId>>>()?;
    match found.as_slice() {
        [] => Ok(None),
        [subject] => Ok(Some(*subject)),
        _ => Err(OwnerIdentityError::AmbiguousIdentity),
    }
}

fn apply_owner_principal_on(
    tx: &Transaction<'_>,
    instance: &GateInstanceId,
    change: &OwnerExternalChange,
    now: i64,
) -> std::result::Result<OwnerPrincipalOutcome, OwnerIdentityError> {
    let current = read_active_revision(tx, instance)?.ok_or(OwnerIdentityError::InstanceUnknown)?;
    let ActiveRevision {
        revision,
        present,
        enabled,
        schema,
        bytes,
        secret_set,
    } = current;
    if !present {
        return Err(OwnerIdentityError::InstanceUnknown);
    }
    let old_owner = owner_from_bytes(&bytes)?;
    let new_owner = match change {
        OwnerExternalChange::Keep => old_owner.clone(),
        OwnerExternalChange::Set(value) => value.clone(),
    };
    let old_subjects = participant_subjects(tx, instance)?;
    let old_subject = if old_owner.is_empty() {
        None
    } else {
        identity_on_instance(tx, instance, &old_owner)?
    };
    let next_revision = revision + 1;
    let next_bytes = if new_owner == old_owner {
        bytes.clone()
    } else {
        with_owner_external_id(&bytes, &new_owner)?
    };
    if new_owner != old_owner {
        tx.execute(
            "INSERT INTO gate_instance_revisions(
               instance_id,revision,present,enabled,config_schema_id,config_bytes,
               config_digest,secret_set_id,created_at
             ) VALUES(?1,?2,1,?3,?4,?5,?6,?7,?8)",
            params![
                instance.as_str(),
                next_revision,
                enabled as i64,
                schema,
                next_bytes,
                sha256(&next_bytes),
                secret_set,
                now
            ],
        )?;
        tx.execute(
            "UPDATE gate_instances SET active_revision=?1 WHERE instance_id=?2",
            params![next_revision, instance.as_str()],
        )?;
    }
    let new_subjects = participant_subjects(tx, instance)?;
    if new_owner != old_owner {
        reconcile_owner_principal_on(tx, instance, &old_subjects, &new_subjects)?;
    }
    let mut elevated = None;
    if let OwnerExternalChange::Set(_) = change {
        if let Some(previous) = old_subject {
            if old_owner != new_owner {
                set_subject_standing_on(tx, previous, Standing::Unknown)?;
            }
        }
        if !new_owner.is_empty() {
            if let Some(subject) = resolve_owner_subject(tx, instance, &new_owner)? {
                if identity_on_instance(tx, instance, &new_owner)?.is_none() {
                    register_gate_identity_on(tx, instance, &new_owner, subject, None)?;
                }
                set_subject_standing_on(tx, subject, Standing::Owner)?;
                elevated = Some(subject);
            }
        }
    }
    let written_revision = if new_owner == old_owner {
        revision as u64
    } else {
        next_revision as u64
    };
    Ok(OwnerPrincipalOutcome {
        instance_id: instance.clone(),
        owner_external_id: new_owner,
        revision: written_revision,
        elevated,
    })
}

fn dedicated_label(kind: &str, agent_id: &str) -> String {
    format!("dedicated:{kind}:{}", b64url_nopad(agent_id.as_bytes()))
}

fn b64url_nopad(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = bytes.get(i + 1).copied();
        let b2 = bytes.get(i + 2).copied();
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        if b1.is_some() {
            out.push(
                T[(((b1.unwrap_or(0) & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char,
            );
        }
        if b2.is_some() {
            out.push(T[(b2.unwrap_or(0) & 0x3f) as usize] as char);
        }
        i += 3;
    }
    out
}

fn default_config_bytes(kind: &str, owner_external_id: &str) -> Result<Vec<u8>> {
    let value = match kind {
        "discord" => json!({
            "agent_ids": [],
            "legacy_updated_at": "",
            "owner_external_id": owner_external_id,
            "self_external_id": Value::Null,
        }),
        "nostr" => json!({
            "filter": {"authors": [], "keywords": [], "kinds": []},
            "legacy_updated_at": "",
            "owner_external_id": owner_external_id,
            "relays": [],
            "self_external_id": "",
        }),
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    serde_json::to_vec(&value).map_err(|_| rusqlite::Error::InvalidQuery)
}

impl Store {
    pub fn commit_gate_config_revision(
        &self,
        instance: &GateInstanceId,
        owner_external_id: &str,
        now: i64,
    ) -> std::result::Result<OwnerPrincipalOutcome, OwnerIdentityError> {
        self.apply_owner_principal(
            instance,
            OwnerExternalChange::Set(owner_external_id.into()),
            now,
        )
    }

    pub fn register_gate_identity(
        &self,
        instance: &GateInstanceId,
        external_id: &str,
        subject: SubjectId,
        display_name: Option<&str>,
    ) -> std::result::Result<(), OwnerIdentityError> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        register_gate_identity_on(&tx, instance, external_id, subject, display_name)?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_subject_standing(&self, subject: SubjectId, standing: Standing) -> Result<()> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        set_subject_standing_on(&tx, subject, standing)?;
        tx.commit()
    }

    pub fn apply_owner_principal(
        &self,
        instance: &GateInstanceId,
        change: OwnerExternalChange,
        now: i64,
    ) -> std::result::Result<OwnerPrincipalOutcome, OwnerIdentityError> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = apply_owner_principal_on(&tx, instance, &change, now)?;
        tx.commit()?;
        Ok(out)
    }

    pub fn dedicated_gate_instance(
        &self,
        kind: &GateKindId,
        owner_subject: SubjectId,
    ) -> std::result::Result<Option<GateInstanceId>, OwnerIdentityError> {
        let conn = self.c();
        let mut stmt = conn.prepare(
            "SELECT instance_id FROM gate_instances
             WHERE kind_id=?1 AND owner_subject_id=?2 ORDER BY instance_id",
        )?;
        let rows = stmt
            .query_map(params![kind.as_str(), owner_subject], |row| row.get(0))?
            .collect::<Result<Vec<String>>>()?;
        match rows.as_slice() {
            [] => Ok(None),
            [id] => Ok(Some(parse_instance(id.clone())?)),
            _ => Err(OwnerIdentityError::AmbiguousIdentity),
        }
    }

    pub fn ensure_dedicated_gate_instance(
        &self,
        kind: &GateKindId,
        owner_subject: SubjectId,
        now: i64,
    ) -> std::result::Result<GateInstanceId, OwnerIdentityError> {
        if let Some(existing) = self.dedicated_gate_instance(kind, owner_subject)? {
            return Ok(existing);
        }
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let kind_name = kind.as_str();
        let schema = match kind_name {
            "discord" => "gate-config/discord/v1",
            "nostr" => "gate-config/nostr/v1",
            _ => return Err(rusqlite::Error::InvalidQuery.into()),
        };
        tx.execute(
            "INSERT INTO gate_kinds(kind_id,protocol_major,origin_scope,ingress_discovery)
             VALUES(?1,2,?2,?3)
             ON CONFLICT(kind_id) DO UPDATE SET protocol_major=2,origin_scope=?2,ingress_discovery=?3",
            params![
                kind_name,
                OriginScope::KindAddress.as_wire(),
                IngressDiscovery::Membership.as_wire()
            ],
        )?;
        let instance = parse_instance(runtime_uuid_v7(
            now,
            &format!("dedicated\0{kind_name}\0{owner_subject}"),
        ))?;
        let bytes = default_config_bytes(kind_name, "")?;
        tx.execute(
            "INSERT INTO gate_instances(instance_id,kind_id,label,owner_subject_id,active_revision,lifecycle)
             VALUES(?1,?2,?3,?4,1,'stopped')",
            params![
                instance.as_str(),
                kind_name,
                dedicated_label(kind_name, &owner_subject.to_string()),
                owner_subject
            ],
        )?;
        tx.execute(
            "INSERT INTO gate_instance_revisions(
               instance_id,revision,present,enabled,config_schema_id,config_bytes,
               config_digest,secret_set_id,created_at
             ) VALUES(?1,1,1,1,?2,?3,?4,NULL,?5)",
            params![instance.as_str(), schema, bytes, sha256(&bytes), now],
        )?;
        tx.commit()?;
        Ok(instance)
    }

    pub fn gate_owner_projection(
        &self,
        instance: &GateInstanceId,
    ) -> Result<Option<GateOwnerProjection>> {
        let conn = self.c();
        let row = conn
            .query_row(
                "SELECT gi.kind_id,r.present,r.enabled,r.config_bytes,r.secret_set_id,
                        (SELECT COUNT(*) FROM gate_connections gc
                          WHERE gc.instance_id=gi.instance_id
                            AND gc.revision=gi.active_revision
                            AND gc.state='active')
                 FROM gate_instances gi
                 JOIN gate_instance_revisions r
                   ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
                 WHERE gi.instance_id=?1",
                params![instance.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, i64>(2)? != 0,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, i64>(5)? != 0,
                    ))
                },
            )
            .optional()?;
        let Some((kind, present, enabled, bytes, secret_set, running)) = row else {
            return Ok(None);
        };
        let has_secret = match secret_set {
            Some(set_id) => {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM secret_values WHERE secret_set_id=?1 AND length(value)>0)",
                    params![set_id],
                    |row| row.get::<_, i64>(0),
                )? != 0
            }
            None => false,
        };
        Ok(Some(GateOwnerProjection {
            instance_id: instance.clone(),
            kind_id: parse_kind(kind)?,
            present,
            enabled,
            running: present && running,
            owner_external_id: owner_from_bytes(&bytes)?,
            has_secret,
            config_bytes: bytes,
        }))
    }

    pub fn tombstone_gate_instance(
        &self,
        instance: &GateInstanceId,
        now: i64,
    ) -> std::result::Result<bool, OwnerIdentityError> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some(current) = read_active_revision(&tx, instance)? else {
            return Err(OwnerIdentityError::InstanceUnknown);
        };
        let ActiveRevision {
            revision,
            present,
            schema,
            bytes,
            secret_set,
            ..
        } = current;
        if !present {
            tx.commit()?;
            return Ok(false);
        }
        let next = revision + 1;
        tx.execute(
            "INSERT INTO gate_instance_revisions(
               instance_id,revision,present,enabled,config_schema_id,config_bytes,
               config_digest,secret_set_id,created_at
             ) VALUES(?1,?2,0,0,?3,?4,?5,?6,?7)",
            params![
                instance.as_str(),
                next,
                schema,
                bytes,
                sha256(&bytes),
                secret_set,
                now
            ],
        )?;
        tx.execute(
            "UPDATE gate_instances SET active_revision=?1,lifecycle='stopped' WHERE instance_id=?2",
            params![next, instance.as_str()],
        )?;
        tx.execute(
            "UPDATE gate_connections SET state='closed',disconnected_at=?2
             WHERE instance_id=?1 AND state='active'",
            params![instance.as_str(), now],
        )?;
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_port::{IngressDiscovery, SubjectKind};

    const INSTANCE: &str = "018f8020-0000-7000-8000-000000000001";

    fn kind() -> GateKindId {
        GateKindId::parse("discord".to_string()).unwrap()
    }

    fn instance() -> GateInstanceId {
        GateInstanceId::parse(INSTANCE.to_string()).unwrap()
    }

    fn seed_dedicated(store: &Store) -> (SubjectId, SubjectId) {
        let agent = store
            .create_subject(
                SubjectKind::Agent,
                "A",
                "persona",
                "engine",
                Standing::Trusted,
                1,
            )
            .unwrap();
        let human = store
            .create_subject(SubjectKind::Human, "H", "", "", Standing::Unknown, 2)
            .unwrap();
        let config = serde_json::to_vec(&json!({
            "agent_ids": [],
            "legacy_updated_at": "",
            "owner_external_id": "",
            "self_external_id": Value::Null,
        }))
        .unwrap();
        store
            .install_gate_instance_revision(
                &instance(),
                &kind(),
                "dedicated:discord:Zg",
                Some(agent),
                1,
                true,
                OriginScope::KindAddress,
                IngressDiscovery::Membership,
                "gate-config/discord/v1",
                &config,
                10,
            )
            .unwrap();
        (agent, human)
    }

    fn owner_bytes(store: &Store) -> String {
        let bytes: Vec<u8> = store
            .c()
            .query_row(
                "SELECT r.config_bytes FROM gate_instances gi
                 JOIN gate_instance_revisions r
                   ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
                 WHERE gi.instance_id=?1",
                params![INSTANCE],
                |row| row.get(0),
            )
            .unwrap();
        owner_from_bytes(&bytes).unwrap()
    }

    fn standing_of(store: &Store, subject: SubjectId) -> String {
        store
            .c()
            .query_row(
                "SELECT standing FROM subjects WHERE id=?1",
                params![subject],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn set_writes_owner_external_id_and_elevates_existing_identity() {
        let store = Store::new_in_memory().unwrap();
        let (_agent, human) = seed_dedicated(&store);
        store
            .register_gate_identity(&instance(), "owner-1", human, None)
            .unwrap();
        let out = store
            .commit_gate_config_revision(&instance(), "owner-1", 20)
            .unwrap();
        assert_eq!(out.owner_external_id, "owner-1");
        assert_eq!(out.elevated, Some(human));
        assert_eq!(owner_bytes(&store), "owner-1");
        assert_eq!(standing_of(&store, human), "owner");
        let rev: i64 = store
            .c()
            .query_row(
                "SELECT active_revision FROM gate_instances WHERE instance_id=?1",
                params![INSTANCE],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rev, 2);
    }

    #[test]
    fn clear_writes_empty_and_does_not_lock_last_owner() {
        let store = Store::new_in_memory().unwrap();
        let (_agent, human) = seed_dedicated(&store);
        store
            .register_gate_identity(&instance(), "owner-1", human, None)
            .unwrap();
        store
            .commit_gate_config_revision(&instance(), "owner-1", 20)
            .unwrap();
        let out = store
            .commit_gate_config_revision(&instance(), "", 21)
            .unwrap();
        assert_eq!(out.owner_external_id, "");
        assert_eq!(out.elevated, None);
        assert_eq!(owner_bytes(&store), "");
        assert_eq!(standing_of(&store, human), "unknown");
    }

    #[test]
    fn keep_leaves_owner_and_standing() {
        let store = Store::new_in_memory().unwrap();
        let (_agent, human) = seed_dedicated(&store);
        store
            .register_gate_identity(&instance(), "owner-1", human, None)
            .unwrap();
        store
            .commit_gate_config_revision(&instance(), "owner-1", 20)
            .unwrap();
        let out = store
            .apply_owner_principal(&instance(), OwnerExternalChange::Keep, 21)
            .unwrap();
        assert_eq!(out.owner_external_id, "owner-1");
        assert_eq!(out.revision, 2);
        assert_eq!(owner_bytes(&store), "owner-1");
        assert_eq!(standing_of(&store, human), "owner");
    }

    #[test]
    fn register_gate_identity_fails_loud_on_steal() {
        let store = Store::new_in_memory().unwrap();
        let (_agent, human) = seed_dedicated(&store);
        let other = store
            .create_subject(SubjectKind::Human, "X", "", "", Standing::Unknown, 3)
            .unwrap();
        store
            .register_gate_identity(&instance(), "owner-1", human, None)
            .unwrap();
        let err = store
            .register_gate_identity(&instance(), "owner-1", other, None)
            .expect_err("steal must fail");
        assert!(matches!(err, OwnerIdentityError::IdentityConflict));
        let bound: i64 = store
            .c()
            .query_row(
                "SELECT subject_id FROM gate_subject_identities
                 WHERE instance_id=?1 AND external_id='owner-1'",
                params![INSTANCE],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bound, human);
    }

    #[test]
    fn set_without_identity_writes_config_and_does_not_invent_owner() {
        let store = Store::new_in_memory().unwrap();
        let (_agent, human) = seed_dedicated(&store);
        let out = store
            .commit_gate_config_revision(&instance(), "owner-new", 20)
            .unwrap();
        assert_eq!(out.owner_external_id, "owner-new");
        assert_eq!(out.elevated, None);
        assert_eq!(standing_of(&store, human), "unknown");
        let identities: i64 = store
            .c()
            .query_row("SELECT COUNT(*) FROM gate_subject_identities", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(identities, 0);
    }

    #[test]
    fn set_subject_standing_accepts_unknown_without_last_owner_lock() {
        let store = Store::new_in_memory().unwrap();
        let human = store
            .create_subject(SubjectKind::Human, "H", "", "", Standing::Owner, 1)
            .unwrap();
        store
            .set_subject_standing(human, Standing::Unknown)
            .unwrap();
        assert_eq!(standing_of(&store, human), "unknown");
    }
}
