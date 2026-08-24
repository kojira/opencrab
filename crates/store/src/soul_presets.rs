//! B 表 `soul_presets`（DESIGN-DB-MIGRATION §12.7）と Apply→SubjectPatch。

use crate::subjects::{SubjectCommandError, SubjectPatch};
use crate::Store;
use opencrab_port::SubjectId;
use rusqlite::{params, OptionalExtension};

#[derive(Debug)]
pub enum SoulPresetError {
    Store(rusqlite::Error),
    AgentMissing,
    PresetMissing,
    EmptyIdentity,
}

impl From<rusqlite::Error> for SoulPresetError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

impl From<SubjectCommandError> for SoulPresetError {
    fn from(error: SubjectCommandError) -> Self {
        match error {
            SubjectCommandError::Store(error) => Self::Store(error),
            SubjectCommandError::EmptyIdentity => Self::EmptyIdentity,
        }
    }
}

impl std::fmt::Display for SoulPresetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "{error}"),
            Self::AgentMissing => write!(f, "Agent not found."),
            Self::PresetMissing => write!(f, "Preset not found."),
            Self::EmptyIdentity => write!(f, "name and persona_name must be non-empty"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoulPreset {
    pub id: String,
    pub agent_id: String,
    pub preset_name: String,
    pub persona_name: String,
    pub custom_traits_json: Option<String>,
}

impl Store {
    pub fn soul_preset_list(
        &self,
        agent: SubjectId,
    ) -> std::result::Result<Vec<SoulPreset>, SoulPresetError> {
        let conn = self.c();
        let mut stmt = conn.prepare(
            "SELECT id,agent_id,preset_name,persona_name,custom_traits_json
             FROM soul_presets WHERE agent_id=?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map(params![agent.to_string()], |row| {
                Ok(SoulPreset {
                    id: row.get(0)?,
                    agent_id: row.get(1)?,
                    preset_name: row.get(2)?,
                    persona_name: row.get(3)?,
                    custom_traits_json: row.get(4)?,
                })
            })?
            .collect::<crate::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn soul_preset_create(
        &self,
        agent: SubjectId,
        preset_name: &str,
        now: i64,
    ) -> std::result::Result<String, SoulPresetError> {
        let view = self.subject_dashboard_view(agent)?;
        let Some(view) = view else {
            return Err(SoulPresetError::AgentMissing);
        };
        let Some(persona_name) = view.persona_name.filter(|value| !value.is_empty()) else {
            return Err(SoulPresetError::EmptyIdentity);
        };
        let id = uuid::Uuid::new_v4().to_string();
        let ts = now.to_string();
        self.c().execute(
            "INSERT INTO soul_presets(
               id,agent_id,preset_name,persona_name,custom_traits_json,created_at,updated_at
             ) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                id,
                agent.to_string(),
                preset_name,
                persona_name,
                view.personality,
                ts,
                ts
            ],
        )?;
        Ok(id)
    }

    pub fn soul_preset_delete(
        &self,
        preset_id: &str,
    ) -> std::result::Result<bool, SoulPresetError> {
        let deleted = self
            .c()
            .execute("DELETE FROM soul_presets WHERE id=?1", params![preset_id])?;
        Ok(deleted > 0)
    }

    pub fn soul_preset_apply(
        &self,
        agent: SubjectId,
        preset_id: &str,
        now: i64,
    ) -> std::result::Result<(), SoulPresetError> {
        let preset = self
            .c()
            .query_row(
                "SELECT persona_name,custom_traits_json FROM soul_presets WHERE id=?1",
                params![preset_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let Some((persona_name, custom_traits)) = preset else {
            return Err(SoulPresetError::PresetMissing);
        };
        let patched = self.subject_patch(
            agent,
            &SubjectPatch {
                persona_name: Some(persona_name),
                personality: Some(custom_traits.unwrap_or_default()),
                ..SubjectPatch::default()
            },
            now,
        )?;
        if !patched {
            return Err(SoulPresetError::AgentMissing);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::new_in_memory().unwrap()
    }

    #[test]
    fn create_list_apply_delete_writes_persona_via_subject_patch() {
        let store = store();
        let id = store.subject_create(None, "Ada", "Helper", 10).unwrap();
        store
            .subject_patch(
                id,
                &SubjectPatch {
                    personality: Some("curious".into()),
                    ..SubjectPatch::default()
                },
                11,
            )
            .unwrap();
        let preset = store.soul_preset_create(id, "saved", 12).unwrap();
        let listed = store.soul_preset_list(id).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, preset);
        assert_eq!(listed[0].preset_name, "saved");
        assert_eq!(listed[0].persona_name, "Helper");
        assert_eq!(listed[0].custom_traits_json.as_deref(), Some("curious"));

        store
            .subject_patch(
                id,
                &SubjectPatch {
                    persona_name: Some("Guide".into()),
                    personality: Some(String::new()),
                    ..SubjectPatch::default()
                },
                13,
            )
            .unwrap();
        store.soul_preset_apply(id, &preset, 14).unwrap();
        let view = store.subject_dashboard_view(id).unwrap().unwrap();
        assert_eq!(view.persona_name.as_deref(), Some("Helper"));
        assert_eq!(view.personality.as_deref(), Some("curious"));
        let persona: String = store
            .c()
            .query_row(
                "SELECT persona FROM subjects WHERE id=?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(persona, "You are Ada (Helper).\n\ncurious");

        assert!(store.soul_preset_delete(&preset).unwrap());
        assert!(!store.soul_preset_delete(&preset).unwrap());
        assert!(store.soul_preset_list(id).unwrap().is_empty());
    }

    #[test]
    fn apply_and_create_fail_loud_when_missing() {
        let store = store();
        let err = store
            .soul_preset_create(7, "x", 1)
            .expect_err("missing agent");
        assert!(matches!(err, SoulPresetError::AgentMissing));
        let err = store
            .soul_preset_apply(7, "no-such", 1)
            .expect_err("missing preset");
        assert!(matches!(err, SoulPresetError::PresetMissing));
    }
}
