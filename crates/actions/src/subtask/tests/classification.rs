pub(super) mod cases {
    use super::super::dispatcher::cases::FakeExecutor;
    use super::super::sink::cases::RecordingSink;
    use super::super::*;

    /// [P1 回帰] run 内共有状態を書く `select_llm` は dispatch しない（inline のまま）。
    #[test]
    fn select_llm_is_not_dispatched() {
        let set = default_non_dispatch_tools();
        assert!(
            set.contains("select_llm"),
            "select_llm は run 内共有状態（model_override）を書くため inline に残す"
        );
    }

    /// [P1 回帰 / fail-closed] `ActionDispatcher` の core アクション**全名**が
    /// inline / dispatch のどちらかに分類されている（#152）。
    ///
    /// これが無かった頃は core 32 個が分類ガードの外にあり全 dispatch されていた:
    /// 記憶想起フロー（`search_memory_index` → `retrieve_memory_nodes`）が背景往復
    /// 2 回 = ユーザーへ 4 通、`open_task` は task_id の代わりに `spawned` が返る、
    /// という壊れ方をしていた。既存の `pure_read_tools_are_not_dispatched` は Discord
    /// gateway の読み取りしか見ないので検知できなかった。
    #[test]
    fn core_actions_are_classified_for_dispatch() {
        let names = crate::dispatcher::ActionDispatcher::new().action_names();
        assert!(
            !names.is_empty(),
            "core アクションが 1 つも登録されていない"
        );

        for name in &names {
            let inline = crate::bridge::CORE_INLINE_ACTIONS.contains(&name.as_str());
            let dispatchable = crate::bridge::CORE_DISPATCHABLE_ACTIONS.contains(&name.as_str());
            assert!(
                inline ^ dispatchable,
                "core アクション {name} が未分類（または両方に居る）。\
                 新しいアクションを登録したら CORE_INLINE_ACTIONS か \
                 CORE_DISPATCHABLE_ACTIONS のどちらかへ入れること（判定基準は \
                 default_non_dispatch_tools の doc / docs/DESIGN.md）"
            );
        }
        // 死名検出: 一覧側に実在しない名前を残さない（空振りする分類を防ぐ）。
        for name in crate::bridge::CORE_INLINE_ACTIONS {
            assert!(
                names.contains(&(*name).to_string()),
                "CORE_INLINE_ACTIONS の {name} が ActionDispatcher に無い（死名）"
            );
        }
        for name in crate::bridge::CORE_DISPATCHABLE_ACTIONS {
            assert!(
                names.contains(&(*name).to_string()),
                "CORE_DISPATCHABLE_ACTIONS の {name} が ActionDispatcher に無い（死名）"
            );
        }
        assert_eq!(
            names.len(),
            crate::bridge::CORE_INLINE_ACTIONS.len()
                + crate::bridge::CORE_DISPATCHABLE_ACTIONS.len(),
            "分類の総数が登録アクション数と一致しない"
        );

        // 分類が実際に効いている（除外集合へ反映されている）。
        let non_dispatch = default_non_dispatch_tools();
        for name in crate::bridge::CORE_INLINE_ACTIONS {
            assert!(
                non_dispatch.contains(*name),
                "{name} は inline 分類なのに dispatch されてしまう"
            );
        }
        for name in crate::bridge::CORE_DISPATCHABLE_ACTIONS {
            assert!(
                !non_dispatch.contains(*name),
                "{name} は dispatch 可分類なのに inline 集合に居る"
            );
        }
    }

    /// [P1 回帰] system prompt が指示する記憶想起フローと台帳の同ターン連鎖は inline。
    /// dispatch されると 1 質問が複数ターン・複数メッセージに割れる。
    #[test]
    fn memory_recall_and_task_ledger_tools_are_inline() {
        let set = default_non_dispatch_tools();
        for name in [
            // 記憶想起（2 段連鎖）。
            "search_memory_index",
            "retrieve_memory_nodes",
            "browse_memory_index",
            // 純粋読み取り。
            "ws_read",
            "ws_list",
            "get_task",
            "read_skill",
            "get_system_info",
            // 同ターン結果依存（戻り値の task_id を後続で使う）。
            "open_task",
        ] {
            assert!(
                set.contains(name),
                "{name} は inline でなければならない（分類基準 3/5）"
            );
        }
    }

    /// [P1 回帰] MCP ツール（`mcp__*`）は既定 inline。運用者が繋いだ任意ツールの性質
    /// （配送系か / 同ターン結果依存か）は静的に分類できないため安全側に倒す。
    #[tokio::test]
    async fn mcp_tools_are_not_dispatched_by_default() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: true });
        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry,
            db,
            sink,
            "agent-a",
            "web-agent-a-conv1",
        );

        assert!(!dispatcher.should_dispatch("mcp__slack__post_message"));
        assert!(!dispatcher.should_dispatch("mcp__anything__at_all"));
        // 非 MCP の dispatch 可ツールは従来どおり dispatch される。
        assert!(dispatcher.should_dispatch("execute_shell"));
        assert!(dispatcher.should_dispatch("ws_write"));
    }

    /// core 分類集合の内部整合性（#152）。
    ///
    /// gateway / MCP のツールは分類の権威が各定義の `class.dispatch` 属性へ移った（PR-2B）
    /// ので、ここでは `GatewayActionDef` を持たない **core だけ**を見る:
    /// - inline 集合（[`crate::bridge::CORE_INLINE_ACTIONS`]）と dispatch 可リスト
    ///   （[`crate::bridge::CORE_DISPATCHABLE_ACTIONS`]）は互いに素。
    /// - inline 集合は重複を含まない（一覧の手編集で二重に足す事故の検出）。
    #[test]
    fn dispatch_classification_sets_are_consistent() {
        let non_dispatch = default_non_dispatch_tools();
        for name in crate::bridge::CORE_DISPATCHABLE_ACTIONS {
            assert!(
                !non_dispatch.contains(*name),
                "{name} が dispatch 可リストと inline 集合の両方に居る"
            );
        }
        let unique: HashSet<&&str> = crate::bridge::CORE_INLINE_ACTIONS.iter().collect();
        assert_eq!(
            unique.len(),
            crate::bridge::CORE_INLINE_ACTIONS.len(),
            "CORE_INLINE_ACTIONS に重複がある"
        );
    }

    /// 非同期化除外の権威は各ツール定義の `class.dispatch` 属性であり、
    /// `BridgedExecutor::inline_tool_names` が索引から `dispatch == Inline` を集める。
    ///
    /// `send_ui` のような配送系 inline ツールは inline_tool_names に載り（＝ dispatch
    /// されない）、`nostr_generate_key` のような Dispatchable ツールは載らないことを、
    /// mock gateway の実属性で end-to-end に確認する（`default_non_dispatch_tools()` は
    /// 縮小され gateway ツールを含まなくなったので、この plumbing がその代替）。
    #[test]
    fn inline_tool_names_reads_gateway_dispatch_attribute() {
        use crate::bridge::BridgedExecutor;
        use crate::dispatcher::ActionDispatcher;
        use crate::traits::ActionContext;
        use opencrab_gateway::{
            DispatchMode, GatewayActionDef, GatewayActionResult, GatewayActions,
            GatewayCallContext, SubEngineAccess, ToolClass, ToolSharing,
        };

        struct MockGw;
        #[async_trait::async_trait]
        impl GatewayActions for MockGw {
            fn definitions(&self) -> Vec<GatewayActionDef> {
                let mk = |name: &str, dispatch: DispatchMode| GatewayActionDef {
                    name: name.to_string(),
                    class: ToolClass {
                        dispatch,
                        sub_engine: SubEngineAccess::NotExposed,
                        sharing: ToolSharing::AgentBound,
                    },
                    description: name.to_string(),
                    parameters: serde_json::json!({"type":"object"}),
                };
                vec![
                    mk("send_ui", DispatchMode::Inline),
                    mk("configure_llm_provider", DispatchMode::Inline),
                    mk("nostr_generate_key", DispatchMode::Dispatchable),
                ]
            }
            async fn execute(
                &self,
                _name: &str,
                _args: &serde_json::Value,
                _ctx: &GatewayCallContext,
            ) -> GatewayActionResult {
                GatewayActionResult {
                    success: true,
                    data: None,
                    error: None,
                }
            }
        }

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            caller: CallerIdentity::Agent,
            agent_id: "agent-x".to_string(),
            agent_name: "X".to_string(),
            session_id: Some("web-agent-x-c1".to_string()),
            db: db.clone(),
            workspace: Arc::new(ws),
            last_metrics_id: Arc::new(Mutex::new(None)),
            model_override: Arc::new(Mutex::new(None)),
            current_purpose: Arc::new(Mutex::new("conversation".to_string())),
            runtime_info: Arc::new(Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test".to_string(),
                active_model: None,
                available_providers: vec![],
                gateway: "web".to_string(),
            })),
        };
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGw));
        let inline = executor.inline_tool_names();
        // 配送系 / 設定系（Inline 属性）は非同期化しない。
        assert!(inline.contains("send_ui"));
        assert!(inline.contains("configure_llm_provider"));
        // 制御ツール ＋ core inline は常に含まれる（default_non_dispatch_tools 由来）。
        assert!(inline.contains("spawn_subtask"));
        assert!(inline.contains("declare_done"));
        // 長時間の鍵探索（Dispatchable 属性）は含まれない = dispatch 対象。
        assert!(!inline.contains("nostr_generate_key"));
    }
}
