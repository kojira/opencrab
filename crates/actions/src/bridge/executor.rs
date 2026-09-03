use std::sync::Arc;

use async_trait::async_trait;
use opencrab_core::{ActionExecutor, ActionResult as CoreActionResult, FunctionDefinition};
use opencrab_gateway::GatewayActions;

use crate::dispatcher::ActionDispatcher;
use crate::traits::{ActionContext, ActionResult as ActionsActionResult};

use super::{
    is_rejection, tool_policy, EffectiveToolDefinition, ExecutorRuntimeState, ToolEvent,
    ToolEventSink, ToolEventStatus, ToolSlot, CORE_DISPATCHABLE_ACTIONS, CORE_INLINE_ACTIONS,
    MAX_DEPTH, MCP_TOOL_PREFIX, REJECTION_CODE_PREFIX,
};

pub struct BridgedExecutor {
    dispatcher: ActionDispatcher,
    context: ActionContext,
    gateway_actions: Option<Arc<dyn GatewayActions>>,
    /// MCP ツール源（`GatewayActions` 実装）。gateway_actions とは別スロット
    /// （MCP は全ターンで利用可、gateway は transport 毎で単数のため）。
    mcp_actions: Option<Arc<dyn GatewayActions>>,
    depth: u32,
    tool_event_sink: Option<Arc<dyn ToolEventSink>>,
    /// この run を起こした inbound メッセージの返信先（gateway 不透明 token / #158 S1）。
    /// `gateway_call_context` が `GatewayCallContext.reply_target` に載せ、宛先引数を
    /// 省略したツール呼び出しのフォールバックにする。既定 `None`。
    reply_target: Option<String>,
    /// この run で使えるツール名の許可リスト（#368）。`Some` のとき、可視性
    /// （`list_tools`）と実行（`dispatch_inner`）の**両方**を、ここに載る名前だけに絞る。
    /// caller/depth ゲート（`tool_policy` / `policy_allows`）は弱めず、その**上に重ねる**
    /// 追加の deny-by-default。
    ///
    /// **全スロット（dispatcher / gateway own / MCP）に効く**のが要点。3 スロットとも
    /// 可視は `list_tools`、実行は `dispatch_inner` を通るので、この 1 箇所で覆える
    /// （スロット個別のフィルタを別々に足す必要がない）。
    ///
    /// 既定 `None`（無制限 = 従来挙動）。対話ターン・heartbeat・subtask は `None` の
    /// ままで一切変わらない。sleep 整理ラン（`memory_organize`）だけが `Some` を渡す。
    tool_allowlist: Option<std::collections::HashSet<String>>,
    /// 名前 → `ToolClass` の索引（分類の権威）。
    ///
    /// gateway / MCP の `definitions()` を舐めて `(name, class)` を入れ、core のツール
    /// （`GatewayActionDef` を持たない）は [`CORE_INLINE_ACTIONS`] / [`CORE_DISPATCHABLE_ACTIONS`]
    /// から `dispatch` を合成する（`sub_engine = NotExposed`、`sharing = AgentBound`。core は
    /// 許可リストにも拒否リストにも属さないため一律 `NotExposed` で現行と等価）。gateway /
    /// MCP を差し替えたら [`Self::rebuild_tool_class_index`] で作り直す。sub-engine 遮断
    /// （`sub_engine == Blocked`）と非同期化除外（`dispatch == Inline`）をここから引く。
    /// 索引に無い名前は「遮断しない」（属性を名乗る定義が無いツールは既定で通す）。
    tool_class_index: std::collections::HashMap<String, opencrab_gateway::ToolClass>,
    /// §2.7 ツール階層（turn ローカル）: describe_tools でこのターンに活性化したツール名。
    /// depth 0 の list_tools で常時集合に union して投影する。新しい登録簿は作らず、既存の
    /// effective_tool_definitions（policy 済み）に retain で効かせる。
    activated_tools: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl BridgedExecutor {
    pub fn new(dispatcher: ActionDispatcher, context: ActionContext) -> Self {
        let mut this = Self {
            dispatcher,
            context,
            gateway_actions: None,
            mcp_actions: None,
            depth: 0,
            tool_event_sink: None,
            reply_target: None,
            tool_allowlist: None,
            tool_class_index: std::collections::HashMap::new(),
            activated_tools: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        };
        this.rebuild_tool_class_index();
        this
    }

    /// 名前 → `ToolClass` 索引を作り直す（core 合成 + gateway + MCP）。
    ///
    /// gateway / MCP を差し替えたら必ず呼ぶ（`with_gateway_actions` / `with_mcp_actions`）。
    /// 後入れ優先で挿入する: gateway / MCP の実定義が core 合成より優先されるが、名前空間は
    /// 重ならない（core と gateway own と MCP プレフィックスは互いに素）ので実際の衝突は無い。
    fn rebuild_tool_class_index(&mut self) {
        use opencrab_gateway::{DispatchMode, SubEngineAccess, ToolClass, ToolSharing};
        let mut index: std::collections::HashMap<String, ToolClass> =
            std::collections::HashMap::new();
        // core のツールは `GatewayActionDef` を持たないので合成する。core は許可リストにも
        // 拒否リストにも 1 つも属さないため `sub_engine = NotExposed` で現行と等価。
        let synth = |dispatch: DispatchMode| ToolClass {
            dispatch,
            sub_engine: SubEngineAccess::NotExposed,
            sharing: ToolSharing::AgentBound,
        };
        for name in CORE_INLINE_ACTIONS {
            index.insert((*name).to_string(), synth(DispatchMode::Inline));
        }
        for name in CORE_DISPATCHABLE_ACTIONS {
            index.insert((*name).to_string(), synth(DispatchMode::Dispatchable));
        }
        if let Some(ref gw) = self.gateway_actions {
            for def in gw.definitions() {
                index.insert(def.name, def.class);
            }
        }
        if let Some(ref mcp) = self.mcp_actions {
            for def in mcp.definitions() {
                index.insert(def.name, def.class);
            }
        }
        self.tool_class_index = index;
    }

    /// depth>=1 の sub-engine から遮断すべきか（`class.sub_engine == Blocked`）。
    /// 索引に無い名前は `false`（属性を名乗る定義が無いツールは遮断しない）。
    ///
    /// **多層防御の層が移ったことの記録（消さないこと）**:
    /// - **本番では事実上不活性**。depth>=1 では `gateway_actions` が常に
    ///   [`SubEngineGatewayActions`]（`Allowed` だけに事前フィルタする外周）なので、索引に
    ///   `Blocked` が入らず、この二層目は必ず `false` を返す。実効ゲートは外周フィルタが担う。
    ///   挙動は旧実装と完全に等価（旧 `DISCORD_ACTIONS` の名前ベース深さ拒否も、外周の許可
    ///   リストの上に乗る冗長な層だった）。
    /// - **将来 depth>=1 で生の gateway を直付けする経路を足すと、この層が復活する**
    ///   （外周フィルタを通らないツールに対して `Blocked` 属性が実効ゲートになる）。だから
    ///   「使われていないから消す」判断はしないこと。多層防御の意図は残す。
    fn is_blocked_in_subengine(&self, name: &str) -> bool {
        self.tool_class_index
            .get(name)
            .map(|c| c.sub_engine == opencrab_gateway::SubEngineAccess::Blocked)
            .unwrap_or(false)
    }

    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }

    pub fn with_gateway_actions(mut self, actions: Arc<dyn GatewayActions>) -> Self {
        self.gateway_actions = Some(actions);
        self.rebuild_tool_class_index();
        self
    }

    /// MCP ツール源を注入する（`mcp__<server>__<tool>` を提供する `GatewayActions`）。
    pub fn with_mcp_actions(mut self, actions: Arc<dyn GatewayActions>) -> Self {
        self.mcp_actions = Some(actions);
        self.rebuild_tool_class_index();
        self
    }

    pub fn with_tool_event_sink(mut self, sink: Arc<dyn ToolEventSink>) -> Self {
        self.tool_event_sink = Some(sink);
        self
    }

    /// inbound メッセージの返信先（gateway 不透明 token）を注入する（#158 S1）。
    ///
    /// `RunRequest.reply_target` と同じ `Option<String>` をそのまま受け、ツール実行時の
    /// `GatewayCallContext.reply_target` として gateway 実装へ運ぶ。未注入なら `None`
    /// のままで、宛先を明示するツール呼び出しの挙動は変わらない。
    pub fn with_reply_target(mut self, reply_target: Option<String>) -> Self {
        self.reply_target = reply_target;
        self
    }

    /// この run のツール許可リストを注入する（#368）。`Some` のとき、可視性と実行の
    /// 両方を、渡した名前だけに絞る（caller/depth ゲートの**上乗せ**）。`None`（既定）は
    /// 無制限で従来どおり。sleep 整理ランだけが渡す。
    pub fn with_tool_allowlist(mut self, allowlist: Option<Vec<String>>) -> Self {
        self.tool_allowlist = allowlist.map(|v| v.into_iter().collect());
        self
    }

    /// LLM に見せる実効定義を、production の slot/class 索引と一緒に列挙する。
    ///
    /// `list_tools` はこの結果から定義だけを取り出すため、採取用の分類再構築と
    /// production 可視性が別々に進む余地はない。
    pub fn effective_tool_definitions(&self) -> Vec<EffectiveToolDefinition> {
        let opt_desc = |description: String| {
            if description.is_empty() {
                None
            } else {
                Some(description)
            }
        };
        let available_providers = self
            .context
            .runtime_info
            .lock()
            .map(|info| info.available_providers.clone())
            .unwrap_or_default();
        let mut tools: Vec<EffectiveToolDefinition> = self
            .dispatcher
            .get_definitions(&[])
            .into_iter()
            .filter(|definition| {
                self.policy_allows(&definition.name) && self.run_allows(&definition.name)
            })
            .map(|definition| {
                let class = self.tool_class_index.get(&definition.name).copied();
                let (description, parameters) = if definition.name == "select_llm" {
                    let (desc, params) =
                        crate::llm_selection::select_llm_schema(&available_providers);
                    (opt_desc(desc), params)
                } else {
                    (opt_desc(definition.description), definition.parameters)
                };
                EffectiveToolDefinition {
                    definition: FunctionDefinition {
                        name: definition.name,
                        description,
                        parameters,
                    },
                    class,
                    slot: ToolSlot::Dispatcher,
                }
            })
            .collect();

        if let Some(ref gateway) = self.gateway_actions {
            for definition in gateway.definitions() {
                if !self.policy_allows(&definition.name) || !self.run_allows(&definition.name) {
                    continue;
                }
                let class = self.tool_class_index.get(&definition.name).copied();
                tools.push(EffectiveToolDefinition {
                    definition: FunctionDefinition {
                        name: definition.name,
                        description: opt_desc(definition.description),
                        parameters: definition.parameters,
                    },
                    class,
                    slot: ToolSlot::Gateway,
                });
            }
        }

        if let Some(ref mcp) = self.mcp_actions {
            for definition in mcp.definitions() {
                if !self.policy_allows(&definition.name) || !self.run_allows(&definition.name) {
                    continue;
                }
                let class = self.tool_class_index.get(&definition.name).copied();
                tools.push(EffectiveToolDefinition {
                    definition: FunctionDefinition {
                        name: definition.name,
                        description: opt_desc(definition.description),
                        parameters: definition.parameters,
                    },
                    class,
                    slot: ToolSlot::Mcp,
                });
            }
        }
        tools
    }

    /// 同じ executor を使う engine が次の LLM call で読む turn-local 状態。
    pub fn runtime_state(&self) -> ExecutorRuntimeState {
        ExecutorRuntimeState {
            model_override: self
                .context
                .model_override
                .lock()
                .ok()
                .and_then(|value| value.clone()),
            current_purpose: self
                .context
                .current_purpose
                .lock()
                .map(|value| value.clone())
                .unwrap_or_default(),
        }
    }

    /// この run のツール許可リストが `name` を許すか（#368）。`None`（未設定）なら常に
    /// 許可（無制限）。`Some` のときは集合に載る名前だけを許す。`policy_allows`（caller/depth
    /// ゲート）とは独立の**追加**述語で、`list_tools`（可視）と `dispatch_inner`（実行）の
    /// 両方が同じこの述語を通すことで「見えるが呼べない / 見えないが呼べる」の食い違いを防ぐ。
    fn run_allows(&self, name: &str) -> bool {
        match &self.tool_allowlist {
            None => true,
            Some(set) => set.contains(name),
        }
    }

    /// dispatcher の CallerIdentity を gateway 境界の型付き caller に写像する。
    /// CoAgent の agent_id は保存する（旧 `__caller` 文字列注入では落ちていた）。
    fn gateway_call_context(
        &self,
        tool_call_id: Option<&str>,
    ) -> opencrab_gateway::GatewayCallContext {
        let caller = match &self.context.caller {
            crate::traits::CallerIdentity::Owner => opencrab_gateway::GatewayCaller::Owner,
            crate::traits::CallerIdentity::Agent => opencrab_gateway::GatewayCaller::Agent,
            crate::traits::CallerIdentity::CoAgent { agent_id } => {
                opencrab_gateway::GatewayCaller::CoAgent {
                    agent_id: agent_id.clone(),
                }
            }
            crate::traits::CallerIdentity::TrustedUser => {
                opencrab_gateway::GatewayCaller::TrustedUser
            }
        };
        opencrab_gateway::GatewayCallContext {
            caller,
            session_id: self.context.session_id.clone(),
            depth: self.depth,
            agent_id: self.context.agent_id.clone(),
            // 合成 gateway 自身のハンドルを子へ渡す（RFC #152 S2）。sub-engine を
            // 構築する `spawn_subtask` が「自分を包む合成 gateway」を辿れるように
            // する注入口。Arc は本 executor が所有し、ここでは clone して短命な
            // ctx に載せるだけ（自己参照 Arc ではない＝サイクルなし）。
            root_gateway: self.gateway_actions.clone(),
            // inbound の返信先を gateway 実装まで運ぶ（#158 S1）。宛先を引数で受ける
            // アクションが、引数省略時のフォールバックとして読む。
            reply_target: self.reply_target.clone(),
            // #915: engine の tool_call.id を発話 op invoke の call_id へ伝播する。
            tool_call_id: tool_call_id.filter(|s| !s.is_empty()).map(str::to_string),
        }
    }

    fn caller_is_owner(&self) -> bool {
        // #485: co_agent は owner 等価（オーナー指示 2026-08-10。#330 を覆す）。owner 判定の
        // 唯一の源は `CallerIdentity::is_owner_equivalent`。OWNER_ONLY_ACTIONS（execute_shell /
        // ws_* / configure_* / (add|remove)_allowed_command 等）の可視性・実行の双方がここを通る。
        self.context.caller.is_owner_equivalent()
    }

    fn caller_is_trusted(&self) -> bool {
        matches!(
            self.context.caller,
            crate::traits::CallerIdentity::Owner
                | crate::traits::CallerIdentity::CoAgent { .. }
                | crate::traits::CallerIdentity::TrustedUser
        )
    }

    /// このコンテキスト（caller/depth）で name が可視・実行可能か（#45）。
    /// list_tools と dispatch_inner が同一のポリシー判定を共有するための述語。
    fn policy_allows(&self, name: &str) -> bool {
        let policy = tool_policy(name);
        if policy.owner_only && !self.caller_is_owner() {
            return false;
        }
        if policy.trusted_only && !self.caller_is_trusted() {
            return false;
        }
        if self.depth >= 1 && self.is_blocked_in_subengine(name) {
            return false;
        }
        if self.depth >= MAX_DEPTH && policy.depth_capped {
            return false;
        }
        true
    }

    /// §2.7 describe_tools 実体: 指定名の schema（policy 済み effective 定義から）を返し、
    /// このターンの活性化集合へ足す。存在しない/不可視の名前は不明として返す。
    fn describe_tools_impl(&self, args: &serde_json::Value) -> CoreActionResult {
        let names: Vec<String> = args
            .get("names")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if names.is_empty() {
            return CoreActionResult {
                success: false,
                data: serde_json::Value::Null,
                error: Some("describe_tools: names[] is required".to_string()),
            };
        }
        let effective = self.effective_tool_definitions();
        let mut loaded = Vec::new();
        let mut unknown = Vec::new();
        for name in &names {
            if let Some(t) = effective.iter().find(|t| &t.definition.name == name) {
                loaded.push(serde_json::json!({
                    "name": t.definition.name,
                    "description": t.definition.description,
                    "parameters": t.definition.parameters,
                }));
            } else {
                unknown.push(name.clone());
            }
        }
        if let Ok(mut set) = self.activated_tools.lock() {
            for t in &loaded {
                if let Some(n) = t.get("name").and_then(|v| v.as_str()) {
                    set.insert(n.to_string());
                }
            }
        }
        CoreActionResult {
            success: true,
            data: serde_json::json!({
                "loaded": loaded,
                "unknown": unknown,
                "note": "Loaded tools are now callable for the rest of this turn."
            }),
            error: None,
        }
    }

    /// 実際のディスパッチ本体（dispatcher → gateway fallback）。
    /// instrumentation は `ActionExecutor::execute` 側で wrap する。
    async fn dispatch_inner(
        &self,
        name: &str,
        args: &serde_json::Value,
        tool_call_id: Option<&str>,
    ) -> CoreActionResult {
        // §2.7: describe_tools は list_tools の上の合成 query ツール（新登録簿なし）。
        // このターンの活性化集合へ名前を足し、以後の list_tools がそれを投影する。実体ツールの
        // 実行ではないので policy/run ゲートの前で処理する（activate は可視化だけで、実行時には
        // 対象ツール自身の policy が別途効く）。
        if name == "describe_tools" {
            return self.describe_tools_impl(args);
        }
        // 可視性（list_tools）と同じポリシー表を実行時にも強制する（#45）。
        // 一覧から隠しただけでは、モデルが名前を記憶で呼んだ場合に素通しになる。
        let reject = |msg: String| CoreActionResult {
            success: false,
            data: serde_json::Value::Null,
            error: Some(format!("{REJECTION_CODE_PREFIX}{msg}")),
        };
        let policy = tool_policy(name);
        if policy.owner_only && !self.caller_is_owner() {
            return reject(format!("action '{name}' requires owner"));
        }
        if policy.trusted_only && !self.caller_is_trusted() {
            return reject(format!(
                "action '{name}' requires a trusted caller (owner/co_agent/trusted_user)"
            ));
        }
        if self.depth >= 1 && self.is_blocked_in_subengine(name) {
            return reject(format!(
                "action '{name}' is not available in sub-engines (depth {})",
                self.depth
            ));
        }
        if self.depth >= MAX_DEPTH && policy.depth_capped {
            return reject(format!(
                "{name} is not available at depth {} (max nesting: {MAX_DEPTH})",
                self.depth
            ));
        }
        // この run の許可リスト（#368）: caller/depth ゲートを通っても、許可リストの外なら
        // 実行を拒否する。MCP/dispatcher/gateway のどのスロットへ振り分ける**前**に効かせる
        // ことで、全スロットを 1 箇所で覆う（見えないが呼べる、を塞ぐ）。
        if !self.run_allows(name) {
            return reject(format!(
                "action '{name}' is not available in this run (tool allowlist)"
            ));
        }

        // MCP ツール（mcp__ プレフィックス）は MCP プロバイダへ振り分ける。gateway が
        // unknown を返す前に処理する（名前空間は dispatcher/gateway と重ならない）。
        if name.starts_with(MCP_TOOL_PREFIX) {
            if let Some(ref mcp) = self.mcp_actions {
                let ctx = self.gateway_call_context(tool_call_id);
                let r = mcp.execute(name, args, &ctx).await;
                return CoreActionResult {
                    success: r.success,
                    data: r.data.unwrap_or(serde_json::Value::Null),
                    error: r.error,
                };
            }
        }

        // Try dispatcher first. フォールバック判定は登録有無で行う
        // （"Unknown action" エラー文言の文字列比較は、実アクションが同文を
        // 返した場合に gateway へ誤ルートするため廃止 — #36）。
        if self.dispatcher.has_action(name) {
            return self
                .dispatcher
                .execute(name, args, &self.context)
                .await
                .into();
        }

        // Fallback to gateway actions.
        if let Some(ref gw) = self.gateway_actions {
            // 実行コンテキストは型付きで渡す。LLM 由来の args には混ぜない（#36）。
            let ctx = self.gateway_call_context(tool_call_id);
            let gw_result = gw.execute(name, args, &ctx).await;
            return CoreActionResult {
                success: gw_result.success,
                data: gw_result.data.unwrap_or(serde_json::Value::Null),
                error: gw_result.error,
            };
        }

        // dispatcher にも gateway にも無い。
        CoreActionResult {
            success: false,
            data: serde_json::Value::Null,
            error: Some(format!("Unknown action: {name}")),
        }
    }
}

impl BridgedExecutor {
    /// instrumentation 付き実行本体。
    ///
    /// `tool_call_id` は LLM 由来の元 ID を伝播するための相関キー。`Some(id)` なら
    /// その ID を webhook/トレースの相関に使う（skill engine の tool_call.id と一致）。
    /// `None`（id を持たない直接呼び出し）のときのみ合成 UUID を生成し、ペイロード上は
    /// `correlation = "synthetic"` として区別できるようにする。
    async fn execute_instrumented(
        &self,
        name: &str,
        args: &serde_json::Value,
        tool_call_id: Option<&str>,
    ) -> CoreActionResult {
        let Some(sink) = self.tool_event_sink.clone() else {
            let started_at = chrono::Utc::now().to_rfc3339();
            let start = std::time::Instant::now();
            let result = self.dispatch_inner(name, args, tool_call_id).await;
            self.write_tool_log(
                name,
                args,
                &result,
                &started_at,
                start.elapsed().as_millis() as i64,
            );
            return result;
        };
        // 相関 ID: LLM 由来 ID があれば伝播、無ければ合成（同 start/terminal で一致）。
        let synthetic;
        let call_id: &str = match tool_call_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                synthetic = uuid::Uuid::new_v4().to_string();
                &synthetic
            }
        };
        let started_at = chrono::Utc::now().to_rfc3339();
        let session_id = self.context.session_id.as_deref();
        // #620: 旧来の「nsec キー名でマスクする」sink ゲート（SECRET_KEYS）は撤去した。
        // キー名一致は実際の混入（値の中に鍵が含まれる形）を検出できず、そもそも `nsec` を
        // キーに持つ JSON を tool 引数/結果へ出す producer は皆無だった（列挙で確認 / #620）。
        // 鍵は at-rest 暗号化と実行時 env 注入で「読める範囲の外」に置く方式へ移した。
        let sink_args = args;
        sink.on_event(&ToolEvent {
            tool_name: name,
            tool_call_id: call_id,
            agent_id: &self.context.agent_id,
            session_id,
            depth: self.depth,
            status: ToolEventStatus::Started,
            started_at: &started_at,
            duration_ms: None,
            args: sink_args,
            result: None,
            error: None,
        });
        let start = std::time::Instant::now();
        let result = self.dispatch_inner(name, args, Some(call_id)).await;
        let duration_ms = start.elapsed().as_millis() as u64;
        let status = if result.success {
            ToolEventStatus::Completed
        } else if is_rejection(result.error.as_deref()) {
            // permission-policy 拒否を観測可能にする（raw URL/token は載せない）。
            tracing::debug!(
                tool = %name,
                tool_call_id = %call_id,
                depth = self.depth,
                "tool call classified as rejected (policy)"
            );
            ToolEventStatus::Rejected
        } else {
            ToolEventStatus::Failed
        };
        // #620: 結果側の nsec キー名マスク（SECRET_KEYS）も撤去（上と同じ理由）。
        let sink_result = &result.data;
        sink.on_event(&ToolEvent {
            tool_name: name,
            tool_call_id: call_id,
            agent_id: &self.context.agent_id,
            session_id,
            depth: self.depth,
            status,
            started_at: &started_at,
            duration_ms: Some(duration_ms),
            args: sink_args,
            result: Some(sink_result),
            error: result.error.as_deref(),
        });
        self.write_tool_log(name, args, &result, &started_at, duration_ms as i64);
        result
    }

    /// ツール 1 実行を `tool_logs` へ 1 行書く（載せ替え工程 5-b）。
    ///
    /// 成否に関わらず必ず書く。INSERT 失敗は握り潰さない（fail-loud）。
    /// 返り値・`memory_sessions` / sink は変えない。
    fn write_tool_log(
        &self,
        name: &str,
        args: &serde_json::Value,
        result: &CoreActionResult,
        started_at: &str,
        latency_ms: i64,
    ) {
        let outcome = tool_log_outcome(result);
        let result_text = tool_log_result_text(result);
        let conn = self
            .context
            .db
            .lock()
            .unwrap_or_else(|e| panic!("tool_logs: db lock poisoned: {e}"));
        opencrab_db::queries::insert_tool_log(
            &conn,
            &opencrab_db::queries::ToolLogWrite {
                agent_id: self.context.agent_id.clone(),
                session_id: self.context.session_id.clone(),
                tool_name: name.to_string(),
                args_json: args.to_string(),
                outcome: outcome.to_string(),
                result_text,
                started_at: Some(started_at.to_string()),
                latency_ms: Some(latency_ms),
                iteration: None,
            },
        )
        .unwrap_or_else(|e| panic!("tool_logs insert failed: {e:#}"));
    }
}

/// `tool_logs.outcome` への写像。未知の第 4 態を発明しない。
fn tool_log_outcome(result: &CoreActionResult) -> &'static str {
    if result.success {
        "done"
    } else if is_rejection(result.error.as_deref()) {
        "refused"
    } else {
        "failed"
    }
}

/// 結果の要約。成功は data、失敗は error。空には落とさない（無いときだけ空文字）。
fn tool_log_result_text(result: &CoreActionResult) -> String {
    if result.success {
        match &result.data {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    } else {
        result.error.clone().unwrap_or_default()
    }
}

#[async_trait]
impl ActionExecutor for BridgedExecutor {
    async fn execute(&self, name: &str, args: &serde_json::Value) -> CoreActionResult {
        self.execute_instrumented(name, args, None).await
    }

    async fn execute_with_id(
        &self,
        name: &str,
        args: &serde_json::Value,
        tool_call_id: &str,
    ) -> CoreActionResult {
        self.execute_instrumented(name, args, Some(tool_call_id))
            .await
    }

    fn list_tools(&self) -> Vec<FunctionDefinition> {
        let mut tools: Vec<FunctionDefinition> = self
            .effective_tool_definitions()
            .into_iter()
            .map(|tool| tool.definition)
            .collect();
        // §2.7 ツール階層: depth 0（通常会話ターン）で LLM へ投影する関数を「常時集合（≤15/16）」に
        // 絞る。可視性のみを絞り実行ゲート（policy/run_allows）は不変（可視≠実行可否）。
        // このターンに describe_tools で活性化した名前を常時集合に union（policy 済みの effective
        // 定義に retain で効かせるので、owner-only 等の可視条件は保たれる）。
        //
        // 会話 op は**レーン依存**（Discord=reply/reaction/resolve、Nostr=reply/reaction/repost/
        // resolve）。ハードコード列挙をやめ、op 宣言側の分類を使う: 発話クラス
        // （`DispatchMode::Utterance`＝reply/reaction/repost）を常時に含め、照会 op の `resolve` は
        // レーン共通の固定名で含める。これで Nostr レーンでは repost が自動で常時集合に入る。
        if self.depth == 0 {
            // 会話 op 以外の常時集合（レーン非依存）＋会話の照会 op `resolve`。
            const ALWAYS_FIXED: &[&str] = &[
                "resolve",
                "execute_shell",
                "spawn_subtask",
                "cancel_subtask",
                "steer_subtask",
                "retrieve_memory_nodes",
                "search_memory_index",
                "browse_memory_index",
                "open_task",
                "record_task_progress",
                "close_task",
                "read_skill",
            ];
            let activated = self
                .activated_tools
                .lock()
                .map(|s| s.clone())
                .unwrap_or_default();
            let is_utterance = |name: &str| {
                self.tool_class_index
                    .get(name)
                    .is_some_and(|c| c.dispatch == opencrab_gateway::DispatchMode::Utterance)
            };
            tools.retain(|t| {
                ALWAYS_FIXED.contains(&t.name.as_str())
                    || activated.contains(&t.name)
                    || is_utterance(&t.name)
            });
            // describe_tools 自体を常時投影（合成定義・新登録簿なし）。
            tools.push(FunctionDefinition {
                name: "describe_tools".to_string(),
                description: Some(
                    "Load the parameter schemas of tools listed by name under \"More tools\" \
                     so you can call them. Pass names as a JSON array of strings. The loaded \
                     tools stay available for the rest of this turn."
                        .to_string(),
                ),
                parameters: serde_json::json!({
                    "type": "object",
                    "required": ["names"],
                    "properties": {
                        "names": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Tool names to load (from the More tools index)."
                        }
                    }
                }),
            });
        }
        tools
    }

    /// 非同期化しないツール名（inline 実行のまま）。
    ///
    /// 索引から `dispatch == Inline` の名前を集め、[`crate::subtask::default_non_dispatch_tools`]
    /// （制御ツール ＋ core inline）と合わせて返す。gateway / MCP を注入していない executor
    /// でも制御ツールと core は必ず inline に残る（`default_non_dispatch_tools` が保証）。
    fn inline_tool_names(&self) -> std::collections::HashSet<String> {
        use opencrab_gateway::DispatchMode;
        let mut set = crate::subtask::default_non_dispatch_tools();
        for (name, class) in &self.tool_class_index {
            // Inline も Utterance も背景 subtask 化しない（should_dispatch=false）。Utterance は
            // inline 経路に載せつつ engine が `is_utterance` で撃ちっぱなし配送へ分岐する。
            if matches!(
                class.dispatch,
                DispatchMode::Inline | DispatchMode::Utterance
            ) {
                set.insert(name.clone());
            }
        }
        set
    }

    /// 発話クラス（撃ちっぱなし・§3.3.1 C4）のツール名集合。索引から
    /// `dispatch == Utterance` を集める。照会/道具クラスとはここで分離する。
    fn utterance_tool_names(&self) -> std::collections::HashSet<String> {
        use opencrab_gateway::DispatchMode;
        self.tool_class_index
            .iter()
            .filter(|(_, class)| class.dispatch == DispatchMode::Utterance)
            .map(|(name, _)| name.clone())
            .collect()
    }
}

impl From<ActionsActionResult> for CoreActionResult {
    fn from(ar: ActionsActionResult) -> Self {
        CoreActionResult {
            success: ar.success,
            data: ar.data.unwrap_or(serde_json::Value::Null),
            error: ar.error,
        }
    }
}

// Static assertion: BridgedExecutor must be Send + Sync (required by ActionExecutor).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BridgedExecutor>();
};

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
