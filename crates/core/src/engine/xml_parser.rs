use serde_json::Value;

use super::types::ToolCall;

// ---------------------------------------------------------------------------
// XML <function_calls> parser helpers
// ---------------------------------------------------------------------------

/// Parse `<function_calls>` XML blocks that some LLMs emit in content instead
/// of using structured tool calls.
///
/// Supports:
/// ```xml
/// <function_calls>
/// <invoke name="tool_name">
/// <param1>value1</param1>
/// <param2>["a","b"]</param2>
/// </invoke>
/// </function_calls>
/// ```
pub fn parse_xml_tool_calls(content: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut search_from = 0;

    while let Some(fc_start) = content[search_from..].find("<function_calls>") {
        let fc_start = search_from + fc_start;
        let fc_end = match content[fc_start..].find("</function_calls>") {
            Some(pos) => fc_start + pos + "</function_calls>".len(),
            None => break,
        };
        let block = &content[fc_start..fc_end];

        // Parse each <invoke name="...">...</invoke> within the block.
        let mut invoke_from = 0;
        while let Some(inv_start) = block[invoke_from..].find("<invoke") {
            let inv_start = invoke_from + inv_start;
            let inv_end = match block[inv_start..].find("</invoke>") {
                Some(pos) => inv_start + pos + "</invoke>".len(),
                None => break,
            };
            let invoke_block = &block[inv_start..inv_end];

            // Extract name from <invoke name="...">
            if let Some(tool_name) = extract_attribute(invoke_block, "name") {
                // Find the end of the opening <invoke ...> tag.
                let body_start = match invoke_block.find('>') {
                    Some(pos) => pos + 1,
                    None => {
                        invoke_from = inv_end;
                        continue;
                    }
                };
                let body_end = invoke_block.len() - "</invoke>".len();
                let body = &invoke_block[body_start..body_end];

                let arguments = parse_invoke_body(body);
                let id = format!("xml_tc_{}", calls.len());

                calls.push(ToolCall {
                    id,
                    name: tool_name,
                    arguments,
                });
            }

            invoke_from = inv_end;
        }

        search_from = fc_end;
    }

    calls
}

/// Extract an attribute value from an XML tag string, e.g. `name="foo"` -> `"foo"`.
fn extract_attribute(tag: &str, attr: &str) -> Option<String> {
    let pattern = format!("{}=\"", attr);
    let start = tag.find(&pattern)? + pattern.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

/// Parse the body of an `<invoke>` block into a JSON object.
/// Each `<tag>value</tag>` becomes a key-value pair. If the value parses as
/// JSON (array or object), it is stored as the parsed Value; otherwise as a string.
fn parse_invoke_body(body: &str) -> Value {
    let mut map = serde_json::Map::new();
    let mut pos = 0;

    while pos < body.len() {
        // Find next opening tag.
        let tag_open = match body[pos..].find('<') {
            Some(p) => pos + p,
            None => break,
        };
        let tag_close = match body[tag_open..].find('>') {
            Some(p) => tag_open + p,
            None => break,
        };

        // Skip if this looks like a closing tag.
        if body.get(tag_open + 1..tag_open + 2) == Some("/") {
            pos = tag_close + 1;
            continue;
        }

        let tag_name = &body[tag_open + 1..tag_close];
        // Skip tags with attributes or self-closing tags for simplicity.
        if tag_name.contains(' ') || tag_name.contains('/') {
            pos = tag_close + 1;
            continue;
        }

        let closing = format!("</{}>", tag_name);
        let value_start = tag_close + 1;
        let value_end = match body[value_start..].find(&closing) {
            Some(p) => value_start + p,
            None => {
                pos = tag_close + 1;
                continue;
            }
        };

        let raw_value = body[value_start..value_end].trim();

        // Try to parse as JSON value (array, object, number, bool).
        let json_value = match serde_json::from_str::<Value>(raw_value) {
            Ok(v @ Value::Array(_))
            | Ok(v @ Value::Object(_))
            | Ok(v @ Value::Number(_))
            | Ok(v @ Value::Bool(_)) => v,
            _ => Value::String(raw_value.to_string()),
        };

        map.insert(tag_name.to_string(), json_value);
        pos = value_end + closing.len();
    }

    Value::Object(map)
}

/// Strip all `<function_calls>...</function_calls>` blocks from content.
pub(crate) fn strip_function_calls_xml(content: &str) -> String {
    let mut result = String::new();
    let mut pos = 0;

    while let Some(start) = content[pos..].find("<function_calls>") {
        let start = pos + start;
        result.push_str(&content[pos..start]);
        match content[start..].find("</function_calls>") {
            Some(end) => pos = start + end + "</function_calls>".len(),
            None => {
                // Unclosed tag — remove the rest.
                return result.trim().to_string();
            }
        }
    }
    result.push_str(&content[pos..]);
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_xml_execute_shell() {
        let xml = r#"Here is the result:
<function_calls>
<invoke name="execute_shell">
<command>curl</command>
<args>["https://wttr.in/Hakata?format=%l:+%c+%t"]</args>
</invoke>
</function_calls>"#;

        let calls = parse_xml_tool_calls(xml);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "execute_shell");
        assert_eq!(calls[0].id, "xml_tc_0");
        assert_eq!(calls[0].arguments["command"], "curl");
        // args should be parsed as a JSON array
        let args = &calls[0].arguments["args"];
        assert!(args.is_array());
        assert_eq!(args[0], "https://wttr.in/Hakata?format=%l:+%c+%t");
    }

    #[test]
    fn test_parse_xml_single_param() {
        let xml = r#"<function_calls>
<invoke name="send_message">
<text>Hello world</text>
</invoke>
</function_calls>"#;

        let calls = parse_xml_tool_calls(xml);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "send_message");
        // Single text param should be a JSON string
        assert_eq!(calls[0].arguments["text"], "Hello world");
    }

    #[test]
    fn test_parse_xml_no_xml() {
        let calls = parse_xml_tool_calls("Just a normal response with no XML.");
        assert!(calls.is_empty());
    }

    #[test]
    fn test_parse_xml_multiple_invoke() {
        let xml = r#"<function_calls>
<invoke name="tool_a">
<x>1</x>
</invoke>
<invoke name="tool_b">
<y>two</y>
</invoke>
</function_calls>"#;

        let calls = parse_xml_tool_calls(xml);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "tool_a");
        assert_eq!(calls[0].id, "xml_tc_0");
        assert_eq!(calls[1].name, "tool_b");
        assert_eq!(calls[1].id, "xml_tc_1");
    }

    #[test]
    fn test_parse_xml_json_value_types() {
        let xml = r#"<function_calls>
<invoke name="test">
<arr>[1, 2, 3]</arr>
<obj>{"key": "val"}</obj>
<num>42</num>
<flag>true</flag>
<text>plain string</text>
</invoke>
</function_calls>"#;

        let calls = parse_xml_tool_calls(xml);
        assert_eq!(calls.len(), 1);
        let args = &calls[0].arguments;
        assert!(args["arr"].is_array());
        assert_eq!(args["arr"][0], 1);
        assert!(args["obj"].is_object());
        assert_eq!(args["obj"]["key"], "val");
        assert_eq!(args["num"], 42);
        assert_eq!(args["flag"], true);
        assert_eq!(args["text"], "plain string");
    }

    #[test]
    fn test_strip_function_calls_xml() {
        let content = "Before\n<function_calls>\n<invoke name=\"x\"><a>1</a></invoke>\n</function_calls>\nAfter";
        let stripped = strip_function_calls_xml(content);
        assert_eq!(stripped, "Before\n\nAfter");
        assert!(!stripped.contains("<function_calls>"));
    }
}
