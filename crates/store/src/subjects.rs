//! Agent subject 書きコマンド（DESIGN-DASHBOARD-P2 SLICE 1）。
//!
//! HTTP は持たない。判断と SQL はここ。旧 `agents` は書かない。
//! persona 合成は本体 `process.rs:241-257` の byte-shape。
//! DELETE は現行 5 表相当だけ（DESIGN-RULINGS）。154 辺グラフは作らない。

use crate::{sha256, Store};
use opencrab_port::SubjectId;
use rusqlite::{params, OptionalExtension, Transaction};

const TURN_RUNNER_ENGINE: &str = "engine";
const UNSET_HISTORY_POLICY: &str = r#"{"budget_tokens":null}"#;
const UNSET_OUTPUT_POLICY: &str = r#"{"max_output_tokens":null}"#;

#[derive(Debug)]
pub enum SubjectCommandError {
    Store(rusqlite::Error),
    EmptyIdentity,
}

impl From<rusqlite::Error> for SubjectCommandError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

impl std::fmt::Display for SubjectCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "{error}"),
            Self::EmptyIdentity => write!(f, "name and persona_name must be non-empty"),
        }
    }
}

/// PUT の適用欄。`personality` / `model` の None はクリア。instructions の空はクリア。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubjectReplace {
    pub name: String,
    pub persona_name: String,
    pub personality: Option<String>,
    pub instructions: String,
    pub model: Option<String>,
}

/// PATCH 省略 / JSON null = keep。`Some("")` はクリア（本体 serde が null を未提供に潰す）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubjectPatch {
    pub name: Option<String>,
    pub persona_name: Option<String>,
    pub personality: Option<String>,
    pub instructions: Option<String>,
    pub model: Option<String>,
}

/// GET /api/agents/{id} が読む適用済み欄。未復元欄は handler が null のまま出す。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubjectDashboardView {
    pub id: SubjectId,
    pub name: String,
    pub persona_name: Option<String>,
    pub personality: Option<String>,
    pub instructions: String,
    pub model: Option<String>,
}

/// 本体 `process.rs:241-257`: `You are {name} ({persona_name}).` + 非空 personality + 非空 Instructions。
pub fn compose_subject_persona(
    name: &str,
    persona_name: &str,
    personality: Option<&str>,
    instructions: &str,
) -> String {
    let mut persona = format!("You are {name} ({persona_name}).");
    if let Some(traits) = personality.filter(|value| !value.is_empty()) {
        persona.push_str("\n\n");
        persona.push_str(traits);
    }
    if !instructions.is_empty() {
        persona.push_str("\n\n## Instructions\n");
        persona.push_str(instructions);
    }
    persona
}

fn require_identity(
    name: &str,
    persona_name: &str,
) -> std::result::Result<(), SubjectCommandError> {
    if name.is_empty() || persona_name.is_empty() {
        return Err(SubjectCommandError::EmptyIdentity);
    }
    Ok(())
}

struct ProfileState {
    persona_name: String,
    personality: Option<String>,
    instructions: String,
}

struct RuntimeState {
    model_alias: Option<String>,
}

fn read_profile(tx: &Transaction<'_>, id: SubjectId) -> crate::Result<Option<ProfileState>> {
    tx.query_row(
        "SELECT persona_name,persona,instructions FROM subject_profiles
         WHERE subject_id=?1 AND revision=1",
        params![id],
        |row| {
            Ok(ProfileState {
                persona_name: row.get(0)?,
                personality: row.get(1)?,
                instructions: row.get(2)?,
            })
        },
    )
    .optional()
}

fn read_runtime(tx: &Transaction<'_>, id: SubjectId) -> crate::Result<Option<RuntimeState>> {
    tx.query_row(
        "SELECT model_alias FROM subject_runtime_configs
         WHERE subject_id=?1 AND revision=1",
        params![id],
        |row| {
            Ok(RuntimeState {
                model_alias: row.get(0)?,
            })
        },
    )
    .optional()
}

fn read_name(tx: &Transaction<'_>, id: SubjectId) -> crate::Result<Option<String>> {
    tx.query_row(
        "SELECT name FROM subjects WHERE id=?1",
        params![id],
        |row| row.get(0),
    )
    .optional()
}

fn upsert_profile(
    tx: &Transaction<'_>,
    id: SubjectId,
    persona_name: &str,
    personality: Option<&str>,
    instructions: &str,
    now: i64,
) -> crate::Result<()> {
    tx.execute(
        "INSERT INTO subject_profiles(
           subject_id,revision,persona_name,persona,instructions,
           default_heartbeat_instructions,job_title,organization,image_url,metadata,updated_at
         ) VALUES(?1,1,?2,?3,?4,'',NULL,NULL,NULL,NULL,?5)
         ON CONFLICT(subject_id,revision) DO UPDATE SET
           persona_name=excluded.persona_name,
           persona=excluded.persona,
           instructions=excluded.instructions,
           updated_at=excluded.updated_at",
        params![id, persona_name, personality, instructions, now],
    )?;
    Ok(())
}

fn upsert_runtime(
    tx: &Transaction<'_>,
    id: SubjectId,
    model_alias: Option<&str>,
    now: i64,
) -> crate::Result<()> {
    tx.execute(
        "INSERT INTO subject_runtime_configs(
           subject_id,revision,created_at,model_alias,reasoning_effort,web_search_enabled,
           history_policy,output_policy,model_route_id,source_config
         ) VALUES(?1,1,?2,?3,NULL,NULL,?4,?5,NULL,NULL)
         ON CONFLICT(subject_id,revision) DO UPDATE SET
           model_alias=excluded.model_alias",
        params![
            id,
            now,
            model_alias,
            UNSET_HISTORY_POLICY,
            UNSET_OUTPUT_POLICY
        ],
    )?;
    Ok(())
}

fn turn_runner_for(model: Option<&str>) -> &str {
    match model {
        Some(value) if !value.is_empty() => value,
        _ => TURN_RUNNER_ENGINE,
    }
}

fn apply_persona_row(
    tx: &Transaction<'_>,
    id: SubjectId,
    name: &str,
    persona_name: &str,
    personality: Option<&str>,
    instructions: &str,
) -> crate::Result<()> {
    let persona = compose_subject_persona(name, persona_name, personality, instructions);
    tx.execute(
        "UPDATE subjects SET name=?2,persona=?3 WHERE id=?1",
        params![id, name, persona],
    )?;
    Ok(())
}

fn apply_model_row(tx: &Transaction<'_>, id: SubjectId, model: Option<&str>) -> crate::Result<()> {
    tx.execute(
        "UPDATE subjects SET turn_runner=?2 WHERE id=?1",
        params![id, turn_runner_for(model)],
    )?;
    Ok(())
}

fn tombstone_owned_discord(tx: &Transaction<'_>, id: SubjectId, now: i64) -> crate::Result<()> {
    // body `delete_agent` は `agent_discord_config` だけを消す。nostr instance は止めない。
    let mut stmt = tx.prepare(
        "SELECT instance_id FROM gate_instances
         WHERE owner_subject_id=?1 AND kind_id='discord' ORDER BY instance_id",
    )?;
    let instances = stmt
        .query_map(params![id], |row| row.get::<_, String>(0))?
        .collect::<crate::Result<Vec<String>>>()?;
    drop(stmt);
    for instance in instances {
        let current = tx
            .query_row(
                "SELECT r.revision,r.present,r.config_schema_id,r.config_bytes,r.secret_set_id
                 FROM gate_instances gi
                 JOIN gate_instance_revisions r
                   ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
                 WHERE gi.instance_id=?1",
                params![instance],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((revision, present, schema, bytes, secret_set)) = current else {
            continue;
        };
        if present == 0 {
            continue;
        }
        let next = revision + 1;
        tx.execute(
            "INSERT INTO gate_instance_revisions(
               instance_id,revision,present,enabled,config_schema_id,config_bytes,
               config_digest,secret_set_id,created_at
             ) VALUES(?1,?2,0,0,?3,?4,?5,?6,?7)",
            params![
                instance,
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
            params![next, instance],
        )?;
        tx.execute(
            "UPDATE gate_connections SET state='closed',disconnected_at=?2
             WHERE instance_id=?1 AND state='active'",
            params![instance, now],
        )?;
    }
    Ok(())
}

impl Store {
    pub fn subject_create(
        &self,
        id: Option<SubjectId>,
        name: &str,
        persona_name: &str,
        now: i64,
    ) -> std::result::Result<SubjectId, SubjectCommandError> {
        require_identity(name, persona_name)?;
        let persona = compose_subject_persona(name, persona_name, None, "");
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        match id {
            Some(explicit) => {
                tx.execute(
                    "INSERT INTO subjects(id,kind,name,persona,turn_runner,standing,created_at)
                     VALUES(?1,'agent',?2,?3,?4,'trusted',?5)",
                    params![explicit, name, persona, TURN_RUNNER_ENGINE, now],
                )?;
            }
            None => {
                tx.execute(
                    "INSERT INTO subjects(kind,name,persona,turn_runner,standing,created_at)
                     VALUES('agent',?1,?2,?3,'trusted',?4)",
                    params![name, persona, TURN_RUNNER_ENGINE, now],
                )?;
            }
        }
        let subject = id.unwrap_or_else(|| tx.last_insert_rowid());
        upsert_profile(&tx, subject, persona_name, None, "", now)?;
        upsert_runtime(&tx, subject, None, now)?;
        tx.commit()?;
        Ok(subject)
    }

    pub fn subject_replace(
        &self,
        id: SubjectId,
        replace: &SubjectReplace,
        now: i64,
    ) -> std::result::Result<bool, SubjectCommandError> {
        require_identity(&replace.name, &replace.persona_name)?;
        let personality = replace.personality.as_deref();
        let model = replace.model.as_deref();
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if read_name(&tx, id)?.is_none() {
            return Ok(false);
        }
        apply_persona_row(
            &tx,
            id,
            &replace.name,
            &replace.persona_name,
            personality,
            &replace.instructions,
        )?;
        apply_model_row(&tx, id, model)?;
        upsert_profile(
            &tx,
            id,
            &replace.persona_name,
            personality,
            &replace.instructions,
            now,
        )?;
        upsert_runtime(&tx, id, model.filter(|value| !value.is_empty()), now)?;
        tx.commit()?;
        Ok(true)
    }

    pub fn subject_patch(
        &self,
        id: SubjectId,
        patch: &SubjectPatch,
        now: i64,
    ) -> std::result::Result<bool, SubjectCommandError> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some(current_name) = read_name(&tx, id)? else {
            return Ok(false);
        };
        let profile = read_profile(&tx, id)?.unwrap_or(ProfileState {
            persona_name: String::new(),
            personality: None,
            instructions: String::new(),
        });
        let runtime = read_runtime(&tx, id)?.unwrap_or(RuntimeState { model_alias: None });
        let name = patch.name.as_deref().unwrap_or(&current_name);
        let persona_name = patch
            .persona_name
            .as_deref()
            .unwrap_or(&profile.persona_name);
        require_identity(name, persona_name)?;
        let personality = match patch.personality.as_deref() {
            Some("") => None,
            Some(value) => Some(value),
            None => profile.personality.as_deref(),
        };
        let instructions = patch
            .instructions
            .as_deref()
            .unwrap_or(&profile.instructions);
        let model = match patch.model.as_deref() {
            Some("") => None,
            Some(value) => Some(value),
            None => runtime.model_alias.as_deref(),
        };
        apply_persona_row(&tx, id, name, persona_name, personality, instructions)?;
        apply_model_row(&tx, id, model)?;
        upsert_profile(&tx, id, persona_name, personality, instructions, now)?;
        upsert_runtime(&tx, id, model, now)?;
        tx.commit()?;
        Ok(true)
    }

    pub fn subject_delete(
        &self,
        id: SubjectId,
        now: i64,
    ) -> std::result::Result<bool, SubjectCommandError> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // body `delete_agent` は agents 行の有無にかかわらず ancillary 4 family を続ける。
        let deleted = tx.execute("DELETE FROM subjects WHERE id=?1", params![id])?;
        tx.execute(
            "DELETE FROM soul_presets WHERE agent_id=?1",
            params![id.to_string()],
        )?;
        tx.execute("DELETE FROM skills WHERE owner_subject_id=?1", params![id])?;
        tx.execute("DELETE FROM memories WHERE subject_id=?1", params![id])?;
        tombstone_owned_discord(&tx, id, now)?;
        tx.commit()?;
        Ok(deleted > 0)
    }

    pub fn subject_dashboard_view(
        &self,
        id: SubjectId,
    ) -> std::result::Result<Option<SubjectDashboardView>, SubjectCommandError> {
        let conn = self.c();
        let Some(name) = conn
            .query_row(
                "SELECT name FROM subjects WHERE id=?1",
                params![id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let profile = conn
            .query_row(
                "SELECT persona_name,persona,instructions FROM subject_profiles
                 WHERE subject_id=?1 AND revision=1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let model = conn
            .query_row(
                "SELECT model_alias FROM subject_runtime_configs
                 WHERE subject_id=?1 AND revision=1",
                params![id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let (persona_name, personality, instructions) = match profile {
            Some((persona_name, personality, instructions)) => {
                (Some(persona_name), personality, instructions)
            }
            None => (None, None, String::new()),
        };
        Ok(Some(SubjectDashboardView {
            id,
            name,
            persona_name,
            personality,
            instructions,
            model,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_port::{GateInstanceId, GateKindId, IngressDiscovery, OriginScope};
    use serde_json::json;

    fn store() -> Store {
        let store = Store::new_in_memory().unwrap();
        store
            .c()
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS subject_profiles(
                  subject_id INTEGER NOT NULL,
                  revision INTEGER NOT NULL,
                  persona_name TEXT NOT NULL,
                  persona TEXT,
                  instructions TEXT NOT NULL,
                  default_heartbeat_instructions TEXT NOT NULL,
                  job_title TEXT,
                  organization TEXT,
                  image_url TEXT,
                  metadata TEXT,
                  updated_at INTEGER NOT NULL,
                  PRIMARY KEY(subject_id,revision)
                );
                CREATE TABLE IF NOT EXISTS subject_runtime_configs(
                  subject_id INTEGER NOT NULL,
                  revision INTEGER NOT NULL,
                  created_at INTEGER NOT NULL,
                  model_alias TEXT,
                  reasoning_effort TEXT,
                  web_search_enabled INTEGER,
                  history_policy TEXT NOT NULL,
                  output_policy TEXT NOT NULL,
                  model_route_id TEXT,
                  source_config BLOB,
                  PRIMARY KEY(subject_id,revision)
                );
                CREATE TABLE IF NOT EXISTS soul_presets(
                  id TEXT PRIMARY KEY,
                  agent_id TEXT NOT NULL,
                  preset_name TEXT NOT NULL,
                  persona_name TEXT NOT NULL,
                  custom_traits_json TEXT,
                  created_at TEXT NOT NULL,
                  updated_at TEXT NOT NULL
                );
                "#,
            )
            .unwrap();
        store
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.c().query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn count_bound(store: &Store, sql: &str, id: SubjectId) -> i64 {
        store
            .c()
            .query_row(sql, params![id], |row| row.get(0))
            .unwrap()
    }

    fn insert_skill(store: &Store, owner: SubjectId, skill_id: &str) {
        store
            .c()
            .execute(
                "INSERT INTO skills(
                   skill_id,owner_subject_id,name,description,situation_pattern,guidance,
                   permission,visible_to_agent,active,archived,state,definition,source_type,
                   revision,usage_count,created_at,updated_at
                 ) VALUES(?1,?2,'n','d','s','g','agent',1,1,0,'active',x'7b7d','standard',1,0,1,1)",
                params![skill_id, owner],
            )
            .unwrap();
    }

    #[test]
    fn compose_subject_persona_byte_shape() {
        assert_eq!(
            compose_subject_persona("Ada", "Helper", None, ""),
            "You are Ada (Helper)."
        );
        assert_eq!(
            compose_subject_persona("Ada", "Helper", Some(""), ""),
            "You are Ada (Helper)."
        );
        assert_eq!(
            compose_subject_persona("Ada", "Helper", Some("curious"), ""),
            "You are Ada (Helper).\n\ncurious"
        );
        assert_eq!(
            compose_subject_persona("Ada", "Helper", None, "be brief"),
            "You are Ada (Helper).\n\n## Instructions\nbe brief"
        );
        assert_eq!(
            compose_subject_persona("Ada", "Helper", Some("curious"), "be brief"),
            "You are Ada (Helper).\n\ncurious\n\n## Instructions\nbe brief"
        );
    }

    #[test]
    fn skills_schema_has_owner_subject_id() {
        let store = Store::new_in_memory().unwrap();
        let sql: String = store
            .c()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='skills'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sql.contains("owner_subject_id"));
        assert!(sql.contains("skill_id TEXT NOT NULL PRIMARY KEY"));
        assert!(sql.contains("state TEXT NOT NULL CHECK(state IN ('active','retired','archived'))"));
    }

    #[test]
    fn subject_create_writes_core_and_composed_persona() {
        let store = store();
        let id = store.subject_create(None, "Ada", "Helper", 10).unwrap();
        let (kind, name, persona, turn_runner, standing): (String, String, String, String, String) =
            store
                .c()
                .query_row(
                    "SELECT kind,name,persona,turn_runner,standing FROM subjects WHERE id=?1",
                    params![id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .unwrap();
        assert_eq!(kind, "agent");
        assert_eq!(name, "Ada");
        assert_eq!(persona, "You are Ada (Helper).");
        assert_eq!(turn_runner, "engine");
        assert_eq!(standing, "trusted");
        let (persona_name, personality, instructions): (String, Option<String>, String) = store
            .c()
            .query_row(
                "SELECT persona_name,persona,instructions FROM subject_profiles WHERE subject_id=?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(persona_name, "Helper");
        assert_eq!(personality, None);
        assert_eq!(instructions, "");
        let (model_alias, history, output): (Option<String>, String, String) = store
            .c()
            .query_row(
                "SELECT model_alias,history_policy,output_policy FROM subject_runtime_configs
                 WHERE subject_id=?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(model_alias, None);
        assert_eq!(history, UNSET_HISTORY_POLICY);
        assert_eq!(output, UNSET_OUTPUT_POLICY);
        let agents: i64 = store
            .c()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agents'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(agents, 0);
    }

    #[test]
    fn subject_create_explicit_id_and_rejects_empty_identity() {
        let store = store();
        let id = store.subject_create(Some(42), "Ada", "Helper", 10).unwrap();
        assert_eq!(id, 42);
        let err = store
            .subject_create(None, "", "Helper", 11)
            .expect_err("empty name");
        assert!(matches!(err, SubjectCommandError::EmptyIdentity));
        let err = store
            .subject_create(None, "Ada", "", 11)
            .expect_err("empty persona_name");
        assert!(matches!(err, SubjectCommandError::EmptyIdentity));
        assert_eq!(count(&store, "SELECT COUNT(*) FROM subjects"), 1);
    }

    #[test]
    fn subject_replace_updates_and_recomposes() {
        let store = store();
        let id = store.subject_create(None, "Ada", "Helper", 10).unwrap();
        assert!(store
            .subject_replace(
                id,
                &SubjectReplace {
                    name: "Bea".into(),
                    persona_name: "Guide".into(),
                    personality: Some("curious".into()),
                    instructions: "be brief".into(),
                    model: Some("provider:model".into()),
                },
                20
            )
            .unwrap());
        let (name, persona, turn_runner): (String, String, String) = store
            .c()
            .query_row(
                "SELECT name,persona,turn_runner FROM subjects WHERE id=?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(name, "Bea");
        assert_eq!(
            persona,
            "You are Bea (Guide).\n\ncurious\n\n## Instructions\nbe brief"
        );
        assert_eq!(turn_runner, "provider:model");
        let model: Option<String> = store
            .c()
            .query_row(
                "SELECT model_alias FROM subject_runtime_configs WHERE subject_id=?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(model.as_deref(), Some("provider:model"));
        assert!(!store
            .subject_replace(
                99,
                &SubjectReplace {
                    name: "X".into(),
                    persona_name: "Y".into(),
                    personality: None,
                    instructions: String::new(),
                    model: None,
                },
                21
            )
            .unwrap());
    }

    #[test]
    fn subject_patch_null_semantics_keep_and_clear() {
        let store = store();
        let id = store.subject_create(None, "Ada", "Helper", 10).unwrap();
        store
            .subject_replace(
                id,
                &SubjectReplace {
                    name: "Ada".into(),
                    persona_name: "Helper".into(),
                    personality: Some("curious".into()),
                    instructions: "be brief".into(),
                    model: Some("provider:model".into()),
                },
                20,
            )
            .unwrap();
        assert!(store
            .subject_patch(
                id,
                &SubjectPatch {
                    name: Some("Bea".into()),
                    ..SubjectPatch::default()
                },
                30,
            )
            .unwrap());
        let (name, persona, persona_name, personality, instructions, model): (
            String,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
        ) = store
            .c()
            .query_row(
                "SELECT s.name,s.persona,p.persona_name,p.persona,p.instructions,r.model_alias
                 FROM subjects s
                 JOIN subject_profiles p ON p.subject_id=s.id AND p.revision=1
                 JOIN subject_runtime_configs r ON r.subject_id=s.id AND r.revision=1
                 WHERE s.id=?1",
                params![id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(name, "Bea");
        assert_eq!(persona_name, "Helper");
        assert_eq!(personality.as_deref(), Some("curious"));
        assert_eq!(instructions, "be brief");
        assert_eq!(model.as_deref(), Some("provider:model"));
        assert_eq!(
            persona,
            "You are Bea (Helper).\n\ncurious\n\n## Instructions\nbe brief"
        );
        assert!(store
            .subject_patch(
                id,
                &SubjectPatch {
                    personality: Some(String::new()),
                    instructions: Some(String::new()),
                    model: Some(String::new()),
                    ..SubjectPatch::default()
                },
                31,
            )
            .unwrap());
        let (persona, personality, instructions, model, turn_runner): (
            String,
            Option<String>,
            String,
            Option<String>,
            String,
        ) = store
            .c()
            .query_row(
                "SELECT s.persona,p.persona,p.instructions,r.model_alias,s.turn_runner
                 FROM subjects s
                 JOIN subject_profiles p ON p.subject_id=s.id AND p.revision=1
                 JOIN subject_runtime_configs r ON r.subject_id=s.id AND r.revision=1
                 WHERE s.id=?1",
                params![id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(personality, None);
        assert_eq!(instructions, "");
        assert_eq!(model, None);
        assert_eq!(turn_runner, "engine");
        assert_eq!(persona, "You are Bea (Helper).");
    }

    #[test]
    fn subject_delete_touches_exactly_body_table_set() {
        let store = store();
        let keep = store.subject_create(None, "Keep", "K", 10).unwrap();
        let gone = store.subject_create(None, "Gone", "G", 11).unwrap();
        insert_skill(&store, keep, "skill-keep");
        insert_skill(&store, gone, "skill-gone");
        store
            .c()
            .execute(
                "INSERT INTO memories(subject_id,body,written_at) VALUES(?1,'keep-mem',1),(?2,'gone-mem',1)",
                params![keep, gone],
            )
            .unwrap();
        store
            .c()
            .execute(
                "INSERT INTO soul_presets(id,agent_id,preset_name,persona_name,created_at,updated_at)
                 VALUES('p-keep',?1,'pk','n','t','t'),('p-gone',?2,'pg','n','t','t')",
                params![keep.to_string(), gone.to_string()],
            )
            .unwrap();
        store
            .c()
            .execute(
                "INSERT INTO offloads(activity_id,subject_id,place_id,body,truncated,created_at)
                 VALUES(1,?1,1,'orphan',0,1)",
                params![gone],
            )
            .unwrap();
        let instance =
            GateInstanceId::parse("018f8020-0000-7000-8000-000000000001".to_string()).unwrap();
        let kind = GateKindId::parse("discord".to_string()).unwrap();
        let config = serde_json::to_vec(&json!({
            "agent_ids": [],
            "legacy_updated_at": "",
            "owner_external_id": "",
            "self_external_id": null,
        }))
        .unwrap();
        store
            .install_gate_instance_revision(
                &instance,
                &kind,
                "dedicated:discord:gone",
                Some(gone),
                1,
                true,
                OriginScope::KindAddress,
                IngressDiscovery::Membership,
                "gate-config/discord/v1",
                &config,
                12,
            )
            .unwrap();

        assert!(store.subject_delete(gone, 40).unwrap());
        assert!(!store.subject_delete(gone, 41).unwrap());

        assert_eq!(
            count_bound(&store, "SELECT COUNT(*) FROM subjects WHERE id=?1", gone),
            0
        );
        assert_eq!(
            count_bound(&store, "SELECT COUNT(*) FROM subjects WHERE id=?1", keep),
            1
        );
        assert_eq!(
            count_bound(
                &store,
                "SELECT COUNT(*) FROM skills WHERE owner_subject_id=?1",
                gone
            ),
            0
        );
        assert_eq!(
            count_bound(
                &store,
                "SELECT COUNT(*) FROM skills WHERE owner_subject_id=?1",
                keep
            ),
            1
        );
        assert_eq!(
            count_bound(
                &store,
                "SELECT COUNT(*) FROM memories WHERE subject_id=?1",
                gone
            ),
            0
        );
        assert_eq!(
            count_bound(
                &store,
                "SELECT COUNT(*) FROM memories WHERE subject_id=?1",
                keep
            ),
            1
        );
        assert_eq!(
            count(
                &store,
                &format!("SELECT COUNT(*) FROM soul_presets WHERE agent_id='{gone}'")
            ),
            0
        );
        assert_eq!(
            count(
                &store,
                &format!("SELECT COUNT(*) FROM soul_presets WHERE agent_id='{keep}'")
            ),
            1
        );
        let present: i64 = store
            .c()
            .query_row(
                "SELECT r.present FROM gate_instances gi
                 JOIN gate_instance_revisions r
                   ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
                 WHERE gi.owner_subject_id=?1",
                params![gone],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(present, 0);
        assert_eq!(
            count_bound(
                &store,
                "SELECT COUNT(*) FROM subject_profiles WHERE subject_id=?1",
                gone
            ),
            1
        );
        assert_eq!(
            count_bound(
                &store,
                "SELECT COUNT(*) FROM offloads WHERE subject_id=?1",
                gone
            ),
            1
        );
    }

    fn install_owned_gate(
        store: &Store,
        owner: SubjectId,
        kind: &str,
        instance: &str,
        label: &str,
    ) {
        let instance = GateInstanceId::parse(instance.to_string()).unwrap();
        let kind_id = GateKindId::parse(kind.to_string()).unwrap();
        let schema = format!("gate-config/{kind}/v1");
        let config = serde_json::to_vec(&json!({
            "agent_ids": [],
            "legacy_updated_at": "",
            "owner_external_id": "",
            "self_external_id": null,
        }))
        .unwrap();
        store
            .install_gate_instance_revision(
                &instance,
                &kind_id,
                label,
                Some(owner),
                1,
                true,
                OriginScope::KindAddress,
                IngressDiscovery::Membership,
                &schema,
                &config,
                12,
            )
            .unwrap();
    }

    fn active_present(store: &Store, instance: &str) -> i64 {
        store
            .c()
            .query_row(
                "SELECT r.present FROM gate_instances gi
                 JOIN gate_instance_revisions r
                   ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
                 WHERE gi.instance_id=?1",
                params![instance],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn subject_delete_tombstones_discord_only() {
        let store = store();
        let gone = store.subject_create(None, "Gone", "G", 11).unwrap();
        install_owned_gate(
            &store,
            gone,
            "discord",
            "018f8020-0000-7000-8000-000000000011",
            "dedicated:discord:gone",
        );
        install_owned_gate(
            &store,
            gone,
            "nostr",
            "018f8020-0000-7000-8000-000000000012",
            "dedicated:nostr:gone",
        );

        assert!(store.subject_delete(gone, 40).unwrap());
        assert_eq!(
            active_present(&store, "018f8020-0000-7000-8000-000000000011"),
            0
        );
        assert_eq!(
            active_present(&store, "018f8020-0000-7000-8000-000000000012"),
            1
        );
    }

    #[test]
    fn subject_delete_cleans_ancillaries_when_subject_absent() {
        let store = store();
        let missing: SubjectId = 99;
        insert_skill(&store, missing, "skill-orphan");
        store
            .c()
            .execute(
                "INSERT INTO memories(subject_id,body,written_at) VALUES(?1,'orphan-mem',1)",
                params![missing],
            )
            .unwrap();
        store
            .c()
            .execute(
                "INSERT INTO soul_presets(id,agent_id,preset_name,persona_name,created_at,updated_at)
                 VALUES('p-orphan',?1,'po','n','t','t')",
                params![missing.to_string()],
            )
            .unwrap();
        install_owned_gate(
            &store,
            missing,
            "discord",
            "018f8020-0000-7000-8000-000000000099",
            "dedicated:discord:orphan",
        );

        assert!(!store.subject_delete(missing, 50).unwrap());
        assert_eq!(
            count_bound(
                &store,
                "SELECT COUNT(*) FROM skills WHERE owner_subject_id=?1",
                missing
            ),
            0
        );
        assert_eq!(
            count_bound(
                &store,
                "SELECT COUNT(*) FROM memories WHERE subject_id=?1",
                missing
            ),
            0
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM soul_presets WHERE agent_id='99'"
            ),
            0
        );
        assert_eq!(
            active_present(&store, "018f8020-0000-7000-8000-000000000099"),
            0
        );
    }
}
