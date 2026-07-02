//! タスク台帳アクション（LOOPS III+IV: 前向きワーキング状態の永続化）。
//!
//! goal / 契約（受け入れ条件）/ 進捗 / 決定を SQLite に永続化し、context 圧縮や
//! 再起動をまたいで作業状態を維持する。セッションごとに active タスクは1件。
//! agent_id / session_id は `ActionContext` から解決し、LLM には ID を渡させない。

use async_trait::async_trait;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult};

/// ctx から (agent_id, session_id) を取り出す。session が無い文脈では台帳は使えない。
fn session_scope(ctx: &ActionContext) -> Result<(&str, &str), ActionResult> {
    match ctx.session_id.as_deref() {
        Some(session_id) => Ok((ctx.agent_id.as_str(), session_id)),
        None => Err(ActionResult::error(
            "task ledger requires a session context (no session_id available)",
        )),
    }
}

fn task_json(task: &opencrab_db::queries::TaskLedgerRow) -> serde_json::Value {
    json!({
        "task_id": task.id,
        "goal": task.goal,
        "contract": task.contract,
        "status": task.status,
        "created_at": task.created_at,
        "updated_at": task.updated_at,
    })
}

/// タスクを開始する（goal + 受け入れ条件の契約）
pub struct OpenTaskAction;

#[async_trait]
impl Action for OpenTaskAction {
    fn name(&self) -> &str {
        "open_task"
    }

    fn description(&self) -> &str {
        "タスク台帳に新しいタスクを開く。goal（何を達成するか）と contract（受け入れ条件 = 完了と判断できるテスト可能な条件）を記録する。contract は作業を始める前にユーザーと合意すること。台帳はDBに永続化され、コンテキスト圧縮や再起動をまたいで保持される。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["goal"],
            "properties": {
                "goal": {
                    "type": "string",
                    "description": "このタスクで達成すること"
                },
                "contract": {
                    "type": "string",
                    "description": "受け入れ条件。どうなったら完了とみなすか（テスト可能な形で）。作業開始前にユーザーと合意した内容を書く"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let (agent_id, session_id) = match session_scope(ctx) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let Some(goal) = args["goal"].as_str().filter(|g| !g.trim().is_empty()) else {
            return ActionResult::error("goal is required");
        };
        let contract = args["contract"].as_str().filter(|c| !c.trim().is_empty());

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(e) => return ActionResult::error(&format!("Failed to acquire DB lock: {e}")),
        };

        match opencrab_db::queries::get_active_task_for_session(&conn, agent_id, session_id) {
            Ok(Some(existing)) => {
                return ActionResult::error(&format!(
                    "An active task already exists in this session: #{} \"{}\". Close it with close_task, or record progress on it instead.",
                    existing.id, existing.goal
                ));
            }
            Ok(None) => {}
            Err(e) => return ActionResult::error(&format!("Failed to check active task: {e}")),
        }

        match opencrab_db::queries::insert_task_ledger(&conn, agent_id, session_id, goal, contract)
        {
            Ok(task_id) => ActionResult::success(json!({
                "task_id": task_id,
                "goal": goal,
                "contract": contract,
                "status": "active",
            })),
            Err(e) => ActionResult::error(&format!("Failed to open task: {e}")),
        }
    }
}

/// goal / 契約を再交渉する
pub struct UpdateTaskContractAction;

#[async_trait]
impl Action for UpdateTaskContractAction {
    fn name(&self) -> &str {
        "update_task_contract"
    }

    fn description(&self) -> &str {
        "現在の active タスクの goal / contract（受け入れ条件）を更新する。作業中に完了条件をユーザーと再交渉した場合に使う。指定しなかったフィールドは変更されない。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "goal": {
                    "type": "string",
                    "description": "新しい goal（変更する場合のみ）"
                },
                "contract": {
                    "type": "string",
                    "description": "新しい受け入れ条件（変更する場合のみ）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let (agent_id, session_id) = match session_scope(ctx) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let goal = args["goal"].as_str().filter(|g| !g.trim().is_empty());
        let contract = args["contract"].as_str().filter(|c| !c.trim().is_empty());
        if goal.is_none() && contract.is_none() {
            return ActionResult::error("at least one of goal / contract is required");
        }

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(e) => return ActionResult::error(&format!("Failed to acquire DB lock: {e}")),
        };

        let task = match opencrab_db::queries::get_active_task_for_session(
            &conn, agent_id, session_id,
        ) {
            Ok(Some(t)) => t,
            Ok(None) => {
                return ActionResult::error("no active task in this session — call open_task first")
            }
            Err(e) => return ActionResult::error(&format!("Failed to get active task: {e}")),
        };

        match opencrab_db::queries::update_task_goal_contract(&conn, agent_id, task.id, goal, contract)
        {
            Ok(true) => match opencrab_db::queries::get_task_ledger(&conn, agent_id, task.id) {
                Ok(Some(updated)) => ActionResult::success(task_json(&updated)),
                _ => ActionResult::success(json!({ "task_id": task.id })),
            },
            Ok(false) => ActionResult::error("task disappeared during update"),
            Err(e) => ActionResult::error(&format!("Failed to update task: {e}")),
        }
    }
}

/// 進捗・決定・ブロッカーを追記する
pub struct RecordTaskProgressAction;

const PROGRESS_KINDS: [&str; 3] = ["progress", "decision", "blocker"];

#[async_trait]
impl Action for RecordTaskProgressAction {
    fn name(&self) -> &str {
        "record_task_progress"
    }

    fn description(&self) -> &str {
        "現在の active タスクに進捗を追記する（追記式・永続）。意味のあるステップの完了ごとに kind=progress、方針決定は kind=decision（なぜそう決めたかも書く）、障害は kind=blocker で記録する。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["content"],
            "properties": {
                "content": {
                    "type": "string",
                    "description": "記録する内容。decision の場合は理由（WHY）も含める"
                },
                "kind": {
                    "type": "string",
                    "enum": PROGRESS_KINDS,
                    "description": "エントリ種別（デフォルト: progress）",
                    "default": "progress"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let (agent_id, session_id) = match session_scope(ctx) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let Some(content) = args["content"].as_str().filter(|c| !c.trim().is_empty()) else {
            return ActionResult::error("content is required");
        };
        let kind = args["kind"].as_str().unwrap_or("progress");
        if !PROGRESS_KINDS.contains(&kind) {
            return ActionResult::error(&format!(
                "invalid kind '{kind}' — must be one of: progress, decision, blocker"
            ));
        }

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(e) => return ActionResult::error(&format!("Failed to acquire DB lock: {e}")),
        };

        let task = match opencrab_db::queries::get_active_task_for_session(
            &conn, agent_id, session_id,
        ) {
            Ok(Some(t)) => t,
            Ok(None) => {
                return ActionResult::error("no active task in this session — call open_task first")
            }
            Err(e) => return ActionResult::error(&format!("Failed to get active task: {e}")),
        };

        match opencrab_db::queries::insert_task_progress(&conn, task.id, kind, content) {
            Ok(progress_id) => ActionResult::success(json!({
                "task_id": task.id,
                "progress_id": progress_id,
                "kind": kind,
            })),
            Err(e) => ActionResult::error(&format!("Failed to record progress: {e}")),
        }
    }
}

/// タスクを閉じる（done / abandoned）
pub struct CloseTaskAction;

#[async_trait]
impl Action for CloseTaskAction {
    fn name(&self) -> &str {
        "close_task"
    }

    fn description(&self) -> &str {
        "現在の active タスクを閉じる。contract（受け入れ条件）を満たしたら status=done、中止するなら status=abandoned。summary を渡すと最後の進捗エントリとして記録される。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["status"],
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["done", "abandoned"],
                    "description": "done = 受け入れ条件を満たして完了 / abandoned = 中止"
                },
                "summary": {
                    "type": "string",
                    "description": "締めの要約（結果、残課題など）。進捗ログの最終エントリとして追記される"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let (agent_id, session_id) = match session_scope(ctx) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let status = match args["status"].as_str() {
            Some(s @ ("done" | "abandoned")) => s,
            Some(other) => {
                return ActionResult::error(&format!(
                    "invalid status '{other}' — must be 'done' or 'abandoned'"
                ))
            }
            None => return ActionResult::error("status is required ('done' or 'abandoned')"),
        };
        let summary = args["summary"].as_str().filter(|s| !s.trim().is_empty());

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(e) => return ActionResult::error(&format!("Failed to acquire DB lock: {e}")),
        };

        let task = match opencrab_db::queries::get_active_task_for_session(
            &conn, agent_id, session_id,
        ) {
            Ok(Some(t)) => t,
            Ok(None) => {
                return ActionResult::error("no active task in this session — nothing to close")
            }
            Err(e) => return ActionResult::error(&format!("Failed to get active task: {e}")),
        };

        if let Some(summary) = summary {
            if let Err(e) =
                opencrab_db::queries::insert_task_progress(&conn, task.id, "progress", summary)
            {
                return ActionResult::error(&format!("Failed to record summary: {e}"));
            }
        }

        match opencrab_db::queries::update_task_status(&conn, agent_id, task.id, status) {
            Ok(true) => ActionResult::success(json!({
                "task_id": task.id,
                "goal": task.goal,
                "status": status,
            })),
            Ok(false) => ActionResult::error("task disappeared during close"),
            Err(e) => ActionResult::error(&format!("Failed to close task: {e}")),
        }
    }
}

/// タスクと全進捗履歴を取得する
pub struct GetTaskAction;

/// get_task が返す進捗エントリ数の上限。
const GET_TASK_PROGRESS_CAP: usize = 200;

#[async_trait]
impl Action for GetTaskAction {
    fn name(&self) -> &str {
        "get_task"
    }

    fn description(&self) -> &str {
        "タスク台帳からタスクと進捗履歴を取得する。task_id 省略時はこのセッションの active タスク。会話に注入される [Task Ledger] セクションは直近分のみなので、全履歴が必要なときに使う。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "integer",
                    "description": "取得するタスクID（省略時: このセッションの active タスク）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let (agent_id, session_id) = match session_scope(ctx) {
            Ok(s) => s,
            Err(e) => return e,
        };

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(e) => return ActionResult::error(&format!("Failed to acquire DB lock: {e}")),
        };

        let task = match args["task_id"].as_i64() {
            Some(task_id) => match opencrab_db::queries::get_task_ledger(&conn, agent_id, task_id)
            {
                Ok(Some(t)) => t,
                Ok(None) => return ActionResult::error(&format!("task #{task_id} not found")),
                Err(e) => return ActionResult::error(&format!("Failed to get task: {e}")),
            },
            None => match opencrab_db::queries::get_active_task_for_session(
                &conn, agent_id, session_id,
            ) {
                Ok(Some(t)) => t,
                Ok(None) => {
                    return ActionResult::error(
                        "no active task in this session — pass task_id or call open_task first",
                    )
                }
                Err(e) => return ActionResult::error(&format!("Failed to get active task: {e}")),
            },
        };

        let progress = match opencrab_db::queries::list_recent_task_progress(
            &conn,
            task.id,
            GET_TASK_PROGRESS_CAP,
        ) {
            Ok(p) => p,
            Err(e) => return ActionResult::error(&format!("Failed to list progress: {e}")),
        };
        let total = opencrab_db::queries::count_task_progress(&conn, task.id).unwrap_or(-1);

        let mut result = task_json(&task);
        result["progress_total"] = json!(total);
        result["progress"] = json!(progress
            .iter()
            .map(|p| json!({
                "kind": p.kind,
                "content": p.content,
                "created_at": p.created_at,
            }))
            .collect::<Vec<_>>());
        ActionResult::success(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::*;
    use serde_json::json;

    fn test_context() -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: opencrab_db::Db::from_connection(conn),
            workspace: std::sync::Arc::new(ws),
            last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: std::sync::Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test-model".to_string(),
                active_model: None,
                available_providers: vec!["mock".to_string()],
                gateway: "test".to_string(),
            })),
            caller: CallerIdentity::Owner,
        };
        (dir, ctx)
    }

    #[tokio::test]
    async fn test_open_task_and_duplicate_open_fails() {
        let (_dir, ctx) = test_context();
        let result = OpenTaskAction
            .execute(&json!({"goal": "ship it", "contract": "tests green"}), &ctx)
            .await;
        assert!(result.success);
        let task_id = result.data.unwrap()["task_id"].as_i64().unwrap();

        // 2つ目の open は既存タスク情報付きで拒否
        let dup = OpenTaskAction.execute(&json!({"goal": "another"}), &ctx).await;
        assert!(!dup.success);
        let msg = dup.error.unwrap();
        assert!(msg.contains(&format!("#{task_id}")));
        assert!(msg.contains("ship it"));
    }

    #[tokio::test]
    async fn test_record_without_task_fails() {
        let (_dir, ctx) = test_context();
        let result = RecordTaskProgressAction
            .execute(&json!({"content": "step"}), &ctx)
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("open_task"));
    }

    #[tokio::test]
    async fn test_record_rejects_invalid_kind() {
        let (_dir, ctx) = test_context();
        OpenTaskAction.execute(&json!({"goal": "g"}), &ctx).await;
        let result = RecordTaskProgressAction
            .execute(&json!({"content": "x", "kind": "musing"}), &ctx)
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("invalid kind"));
    }

    #[tokio::test]
    async fn test_full_lifecycle_open_record_close() {
        let (_dir, ctx) = test_context();
        let open = OpenTaskAction.execute(&json!({"goal": "g"}), &ctx).await;
        assert!(open.success);

        let rec = RecordTaskProgressAction
            .execute(&json!({"content": "decided X because Y", "kind": "decision"}), &ctx)
            .await;
        assert!(rec.success);

        let close = CloseTaskAction
            .execute(&json!({"status": "done", "summary": "all finished"}), &ctx)
            .await;
        assert!(close.success);
        assert_eq!(close.data.unwrap()["status"], "done");

        // close 後は active タスクが無い
        let rec2 = RecordTaskProgressAction
            .execute(&json!({"content": "late"}), &ctx)
            .await;
        assert!(!rec2.success);

        // get_task(task_id) で summary が最終進捗として見える
        {
            let conn = ctx.db.lock().unwrap();
            let task =
                opencrab_db::queries::get_active_task_for_session(&conn, "agent-1", "session-1")
                    .unwrap();
            assert!(task.is_none());
        }
        let get = GetTaskAction.execute(&json!({"task_id": 1}), &ctx).await;
        assert!(get.success);
        let data = get.data.unwrap();
        assert_eq!(data["status"], "done");
        let progress = data["progress"].as_array().unwrap();
        assert_eq!(progress.len(), 2);
        assert_eq!(progress[0]["kind"], "decision");
        assert_eq!(progress[1]["content"], "all finished");
    }

    #[tokio::test]
    async fn test_update_contract() {
        let (_dir, ctx) = test_context();
        OpenTaskAction.execute(&json!({"goal": "g"}), &ctx).await;

        let none = UpdateTaskContractAction.execute(&json!({}), &ctx).await;
        assert!(!none.success);

        let upd = UpdateTaskContractAction
            .execute(&json!({"contract": "CI green"}), &ctx)
            .await;
        assert!(upd.success);
        let data = upd.data.unwrap();
        assert_eq!(data["goal"], "g");
        assert_eq!(data["contract"], "CI green");
    }

    #[tokio::test]
    async fn test_requires_session_context() {
        let (_dir, mut ctx) = test_context();
        ctx.session_id = None;
        let result = OpenTaskAction.execute(&json!({"goal": "g"}), &ctx).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("session"));
    }
}
