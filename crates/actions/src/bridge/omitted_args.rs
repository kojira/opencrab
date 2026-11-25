//! D-1150: an optional argument sent as `""` means "not given", for every tool.
//!
//! LLMs often fill an optional argument they mean to omit with an empty string (production:
//! `say {"user": ""}` on a gateway operation, `update_my_schedule {"timezone": ""}` on a core
//! tool). Every tool call passes [`BridgedExecutor::dispatch_inner`], so the rule lives here once:
//! a top-level argument that the tool's parameters schema (the definition the LLM sees) does not
//! list in `required`, and whose value is an empty or blank string, is removed before the tool
//! runs. Required arguments and nested values reach the tool as sent; each tool still validates
//! the values it receives. A tool without a known schema gets its arguments unchanged.

use serde_json::Value;

use super::BridgedExecutor;

impl BridgedExecutor {
    /// `args` without the empty optional arguments of `name` (see the module comment).
    pub(super) fn without_omitted_args(&self, name: &str, args: &Value) -> Value {
        if !has_blank_string(args) {
            return args.clone();
        }
        let schema = self
            .effective_tool_definitions()
            .into_iter()
            .find(|tool| tool.definition.name == name)
            .map(|tool| tool.definition.parameters);
        match schema {
            Some(schema) => drop_empty_optional_args(&schema, args),
            None => args.clone(),
        }
    }
}

fn is_blank_string(value: &Value) -> bool {
    matches!(value, Value::String(s) if s.trim().is_empty())
}

fn has_blank_string(args: &Value) -> bool {
    args.as_object()
        .is_some_and(|map| map.values().any(is_blank_string))
}

fn drop_empty_optional_args(schema: &Value, args: &Value) -> Value {
    let Value::Object(map) = args else {
        return args.clone();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    Value::Object(
        map.iter()
            .filter(|(key, value)| required.contains(&key.as_str()) || !is_blank_string(value))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::drop_empty_optional_args;
    use serde_json::json;

    #[test]
    fn drops_blank_optional_strings_and_keeps_required_and_nested() {
        let schema = json!({"type": "object", "required": ["text"], "properties": {
            "text": {"type": "string"}, "user": {"type": "string"}, "tz": {"type": "string"},
            "opts": {"type": "object"}, "enabled": {"type": "boolean"}
        }});
        let args = json!({
            "text": "", "user": "", "tz": "  ", "opts": {"inner": ""}, "enabled": false,
            "note": "kept"
        });
        assert_eq!(
            drop_empty_optional_args(&schema, &args),
            json!({"text": "", "opts": {"inner": ""}, "enabled": false, "note": "kept"})
        );
    }
}
