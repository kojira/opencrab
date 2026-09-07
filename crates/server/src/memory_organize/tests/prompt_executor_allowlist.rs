    fn topic(id: &str, short: &str, title: &str, summary: &str) -> IndexNodeRow {
        IndexNodeRow {
            id: id.to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: "session_log".to_string(),
            title: title.to_string(),
            summary: summary.to_string(),
            start_log_id: None,
            end_log_id: Some(10),
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 3,
            child_count: 0,
            token_count: 0,
            created_at: "2026-08-01T00:00:00Z".to_string(),
            updated_at: "2026-08-01T00:00:00Z".to_string(),
            short_id: Some(short.to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        }
    }

    fn plan_with(worklist: Vec<IndexNodeRow>, tags: Vec<(String, i64)>) -> OrganizePlan {
        let worklist_size = worklist.len();
        OrganizePlan {
            persona_name: "テスト太郎".to_string(),
            personality: Some("あなたは慎重で記録魔です。".to_string()),
            instructions: String::new(),
            snapshot_log_id: 100,
            worklist,
            worklist_size,
            new_topic_count: worklist_size as i64,
            new_presented: worklist_size,
            backlog_presented: 0,
            backlog_remaining: 0,
            tags,
            new_marker_advance_to: Some("2026-08-02T00:00:00Z".to_string()),
            backlog_marker_advance_to: None,
            run_at: "2026-08-02T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn system_prompt_includes_persona_worklist_tags_and_tool_names() {
        let plan = plan_with(
            vec![
                topic("id1", "t42", "送金の設計", "手数料の扱いを議論した"),
                topic("id2", "t43", "Nostr 連携", "リレー選定の話"),
            ],
            vec![("お金".to_string(), 3), ("技術".to_string(), 7)],
        );
        let sp = build_system_prompt(&plan);
        // 人格が載る
        assert!(sp.contains("あなたは慎重で記録魔です。"));
        // worklist が短縮IDつきで載る
        assert!(sp.contains("[t42] 送金の設計 — 手数料の扱いを議論した"));
        assert!(sp.contains("[t43] Nostr 連携"));
        // 現行タグが件数つきで載る
        assert!(sp.contains("お金（3件）"));
        assert!(sp.contains("技術（7件）"));
        // タグ道具の名が載る（発散させないための道具の明示）
        assert!(sp.contains("tag_topic"));
        assert!(sp.contains("merge_tags"));
        // 新規と過去分が混ざりうる旨の明示（#365）。
        assert!(sp.contains("過去の分が混ざっています"));
    }

    #[test]
    fn system_prompt_handles_no_tags() {
        let plan = plan_with(
            vec![topic("id1", "t1", "初めての記憶", "最初の一歩")],
            vec![],
        );
        let sp = build_system_prompt(&plan);
        assert!(sp.contains("まだタグはありません"));
    }

    #[test]
    fn cursor_roundtrips_and_tolerates_bare_timestamp() {
        // format → parse で往復する。
        let m = format_cursor("2026-08-03T00:00:00Z", "topic-a1-s-000");
        assert_eq!(m, "2026-08-03T00:00:00Z|topic-a1-s-000");
        assert_eq!(
            parse_cursor(&m),
            (
                "2026-08-03T00:00:00Z".to_string(),
                "topic-a1-s-000".to_string()
            )
        );
        // `|` 無し（初回シードの素の刻時 / 旧形式）は id 空で解釈する。
        assert_eq!(
            parse_cursor("2026-08-03T00:00:00Z"),
            ("2026-08-03T00:00:00Z".to_string(), String::new())
        );
    }

    #[test]
    fn topic_line_omits_dash_when_summary_empty() {
        let line = format_topic_line(&topic("id1", "t1", "無要約", ""));
        assert_eq!(line, "- [t1] 無要約");
    }

    // --- 整理ランのツール許可リスト（#368 / 実測）---

    /// MCP スロット検証用: `mcp__` 名前空間の外部ツールを 1 つ定義するモック。
    /// 整理ランは depth 0 なので本番でも MCP が注入されうる。許可リストが MCP スロットも
    /// 覆うことを実測する。
    struct MockMcpSlot;

    #[async_trait::async_trait]
    impl opencrab_gateway::GatewayActions for MockMcpSlot {
        fn definitions(&self) -> Vec<opencrab_gateway::GatewayActionDef> {
            vec![opencrab_gateway::GatewayActionDef {
                name: "mcp__ext__send".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "external send".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }]
        }
        async fn execute(
            &self,
            name: &str,
            _args: &serde_json::Value,
            _ctx: &opencrab_gateway::GatewayCallContext,
        ) -> opencrab_gateway::GatewayActionResult {
            opencrab_gateway::GatewayActionResult {
                success: true,
                data: Some(serde_json::json!({ "reached": name })),
                error: None,
            }
        }
    }

    /// 整理ランが**実際に受け取る合成 executor**を、`process::run_agent_response` の run
    /// 構築と同じ配線で組む（dispatcher core + config 駆動の execute_shell + gateway own =
    /// `SystemGatewayActions` + MCP スロット）。`with_allowlist=true` で
    /// `ORGANIZE_ALLOWED_TOOLS` を載せる（整理ランと同じ）。
    fn build_organize_executor(
        state: &AppState,
        with_allowlist: bool,
    ) -> opencrab_actions::BridgedExecutor {
        // dispatcher: core アクション + config 駆動の execute_shell。
        let mut dispatcher = opencrab_actions::ActionDispatcher::new();
        let tools_cfg = opencrab_actions::tools::ToolsConfig {
            enabled: true,
            shell: Some(opencrab_actions::tools::ShellToolConfig {
                enabled: true,
                allowed_commands: vec!["echo".to_string()],
                ..Default::default()
            }),
        };
        opencrab_actions::register_tools_from_config(&tools_cfg, &mut dispatcher);

        let ws_path = std::env::temp_dir().join(format!("organize-tools-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws_path).unwrap();
        let workspace = opencrab_core::workspace::Workspace::from_root(&ws_path).unwrap();

        // 整理ランと同じ caller=Owner。放置すると Owner の全ツールが届く前提を再現する。
        let ctx = opencrab_actions::ActionContext {
            caller: CallerIdentity::Owner,
            agent_id: "a1".to_string(),
            agent_name: "a1".to_string(),
            session_id: Some("sleep-organize-a1-1".to_string()),
            db: state.db.clone(),
            workspace: std::sync::Arc::new(workspace),
            last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: std::sync::Arc::new(std::sync::Mutex::new(
                opencrab_actions::RuntimeInfo {
                    default_model: "mock:test".to_string(),
                    active_model: None,
                    available_providers: vec!["mock".to_string()],
                    gateway: "sleep".to_string(),
                },
            )),
        };

        // gateway own = SystemGatewayActions（configure_* / nostr_run / spawn_subtask を own で持つ）。
        // depth 0 なので本番でもこの合成 gateway がそのまま渡る（sub-engine の絞りは通らない）。
        let system_actions: std::sync::Arc<dyn opencrab_gateway::GatewayActions> =
            std::sync::Arc::new(crate::system_actions::SystemGatewayActions::new(
                state.clone(),
                None,
                None,
                None,
            ));

        let mut bridged = opencrab_actions::BridgedExecutor::new(dispatcher, ctx)
            .with_depth(0)
            .with_gateway_actions(system_actions)
            .with_mcp_actions(std::sync::Arc::new(MockMcpSlot));
        if with_allowlist {
            bridged = bridged.with_tool_allowlist(Some(
                ORGANIZE_ALLOWED_TOOLS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ));
        }
        bridged
    }

    /// 整理ランに渡すツールセットは、記憶の読み取り・タグ操作・終了宣言だけ。
    /// **眠っている間に外へ手が出る**ツール（`execute_shell` / `nostr_run` / `spawn_subtask` /
    /// `ws_write` / `ws_delete` / `configure_*` / `update_instructions`）は 3 経路すべてで塞ぐ:
    ///   経路1: `run_allows` 相当（許可リスト定数の内容）
    ///   経路2: `list_tools`（可視性）
    ///   経路3: `dispatch`（実行）
    #[tokio::test]
    async fn organize_run_tool_allowlist_excludes_outward_tools() {
        // list_tools / execute は ActionExecutor トレイト経由。
        use opencrab_core::ActionExecutor;
        let state = crate::test_app_state();

        // 眠っている間に外へ手が出る／状態を書き換えるツール（全スロットにまたがる）。
        // nostr_run は露出撤去済み（返信は say 一本 / #840）なので外向きツール表からは外す
        // （own 定義に無く「許可リスト無しでは届くはず」の対照が成り立たない）。
        // #654: configure_nostr の定義は nostr feature 依存（#651）。off では定義が無く対照が
        // 空論になるので、期待値も同じ cfg で組む（feature off でも他の外向きツールは全経路で
        // 塞がることを引き続き固定する）。nostr off では下の push が cfg で消え mut が不要になる。
        #[cfg_attr(not(feature = "nostr"), allow(unused_mut))]
        let mut forbidden = vec![
            "execute_shell",          // dispatcher（config 駆動）
            "ws_write",               // dispatcher core
            "ws_delete",              // dispatcher core
            "update_instructions",    // dispatcher core（owner 専用の指示書書き換え）
            "spawn_subtask",          // gateway own
            "configure_llm_provider", // gateway own
            "configure_self",         // gateway own
            "configure_mcp_server",   // gateway own
            "mcp__ext__send",         // MCP スロット
        ];
        #[cfg(feature = "nostr")]
        {
            forbidden.push("configure_nostr"); // gateway own
        }
        // 整理に要る読み取り・タグ・終了宣言。
        let allowed = [
            "browse_memory_index",
            "search_memory_index",
            "retrieve_memory_nodes",
            "search_my_history",
            "tag_topic",
            "untag_topic",
            "merge_tags",
            "declare_done",
        ];

        // 経路1: 許可リスト定数そのものの内容。
        for f in forbidden.iter() {
            assert!(
                !ORGANIZE_ALLOWED_TOOLS.contains(f),
                "許可リストに外向きツール {f} が入っている"
            );
        }
        for a in allowed {
            assert!(
                ORGANIZE_ALLOWED_TOOLS.contains(&a),
                "許可リストに {a} が無い（整理に必要）"
            );
        }

        // --- 対照: 許可リスト無し（None）なら Owner の全ツールが届く（危険の再現） ---
        let unrestricted = build_organize_executor(&state, false);
        // #923: allowlist の可視性は narrowing 前の policy＋allowlist 層で検証する（list_tools は
        // depth0 で常時集合に絞るため、allowlist 契約は effective_tool_definitions で見る）。
        let base: Vec<String> = unrestricted
            .effective_tool_definitions()
            .into_iter()
            .map(|t| t.definition.name)
            .collect();
        for f in forbidden.iter() {
            assert!(
                base.contains(&f.to_string()),
                "許可リスト無しでは {f} が届くはず（許可リストが効いている証跡の対照）: {base:?}"
            );
        }

        // --- 整理ラン（許可リスト有り） ---
        let executor = build_organize_executor(&state, true);

        // 経路2: 可視性（policy＋allowlist 層）。許可外は 1 つも出ない。
        let visible: Vec<String> = executor
            .effective_tool_definitions()
            .into_iter()
            .map(|t| t.definition.name)
            .collect();
        for f in forbidden.iter() {
            assert!(
                !visible.contains(&f.to_string()),
                "整理ランの list_tools に外向きツール {f} が出ている: {visible:?}"
            );
        }
        for a in allowed {
            assert!(
                visible.contains(&a.to_string()),
                "整理ランの list_tools に {a} が無い: {visible:?}"
            );
        }

        // 経路3: dispatch（実行）。許可外は構造的拒否で、実装へは届かない。
        for f in forbidden.iter().copied() {
            let r = executor.execute(f, &serde_json::json!({})).await;
            assert!(!r.success, "整理ランで {f} の実行が成功してはならない");
            let err = r.error.unwrap_or_default();
            assert!(
                err.starts_with(opencrab_actions::REJECTION_CODE_PREFIX),
                "整理ランで {f} は構造的拒否であるべき: {err}"
            );
            assert!(
                r.data.get("reached").is_none(),
                "整理ランで {f} が実装へ届いてはならない"
            );
        }

        // 実測の記録（推定でなく実際に受け取るツール名。テスト出力に残す）。
        let mut dump = visible.clone();
        dump.sort();
        eprintln!("[#368] 整理ランが実際に受け取るツール: {dump:?}");
    }

    // --- 本番のラン構築を通さない全経路テスト（#370）---
    //
    // `run_organize` は `AppState` を受け取らず、1 ターンを回す口（`OrganizeTurnRunner`）だけを
    // 外から受ける。テストは結果を差し替えるフェイクを渡す。フェイクは `run_agent_response` を
    // 呼ばないので、webhook も gateway も MCP も LLM も**一切構築されない**（構造的に sleep の依存に
    // 入らない = 隔離実験が本番へ飛んだ #370 の再発を症状封じでなく構造で防ぐ）。ゲート → 実行 →
    // clean/partial 判定 → マーカー前進/据え置き → 監査、までを LLM ゼロコールで検証する。

    enum FakeOutcome {
        Completed,
        StoppedByLimit,
        Error,
    }

    /// テスト用の [`OrganizeTurnRunner`]。受け取った `RunRequest` の要点を記録し、設定した結果を
    /// 返すだけ。何も構築しない（外向きの口は一切現れない）。
    struct FakeRunner {
        outcome: FakeOutcome,
        calls: AtomicUsize,
        captured: std::sync::Mutex<Option<CapturedReq>>,
    }

    /// フェイクが観測した `RunRequest` の要点（本番配線が保たれているかの検証用）。
    struct CapturedReq {
        gateway: String,
        caller_is_owner: bool,
        tool_allowlist: Option<Vec<String>>,
        has_gateway_actions: bool,
        persist_turn_logs: bool,
    }

    impl FakeRunner {
        fn new(outcome: FakeOutcome) -> Self {
            Self {
                outcome,
                calls: AtomicUsize::new(0),
                captured: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl OrganizeTurnRunner for FakeRunner {
        async fn run_turn(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.captured.lock().unwrap() = Some(CapturedReq {
                gateway: req.gateway.clone(),
                caller_is_owner: matches!(req.caller, CallerIdentity::Owner),
                tool_allowlist: req.tool_allowlist.clone(),
                has_gateway_actions: req.gateway_actions.is_some(),
                persist_turn_logs: req.persist_turn_logs,
            });
            match self.outcome {
                FakeOutcome::Completed => Ok(engine_result(false)),
                FakeOutcome::StoppedByLimit => Ok(engine_result(true)),
                FakeOutcome::Error => Err(anyhow::anyhow!("simulated run failure")),
            }
        }
    }

