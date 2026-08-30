use super::*;
// 本体は `is_owner_equivalent()` / `trust_level()` 経由でしか caller を見ないので、
// 列挙子そのものを組み立てるのはテストだけ。本体側の `use` に混ぜると未使用警告になる。
use opencrab_gateway::GatewayCaller;

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

/// 指定した名前のツールを定義し、`execute` の到達を記録する inner のフェイク。
struct RecordingInner {
    names: Vec<String>,
    calls: std::sync::Mutex<Vec<String>>,
}

impl RecordingInner {
    fn new(names: &[&str]) -> Self {
        Self {
            names: names.iter().map(|s| s.to_string()).collect(),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl GatewayActions for RecordingInner {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        self.names
            .iter()
            .map(|n| GatewayActionDef {
                name: n.clone(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: format!("{n} (inner)"),
                parameters: json!({"type": "object"}),
            })
            .collect()
    }
    async fn execute(
        &self,
        name: &str,
        _args: &Value,
        _ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        self.calls.lock().unwrap().push(name.to_string());
        GatewayActionResult {
            success: true,
            data: Some(json!({ "reached_inner": name })),
            error: None,
        }
    }
}

/// 受け取った settle を記録する `SubtaskCompletionSink`。
#[derive(Default)]
struct RecordingSink {
    settled: std::sync::Mutex<Vec<SubtaskSettled>>,
}

impl RecordingSink {
    fn settled(&self) -> Vec<SubtaskSettled> {
        self.settled.lock().unwrap().clone()
    }
}

impl SubtaskCompletionSink for RecordingSink {
    fn session_prefix(&self) -> &'static str {
        ""
    }
    fn forwards_progress(&self) -> bool {
        true
    }
    fn deliver_continuation(&self, ev: SubtaskSettled) {
        self.settled.lock().unwrap().push(ev);
    }
}

/// 走行中扱いの subtask を 1 件だけ持つ registry。
fn registry_with(subtask_id: &str, session_id: &str, parent_session_id: &str) -> SubtaskRegistry {
    registry_with_caller(
        subtask_id,
        session_id,
        parent_session_id,
        opencrab_actions::CallerIdentity::Agent,
    )
}

/// 親ターンの呼び出し元を指定して 1 件登録した registry（#298）。
fn registry_with_caller(
    subtask_id: &str,
    session_id: &str,
    parent_session_id: &str,
    caller: opencrab_actions::CallerIdentity,
) -> SubtaskRegistry {
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    registry.insert(
        subtask_id.to_string(),
        opencrab_actions::SpawnedSubtask {
            abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            session_id: session_id.to_string(),
            parent_session_id: parent_session_id.to_string(),
            agent_id: "agent-x".to_string(),
            label: "job".to_string(),
            tool_name: "spawn_subtask".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: None,
            caller,
            lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            steerable: false,
        },
    );
    registry
}

/// steer テスト用: `steerable=true` の subtask を 1 件登録した registry（#647）。
fn registry_with_steerable(
    subtask_id: &str,
    session_id: &str,
    parent_session_id: &str,
    caller: opencrab_actions::CallerIdentity,
) -> SubtaskRegistry {
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    registry.insert(
        subtask_id.to_string(),
        opencrab_actions::SpawnedSubtask {
            abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            session_id: session_id.to_string(),
            parent_session_id: parent_session_id.to_string(),
            agent_id: "agent-x".to_string(),
            label: "job".to_string(),
            tool_name: "spawn_subtask".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: None,
            caller,
            lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            steerable: true,
        },
    );
    registry
}

fn sub_ctx(session_id: &str) -> GatewayCallContext {
    GatewayCallContext::new(GatewayCaller::Agent, "agent-x")
        .with_session_id(session_id)
        .with_depth(1)
}

/// #647 gateway: 親セッションからの steer は success を返し、data.steered=true と note を載せる。
#[tokio::test]
async fn steer_subtask_gateway_accepted_maps_to_success() {
    let state = crate::test_app_state();
    let registry = registry_with_steerable(
        "st-1",
        "subtask-st-1",
        "nostr-agent-a",
        opencrab_actions::CallerIdentity::Agent,
    );
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "steer_subtask",
            &json!({ "subtask_id": "st-1", "message": "出力は JSON で" }),
            &ctx,
        )
        .await;
    assert!(r.success, "親からの steer は通る: {:?}", r.error);
    let data = r.data.expect("data");
    assert_eq!(data["steered"], json!(true));
    // sub-session の履歴に steer が 1 本落ちる。
    let conn = state.db.lock().unwrap();
    let logs = opencrab_db::queries::list_session_logs_by_session(&conn, "subtask-st-1").unwrap();
    assert_eq!(
        logs.iter()
            .filter(|l| l.log_type == opencrab_actions::STEER_LOG_TYPE)
            .count(),
        1
    );
}

/// #647 gateway: 空 message は fail-closed で弾く（registry を引くより前）。
#[tokio::test]
async fn steer_subtask_gateway_empty_message_is_rejected() {
    let state = crate::test_app_state();
    let registry = registry_with_steerable(
        "st-1",
        "subtask-st-1",
        "nostr-agent-a",
        opencrab_actions::CallerIdentity::Agent,
    );
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    // 空白のみ。
    let r = actions
        .execute(
            "steer_subtask",
            &json!({ "subtask_id": "st-1", "message": "   " }),
            &ctx,
        )
        .await;
    assert!(!r.success, "空 message は弾く");
    assert!(r.error.unwrap().contains("message"));
    // message キーそのものが無い場合も弾く。
    let r2 = actions
        .execute("steer_subtask", &json!({ "subtask_id": "st-1" }), &ctx)
        .await;
    assert!(!r2.success, "message 欠落は弾く");
}

/// #647 gateway: registry 未配線（dispatch を追跡していない）は not found。
#[tokio::test]
async fn steer_subtask_gateway_no_registry_is_not_found() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "steer_subtask",
            &json!({ "subtask_id": "st-1", "message": "x" }),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert!(r.error.unwrap().contains("not found"));
}

/// #647 gateway: auto-dispatch（steerable=false）は NotSteerable をエラーで返す（黙って無視しない）。
#[tokio::test]
async fn steer_subtask_gateway_auto_dispatch_maps_to_error() {
    let state = crate::test_app_state();
    let registry = registry_with_caller(
        "st-ad",
        "subtask-st-ad",
        "nostr-agent-a",
        opencrab_actions::CallerIdentity::Agent,
    ); // steerable=false
    let actions = SystemGatewayActions::new(state, None, Some(registry), None);
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "steer_subtask",
            &json!({ "subtask_id": "st-ad", "message": "x" }),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert!(r.error.unwrap().contains("auto-dispatch"));
}

/// #647 gateway: 他セッションの Agent からは Unauthorized（拒否コード付き）。
#[tokio::test]
async fn steer_subtask_gateway_foreign_session_maps_to_unauthorized() {
    let state = crate::test_app_state();
    let registry = registry_with_steerable(
        "st-f",
        "subtask-st-f",
        "web-other-c9",
        opencrab_actions::CallerIdentity::Agent,
    );
    let actions = SystemGatewayActions::new(state, None, Some(registry), None);
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "steer_subtask",
            &json!({ "subtask_id": "st-f", "message": "x" }),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert!(r.error.unwrap().starts_with(REJECTION_CODE_PREFIX));
}

/// #647 gateway: registry にも DB にも無い id は not found（present registry + 別 id）。
#[tokio::test]
async fn steer_subtask_gateway_unknown_id_is_not_found() {
    let state = crate::test_app_state();
    let registry = registry_with_steerable(
        "st-1",
        "subtask-st-1",
        "nostr-agent-a",
        opencrab_actions::CallerIdentity::Agent,
    );
    let actions = SystemGatewayActions::new(state, None, Some(registry), None);
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "steer_subtask",
            &json!({ "subtask_id": "does-not-exist", "message": "x" }),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert!(r.error.unwrap().contains("not found"));
}

/// 親セッションログに記録された subtask_progress のメッセージ一覧。
fn progress_messages(state: &AppState, parent_session_id: &str) -> Vec<String> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::list_session_logs_by_session(&conn, parent_session_id)
        .unwrap()
        .into_iter()
        .filter_map(|row| {
            let v: Value = serde_json::from_str(&row.content).ok()?;
            if v.get("type").and_then(|t| t.as_str()) != Some("subtask_progress") {
                return None;
            }
            Some(v.get("message")?.as_str()?.to_string())
        })
        .collect()
}

/// **非 Discord（inner なし）で report_progress が動く**（#175 S1 の主目的）。
/// 親ログに本文が残り、デバウンス後に完了受け口へ `Progress` が届く。
#[tokio::test(start_paused = true)]
async fn report_progress_works_without_inner_gateway() {
    let state = crate::test_app_state();
    let registry = registry_with("st-1", "subtask-st-1", "web-parent-1");
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "halfway there" }),
            &sub_ctx("subtask-st-1"),
        )
        .await;
    assert!(r.success, "error: {:?}", r.error);
    assert_eq!(r.data.as_ref().unwrap()["notified"], json!(true));

    // 本文は親セッションログへ永続化される（sink には運ばない / RFC §1.3）。
    assert_eq!(
        progress_messages(&state, "web-parent-1"),
        vec!["halfway there".to_string()]
    );

    // デバウンス満了後に Progress が 1 本届く。
    tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;
    let settled = sink.settled();
    assert_eq!(settled.len(), 1, "デバウンス後に Progress が 1 本届く");
    assert_eq!(settled[0].kind, SettleKind::Progress);
    assert_eq!(settled[0].session_id, "web-parent-1");
    assert_eq!(settled[0].subtask_id, "st-1");
    assert_eq!(settled[0].exit_reason, "progress");
}

/// **Discord（inner あり）では inner へ委譲される**（S1 で Discord 経路は挙動不変）。
/// own 実装は走らない＝親ログを書かない。
#[tokio::test]
async fn report_progress_delegates_to_inner_when_inner_defines_it() {
    let state = crate::test_app_state();
    let inner = Arc::new(RecordingInner::new(&["report_progress", "spawn_subtask"]));
    let registry = registry_with("st-1", "subtask-st-1", "discord-parent-1");
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        Some(inner.clone() as Arc<dyn GatewayActions>),
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "from discord" }),
            &sub_ctx("subtask-st-1"),
        )
        .await;
    assert!(r.success);
    assert_eq!(
        r.data.unwrap()["reached_inner"],
        json!("report_progress"),
        "inner（Discord 実装）へ委譲されなければならない"
    );
    assert_eq!(inner.calls(), vec!["report_progress".to_string()]);
    // own 実装は走っていない（親ログも sink も触っていない）。
    assert!(progress_messages(&state, "discord-parent-1").is_empty());
    assert!(sink.settled().is_empty());
}

/// 所有権ゲート: 他人の subtask（自分の session でも親でもない）は拒否する。
#[tokio::test]
async fn report_progress_rejects_foreign_subtask() {
    let state = crate::test_app_state();
    let registry = registry_with("st-1", "subtask-st-1", "parent-of-someone-else");
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink as Arc<dyn SubtaskCompletionSink>),
    );

    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "sneaky", "subtask_id": "st-1" }),
            &sub_ctx("some-other-session"),
        )
        .await;
    assert!(!r.success);
    let e = r.error.unwrap();
    assert!(
        e.starts_with(REJECTION_CODE_PREFIX),
        "権限拒否は構造的マーカー付き: {e}"
    );
    // 他セッションの親ログを汚さない。
    assert!(progress_messages(&state, "parent-of-someone-else").is_empty());
}

/// 親セッションからの代理報告は許す（所有権ゲートの片方の分岐）。
///
/// 所有権ゲートは「自分の subtask」か「自分が親である subtask」のどちらかなら通す。
/// 親側の分岐を落としても他のテストは全て通ってしまう（変異実験で確認済み）ため、
/// ここで固定する。Discord 側にも同趣旨のテストがある。
#[tokio::test]
async fn report_progress_allows_parent_reporting_child() {
    let state = crate::test_app_state();
    let registry = registry_with("st-1", "subtask-st-1", "parent-session");
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    // 呼び出し元は subtask 本人ではなく「親セッション」。
    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "親からの代理報告", "subtask_id": "st-1" }),
            &sub_ctx("parent-session"),
        )
        .await;
    assert!(
        r.success,
        "親セッションからの代理報告は許される: {:?}",
        r.error
    );
    assert!(
        progress_messages(&state, "parent-session")
            .iter()
            .any(|m| m.contains("親からの代理報告")),
        "親セッションのログへ記録される"
    );
}

/// #331: セッションを 1 本にした（#323）結果、親経路（`parent_session_id` 一致）だけでは
/// 見知らぬ相手（caller=Agent）のターンから Owner 由来の subtask へ進捗を差し込め、親会話の
/// resume（メインエンジン再呼び出し）を誘発できてしまう。caller ゲートでこれを塞ぐ。
#[tokio::test]
async fn report_progress_non_owner_cannot_report_owner_spawned_via_parent() {
    let state = crate::test_app_state();
    // オーナー発のターンが spawn した subtask。親は 1本化セッション（呼び出し元と一致）。
    let registry = registry_with_caller(
        "st-1",
        "subtask-st-1",
        "nostr-agent-a",
        opencrab_actions::CallerIdentity::Owner,
    );
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    // 見知らぬ相手（caller=Agent）のターン。session は親と一致している（1本化）。
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "sneaky", "subtask_id": "st-1" }),
            &ctx,
        )
        .await;
    assert!(!r.success, "非オーナーは Owner 由来へ進捗を差し込めない");
    assert!(
        r.error.unwrap().starts_with(REJECTION_CODE_PREFIX),
        "権限拒否は構造的マーカー付き"
    );
    // 親ログを汚さない & resume も起こさない。
    assert!(progress_messages(&state, "nostr-agent-a").is_empty());
    tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;
    assert!(sink.settled().is_empty(), "resume を誘発しない");
}

/// #331: 同じ状況でも Owner のターンからは従来どおり進捗を代理報告できる。
#[tokio::test]
async fn report_progress_owner_can_report_owner_spawned_via_parent() {
    let state = crate::test_app_state();
    let registry = registry_with_caller(
        "st-1",
        "subtask-st-1",
        "nostr-agent-a",
        opencrab_actions::CallerIdentity::Owner,
    );
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);

    let ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "owner 代理報告", "subtask_id": "st-1" }),
            &ctx,
        )
        .await;
    assert!(r.success, "Owner のターンからは通る: {:?}", r.error);
    assert!(progress_messages(&state, "nostr-agent-a")
        .iter()
        .any(|m| m.contains("owner 代理報告")));
}

/// #331: サブエージェント自身（depth>=1・自セッション）の進捗報告は、subtask が Owner 由来
/// でも通る。self 経路には caller ゲートを掛けない（掛けると進捗報告が死ぬ）。自セッションは
/// 本人しか名乗れないので攻撃経路にはならない。
#[tokio::test]
async fn report_progress_subagent_self_report_survives_for_owner_spawned() {
    let state = crate::test_app_state();
    let registry = registry_with_caller(
        "st-1",
        "subtask-st-1",
        "nostr-agent-a",
        opencrab_actions::CallerIdentity::Owner,
    );
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);

    // subtask 本人（sub-engine = caller Agent, depth 1, 自セッション）。
    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "作業中です" }),
            &sub_ctx("subtask-st-1"),
        )
        .await;
    assert!(
        r.success,
        "サブエージェント自身の進捗報告は Owner 由来でも通る: {:?}",
        r.error
    );
    assert!(progress_messages(&state, "nostr-agent-a")
        .iter()
        .any(|m| m == "作業中です"));
}

/// #331: Agent 由来の subtask は従来どおり Agent のターンから親経由で代理報告できる
/// （正常系を壊さない）。cancel 側の `cancel_subtask_agent_can_cancel_agent_spawned` に
/// 対応する report_progress 版。caller=Agent / spawner=Agent なので caller ゲートを通る。
#[tokio::test]
async fn report_progress_agent_can_report_agent_spawned_via_parent() {
    let state = crate::test_app_state();
    // 既定の caller=Agent。親は 1本化セッション（呼び出し元と一致）、subtask 本体は別セッション。
    let registry = registry_with("st-1", "subtask-st-1", "nostr-agent-a");
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    // Agent のターン。session は親と一致（is_parent 経路）だが subtask 本体とは別。
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "agent 代理報告", "subtask_id": "st-1" }),
            &ctx,
        )
        .await;
    assert!(
        r.success,
        "Agent 由来の subtask は Agent のターンから親経由で代理報告できる: {:?}",
        r.error
    );
    assert!(progress_messages(&state, "nostr-agent-a")
        .iter()
        .any(|m| m.contains("agent 代理報告")));
}

/// セッション必須ガード（fail-closed）: session_id が無い文脈では実行できない。
#[tokio::test]
async fn report_progress_requires_session_context() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-x");
    let r = actions
        .execute("report_progress", &json!({ "message": "x" }), &ctx)
        .await;
    assert!(!r.success);
    assert!(r.error.unwrap().contains("session_id"));
}

/// message は必須。
#[tokio::test]
async fn report_progress_requires_message() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let r = actions
        .execute("report_progress", &json!({}), &sub_ctx("subtask-st-1"))
        .await;
    assert!(!r.success);
    assert!(r.error.unwrap().contains("'message' is required"));
}

/// 完了受け口が未配線なら、記録だけして通知はしない（デバウンスタスクも起動しない）。
/// 「黙って消える」のを避けるため、結果に `notified: false` を載せる。
#[tokio::test(start_paused = true)]
async fn report_progress_records_but_does_not_notify_without_sink() {
    let state = crate::test_app_state();
    let registry = registry_with("st-1", "subtask-st-1", "rest-parent-1");
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);

    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "no sink here" }),
            &sub_ctx("subtask-st-1"),
        )
        .await;
    assert!(r.success);
    assert_eq!(r.data.unwrap()["notified"], json!(false));
    // 記録は残る。
    assert_eq!(
        progress_messages(&state, "rest-parent-1"),
        vec!["no sink here".to_string()]
    );
    // デバウンスタスクを起動していない＝世代カウンタも進んでいない。
    tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;
    assert!(
        !state.progress_debounce.claim_latest("rest-parent-1", 1),
        "受け口未配線ではデバウンス世代を消費しない"
    );
}

/// **デバウンス状態が `AppState` 側にあることを固定する回帰テスト（#175 S1 の最重要点）**。
///
/// `SystemGatewayActions` は run ごとに作り直される。デバウンス世代カウンタを
/// この構造体のフィールドに置くと、2 回目の呼び出しで世代が 0 から張り直され、
/// **両方の呼び出しが発火する**（＝バーストで LLM を無駄に呼ぶ）。ここでは
/// 別インスタンスから 2 回報告し、届く `Progress` が 1 本だけであることを固定する。
#[tokio::test(start_paused = true)]
async fn progress_debounce_survives_gateway_recreation() {
    let state = crate::test_app_state();
    let registry = registry_with("st-1", "subtask-st-1", "web-parent-1");
    let sink = Arc::new(RecordingSink::default());

    // 1 回目: この run 用の gateway インスタンス。
    let first = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry.clone()),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );
    assert!(
        first
            .execute(
                "report_progress",
                &json!({ "message": "step 1" }),
                &sub_ctx("subtask-st-1")
            )
            .await
            .success
    );
    drop(first);

    // 2 回目: 別の run（＝別インスタンス）。同じ AppState を共有する。
    let second = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );
    assert!(
        second
            .execute(
                "report_progress",
                &json!({ "message": "step 2" }),
                &sub_ctx("subtask-st-1")
            )
            .await
            .success
    );

    tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;

    // 本文は 2 件とも親ログへ残る（間引くのは通知だけ）。
    assert_eq!(
        progress_messages(&state, "web-parent-1"),
        vec!["step 1".to_string(), "step 2".to_string()]
    );
    // 通知は最後の 1 本だけ。デバウンス状態をインスタンスのフィールドに移すと 2 本届く。
    let settled = sink.settled();
    assert_eq!(
            settled.len(),
            1,
            "デバウンスは gateway の作り直しを跨いで効かなければならない（AppState 側に置く）。届いた: {settled:?}"
        );
    assert_eq!(settled[0].kind, SettleKind::Progress);
}

/// **#298 の直接のトリガ**: `report_progress` のデバウンス発火は親会話を resume
/// するので、通知には**親ターンの呼び出し元**を載せる。
///
/// `ctx.caller`（= sub-engine 自身 = `Agent`）を載せると、進捗を報告した瞬間に
/// 親ターンが最小権限へ降格し、owner/trusted のツールが丸ごと消える。
#[tokio::test(start_paused = true)]
async fn report_progress_carries_the_parent_caller_to_the_sink() {
    let state = crate::test_app_state();
    let registry = registry_with_caller(
        "st-1",
        "subtask-st-1",
        "web-parent-1",
        opencrab_actions::CallerIdentity::Owner,
    );
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    assert!(
        actions
            .execute(
                "report_progress",
                &json!({ "message": "掘っています" }),
                // 呼ぶのは sub-engine（最小権限）。ここの caller を使ってはならない。
                &sub_ctx("subtask-st-1"),
            )
            .await
            .success
    );
    tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;

    let settled = sink.settled();
    assert_eq!(settled.len(), 1, "進捗通知は 1 本: {settled:?}");
    assert_eq!(settled[0].kind, SettleKind::Progress);
    assert_eq!(
        settled[0].caller,
        opencrab_actions::CallerIdentity::Owner,
        "進捗を報告すると親ターンの権限が落ちる（#298 の自爆的な挙動）"
    );
}

/// 昇格経路にはしない: 親が `Agent` なら進捗通知の caller も `Agent`。
#[tokio::test(start_paused = true)]
async fn report_progress_does_not_escalate_agent_callers() {
    let state = crate::test_app_state();
    let registry = registry_with("st-1", "subtask-st-1", "web-parent-1");
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    assert!(
        actions
            .execute(
                "report_progress",
                &json!({ "message": "掘っています" }),
                &sub_ctx("subtask-st-1"),
            )
            .await
            .success
    );
    tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;

    let settled = sink.settled();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].caller, opencrab_actions::CallerIdentity::Agent);
}

// ---- #157 S1: 汎用管理ツール 4 個の gateway 非依存化 ----
//
// 移設前（origin/main）にはこの 4 ツールの挙動テストが**1 件も無かった**ため、
// ここは「移植」ではなく新規に契約を覆うテスト群である。守っている不変条件は
// `crate::agent_management` のモジュール doc に列挙してある。

/// 実行許可設定に shell セクションを持たせた `AppState`。
///
/// `initial` は**設定ファイル由来**の許可コマンド（グローバル設定）を模す。
/// per-agent の許可（DB）と混ざらないこと（#202）を検証するには、この 2 つが
/// 区別できる構成が必要。
fn state_with_shell(initial: &[&str]) -> AppState {
    let state = crate::test_app_state();
    {
        let mut cfg = state.tools_config.write().unwrap();
        cfg.enabled = true;
        cfg.shell = Some(opencrab_actions::tools::ShellToolConfig {
            enabled: true,
            allowed_commands: initial.iter().map(|s| s.to_string()).collect(),
            timeout_secs: 30,
            max_timeout_secs: 300,
            working_dir: None,
            inherit_env: false,
            allowed_env_vars: Vec::new(),
            max_output_bytes: 1024,
            commands: Vec::new(),
        });
    }
    state
}

/// 走行中の実行許可設定（`AppState.tools_config`）に載っているコマンド一覧。
fn live_allowed_commands(state: &AppState) -> Vec<String> {
    state
        .tools_config
        .read()
        .unwrap()
        .shell
        .as_ref()
        .map(|s| s.allowed_commands.clone())
        .unwrap_or_default()
}

/// DB に永続化されている許可コマンド一覧。
fn db_allowed_commands(state: &AppState, agent_id: &str) -> Vec<String> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::list_agent_allowed_commands(&conn, agent_id).unwrap()
}

/// **次の run** がそのエージェントに許可するコマンド一覧。
///
/// 応答生成（`crate::process`）が毎 run 呼ぶ解決点をそのまま使う。グローバル設定と
/// 混同しないよう、per-agent の実効値はこのヘルパー越しにだけ見る。
fn run_allowed_commands(state: &AppState, agent_id: &str) -> Vec<String> {
    crate::process::resolve_run_tools_config(state, agent_id)
        .shell
        .map(|s| s.allowed_commands)
        .unwrap_or_default()
}

/// シェルツールを実際に dispatch するための `ActionContext`（作業ディレクトリ付き）。
fn shell_ctx() -> (tempfile::TempDir, opencrab_actions::ActionContext) {
    let dir = tempfile::TempDir::new().unwrap();
    let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
    let conn = opencrab_db::init_memory().unwrap();
    let ctx = opencrab_actions::ActionContext {
        caller: opencrab_actions::CallerIdentity::Owner,
        agent_id: "agent-x".to_string(),
        agent_name: "Agent X".to_string(),
        session_id: None,
        db: opencrab_db::Db::from_connection(conn),
        workspace: Arc::new(ws),
        last_metrics_id: Arc::new(std::sync::Mutex::new(None)),
        model_override: Arc::new(std::sync::Mutex::new(None)),
        current_purpose: Arc::new(std::sync::Mutex::new("test".to_string())),
        runtime_info: Arc::new(std::sync::Mutex::new(opencrab_actions::RuntimeInfo {
            default_model: "mock:test".to_string(),
            active_model: None,
            available_providers: vec![],
            gateway: "test".to_string(),
        })),
    };
    (dir, ctx)
}

fn owner_ctx() -> GatewayCallContext {
    GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
}

fn agent_ctx() -> GatewayCallContext {
    GatewayCallContext::new(GatewayCaller::Agent, "agent-x")
}

// ---- #157 S5: 通知先（webhook）の管理ツール ----

/// 移設した 6 ツールの名前（#157 S5）。`ensure_*` は含まない（Discord 側に残る）。
const MOVED_WEBHOOK_TOOLS: &[&str] = &[
    "get_default_subtask_webhook",
    "set_default_subtask_webhook",
    "list_subtask_webhooks",
    "get_default_webhook",
    "set_default_webhook",
    "list_webhooks",
];

/// **#157 S5 の本題**: 6 ツールが own 定義にちょうど 1 件ずつある。
#[test]
fn webhook_target_tools_are_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    for name in MOVED_WEBHOOK_TOOLS {
        assert_eq!(
            defs.iter().filter(|d| &d.name == name).count(),
            1,
            "{name} は own 定義にちょうど 1 件必要（#157 S5）"
        );
    }
}

/// **Discord 無効の構成でも 6 ツールが露出する**（#157 S5 の証明）。
///
/// `inner = None` は「transport 固有 gateway が居ない」経路（web / REST / Nostr /
/// heartbeat、および Discord feature 無効ビルド）そのもの。移設前はこの構成で
/// 6 ツールが一切出なかった＝ #157 が報告している不具合そのもの。
#[test]
fn webhook_target_tools_are_exposed_without_any_transport_gateway() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    for name in MOVED_WEBHOOK_TOOLS {
        assert!(
                names.contains(&name.to_string()),
                "transport gateway 無しの構成で {name} が露出しない（#157 の不具合そのもの）: {names:?}"
            );
    }
    // 逆に、Discord に残した `ensure_*` はここには出ない（inner が居ないため）。
    for name in ["ensure_webhook", "ensure_subtask_webhook"] {
        assert!(
            !names.contains(&name.to_string()),
            "{name} は Discord gateway 由来のはず（own に増やしてはいけない）"
        );
    }
}

/// 引数スキーマを移設前（Discord 定義）と同一に保つ。
///
/// 名前・`required`・プロパティ名の集合をリテラルで固定する。ここが変わると
/// 既存の会話ログにあるツール呼び出しが通らなくなる。
#[test]
fn webhook_target_tool_schemas_match_the_discord_originals() {
    let defs = SystemGatewayActions::own_definitions();
    let find = |n: &str| defs.iter().find(|d| d.name == n).unwrap();
    let props = |n: &str| {
        let mut keys: Vec<String> = find(n).parameters["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    };

    assert_eq!(
        find("get_default_subtask_webhook").parameters["required"],
        json!([])
    );
    assert_eq!(
        props("get_default_subtask_webhook"),
        vec!["agent_id", "scope", "tool_name"]
    );

    assert_eq!(
        find("set_default_subtask_webhook").parameters["required"],
        json!(["scope"])
    );
    assert_eq!(
        props("set_default_subtask_webhook"),
        vec![
            "agent_id",
            "enabled",
            "events",
            "kind",
            "max_chars",
            "output_mode",
            "scope",
            "tool_name",
            "url",
        ]
    );

    assert_eq!(
        find("list_subtask_webhooks").parameters["required"],
        json!([])
    );
    assert_eq!(
        props("list_subtask_webhooks"),
        vec!["agent_id", "include_disabled", "scope"]
    );

    assert_eq!(
        find("get_default_webhook").parameters["required"],
        json!([])
    );
    assert_eq!(
        props("get_default_webhook"),
        vec!["agent_id", "family", "tool_name"]
    );

    assert_eq!(
        find("set_default_webhook").parameters["required"],
        json!(["scope"])
    );
    assert_eq!(
        props("set_default_webhook"),
        vec![
            "agent_id",
            "enabled",
            "events",
            "family",
            "max_chars",
            "output_mode",
            "scope",
            "tool_name",
            "url",
        ]
    );

    assert_eq!(find("list_webhooks").parameters["required"], json!([]));
    assert_eq!(
        props("list_webhooks"),
        vec!["agent_id", "family", "include_disabled", "scope"]
    );
}

/// **6 ツールは inner へ委譲されない**（own が唯一の実装）。
///
/// 委譲パターンのまま残すと、Discord が誤って再定義したときに own の実装が黙って
/// バイパスされる（#155 の後退）。`ensure_*` は逆に inner へ渡る必要がある。
#[tokio::test]
async fn webhook_target_tools_are_not_delegated_to_inner() {
    let state = crate::test_app_state();
    let inner = Arc::new(RecordingInner::new(&[
        "get_default_subtask_webhook",
        "set_default_subtask_webhook",
        "list_subtask_webhooks",
        "get_default_webhook",
        "set_default_webhook",
        "list_webhooks",
        "ensure_webhook",
    ]));
    let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);

    for name in MOVED_WEBHOOK_TOOLS {
        let _ = actions
            .execute(name, &json!({"scope": "agent"}), &owner_ctx())
            .await;
    }
    assert!(
        inner.calls().is_empty(),
        "移設した 6 ツールが inner へ委譲された: {:?}",
        inner.calls()
    );

    // Discord に残した `ensure_webhook` は既定アームで inner へ委譲される。
    let _ = actions
        .execute("ensure_webhook", &json!({}), &owner_ctx())
        .await;
    assert_eq!(inner.calls(), vec!["ensure_webhook".to_string()]);
}

/// **#157 S1 の本題**: 4 ツールが `SystemGatewayActions` の own 定義になっている。
///
/// own 定義は transport の有無に依存しないため、これが `definitions()` に出ることは
/// 「web / Nostr / REST / heartbeat でも使える」ことと同義である。own から消えると
/// Discord 専用に逆戻りする（それが #157 が報告している不具合そのもの）。
#[test]
fn generic_management_tools_are_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    for name in [
        "update_memory_index_config",
        "add_allowed_command",
        "list_allowed_commands",
        "remove_allowed_command",
    ] {
        assert_eq!(
            defs.iter().filter(|d| d.name == name).count(),
            1,
            "{name} は own 定義にちょうど 1 件必要（#157 S1）"
        );
    }
}

/// **Discord 無効の構成でも 4 ツールが露出する**（#157 S1 の証明）。
///
/// `inner = None` は「transport 固有 gateway が居ない」経路（web / REST /
/// heartbeat、および Discord feature 無効ビルド）そのもの。移設前はこの構成で
/// 4 ツールが一切出なかった。
#[test]
fn generic_management_tools_are_exposed_without_any_transport_gateway() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    for name in [
        "update_memory_index_config",
        "add_allowed_command",
        "list_allowed_commands",
        "remove_allowed_command",
    ] {
        assert!(
                names.contains(&name.to_string()),
                "transport gateway 無しの構成で {name} が露出しない（#157 の不具合そのもの）: {names:?}"
            );
    }
}

/// 引数スキーマを移設前（Discord 定義）と同一に保つ。
#[test]
fn generic_management_tool_schemas_match_the_discord_originals() {
    let defs = SystemGatewayActions::own_definitions();
    let find = |n: &str| defs.iter().find(|d| d.name == n).unwrap();

    let d = find("update_memory_index_config");
    assert!(d.parameters["required"].as_array().unwrap().is_empty());
    let props = d.parameters["properties"].as_object().unwrap();
    assert_eq!(props["batch_size"]["type"], json!("integer"));
    assert_eq!(props["threshold"]["type"], json!("integer"));

    for n in ["add_allowed_command", "remove_allowed_command"] {
        let d = find(n);
        assert_eq!(d.parameters["required"], json!(["command"]), "{n}");
        assert_eq!(
            d.parameters["properties"]["command"]["type"],
            json!("string"),
            "{n}"
        );
    }

    let d = find("list_allowed_commands");
    assert!(d.parameters["required"].as_array().unwrap().is_empty());
    assert!(d.parameters["properties"].as_object().unwrap().is_empty());
}

/// **オーナー限定検査が移設後も効く**（add）。
///
/// #330 以降は多層防御になった: bridge policy 層（`OWNER_ONLY_ACTIONS`）でも
/// `add_allowed_command` / `remove_allowed_command` を owner_only にゲートし、加えて
/// この server ハンドラ内検査も残る。SystemGatewayActions を直接叩くこのテストは
/// bridge 層を通らないので、ハンドラ側の owner 検査が単独で効くことを固定する。
///
/// エラー文言はバイト単位で移設前と同一（移設で文言が変わっていないことの防波堤）。
#[tokio::test]
async fn add_allowed_command_rejects_non_owner_without_side_effects() {
    // bridge policy 層でも owner_only であること（#330 の多層防御）。
    assert!(
        opencrab_actions::OWNER_ONLY_ACTIONS.contains(&"add_allowed_command"),
        "#330: add_allowed_command は bridge policy 層でも owner_only であるべき"
    );

    let state = state_with_shell(&[]);
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    let r = actions
        .execute(
            "add_allowed_command",
            &json!({"command": "curl"}),
            &agent_ctx(),
        )
        .await;

    assert!(!r.success);
    assert_eq!(
        r.error.as_deref(),
        Some("このアクションはオーナーのみ実行できます"),
        "拒否文言は移設前と 1 文字も変えない"
    );
    assert!(r.data.is_none());
    // 副作用ゼロ: DB も走行中の実行許可設定も変わらない。
    assert!(db_allowed_commands(&state, "agent-x").is_empty());
    assert!(live_allowed_commands(&state).is_empty());
}

/// **オーナー限定検査が移設後も効く**（remove）。既に許可済みのコマンドが
/// 非オーナーの呼び出しで消えないこと。
#[tokio::test]
async fn remove_allowed_command_rejects_non_owner_without_side_effects() {
    // bridge policy 層でも owner_only であること（#330 の多層防御）。
    assert!(
        opencrab_actions::OWNER_ONLY_ACTIONS.contains(&"remove_allowed_command"),
        "#330: remove_allowed_command は bridge policy 層でも owner_only であるべき"
    );

    let state = state_with_shell(&["git"]);
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", "git", "owner").unwrap();
    }
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    let r = actions
        .execute(
            "remove_allowed_command",
            &json!({"command": "git"}),
            &agent_ctx(),
        )
        .await;

    assert!(!r.success);
    assert_eq!(
        r.error.as_deref(),
        Some("このアクションはオーナーのみ実行できます"),
        "拒否文言は移設前と 1 文字も変えない"
    );
    // 許可は残っている（DB も走行中の設定も）。
    assert_eq!(db_allowed_commands(&state, "agent-x"), vec!["git"]);
    assert_eq!(live_allowed_commands(&state), vec!["git"]);
}

/// **グローバルな実行許可設定へは書かない**（#202）。DB だけが更新される。
///
/// 移設前の Discord 実装は DB と併せてグローバル設定にも書き込んでいた。応答生成は
/// **全エージェント**についてこの設定を実行許可の土台として複製する
/// （`crate::process::resolve_run_tools_config`）ので、その書き込みは
/// 「A が許可したコマンドが全エージェントで実行可能になる」漏れそのものだった。
///
/// このテストは**旧 `add_allowed_command_updates_the_live_shared_tools_config` の
/// 反転**である。旧テストは漏れを不変条件として固定していた。
#[tokio::test]
async fn add_allowed_command_does_not_write_to_the_global_tools_config() {
    let state = state_with_shell(&["ls"]);
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    let r = actions
        .execute(
            "add_allowed_command",
            &json!({"command": "curl"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);

    // DB へ永続化されている（信頼できる情報源）。
    assert_eq!(db_allowed_commands(&state, "agent-x"), vec!["curl"]);
    // グローバル設定は 1 文字も変わらない。
    assert_eq!(
        live_allowed_commands(&state),
        vec!["ls"],
        "グローバル設定へ書き込むと全エージェントへ漏れる（#202）"
    );
}

/// 削除もグローバル設定を触らない（追加と対称 / #202）。
///
/// 旧実装は `retain` でグローバル設定からも消していたため、**設定ファイル由来の
/// コマンドをエージェントの操作でグローバルに削除できた**。
/// 旧 `remove_allowed_command_updates_the_live_shared_tools_config` の反転。
#[tokio::test]
async fn remove_allowed_command_does_not_write_to_the_global_tools_config() {
    // "curl" は**設定ファイル由来**でもあり、かつ agent-x の DB 許可でもある状態。
    let state = state_with_shell(&["ls", "curl"]);
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", "curl", "owner").unwrap();
    }
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    let r = actions
        .execute(
            "remove_allowed_command",
            &json!({"command": "curl"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);

    assert!(db_allowed_commands(&state, "agent-x").is_empty());
    assert_eq!(
        live_allowed_commands(&state),
        vec!["ls", "curl"],
        "設定ファイル由来のコマンドをエージェントの操作で消してはならない（#202）"
    );
}

/// **エージェント A の追加が、エージェント B の実行許可を変えない**（#202 の本体）。
///
/// 「次の run が何を許可するか」は `crate::process::resolve_run_tools_config` が
/// 決める（応答生成が毎 run 呼ぶ唯一の解決点）。A の追加後にそれを両エージェントで
/// 解決し、A にだけ効いていることを固定する。
///
/// これが `add_allowed_command_takes_effect_on_the_next_run_but_not_the_same_turn` と対になって
/// 「撤去しても呼び出し元は困らない / 他エージェントへは漏れない」の両方を証明する。
#[tokio::test]
async fn add_allowed_command_does_not_change_another_agents_permissions() {
    let state = state_with_shell(&["ls"]);
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    let r = actions
        .execute(
            "add_allowed_command",
            &json!({"command": "curl"}),
            &GatewayCallContext::new(GatewayCaller::Owner, "agent-a"),
        )
        .await;
    assert!(r.success, "{:?}", r.error);

    assert_eq!(
        run_allowed_commands(&state, "agent-a"),
        vec!["ls", "curl"],
        "追加したエージェント自身には次の run で効かなければならない"
    );
    assert_eq!(
        run_allowed_commands(&state, "agent-b"),
        vec!["ls"],
        "agent-a の追加が agent-b の実行許可を広げてはならない（#202）"
    );
    // グローバル設定そのものも汚れていない。
    assert_eq!(live_allowed_commands(&state), vec!["ls"]);
}

/// **エージェント A の削除が、設定ファイル由来のコマンドや B の許可を消さない**（#202）。
#[tokio::test]
async fn remove_allowed_command_does_not_change_another_agents_permissions() {
    // 設定ファイル由来: "ls"。A と B の両方が DB で "curl" を許可されている。
    let state = state_with_shell(&["ls"]);
    {
        let conn = state.db.lock().unwrap();
        for agent in ["agent-a", "agent-b"] {
            opencrab_db::queries::add_agent_allowed_command(&conn, agent, "curl", "owner").unwrap();
        }
    }
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    let r = actions
        .execute(
            "remove_allowed_command",
            &json!({"command": "curl"}),
            &GatewayCallContext::new(GatewayCaller::Owner, "agent-a"),
        )
        .await;
    assert!(r.success, "{:?}", r.error);

    assert_eq!(
        run_allowed_commands(&state, "agent-a"),
        vec!["ls"],
        "削除は呼び出したエージェントには次の run で効く"
    );
    assert_eq!(
        run_allowed_commands(&state, "agent-b"),
        vec!["ls", "curl"],
        "agent-a の削除が agent-b の許可を消してはならない（#202）"
    );
    assert_eq!(
        live_allowed_commands(&state),
        vec!["ls"],
        "設定ファイル由来の許可は残る（#202）"
    );
}

/// **追加した許可は「次の run」で呼び出したエージェントに効く**（撤去の前提の実証）。
///
/// グローバル設定への書き込みを撤去してよい根拠は 2 つあり、両方をここで実際に
/// 走らせて確かめる。
///
/// 1. **次の run で効く**: run の冒頭で `resolve_run_tools_config` が DB の許可を
///    ローカル複製へマージし、`register_tools_from_config` がそれを `ShellToolAction`
///    へ渡す。したがって次の run のシェルツールは許可リスト検査を通す。
/// 2. **同ターンでは元から効かない**: ツール登録は run 冒頭のスナップショットなので、
///    許可を追加しても**その run で登録済みのツール**には届かない。つまりグローバル
///    書き込みを撤去しても失われる機能は無い（撤去前も同ターン反映は無かった）。
///
/// 許可リスト検査だけを見るため、実際には存在しないコマンド名を使う。
/// 拒否は「allowed list に無い」/ 通過は「spawn 失敗」で区別でき、プロセスは
/// 一切起動しない（PATH や OS 差に依存しない）。
#[tokio::test]
async fn add_allowed_command_takes_effect_on_the_next_run_but_not_the_same_turn() {
    const CMD: &str = "opencrab_absent_probe";

    let state = state_with_shell(&[]);
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    // --- この run のツールを登録する（run 冒頭のスナップショット） ---
    let mut this_run = opencrab_actions::ActionDispatcher::new();
    opencrab_actions::register_tools_from_config(
        &crate::process::resolve_run_tools_config(&state, "agent-x"),
        &mut this_run,
    );

    // --- 走行中に許可を追加する ---
    let r = actions
        .execute(
            "add_allowed_command",
            &json!({"command": CMD}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);

    let (_dir, ctx) = shell_ctx();

    // 根拠 2: **同ターンでは効かない**（登録済みツールはスナップショットを持つ）。
    let same_turn = this_run
        .execute("execute_shell", &json!({"command": CMD}), &ctx)
        .await;
    assert!(!same_turn.success);
    assert!(
        same_turn
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("is not in the allowed list"),
        "同ターン反映は元から効かない前提が崩れている: {:?}",
        same_turn.error
    );

    // 根拠 1: **次の run では効く**（DB からマージされる）。
    let mut next_run = opencrab_actions::ActionDispatcher::new();
    opencrab_actions::register_tools_from_config(
        &crate::process::resolve_run_tools_config(&state, "agent-x"),
        &mut next_run,
    );
    let next = next_run
        .execute("execute_shell", &json!({"command": CMD}), &ctx)
        .await;
    assert!(!next.success, "存在しないコマンドなので spawn は失敗する");
    let e = next.error.as_deref().unwrap_or_default();
    assert!(
        !e.contains("is not in the allowed list"),
        "次の run では許可リスト検査を通らなければならない（撤去の前提）: {e}"
    );
    assert!(
        e.contains("Failed to spawn command"),
        "許可リストを通過して spawn まで到達したはず: {e}"
    );

    // グローバル設定は最後まで汚れていない。
    assert!(live_allowed_commands(&state).is_empty());
}

/// **コマンド名の文字種検査が効く**（英数字・`-`・`_` のみ）。
///
/// 同系統の `manage_allowed_commands` は trim だけなので、移設でこちらを緩めると
/// `rm -rf /` のようなシェル片やパス区切りを許可リストへ入れられてしまう。
/// 検査は DB へ触る**前**に行う（副作用ゼロ）。
#[tokio::test]
async fn add_allowed_command_rejects_invalid_command_characters() {
    let state = state_with_shell(&[]);
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    for bad in ["rm -rf /", "/bin/sh", "git;whoami", "cat|less", "a$b"] {
        let r = actions
            .execute(
                "add_allowed_command",
                &json!({"command": bad}),
                &owner_ctx(),
            )
            .await;
        assert!(!r.success, "{bad} は拒否されなければならない");
        let e = r.error.unwrap();
        assert_eq!(
                e,
                format!(
                    "コマンド名に無効な文字が含まれています: {}（英数字・ハイフン・アンダースコアのみ使用可）",
                    bad
                ),
                "文字種エラーの文言は移設前と同一"
            );
    }
    // 1 件も通っていない。
    assert!(db_allowed_commands(&state, "agent-x").is_empty());
    assert!(live_allowed_commands(&state).is_empty());

    // 対: 妥当な文字（英数字・ハイフン・アンダースコア）は通る。
    for good in ["curl", "docker-compose", "my_tool", "python3"] {
        let r = actions
            .execute(
                "add_allowed_command",
                &json!({"command": good}),
                &owner_ctx(),
            )
            .await;
        assert!(r.success, "{good} は許可されるべき: {:?}", r.error);
    }
}

/// `command` 未指定 / 空文字は移設前と同じ文言で失敗する（add / remove の両方）。
#[tokio::test]
async fn allowed_command_tools_require_a_non_empty_command() {
    let state = state_with_shell(&[]);
    let actions = SystemGatewayActions::new(state, None, None, None);
    for name in ["add_allowed_command", "remove_allowed_command"] {
        for args in [json!({}), json!({"command": ""}), json!({"command": 42})] {
            let r = actions.execute(name, &args, &owner_ctx()).await;
            assert!(!r.success, "{name} {args}");
            assert_eq!(
                r.error.as_deref(),
                Some("commandパラメータが必要です"),
                "{name} {args}"
            );
        }
    }
}

/// **レスポンス JSON が移設前と同一**（許可コマンド 3 種）。期待値をリテラルで固定する。
#[tokio::test]
async fn allowed_command_response_json_is_unchanged() {
    let state = state_with_shell(&[]);
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    // 追加（新規）
    let r = actions
        .execute(
            "add_allowed_command",
            &json!({"command": "curl"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "command": "curl",
            "agent_id": "agent-x",
            "message": "`curl` を許可コマンドに追加しました",
        })
    );

    // 追加（既存）: `already_exists` が付く。
    let r = actions
        .execute(
            "add_allowed_command",
            &json!({"command": "curl"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "command": "curl",
            "agent_id": "agent-x",
            "message": "`curl` はすでに許可コマンドに登録されています",
            "already_exists": true,
        })
    );

    // 一覧: commands / count / agent_id の 3 キー。
    let r = actions
        .execute("list_allowed_commands", &json!({}), &agent_ctx())
        .await;
    assert!(r.success);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "commands": ["curl"],
            "count": 1,
            "agent_id": "agent-x",
        })
    );

    // 削除（存在した）
    let r = actions
        .execute(
            "remove_allowed_command",
            &json!({"command": "curl"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "command": "curl",
            "agent_id": "agent-x",
            "message": "`curl` を許可コマンドから削除しました",
        })
    );

    // 削除（存在しない）: `not_found` が付き、success は true のまま。
    let r = actions
        .execute(
            "remove_allowed_command",
            &json!({"command": "curl"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "command": "curl",
            "agent_id": "agent-x",
            "message": "`curl` は許可コマンドに登録されていませんでした",
            "not_found": true,
        })
    );
}

/// 一覧は**呼び出し元のエージェント**の許可コマンドだけを返す（agent_id スコープ）。
///
/// `state_with_shell(&[])` を使うのは意図的: 生の `test_app_state()` は
/// `ToolsConfig::default()`（`enabled: false`）で、#311 のゲート追加後は一覧が
/// 空になってしまう。ここで検証したいのは agent_id スコープであってゲートではないので、
/// ゲートを開いた（`enabled: true` / `shell.enabled: true`）最小構成に載せ替える。
#[tokio::test]
async fn list_allowed_commands_is_scoped_to_the_calling_agent() {
    let state = state_with_shell(&[]);
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", "curl", "owner").unwrap();
        opencrab_db::queries::add_agent_allowed_command(&conn, "other-agent", "wget", "owner")
            .unwrap();
    }
    let actions = SystemGatewayActions::new(state, None, None, None);
    let r = actions
        .execute("list_allowed_commands", &json!({}), &agent_ctx())
        .await;
    assert!(r.success);
    assert_eq!(r.data.unwrap()["commands"], json!(["curl"]));
}

// ---- #300: 一覧が「実効リスト」であること ----
//
// 不具合そのものは「`list_allowed_commands` が DB 行しか返さず、設定ファイル由来の
// コマンドが落ちる」。エージェントは戻り値を「これが使える全部だ」と読むので、
// 落ちた分は「使えない」と誤認され、実際には実行できる作業が止まった。

/// 設定ファイル相当の shell 設定（構造化 `commands` + 素の `allowed_commands`）を
/// 持つ `AppState`。実運用の `config/default.toml` は `[[tools.shell.commands]]`
/// （構造化）で 10 個を与えるので、そちら側も再現できないと #300 を覆えない。
fn state_with_shell_commands(structured: &[&str], plain: &[&str]) -> AppState {
    let state = state_with_shell(plain);
    {
        let mut cfg = state.tools_config.write().unwrap();
        let shell = cfg.shell.as_mut().unwrap();
        shell.commands = structured
            .iter()
            .map(|name| opencrab_actions::tools::config::CommandConfig {
                name: name.to_string(),
                permission: opencrab_actions::tools::config::CommandPermission::Agent,
                timeout_secs: None,
                description: None,
            })
            .collect();
    }
    state
}

async fn listed_commands(state: &AppState) -> Vec<String> {
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute("list_allowed_commands", &json!({}), &agent_ctx())
        .await;
    assert!(r.success, "{:?}", r.error);
    serde_json::from_value(r.data.unwrap()["commands"].clone()).unwrap()
}

/// **DB に 1 行も無くても設定ファイル由来のコマンドが返る**（#300 の中核）。
///
/// 修正前はここが `[]` / `count: 0` になり、エージェントは「シェルは何も使えない」と
/// 読んだ。実際には設定ファイル分がそのまま実行できる。
#[tokio::test]
async fn list_allowed_commands_includes_config_base_without_any_db_row() {
    let state = state_with_shell_commands(&["ls", "cat", "grep"], &["python3"]);
    assert!(
        db_allowed_commands(&state, "agent-x").is_empty(),
        "前提: DB に per-agent の行は無い"
    );

    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute("list_allowed_commands", &json!({}), &agent_ctx())
        .await;
    assert!(r.success, "{:?}", r.error);
    let data = r.data.unwrap();
    assert_eq!(
        data["commands"],
        json!(["ls", "cat", "grep", "python3"]),
        "設定ファイル由来のコマンドが戻り値から落ちている（#300）"
    );
    // `count` は `commands` と必ず一致する（片方だけ古い値だと誤認の材料になる）。
    assert_eq!(data["count"], json!(4));
    assert_eq!(data["agent_id"], json!("agent-x"));
}

/// **設定ファイル分と DB 行が合成され、重複しない**。
///
/// 合成規則は `resolve_run_tools_config` +
/// `ShellToolConfig::effective_commands()` のものをそのまま使う:
/// 構造化 `commands` が先、その後ろに `allowed_commands`（設定 → DB の順）、
/// 既出の名前は積まない。`cargo` は設定と DB の両方にあるが 1 個だけ出る。
#[tokio::test]
async fn list_allowed_commands_merges_db_rows_with_config_without_duplicates() {
    let state = state_with_shell_commands(&["cargo"], &["ls", "cat"]);
    {
        let conn = state.db.lock().unwrap();
        for cmd in ["cargo", "mkdir"] {
            opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", cmd, "owner")
                .unwrap();
        }
    }
    assert_eq!(
        listed_commands(&state).await,
        vec!["cargo", "ls", "cat", "mkdir"],
        "設定 + DB の合成が実効リストと一致しない（#300）"
    );
}

/// **戻り値が `execute_shell` の `Allowed: ...` と 1 コマンドも食い違わない**。
///
/// #300 の実害はこの 2 つのズレそのもの。プロンプト側は正しく 12 個を並べていたのに
/// 一覧は 2 個しか返さず、エージェントは一覧を信じて止まった。両者を同じ解決点
/// （`process::effective_allowed_commands` / `resolve_run_tools_config`）から作る
/// 限りズレは起き得ないが、片方だけ書き換えられたらここで落ちる。
#[tokio::test]
async fn list_allowed_commands_matches_the_execute_shell_description() {
    let state = state_with_shell_commands(&["curl", "echo", "jq", "cargo"], &["python3"]);
    {
        let conn = state.db.lock().unwrap();
        for cmd in ["cargo", "mkdir"] {
            opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", cmd, "owner")
                .unwrap();
        }
    }

    // LLM に実際に渡る `execute_shell` の引数説明を、run と同じ手順で組み立てる。
    let shell_cfg = crate::process::resolve_run_tools_config(&state, "agent-x")
        .shell
        .expect("shell 設定");
    let shell_action = opencrab_actions::tools::shell::ShellToolAction::new(shell_cfg);
    let desc = opencrab_actions::Action::parameters(&shell_action)["properties"]["command"]
        ["description"]
        .as_str()
        .unwrap()
        .to_string();
    // このパースは `crates/actions/src/tools/shell.rs` の書式
    // （`... Allowed: a, b, c` で末尾がコマンド列）に依存している。
    let from_prompt: Vec<String> = desc
        .split_once("Allowed: ")
        .expect("`Allowed: ` を含む説明文（書式は crates/actions/src/tools/shell.rs）")
        .1
        .split(", ")
        .map(str::to_string)
        .collect();

    assert_eq!(
        listed_commands(&state).await,
        from_prompt,
        "一覧ツールの戻り値とプロンプトの Allowed が食い違っている（#300 の実害そのもの）。\
             ただし `Allowed: ` の後ろに文を足した場合もここが落ちる — 差分が末尾要素だけなら\
             まず `crates/actions/src/tools/shell.rs` の description 書式を確認し、\
             書式を変えたならこのパースを追随させること"
    );
}

/// owner 向けの `manage_allowed_commands(action="list")` も同じ実効リストを返す。
///
/// 一覧を返す口が 2 つあるので、片方だけ直すと「どちらを読んだか」で挙動が割れる。
#[tokio::test]
async fn manage_allowed_commands_list_returns_the_same_effective_list() {
    let state = state_with_shell_commands(&["cargo"], &["ls"]);
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", "mkdir", "owner")
            .unwrap();
    }
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute(
            "manage_allowed_commands",
            &json!({"action": "list"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(r.data.unwrap()["commands"], json!(["cargo", "ls", "mkdir"]));
}

// ---- #311: 一覧は execute_shell の登録ゲートに追従する ----
//
// 実際の run は `register_tools_from_config`（crates/actions/src/tools/mod.rs）で
// `tools.enabled` と `tools.shell.enabled` の 2 段ゲートを通り、どちらかが false なら
// execute_shell 自体が登録されない。ゲートを閉じた構成で一覧が「実行できない
// コマンド」を返すと、エージェントが「使える」と誤認する（#311）。
// 下の 3 本はゲートが**空を返す**こと、4 本目はゲートが開いているとき従来どおり
// 返ること（恒真回避の対照）を固定する。ゲートを外すと下 3 本が赤くなる。

/// `tools.enabled = false` のとき一覧は空。
/// ゲートが無ければ `["ls", "cat"]` が返るので、空であることがゲートの証拠。
#[tokio::test]
async fn list_is_empty_when_tools_disabled() {
    let state = state_with_shell(&["ls", "cat"]);
    state.tools_config.write().unwrap().enabled = false;
    assert!(
        listed_commands(&state).await.is_empty(),
        "tools.enabled=false でも一覧が空になっていない（#311）"
    );
}

/// `tools.shell.enabled = false` のとき一覧は空（shell 設定はあるが無効）。
#[tokio::test]
async fn list_is_empty_when_shell_disabled() {
    let state = state_with_shell(&["ls", "cat"]);
    state
        .tools_config
        .write()
        .unwrap()
        .shell
        .as_mut()
        .unwrap()
        .enabled = false;
    assert!(
        listed_commands(&state).await.is_empty(),
        "tools.shell.enabled=false でも一覧が空になっていない（#311）"
    );
}

/// `shell` そのものが無い構成でも空（`tools.enabled=true` だが shell=None）。
#[tokio::test]
async fn list_is_empty_when_shell_absent() {
    let state = state_with_shell(&["ls"]);
    {
        let mut cfg = state.tools_config.write().unwrap();
        cfg.enabled = true;
        cfg.shell = None;
    }
    assert!(
        listed_commands(&state).await.is_empty(),
        "shell 設定が無いのに一覧が空でない（#311）"
    );
}

/// 両ゲートが true のときだけ従来どおり実効リストを返す（上の空テストが恒真でない対照）。
#[tokio::test]
async fn list_returns_commands_when_both_gates_enabled() {
    // state_with_shell の既定は enabled=true / shell.enabled=true。
    let state = state_with_shell(&["ls", "cat"]);
    assert_eq!(
        listed_commands(&state).await,
        vec!["ls", "cat"],
        "両ゲート true でも従来の実効リストが返らない（#311 のゲートが過剰）"
    );
}

/// owner 向けの `manage_allowed_commands(action="list")` も同じゲートに従う。
/// 一覧の口が 2 つあるので、片方だけ塞いで漏れないことを確かめる（#311）。
#[tokio::test]
async fn manage_allowed_commands_list_is_empty_when_tools_disabled() {
    let state = state_with_shell(&["ls", "cat"]);
    state.tools_config.write().unwrap().enabled = false;
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute(
            "manage_allowed_commands",
            &json!({"action": "list"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(
        r.data.unwrap()["commands"],
        json!([]),
        "manage 経路が list 経路と食い違い、ゲートを漏れている（#311）"
    );
}

/// **レスポンス JSON が移設前と同一**（記憶インデックス設定）。
/// `previous` / `current` の入れ子形をリテラルで固定する。
#[tokio::test]
async fn update_memory_index_config_response_json_is_unchanged() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    // 未設定からの更新: previous は既定値。
    let r = actions
        .execute(
            "update_memory_index_config",
            &json!({"batch_size": 10}),
            &agent_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "agent_id": "agent-x",
            "previous": {
                "batch_size": opencrab_db::queries::BATCH_SIZE_DEFAULT,
                "threshold": opencrab_db::queries::THRESHOLD_DEFAULT,
            },
            "current": { "batch_size": 10, "threshold": opencrab_db::queries::THRESHOLD_DEFAULT },
        })
    );

    // 片方だけ指定すると、もう片方は現状維持。
    let r = actions
        .execute(
            "update_memory_index_config",
            &json!({"threshold": 5}),
            &agent_ctx(),
        )
        .await;
    assert!(r.success);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "agent_id": "agent-x",
            "previous": { "batch_size": 10, "threshold": opencrab_db::queries::THRESHOLD_DEFAULT },
            "current": { "batch_size": 10, "threshold": 5 },
        })
    );

    // DB へ永続化されている。
    let conn = state.db.lock().unwrap();
    let cfg = opencrab_db::queries::get_memory_index_config(&conn, "agent-x").unwrap();
    assert_eq!((cfg.batch_size, cfg.threshold), (10, 5));
}

/// 引数が両方欠けているときは移設前と同じ文言で失敗する。
#[tokio::test]
async fn update_memory_index_config_requires_at_least_one_field() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let r = actions
        .execute("update_memory_index_config", &json!({}), &agent_ctx())
        .await;
    assert!(!r.success);
    assert_eq!(
        r.error.as_deref(),
        Some("batch_sizeまたはthresholdの少なくとも1つが必要です")
    );
}

/// 移設した 4 ツールは **inner（Discord）へ委譲しない**。
///
/// `cancel_subtask` / `report_progress` は Discord 固有の後処理を保つため委譲する
/// が、この 4 つは Discord 側の実装を撤去したので own が処理しなければならない。
/// 委譲パターンで書くと、Discord が誤って再定義したときに own の実装が黙って
/// バイパスされる。
#[tokio::test]
async fn generic_management_tools_are_not_delegated_to_inner() {
    let state = state_with_shell(&[]);
    let inner = Arc::new(RecordingInner::new(&[
        "update_memory_index_config",
        "add_allowed_command",
        "list_allowed_commands",
        "remove_allowed_command",
    ]));
    let actions = SystemGatewayActions::new(
        state.clone(),
        Some(inner.clone() as Arc<dyn GatewayActions>),
        None,
        None,
    );

    for (name, args) in [
        ("update_memory_index_config", json!({"batch_size": 7})),
        ("add_allowed_command", json!({"command": "curl"})),
        ("list_allowed_commands", json!({})),
        ("remove_allowed_command", json!({"command": "curl"})),
    ] {
        let r = actions.execute(name, &args, &owner_ctx()).await;
        assert!(r.success, "{name}: {:?}", r.error);
        assert!(
            r.data.as_ref().unwrap().get("reached_inner").is_none(),
            "{name} が inner へ委譲されている（own が処理すべき）"
        );
    }
    assert!(
        inner.calls().is_empty(),
        "inner へ到達してはならない: {:?}",
        inner.calls()
    );
}

/// **transport gateway が inner に居ても（REST + Discord 構成）漏れないことの固定**。
///
/// このテストは**旧 `hot_reload_reaches_the_shared_config_even_with_a_transport_inner`
/// の反転**である。旧テストは「inner が居てもグローバル設定に反映される」ことを
/// 不変条件として固定していたが、それは #202 の漏れそのものだった。
///
/// 経緯（#197 との関係）: REST（`crate::api::agents_messages`）は Discord が有効な
/// とき `SystemGatewayActions { inner: Some(DiscordGatewayActions) }` を組む。移設前は
/// その Discord gateway へ `Arc::new(RwLock::new(state.tools_config.read().clone()))`
/// ＝**使い捨てのコピー**を渡していた。そのおかげで REST 経路は**偶然この漏れが
/// 無かった**。素朴に移設すると共有実体へ届いて漏れる側に揃ってしまうため、同じ
/// 変更でグローバル書き込みを撤去した。
///
/// #197 について構造面で言えることは、`DiscordGatewayActions::new` がもう実行許可
/// 設定を受け取らない（引数自体が消えた）＝**別インスタンスを作る余地がコンパイル時に
/// 無い**という点だけである。
#[tokio::test]
async fn add_allowed_command_does_not_leak_to_the_global_config_with_a_transport_inner() {
    let state = state_with_shell(&[]);
    // REST + Discord 相当: transport gateway が inner に居る構成。
    let inner = Arc::new(RecordingInner::new(&["discord_send_file"]));
    let actions = SystemGatewayActions::new(
        state.clone(),
        Some(inner as Arc<dyn GatewayActions>),
        None,
        None,
    );

    let r = actions
        .execute(
            "add_allowed_command",
            &json!({"command": "curl"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);

    // DB にだけ入る。
    assert_eq!(db_allowed_commands(&state, "agent-x"), vec!["curl"]);
    assert!(
        live_allowed_commands(&state).is_empty(),
        "inner の有無に関わらずグローバル設定へ書いてはならない（#202）: {:?}",
        live_allowed_commands(&state)
    );
}

// ================================================================================
// #157 S6: スキル生成（create_skill）の移植テスト
//
// 旧 Discord 実装（`crates/discord` の `gateway_actions/agent_management.rs`）にあった
// 3 テスト（基本 / 同名 dedup / 非 trusted 拒否）をそのまま持ってきたもの（1 件も
// 落としていない）＋ 移設の本題（非 Discord 構成でも定義に現れる）・inner へ委譲
// しないこと・レスポンス JSON / エラー文言 / `source_type` のリテラル固定。
// ================================================================================

fn co_agent_ctx() -> GatewayCallContext {
    GatewayCallContext::new(
        GatewayCaller::CoAgent {
            agent_id: "agent-peer".to_string(),
        },
        "agent-x",
    )
}

/// DB 上のスキル（アーカイブ済みも含む）を取得する。
fn db_skill(state: &AppState, name: &str) -> Option<opencrab_db::queries::SkillRow> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::find_skill_by_name_any(&conn, "agent-x", name).unwrap()
}

/// **#157 S6 の本題**: `create_skill` が own 定義にちょうど 1 件ある。
///
/// own 定義は transport の有無に依存しないため、これが `definitions()` に出ることは
/// 「web / Nostr / REST / heartbeat でも使える」ことと同義。own から消えると Discord
/// 専用に逆戻りする（それが #157 が報告している不具合そのもの）。
#[test]
fn create_skill_is_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    assert_eq!(
        defs.iter().filter(|d| d.name == "create_skill").count(),
        1,
        "create_skill は own 定義にちょうど 1 件必要（#157 S6）"
    );
}

/// **Discord 無効の構成でも露出する**（#157 S6 の証明）。
///
/// `inner = None` は「transport 固有 gateway が居ない」経路（web / REST / Nostr /
/// heartbeat、および Discord feature 無効ビルド）そのもの。移設前はこの構成で
/// `create_skill` が一切出なかった。
#[test]
fn create_skill_is_exposed_without_any_transport_gateway() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    assert!(
            names.contains(&"create_skill".to_string()),
            "transport gateway 無しの構成で create_skill が露出しない（#157 の不具合そのもの）: {names:?}"
        );
}

/// 定義（description / 引数スキーマ）を移設前（Discord 定義）と 1 バイトも変えない。
///
/// description は LLM がツールを選ぶ唯一の手がかりなので、文言が変わると挙動が変わる。
#[test]
fn create_skill_definition_matches_the_discord_original() {
    let defs = SystemGatewayActions::own_definitions();
    let d = defs.iter().find(|d| d.name == "create_skill").unwrap();
    assert_eq!(
            d.description,
            "ユーザーから「〇〇するスキルを作って」と言われたとき新しいスキルを作成する。guidanceにコマンド例・使い方を書くことで、LLMがexecute_shellで動的に実行できるようになる。同名スキルが存在する場合は更新される。"
        );
    assert_eq!(d.parameters["type"], json!("object"));
    assert_eq!(d.parameters["required"], json!(["name", "description"]));
    let props = d.parameters["properties"].as_object().unwrap();
    let mut keys: Vec<&str> = props.keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["description", "guidance", "name"]);
    for k in ["name", "description", "guidance"] {
        assert_eq!(props[k]["type"], json!("string"), "{k}");
    }
    assert_eq!(props["name"]["description"], json!("スキル名"));
    assert_eq!(props["description"]["description"], json!("スキルの説明"));
    assert_eq!(
        props["guidance"]["description"],
        json!("スキルのガイダンス（省略時は空文字列）")
    );
}

/// **inner へ委譲されない**（own が唯一の実装）。
///
/// 委譲パターンのまま残すと、Discord が誤って再定義したときに own の実装が黙って
/// バイパスされる（#155 の後退）。
#[tokio::test]
async fn create_skill_is_not_delegated_to_inner() {
    let state = crate::test_app_state();
    let inner = Arc::new(RecordingInner::new(&["create_skill"]));
    let actions = SystemGatewayActions::new(state.clone(), Some(inner.clone()), None, None);

    let r = actions
        .execute(
            "create_skill",
            &json!({"name": "天気確認", "description": "curl wttr.in で天気を確認する"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert!(
        inner.calls().is_empty(),
        "create_skill が inner へ委譲された: {:?}",
        inner.calls()
    );
    // own の実装が実際に走った証拠（inner のフェイクは DB を触らない）。
    assert!(db_skill(&state, "天気確認").is_some());
}

/// 移植: 基本の作成。レスポンス JSON のキーと `action` の値、DB に書く
/// `source_type` / `permission` / `situation_pattern` をリテラルで固定する。
#[tokio::test]
async fn create_skill_basic() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let result = actions
        .execute(
            "create_skill",
            &json!({
                "name": "天気確認",
                "description": "curl wttr.inで天気を確認する"
            }),
            &owner_ctx(),
        )
        .await;
    assert!(result.success, "create_skill should succeed");
    let data = result.data.unwrap();
    assert!(data["id"].is_string(), "should return id");
    assert_eq!(data["name"], json!("天気確認"));
    assert_eq!(data["action"], json!("created"));
    let mut keys: Vec<&str> = data
        .as_object()
        .unwrap()
        .keys()
        .map(|s| s.as_str())
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["action", "id", "name"]);

    // 記録される取得元（`source_type`）を移設で変えない。core の `create_my_skill` は
    // `"self_created"` を書く**別のツール**（#157 では統廃合しない）。
    let row = db_skill(&state, "天気確認").unwrap();
    assert_eq!(row.source_type, "acquired");
    assert_eq!(row.permission, "\"agent\"");
    assert_eq!(row.situation_pattern, "");
    assert_eq!(row.guidance, "", "guidance 省略時は空文字列");
    assert!(row.is_active);
    assert!(!row.archived);
}

/// 移植: 同名は upsert（`action="updated"`。行は増えない）。
#[tokio::test]
async fn create_skill_dedup() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let first = actions
        .execute(
            "create_skill",
            &json!({
                "name": "天気確認",
                "description": "first version"
            }),
            &owner_ctx(),
        )
        .await;
    assert!(first.success);
    let result2 = actions
        .execute(
            "create_skill",
            &json!({
                "name": "天気確認",
                "description": "updated version",
                "guidance": "curl wttr.in"
            }),
            &owner_ctx(),
        )
        .await;
    assert!(result2.success, "second create should succeed (dedup)");
    let data = result2.data.unwrap();
    assert_eq!(data["action"], json!("updated"));
    // 同じ行が更新される（id 不変・description / guidance だけ差し替わる）。
    assert_eq!(data["id"], first.data.unwrap()["id"]);
    let row = db_skill(&state, "天気確認").unwrap();
    assert_eq!(row.description, "updated version");
    assert_eq!(row.guidance, "curl wttr.in");
    let conn = state.db.lock().unwrap();
    let all = opencrab_db::queries::list_skills(&conn, "agent-x", false).unwrap();
    assert_eq!(all.len(), 1, "同名で行が増えてはならない");
}

/// アーカイブ済みの同名スキルは復活する（`action="restored"` / archived=false）。
#[tokio::test]
async fn create_skill_restores_archived_skill() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    assert!(
        actions
            .execute(
                "create_skill",
                &json!({"name": "天気確認", "description": "v1"}),
                &owner_ctx(),
            )
            .await
            .success
    );
    {
        let conn = state.db.lock().unwrap();
        let mut row = opencrab_db::queries::find_skill_by_name_any(&conn, "agent-x", "天気確認")
            .unwrap()
            .unwrap();
        row.archived = true;
        row.is_active = false;
        opencrab_db::queries::update_skill(&conn, &row).unwrap();
    }

    let r = actions
        .execute(
            "create_skill",
            &json!({"name": "天気確認", "description": "v2"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(r.data.unwrap()["action"], json!("restored"));
    let row = db_skill(&state, "天気確認").unwrap();
    assert!(!row.archived);
    assert!(row.is_active);
    assert_eq!(row.description, "v2");
}

/// 移植: 非 trusted（素の Agent）は拒否。**エラー文言はバイト単位で移設前と同一。**
///
/// このゲートは**二重構造**である: bridge の `TRUSTED_ONLY_ACTIONS` が可視性と実行の
/// 双方を（名前ベースで）ゲートし、ハンドラ内の `matches!` が多層防御として残る。
/// bridge 側は名前で引くので移設しても効き続ける（そのことをここで固定する）。
/// なお**ハンドラ側の拒否はマーカー無し**（`REJECTION_CODE_PREFIX` を付けない）で、
/// これも移設前と同じ形。
#[tokio::test]
async fn create_skill_rejected_for_non_owner() {
    assert!(
        opencrab_actions::TRUSTED_ONLY_ACTIONS.contains(&"create_skill"),
        "bridge 側の trusted ゲートが消えたら、ハンドラ内検査が唯一のゲートになる"
    );

    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let result = actions
        .execute(
            "create_skill",
            &json!({
                "name": "test",
                "description": "test"
            }),
            &agent_ctx(),
        )
        .await;
    assert!(!result.success);
    let err = result.error.unwrap();
    assert!(err.contains("trusted user"));
    assert_eq!(
        err, "このアクションはtrusted userのみ実行できます",
        "拒否文言は移設前と 1 バイトも変えない"
    );
    assert!(
        !err.starts_with(REJECTION_CODE_PREFIX),
        "マーカーの有無も移設前と同じ（付けない）"
    );
    // 副作用ゼロ。
    assert!(db_skill(&state, "test").is_none());
}

/// trusted_user / co_agent は実行できる（許可集合を移設で狭めない）。
#[tokio::test]
async fn create_skill_allowed_for_trusted_user_and_co_agent() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    for (i, ctx) in [trusted_ctx(), co_agent_ctx()].into_iter().enumerate() {
        let name = format!("skill-{i}");
        let r = actions
            .execute(
                "create_skill",
                &json!({"name": name, "description": "d"}),
                &ctx,
            )
            .await;
        assert!(
            r.success,
            "{:?} は実行できるべき: {:?}",
            ctx.caller, r.error
        );
        assert!(db_skill(&state, &name).is_some());
    }
}

/// 必須引数エラーの文言（英語のまま・マーカー無し）を固定する。
#[tokio::test]
async fn create_skill_missing_arguments_keep_original_messages() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);

    let r = actions
        .execute("create_skill", &json!({}), &owner_ctx())
        .await;
    assert!(!r.success);
    assert_eq!(r.error.as_deref(), Some("name is required"));

    let r = actions
        .execute("create_skill", &json!({"name": "n"}), &owner_ctx())
        .await;
    assert!(!r.success);
    assert_eq!(r.error.as_deref(), Some("description is required"));
}

/// 分類の所属を移設で変えない（Discord でも dispatchable だった）。分類の権威は
/// `own_definitions()` の `class.dispatch` 属性なので、それを直接見る。
#[test]
fn create_skill_stays_dispatchable() {
    use opencrab_gateway::DispatchMode;
    let defs = SystemGatewayActions::own_definitions();
    let class = defs
        .iter()
        .find(|d| d.name == "create_skill")
        .expect("create_skill が own_definitions() に無い")
        .class;
    assert_eq!(
        class.dispatch,
        DispatchMode::Dispatchable,
        "create_skill は移設前と同じく dispatch 対象に残す（結果を同ターンで使わない）"
    );
}

// ================================================================================
// #157 S2 / #184: 停止（cancel_subtask）の移植テスト
//
// 旧 Discord 実装（`crates/discord` の `execute_cancel_subtask`）にあった 8 テストを
// そのまま持ってきたもの（1 件も落としていない）。停止の実装は
// `opencrab_actions::cancel_subtask` 1 箇所になったので、契約はこの合成層で固定する。
// ================================================================================

/// 停止対象を任意の label / tool_name で 1 件登録した registry を作る。
fn registry_with_labeled(
    subtask_id: &str,
    session_id: &str,
    parent_session_id: &str,
    label: &str,
    tool_name: &str,
) -> SubtaskRegistry {
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    registry.insert(
        subtask_id.to_string(),
        opencrab_actions::SpawnedSubtask {
            abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            session_id: session_id.to_string(),
            parent_session_id: parent_session_id.to_string(),
            agent_id: "agent-x".to_string(),
            label: label.to_string(),
            tool_name: tool_name.to_string(),
            started_at: std::time::Instant::now(),
            reply_target: None,
            caller: opencrab_actions::CallerIdentity::Agent,
            lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            steerable: false,
        },
    );
    registry
}

/// sub-session の行を作る（明示的な `spawn_subtask` 相当）。
fn insert_sub_session(state: &AppState, session_id: &str, theme: &str) {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::insert_session(
        &conn,
        &opencrab_db::queries::SessionRow {
            id: session_id.to_string(),
            mode: "subtask".to_string(),
            theme: theme.to_string(),
            phase: "active".to_string(),
            turn_number: 0,
            status: "active".to_string(),
            participant_ids_json: json!(["agent-x"]).to_string(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: None,
        },
    )
    .unwrap();
}

/// 停止ログ（`tool_cancelled`）を親セッションから 1 件だけ引く。
fn cancelled_log(state: &AppState, parent_session_id: &str) -> opencrab_db::queries::SessionLogRow {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::list_recent_session_logs(&conn, parent_session_id, 20)
        .unwrap()
        .into_iter()
        .find(|l| l.log_type == "tool_cancelled")
        .expect("tool_cancelled が親ログに残る")
}

fn cancelled_log_metadata(state: &AppState, parent_session_id: &str) -> Value {
    serde_json::from_str(
        cancelled_log(state, parent_session_id)
            .metadata_json
            .as_deref()
            .unwrap(),
    )
    .unwrap()
}

fn parent_ctx(parent_session_id: &str) -> GatewayCallContext {
    GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id(parent_session_id)
}

async fn cancel(
    actions: &SystemGatewayActions,
    subtask_id: &str,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    actions
        .execute("cancel_subtask", &json!({"subtask_id": subtask_id}), ctx)
        .await
}

/// 不在は**権限拒否ではない**プレーンなエラー（旧 Discord テストの移植）。
#[tokio::test]
async fn cancel_subtask_not_found_is_plain_error() {
    let state = crate::test_app_state();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let actions = SystemGatewayActions::new(state, None, Some(registry), None);
    let r = cancel(&actions, "no-such", &parent_ctx("web-agent-x-c1")).await;
    assert!(!r.success);
    let err = r.error.unwrap();
    assert_eq!(err, "cancel_subtask: subtask 'no-such' not found");
    assert!(!err.starts_with(REJECTION_CODE_PREFIX));
}

/// 他セッションが親の subtask は拒否し、エントリも残す（abort しない）。
#[tokio::test]
async fn cancel_subtask_rejects_foreign_session() {
    let state = crate::test_app_state();
    let registry = registry_with("st-x", "subtask-x1", "web-other-c9");
    let actions = SystemGatewayActions::new(state, None, Some(registry.clone()), None);
    let r = cancel(&actions, "st-x", &parent_ctx("web-agent-x-c1")).await;
    assert!(!r.success);
    assert_eq!(
            r.error.as_deref().unwrap(),
            format!("{REJECTION_CODE_PREFIX}cancel_subtask: subtask 'st-x' をこのセッションからキャンセルする権限がありません（親セッションまたは owner のみ）")
        );
    assert!(registry.contains_key("st-x"), "abort されていない");
}

/// 親セッションからの停止は成功し、registry から除去される。
#[tokio::test]
async fn cancel_subtask_allows_parent_session() {
    let state = crate::test_app_state();
    let parent = "web-agent-x-c1";
    let registry = registry_with("st-mine", "subtask-m1", parent);
    let actions = SystemGatewayActions::new(state, None, Some(registry.clone()), None);
    let r = cancel(&actions, "st-mine", &parent_ctx(parent)).await;
    assert!(r.success, "{:?}", r.error);
    // レスポンス JSON も旧実装と同一。
    assert_eq!(
        r.data.unwrap(),
        json!({"cancelled": true, "subtask_id": "st-mine"})
    );
    assert!(!registry.contains_key("st-mine"));
}

/// owner は無関係なセッション文脈からでも停止できる。
#[tokio::test]
async fn cancel_subtask_owner_bypasses_session_check() {
    let state = crate::test_app_state();
    let registry = registry_with("st-any", "subtask-a1", "web-other-c9");
    let actions = SystemGatewayActions::new(state, None, Some(registry.clone()), None);
    let r = cancel(&actions, "st-any", &owner_ctx()).await;
    assert!(r.success, "{:?}", r.error);
    assert!(!registry.contains_key("st-any"));
}

/// セッション文脈の無い agent は他人の subtask を停止できない。
#[tokio::test]
async fn cancel_subtask_rejects_agent_without_session() {
    let state = crate::test_app_state();
    let registry = registry_with("st-ns", "subtask-n1", "web-other-c9");
    let actions = SystemGatewayActions::new(state, None, Some(registry.clone()), None);
    let r = cancel(&actions, "st-ns", &agent_ctx()).await;
    assert!(!r.success);
    assert!(r
        .error
        .as_deref()
        .unwrap()
        .starts_with(REJECTION_CODE_PREFIX));
    assert!(registry.contains_key("st-ns"));
}

/// #176: 自動 dispatch した subtask は sub-session の行を持たないため theme を引けず、
/// registry の label（ツール名を含む）へフォールバックする。
#[tokio::test]
async fn cancel_subtask_falls_back_to_label_without_sub_session() {
    let state = crate::test_app_state();
    let parent = "web-agent-x-c1";
    // sub-session は**作らない**（自動 dispatch の再現）。
    let registry = registry_with_labeled(
        "st-auto",
        "subtask-auto1",
        parent,
        "execute_shell(ls -la)",
        "execute_shell",
    );
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);
    let r = cancel(&actions, "st-auto", &parent_ctx(parent)).await;
    assert!(r.success, "{:?}", r.error);

    let log = cancelled_log(&state, parent);
    assert_ne!(
        log.content, "subtask '' was cancelled",
        "sub-session が無いとラベルが空になっている（#176 の退行）"
    );
    assert_eq!(log.content, "subtask 'execute_shell(ls -la)' was cancelled");
    let meta = cancelled_log_metadata(&state, parent);
    assert_eq!(meta["task"], "execute_shell(ls -la)");
    // #184: 種別名は固定値ではなく**実際に停止したツール名**。
    assert_eq!(meta["tool_name"], "execute_shell");
    assert_eq!(meta["tool_call_id"], "st-auto");
    assert_eq!(meta["label"], "execute_shell(ls -la)");
    assert_eq!(meta["completed_calls"], json!([]));
}

/// 明示的な `spawn_subtask`（sub-session あり）では theme を使い、`Subtask: ` prefix を
/// 除去する。
#[tokio::test]
async fn cancel_subtask_prefers_sub_session_theme() {
    let state = crate::test_app_state();
    let parent = "web-agent-x-c1";
    insert_sub_session(&state, "subtask-explicit1", "Subtask: ログを調査する");
    let registry = registry_with_labeled(
        "st-explicit",
        "subtask-explicit1",
        parent,
        "spawn_subtask(ログを調査する)",
        "spawn_subtask",
    );
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);
    let r = cancel(&actions, "st-explicit", &parent_ctx(parent)).await;
    assert!(r.success, "{:?}", r.error);

    assert_eq!(
        cancelled_log(&state, parent).content,
        "subtask 'ログを調査する' was cancelled"
    );
    let meta = cancelled_log_metadata(&state, parent);
    assert_eq!(meta["task"], "ログを調査する");
    assert_eq!(meta["tool_name"], "spawn_subtask");
}

/// sub-session はあるが theme が空のケースでも label へフォールバックする。
#[tokio::test]
async fn cancel_subtask_falls_back_on_empty_theme() {
    let state = crate::test_app_state();
    let parent = "web-agent-x-c1";
    insert_sub_session(&state, "subtask-empty1", "");
    let registry = registry_with_labeled(
        "st-empty",
        "subtask-empty1",
        parent,
        "nostr_generate_key(main)",
        "nostr_generate_key",
    );
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);
    let r = cancel(&actions, "st-empty", &parent_ctx(parent)).await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(
        cancelled_log(&state, parent).content,
        "subtask 'nostr_generate_key(main)' was cancelled"
    );
}

/// 旧 Discord 実装の固有の後始末その 1: **中断を lifecycle 通知口へ伝え、随伴マップ
/// から外す**。落とすと lifecycle webhook の `aborted` が黙って消える。
#[tokio::test]
async fn cancel_subtask_notifies_the_run_notifier() {
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<String>>);
    impl opencrab_actions::subtask_notify::SubtaskRunNotifier for Recorder {
        fn on_cancelled(&self, _duration_ms: u64) {
            self.0.lock().unwrap().push("cancelled".to_string());
        }
    }

    let state = crate::test_app_state();
    let recorder = Arc::new(Recorder::default());
    state
        .subtask_notifiers
        .insert("st-1".to_string(), recorder.clone());
    let parent = "web-agent-x-c1";
    let registry = registry_with("st-1", "subtask-st-1", parent);
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);

    let r = cancel(&actions, "st-1", &parent_ctx(parent)).await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(recorder.0.lock().unwrap().clone(), vec!["cancelled"]);
    assert!(
        !state.subtask_notifiers.contains_key("st-1"),
        "通知口は registry と対で除去する"
    );
}

/// **停止も完了 sink（`on_subtask_cancelled`）へ通知する**（#184 / REST の永久 active
/// バグ）。委譲していた頃の Discord 経路はこれを落としていた。
#[tokio::test]
async fn cancel_subtask_notifies_the_completion_sink() {
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<String>>);
    impl SubtaskCompletionSink for Recorder {
        fn session_prefix(&self) -> &'static str {
            ""
        }
        fn forwards_progress(&self) -> bool {
            true
        }
        fn deliver_continuation(&self, _ev: SubtaskSettled) {
            self.0.lock().unwrap().push("settled".to_string());
        }
        fn on_subtask_cancelled(&self, ev: SubtaskSettled) {
            self.0
                .lock()
                .unwrap()
                .push(format!("cancelled:{}:{}", ev.subtask_id, ev.exit_reason));
        }
    }

    let state = crate::test_app_state();
    let parent = "web-agent-x-c1";
    let registry = registry_with("st-1", "subtask-st-1", parent);
    let sink = Arc::new(Recorder::default());
    let actions = SystemGatewayActions::new(
        state,
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    let r = cancel(&actions, "st-1", &parent_ctx(parent)).await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(
        sink.0.lock().unwrap().clone(),
        vec!["cancelled:st-1:cancelled"],
        "停止は on_subtask_cancelled だけを呼ぶ（resume する on_subtask_settled は呼ばない）"
    );
}

/// **negative assert（#157 S2）**: Discord が `cancel_subtask` を再定義しても own が
/// 処理する。委譲パターンに戻すと own の後始末（通知・部分結果ログ・sink）が黙って
/// バイパスされるので、その経路を作らせない。
#[tokio::test]
async fn cancel_subtask_is_not_delegated_to_inner() {
    let state = crate::test_app_state();
    let parent = "web-agent-x-c1";
    let registry = registry_with("st-1", "subtask-st-1", parent);
    let inner = Arc::new(RecordingInner::new(&["cancel_subtask"]));
    let actions = SystemGatewayActions::new(
        state,
        Some(inner.clone() as Arc<dyn GatewayActions>),
        Some(registry.clone()),
        None,
    );

    let r = cancel(&actions, "st-1", &parent_ctx(parent)).await;
    assert!(r.success, "{:?}", r.error);
    assert!(
        r.data.as_ref().unwrap().get("reached_inner").is_none(),
        "cancel_subtask が inner へ委譲されている（own が処理すべき）"
    );
    assert!(
        inner.calls().is_empty(),
        "inner へ到達してはならない: {:?}",
        inner.calls()
    );
    assert!(!registry.contains_key("st-1"), "own が実際に停止している");
}

/// merge 後も `cancel_subtask` は 1 件（own 優先で dedup）。
#[test]
fn merge_definitions_still_dedups_cancel_subtask() {
    let inner: Arc<dyn GatewayActions> = Arc::new(RecordingInner::new(&["cancel_subtask"]));
    let merged = SystemGatewayActions::merge_definitions(
        SystemGatewayActions::own_definitions(),
        Some(&inner),
    );
    assert_eq!(
        merged.iter().filter(|d| d.name == "cancel_subtask").count(),
        1
    );
}

// ================================================================================
// #157 S3: ハートビート指示ツールの移植テスト
//
// 旧 Discord 実装（`crates/discord` の `heartbeat_instructions.rs`）にあった 4 テストを
// そのまま持ってきたもの（1 件も落としていない）＋ 移設の本題（非 Discord 構成でも
// 定義に現れる）とレスポンス JSON / エラー文言のリテラル固定。
// ================================================================================

fn trusted_ctx() -> GatewayCallContext {
    GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x")
}

/// エージェント行を用意する（`scope="agent"` の patch 対象）。
fn insert_agent(state: &AppState, heartbeat_instructions: &str) {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: "agent-x".to_string(),
            name: "N".to_string(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "P".to_string(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: heartbeat_instructions.to_string(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
}

fn audit_rows(state: &AppState) -> Vec<opencrab_db::queries::HeartbeatInstructionsAuditRow> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::list_heartbeat_instructions_audit(&conn, "agent-x", 10).unwrap()
}

/// **#157 S3 の本題**: 2 ツールが own 定義（= transport の有無に依存せず全ターンで
/// 露出する）。own から消えると Discord 専用に逆戻りする。
#[test]
fn heartbeat_instruction_tools_are_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    for name in [
        "update_heartbeat_instructions",
        "read_heartbeat_instructions",
    ] {
        assert_eq!(
            defs.iter().filter(|d| d.name == name).count(),
            1,
            "{name} は own 定義にちょうど 1 件必要（#157 S3）"
        );
    }
    let update = defs
        .iter()
        .find(|d| d.name == "update_heartbeat_instructions")
        .unwrap();
    let required = update.parameters["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "scope"));
    assert!(required.iter().any(|v| v == "instructions"));
    let props = update.parameters["properties"].as_object().unwrap();
    for key in ["scope", "channel_id", "guild_id", "instructions", "reason"] {
        assert!(props.contains_key(key), "missing property: {key}");
    }
    let read = defs
        .iter()
        .find(|d| d.name == "read_heartbeat_instructions")
        .unwrap();
    assert!(read.parameters["required"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "scope"));
}

/// **Discord 無効の構成でも定義に現れる**（#157 の本題）。inner=None は
/// web / Nostr / REST / heartbeat 経路そのもの。
#[test]
fn heartbeat_instruction_tools_are_visible_without_discord() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    assert!(names.contains(&"update_heartbeat_instructions".to_string()));
    assert!(names.contains(&"read_heartbeat_instructions".to_string()));
    // 停止も同様（#157 S2）。
    assert!(names.contains(&"cancel_subtask".to_string()));
}

/// owner 以外は拒否し、監査ログも残さない（旧 Discord テストの移植）。
#[tokio::test]
async fn update_heartbeat_instructions_rejected_for_non_owner() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({"scope": "agent", "instructions": "話題があるときだけ話す"}),
            &trusted_ctx(),
        )
        .await;
    assert!(!r.success);
    assert_eq!(
        r.error.as_deref(),
        Some("このアクションはオーナーのみ実行できます")
    );
    assert!(audit_rows(&state).is_empty(), "監査ログを残してはならない");
}

/// owner は成功し、DB へ反映され、監査ログに old/new/reason が残る（旧テストの移植）。
/// レスポンス JSON もリテラルで固定する。
#[tokio::test]
async fn update_heartbeat_instructions_owner_success_and_audit() {
    let state = crate::test_app_state();
    insert_agent(&state, "OLD");
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({
                "scope": "agent",
                "instructions": "NEW指示",
                "reason": "オーナー依頼",
            }),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "success": true,
            "scope": "agent",
            "channel_id": Value::Null,
            "length": 5,
            "preview": "NEW指示",
        })
    );

    {
        let conn = state.db.lock().unwrap();
        let got = opencrab_db::queries::get_agent(&conn, "agent-x")
            .unwrap()
            .unwrap();
        assert_eq!(got.heartbeat_instructions, "NEW指示");
    }
    let rows = audit_rows(&state);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].scope, "agent");
    assert_eq!(rows[0].old_value.as_deref(), Some("OLD"));
    assert_eq!(rows[0].new_value.as_deref(), Some("NEW指示"));
    assert_eq!(rows[0].reason.as_deref(), Some("オーナー依頼"));
    assert_eq!(rows[0].caller_identity, GatewayCaller::Owner.label());
}

/// エージェント行が無ければ移設前と同じ文言で失敗する。
#[tokio::test]
async fn update_heartbeat_instructions_missing_agent_and_bad_args() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);

    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({"scope": "agent", "instructions": "x"}),
            &owner_ctx(),
        )
        .await;
    assert_eq!(r.error.as_deref(), Some("エージェントが見つかりません"));

    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({"scope": "agent"}),
            &owner_ctx(),
        )
        .await;
    assert_eq!(r.error.as_deref(), Some("instructionsパラメータが必要です"));

    let too_long = "あ".repeat(opencrab_db::queries::MAX_HEARTBEAT_INSTRUCTIONS_LEN + 1);
    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({"scope": "agent", "instructions": too_long}),
            &owner_ctx(),
        )
        .await;
    assert_eq!(
        r.error.as_deref(),
        Some(
            format!(
                "instructionsが長すぎます（最大{}文字）",
                opencrab_db::queries::MAX_HEARTBEAT_INSTRUCTIONS_LEN
            )
            .as_str()
        )
    );

    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({"scope": "channel", "instructions": "x"}),
            &owner_ctx(),
        )
        .await;
    assert_eq!(
        r.error.as_deref(),
        Some("scope=channelのときはchannel_idが必要です")
    );

    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({"scope": "channel", "channel_id": "ch1", "instructions": "x"}),
            &owner_ctx(),
        )
        .await;
    assert_eq!(
        r.error.as_deref(),
        Some("新規チャンネル設定の作成にはguild_idが必要です")
    );

    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({"scope": "nope", "instructions": "x"}),
            &owner_ctx(),
        )
        .await;
    assert_eq!(
        r.error.as_deref(),
        Some("不明なscope: nope（agent または channel）")
    );
}

/// `scope="effective"` が解決結果（source + instructions）を返す（旧テストの移植）。
#[tokio::test]
async fn read_heartbeat_instructions_effective() {
    let state = crate::test_app_state();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_channel_config(
            &conn,
            &opencrab_db::queries::ChannelConfigRow {
                channel_id: "ch1".to_string(),
                agent_id: "agent-x".to_string(),
                guild_id: "g1".to_string(),
                channel_name: String::new(),
                readable: true,
                writable: true,
                whitelisted: false,
                heartbeat_enabled: true,
                heartbeat_interval_secs: None,
                heartbeat_instructions: "業務連絡のみ".to_string(),
            },
        )
        .unwrap();
    }
    let actions = SystemGatewayActions::new(state, None, None, None);
    let r = actions
        .execute(
            "read_heartbeat_instructions",
            &json!({"scope": "effective", "channel_id": "ch1"}),
            &trusted_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    let data = r.data.unwrap();
    assert_eq!(data["scope"], "effective");
    assert_eq!(data["channel_id"], "ch1");
    assert_eq!(data["source"], "channel");
    assert_eq!(data["instructions"], "業務連絡のみ");
}

/// 素の agent は拒否、co_agent は許可（旧テストの移植）。移設後も権限のゲートが効く。
#[tokio::test]
async fn read_heartbeat_instructions_rejected_for_plain_agent() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let r = actions
        .execute(
            "read_heartbeat_instructions",
            &json!({"scope": "agent"}),
            &agent_ctx(),
        )
        .await;
    assert!(!r.success);
    assert_eq!(
        r.error.as_deref(),
        Some("このアクションは信頼済みの呼び出し元のみ実行できます")
    );

    let allowed = actions
        .execute(
            "read_heartbeat_instructions",
            &json!({"scope": "agent"}),
            &GatewayCallContext::new(
                GatewayCaller::CoAgent {
                    agent_id: "co-agent-1".to_string(),
                },
                "agent-x",
            ),
        )
        .await;
    assert!(allowed.success, "{:?}", allowed.error);
    assert_eq!(
        allowed.data.unwrap(),
        json!({"scope": "agent", "instructions": ""})
    );
}

/// **チャンネル単位設定の非対称（#157 S3）**: 非 Discord 経路には通常チャンネル設定の
/// 行が無いので、`scope="channel"` は空文字列を返し、`scope="effective"` は
/// エージェント/既定へフォールバックする。エラーにはならない（露出はする）。
#[tokio::test]
async fn read_heartbeat_instructions_channel_scope_is_empty_without_a_channel_row() {
    let state = crate::test_app_state();
    insert_agent(&state, "エージェント既定の指示");
    let actions = SystemGatewayActions::new(state, None, None, None);

    let r = actions
        .execute(
            "read_heartbeat_instructions",
            &json!({"scope": "channel", "channel_id": "no-such-channel"}),
            &trusted_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "scope": "channel",
            "channel_id": "no-such-channel",
            "instructions": "",
        })
    );

    let r = actions
        .execute(
            "read_heartbeat_instructions",
            &json!({"scope": "effective", "channel_id": "no-such-channel"}),
            &trusted_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    let data = r.data.unwrap();
    assert_eq!(data["instructions"], "エージェント既定の指示");
    assert_eq!(data["source"], "agent");
}

/// 読み出しの引数エラー文言も移設前と同一。
#[tokio::test]
async fn read_heartbeat_instructions_bad_args() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let r = actions
        .execute(
            "read_heartbeat_instructions",
            &json!({"scope": "channel"}),
            &trusted_ctx(),
        )
        .await;
    assert_eq!(
        r.error.as_deref(),
        Some("scope=channelのときはchannel_idが必要です")
    );

    let r = actions
        .execute(
            "read_heartbeat_instructions",
            &json!({"scope": "nope"}),
            &trusted_ctx(),
        )
        .await;
    assert_eq!(
        r.error.as_deref(),
        Some("不明なscope: nope（agent / channel / effective）")
    );
}

/// **negative assert（#157 S3）**: Discord がハートビート指示ツールを再定義しても own が
/// 処理する（委譲パターンにしない）。
#[tokio::test]
async fn heartbeat_instruction_tools_are_not_delegated_to_inner() {
    let state = crate::test_app_state();
    insert_agent(&state, "OLD");
    let inner = Arc::new(RecordingInner::new(&[
        "update_heartbeat_instructions",
        "read_heartbeat_instructions",
    ]));
    let actions = SystemGatewayActions::new(
        state,
        Some(inner.clone() as Arc<dyn GatewayActions>),
        None,
        None,
    );

    for (name, args) in [
        (
            "update_heartbeat_instructions",
            json!({"scope": "agent", "instructions": "NEW"}),
        ),
        ("read_heartbeat_instructions", json!({"scope": "agent"})),
    ] {
        let r = actions.execute(name, &args, &owner_ctx()).await;
        assert!(r.success, "{name}: {:?}", r.error);
        assert!(
            r.data.as_ref().unwrap().get("reached_inner").is_none(),
            "{name} が inner へ委譲されている（own が処理すべき）"
        );
    }
    assert!(
        inner.calls().is_empty(),
        "inner へ到達してはならない: {:?}",
        inner.calls()
    );

    // merge 後も 1 件（own 優先で dedup）。
    let inner2: Arc<dyn GatewayActions> = Arc::new(RecordingInner::new(&[
        "update_heartbeat_instructions",
        "read_heartbeat_instructions",
    ]));
    let merged = SystemGatewayActions::merge_definitions(
        SystemGatewayActions::own_definitions(),
        Some(&inner2),
    );
    for name in [
        "update_heartbeat_instructions",
        "read_heartbeat_instructions",
    ] {
        assert_eq!(merged.iter().filter(|d| d.name == name).count(), 1);
    }
}

// ================================================================================
// #247 段階 2 / #456 PR3: エージェント自身のハートビート設定ツール（セッション単位）
// ================================================================================

/// 境界値を固定した state（下限 300 / 既定 1800）。live G は既定 false。
fn heartbeat_state() -> AppState {
    let mut state = crate::test_app_state();
    state.heartbeat_limits = crate::config::HeartbeatLimits {
        default_interval_secs: 1800,
        min_interval_secs: 300,
    };
    state
}

/// live G（global heartbeat kill-switch）を固定した state。`discord-` のゲート理由の検証用。
// #654: 使うのは discord_ctx を立てる G ゲート検証（discord feature 依存・#651）だけなので同じ cfg で囲む。
#[cfg(feature = "discord")]
fn heartbeat_state_with_g(g: bool) -> AppState {
    let mut state = heartbeat_state();
    state.heartbeat_config_rx =
        crate::disconnected_heartbeat_config_rx(opencrab_core::heartbeat::HeartbeatConfig {
            interval_secs: 7,
            enabled: g,
        });
    state
}

/// 現在セッションを Nostr（`nostr-{agent}`）にした ctx（信頼済み呼び出し元）。
/// agent_id `agent-x` はハイフンを含むが、resolve は保存済み agent_id で剥がすので割れない。
fn nostr_ctx() -> GatewayCallContext {
    let mut c = GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x");
    c.session_id = Some("nostr-agent-x".to_string());
    c
}

/// 現在セッションを Discord チャンネル（`discord-{agent}-{guild}-{channel}`）にした ctx。
// #654: discord セッションの発火経路（DiscordFire）は discord feature 時のみ登録される（#651）。
// この ctx を使う test は同じ cfg で囲まれているので helper も揃える。
#[cfg(feature = "discord")]
fn discord_ctx() -> GatewayCallContext {
    let mut c = GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x");
    c.session_id = Some("discord-agent-x-100-200".to_string());
    c
}

/// own 定義に 1 件ずつ露出し、**廃止スコープ引数の痕跡がゼロ**であることを固定する
/// （#456 受け入れ基準）。`agent_id` も無い（他人を指す経路を作らない）。
#[test]
fn agent_heartbeat_tools_have_no_scope_args() {
    let defs = SystemGatewayActions::own_definitions();
    for name in ["get_my_heartbeat", "set_my_heartbeat"] {
        assert_eq!(
            defs.iter().filter(|d| d.name == name).count(),
            1,
            "{name} は own 定義にちょうど 1 件必要"
        );
        let def = defs.iter().find(|d| d.name == name).unwrap();
        let props = def
            .parameters
            .get("properties")
            .and_then(|p| p.as_object())
            .cloned()
            .unwrap_or_default();
        for forbidden in ["scope", "channel_id", "guild_id", "agent_id"] {
            assert!(
                !props.contains_key(forbidden),
                "{name} に廃止引数 {forbidden} を生やしてはならない（#456）"
            );
        }
        // schema 文字列全体でも痕跡ゼロ（enum 値・説明文含め）。
        let schema = def.parameters.to_string();
        for forbidden in ["scope", "channel_id", "guild_id", "agent_id"] {
            assert!(
                !schema.contains(forbidden),
                "{name} の parameters に {forbidden} の痕跡が残っている"
            );
        }
    }
    let set = defs.iter().find(|d| d.name == "set_my_heartbeat").unwrap();
    let props = set.parameters["properties"].as_object().unwrap();
    for key in ["enabled", "interval_secs"] {
        assert!(props.contains_key(key), "missing property: {key}");
    }
}

/// #394 の教訓（道具は説明が無いと使われない）を説明文で担保する。オーナー発端は
/// エージェントが「next_run_at が無い」と実在しない名前で呼んだこと。正しい名前
/// （`next_fire_at`）と、その意味・形式（UTC RFC3339）・null になる条件・`gated` の
/// 意味が説明文に書かれていることを固定する（別名は作らない＝二重語彙を増やさない）。
#[test]
fn get_my_heartbeat_description_explains_next_fire_at_and_gating() {
    let defs = SystemGatewayActions::own_definitions();
    let desc = &defs
        .iter()
        .find(|d| d.name == "get_my_heartbeat")
        .unwrap()
        .description;
    for needle in ["next_fire_at", "RFC3339", "UTC", "null", "gated"] {
        assert!(
            desc.contains(needle),
            "get_my_heartbeat の説明文に '{needle}' が必要（#394）: {desc}"
        );
    }
    // gated=true でも next_fire_at は非 null（ゲート解除後に発火する時刻）。その 1 フィールド
    // だけ読んでも「この時刻に発火する」と誤読しないよう、意味を説明文で確定する（#394）。
    assert!(
        desc.contains("この時刻が来ても実際には発火しない"),
        "gated 時の next_fire_at の意味が説明文に無い（#394）: {desc}"
    );
    // 別名 next_run_at は作らない（二重語彙を増やさない・#456）。
    assert!(
        !desc.contains("next_run_at"),
        "next_run_at 別名を説明文に持ち込まない（#456）"
    );
}

/// **既定は無効**（#240）。設定したことが無いセッションは無効で返る。応答に廃止フィールド
/// （scope/channel_id）が無く、`next_fire_at` フィールドが存在する（#439-4）。
// #654: nostr セッションの発火経路（NostrFire descriptor）は nostr feature 時のみ登録される
// （#651）。off では fail-closed になり、検証対象の発火計算そのものが存在しないので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn get_my_heartbeat_defaults_to_disabled() {
    let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
    let r = actions
        .execute("get_my_heartbeat", &json!({}), &nostr_ctx())
        .await;
    assert!(r.success, "{:?}", r.error);
    let d = r.data.unwrap();
    assert_eq!(d["session_id"], "nostr-agent-x");
    assert_eq!(d["enabled"], false);
    assert_eq!(d["interval_secs"], 1800, "既定へフォールバック");
    assert_eq!(d["configured_interval_secs"], serde_json::Value::Null);
    assert_eq!(
        d["next_fire_at"],
        serde_json::Value::Null,
        "無効は next_fire_at=null"
    );
    assert_eq!(d["gated"], false);
    assert_eq!(d["gated_reason"], serde_json::Value::Null);
    assert_eq!(d["min_interval_secs"], 300);
    assert_eq!(d["max_interval_secs"], 86400);
    assert_eq!(d["default_interval_secs"], 1800);
    assert!(d.get("scope").is_none(), "応答に scope を残さない");
    assert!(
        d.get("channel_id").is_none(),
        "応答に channel_id を残さない"
    );
}

/// 有効化 + 間隔設定が DB に載り、`next_fire_at` が算出されて未来を指す（#439-4）。
/// nostr は G 非依存なので gated にならない。有効化で anchor=now・last_fired=NULL（§4.4）。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_enables_and_computes_next_fire_at() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let before = chrono::Utc::now();
    let r = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    let d = r.data.unwrap();
    assert_eq!(d["success"], true);
    assert_eq!(d["enabled"], true);
    assert_eq!(d["interval_secs"], 600);
    assert_eq!(d["configured_interval_secs"], 600);
    assert_eq!(d["gated"], false, "nostr は G 非依存で gated にならない");
    assert_eq!(
        d["last_fired_at"],
        serde_json::Value::Null,
        "有効化で last_fired はリセット（§4.4）"
    );
    let anchor = chrono::DateTime::parse_from_rfc3339(d["anchor_at"].as_str().unwrap()).unwrap();
    assert!(
        anchor >= before - chrono::Duration::seconds(2),
        "有効化で anchor=now"
    );
    let nf = chrono::DateTime::parse_from_rfc3339(
        d["next_fire_at"].as_str().expect("next_fire_at 必須"),
    )
    .unwrap();
    assert!(nf > chrono::Utc::now(), "next_fire は未来（now+interval）");
    // DB へ反映（get で読み直し）。
    let g = actions
        .execute("get_my_heartbeat", &json!({}), &nostr_ctx())
        .await;
    assert_eq!(g.data.unwrap()["enabled"], true);
}

/// 下限より短い間隔は**拒否**し（丸めない）、DB に一切書かない。
#[tokio::test]
async fn set_my_heartbeat_rejects_interval_below_floor_without_writing() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 1}),
            &nostr_ctx(),
        )
        .await;
    assert!(!r.success);
    assert!(r.error.unwrap().contains("短すぎ"));
    let conn = state.db.lock().unwrap();
    assert!(
        opencrab_db::queries::get_session_heartbeat_config(&conn, "agent-x", "nostr-agent-x")
            .unwrap()
            .is_none(),
        "拒否時は行を作らない"
    );
}

/// enabled も interval も無ければエラー。
#[tokio::test]
async fn set_my_heartbeat_bad_args() {
    let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
    let r = actions
        .execute("set_my_heartbeat", &json!({}), &nostr_ctx())
        .await;
    assert!(!r.success);
    assert_eq!(
        r.error.unwrap(),
        "enabled か interval_secs のどちらかが必要です"
    );
}

/// 他エージェントを指す引数（agent_id 等）は黙殺せず明示エラー。
#[tokio::test]
async fn set_my_heartbeat_cannot_target_another_agent() {
    let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
    for key in ["agent_id", "target_agent_id", "agent"] {
        let r = actions
            .execute(
                "set_my_heartbeat",
                &json!({key: "victim", "enabled": true}),
                &nostr_ctx(),
            )
            .await;
        assert!(!r.success, "{key} を無視してはいけない");
        assert!(r.error.unwrap().contains(key));
    }
}

/// 廃止したスコープ引数（scope/channel_id/guild_id）は黙殺せず**廃止を明示**して誘導する。
#[tokio::test]
async fn heartbeat_tools_reject_removed_scope_args() {
    let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
    for key in ["scope", "channel_id", "guild_id"] {
        let r = actions
            .execute(
                "set_my_heartbeat",
                &json!({key: "channel", "enabled": true}),
                &nostr_ctx(),
            )
            .await;
        assert!(!r.success, "{key} は廃止・黙殺しない（#456）");
        assert!(r.error.unwrap().contains("廃止"));
    }
    // get も同様。
    let g = actions
        .execute(
            "get_my_heartbeat",
            &json!({"scope": "channel"}),
            &nostr_ctx(),
        )
        .await;
    assert!(!g.success);
    assert!(g.error.unwrap().contains("廃止"));
}

/// 発火経路の無いセッション（session_id なし / web-）は fail-closed（設計 §13.1）。
/// 「設定できたのに永遠に発火しない行」を作らせない。**エラーには理由だけでなく remedy
/// （どこで実行すればよいか）を書く**（#456 の発端は混乱・M-b）。詰まらせて終わらない。
// #654: この test は remedy 文言が Discord と Nostr の両方を含むこと（fire_target_hint が両
// descriptor を畳む）を検証する。両 descriptor は各 feature 時のみ登録される（#651）ので、両方の
// feature が揃うときだけ意味を持つ。off では hint が空になり検証が成立しないので同じ cfg で囲む。
#[cfg(all(feature = "discord", feature = "nostr"))]
#[tokio::test]
async fn heartbeat_tools_fail_closed_without_fireable_session() {
    let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
    // remedy 相当の文言（次に何をすればよいかが 1 読で分かる）が含まれること。
    let has_remedy = |msg: &str| {
        msg.contains("Discord") && msg.contains("Nostr") && msg.contains("実行してください")
    };
    // (a) セッション文脈なし。
    let mut none_ctx = GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x");
    none_ctx.session_id = None;
    let r = actions
        .execute("set_my_heartbeat", &json!({"enabled": true}), &none_ctx)
        .await;
    assert!(!r.success);
    let e = r.error.unwrap();
    assert!(e.contains("セッション文脈"), "理由: {e}");
    assert!(has_remedy(&e), "remedy が無い（詰まらせる）: {e}");
    // (b) 発火経路の無い種別（web-）。
    let mut web = GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x");
    web.session_id = Some("web-agent-x".to_string());
    let r = actions
        .execute("set_my_heartbeat", &json!({"enabled": true}), &web)
        .await;
    assert!(!r.success);
    let e = r.error.unwrap();
    assert!(e.contains("発火経路"), "理由: {e}");
    assert!(has_remedy(&e), "remedy が無い（詰まらせる）: {e}");
    // get も fail-closed かつ remedy 付き。
    let r = actions.execute("get_my_heartbeat", &json!({}), &web).await;
    assert!(!r.success);
    let e = r.error.unwrap();
    assert!(e.contains("発火経路"), "理由: {e}");
    assert!(has_remedy(&e), "remedy が無い（詰まらせる）: {e}");
}

/// `discord-` セッションは G=false のとき「enabled なのに発火しない」理由を本人へ見せる
/// （#394 / #4）。**whitelist は理由に含めない**（現行発火経路にゲートとして無い・§5 N3）。
// #654: discord セッションの発火経路（DiscordFire descriptor）は discord feature 時のみ登録される
// （#651）。off では discord_ctx が fail-closed になり G ゲート理由を検証できないので同じ cfg で囲む。
#[cfg(feature = "discord")]
#[tokio::test]
async fn get_my_heartbeat_shows_discord_gated_when_global_g_is_false() {
    let state = heartbeat_state_with_g(false);
    let actions = SystemGatewayActions::new(state, None, None, None);
    let s = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &discord_ctx(),
        )
        .await;
    assert!(s.success, "{:?}", s.error);
    let d = s.data.unwrap();
    assert_eq!(d["enabled"], true);
    assert_eq!(d["gated"], true, "G=false の discord は gated");
    let reason = d["gated_reason"].as_str().unwrap();
    assert!(reason.contains("グローバル"), "理由に G を示す: {reason}");
    assert!(
        !reason.contains("whitelist"),
        "whitelist を理由にしない（嘘・§5 N3）"
    );
}

/// G=true なら `discord-` セッションは gated でない。
// #654: discord セッションの発火経路は discord feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "discord")]
#[tokio::test]
async fn discord_not_gated_when_global_g_is_true() {
    let state = heartbeat_state_with_g(true);
    let actions = SystemGatewayActions::new(state, None, None, None);
    let s = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &discord_ctx(),
        )
        .await;
    let d = s.data.unwrap();
    assert_eq!(d["gated"], false, "G=true の discord は gated でない");
    assert_eq!(d["gated_reason"], serde_json::Value::Null);
}

/// 壊れた間隔（0 以下）で enabled の行は、実効 null・next_fire_at null・gated（理由=間隔）。
/// set 経路は <=0 を拒否するので DB へ直接書いて経路を作る（保険ゲートの可視化）。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn get_my_heartbeat_gates_on_broken_interval() {
    let state = heartbeat_state();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_session_heartbeat_config(
            &conn,
            &opencrab_db::queries::SessionHeartbeatConfigRow {
                agent_id: "agent-x".into(),
                session_id: "nostr-agent-x".into(),
                enabled: true,
                interval_secs: Some(0),
                anchor_at: Some(chrono::Utc::now().to_rfc3339()),
                last_fired_at: None,
            },
        )
        .unwrap();
    }
    let actions = SystemGatewayActions::new(state, None, None, None);
    let r = actions
        .execute("get_my_heartbeat", &json!({}), &nostr_ctx())
        .await;
    let d = r.data.unwrap();
    assert_eq!(d["enabled"], true);
    assert_eq!(
        d["interval_secs"],
        serde_json::Value::Null,
        "壊れた間隔は実効 null"
    );
    assert_eq!(d["next_fire_at"], serde_json::Value::Null);
    assert_eq!(d["gated"], true);
    assert!(d["gated_reason"].as_str().unwrap().contains("間隔"));
}

/// 明示の無効化は anchor/last_fired を触らない（位相保存・再有効化まで保つ）。next_fire_at は null。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_disable_keeps_phase() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let e = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    let anchor1 = e.data.unwrap()["anchor_at"].as_str().unwrap().to_string();
    let d = actions
        .execute("set_my_heartbeat", &json!({"enabled": false}), &nostr_ctx())
        .await;
    let data = d.data.unwrap();
    assert_eq!(data["enabled"], false);
    assert_eq!(
        data["anchor_at"].as_str().unwrap(),
        anchor1,
        "無効化で anchor を触らない（§4.4）"
    );
    assert_eq!(data["next_fire_at"], serde_json::Value::Null);
}

/// #605: 間隔変更は anchor を now へ張り直さない（起点を据え置く）。以前は毎回 now へ
/// リセットしていたため、調整のたびに次回発火が先送りされて発火しなかった。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_interval_change_preserves_anchor() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let e = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 3600}),
            &nostr_ctx(),
        )
        .await;
    let anchor1 = e.data.unwrap()["anchor_at"].as_str().unwrap().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let d = actions
        .execute(
            "set_my_heartbeat",
            &json!({"interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    let data = d.data.unwrap();
    assert_eq!(data["enabled"], true, "enabled は保持");
    assert_eq!(data["interval_secs"], 600);
    assert_eq!(
        data["anchor_at"].as_str().unwrap(),
        anchor1,
        "間隔変更で anchor を据え置く（#605: now へ張り直さない）"
    );
}

/// #605 の本丸: 設定変更で `last_fired_at`（発火した事実）を消さない。消すと next_fire が
/// anchor 基準へ戻り、調整のたびに位相が先送りされて発火しなくなる。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_preserves_last_fired_across_config_change() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let _ = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 3600}),
            &nostr_ctx(),
        )
        .await;
    // 「実際に発火した」事実を刻む（発火経路だけが行う操作を模す）。
    let fired_at = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_session_last_fired(&conn, "agent-x", "nostr-agent-x", &fired_at)
            .unwrap();
    }
    // enabled は変えず interval だけ 600 へ。
    let d = actions
        .execute(
            "set_my_heartbeat",
            &json!({"interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    let data = d.data.unwrap();
    assert_eq!(data["interval_secs"], 600);
    assert_eq!(
        data["last_fired_at"].as_str().unwrap(),
        fired_at,
        "設定変更で last_fired が消えた（#605 の退行）"
    );
    // next_fire = last_fired + interval（now 基準へ張り直さない）。
    let got = chrono::DateTime::parse_from_rfc3339(data["next_fire_at"].as_str().unwrap()).unwrap();
    let exp =
        chrono::DateTime::parse_from_rfc3339(&fired_at).unwrap() + chrono::Duration::seconds(600);
    assert_eq!(
        got, exp,
        "next_fire は last_fired+interval であるべき（now 基準ではない）"
    );
}

/// #605 対称ケース: 間隔の**延長**でも last_fired を保ち、next_fire = last_fired+（延ばした）interval。
/// 短縮ケース（preserves_last_fired_across_config_change）と経路は同一だが、対称性のため延長方向も明示する。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_preserves_last_fired_when_interval_extended() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let _ = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    // 「実際に発火した」事実を刻む（発火経路だけが行う操作を模す）。
    let fired_at = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_session_last_fired(&conn, "agent-x", "nostr-agent-x", &fired_at)
            .unwrap();
    }
    // enabled は変えず interval を 600 → 7200 へ**延長**。
    let d = actions
        .execute(
            "set_my_heartbeat",
            &json!({"interval_secs": 7200}),
            &nostr_ctx(),
        )
        .await;
    let data = d.data.unwrap();
    assert_eq!(data["interval_secs"], 7200);
    assert_eq!(
        data["last_fired_at"].as_str().unwrap(),
        fired_at,
        "間隔延長で last_fired が消えた（#605 の退行）"
    );
    // next_fire = last_fired + interval（延ばした 7200 を使う。now 基準へ張り直さない）。
    let got = chrono::DateTime::parse_from_rfc3339(data["next_fire_at"].as_str().unwrap()).unwrap();
    let exp =
        chrono::DateTime::parse_from_rfc3339(&fired_at).unwrap() + chrono::Duration::seconds(7200);
    assert_eq!(
        got, exp,
        "next_fire は last_fired+（延ばした）interval であるべき（now 基準ではない）"
    );
    // last_fired が -120 秒でも 7200 秒後は十分未来＝延長で発火が先送りされる。
    assert!(
        got > chrono::Utc::now(),
        "延長後の next_fire は未来（+7200）であるべき: {got}"
    );
}

/// #605: 発火済みセッションの**再有効化**でも last_fired を保つ（→ next_fire = last_fired+interval。
/// 過ぎていれば即発火する）。以前は再有効化で last_fired=NULL・anchor=now になり先送りされた。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_reenable_after_fire_preserves_last_fired() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let _ = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    let fired_at = (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_session_last_fired(&conn, "agent-x", "nostr-agent-x", &fired_at)
            .unwrap();
    }
    let _ = actions
        .execute("set_my_heartbeat", &json!({"enabled": false}), &nostr_ctx())
        .await;
    let d = actions
        .execute("set_my_heartbeat", &json!({"enabled": true}), &nostr_ctx())
        .await;
    let data = d.data.unwrap();
    assert_eq!(data["enabled"], true);
    assert_eq!(
        data["last_fired_at"].as_str().unwrap(),
        fired_at,
        "再有効化で last_fired を消さない（#605）"
    );
    let got = chrono::DateTime::parse_from_rfc3339(data["next_fire_at"].as_str().unwrap()).unwrap();
    let exp =
        chrono::DateTime::parse_from_rfc3339(&fired_at).unwrap() + chrono::Duration::seconds(600);
    assert_eq!(
        got, exp,
        "next_fire = last_fired+interval（now+interval へ逃がさない）"
    );
}

/// #605: 初回有効化は従来どおり anchor=now を打ち、next_fire = now+interval（enable 直後の
/// 即発火は避ける）。last_fired はまだ無い。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_first_enable_sets_anchor_to_now() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let before = chrono::Utc::now();
    let d = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    let data = d.data.unwrap();
    assert_eq!(
        data["last_fired_at"],
        serde_json::Value::Null,
        "初回は未発火"
    );
    let anchor = chrono::DateTime::parse_from_rfc3339(data["anchor_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(
        anchor >= before - chrono::Duration::seconds(5)
            && anchor <= chrono::Utc::now() + chrono::Duration::seconds(5),
        "初回有効化は anchor を now 付近に打つ: {anchor}"
    );
    let next = chrono::DateTime::parse_from_rfc3339(data["next_fire_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(
        next > chrono::Utc::now(),
        "初回有効化の next_fire は未来（now+interval・即発火しない）: {next}"
    );
}

/// #605 の目玉を直接 assert: `last_fired + interval < now` なら next_fire は**過去**（＝即発火）。
/// 既存テストは last_fired が -30/-120 秒で next_fire が常に未来だったため、この核心を守っていなかった。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_next_fire_is_in_the_past_when_overdue() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let _ = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    // 前回発火を interval より前（2000 秒前）に置く → last_fired + 600 は過去。
    let fired_at = (chrono::Utc::now() - chrono::Duration::seconds(2000)).to_rfc3339();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_session_last_fired(&conn, "agent-x", "nostr-agent-x", &fired_at)
            .unwrap();
    }
    // 設定変更（再有効化）。last_fired は保持され、next_fire は過去のまま＝即発火扱い。
    let d = actions
        .execute("set_my_heartbeat", &json!({"enabled": true}), &nostr_ctx())
        .await;
    let data = d.data.unwrap();
    assert_eq!(data["last_fired_at"].as_str().unwrap(), fired_at);
    let next = chrono::DateTime::parse_from_rfc3339(data["next_fire_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(
        next < chrono::Utc::now(),
        "next_fire は過去であるべき（即発火）: {next}"
    );
    let exp = (chrono::DateTime::parse_from_rfc3339(&fired_at).unwrap()
        + chrono::Duration::seconds(600))
    .with_timezone(&chrono::Utc);
    assert_eq!(next, exp, "next_fire = last_fired + interval");
}

/// #605 doc の 2 ケース目: **未発火 + 古い anchor + 間隔短縮**でも next_fire は過去＝即発火。
/// anchor を据え置く（now へ張り直さない）ので `anchor+新interval` が過ぎれば直ちに発火する。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_never_fired_old_anchor_shorten_fires_immediately() {
    let state = heartbeat_state();
    // 未発火・古い anchor（10000 秒前）・enabled・長い間隔の行を直接用意する。
    let old_anchor = (chrono::Utc::now() - chrono::Duration::seconds(10000)).to_rfc3339();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_session_heartbeat_config(
            &conn,
            &opencrab_db::queries::SessionHeartbeatConfigRow {
                agent_id: "agent-x".into(),
                session_id: "nostr-agent-x".into(),
                enabled: true,
                interval_secs: Some(3600),
                anchor_at: Some(old_anchor.clone()),
                last_fired_at: None,
            },
        )
        .unwrap();
    }
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    // 間隔を 600 へ短縮（enabled 引数なし）。anchor は据え置き（古いまま）。
    let d = actions
        .execute(
            "set_my_heartbeat",
            &json!({"interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    let data = d.data.unwrap();
    assert_eq!(
        data["last_fired_at"],
        serde_json::Value::Null,
        "未発火のまま"
    );
    assert_eq!(
        data["anchor_at"].as_str().unwrap(),
        old_anchor,
        "古い anchor は据え置き（now へ張り直さない）"
    );
    let next = chrono::DateTime::parse_from_rfc3339(data["next_fire_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(
        next < chrono::Utc::now(),
        "anchor+interval が過去 → 即発火: {next}"
    );
    let exp = (chrono::DateTime::parse_from_rfc3339(&old_anchor).unwrap()
        + chrono::Duration::seconds(600))
    .with_timezone(&chrono::Utc);
    assert_eq!(next, exp, "next_fire = anchor + interval");
}

/// #437: set 後に中央スケジューラを起こす（即時反映）。notify の permit を消費できる。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_wakes_scheduler() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let _ = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    let woke = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        state.scheduler_wake.notified(),
    )
    .await;
    assert!(
        woke.is_ok(),
        "set_my_heartbeat は #437 で scheduler_wake を鳴らす"
    );
}

/// 未信頼の素の Agent からは get/set とも拒否（多層防御）。
#[tokio::test]
async fn agent_heartbeat_tools_reject_untrusted_agent() {
    let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
    let mut agent = GatewayCallContext::new(GatewayCaller::Agent, "agent-x");
    agent.session_id = Some("nostr-agent-x".to_string());
    for name in ["get_my_heartbeat", "set_my_heartbeat"] {
        let r = actions
            .execute(name, &json!({"enabled": true}), &agent)
            .await;
        assert!(!r.success, "{name} は素の Agent を拒否");
        assert!(r.error.unwrap().contains("信頼済み"));
    }
}

/// Owner は許可（自分の設定を自分で触るのが目的）。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_allows_owner() {
    let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
    let mut owner = GatewayCallContext::new(GatewayCaller::Owner, "agent-x");
    owner.session_id = Some("nostr-agent-x".to_string());
    let r = actions
        .execute("set_my_heartbeat", &json!({"enabled": true}), &owner)
        .await;
    assert!(r.success, "{:?}", r.error);
}

// ---- #156 S3: A2UI 送信（send_ui）の gateway 非依存化 ----

/// A2UI 描画面を提供する inner のフェイク（Discord の代役）。
struct A2uiProvidingInner {
    surface: Arc<opencrab_core::a2ui::A2uiSurface>,
    calls: std::sync::Mutex<Vec<String>>,
}

struct NoopRenderer;

#[async_trait]
impl opencrab_core::a2ui::UiRenderer for NoopRenderer {
    async fn render(
        &self,
        _surface_id: &str,
        _components: &[opencrab_core::a2ui::A2uiComponent],
        channel: &opencrab_core::a2ui::RenderTarget,
    ) -> Result<opencrab_core::a2ui::RenderedMessage, opencrab_core::a2ui::RenderError> {
        Ok(opencrab_core::a2ui::RenderedMessage {
            platform: channel.platform.clone(),
            message_id: Some("m1".into()),
            channel_id: channel.channel_id.clone(),
        })
    }
    async fn update_on_response(
        &self,
        _rendered: &opencrab_core::a2ui::RenderedMessage,
        _response: &opencrab_core::a2ui::UserActionResponse,
    ) -> Result<(), opencrab_core::a2ui::RenderError> {
        Ok(())
    }
    async fn update_on_timeout(
        &self,
        _rendered: &opencrab_core::a2ui::RenderedMessage,
    ) -> Result<(), opencrab_core::a2ui::RenderError> {
        Ok(())
    }
}

struct CountingUiSink(std::sync::Mutex<usize>);

impl opencrab_core::a2ui::UiResponseSink for CountingUiSink {
    fn on_ui_response(&self, _ev: opencrab_core::a2ui::UiResponseEvent) {
        *self.0.lock().unwrap() += 1;
    }
}

impl A2uiProvidingInner {
    fn new(owner_id: &str) -> Self {
        Self {
            surface: Arc::new(opencrab_core::a2ui::A2uiSurface {
                renderer: Arc::new(NoopRenderer),
                platform: "fake".to_string(),
                owner_id: owner_id.to_string(),
                pending: Some(opencrab_core::a2ui::PendingUiSurface {
                    registry: Arc::new(dashmap::DashMap::new()),
                    sink: Arc::new(CountingUiSink(std::sync::Mutex::new(0))),
                }),
            }),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl GatewayActions for A2uiProvidingInner {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        // transport 側は `send_ui` を**定義しない**（移設済み）。
        vec![GatewayActionDef {
            name: "fake_transport_tool".to_string(),
            class: opencrab_gateway::ToolClass {
                dispatch: opencrab_gateway::DispatchMode::Inline,
                sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                sharing: opencrab_gateway::ToolSharing::AgentBound,
            },
            description: "x".to_string(),
            parameters: json!({"type": "object"}),
        }]
    }
    async fn execute(
        &self,
        name: &str,
        _args: &Value,
        _ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        self.calls.lock().unwrap().push(name.to_string());
        GatewayActionResult {
            success: true,
            data: Some(json!({ "reached_inner": name })),
            error: None,
        }
    }
    fn a2ui_surface(&self) -> Option<Arc<opencrab_core::a2ui::A2uiSurface>> {
        Some(self.surface.clone())
    }
}

/// `own_definitions()` に `send_ui` が 1 件だけある（transport 非依存で全ターンに露出）。
/// 消すと `send_ui` の分類・sub-engine 遮断の属性検査が空振りする。
#[test]
fn send_ui_is_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    assert_eq!(
        defs.iter().filter(|d| d.name == "send_ui").count(),
        1,
        "send_ui must be defined exactly once in own_definitions"
    );
}

/// **移設の本題**: transport 固有の gateway が Discord でなくても、A2UI 描画面を
/// 提供すれば `send_ui` が露出し、実体（gateway 非依存層）が動く。
#[tokio::test]
async fn send_ui_works_for_any_transport_that_provides_a_surface() {
    let state = crate::test_app_state();
    let inner = Arc::new(A2uiProvidingInner::new("owner-1"));
    let actions = SystemGatewayActions::new(state.clone(), Some(inner.clone()), None, None);

    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    assert!(names.contains(&"send_ui".to_string()), "{names:?}");

    let ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("fake-session-1");
    let r = actions
        .execute(
            "send_ui",
            &json!({
                "channel_id": "42",
                "components": [{"id": "t", "component": "Text", "text": "hi"}],
            }),
            &ctx,
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    let interaction_id = r.data.unwrap()["interaction_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 保留状態は transport の描画面の登録簿に載る（コアの型だけ）。
    let surface = inner.a2ui_surface().unwrap();
    let pending = surface.pending.as_ref().unwrap();
    let entry = pending.registry.get(&interaction_id).expect("registered");
    assert_eq!(entry.target.channel_id, "42");
    assert_eq!(entry.target.platform, "fake");
    // オーナー限定ゲートの識別子が空文字にならない（空だと誰でも操作できてしまう）。
    assert_eq!(entry.owner_id, "owner-1");

    // **inner へ委譲していない**（own が唯一の実装）。
    assert!(
        !inner.calls.lock().unwrap().iter().any(|c| c == "send_ui"),
        "send_ui must not be delegated to inner: {:?}",
        inner.calls.lock().unwrap()
    );
}

/// 描画面を持たない transport（web / Nostr / REST / heartbeat）のターンでは
/// **露出しない**（移設前の露出範囲＝Discord 経路のみ、と一致させる）。
/// 名前で呼ばれても inner へ落とさず明示エラー（fail-closed）。
#[tokio::test]
async fn send_ui_is_hidden_and_refused_without_a_surface() {
    let state = crate::test_app_state();
    // inner なし（web / REST / Nostr / heartbeat、Discord feature 無効ビルド）。
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    assert!(!names.contains(&"send_ui".to_string()), "{names:?}");

    let ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("web-session-1");
    let r = actions
        .execute(
            "send_ui",
            &json!({"channel_id": "1", "components": []}),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert_eq!(
        r.error.unwrap(),
        "send_ui はこのゲートウェイでは利用できません（UI を描画できません）"
    );

    // A2UI を提供しない inner を挟んでも同じ（inner へ委譲しない）。
    let inner = Arc::new(RecordingInner::new(&["some_transport_tool"]));
    let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    assert!(!names.contains(&"send_ui".to_string()), "{names:?}");
    let r = actions
        .execute(
            "send_ui",
            &json!({"channel_id": "1", "components": []}),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert!(
        !inner.calls().iter().any(|c| c == "send_ui"),
        "must not fall through to inner: {:?}",
        inner.calls()
    );
}

/// **sub-engine からの遮断**（移設前は Discord 側テストが固定していた不変条件）。
///
/// `send_ui` の定義は `class.sub_engine == Blocked`（`Allowed` ではない）を名乗るので、
/// 合成 gateway が `send_ui` を露出していても depth >= 1 では一覧に出ず、名前指定でも
/// 権限拒否（`rejected:` マーカー）になる。
#[tokio::test]
async fn send_ui_is_blocked_in_sub_engine() {
    let state = crate::test_app_state();
    let transport = Arc::new(A2uiProvidingInner::new("owner-1"));

    // **本番と同じ入れ子の配線**を組む（`crates/server/src/process.rs`）:
    //   depth0: SystemGatewayActions(inner = transport)             ← 親ターン
    //   spawn_subtask が ctx.root_gateway = depth0 の合成 gateway を子へ渡す
    //   depth1: SystemGatewayActions(inner = depth0 の合成 gateway) ← 子ターン
    //           を SubEngineGatewayActions で包む
    // 1 段構成で組むと、内側の合成 gateway が描画面を転送できているかを検出できない。
    let depth0: Arc<dyn GatewayActions> = Arc::new(SystemGatewayActions::new(
        state.clone(),
        Some(transport),
        None,
        None,
    ));
    // 親ターンでは露出する（前提の確認）。
    assert!(depth0.definitions().iter().any(|d| d.name == "send_ui"));

    let depth1: Arc<dyn GatewayActions> = Arc::new(SystemGatewayActions::new(
        state,
        Some(depth0.clone()),
        None,
        None,
    ));
    // 描画面が入れ子の内側まで届いている（届かないと下の拒否分類が
    // 「Unknown gateway action」へ変わる）。
    assert!(
        depth1.definitions().iter().any(|d| d.name == "send_ui"),
        "A2UI 描画面が入れ子の合成 gateway へ転送されていない"
    );

    let sub = opencrab_actions::SubEngineGatewayActions::new(depth1);
    let names: Vec<String> = sub.definitions().into_iter().map(|d| d.name).collect();
    assert!(
        !names.contains(&"send_ui".to_string()),
        "send_ui must NOT be exposed to the sub-engine: {names:?}"
    );

    let r = sub
        .execute(
            "send_ui",
            &json!({"channel_id": "1", "components": []}),
            &sub_ctx("subtask-s1"),
        )
        .await;
    assert!(!r.success, "send_ui must be blocked in the sub-engine");
    // 移設前と同じ分類（実在するが許可外 = 権限拒否）。「そんなツールは無い」に
    // 落ちると幻覚ツール名と同じ扱いになり、拒否の観測が変わる。
    let err = r.error.as_deref().unwrap();
    assert!(
        err.starts_with(opencrab_actions::REJECTION_CODE_PREFIX),
        "send_ui must be a policy rejection, not an unknown tool: {err}"
    );
    assert!(
        !err.contains("Unknown gateway action"),
        "分類が「そんなツールは無い」へ退行している: {err}"
    );

    // 多層防御: 定義自身が `class.sub_engine == Blocked` を名乗る（分類の権威は属性）。
    let send_ui_class = SystemGatewayActions::own_definitions()
        .into_iter()
        .find(|d| d.name == "send_ui")
        .expect("send_ui が own_definitions() に無い")
        .class;
    assert_eq!(
        send_ui_class.sub_engine,
        opencrab_gateway::SubEngineAccess::Blocked,
        "send_ui は sub-engine 拒否属性を名乗るべき"
    );
}

/// `send_ui` は inline（配送系 + ユーザー応答待ち）。分類の権威は定義の `class.dispatch`。
#[test]
fn send_ui_stays_inline_after_the_move() {
    let class = SystemGatewayActions::own_definitions()
        .into_iter()
        .find(|d| d.name == "send_ui")
        .expect("send_ui が own_definitions() に無い")
        .class;
    assert_eq!(class.dispatch, opencrab_gateway::DispatchMode::Inline);
}

// ---- #157 S7: ピアレビュー依頼（request_peer_review）の gateway 非依存化 ----

/// 素テキスト配送口を提供する inner のフェイク（Discord の代役）。
struct DeliveryProvidingInner {
    delivery: Arc<FakeTextDelivery>,
    calls: std::sync::Mutex<Vec<String>>,
    /// true なら `request_peer_review` を**再定義**する（negative assert 用）。
    redefines_peer_review: bool,
}

/// 送信を記録するだけの [`TextDelivery`]。Discord と同じ規約
/// （数値宛先 / `<@id>` / 1900 chars）を模す。
#[derive(Default)]
struct FakeTextDelivery {
    sent: std::sync::Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl opencrab_core::text_delivery::TextDelivery for FakeTextDelivery {
    fn validate_target(&self, target: &str) -> Result<(), String> {
        if target.parse::<u64>().is_ok() {
            Ok(())
        } else {
            Err(format!("無効なchannel_id: {target}"))
        }
    }
    fn mention(&self, user_id: &str) -> String {
        format!("<@{user_id}>")
    }
    fn chunk_limit(&self) -> usize {
        1900
    }
    async fn send_text(&self, target: &str, text: &str) -> Result<(), String> {
        self.sent
            .lock()
            .unwrap()
            .push((target.to_string(), text.to_string()));
        Ok(())
    }
}

impl DeliveryProvidingInner {
    fn new() -> Self {
        Self {
            delivery: Arc::new(FakeTextDelivery::default()),
            calls: std::sync::Mutex::new(Vec::new()),
            redefines_peer_review: false,
        }
    }
    /// transport が誤って移設済みツールを再定義した構成。
    fn redefining() -> Self {
        Self {
            redefines_peer_review: true,
            ..Self::new()
        }
    }
}

#[async_trait]
impl GatewayActions for DeliveryProvidingInner {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        // transport 側は `request_peer_review` を**定義しない**（移設済み）。
        let mut defs = vec![GatewayActionDef {
            name: "fake_transport_tool".to_string(),
            class: opencrab_gateway::ToolClass {
                dispatch: opencrab_gateway::DispatchMode::Inline,
                sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                sharing: opencrab_gateway::ToolSharing::AgentBound,
            },
            description: "x".to_string(),
            parameters: json!({"type": "object"}),
        }];
        if self.redefines_peer_review {
            defs.push(GatewayActionDef {
                name: "request_peer_review".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::Blocked,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "transport の古い実装".to_string(),
                parameters: json!({"type": "object"}),
            });
        }
        defs
    }
    async fn execute(
        &self,
        name: &str,
        _args: &Value,
        _ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        self.calls.lock().unwrap().push(name.to_string());
        GatewayActionResult {
            success: true,
            data: Some(json!({ "reached_inner": name })),
            error: None,
        }
    }
    fn text_delivery(&self) -> Option<Arc<dyn opencrab_core::text_delivery::TextDelivery>> {
        Some(self.delivery.clone())
    }
}

/// `own_definitions()` に `request_peer_review` が 1 件だけある（transport 非依存で
/// 全ターンに露出）。消すと分類・sub-engine 遮断の属性検査が空振りする。
#[test]
fn request_peer_review_is_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    assert_eq!(
        defs.iter()
            .filter(|d| d.name == "request_peer_review")
            .count(),
        1,
        "request_peer_review must be defined exactly once in own_definitions"
    );
}

/// **移設の本題（#157）**: Discord 無効の構成（`inner = None` / web・REST・heartbeat・
/// Nostr のターン）でも `request_peer_review` が**定義に現れる**。
///
/// `send_ui`（描画面が無いと露出しない）とはここが違う: 配送口が無いのは
/// 「送れない」だけで、ツールの存在自体を transport の有無に依存させない。
#[test]
fn request_peer_review_is_defined_even_without_discord() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    assert!(
        names.contains(&"request_peer_review".to_string()),
        "Discord 無効の構成でも定義に出ること: {names:?}"
    );
}

/// **移設の本題**: transport が Discord でなくても、素テキストの配送口を提供すれば
/// 依頼が実際に投稿される（ヘッダ + part X/N）。
#[tokio::test]
async fn request_peer_review_works_for_any_transport_that_provides_delivery() {
    let state = crate::test_app_state();
    let inner = Arc::new(DeliveryProvidingInner::new());
    let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);

    let ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("fake-session-1");
    let r = actions
        .execute(
            "request_peer_review",
            &json!({"content": "raw diff", "channel_id": "555"}),
            &ctx,
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    let data = r.data.unwrap();
    assert_eq!(data["channel_id"], "555");
    assert_eq!(data["parts"], 1);
    assert_eq!(
        data["message"],
        "ピアレビュー依頼を投稿しました。[Peer Review] で始まる返信を待ってください。"
    );

    // ヘッダ + part 1/1 の 2 通が配送口へ出た。
    let sent = inner.delivery.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].0, "555");
    assert!(sent[0].1.starts_with("[Peer Review Request] from agent-x"));
    assert_eq!(sent[1].1, "part 1/1\nraw diff");

    // **inner へ委譲していない**（own が唯一の実装）。
    assert!(
        !inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "request_peer_review"),
        "request_peer_review must not be delegated to inner: {:?}",
        inner.calls.lock().unwrap()
    );
}

/// 宛先の妥当性判定と文言は transport（配送口）の責務。移設前の
/// `無効なchannel_id: …` がそのまま返る。
#[tokio::test]
async fn invalid_target_error_comes_from_the_transport() {
    let state = crate::test_app_state();
    let inner = Arc::new(DeliveryProvidingInner::new());
    let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);
    let ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("fake-session-1");
    let r = actions
        .execute(
            "request_peer_review",
            &json!({"content": "diff", "channel_id": "not-a-number"}),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert_eq!(r.error.unwrap(), "無効なchannel_id: not-a-number");
    // 1 通も出していない（fail-closed）。
    assert!(inner.delivery.sent.lock().unwrap().is_empty());
}

/// 配送口を持たない transport では**定義には出るが実行は明示エラー**（fail-closed）。
/// 黙って inner へ落とさない。
#[tokio::test]
async fn request_peer_review_is_refused_without_a_delivery() {
    let state = crate::test_app_state();
    let ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("web-session-1");

    // inner なし。
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute(
            "request_peer_review",
            &json!({"content": "diff", "channel_id": "1"}),
            &ctx,
        )
        .await;
    assert!(!r.success);
    // 既存の 8 種のエラー文言は変えていない。ここは移設で新設した文言で、
    // 共有プロンプトが全ターンでレビュー依頼を促すため**次の行動**まで書く。
    assert_eq!(
            r.error.unwrap(),
            "request_peer_review はこのゲートウェイでは利用できません（メッセージを送信できません）。\
             このターンの transport はテキストを送れないため、ピアレビュー依頼は省略して先へ進んでよい。"
        );

    // 配送口を提供しない inner を挟んでも同じ（inner へ委譲しない）。
    let inner = Arc::new(RecordingInner::new(&["some_transport_tool"]));
    let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    assert!(
        names.contains(&"request_peer_review".to_string()),
        "{names:?}"
    );
    let r = actions
        .execute(
            "request_peer_review",
            &json!({"content": "diff", "channel_id": "1"}),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert!(
        !inner.calls().iter().any(|c| c == "request_peer_review"),
        "must not fall through to inner: {:?}",
        inner.calls()
    );
}

/// **sub-engine からの遮断**（移設前は Discord 側テストが固定していた不変条件）。
///
/// `request_peer_review` の定義は `class.sub_engine == Blocked`（`Allowed` ではない）を
/// 名乗るので、合成 gateway が露出していても depth >= 1 では一覧に出ず、名前指定でも
/// 権限拒否（`rejected:` マーカー）になる。
#[tokio::test]
async fn request_peer_review_is_blocked_in_sub_engine() {
    let state = crate::test_app_state();
    let transport = Arc::new(DeliveryProvidingInner::new());

    // 本番と同じ入れ子の配線（`crates/server/src/process.rs`）。
    let depth0: Arc<dyn GatewayActions> = Arc::new(SystemGatewayActions::new(
        state.clone(),
        Some(transport),
        None,
        None,
    ));
    assert!(depth0
        .definitions()
        .iter()
        .any(|d| d.name == "request_peer_review"));
    // 配送口が入れ子の内側まで転送されている（能力を黙って落とさない）。
    assert!(depth0.text_delivery().is_some());

    let depth1: Arc<dyn GatewayActions> = Arc::new(SystemGatewayActions::new(
        state,
        Some(depth0.clone()),
        None,
        None,
    ));
    assert!(depth1.text_delivery().is_some());

    let sub = opencrab_actions::SubEngineGatewayActions::new(depth1);
    let names: Vec<String> = sub.definitions().into_iter().map(|d| d.name).collect();
    assert!(
        !names.contains(&"request_peer_review".to_string()),
        "request_peer_review must NOT be exposed to the sub-engine: {names:?}"
    );

    let r = sub
        .execute(
            "request_peer_review",
            &json!({"content": "diff", "channel_id": "1"}),
            &sub_ctx("subtask-s1"),
        )
        .await;
    assert!(!r.success);
    // 移設前と同じ分類（実在するが許可外 = 権限拒否）。
    let err = r.error.as_deref().unwrap();
    assert!(
        err.starts_with(opencrab_actions::REJECTION_CODE_PREFIX),
        "request_peer_review must be a policy rejection: {err}"
    );
    assert!(
        !err.contains("Unknown gateway action"),
        "分類が「そんなツールは無い」へ退行している: {err}"
    );

    // 多層防御: 定義自身が `class.sub_engine == Blocked` を名乗る（分類の権威は属性）。
    let class = SystemGatewayActions::own_definitions()
        .into_iter()
        .find(|d| d.name == "request_peer_review")
        .expect("request_peer_review が own_definitions() に無い")
        .class;
    assert_eq!(
        class.sub_engine,
        opencrab_gateway::SubEngineAccess::Blocked,
        "request_peer_review は sub-engine 拒否属性を名乗るべき"
    );
}

/// `request_peer_review` は inline（配送系）。分類の権威は定義の `class.dispatch`。
#[test]
fn request_peer_review_stays_inline_after_the_move() {
    let class = SystemGatewayActions::own_definitions()
        .into_iter()
        .find(|d| d.name == "request_peer_review")
        .expect("request_peer_review が own_definitions() に無い")
        .class;
    assert_eq!(class.dispatch, opencrab_gateway::DispatchMode::Inline);
}

/// **negative assert（#157 S7）**: transport（Discord）が `request_peer_review` を
/// 再定義しても own が処理する（委譲パターンにしない）。
///
/// 委譲のままにすると、dedup（own 優先）で定義は own に食われるのに実行は transport の
/// 古い実装へ流れ、レビュアー解決や台帳記録が黙ってバイパスされる。
#[tokio::test]
async fn own_handles_request_peer_review_even_if_the_transport_redefines_it() {
    let state = crate::test_app_state();
    let inner = Arc::new(DeliveryProvidingInner::redefining());
    let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);

    // 定義は 1 件だけ（own 優先の dedup）。
    let defs = actions.definitions();
    assert_eq!(
        defs.iter()
            .filter(|d| d.name == "request_peer_review")
            .count(),
        1
    );

    let ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("fake-session-1");
    let r = actions
        .execute(
            "request_peer_review",
            &json!({"content": "diff", "channel_id": "7"}),
            &ctx,
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    // own の実装が動いた証拠: 配送口へヘッダ + part が出て、inner の execute は
    // 呼ばれていない。
    assert_eq!(inner.delivery.sent.lock().unwrap().len(), 2);
    assert!(
        !inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "request_peer_review"),
        "own must not delegate: {:?}",
        inner.calls.lock().unwrap()
    );
}

// ------------------------------------------------------------------
// #268: nostr_run 薄い passthrough（server-own / caller 制限なし・#303）
// ------------------------------------------------------------------

/// `nostr_run` の委譲先を検証する fake passthrough capability。
/// 呼ばれた (agent_id, subcommand, args) を記録し、固定文字列 or エラーを返す。
// #654: nostr_run は nostr feature 依存（#651）。off ではツールが無く、この検証群と
// その helper は意味を持たないので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[derive(Default)]
struct RecordingPassthrough {
    calls: std::sync::Mutex<Vec<(String, String, Vec<String>)>>,
    fail: bool,
}

#[cfg(feature = "nostr")]
#[async_trait]
impl opencrab_actions::GatewayNostrPassthrough for RecordingPassthrough {
    async fn run(
        &self,
        agent_id: &str,
        subcommand: &str,
        args: &[String],
    ) -> anyhow::Result<String> {
        self.calls.lock().unwrap().push((
            agent_id.to_string(),
            subcommand.to_string(),
            args.to_vec(),
        ));
        if self.fail {
            anyhow::bail!("passthrough boom");
        }
        Ok(format!("ran {subcommand}"))
    }
}

/// NOSTR 種別で `nostr_passthrough` capability だけを提供する fake gateway。
#[cfg(feature = "nostr")]
struct FakeNostrGateway {
    passthrough: Arc<RecordingPassthrough>,
}

#[cfg(feature = "nostr")]
#[async_trait]
impl opencrab_actions::AgentGatewayLifecycle for FakeNostrGateway {
    fn kind(&self) -> &'static str {
        opencrab_actions::gateway_kinds::NOSTR
    }
    async fn start(&self, _agent_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&self, _agent_id: &str) {}
    fn is_running(&self, _agent_id: &str) -> bool {
        false
    }
    async fn restore_all(&self) {}
    async fn shutdown_all(&self) {}
    fn nostr_passthrough(&self) -> Option<Arc<dyn opencrab_actions::GatewayNostrPassthrough>> {
        Some(self.passthrough.clone())
    }
}

#[cfg(feature = "nostr")]
fn register_fake_nostr(state: &AppState, fail: bool) -> Arc<RecordingPassthrough> {
    let passthrough = Arc::new(RecordingPassthrough {
        fail,
        ..Default::default()
    });
    state.gateways.register(Arc::new(FakeNostrGateway {
        passthrough: passthrough.clone(),
    }));
    passthrough
}

/// `nostr_run`（薄い nostaro passthrough / #268）は**そもそも使えないように**撤去した
/// （オーナー裁定）。own 定義に無く、名前指定で呼んでも fail-close で拒否される。
///
/// 返信は core の say 一本（gateway が対象ノートへの nostaro reply として投稿する / #840）、
/// 独立投稿は nostr_post。ここは feature の有無に依らず「露出されない」ことを固定する。
#[tokio::test]
async fn nostr_run_is_unexposed_and_fail_closes() {
    let names: Vec<String> = SystemGatewayActions::own_definitions()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert!(
        !names.contains(&"nostr_run".to_string()),
        "nostr_run は露出撤去したので own 定義に無いこと"
    );

    // 名前指定で呼んでも fail-close（黙って成功に見せない）。gateway 未登録でも拒否理由は
    // 「未構成」ではなく「撤去」であること（fail-close が passthrough を引く前に効く）。
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let ctx = GatewayCallContext::new(GatewayCaller::Owner, "agent-x");
    let r = actions
        .execute("nostr_run", &json!({"subcommand": "post"}), &ctx)
        .await;
    assert!(!r.success, "nostr_run は成功してはいけない");
    let msg = r.error.unwrap();
    assert!(msg.contains("撤去"), "拒否理由に「撤去」を含む: {msg}");
}

/// fail-close は passthrough capability が登録されていても**委譲しない**（#268 の委譲配線が
/// 生き残って露出撤去が骨抜きにならないことの回帰）。
// #654: fake passthrough helper（register_fake_nostr 等）は nostr feature 依存なので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn nostr_run_does_not_delegate_even_with_passthrough() {
    let state = crate::test_app_state();
    let rec = register_fake_nostr(&state, false);
    let actions = SystemGatewayActions::new(state, None, None, None);
    // caller=Agent（Nostr 受信ターン相当）でも通らない。
    let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-268");
    let r = actions
        .execute(
            "nostr_run",
            &json!({"subcommand": "timeline", "args": ["--limit", "5"]}),
            &ctx,
        )
        .await;
    assert!(!r.success, "露出撤去した nostr_run は passthrough があっても拒否される");
    assert!(r.error.unwrap().contains("撤去"));
    // capability は一度も呼ばれていない（委譲していない）。
    assert!(
        rec.calls.lock().unwrap().is_empty(),
        "fail-close なのに passthrough capability が呼ばれている（委譲配線が生き残っている）"
    );
}
