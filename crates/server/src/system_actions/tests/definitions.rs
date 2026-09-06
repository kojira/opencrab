use super::super::*;
use super::support::*;

#[test]
fn own_definition_shape() {
    let defs = SystemGatewayActions::own_definitions();
    let d = defs
        .iter()
        .find(|d| d.name == "configure_llm_provider")
        .expect("configure_llm_provider must be defined");
    // provider は必須。
    let required = d.parameters["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "provider"));
    // 秘密情報 api_key は LLM ツールでは露出しない（ダッシュボード専用）。
    let props = d.parameters["properties"].as_object().unwrap();
    assert!(
        !props.contains_key("api_key"),
        "api_key must not be settable via the agent tool"
    );
    // 起動系フィールドは受け付ける。
    for key in ["binary_path", "args", "working_dir", "timeout_secs"] {
        assert!(props.contains_key(key), "missing property: {key}");
    }
}

/// Regression guard for #146: nostr_generate_key must be a *own* definition
/// (bootstrap tool) so it is exposed on every turn regardless of whether the
/// nostr watch loop / keys are configured. If someone moves it back into the
/// key-gated inner NostrGatewayActions bundle, own_definitions loses it and
/// this test fails — that is the "露出が二度と消えない" guard.
// #654: nostr ツール定義は #651 で nostr feature 依存になった。feature off では定義自体が
// 存在せず「常時露出」が空論になるので、同じ cfg で囲む（#630 の外した構成でのテスト経路）。
#[cfg(feature = "nostr")]
#[test]
fn nostr_generate_key_is_always_exposed() {
    let defs = SystemGatewayActions::own_definitions();
    let d = defs
        .iter()
        .find(|d| d.name == "nostr_generate_key")
        .expect("nostr_generate_key must be an own (always-exposed) definition (#146)");
    // vanity 用の任意 prefix パラメータを受ける。
    let props = d.parameters["properties"].as_object().unwrap();
    assert!(
        props.contains_key("prefix"),
        "nostr_generate_key must accept an optional vanity `prefix`"
    );
    // bootstrap ツールは required なし（引数なしでも鍵を作れる）。
    assert!(
        d.parameters.get("required").is_none(),
        "nostr_generate_key must not require any argument"
    );
}

/// #264: nostr_list_keys must also be a *own* (bootstrap) definition so the
/// agent can inspect its generated keys before adopting one, even when no
/// nostr gateway is running / no key is configured. It must not require args
/// and must not leak nsec (it only returns npubs).
// #654: nostr 定義は nostr feature 依存（#651）。off では定義が無いので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[test]
fn nostr_list_keys_is_always_exposed() {
    let defs = SystemGatewayActions::own_definitions();
    let d = defs
        .iter()
        .find(|d| d.name == "nostr_list_keys")
        .expect("nostr_list_keys must be an own (always-exposed) definition (#264)");
    assert!(
        d.parameters.get("required").is_none(),
        "nostr_list_keys must not require any argument"
    );
}

/// #264: nostr_switch_identity must be a *own* (bootstrap) definition so an
/// unconfigured agent can adopt a generated key and self-connect on any turn
/// (not only when a nostr watch loop is already running). It requires `npub`.
// #654: nostr 定義は nostr feature 依存（#651）。off では定義が無いので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[test]
fn nostr_switch_identity_is_always_exposed() {
    let defs = SystemGatewayActions::own_definitions();
    let d = defs
        .iter()
        .find(|d| d.name == "nostr_switch_identity")
        .expect("nostr_switch_identity must be an own (always-exposed) definition (#264)");
    let required = d.parameters["required"].as_array().unwrap();
    assert!(
        required.iter().any(|v| v == "npub"),
        "nostr_switch_identity must require npub"
    );
}

/// definitions() dedups own vs inner by name: when the inner gateway also
/// defines nostr_generate_key (nostr watch loop running), the merged tool
/// list must still contain exactly one entry (providers reject duplicates).
// #654: nostr_generate_key の定義は nostr feature 依存（#651）。off では 0 件になり
// 「重複せず 1 件」の不変条件が空論になるので、同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[test]
fn definitions_dedup_keeps_single_nostr_generate_key() {
    // own_definitions is the source that definitions() starts from; assert it
    // is unique there so the dedup contract holds.
    let defs = SystemGatewayActions::own_definitions();
    let count = defs
        .iter()
        .filter(|d| d.name == "nostr_generate_key")
        .count();
    assert_eq!(
        count, 1,
        "nostr_generate_key must be defined exactly once in own_definitions"
    );
}

/// Regression guard for #161: cancel_subtask must be an *own* (server-neutral)
/// definition so web / Nostr / REST — not just Discord — expose the tool to
/// stop auto-dispatched subtasks. If it is removed from own_definitions the
/// tool disappears on every non-Discord transport again — that is the bug this
/// guards against.
#[test]
fn cancel_subtask_is_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    let d = defs
        .iter()
        .find(|d| d.name == "cancel_subtask")
        .expect("cancel_subtask must be an own (server-neutral) definition (#161)");
    // subtask_id は必須。
    let required = d.parameters["required"].as_array().unwrap();
    assert!(
        required.iter().any(|v| v == "subtask_id"),
        "cancel_subtask must require subtask_id"
    );
    // own に丁度1件（dedup の source が一意）。
    let count = defs.iter().filter(|d| d.name == "cancel_subtask").count();
    assert_eq!(
        count, 1,
        "cancel_subtask must be defined exactly once in own_definitions"
    );
}

/// **README の gateway アクション表が実装と一致する**。
///
/// 「## Action System」節の**前半**（core アクション表）には
/// `readme_action_table_matches_the_dispatcher`（`crates/actions/src/dispatcher.rs`）が
/// あるが、**後半の gateway 表には同じ検査が無かった**。その結果、実装だけが進んで
/// 表が 7 個（`nostr_list_keys` / `nostr_switch_identity` / `nostr_run` /
/// `get_my_nostr_relay` / `set_my_nostr_relay` / `get_my_heartbeat` /
/// `set_my_heartbeat`）を落としたまま誰も気付かず、README は「Config 行に
/// `nostr_generate_key` だけ」という状態で残っていた。分類の網羅性検査と同じく
/// **実装（`own_definitions()`）を起点に**走査し、両方向を要求する: ツールを足したら
/// README に書くまで落ち（漏れ）、README から消しても落ちる（死名）。
///
/// 対象は露出範囲の列が「all turns」で始まる行だけ。transport 固有の行
/// （`Discord turns only` / `Nostr turns only`）は**どのテストも README と
/// 突き合わせていない**（`test_definitions_returns_expected_count` 等は定数と
/// `definitions()` のドリフトを見るだけで、README の行は見ない）。つまりこの表の
/// transport 行は今も無検査で腐りうる。埋めるなら transport 側にも同型のテストを
/// 足すこと。
// #654: README の gateway アクション表「all turns」行は nostr ツール（nostr_generate_key /
// nostr_list_keys / nostr_switch_identity / nostr_run / get/set_my_nostr_relay …）を載せている。
// own_definitions() の feature 依存部分は nostr のみ（#651）なので、README（全部入りの正典）と
// own の突き合わせが成立するのは nostr feature 時だけ。off では documented ⊄ registered になる
// ので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[test]
fn server_gateway_action_table_matches_own_definitions() {
    let readme_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md");
    let readme = std::fs::read_to_string(readme_path)
        .unwrap_or_else(|e| panic!("README.md を読めない ({readme_path}): {e}"));

    // 「## Action System」節のうち、gateway アクション表の導入文より後だけを見る
    // （それより前は core アクション表 = dispatcher 側の検査の担当）。
    let table = readme
        .split("## Action System")
        .nth(1)
        .expect("README に '## Action System' 節が無い")
        .split("\n## ")
        .next()
        .unwrap()
        .split("In addition, **gateway actions**")
        .nth(1)
        .expect("README の Action System 節に gateway アクション表の導入文が無い");

    // 表の行 `| **Category** | `a`, `b` | 露出範囲 |` から、露出範囲が「all turns」で
    // 始まる行の 2 列目だけを拾う。
    let mut documented: Vec<String> = Vec::new();
    for line in table.lines().filter(|l| l.starts_with("| **")) {
        let cols: Vec<&str> = line.split('|').collect();
        let (Some(actions_col), Some(available_col)) = (cols.get(2), cols.get(3)) else {
            continue;
        };
        if !available_col.trim_start().starts_with("all turns") {
            continue;
        }
        for part in actions_col.split('`').skip(1).step_by(2) {
            documented.push(part.to_string());
        }
    }
    documented.sort();
    documented.dedup();
    assert!(
        !documented.is_empty(),
        "README の gateway アクション表から「all turns」行のツール名を 1 つも拾えていない\
             （表の形を変えたならこのパーサも直すこと）"
    );

    let mut registered: Vec<String> = SystemGatewayActions::own_definitions()
        .into_iter()
        .map(|d| d.name)
        .collect();
    registered.sort();

    let missing: Vec<&String> = registered
        .iter()
        .filter(|n| !documented.contains(n))
        .collect();
    assert!(
        missing.is_empty(),
        "README の gateway アクション表に載っていない own 定義: {missing:?}\n\
             （SystemGatewayActions にツールを足したら README の表にも足すこと）"
    );

    let dead: Vec<&String> = documented
        .iter()
        .filter(|n| !registered.contains(n))
        .collect();
    assert!(
        dead.is_empty(),
        "README の gateway アクション表の「all turns」行が own 定義に無い名前を載せている: \
             {dead:?}\n（transport 固有のツールなら露出範囲の列を \
             `Discord turns only` / `Nostr turns only` のように書くこと）"
    );
}

/// **分類属性の集合を固定する**（不変条件）。
///
/// 分類の権威は各ツール定義の属性（`GatewayActionDef.class`）へ移った（PR-2B）ので、
/// gateway 固有の権威リストは削除した。ここでは権威リストに依存しない不変条件を
/// `own_definitions()` の属性から直接固定する（3 軸とも「値を書き間違えたら落ちる」状態に
/// する）:
/// - **Dispatchable 集合 == {nostr_generate_key, rebuild_memory_index,
///   update_memory_index_config, update_heartbeat_instructions, create_skill}**（長時間 or
///   同ターンで読み戻さない書き込み。他は全部 `Inline`。`nostr_generate_key` は nostr
///   feature 時のみ push されるので期待値も同じ feature 条件で組む / PR-1B）。
/// - **Allowed 集合 == {report_progress, nostr_generate_key}**（sub-engine から到達可能な
///   ツールが増えていないことの固定。`nostr_generate_key` は nostr feature 時のみ push / PR-1B）。
/// - **ConversationBound == {send_ui}**（server own で live セッションに束縛される唯一の
///   ツール。全ゲート横断では {discord_add_reaction, nostr_reply, send_ui} で残り 2 つは
///   discord / nostr 側の同名テストが覆う）。
///
/// `sub_engine == Blocked`（配送系の深さ拒否）は `send_ui_is_blocked_in_sub_engine` 等が
/// 挙動で覆うのでここでは固定しない。
#[test]
fn server_tool_class_invariants_are_fixed() {
    use opencrab_gateway::{DispatchMode, SubEngineAccess, ToolSharing};
    let defs = SystemGatewayActions::own_definitions();
    assert!(!defs.is_empty());
    let dispatchable: std::collections::BTreeSet<String> = defs
        .iter()
        .filter(|d| d.class.dispatch == DispatchMode::Dispatchable)
        .map(|d| d.name.clone())
        .collect();
    let allowed: std::collections::BTreeSet<String> = defs
        .iter()
        .filter(|d| d.class.sub_engine == SubEngineAccess::Allowed)
        .map(|d| d.name.clone())
        .collect();
    let conv_bound: std::collections::BTreeSet<String> = defs
        .iter()
        .filter(|d| d.class.sharing == ToolSharing::ConversationBound)
        .map(|d| d.name.clone())
        .collect();

    // Dispatchable（長時間 / 同ターンで読み戻さない書き込み）。`nostr_generate_key` のみ
    // nostr feature に依存する（PR-1B）ので期待値も同じ cfg で組む。
    // #654: nostr off では下の insert が cfg で消え mut が不要になる。
    #[cfg_attr(not(feature = "nostr"), allow(unused_mut))]
    let mut expected_dispatch: std::collections::BTreeSet<String> = [
        "rebuild_memory_index",
        "update_memory_index_config",
        "update_heartbeat_instructions",
        "create_skill",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    #[cfg(feature = "nostr")]
    expected_dispatch.insert("nostr_generate_key".to_string());
    assert_eq!(
        dispatchable, expected_dispatch,
        "server own の Dispatchable 集合がずれている（dispatch 属性の Inline/Dispatchable 取り違え）"
    );

    let expected_conv: std::collections::BTreeSet<String> =
        std::iter::once("send_ui".to_string()).collect();
    assert_eq!(
        conv_bound, expected_conv,
        "server own の ConversationBound 集合がずれている（sharing 属性の付け忘れ/誤り）"
    );

    // 【段階1(#651) の feature 化との相互作用】`nostr_generate_key` の def は
    // `#[cfg(feature = "nostr")]` に囲まれている（PR-1B）。よって `own_definitions()` から
    // 集めた Allowed 集合は nostr 構成の有無で縮む。期待値も **同じ feature 条件**で組む。
    // #654: nostr off では下の insert が cfg で消え mut が不要になる。
    #[cfg_attr(not(feature = "nostr"), allow(unused_mut))]
    let mut expected: std::collections::BTreeSet<String> =
        std::iter::once("report_progress".to_string()).collect();
    #[cfg(feature = "nostr")]
    expected.insert("nostr_generate_key".to_string());
    assert_eq!(
        allowed, expected,
        "server own_definitions の Allowed 集合が期待値（feature 条件込み）と一致しない"
    );
}

/// [P1 回帰] 設定変更ツールは inline（同ターンで結果を返す）。長時間の鍵探索・記憶
/// インデックス設定の書き込みだけが background。分類の権威は各定義の属性なので
/// `own_definitions()` の `class.dispatch` を直接見る。
#[test]
fn config_tools_are_inline_and_key_generation_is_dispatched() {
    use opencrab_gateway::DispatchMode;
    let defs = SystemGatewayActions::own_definitions();
    let class_of = |name: &str| {
        defs.iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("{name} が own_definitions() に無い"))
            .class
    };
    for name in [
        "configure_llm_provider",
        "manage_allowed_commands",
        "configure_self",
        "configure_mcp_server",
        "cancel_subtask",
        // #157 S1 で Discord から移設。分類の所属（inline）は移設前と同じ。
        "list_allowed_commands",
        "add_allowed_command",
        "remove_allowed_command",
        // #157 S3 で Discord から移設（読み出し = inline）。
        "read_heartbeat_instructions",
    ] {
        assert_eq!(
            class_of(name).dispatch,
            DispatchMode::Inline,
            "{name} は background 化してはならない（設定の共有状態書き込み / 一覧の即答）"
        );
    }
    // `configure_nostr` の def は nostr feature 時のみ push される（PR-1B）。
    #[cfg(feature = "nostr")]
    assert_eq!(
        class_of("configure_nostr").dispatch,
        DispatchMode::Inline,
        "configure_nostr は background 化してはならない（設定の共有状態書き込み）"
    );
    // 長時間 / 同ターンで読み戻さない書き込みは dispatch 対象に残す。
    for name in [
        "update_memory_index_config",
        "update_heartbeat_instructions",
    ] {
        assert_eq!(
            class_of(name).dispatch,
            DispatchMode::Dispatchable,
            "{name} は dispatch 対象に残す（同ターンで読み戻さない書き込み）"
        );
    }
    #[cfg(feature = "nostr")]
    assert_eq!(
        class_of("nostr_generate_key").dispatch,
        DispatchMode::Dispatchable,
        "nostr_generate_key は長時間の vanity 探索なので dispatch 対象に残す"
    );
}

/// #161: Discord のような inner が cancel_subtask を定義しても、merge 後は
/// own の1件だけが残る（providers は同名重複を拒否しうる）。merge_definitions を
/// 直接叩くことで AppState 無しに実コードの dedup 契約を検証する。
#[test]
fn merge_definitions_dedups_cancel_subtask_from_inner() {
    use opencrab_gateway::{GatewayActionResult, GatewayCallContext};

    /// cancel_subtask と固有ツールを定義する Discord 風 inner モック。
    struct InnerWithCancel;
    #[async_trait]
    impl GatewayActions for InnerWithCancel {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![
                GatewayActionDef {
                    name: "cancel_subtask".to_string(),
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Inline,
                        sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
                    description: "discord cancel".to_string(),
                    parameters: json!({"type": "object"}),
                },
                GatewayActionDef {
                    name: "discord_only_tool".to_string(),
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Inline,
                        sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
                    description: "x".to_string(),
                    parameters: json!({"type": "object"}),
                },
            ]
        }
        async fn execute(
            &self,
            _name: &str,
            _args: &Value,
            _ctx: &GatewayCallContext,
        ) -> GatewayActionResult {
            GatewayActionResult {
                success: true,
                data: None,
                error: None,
            }
        }
    }

    let inner: Arc<dyn GatewayActions> = Arc::new(InnerWithCancel);
    let merged = SystemGatewayActions::merge_definitions(
        SystemGatewayActions::own_definitions(),
        Some(&inner),
    );
    let cancel_count = merged.iter().filter(|d| d.name == "cancel_subtask").count();
    assert_eq!(
        cancel_count, 1,
        "merge 後も cancel_subtask は1件（own 優先で dedup）"
    );
    // inner 固有ツールは通す（dedup は同名のみ）。
    assert!(merged.iter().any(|d| d.name == "discord_only_tool"));
}

// ---- #175 S1: report_progress の gateway 非依存化 ----

/// Regression guard for #175 S1: report_progress must be an *own* (server-neutral)
/// definition so web / Nostr / REST / heartbeat — not just Discord — can let a
/// sub-engine report progress.
#[test]
fn report_progress_is_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    let d = defs
        .iter()
        .find(|d| d.name == "report_progress")
        .expect("report_progress must be an own (server-neutral) definition (#175 S1)");
    // message は必須 / subtask_id は任意（sub-engine の system prompt の契約）。
    let required = d.parameters["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "message"));
    assert!(!required.iter().any(|v| v == "subtask_id"));
    let props = d.parameters["properties"].as_object().unwrap();
    assert!(props.contains_key("subtask_id"));
    // own に丁度 1 件（dedup の source が一意）。
    let count = defs.iter().filter(|d| d.name == "report_progress").count();
    assert_eq!(count, 1);
}

// ---- #175 S4: spawn_subtask の gateway 非依存化 ----

/// Regression guard for #175 S4: `spawn_subtask` は *own*（server-neutral）定義。
/// これが own から消えると、web / REST / Nostr / heartbeat でサブタスクを起動できなく
/// なり、Discord だけの機能に逆戻りする。
#[test]
fn spawn_subtask_is_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    let d = defs
        .iter()
        .find(|d| d.name == "spawn_subtask")
        .expect("spawn_subtask must be an own (server-neutral) definition (#175 S4)");
    let required = d.parameters["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "task"));
    let props = d.parameters["properties"].as_object().unwrap();
    for key in ["task", "timeout_secs", "label", "webhook"] {
        assert!(props.contains_key(key), "missing property: {key}");
    }
    assert_eq!(defs.iter().filter(|d| d.name == "spawn_subtask").count(), 1);
}

/// Regression guard for #175 S4 / #155: `rebuild_memory_index` も own 定義
/// （LLM クライアントを要する唯一の Discord ツールだった）。
#[test]
fn rebuild_memory_index_is_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    assert!(
        defs.iter().any(|d| d.name == "rebuild_memory_index"),
        "rebuild_memory_index must be an own definition (#175 S4)"
    );
}

/// **サブタスクのネスト禁止**（壊すと重大）。
///
/// sub-engine の実効ゲートは bridge の MAX_DEPTH ではなく、各ツール定義の
/// `class.sub_engine == Allowed` 属性（`SubEngineGatewayActions` が最外周で絞る）。
/// `spawn_subtask` が server-neutral 層へ移った今、うっかり `Allowed` を名乗らせると
/// サブタスクが無限にネストできてしまう。合成 gateway（own + inner）を
/// `SubEngineGatewayActions` で包んだ結果を直接固定する。
#[test]
fn sub_engine_cannot_see_spawn_subtask() {
    let state = crate::test_app_state();
    let composite: Arc<dyn GatewayActions> =
        Arc::new(SystemGatewayActions::new(state, None, None, None));
    let sub = opencrab_actions::SubEngineGatewayActions::new(composite);
    let names: Vec<String> = sub.definitions().into_iter().map(|d| d.name).collect();
    assert!(
        !names.contains(&"spawn_subtask".to_string()),
        "sub-engine から spawn_subtask が見えてはならない（ネスト禁止）: {names:?}"
    );
    // 許可された制御ツールは見える（許可リストが空振りしていないことの対）。
    assert!(names.contains(&"report_progress".to_string()));
    // #654: nostr_generate_key の定義は nostr feature 依存（#651）。off では露出しないので
    // 期待値も同じ cfg で組む（sub-engine 許可リストの対照そのものは report_progress で担保）。
    #[cfg(feature = "nostr")]
    assert!(names.contains(&"nostr_generate_key".to_string()));
}

/// sub-engine から `spawn_subtask` を名前指定で呼んでも拒否される
/// （定義から隠すだけでは、親コンテキストの記憶で名前を呼ばれると素通しになる）。
#[tokio::test]
async fn sub_engine_execution_of_spawn_subtask_is_rejected() {
    let state = crate::test_app_state();
    let composite: Arc<dyn GatewayActions> =
        Arc::new(SystemGatewayActions::new(state, None, None, None));
    let sub = opencrab_actions::SubEngineGatewayActions::new(composite);
    let r = sub
        .execute(
            "spawn_subtask",
            &json!({ "task": "nested" }),
            &sub_ctx("subtask-st-1"),
        )
        .await;
    assert!(!r.success);
    assert!(
        r.error.unwrap().starts_with(REJECTION_CODE_PREFIX),
        "許可外の実在ツールは権限拒否として返す"
    );
}

/// `report_progress` は随伴マップの通知口へ進捗を渡す（#175 S4 で Discord 実装から
/// 引き継いだ配線）。落とすと lifecycle webhook から進捗が黙って消える。
#[tokio::test]
async fn report_progress_notifies_the_run_notifier() {
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<String>>);
    impl opencrab_actions::subtask_notify::SubtaskRunNotifier for Recorder {
        fn on_progress(&self, detail: &str) {
            self.0.lock().unwrap().push(detail.to_string());
        }
    }

    let state = crate::test_app_state();
    let recorder = Arc::new(Recorder::default());
    state
        .subtask_notifiers
        .insert("st-1".to_string(), recorder.clone());
    let registry = registry_with("st-1", "subtask-st-1", "web-parent-1");
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);

    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "halfway there" }),
            &sub_ctx("subtask-st-1"),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(recorder.0.lock().unwrap().clone(), vec!["halfway there"]);
}

/// Discord のような inner が report_progress を定義しても、merge 後は own の
/// 1 件だけが残る（provider は同名重複を拒否しうる）。
#[test]
fn merge_definitions_dedups_report_progress_from_inner() {
    let inner: Arc<dyn GatewayActions> = Arc::new(RecordingInner::new(&["report_progress"]));
    let merged = SystemGatewayActions::merge_definitions(
        SystemGatewayActions::own_definitions(),
        Some(&inner),
    );
    let count = merged
        .iter()
        .filter(|d| d.name == "report_progress")
        .count();
    assert_eq!(
        count, 1,
        "merge 後も report_progress は1件（own 優先で dedup）"
    );
}
