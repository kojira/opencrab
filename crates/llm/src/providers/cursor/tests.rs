use super::*;

#[test]
fn test_defaults() {
    let p = CursorProvider::new();
    assert_eq!(p.binary_path, "cursor-agent");
    assert_eq!(p.default_model, "auto");
    assert!(p.api_key.is_none());
    assert_eq!(p.timeout, Duration::from_secs(300));
    // 既定サンドボックスは最安全側（enabled）。
    assert_eq!(p.sandbox, "enabled");
}

#[test]
fn test_builders() {
    let p = CursorProvider::new()
        .with_binary_path("cursor")
        .with_default_model("sonnet-4.5")
        .with_timeout_secs(120)
        .with_sandbox("disabled")
        .with_api_key("sk-cursor");
    assert_eq!(p.binary_path, "cursor");
    assert_eq!(p.default_model, "sonnet-4.5");
    assert_eq!(p.timeout, Duration::from_secs(120));
    assert_eq!(p.sandbox, "disabled");
    assert_eq!(p.api_key.as_deref(), Some("sk-cursor"));
    // 空文字は既定を維持 / api_key は None
    let p2 = CursorProvider::new()
        .with_binary_path("")
        .with_sandbox("  ")
        .with_api_key("  ");
    assert_eq!(p2.binary_path, "cursor-agent");
    assert_eq!(p2.sandbox, "enabled");
    assert!(p2.api_key.is_none());
}

/// #674 の核: コマンドラインが「推論専用」の契約を満たすこと。
/// - `--force` / `--yolo` を**絶対に含まない**（危険操作の無承認実行を封じる）
/// - `--plan`（読取専用）・`--sandbox <値>`・`--trust` を含む
/// - `-p --output-format json -m <model>` があり、プロンプトは末尾 positional
#[test]
fn test_build_command_is_inference_only() {
    let p = CursorProvider::new().with_default_model("auto");
    let (cmd, _cwd) = p
        .build_command("gpt-5.2", "[System]\nhi")
        .expect("build_command");
    let args: Vec<String> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    // 危険フラグは存在してはならない。
    assert!(
        !args
            .iter()
            .any(|a| a == "--force" || a == "--yolo" || a == "-f"),
        "cursor は推論専用: --force/--yolo を含めてはならない: {args:?}"
    );
    // 読取専用モード + サンドボックス + 信頼。
    assert!(args.iter().any(|a| a == "--plan"), "--plan 必須: {args:?}");
    let sb = args
        .iter()
        .position(|a| a == "--sandbox")
        .expect("--sandbox 必須");
    assert_eq!(args.get(sb + 1).map(String::as_str), Some("enabled"));
    assert!(
        args.iter().any(|a| a == "--trust"),
        "--trust 必須: {args:?}"
    );
    // headless / JSON / モデル。
    assert!(args.iter().any(|a| a == "-p"));
    let of = args
        .iter()
        .position(|a| a == "--output-format")
        .expect("--output-format");
    assert_eq!(args.get(of + 1).map(String::as_str), Some("json"));
    // モデルは長形式 --model（この CLI 版は -m を受け付けない。実測 #674）。
    assert!(!args.iter().any(|a| a == "-m"), "-m は無効: {args:?}");
    let m = args.iter().position(|a| a == "--model").expect("--model");
    assert_eq!(args.get(m + 1).map(String::as_str), Some("gpt-5.2"));
    // プロンプトは末尾の positional（オプション扱いされない）。
    assert_eq!(args.last().map(String::as_str), Some("[System]\nhi"));
}

/// sandbox の値は config から差し替えられ、コマンドラインに反映されること。
#[test]
fn test_build_command_sandbox_override() {
    let p = CursorProvider::new().with_sandbox("disabled");
    let (cmd, _cwd) = p.build_command("auto", "hi").expect("build_command");
    let args: Vec<String> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let sb = args
        .iter()
        .position(|a| a == "--sandbox")
        .expect("--sandbox");
    assert_eq!(args.get(sb + 1).map(String::as_str), Some("disabled"));
    // 不変条件: サンドボックスを最弱（disabled）にしても読取専用（--plan）は残る。
    assert!(
        args.iter().any(|a| a == "--plan"),
        "sandbox=disabled でも --plan は残らねばならない: {args:?}"
    );
}

/// #682: build_command は空の一時 cwd を作り、その中に deny 設定
/// （`.cursor/cli.json`）だけを置くこと。
/// - cwd を CLI の current_dir に設定している
/// - `.cursor/cli.json` の中身が [`CURSOR_DENY_CONFIG`] と一致し、`version` を含まない
/// - cwd 直下には `.cursor` 以外に何も無い（実 workspace を露出させない ＝ 空 cwd）
/// - grep/glob は deny に列挙しない（効かないものを載せて誤認させない・#682）
#[test]
fn test_build_command_creates_empty_cwd_with_deny_config() {
    let p = CursorProvider::new();
    let (cmd, cwd) = p.build_command("auto", "hi").expect("build_command");

    // CLI の cwd が一時ディレクトリに設定されている。
    assert_eq!(
        cmd.as_std().get_current_dir(),
        Some(cwd.path()),
        "cwd は一時ディレクトリでなければならない"
    );

    // deny 設定の中身が完全一致し、version キーを含まない（project 版は schema エラー）。
    let cli_json = std::fs::read_to_string(cwd.path().join(".cursor").join("cli.json"))
        .expect(".cursor/cli.json が読めること");
    assert_eq!(cli_json, CURSOR_DENY_CONFIG);
    assert!(
        !cli_json.contains("version"),
        "project 版 cli.json に version を付けてはならない: {cli_json}"
    );
    // deny 内容の要点（読取・書込・シェル・ネット・MCP を封じ、allow は空）。
    assert!(cli_json.contains(r#""allow":[]"#));
    for tool in [
        "Read(**)",
        "Write(**)",
        "Shell(**)",
        "WebFetch(**)",
        "WebSearch(**)",
        "Mcp(**)",
    ] {
        assert!(cli_json.contains(tool), "deny に {tool} が無い: {cli_json}");
    }
    // grep 系は deny が効かないので列挙しない（効くと誤認させないため）。
    for absent in ["Grep", "list_dir", "codebase_search", "Glob"] {
        assert!(
            !cli_json.contains(absent),
            "deny 効果のない {absent} を列挙してはならない: {cli_json}"
        );
    }

    // cwd 直下は `.cursor` 以外に何も無い（空 cwd の担保）。
    let entries: Vec<String> = std::fs::read_dir(cwd.path())
        .expect("read_dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        entries,
        vec![".cursor".to_string()],
        "cwd は空 cwd（+deny）でなければならない"
    );
}

/// #682 RAII: 返した TempDir を drop すると一時 cwd がディレクトリごと消える
/// （孤児を残さない）。
#[test]
fn test_temp_cwd_is_removed_on_drop() {
    let p = CursorProvider::new();
    let (_cmd, cwd) = p.build_command("auto", "hi").expect("build_command");
    let path = cwd.path().to_path_buf();
    assert!(path.exists(), "生成直後は cwd が存在する");
    drop(cwd);
    assert!(
        !path.exists(),
        "drop 後は一時 cwd が削除されていなければならない"
    );
}

/// env 最小化: 許可キー（PATH/HOME/CURSOR_API_KEY）以外を渡さない。
/// CURSOR_API_KEY は api_key 指定時のみ含む。
#[test]
fn test_minimal_env_only_allows_expected_keys() {
    // api_key 未指定: CURSOR_API_KEY は含まない。
    let env = minimal_env(None);
    for (k, _) in &env {
        assert!(
            *k == "PATH" || *k == "HOME",
            "予期しない env キーが混入: {k}"
        );
    }
    assert!(!env.iter().any(|(k, _)| *k == "CURSOR_API_KEY"));

    // api_key 指定時: CURSOR_API_KEY を値付きで含む。
    let env2 = minimal_env(Some("sk-test-123"));
    for (k, _) in &env2 {
        assert!(
            *k == "PATH" || *k == "HOME" || *k == "CURSOR_API_KEY",
            "予期しない env キーが混入: {k}"
        );
    }
    assert_eq!(
        env2.iter()
            .find(|(k, _)| *k == "CURSOR_API_KEY")
            .map(|(_, v)| v.as_str()),
        Some("sk-test-123")
    );
}

/// usage（camelCase）を JSON から正しくマッピングすること（コスト計測が効くように）。
#[test]
fn test_parse_cursor_usage_camelcase() {
    let stdout = r#"{"type":"result","is_error":false,"result":"ok","usage":{"inputTokens":14028,"outputTokens":50,"cacheReadTokens":7552,"cacheWriteTokens":3}}"#;
    let usage = parse_cursor_usage(stdout);
    assert_eq!(usage.prompt_tokens, 14028);
    assert_eq!(usage.completion_tokens, 50);
    assert_eq!(usage.total_tokens, 14078);
    assert_eq!(usage.cache_read_input_tokens, 7552);
    assert_eq!(usage.cache_creation_input_tokens, 3);
}

/// usage が無い出力では全ゼロ（握りつぶさず素直に 0）。
#[test]
fn test_parse_cursor_usage_missing_is_zero() {
    let usage = parse_cursor_usage(r#"{"type":"result","result":"ok"}"#);
    assert_eq!(usage.total_tokens, 0);
    assert_eq!(usage.prompt_tokens, 0);
    assert_eq!(usage.completion_tokens, 0);
}

#[test]
fn test_available_models_includes_extra() {
    let p = CursorProvider::new().with_extra_models(vec![("custom-x".to_string(), 128_000)]);
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let models = rt.block_on(p.available_models()).unwrap();
    assert!(models.iter().any(|m| m.id == "auto"));
    assert!(models.iter().any(|m| m.id == "custom-x"));
}

#[test]
fn test_resolve_success_extracts_result() {
    // マルチバイト（絵文字）を含む result も正しく取り出せること。
    // 生バイト文字列は ASCII 限定なので通常文字列を bytes 化して渡す。
    let json = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":10,"result":"Hello 👋","session_id":"s1"}"#;
    let out = resolve_cursor_output("exit status: 0", true, json.as_bytes(), b"").unwrap();
    assert_eq!(out, "Hello 👋");
}

#[test]
fn test_resolve_nonzero_keeps_result() {
    // 非ゼロ終了でも result があれば捨てない
    let json = br#"{"type":"result","result":"partial answer"}"#;
    let out = resolve_cursor_output("exit status: 1", false, json, b"some warning").unwrap();
    assert_eq!(out, "partial answer");
}

#[test]
fn test_resolve_is_error_with_empty_result_fails() {
    let json = br#"{"type":"result","is_error":true,"result":""}"#;
    let err = resolve_cursor_output("exit status: 1", false, json, b"real reason")
        .unwrap_err()
        .to_string();
    assert!(err.contains("real reason"), "{err}");
    assert!(err.contains("exit status: 1"), "{err}");
}

#[test]
fn test_resolve_result_in_last_line_of_stream() {
    // stream-json 風に前段イベントがあり、最後の行が result オブジェクト
    let out = br#"{"type":"assistant","text":"thinking"}
{"type":"result","result":"final text","is_error":false}"#;
    let got = resolve_cursor_output("exit status: 0", true, out, b"").unwrap();
    assert_eq!(got, "final text");
}

#[test]
fn test_resolve_non_json_success_falls_back_to_stdout() {
    // JSON でない（text 形式）出力は成功時 stdout をそのまま使う
    let out = resolve_cursor_output("exit status: 0", true, b"plain text answer", b"").unwrap();
    assert_eq!(out, "plain text answer");
    // 非ゼロ + 非JSON は失敗（stderr を握りつぶさない）
    let err = resolve_cursor_output("exit status: 2", false, b"", b"why it died")
        .unwrap_err()
        .to_string();
    assert!(err.contains("why it died"), "{err}");
}

/// 実 CLI 統合テスト（`#[ignore]`: CI に cursor-agent が無い）。
///
/// 実測（#674 フェーズ1）で固定した契約を回帰として残す:
/// 1. `--plan` で `result` JSON が返り本文が取れる
/// 2. 「ファイルを作れ」と指示しても**作成されない**（読取専用 + deny が効いている）
/// 3. `usage` が非ゼロで拾える（コスト計測が効く）
///
/// cwd は provider が内部で作る空の一時ディレクトリ（#682）。マーカーは別の
/// tempdir 内の絶対パスに置き、write が封じられていることを確認する。
///
/// 実行には cursor-agent のインストールと認証（CURSOR_API_KEY か login 済み）が要る:
///   `cargo test -p opencrab-llm cursor_cli_is_read_only -- --ignored --nocapture`
/// モデルは account 非依存の `auto` を使い、ドリフトを避ける。
#[test]
#[ignore = "requires cursor-agent CLI + auth"]
fn cursor_cli_is_read_only_and_reports_usage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("pwned_by_cursor.txt");
    let provider = CursorProvider::new()
        .with_default_model("auto")
        .with_timeout_secs(180);

    let prompt = format!(
        "Create a file named {} containing PWNED, then tell me its absolute path.",
        marker.display()
    );
    let request = ChatRequest {
        model: String::new(),
        messages: vec![Message::user(prompt)],
        functions: None,
        function_call: None,
        temperature: None,
        max_tokens: None,
        stop: None,
        stream: None,
        metadata: Default::default(),
        agent_id: None,
        reasoning_effort: None,
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let resp = rt
        .block_on(provider.chat_completion(request))
        .expect("cursor-agent chat_completion should succeed");

    // 1. 本文が返る。
    let text = match &resp.choices[0].message.content {
        Some(MessageContent::Text(t)) => t.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert!(!text.trim().is_empty(), "empty response body");

    // 2. 読取専用: ファイルは作られていない。
    assert!(
        !marker.exists(),
        "read-only mode violated: cursor-agent created {}",
        marker.display()
    );

    // 3. usage が拾えている（少なくとも入力トークンは非ゼロ）。
    assert!(
        resp.usage.prompt_tokens > 0,
        "usage not parsed (prompt_tokens=0): {:?}",
        resp.usage
    );
}

/// 実 CLI 統合テスト（`#[ignore]`）: deny→XML フォールバックの回帰（#682・criterion a）。
///
/// ws_read ツール定義を注入した状態で grok に「ファイルを読め」と指示すると、native
/// read が deny で拒否され、注入済み XML `<function_calls>`(ws_read) へフォールバック
/// する——これが opencrab のツールループに乗る唯一の経路。CLI 更新でこの挙動が壊れて
/// いないかを検出する。
///
/// 実測（#682）では発火率は 100% ではない（~11/13。残りは XML を出さず narration で
/// 終わる＝協定不成立で許容）。単発 assert は flaky なので N=5 回し、**過半（>=3）で
/// ws_read XML が出ること**を回帰条件にする（フォールバックが全滅していれば 0/5 で落ちる）。
/// モデルは実測で成立が確認できている `cursor-grok-4.6-high` に固定（GPT 系はツール駆動を
/// cursor 経由で使わない方針・#682）。
///
/// 【この経路で塞げていない穴・#682 受容】native grep は絶対パスを与えれば cwd 外の
/// 任意ファイル内容を読める（cli.json/--sandbox の管轄外）。ここでは扱わない（機構で
/// 塞げないためテスト化しない。モジュール doc とハーネス参照）。
///
///   `cargo test -p opencrab-llm cursor_cli_grok_deny_falls_back -- --ignored --nocapture`
#[test]
#[ignore = "requires cursor-agent CLI + auth (grok), makes 5 API calls"]
fn cursor_cli_grok_deny_falls_back_to_ws_read_xml() {
    use serde_json::json;
    let provider = CursorProvider::new()
        .with_default_model("cursor-grok-4.6-high")
        .with_timeout_secs(180);
    let ws_read = FunctionDefinition {
        name: "ws_read".to_string(),
        description: Some(
            "Read a file from the agent workspace. Returns the file contents.".to_string(),
        ),
        parameters: json!({
            "type": "object",
            "properties": {"path": {"type": "string", "description": "Path to the file"}},
            "required": ["path"]
        }),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut xml_fired = 0;
    for i in 0..5 {
        let request = ChatRequest {
            model: String::new(),
            messages: vec![
                Message::system(
                    "You are an opencrab agent. To read files you MUST use the ws_read tool. \
                     Do not read files any other way."
                        .to_string(),
                ),
                Message::user(
                    "Read the file at path notes/plan.txt and tell me what it contains."
                        .to_string(),
                ),
            ],
            functions: Some(vec![ws_read.clone()]),
            function_call: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            stream: None,
            metadata: Default::default(),
            agent_id: None,
            reasoning_effort: None,
        };
        let resp = rt
            .block_on(provider.chat_completion(request))
            .expect("cursor-agent chat_completion should succeed");
        let text = match &resp.choices[0].message.content {
            Some(MessageContent::Text(t)) => t.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        let fired = text.contains("<invoke name=\"ws_read\">");
        if fired {
            xml_fired += 1;
        }
        eprintln!("run {i}: ws_read_xml={fired} | {}", text.replace('\n', " "));
    }

    assert!(
        xml_fired >= 3,
        "deny→XML フォールバックが過半で発火しなかった（{xml_fired}/5）。CLI 更新で\
         ツール選択経路が壊れた可能性がある"
    );
}
