use super::*;

#[derive(Clone, Debug, Deserialize)]
pub(super) struct ScenarioCatalog {
    pub(super) schema_version: u32,
    pub(super) http: HttpScenarioCatalog,
    pub(super) tool_visibility: ToolVisibilityCatalog,
    pub(super) tool_execution: ToolScenarioCatalog,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct ToolVisibilityCatalog {
    pub(super) cases: Vec<ToolVisibilityScenario>,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct ToolVisibilityScenario {
    pub(super) name: String,
    pub(super) caller: String,
    pub(super) depth: u32,
    pub(super) shell_enabled: bool,
    #[serde(default)]
    pub(super) transport: ToolTransportProfile,
    #[serde(default)]
    pub(super) allowlist: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct HttpScenarioCatalog {
    pub(super) path_parameters: BTreeMap<String, PathParameter>,
    #[serde(default)]
    pub(super) path_overrides: BTreeMap<String, PathParameter>,
    #[serde(default)]
    pub(super) query_suffixes: BTreeMap<String, String>,
    pub(super) normal_bodies: BTreeMap<String, Value>,
    pub(super) normal_uncollected_l3: BTreeMap<String, String>,
    pub(super) bodyless_alternates: BTreeMap<String, AlternateScenario>,
    #[serde(default)]
    pub(super) mutation_postconditions: BTreeMap<String, HttpPostcondition>,
    #[serde(default)]
    pub(super) successful_non_mutations: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct PathParameter {
    pub(super) normal: String,
    pub(super) missing: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum AlternateScenario {
    MissingResource,
    NotApplicable { reason: String },
    Uncollected { reason: String },
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct HttpPostcondition {
    #[serde(default)]
    pub(super) method: Option<String>,
    #[serde(default)]
    pub(super) path: Option<String>,
    #[serde(default)]
    pub(super) body: Option<Value>,
    #[serde(default)]
    pub(super) db_query: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct ToolScenarioCatalog {
    pub(super) success_arguments: BTreeMap<String, Value>,
    pub(super) success_uncollected_l3: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) fixtures: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) postconditions: BTreeMap<String, ToolPostcondition>,
    #[serde(default)]
    pub(super) effectful_tools: BTreeSet<String>,
    #[serde(default)]
    pub(super) read_only_tools: BTreeSet<String>,
    pub(super) forwarding: BTreeMap<String, ForwardingScenario>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ToolTransportProfile {
    #[default]
    WithoutTransport,
    Discord,
    Nostr,
}

impl ToolTransportProfile {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::WithoutTransport => "without_transport",
            Self::Discord => "discord",
            Self::Nostr => "nostr",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct ForwardingScenario {
    pub(super) tool: String,
    pub(super) arguments: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct ToolPostcondition {
    #[serde(default)]
    pub(super) tool: Option<String>,
    #[serde(default)]
    pub(super) method: Option<String>,
    #[serde(default)]
    pub(super) uri: Option<String>,
    #[serde(default)]
    pub(super) db_query: Option<String>,
    pub(super) arguments: Value,
    #[serde(default)]
    pub(super) expect_success: bool,
    #[serde(default)]
    pub(super) expect_status: Option<u16>,
}

pub(super) fn read_json(path: &Path) -> Result<Value, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))
}

pub(super) fn read_scenarios(path: &Path) -> Result<(Value, ScenarioCatalog), String> {
    let raw = read_json(path)?;
    let typed = serde_json::from_value(raw.clone())
        .map_err(|error| format!("parse typed scenario catalog {}: {error}", path.display()))?;
    Ok((raw, typed))
}

pub(super) fn normalize(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(normalize),
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                normalize(value);
                if (key.ends_with("_at")
                    || (matches!(key.as_str(), "date_from" | "date_to")
                        && value.as_str().is_some_and(|s| s.contains('T'))))
                    && value.is_string()
                {
                    *value = Value::String("<timestamp>".to_string());
                }
                if let Some(text) = value.as_str() {
                    let generated = [("unit_id", "unit-"), ("core_id", "core-")]
                        .iter()
                        .find_map(|(field, prefix)| {
                            (key == *field).then(|| text.strip_prefix(prefix)).flatten()
                        })
                        .is_some_and(|suffix| {
                            suffix.len() == 24 && suffix.chars().all(|c| c.is_ascii_hexdigit())
                        });
                    if generated {
                        *value = Value::String(format!("<generated-{key}>"));
                    }
                }
                if matches!(key.as_str(), "duration_ms" | "latency_ms") && value.is_number() {
                    *value = Value::String("<duration>".to_string());
                }
                if key == "score" {
                    if let Some(number) = value.as_f64().and_then(|number| {
                        serde_json::Number::from_f64(
                            (number * 1_000_000_000_000.0).round() / 1_000_000_000_000.0,
                        )
                    }) {
                        *value = Value::Number(number);
                    }
                }
            }
        }
        Value::String(text) => normalize_string(text),
        _ => {}
    }
}

fn normalize_string(text: &mut String) {
    for (prefix, marker) in [
        ("unit-", "<generated-unit-id>"),
        ("core-", "<generated-core-id>"),
    ] {
        if text.strip_prefix(prefix).is_some_and(|suffix| {
            suffix.len() == 24
                && suffix
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
        }) {
            *text = marker.to_string();
            return;
        }
    }
    if uuid::Uuid::parse_str(text).is_ok() {
        *text = "<uuid>".to_string();
        return;
    }
    if text
        .strip_prefix("subtask-")
        .is_some_and(|suffix| uuid::Uuid::parse_str(suffix).is_ok())
    {
        *text = "subtask-<uuid>".to_string();
        return;
    }
    for marker in [
        "opencrab-baseline-l2-tools-",
        "opencrab-baseline-l2-workspace-",
        "opencrab-baseline-l2-process-",
        "opencrab-baseline-l2-import-",
    ] {
        let Some(marker_start) = text.find(marker) else {
            continue;
        };
        let path_start = text[..marker_start]
            .rfind(": ")
            .map(|start| start + 2)
            .or_else(|| text[..marker_start].rfind('=').map(|start| start + 1))
            .unwrap_or(0);
        let marker_end = marker_start + marker.len();
        let path_end = text[marker_end..]
            .find('/')
            .map_or(text.len(), |offset| marker_end + offset + 1);
        text.replace_range(path_start..path_end, "<workspace>/");
    }
}
