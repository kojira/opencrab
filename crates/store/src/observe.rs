//! `ObserveGateAddress`（DESIGN-DISCORD-GATE v15 §1.2）。runtime が transform を呼ばない。

use crate::{
    declared_discord_operations, reconcile_subject_routes_on, runtime_uuid_v7, sha256, Result,
    Store,
};
use opencrab_port::{AddressKind, GateInstanceId, GateKindId, PlaceId, RoutePurpose, SubjectId};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};

const KIND: &str = "discord";
const SOURCE_SYSTEM: &str = "discord";
const BINDING_SCHEMA: &str = "gate-binding/discord/v1";
const CONFIG_SCHEMA: &str = "gate-config/discord/v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LabelUpdate {
    Present(String),
    Absent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObserveRequest<'a> {
    pub instance: &'a GateInstanceId,
    pub address: &'a str,
    pub address_kind: AddressKind,
    pub author_external_id: &'a str,
    pub guild_id: Option<&'a str>,
    pub label: LabelUpdate,
    pub observed_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObserveGateAddress {
    Rejected,
    Ready {
        place: PlaceId,
        binding_id: String,
        admitted_fanout_subjects: Vec<SubjectId>,
    },
    BindingAmbiguous,
    BindingMetadataConflict,
    SourceRefMetadataShapeConflict,
    ParticipantResolutionError,
    ConcurrentEquivalent {
        place: PlaceId,
        binding_id: String,
        admitted_fanout_subjects: Vec<SubjectId>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoredKind {
    Guild,
    Dm,
    Unknown,
}

struct BindingRow {
    binding_id: String,
    place_id: PlaceId,
    metadata: Vec<u8>,
}

struct SourceRefRow {
    place_id: PlaceId,
    classification: String,
    metadata: Option<String>,
}

enum ObserveFail {
    Outcome(ObserveGateAddress),
    Store(rusqlite::Error),
}

impl From<rusqlite::Error> for ObserveFail {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

fn is_unique(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

fn discord_kind() -> Result<GateKindId> {
    GateKindId::parse(KIND.to_string()).map_err(|_| rusqlite::Error::InvalidQuery)
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

fn dedicated_label(agent_id: &str) -> String {
    format!("dedicated:discord:{}", b64url_nopad(agent_id.as_bytes()))
}

fn decode_sqlite_values(bytes: &[u8]) -> Option<Vec<Option<i64>>> {
    if bytes.len() < 8 {
        return None;
    }
    let count = u64::from_be_bytes(bytes[0..8].try_into().ok()?);
    let mut offset = 8usize;
    let mut ints = Vec::new();
    for _ in 0..count {
        let tag = *bytes.get(offset)?;
        offset += 1;
        match tag {
            0 => ints.push(None),
            1 => {
                let value = i64::from_be_bytes(bytes.get(offset..offset + 8)?.try_into().ok()?);
                offset += 8;
                ints.push(Some(value));
            }
            2 => {
                offset = offset.checked_add(8)?;
                ints.push(None);
            }
            3 | 4 => {
                let len =
                    u64::from_be_bytes(bytes.get(offset..offset + 8)?.try_into().ok()?) as usize;
                offset = offset.checked_add(8)?.checked_add(len)?;
                ints.push(None);
            }
            _ => return None,
        }
    }
    Some(ints)
}

/// frozen source `discord_channel_config` ordinal 6 = whitelisted。0=false、nonzero=true。
fn decode_whitelisted(source_row: &[u8]) -> Option<bool> {
    let values = decode_sqlite_values(source_row)?;
    let raw = values.get(6).copied()?;
    Some(raw? != 0)
}

fn decode_heartbeat_enabled(source_row: &[u8]) -> Option<bool> {
    let values = decode_sqlite_values(source_row)?;
    let raw = values.get(7).copied()?;
    Some(raw? != 0)
}

fn stored_kind(bytes: &[u8]) -> Option<StoredKind> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    match value.get("address_kind")?.as_str()? {
        "guild" => Some(StoredKind::Guild),
        "dm" => Some(StoredKind::Dm),
        "unknown" => Some(StoredKind::Unknown),
        _ => None,
    }
}

fn stored_guild_id(bytes: &[u8]) -> Option<Option<String>> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    match value.get("guild_id") {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(id)) => Some(Some(id.clone())),
        _ => None,
    }
}

fn metadata_bytes(kind: AddressKind, guild_id: Option<&str>) -> Vec<u8> {
    let address_kind = match kind {
        AddressKind::Guild => "guild",
        AddressKind::Dm => "dm",
        AddressKind::Thread => "unknown",
    };
    serde_json::to_vec(&json!({
        "address_kind": address_kind,
        "guild_id": guild_id,
    }))
    .expect("binding metadata is object")
}

fn resolve_participants(
    tx: &Transaction<'_>,
    instance: &GateInstanceId,
) -> std::result::Result<Vec<SubjectId>, ObserveFail> {
    let row = tx
        .query_row(
            "SELECT gi.owner_subject_id,r.config_schema_id,r.config_bytes
             FROM gate_instances gi
             JOIN gate_instance_revisions r
               ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
             WHERE gi.instance_id=?1 AND r.present=1",
            params![instance.as_str()],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(ObserveFail::Store)?;
    let Some((owner, schema, config)) = row else {
        return Err(ObserveFail::Outcome(
            ObserveGateAddress::ParticipantResolutionError,
        ));
    };
    if schema != CONFIG_SCHEMA {
        return Err(ObserveFail::Outcome(
            ObserveGateAddress::ParticipantResolutionError,
        ));
    }
    if let Some(owner) = owner {
        return Ok(vec![owner]);
    }
    let parsed: Value = serde_json::from_slice(&config)
        .map_err(|_| ObserveFail::Outcome(ObserveGateAddress::ParticipantResolutionError))?;
    let ids = parsed
        .get("agent_ids")
        .and_then(Value::as_array)
        .ok_or(ObserveFail::Outcome(
            ObserveGateAddress::ParticipantResolutionError,
        ))?;
    let mut subjects = Vec::new();
    for id in ids {
        let agent = id.as_str().ok_or(ObserveFail::Outcome(
            ObserveGateAddress::ParticipantResolutionError,
        ))?;
        let label = dedicated_label(agent);
        let found: Vec<i64> = {
            let mut stmt = tx
                .prepare(
                    "SELECT DISTINCT owner_subject_id FROM gate_instances
                     WHERE kind_id=?1 AND label=?2 AND owner_subject_id IS NOT NULL",
                )
                .map_err(ObserveFail::Store)?;
            let rows = stmt
                .query_map(params![KIND, label], |row| row.get(0))
                .map_err(ObserveFail::Store)?;
            rows.collect::<Result<Vec<_>>>()
                .map_err(ObserveFail::Store)?
        };
        if found.len() != 1 {
            return Err(ObserveFail::Outcome(
                ObserveGateAddress::ParticipantResolutionError,
            ));
        }
        if !subjects.contains(&found[0]) {
            subjects.push(found[0]);
        }
    }
    if subjects.is_empty() {
        return Err(ObserveFail::Outcome(
            ObserveGateAddress::ParticipantResolutionError,
        ));
    }
    Ok(subjects)
}

fn resolve_whitelisted(
    tx: &Transaction<'_>,
    subject: SubjectId,
    place: Option<PlaceId>,
) -> Result<bool> {
    let Some(place) = place else {
        return Ok(false);
    };
    if let Some(flag) = tx
        .query_row(
            "SELECT whitelisted FROM place_subject_policies
             WHERE place_id=?1 AND kind_id=?2 AND subject_id=?3",
            params![place, KIND, subject],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
    {
        return Ok(flag != 0);
    }
    if let Some(source_row) = tx
        .query_row(
            "SELECT source_row FROM place_default_policies
             WHERE place_id=?1 AND kind_id=?2 AND resolution='active'",
            params![place, KIND],
            |row| row.get::<_, Option<Vec<u8>>>(0),
        )
        .optional()?
        .flatten()
    {
        if let Some(flag) = decode_whitelisted(&source_row) {
            return Ok(flag);
        }
    }
    Ok(false)
}

fn owner_external_id(tx: &Transaction<'_>, instance: &GateInstanceId) -> Result<Option<String>> {
    let bytes: Vec<u8> = tx.query_row(
        "SELECT r.config_bytes FROM gate_instances gi
         JOIN gate_instance_revisions r
           ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
         WHERE gi.instance_id=?1",
        params![instance.as_str()],
        |row| row.get(0),
    )?;
    let parsed: Value =
        serde_json::from_slice(&bytes).map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(parsed
        .get("owner_external_id")
        .and_then(Value::as_str)
        .map(str::to_string))
}

fn is_current_dm_principal(
    tx: &Transaction<'_>,
    instance: &GateInstanceId,
    subject: SubjectId,
    external_id: &str,
) -> Result<bool> {
    if owner_external_id(tx, instance)?.as_deref() == Some(external_id) {
        return Ok(true);
    }
    let principal: Option<i64> = tx
        .query_row(
            "SELECT subject_id FROM gate_subject_identities
             WHERE instance_id=?1 AND external_id=?2",
            params![instance.as_str(), external_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(principal) = principal else {
        return Ok(false);
    };
    let granted: i64 = tx.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM grant_sets gs
           JOIN agent_grants ag
             ON ag.grant_set_subject_id=gs.agent_subject_id
            AND ag.grant_set_revision=gs.revision
           WHERE gs.agent_subject_id=?1
             AND gs.revision=(SELECT MAX(revision) FROM grant_sets WHERE agent_subject_id=?1)
             AND ag.principal_subject_id=?2
             AND ag.role IN ('owner','owner_equivalent','trusted')
         )",
        params![subject, principal],
        |row| row.get(0),
    )?;
    Ok(granted != 0)
}

fn heartbeat_enabled(tx: &Transaction<'_>, subject: SubjectId, place: PlaceId) -> Result<bool> {
    let scheduled: i64 = tx.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM schedules
           WHERE owner_subject_id=?1 AND place_id=?2 AND enabled=1
             AND kind IN ('heartbeat','cron','every')
         )",
        params![subject, place],
        |row| row.get(0),
    )?;
    if scheduled != 0 {
        return Ok(true);
    }
    if let Some(flag) = tx
        .query_row(
            "SELECT heartbeat_enabled FROM place_subject_policies
             WHERE place_id=?1 AND kind_id=?2 AND subject_id=?3",
            params![place, KIND, subject],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
    {
        return Ok(flag != 0);
    }
    if let Some(source_row) = tx
        .query_row(
            "SELECT source_row FROM place_default_policies
             WHERE place_id=?1 AND kind_id=?2 AND resolution='active'",
            params![place, KIND],
            |row| row.get::<_, Option<Vec<u8>>>(0),
        )
        .optional()?
        .flatten()
    {
        return Ok(decode_heartbeat_enabled(&source_row).unwrap_or(false));
    }
    Ok(false)
}

fn tool_visible(
    tx: &Transaction<'_>,
    subject: SubjectId,
    place: PlaceId,
    name: &str,
) -> Result<bool> {
    let expanded: i64 = tx.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM expanded_gate_tools
           WHERE place_id=?1 AND subject_id=?2 AND kind_id=?3
         )",
        params![place, subject, KIND],
        |row| row.get(0),
    )?;
    if expanded != 0 {
        return Ok(true);
    }
    let visible: Option<String> = tx
        .query_row(
            "SELECT visibility FROM tool_policy_entries
             WHERE subject_id=?1 AND tool_id=?2
               AND policy_revision=(SELECT MAX(revision) FROM tool_policy_sets WHERE subject_id=?1)",
            params![subject, name],
            |row| row.get(0),
        )
        .optional()?;
    Ok(visible.as_deref() == Some("visible"))
}

fn derive_purposes(
    tx: &Transaction<'_>,
    subject: SubjectId,
    place: PlaceId,
) -> Result<Vec<RoutePurpose>> {
    let mut purposes = vec![RoutePurpose::inbound(), RoutePurpose::outbound()];
    if heartbeat_enabled(tx, subject, place)? {
        purposes.push(RoutePurpose::timed());
    }
    for name in declared_discord_operations() {
        if tool_visible(tx, subject, place, name)? {
            purposes.push(RoutePurpose::tool(name).map_err(|_| rusqlite::Error::InvalidQuery)?);
        }
    }
    Ok(purposes)
}

fn config_contains_subject(
    tx: &Transaction<'_>,
    instance: &str,
    subject: SubjectId,
) -> Result<bool> {
    let (owner, bytes): (Option<i64>, Vec<u8>) = tx.query_row(
        "SELECT gi.owner_subject_id,r.config_bytes
         FROM gate_instances gi
         JOIN gate_instance_revisions r
           ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
         WHERE gi.instance_id=?1 AND r.present=1 AND r.enabled=1",
        params![instance],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if owner == Some(subject) {
        return Ok(true);
    }
    let parsed: Value =
        serde_json::from_slice(&bytes).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let Some(ids) = parsed.get("agent_ids").and_then(Value::as_array) else {
        return Ok(false);
    };
    for id in ids {
        let Some(agent) = id.as_str() else {
            continue;
        };
        let label = dedicated_label(agent);
        let found: Option<i64> = tx
            .query_row(
                "SELECT owner_subject_id FROM gate_instances
                 WHERE kind_id=?1 AND label=?2 AND owner_subject_id=?3",
                params![KIND, label, subject],
                |row| row.get(0),
            )
            .optional()?;
        if found.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn choose_anchor(
    tx: &Transaction<'_>,
    subject: SubjectId,
    place: PlaceId,
) -> std::result::Result<Option<String>, ObserveFail> {
    let mut dedicated = Vec::new();
    let mut shared = Vec::new();
    let mut stmt = tx
        .prepare(
            "SELECT b.binding_id,gi.instance_id,gi.owner_subject_id
             FROM gate_bindings b
             JOIN gate_instances gi ON gi.instance_id=b.instance_id
             JOIN gate_instance_revisions r
               ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
             WHERE b.place_id=?1 AND gi.kind_id=?2 AND r.present=1 AND r.enabled=1
             ORDER BY b.binding_id",
        )
        .map_err(ObserveFail::Store)?;
    let rows = stmt
        .query_map(params![place, KIND], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })
        .map_err(ObserveFail::Store)?;
    for row in rows {
        let (binding, instance, owner) = row.map_err(ObserveFail::Store)?;
        if owner == Some(subject) {
            dedicated.push(binding);
        } else if owner.is_none() && config_contains_subject(tx, &instance, subject)? {
            shared.push(binding);
        }
    }
    if dedicated.len() > 1 || shared.len() > 1 {
        return Err(ObserveFail::Outcome(ObserveGateAddress::BindingAmbiguous));
    }
    Ok(dedicated
        .into_iter()
        .next()
        .or_else(|| shared.into_iter().next()))
}

fn recompute_eligible(
    tx: &Transaction<'_>,
    instance: &GateInstanceId,
    subject: SubjectId,
    place: Option<PlaceId>,
    live_kind: AddressKind,
    stored: Option<StoredKind>,
    author: &str,
) -> Result<bool> {
    if live_kind == AddressKind::Thread {
        return Ok(false);
    }
    if let Some(place) = place {
        let role: Option<String> = tx
            .query_row(
                "SELECT role FROM memberships WHERE place_id=?1 AND subject_id=?2",
                params![place, subject],
                |row| row.get(0),
            )
            .optional()?;
        if role.as_deref() == Some("observer") {
            return Ok(false);
        }
    }
    let effective = match stored {
        Some(StoredKind::Guild) if live_kind != AddressKind::Guild => return Ok(false),
        Some(StoredKind::Dm) if live_kind != AddressKind::Dm => return Ok(false),
        Some(StoredKind::Guild) | None if live_kind == AddressKind::Guild => AddressKind::Guild,
        Some(StoredKind::Dm) | None if live_kind == AddressKind::Dm => AddressKind::Dm,
        Some(StoredKind::Unknown) => live_kind,
        _ => return Ok(false),
    };
    match effective {
        AddressKind::Guild => resolve_whitelisted(tx, subject, place),
        AddressKind::Dm => is_current_dm_principal(tx, instance, subject, author),
        AddressKind::Thread => Ok(false),
    }
}

fn load_binding(
    tx: &Transaction<'_>,
    instance: &GateInstanceId,
    address: &str,
) -> Result<Option<BindingRow>> {
    tx.query_row(
        "SELECT binding_id,place_id,binding_metadata_bytes
         FROM gate_bindings WHERE instance_id=?1 AND address=?2",
        params![instance.as_str(), address],
        |row| {
            Ok(BindingRow {
                binding_id: row.get(0)?,
                place_id: row.get(1)?,
                metadata: row.get(2)?,
            })
        },
    )
    .optional()
}

fn load_source_refs(tx: &Transaction<'_>, address: &str) -> Result<Vec<SourceRefRow>> {
    let mut stmt = tx.prepare(
        "SELECT place_id,classification,metadata FROM place_source_refs
         WHERE source_system=?1 AND source_address=?2",
    )?;
    let rows = stmt.query_map(params![SOURCE_SYSTEM, address], |row| {
        Ok(SourceRefRow {
            place_id: row.get(0)?,
            classification: row.get(1)?,
            metadata: row.get(2)?,
        })
    })?;
    rows.collect()
}

fn inbound_subjects(tx: &Transaction<'_>, place: PlaceId) -> Result<Vec<SubjectId>> {
    let mut stmt = tx.prepare(
        "SELECT DISTINCT subject_id FROM subject_routes
         WHERE place_id=?1 AND kind_id=?2 AND purpose='inbound' ORDER BY subject_id",
    )?;
    let rows = stmt.query_map(params![place, KIND], |row| row.get(0))?;
    rows.collect()
}

fn ensure_membership(
    tx: &Transaction<'_>,
    place: PlaceId,
    subject: SubjectId,
    observed_at: i64,
    latest: i64,
) -> Result<()> {
    let existing: Option<String> = tx
        .query_row(
            "SELECT role FROM memberships WHERE place_id=?1 AND subject_id=?2",
            params![place, subject],
            |row| row.get(0),
        )
        .optional()?;
    match existing.as_deref() {
        None => {
            tx.execute(
                "INSERT INTO memberships(place_id,subject_id,role,shared_seen_seq,joined_at)
                 VALUES(?1,?2,'participant',?3,?4)",
                params![place, subject, latest, observed_at],
            )?;
        }
        Some("observer") => {
            tx.execute(
                "UPDATE memberships SET role='participant' WHERE place_id=?1 AND subject_id=?2",
                params![place, subject],
            )?;
        }
        Some("participant") => {}
        _ => return Err(rusqlite::Error::InvalidQuery),
    }
    Ok(())
}

fn merge_dm_metadata(
    existing: Option<&str>,
    author: &str,
) -> std::result::Result<String, ObserveFail> {
    match existing {
        None => Ok(serde_json::to_string(&json!({"dm_user_id": author})).expect("object")),
        Some(raw) => {
            let mut value: Value = serde_json::from_str(raw).map_err(|_| {
                ObserveFail::Outcome(ObserveGateAddress::SourceRefMetadataShapeConflict)
            })?;
            if !value.is_object() {
                return Err(ObserveFail::Outcome(
                    ObserveGateAddress::SourceRefMetadataShapeConflict,
                ));
            }
            value
                .as_object_mut()
                .expect("object")
                .insert("dm_user_id".into(), Value::String(author.into()));
            Ok(serde_json::to_string(&value).expect("object"))
        }
    }
}

fn observe_on(
    tx: &Transaction<'_>,
    req: &ObserveRequest<'_>,
) -> std::result::Result<ObserveGateAddress, ObserveFail> {
    let instance = req.instance;
    let address = req.address;
    let address_kind = req.address_kind;
    let author = req.author_external_id;
    let guild_id = req.guild_id;
    let label = &req.label;
    let observed_at = req.observed_at;
    if address_kind == AddressKind::Guild && guild_id.map(str::is_empty).unwrap_or(true) {
        return Err(ObserveFail::Store(rusqlite::Error::InvalidQuery));
    }
    if address_kind != AddressKind::Guild && guild_id.is_some() {
        return Err(ObserveFail::Store(rusqlite::Error::InvalidQuery));
    }
    let participants = resolve_participants(tx, instance)?;
    let binding = load_binding(tx, instance, address).map_err(ObserveFail::Store)?;
    let refs = load_source_refs(tx, address).map_err(ObserveFail::Store)?;
    if refs.len() > 1 {
        return Ok(ObserveGateAddress::BindingAmbiguous);
    }
    if address_kind == AddressKind::Thread {
        return Ok(ObserveGateAddress::Rejected);
    }
    let known_place = match (&binding, refs.first()) {
        (Some(binding), Some(source)) if binding.place_id != source.place_id => {
            return Ok(ObserveGateAddress::BindingMetadataConflict);
        }
        (Some(binding), _) => Some(binding.place_id),
        (None, Some(source)) => Some(source.place_id),
        (None, None) => None,
    };
    let stored = binding.as_ref().and_then(|row| stored_kind(&row.metadata));
    if let (Some(stored), Some(row)) = (stored, binding.as_ref()) {
        match (stored, address_kind) {
            (StoredKind::Guild, AddressKind::Dm) | (StoredKind::Dm, AddressKind::Guild) => {
                return Ok(ObserveGateAddress::BindingMetadataConflict);
            }
            (StoredKind::Guild, AddressKind::Guild) => {
                if let Some(Some(saved)) = stored_guild_id(&row.metadata) {
                    if guild_id != Some(saved.as_str()) {
                        return Ok(ObserveGateAddress::BindingMetadataConflict);
                    }
                }
            }
            _ => {}
        }
    }
    let mut candidates = participants.clone();
    if let Some(place) = known_place {
        for subject in inbound_subjects(tx, place).map_err(ObserveFail::Store)? {
            if !candidates.contains(&subject) {
                candidates.push(subject);
            }
        }
    }
    let mut eligible = Vec::new();
    for subject in candidates {
        if recompute_eligible(
            tx,
            instance,
            subject,
            known_place,
            address_kind,
            stored,
            author,
        )
        .map_err(ObserveFail::Store)?
        {
            eligible.push(subject);
        }
    }
    if eligible.is_empty() {
        return Ok(ObserveGateAddress::Rejected);
    }
    let latest: i64 = match known_place {
        Some(place) => tx
            .query_row(
                "SELECT COALESCE(MAX(seq),0) FROM events WHERE place_id=?1",
                params![place],
                |row| row.get(0),
            )
            .map_err(ObserveFail::Store)?,
        None => 0,
    };
    let place = if let Some(place) = known_place {
        if let Some(source) = refs.first() {
            if source.classification == "config_only" {
                tx.execute(
                    "UPDATE place_source_refs SET classification='live'
                     WHERE source_system=?1 AND source_address=?2",
                    params![SOURCE_SYSTEM, address],
                )
                .map_err(ObserveFail::Store)?;
            } else if source.classification != "live" && source.classification != "config_only" {
                // other classification: classification write 0
            }
            if address_kind == AddressKind::Dm {
                let metadata = merge_dm_metadata(source.metadata.as_deref(), author)?;
                tx.execute(
                    "UPDATE place_source_refs SET metadata=?1
                     WHERE source_system=?2 AND source_address=?3",
                    params![metadata, SOURCE_SYSTEM, address],
                )
                .map_err(ObserveFail::Store)?;
            }
        }
        place
    } else {
        tx.execute(
            "INSERT INTO places(address,parent_id,policy_json,inherit_from_place,inherit_up_to_seq,created_at)
             VALUES(?1,NULL,'{}',NULL,NULL,?2)",
            params![address, observed_at],
        )
        .map_err(ObserveFail::Store)?;
        let place = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO place_source_refs(
               source_system,source_address,place_id,source_id,classification,updated_at
             ) VALUES(?1,?2,?3,?4,'live',?5)",
            params![
                SOURCE_SYSTEM,
                address,
                place,
                address.as_bytes(),
                observed_at
            ],
        )
        .map_err(ObserveFail::Store)?;
        if address_kind == AddressKind::Dm {
            let metadata = merge_dm_metadata(None, author)?;
            tx.execute(
                "UPDATE place_source_refs SET metadata=?1
                 WHERE source_system=?2 AND source_address=?3",
                params![metadata, SOURCE_SYSTEM, address],
            )
            .map_err(ObserveFail::Store)?;
        }
        place
    };
    let kind = discord_kind().map_err(ObserveFail::Store)?;
    let scope_id = match tx
        .query_row(
            "SELECT scope_id FROM external_origin_scopes
             WHERE kind_id=?1 AND address=?2 AND mode='kind_address'",
            params![KIND, address],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(ObserveFail::Store)?
    {
        Some(id) => id,
        None => {
            let id = runtime_uuid_v7(observed_at, &format!("scope\0{KIND}\0{address}"));
            tx.execute(
                "INSERT INTO external_origin_scopes(scope_id,kind_id,address,mode,instance_id,place_id)
                 VALUES(?1,?2,?3,'kind_address',NULL,?4)",
                params![id, KIND, address, place],
            )
            .map_err(ObserveFail::Store)?;
            id
        }
    };
    let binding_id = if let Some(existing) = binding {
        if existing.place_id != place {
            return Ok(ObserveGateAddress::BindingMetadataConflict);
        }
        if stored == Some(StoredKind::Unknown)
            && matches!(address_kind, AddressKind::Guild | AddressKind::Dm)
        {
            let meta = metadata_bytes(address_kind, guild_id);
            let digest = sha256(&meta);
            tx.execute(
                "UPDATE gate_bindings SET binding_metadata_bytes=?1,binding_metadata_digest=?2
                 WHERE binding_id=?3",
                params![meta, digest, existing.binding_id],
            )
            .map_err(ObserveFail::Store)?;
        }
        if let LabelUpdate::Present(label) = label {
            tx.execute(
                "UPDATE gate_bindings SET label=?1 WHERE binding_id=?2",
                params![label, existing.binding_id],
            )
            .map_err(ObserveFail::Store)?;
        }
        existing.binding_id
    } else {
        let id = runtime_uuid_v7(
            observed_at,
            &format!("binding\0{}\0{address}", instance.as_str()),
        );
        let meta = metadata_bytes(address_kind, guild_id);
        let digest = sha256(&meta);
        let label_value = match label {
            LabelUpdate::Present(value) => Some(value.as_str()),
            LabelUpdate::Absent => None,
        };
        tx.execute(
            "INSERT INTO gate_bindings(
               binding_id,place_id,instance_id,address,label,origin_scope_id,
               binding_metadata_schema_id,binding_metadata_bytes,binding_metadata_digest
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                id,
                place,
                instance.as_str(),
                address,
                label_value,
                scope_id,
                BINDING_SCHEMA,
                meta,
                digest
            ],
        )
        .map_err(ObserveFail::from)?;
        id
    };
    for subject in &eligible {
        ensure_membership(tx, place, *subject, observed_at, latest).map_err(ObserveFail::Store)?;
    }
    let mut fanout = Vec::new();
    let persist_author = persisted_dm_author(tx, address, address_kind)?;
    for subject in eligible
        .iter()
        .filter(|subject| participants.contains(subject))
    {
        if !recompute_eligible(
            tx,
            instance,
            *subject,
            Some(place),
            address_kind,
            None,
            &persist_author,
        )
        .map_err(ObserveFail::Store)?
        {
            reconcile_subject_routes_on(tx, *subject, place, &kind, None, &[])
                .map_err(ObserveFail::Store)?;
            continue;
        }
        let purposes = derive_purposes(tx, *subject, place).map_err(ObserveFail::Store)?;
        let anchor = choose_anchor(tx, *subject, place)?;
        reconcile_subject_routes_on(tx, *subject, place, &kind, anchor.as_deref(), &purposes)
            .map_err(ObserveFail::Store)?;
    }
    for subject in inbound_subjects(tx, place).map_err(ObserveFail::Store)? {
        if eligible.contains(&subject) {
            fanout.push(subject);
        }
    }
    Ok(ObserveGateAddress::Ready {
        place,
        binding_id,
        admitted_fanout_subjects: fanout,
    })
}

fn persisted_dm_author(
    tx: &Transaction<'_>,
    address: &str,
    address_kind: AddressKind,
) -> std::result::Result<String, ObserveFail> {
    if address_kind != AddressKind::Dm {
        return Ok(String::new());
    }
    let metadata: Option<String> = tx
        .query_row(
            "SELECT metadata FROM place_source_refs
             WHERE source_system=?1 AND source_address=?2",
            params![SOURCE_SYSTEM, address],
            |row| row.get(0),
        )
        .optional()
        .map_err(ObserveFail::Store)?;
    let Some(raw) = metadata else {
        return Err(ObserveFail::Store(rusqlite::Error::InvalidQuery));
    };
    let value: Value = serde_json::from_str(&raw)
        .map_err(|_| ObserveFail::Outcome(ObserveGateAddress::SourceRefMetadataShapeConflict))?;
    match value.get("dm_user_id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => Ok(id.to_string()),
        _ => Err(ObserveFail::Store(rusqlite::Error::InvalidQuery)),
    }
}

impl Store {
    pub fn observe_gate_address(&self, req: ObserveRequest<'_>) -> Result<ObserveGateAddress> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        match observe_on(&tx, &req) {
            Ok(outcome @ ObserveGateAddress::Ready { .. }) => {
                tx.commit()?;
                Ok(outcome)
            }
            Ok(outcome) => Ok(outcome),
            Err(ObserveFail::Outcome(outcome)) => Ok(outcome),
            Err(ObserveFail::Store(error)) if is_unique(&error) => {
                drop(tx);
                reread_equivalent(&conn, req.instance, req.address)
            }
            Err(ObserveFail::Store(error)) => Err(error),
        }
    }
}

fn reread_equivalent(
    conn: &Connection,
    instance: &GateInstanceId,
    address: &str,
) -> Result<ObserveGateAddress> {
    let row = conn
        .query_row(
            "SELECT binding_id,place_id FROM gate_bindings WHERE instance_id=?1 AND address=?2",
            params![instance.as_str(), address],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let Some((binding_id, place)) = row else {
        return Ok(ObserveGateAddress::BindingAmbiguous);
    };
    let admitted = inbound_subjects_on(conn, place)?;
    Ok(ObserveGateAddress::ConcurrentEquivalent {
        place,
        binding_id,
        admitted_fanout_subjects: admitted,
    })
}

fn inbound_subjects_on(conn: &Connection, place: PlaceId) -> Result<Vec<SubjectId>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT subject_id FROM subject_routes
         WHERE place_id=?1 AND kind_id=?2 AND purpose='inbound' ORDER BY subject_id",
    )?;
    let rows = stmt.query_map(params![place, KIND], |row| row.get(0))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use opencrab_port::{IngressDiscovery, OriginScope, Standing, SubjectKind};

    fn encode_row(whitelisted: i64) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&11u64.to_be_bytes());
        for index in 0..11 {
            if matches!(index, 4..=7) {
                let value = if index == 6 { whitelisted } else { 1 };
                out.push(1);
                out.extend_from_slice(&value.to_be_bytes());
            } else if index == 8 {
                out.push(0);
            } else {
                out.push(3);
                out.extend_from_slice(&0u64.to_be_bytes());
            }
        }
        out
    }

    fn fixture() -> (Store, GateInstanceId, SubjectId) {
        let store = Store::new_in_memory().unwrap();
        store.upsert_discord_kind().unwrap();
        let subject = store
            .create_subject(
                SubjectKind::Agent,
                "owner",
                "p",
                "echo",
                Standing::Trusted,
                1,
            )
            .unwrap();
        let instance =
            GateInstanceId::parse("018f0000-0000-7000-8000-000000000041".to_string()).unwrap();
        store
            .install_gate_instance_revision(
                &instance,
                &discord_kind().unwrap(),
                "dedicated:discord:Zg",
                Some(subject),
                1,
                true,
                OriginScope::KindAddress,
                IngressDiscovery::Membership,
                CONFIG_SCHEMA,
                br#"{"agent_ids":[],"owner_external_id":"owner-1","self_external_id":"bot-1"}"#,
                1,
            )
            .unwrap();
        (store, instance, subject)
    }

    #[allow(clippy::too_many_arguments)]
    fn observe(
        store: &Store,
        instance: &GateInstanceId,
        address: &str,
        address_kind: AddressKind,
        author: &str,
        guild_id: Option<&str>,
        label: LabelUpdate,
        observed_at: i64,
    ) -> ObserveGateAddress {
        store
            .observe_gate_address(ObserveRequest {
                instance,
                address,
                address_kind,
                author_external_id: author,
                guild_id,
                label,
                observed_at,
            })
            .unwrap()
    }

    fn writes(store: &Store) -> (i64, i64, i64, i64) {
        let conn = store.c();
        let places: i64 = conn
            .query_row("SELECT COUNT(*) FROM places", [], |r| r.get(0))
            .unwrap();
        let refs: i64 = conn
            .query_row("SELECT COUNT(*) FROM place_source_refs", [], |r| r.get(0))
            .unwrap();
        let bindings: i64 = conn
            .query_row("SELECT COUNT(*) FROM gate_bindings", [], |r| r.get(0))
            .unwrap();
        let routes: i64 = conn
            .query_row("SELECT COUNT(*) FROM subject_routes", [], |r| r.get(0))
            .unwrap();
        (places, refs, bindings, routes)
    }

    #[test]
    fn unobserved_guild_hard_default_rejects_without_writes() {
        let (store, instance, _) = fixture();
        let before = writes(&store);
        let outcome = observe(
            &store,
            &instance,
            "100",
            AddressKind::Guild,
            "user-1",
            Some("200"),
            LabelUpdate::Absent,
            10,
        );
        assert_eq!(outcome, ObserveGateAddress::Rejected);
        assert_eq!(writes(&store), before);
    }

    #[test]
    fn subject_policy_whitelist_opens_unobserved_config_place() {
        let (store, instance, subject) = fixture();
        let place = store
            .create_place(Some("100"), None, "{}", None, 1)
            .unwrap();
        store
            .c()
            .execute(
                "INSERT INTO place_source_refs(source_system,source_address,place_id,source_id,classification,updated_at)
                 VALUES('discord','100',?1,x'313030','config_only',1)",
                params![place],
            )
            .unwrap();
        store
            .c()
            .execute(
                "INSERT INTO place_subject_policies(
                   place_id,kind_id,subject_id,admission,readable,writable,whitelisted,
                   heartbeat_enabled,heartbeat_interval_secs,heartbeat_instructions,
                   instructions_revision,source_row,source_updated_at
                 ) VALUES(?1,'discord',?2,'open',1,1,1,0,NULL,'',0,x'00',1)",
                params![place, subject],
            )
            .unwrap();
        let outcome = observe(
            &store,
            &instance,
            "100",
            AddressKind::Guild,
            "user-1",
            Some("200"),
            LabelUpdate::Present("general".into()),
            10,
        );
        match outcome {
            ObserveGateAddress::Ready {
                place: got,
                admitted_fanout_subjects,
                ..
            } => {
                assert_eq!(got, place);
                assert_eq!(admitted_fanout_subjects, vec![subject]);
            }
            other => panic!("{other:?}"),
        }
        let class: String = store
            .c()
            .query_row(
                "SELECT classification FROM place_source_refs WHERE source_address='100'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(class, "live");
        let routes: i64 = store
            .c()
            .query_row(
                "SELECT COUNT(*) FROM subject_routes WHERE purpose IN ('inbound','outbound')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(routes, 2);
    }

    #[test]
    fn default_source_row_whitelist_used_when_subject_policy_absent() {
        let (store, instance, subject) = fixture();
        let place = store
            .create_place(Some("100"), None, "{}", None, 1)
            .unwrap();
        store
            .c()
            .execute(
                "INSERT INTO place_source_refs(source_system,source_address,place_id,source_id,classification,updated_at)
                 VALUES('discord','100',?1,x'313030','config_only',1)",
                params![place],
            )
            .unwrap();
        store
            .c()
            .execute(
                "INSERT INTO place_default_policies(default_id,place_id,kind_id,resolution,source_row,source_updated_at)
                 VALUES('def-1',?1,'discord','active',?2,1)",
                params![place, encode_row(1)],
            )
            .unwrap();
        let outcome = observe(
            &store,
            &instance,
            "100",
            AddressKind::Guild,
            "user-1",
            Some("200"),
            LabelUpdate::Absent,
            10,
        );
        assert!(matches!(outcome, ObserveGateAddress::Ready { .. }));
        let _ = subject;
    }

    #[test]
    fn subject_policy_false_wins_over_default_true() {
        let (store, instance, subject) = fixture();
        let place = store
            .create_place(Some("100"), None, "{}", None, 1)
            .unwrap();
        store
            .c()
            .execute(
                "INSERT INTO place_source_refs(source_system,source_address,place_id,source_id,classification,updated_at)
                 VALUES('discord','100',?1,x'313030','live',1)",
                params![place],
            )
            .unwrap();
        store
            .c()
            .execute(
                "INSERT INTO place_default_policies(default_id,place_id,kind_id,resolution,source_row,source_updated_at)
                 VALUES('def-1',?1,'discord','active',?2,1)",
                params![place, encode_row(1)],
            )
            .unwrap();
        store
            .c()
            .execute(
                "INSERT INTO place_subject_policies(
                   place_id,kind_id,subject_id,admission,readable,writable,whitelisted,
                   heartbeat_enabled,heartbeat_interval_secs,heartbeat_instructions,
                   instructions_revision,source_row,source_updated_at
                 ) VALUES(?1,'discord',?2,'closed',1,1,0,0,NULL,'',0,x'00',1)",
                params![place, subject],
            )
            .unwrap();
        let before = writes(&store);
        let outcome = observe(
            &store,
            &instance,
            "100",
            AddressKind::Guild,
            "user-1",
            Some("200"),
            LabelUpdate::Absent,
            10,
        );
        assert_eq!(outcome, ObserveGateAddress::Rejected);
        assert_eq!(writes(&store), before);
    }

    #[test]
    fn owner_dm_is_ready_and_thread_is_rejected() {
        let (store, instance, subject) = fixture();
        let dm = observe(
            &store,
            &instance,
            "300",
            AddressKind::Dm,
            "owner-1",
            None,
            LabelUpdate::Absent,
            10,
        );
        assert!(matches!(dm, ObserveGateAddress::Ready { .. }));
        let before = writes(&store);
        let thread = observe(
            &store,
            &instance,
            "400",
            AddressKind::Thread,
            "owner-1",
            None,
            LabelUpdate::Absent,
            11,
        );
        assert_eq!(thread, ObserveGateAddress::Rejected);
        assert_eq!(writes(&store), before);
        let _ = subject;
    }
}
