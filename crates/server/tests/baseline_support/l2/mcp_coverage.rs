use super::*;

async fn collect_mcp_protocol() -> Result<Value, String> {
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (client_stream, server_stream) = duplex(16 * 1024);
    let (client_read, client_write) = tokio::io::split(client_stream);
    let (server_read, mut server_write) = tokio::io::split(server_stream);
    let transcript = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
    let server_transcript = transcript.clone();
    let server = tokio::spawn(async move {
        let mut lines = BufReader::new(server_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let request: Value = serde_json::from_str(&line).expect("local MCP request JSON");
            server_transcript.lock().await.push(request.clone());
            let Some(id) = request.get("id").cloned() else {
                continue;
            };
            let result = match request["method"].as_str() {
                Some("initialize") => {
                    json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"baseline-local","version":"1"}})
                }
                Some("tools/list") => {
                    json!({"tools":[{"name":"echo","description":"local echo","inputSchema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}}]})
                }
                Some("tools/call") => {
                    json!({"content":[{"type":"text","text":"local protocol success"}],"isError":false})
                }
                other => panic!("unexpected local MCP method: {other:?}"),
            };
            let mut response =
                serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"result":result})).unwrap();
            response.push(b'\n');
            server_write.write_all(&response).await.unwrap();
        }
    });
    let connection = opencrab_mcp::McpConnection::new(Box::new(client_write), client_read);
    connection
        .initialize()
        .await
        .map_err(|e| format!("local MCP initialize: {e}"))?;
    let tools = connection
        .list_tools()
        .await
        .map_err(|e| format!("local MCP tools/list: {e}"))?;
    let call = connection
        .call_tool("echo", json!({"value":"baseline"}))
        .await
        .map_err(|e| format!("local MCP tools/call: {e}"))?;
    drop(connection);
    server
        .await
        .map_err(|e| format!("local MCP server task: {e}"))?;
    let transcript = transcript.lock().await.clone();
    Ok(json!({
        "transport":"tokio in-memory duplex (no subprocess, network, credential, or external service)",
        "client_requests":transcript,
        "observed_tools":tools.into_iter().map(|t| json!({"name":t.name,"description":t.description,"input_schema":t.input_schema})).collect::<Vec<_>>(),
        "successful_call":{"text":call.text,"is_error":call.is_error}
    }))
}

fn coverage(http: &Value, tools: &Value) -> Value {
    let mut by_status = BTreeMap::<String, usize>::new();
    let mut rejection_observed = 0;
    let mut effect_observed = 0;
    let mut effect_uncollected = 0;
    if let Some(probes) = http["probes"].as_array() {
        for probe in probes {
            let status = probe
                .pointer("/response/status")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            *by_status.entry(format!("{}xx", status / 100)).or_default() += 1;
            if matches!(
                probe["selection"].as_str(),
                Some("malformed_json_rejection" | "missing_resource_rejection")
            ) && (400..500).contains(&status)
            {
                rejection_observed += 1;
            }
            match probe["effect"]["status"].as_str() {
                Some("observed") => effect_observed += 1,
                Some("uncollected") => effect_uncollected += 1,
                _ => {}
            }
        }
    }
    let uncollected = http["uncollected"].as_array();
    let http_uncollected = uncollected.map_or(0, |items| {
        items
            .iter()
            .filter(|item| item["status"] == "uncollected")
            .count()
    });
    let http_not_applicable = uncollected.map_or(0, |items| {
        items
            .iter()
            .filter(|item| item["status"] == "not_applicable")
            .count()
    });
    let tool_postconditions = tools["successful_calls"].as_array().map_or(0, |items| {
        items
            .iter()
            .filter(|item| !item["postcondition"].is_null() || !item["effect"].is_null())
            .count()
    });
    json!({
        "http_observed_status_classes":by_status,
        "http_facets":{
            "valid_rejections":rejection_observed,
            "effects_observed":effect_observed,
            "effects_uncollected":effect_uncollected,
            "uncollected":http_uncollected,
            "not_applicable":http_not_applicable
        },
        "tool_effects_observed":tool_postconditions,
        "tool_success_uncollected_count":tools["uncollected"].as_array().map_or(0, |items| items.iter().filter(|item| item["facet"] == "success").count()),
        "claim":"Only nonempty observations satisfying their facet predicate are fixed by this artifact; every uncollected or not_applicable entry is an explicit non-claim."
    })
}

pub async fn capture(l1_path: &Path, scenario_path: &Path) -> Result<Value, String> {
    let capture_profile = capture_profile()?;
    let l1 = read_json(l1_path)?;
    let (scenarios, catalog) = read_scenarios(scenario_path)?;
    if catalog.schema_version != 1 {
        return Err("scenario catalog schema_version must be 1".to_string());
    }
    let http = collect_http(&l1, &catalog.http).await?;
    let visibility = collect_visibility(&catalog.tool_visibility)?;
    let tools = collect_tool_execution(&l1, &catalog.tool_execution).await?;
    let mcp = collect_mcp_protocol().await?;
    let coverage = coverage(&http, &tools);
    let mut artifact = json!({
        "schema_version":1,
        "capture_profile":capture_profile,
        "source":{"l1":l1_path.file_name().and_then(|s|s.to_str()).unwrap_or("opencrab-l1.json"),"scenario_catalog":scenario_path.file_name().and_then(|s|s.to_str()).unwrap_or("scenarios.json")},
        "normalization":[
            "content-length response header omitted",
            "timestamp-valued *_at fields and timestamp-valued date_from/date_to fields replaced with <timestamp>",
            "numeric duration_ms/latency_ms replaced with <duration>",
            "floating score values rounded to 12 decimal places to remove platform-level SQLite FTS noise",
            "collector-owned temporary workspace roots replaced with <workspace>",
            "UUID strings and subtask-UUID strings replaced with <uuid> markers",
            "implementation-generated unit-*/core-* identifiers replaced with field-specific markers; fixed fixture identifiers remain literal"
        ],
        "scenario_catalog":scenarios,
        "http":http,
        "tool_visibility":visibility,
        "tool_execution":tools,
        "mcp_protocol":mcp,
        "coverage":coverage
    });
    normalize(&mut artifact);
    Ok(artifact)
}
