use std::collections::HashSet;
use std::sync::Arc;

use futures::FutureExt;
use opencrab_core::{
    ActionExecutor, ActionResult, DispatchCall, DispatchOutcome, FunctionDefinition, ToolDispatcher,
};

use super::settle::{settle_completed, SettleContext};
use super::sink::SubtaskCompletionSink;
use super::{
    CallerIdentity, SpawnedSubtask, SubtaskLifecycle, SubtaskRegistry,
    DEFAULT_DISPATCH_TIMEOUT_SECS,
};

// ---------------------------------------------------------------------------
// S3a: ツール呼び出しバッチを subtask として実行する dispatcher（非ブロック / 全ツール自動化）
// ---------------------------------------------------------------------------

/// `Arc<dyn ActionExecutor>` を `ActionExecutor` として使えるようにする薄い委譲ラッパ。
///
/// `SkillEngine::new` は `Box<dyn ActionExecutor>` を取るが、S3a では同じ合成 executor
/// を engine（inline 実行用）と `SubtaskToolDispatcher`（background 実行用）で**共有**
/// したい。executor を 1 つの `Arc` にまとめ、engine には `Box::new(SharedExecutor(arc))`、
/// dispatcher には `arc.clone()` を渡すことで、`nostr_generate_key` 等 server ツールを
/// 含む合成 gateway 到達性（RFC #152 S2）を dispatch 経路でも共有する。
pub struct SharedExecutor(pub Arc<dyn ActionExecutor>);

#[async_trait::async_trait]
impl ActionExecutor for SharedExecutor {
    async fn execute(&self, name: &str, args: &serde_json::Value) -> ActionResult {
        self.0.execute(name, args).await
    }
    async fn execute_with_id(
        &self,
        name: &str,
        args: &serde_json::Value,
        tool_call_id: &str,
    ) -> ActionResult {
        self.0.execute_with_id(name, args, tool_call_id).await
    }
    fn list_tools(&self) -> Vec<FunctionDefinition> {
        self.0.list_tools()
    }
    fn inline_tool_names(&self) -> HashSet<String> {
        self.0.inline_tool_names()
    }
    fn utterance_tool_names(&self) -> HashSet<String> {
        self.0.utterance_tool_names()
    }
}

/// 既定で auto-dispatch **しない**（＝ inline 実行のまま）ツールのうち、**制御ツールと
/// core アクションだけ**の集合。
///
/// gateway / MCP のツールは、依存の向き（gateway → actions）の都合でここから定義を舐め
/// られない。それらの inline 判定は各ツール定義の属性（`GatewayActionDef.class.dispatch`）
/// が権威になり、`BridgedExecutor` の `ActionExecutor::inline_tool_names` override が索引
/// から `dispatch == Inline` を集めて**この集合と合わせて**返す。
/// `SubtaskToolDispatcher::new` はその `executor.inline_tool_names()` を非同期化除外集合に
/// 使う。
///
/// # 分類基準（この 6 つのどれかに当てはまるツールは inline に残す）
///
/// 1. **制御系**（`spawn_subtask` / `cancel_subtask` / `report_progress` / `steer_subtask`）:
///    それ自体が subtask ライフサイクルを操作する（`steer_subtask` は走行中サブへ追加指示を
///    差し込む / #647）ため background 化しない。
/// 2. **配送系**（Discord 送信・A2UI 送信・VC 参加/退出・peer review 依頼・Nostr 送信）:
///    「送る」こと自体が応答であり、background 化して完了で再注入する意味がない。加えて
///    gateway が「明示送信したか」を親ターンの終わりに見て暗黙返信を抑制する場合
///    （Nostr）、background 化は**二重投稿**を生む。`send_ui` はさらに**ユーザーの応答を
///    待機する** pending interaction を張るため、background 化すると (a) UI 投稿と本文
///    返信の順序が入れ替わり、(b) エージェントは `spawned` しか見えずインタラクション
///    ID を扱えず、(c) クリックによる resume と subtask 決着の resume で返信が 2 通になる。
/// 3. **同ターン結果依存**: 戻り値（URL / ID）をそのターンの後続処理や応答本文に使う用法が
///    通常のもの（`ensure_webhook` / `ensure_subtask_webhook` / `discord_create_webhook` /
///    `discord_create_channel` / `nostr_upload`）。background 化すると値の代わりに
///    `spawned` が返るだけなので、用法そのものが壊れる。
/// 4. **run 内共有状態を書くツール**（`select_llm` / `discord_channel_config` /
///    `nostr_switch_identity`）: `ActionContext` の `model_override` / `current_purpose`、
///    チャンネルの writable、以後の送信 identity のように、走行中の run（や同ターンの配送）
///    が参照している状態を書き換える。background 化すると (a) そのターンには効かず、
///    (b) 書き込みが engine の次イテレーションや配送と競合して「いつのターンから効くか」が
///    非決定になる。制御の効き方を保つため inline に残す。
/// 5. **純粋な読み取りで即答すべきもの**（`list_*` / `get_*` / `ws_read` / `ws_list` /
///    `search_memory_index` / `retrieve_memory_nodes` / `read_heartbeat_instructions`）:
///    dispatch すると質問 1 つが 2 ターン 2 メッセージに割れるだけで、得るものが無い。
///    system prompt が指示する記憶想起フロー（`search_memory_index` →
///    `retrieve_memory_nodes`）のような**同ターンの 2 段連鎖**では、背景往復が 2 回 =
///    ユーザーへ 4 通になる。
/// 6. **報告する価値が無い短時間の書き込み**（`update_impression` / `save_model_insight` /
///    `record_task_progress`）: dispatch には必ず resume ターン（= ユーザーへの追加
///    メッセージ）が 1 本付く。値の小さい書き込みを background 化すると雑音が増えるだけ。
///
/// 逆に dispatch を**残す**のは「長時間かかる」か「同ターンで結果を使わない書き込み」のみ
/// （`rebuild_memory_index` / `create_skill` / `nostr_generate_key` / `ws_write` /
/// `learn_from_experience` / server ツールの `execute_shell` …）。
///
/// # MCP ツール（`mcp__*`）
///
/// **既定 inline**（安全側）。運用者が繋いだ任意の外部ツールなので、配送系（外部へ
/// 「送る」）なのか同ターンで戻り値を使うのかを静的に判定できない。無分類で全 dispatch
/// すると、外部送信系 MCP が background 化されて配送順が入れ替わる/二重送信になる。
/// 判定は [`SubtaskToolDispatcher::should_dispatch`] が名前の接頭辞で行う（一覧に列挙
/// できないため集合ではなく規則で扱う）。長時間の MCP ツールを background 化したい
/// 運用者は `with_non_dispatch` で当該名を除いた集合を渡す。
///
/// # 分類の強制
///
/// gateway / MCP のツールは `GatewayActionDef.class.dispatch` を**構築サイトで必ず**書く
/// （`ToolClass` は `Default` を持たない）ので、「新ツールを足したのに分類し忘れる」ドリフト
/// は型システムが防ぐ。core は `GatewayActionDef` を持たないため
/// [`crate::bridge::CORE_INLINE_ACTIONS`] / [`crate::bridge::CORE_DISPATCHABLE_ACTIONS`] の
/// 2 定数で分類し、`ActionDispatcher::new()` の全名がどちらか一方に属することを
/// `core_actions_are_classified_for_dispatch`（本モジュール）が fail-closed で検査する。
///
/// 呼び出し側は `SubtaskToolDispatcher::with_non_dispatch` で上書き/追加できる。
/// 運用者向けの**分類基準**は `docs/DESIGN.md`「非ブロックツール実行」節。
pub fn default_non_dispatch_tools() -> HashSet<String> {
    let mut set: HashSet<String> = [
        "spawn_subtask",
        "cancel_subtask",
        "report_progress",
        "steer_subtask",
        // §2.7: describe_tools は同ターンの活性集合を親 executor に書くため inline 固定
        // （detach すると子 executor に書いて親の list_tools に反映されない）。
        "describe_tools",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // core アクション（`ActionDispatcher::new()`）の inline 集合。以前はここが空で、
    // core 32 個が分類ガードの外＝全 dispatch だった（記憶想起が 4 通に割れる等）。
    for name in crate::bridge::CORE_INLINE_ACTIONS {
        set.insert((*name).to_string());
    }
    set
}

/// 「同一バッチのツール呼び出しを background subtask として実行する」job のランタイム
/// （RFC #152 S3a）。
///
/// `execute_spawn_subtask` が sub-engine（LLM ループ）を建てるのに対し、これは
/// **指定ツールを合成 executor で順に実行するだけ**の job。spawn / registry 登録
/// （`SpawnedSubtask`, label=`tool(主要引数)`）/ 完了時 `settle_completed`（DB 永続化
/// → registry 除去 → sink 発火）という中核は既存と共有し、job の中身だけ差し替える。
///
/// 不変条件:
/// - **1 バッチ = 1 subtask**。同一 assistant メッセージのツールは並行実行せず
///   `calls` の順に逐次実行する（`write_file` → `execute_shell` の依存順を守る）。
///   結果として完了通知（親の resume）も 1 バッチにつき 1 回で済む。
/// - **必ず決着する**。ツールがハングしても既定タイムアウト
///   （`DEFAULT_DISPATCH_TIMEOUT_SECS`）で `exit_reason="timeout"`、panic しても
///   `catch_unwind` で `exit_reason="error"` として settle し、registry に死骸を残さない。
/// - **cancel と競合しない**。`SubtaskLifecycle` により停止と決着は排他。
///
/// 既知の残課題: 1 run の**別イテレーション**でそれぞれツールが dispatch された場合
/// （LLM が spawned マーカーを見た後にさらにツールを呼ぶ）は subtask が 2 本になり、
/// 完了 sink も 2 回発火する。バッチ内（同一 assistant メッセージ）の N ツールは 1 本に
/// まとまるため、レビューで実証された「1 ターン N ツール → N 通の返信」は解消する。
///
/// `core::ToolDispatcher` を実装し、`SkillEngine` のツール実行点から呼ばれる。
pub struct SubtaskToolDispatcher {
    /// dispatch したツールを実行する合成 executor（server ツールを含む）。
    executor: Arc<dyn ActionExecutor>,
    /// 走行中 subtask の registry（settle 時に除去 / 将来の cancel/list 用）。
    registry: SubtaskRegistry,
    db: opencrab_db::Db,
    /// 完了再注入 sink（gateway 別。Discord=LoopEvent / Nostr=reply ...）。
    sink: Arc<dyn SubtaskCompletionSink>,
    agent_id: String,
    parent_session_id: String,
    /// auto-dispatch しないツール名（既定 = `default_non_dispatch_tools()`）。発話クラスも
    /// ここに含む（背景 subtask 化しない）。
    non_dispatch: HashSet<String>,
    /// 発話クラス（撃ちっぱなし・§3.3.1 C4）のツール名。`is_utterance` の権威。
    /// `non_dispatch` の部分集合で、engine が「inline だが撃ちっぱなし配送」を分ける。
    utterance: HashSet<String>,
    /// dispatch した subtask に付与する gateway 不透明な返信ルーティング token
    /// （#167）。この dispatcher は 1 回の親ターンに紐づくため、inbound の返信先を
    /// そのまま全 dispatch へ引き継ぐ。`None` なら返信配送先の指定なし（Discord は
    /// session_id から復元するため `None` のままでよい）。
    reply_target: Option<String>,
    /// 親 run（この dispatcher を持つターン）の呼び出し元（#298）。
    ///
    /// dispatch した `SpawnedSubtask.caller` に載り、settle 時に
    /// `SubtaskSettled.caller` として sink へ届く。resume が元の権限を落とさない
    /// ための引き継ぎで、既定は最小権限（`Agent`）。
    caller: CallerIdentity,
    /// バッチ全体の実行時間上限。超過すると `exit_reason="timeout"` で settle する。
    timeout: std::time::Duration,
    /// 上限超過の tool_result を退避するエージェントのワークスペース root
    /// （inline 経路 `process.rs` の `tool_result_workspace` と同じもの）。
    /// `None` なら退避せず切り詰める。
    workspace_root: Option<std::path::PathBuf>,
    /// 親ターンが「この run は subtask を起こしたか」を数えるカウンタ（#431）。
    /// 登録簿への登録が済んだところで加算する。`None` なら数えない。
    subtask_starts: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

impl SubtaskToolDispatcher {
    pub fn new(
        executor: Arc<dyn ActionExecutor>,
        registry: SubtaskRegistry,
        db: opencrab_db::Db,
        sink: Arc<dyn SubtaskCompletionSink>,
        agent_id: impl Into<String>,
        parent_session_id: impl Into<String>,
    ) -> Self {
        // 非同期化除外は executor の属性索引から引く（gateway / MCP の `dispatch == Inline`
        // ＋ 制御ツール ＋ core inline）。`BridgedExecutor` が override し、それ以外の
        // executor は既定（空）＋ `default_non_dispatch_tools` 相当を返す。
        let non_dispatch = executor.inline_tool_names();
        let utterance = executor.utterance_tool_names();
        Self {
            executor,
            registry,
            db,
            sink,
            agent_id: agent_id.into(),
            parent_session_id: parent_session_id.into(),
            non_dispatch,
            utterance,
            reply_target: None,
            caller: CallerIdentity::Agent,
            timeout: std::time::Duration::from_secs(DEFAULT_DISPATCH_TIMEOUT_SECS),
            workspace_root: None,
            subtask_starts: None,
        }
    }

    /// 親ターンの subtask 起動カウンタを設定する（#431）。
    ///
    /// 明示 `spawn_subtask` 経路（`SystemGatewayActions`）と**同じカウンタ**を共有し、
    /// 親ターンは 1 つの数で「次の行動を選んだか」を見る。
    pub fn with_subtask_starts(
        mut self,
        counter: Option<Arc<std::sync::atomic::AtomicUsize>>,
    ) -> Self {
        self.subtask_starts = counter;
        self
    }

    /// auto-dispatch 対象外の集合を差し替える。
    pub fn with_non_dispatch(mut self, non_dispatch: HashSet<String>) -> Self {
        self.non_dispatch = non_dispatch;
        self
    }

    /// バッチ実行のタイムアウトを差し替える（既定 = `DEFAULT_DISPATCH_TIMEOUT_SECS`）。
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 大きい tool_result の退避先（エージェントのワークスペース root）を設定する。
    ///
    /// inline 経路と同じ扱い（上限超過はファイルへ退避してポインタだけ DB に残す）を
    /// dispatch 経路でも行うために必要。未設定なら切り詰めのみ。
    pub fn with_workspace_root(mut self, root: Option<std::path::PathBuf>) -> Self {
        self.workspace_root = root;
        self
    }

    /// dispatch する subtask に載せる返信ルーティング token を設定する（#167）。
    ///
    /// settle 時に `settle_completed` が registry から引いて `SubtaskSettled`
    /// へ載せるため、sink は session_id に依らず返信先を得られる。
    pub fn with_reply_target(mut self, reply_target: Option<String>) -> Self {
        self.reply_target = reply_target;
        self
    }

    /// dispatch する subtask に載せる**親 run の呼び出し元**を設定する（#298）。
    ///
    /// settle 時に `settle_completed` が registry から引いて `SubtaskSettled` へ載せる
    /// ため、resume する sink は元のターンと同じ権限で親を再開できる。未設定なら
    /// 最小権限（`Agent`）のまま = 従来挙動。
    pub fn with_caller(mut self, caller: CallerIdentity) -> Self {
        self.caller = caller;
        self
    }
}

/// `tool(主要引数)` 形式のラベルを組む。args が object なら最初のキーの値（scalar）を
/// 40 文字までプレビューする。
fn dispatch_label(tool_name: &str, args: &serde_json::Value) -> String {
    let preview = args
        .as_object()
        .and_then(|m| m.values().next())
        .map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let preview: String = preview.chars().take(40).collect();
    if preview.is_empty() {
        format!("{tool_name}()")
    } else {
        format!("{tool_name}({preview})")
    }
}

/// バッチが背景化したツール名（重複を除いた `", "` 区切り / #184）。
///
/// 停止ログの `tool_name` に載る。以前は `spawn_subtask` 固定だったため、自動 dispatch
/// されたツールを停止すると「spawn_subtask がキャンセルされた」と描画され、直後の
/// 本文行（`subtask 'execute_shell(...)' was cancelled`）と矛盾していた。
fn batch_tool_names(calls: &[DispatchCall]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for c in calls {
        if !names.contains(&c.tool_name.as_str()) {
            names.push(&c.tool_name);
        }
    }
    names.join(", ")
}

/// バッチ全体のラベル（`tool(arg), tool(arg)` を 120 文字で打ち切る）。
fn batch_label(calls: &[DispatchCall]) -> String {
    let joined = calls
        .iter()
        .map(|c| dispatch_label(&c.tool_name, &c.args))
        .collect::<Vec<_>>()
        .join(", ");
    let truncated: String = joined.chars().take(120).collect();
    if truncated.len() < joined.len() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// 1 ツールの実行結果（バッチの決着理由と本文の組み立てに使う）。
struct CallOutcome {
    /// 呼び出し元 LLM の tool_call_id（同じツールを複数回呼んだバッチの対応付け）。
    tool_call_id: String,
    /// 永続化用に無害化済みの結果本文。
    sanitized: String,
    /// このツールが失敗（`success == false` / panic / 未実行）したか。
    failed: bool,
}

/// 複数ツールバッチの完了本文 1 要素を組む。
///
/// `sanitized` は原則 JSON 文字列なので、**文字列としてではなく構造として**埋める
/// （`serde_json::from_str` を試す）。文字列のまま入れると
/// `"result":"[{\"result\":\"{\\\"success\\\":true…"` のような三重エスケープになり、
/// 会話へ再注入するときの整形でさらにエスケープが増えてモデルが読めなくなる。
/// 上限超過時のメタ情報案内（`[Tool result withheld: … bytes, … lines …]`）のように
/// JSON にならない場合だけ文字列で入れる。
///
/// `tool_call_id` も必ず載せる（同じツールを複数回呼んだバッチは順序でしか対応が
/// 取れなかった）。
fn batch_result_entry(tool: &str, outcome: &CallOutcome) -> serde_json::Value {
    let result = serde_json::from_str::<serde_json::Value>(&outcome.sanitized)
        .unwrap_or_else(|_| serde_json::Value::String(outcome.sanitized.clone()));
    serde_json::json!({
        "tool": tool,
        "tool_call_id": outcome.tool_call_id,
        "result": result,
    })
}

impl ToolDispatcher for SubtaskToolDispatcher {
    fn should_dispatch(&self, tool_name: &str) -> bool {
        // MCP ツール（`mcp__*`）は既定 inline（安全側）。運用者が繋いだ任意ツールなので
        // 配送系かどうかを静的に分類できず、全 dispatch すると外部送信系 MCP が
        // background 化されて配送順の入れ替わり/二重送信になる。名前を静的に列挙できない
        // ため集合ではなく接頭辞の規則で扱う（`default_non_dispatch_tools` の doc 参照）。
        if tool_name.starts_with(crate::bridge::MCP_TOOL_PREFIX) {
            return false;
        }
        !self.non_dispatch.contains(tool_name)
    }

    fn is_utterance(&self, tool_name: &str) -> bool {
        self.utterance.contains(tool_name)
    }

    fn dispatch_batch(&self, calls: &[DispatchCall]) -> DispatchOutcome {
        let subtask_id = uuid::Uuid::new_v4().to_string();
        let sub_session_id = format!("subtask-{subtask_id}");
        let label = batch_label(calls);
        // 停止ログに載せる「実際に背景化したツール名」（#184）。
        let tool_names = batch_tool_names(calls);

        // 各種クローン（background タスクへムーブ）。
        let executor = self.executor.clone();
        let registry = self.registry.clone();
        let db = self.db.clone();
        let sink = self.sink.clone();
        let agent_id = self.agent_id.clone();
        let parent_session_id = self.parent_session_id.clone();
        let calls_owned = calls.to_vec();
        let subtask_id_task = subtask_id.clone();
        let sub_session_id_task = sub_session_id.clone();
        let timeout = self.timeout;
        let workspace_root = self.workspace_root.clone();
        // 停止/決着の排他ラッチ。registry のエントリ（cancel 側）とタスク本体
        // （settle 側）が同じラッチを共有する。
        let lifecycle = SubtaskLifecycle::new();
        let lifecycle_task = lifecycle.clone();

        // 開始ゲート: 親が registry へ insert し終えるまでタスク本体を走らせない
        // （即完了する job が親の insert より先に remove して running のままリークするのを防ぐ。
        //  execute_spawn_subtask と同じ不変条件）。
        let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();

        let join = tokio::spawn(async move {
            let _ = start_rx.await;

            // バッチ全体に 1 つの期限を与える（ハングしたツールで永久滞留しない）。
            let deadline = tokio::time::Instant::now() + timeout;
            let mut outcomes: Vec<(String, CallOutcome)> = Vec::with_capacity(calls_owned.len());
            let mut timed_out = false;

            // **逐次実行**（並行にしない）: LLM が並べた順序に依存関係があり得る。
            for call in &calls_owned {
                // panic を settle へ変換する（catch しないと task が unwind して
                // settle を通らず、registry に死骸が残り REST は永久 active になる）。
                let fut = std::panic::AssertUnwindSafe(executor.execute_with_id(
                    &call.tool_name,
                    &call.args,
                    &call.tool_call_id,
                ))
                .catch_unwind();

                let raw = match tokio::time::timeout_at(deadline, fut).await {
                    Ok(Ok(result)) => {
                        let failed = !result.success;
                        let json = serde_json::to_string(&result)
                            .unwrap_or_else(|_| r#"{"success":false}"#.to_string());
                        (json, failed)
                    }
                    Ok(Err(panic_payload)) => {
                        let msg = panic_message(&panic_payload);
                        tracing::error!(
                            tool = %call.tool_name,
                            subtask_id = %subtask_id_task,
                            "dispatched tool panicked; settling as error"
                        );
                        (
                            serde_json::json!({
                                "success": false,
                                "data": null,
                                "error": format!("tool '{}' panicked: {msg}", call.tool_name),
                            })
                            .to_string(),
                            true,
                        )
                    }
                    Err(_elapsed) => {
                        timed_out = true;
                        (
                            serde_json::json!({
                                "success": false,
                                "data": null,
                                "error": format!(
                                    "tool '{}' timed out after {}s",
                                    call.tool_name,
                                    timeout.as_secs()
                                ),
                            })
                            .to_string(),
                            true,
                        )
                    }
                };

                // inline 経路（process.rs の on_tool_result）と**同一**の無害化を通す:
                // 秘密フィールドのマスク ＋ サイズ上限/ワークスペース退避。
                let sanitized = opencrab_core::tool_result_log::sanitize_tool_result_for_log(
                    &call.tool_name,
                    &raw.0,
                    &parent_session_id,
                    &call.tool_call_id,
                    workspace_root.as_deref(),
                );
                let outcome = CallOutcome {
                    tool_call_id: call.tool_call_id.clone(),
                    sanitized,
                    failed: raw.1,
                };
                // cancel されたときに「どこまで進んだか」を親へ残せるよう、完走した
                // call の部分結果をラッチ（cancel 側と共有）へ積む。
                lifecycle_task.record_completed_call(batch_result_entry(&call.tool_name, &outcome));
                outcomes.push((call.tool_name.clone(), outcome));
                if timed_out {
                    // 期限切れ後は残りを実行しない（依存順のため後続は前提が崩れている）。
                    //
                    // ただし**未実行であることは本文に残す**（#152 レビュー P2）。
                    // system prompt は「同じツールを再呼びするな（もう走っている）」と
                    // 指示しているので、痕跡が無いとエージェントは未実行を知る手段が無く
                    // 依頼が無言で消える。
                    for skipped in calls_owned.iter().skip(outcomes.len()) {
                        outcomes.push((
                            skipped.tool_name.clone(),
                            CallOutcome {
                                tool_call_id: skipped.tool_call_id.clone(),
                                sanitized: serde_json::json!({
                                    "success": false,
                                    "data": null,
                                    "error": "skipped: batch timed out",
                                })
                                .to_string(),
                                failed: true,
                            },
                        ));
                    }
                    break;
                }
            }

            let exit_reason = if timed_out {
                "timeout"
            } else if outcomes.iter().any(|(_, o)| o.failed) {
                "error"
            } else {
                "completed"
            };
            // 完了本文（DB へ永続化。sink には運ばない = RFC §1.3）。
            // 単一ツールのときは従来と同じ「ツール結果 JSON そのまま」を保つ。
            let result_text = if calls_owned.len() == 1 {
                outcomes
                    .first()
                    .map(|(_, o)| o.sanitized.clone())
                    .unwrap_or_else(|| r#"{"success":false}"#.to_string())
            } else {
                // 複数ツール: 結果を**構造として**埋め、`tool_call_id` も載せる
                // （三重エスケープと対応付け不能の解消 = レビュー P2）。
                let arr: Vec<serde_json::Value> = outcomes
                    .iter()
                    .map(|(tool, o)| batch_result_entry(tool, o))
                    .collect();
                serde_json::to_string(&arr).unwrap_or_else(|_| r#"[]"#.to_string())
            };

            // #551: 個々のツール結果は上（`sanitize_tool_result_for_log`）で退避済み
            // （≤ 上限）だが、複数ツールの**結合本文**（batch 配列）は合算で巨大になりうる
            // （本番最大 134,863 文字。`tool_result` が退避されるのにこの完了本文だけ素通し
            // だった非対称）。同じ退避をこの結合本文にも掛け、上限超過なら workspace へ退避
            // して本文には notice（パス・行数・読み方）だけ残す。単一ツール／上限内は
            // no-op なので、閾値以下の小さい完了本文の見え方は変わらない。
            let result_text = opencrab_core::tool_result_log::sanitize_tool_result_for_log(
                "subtask_completed",
                &result_text,
                &parent_session_id,
                &subtask_id_task,
                workspace_root.as_deref(),
            );

            // 中核（gateway 非依存）: DB 永続化 → registry 除去 → sink 発火。
            // 順序契約（DB 記録 → 通知）は settle_completed が 1 箇所で保証する。
            settle_completed(
                &registry,
                &db,
                sink.as_ref(),
                SettleContext {
                    parent_session_id,
                    agent_id,
                    subtask_id: subtask_id_task,
                    sub_session_id: sub_session_id_task,
                    exit_reason: exit_reason.to_string(),
                    lifecycle: lifecycle_task,
                },
                &result_text,
            );
        });

        // registry へ登録（開始ゲート解放の前 = insert-before-run）。
        self.registry.insert(
            subtask_id.clone(),
            SpawnedSubtask {
                abort_handle: join.abort_handle(),
                session_id: sub_session_id,
                parent_session_id: self.parent_session_id.clone(),
                agent_id: self.agent_id.clone(),
                label: label.clone(),
                tool_name: tool_names,
                started_at: std::time::Instant::now(),
                reply_target: self.reply_target.clone(),
                // 親ターンの呼び出し元をそのまま引き継ぐ（#298）。
                caller: self.caller.clone(),
                lifecycle,
                // auto-dispatch はツールを順に実行するだけで LLM ループが無く、
                // `run_agent_response` を通らないため steer を読む主体がいない。steer 不可（#647）。
                steerable: false,
            },
        );
        // #431: 登録が成立した = このターンは「次の行動」を起こした。明示
        // `spawn_subtask` 経路と同じカウンタへ載せ、親ターンは 1 つの数で判定する。
        if let Some(c) = &self.subtask_starts {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        // insert 完了 → タスク本体の実行を許可する。
        let _ = start_tx.send(());

        DispatchOutcome { subtask_id, label }
    }
}

/// `catch_unwind` の payload から人間可読なメッセージを取り出す。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}
