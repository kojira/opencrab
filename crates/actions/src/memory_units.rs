//! 記憶の単位（宣言）道具 4 つ（issue #379 #376 段階1）。
//!
//! エージェントが自分の生ログ（memory_sessions）を**俯瞰**し、**範囲を読み**、まとまりを
//! **宣言**する道具。宣言は `node_type='unit'` / `source_type='declared'` で
//! `memory_index_nodes` に載る（v30 で CHECK 拡張）。既存の time-series topic
//! （`node_type='topic'`）とは別 `node_type` なので、索引ビルド・タグ整理・月次ロールアップの
//! worklist へ**構造的に混ざらない**（#379 監査で確定）。
//!
//! 4 つとも **TRUSTED_ONLY**（`bridge::TRUSTED_ONLY_ACTIONS`）で Nostr（caller=Agent）からは
//! list_tools に出ず dispatch でも拒否される。読み取り 2 つ（survey / read）は整理ラン用の
//! `ORGANIZE_ALLOWED_TOOLS` にも入る。記録 2 つ（record / retract）は段階2 まで入れない。
//!
//! 有界化（687 発話の塊を一度に吐かせない）: `read_my_history` は行数 + 総文字数の
//! ハードキャップ + カーソル。`survey_my_history` はバケット数に上限。**生ログは読むだけ**。

use async_trait::async_trait;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult};

/// `read_my_history` の行数ハードキャップ。
const READ_ROW_CAP: usize = 200;
/// `read_my_history` の総文字数ハードキャップ（本文の累計）。
const READ_CHAR_CAP: usize = 40_000;
/// `survey_my_history` のバケット数上限（既定）。
const SURVEY_DEFAULT_MAX_BUCKETS: usize = 60;
/// `survey_my_history` のバケット数上限（これ以上は要求されても丸める）。
const SURVEY_HARD_MAX_BUCKETS: usize = 400;
/// `read_my_history` の `around` の既定半径。
const AROUND_DEFAULT_RADIUS: i64 = 20;
/// `read_my_history` の `around` の半径上限。
const AROUND_MAX_RADIUS: i64 = 100;

/// 生ログを日/時/週で俯瞰する（地図）。
pub struct SurveyMyHistoryAction;

#[async_trait]
impl Action for SurveyMyHistoryAction {
    fn name(&self) -> &str {
        "survey_my_history"
    }

    fn description(&self) -> &str {
        "自分の生ログを日/時/週で俯瞰する（地図）。バケットごとに件数・セッション数・id 範囲・種別内訳を返す。範囲を絞ったら read_my_history で読み、まとまりを record_memory_unit で宣言できる。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "granularity": {
                    "type": "string",
                    "enum": ["day", "hour", "week"],
                    "description": "集計粒度（既定: day）",
                    "default": "day"
                },
                "max_buckets": {
                    "type": "integer",
                    "description": format!("返すバケット数の上限（既定 {SURVEY_DEFAULT_MAX_BUCKETS} / 最大 {SURVEY_HARD_MAX_BUCKETS}）。新しいバケットから返す。"),
                    "default": SURVEY_DEFAULT_MAX_BUCKETS
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let granularity = match args["granularity"].as_str() {
            Some(g @ ("day" | "hour" | "week")) => g,
            Some(other) => {
                return ActionResult::error(&format!(
                    "granularity は day / hour / week のいずれか（受領: {other}）"
                ))
            }
            None => "day",
        };
        let max_buckets = args["max_buckets"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(SURVEY_DEFAULT_MAX_BUCKETS)
            .clamp(1, SURVEY_HARD_MAX_BUCKETS);

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        match opencrab_db::queries::survey_my_history(
            &conn,
            &ctx.agent_id,
            granularity,
            max_buckets,
        ) {
            Ok(survey) => match serde_json::to_value(&survey) {
                Ok(v) => ActionResult::success(v),
                Err(e) => ActionResult::error(&format!("survey のシリアライズに失敗: {e}")),
            },
            Err(e) => ActionResult::error(&format!("survey_my_history に失敗しました: {e}")),
        }
    }
}

/// 生ログを範囲指定で読む（有界: 行数 + 文字数キャップ + カーソル）。
pub struct ReadMyHistoryAction;

#[async_trait]
impl Action for ReadMyHistoryAction {
    fn name(&self) -> &str {
        "read_my_history"
    }

    fn description(&self) -> &str {
        "自分の生ログを範囲指定で読む。指定は次のいずれか 1 つ: session_id（セッション単位）/ from_id+to_id（id 範囲）/ around_id(+radius)（ある id の前後）/ from_time+to_time（時刻範囲）。行数と文字数の上限で打ち切り、続きは next_from_id を cursor_from_id に渡して読む。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "セッション単位で読む" },
                "from_id": { "type": "integer", "description": "id 範囲の下端（to_id と対で使う）" },
                "to_id": { "type": "integer", "description": "id 範囲の上端（from_id と対で使う）" },
                "around_id": { "type": "integer", "description": "この id の前後を読む" },
                "radius": { "type": "integer", "description": format!("around_id の前後件数（既定 {AROUND_DEFAULT_RADIUS} / 最大 {AROUND_MAX_RADIUS}）") },
                "from_time": { "type": "string", "description": "時刻範囲の開始（RFC3339。to_time と対で使う）" },
                "to_time": { "type": "string", "description": "時刻範囲の終了（RFC3339。from_time と対で使う）" },
                "cursor_from_id": { "type": "integer", "description": "続きを読む: 前回の next_from_id をここに渡す" }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        use opencrab_db::queries::HistoryFilter;

        // 排他的にどの範囲指定が来ているかを判定する。
        let has_session = args.get("session_id").and_then(|v| v.as_str()).is_some();
        let has_id_range = args.get("from_id").is_some() || args.get("to_id").is_some();
        let has_around = args.get("around_id").is_some();
        let has_time = args.get("from_time").is_some() || args.get("to_time").is_some();

        let mode_count = [has_session, has_id_range, has_around, has_time]
            .iter()
            .filter(|b| **b)
            .count();
        if mode_count == 0 {
            return ActionResult::error(
                "範囲指定が必要です: session_id / from_id+to_id / around_id / from_time+to_time のいずれか",
            );
        }
        if mode_count > 1 {
            return ActionResult::error(
                "範囲指定は 1 つだけにしてください（session_id / id 範囲 / around / 時刻範囲）",
            );
        }

        let filter = if has_session {
            HistoryFilter::Session(args["session_id"].as_str().unwrap().to_string())
        } else if has_id_range {
            let from_id = match args["from_id"].as_i64() {
                Some(v) => v,
                None => {
                    return ActionResult::error(
                        "from_id と to_id を両方 integer で指定してください",
                    )
                }
            };
            let to_id = match args["to_id"].as_i64() {
                Some(v) => v,
                None => {
                    return ActionResult::error(
                        "from_id と to_id を両方 integer で指定してください",
                    )
                }
            };
            HistoryFilter::IdRange { from_id, to_id }
        } else if has_around {
            let center_id = match args["around_id"].as_i64() {
                Some(v) => v,
                None => return ActionResult::error("around_id は integer で指定してください"),
            };
            let radius = args["radius"]
                .as_i64()
                .unwrap_or(AROUND_DEFAULT_RADIUS)
                .clamp(1, AROUND_MAX_RADIUS);
            HistoryFilter::Around { center_id, radius }
        } else {
            let from_time = match args["from_time"].as_str() {
                Some(v) if !v.is_empty() => v.to_string(),
                _ => {
                    return ActionResult::error(
                        "from_time と to_time を両方指定してください（RFC3339）",
                    )
                }
            };
            let to_time = match args["to_time"].as_str() {
                Some(v) if !v.is_empty() => v.to_string(),
                _ => {
                    return ActionResult::error(
                        "from_time と to_time を両方指定してください（RFC3339）",
                    )
                }
            };
            HistoryFilter::TimeRange { from_time, to_time }
        };

        let cursor_from_id = args["cursor_from_id"].as_i64();

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        match opencrab_db::queries::read_my_history(
            &conn,
            &ctx.agent_id,
            &filter,
            cursor_from_id,
            READ_ROW_CAP,
            READ_CHAR_CAP,
        ) {
            Ok(page) => match serde_json::to_value(&page) {
                Ok(v) => ActionResult::success(v),
                Err(e) => ActionResult::error(&format!("history のシリアライズに失敗: {e}")),
            },
            Err(e) => ActionResult::error(&format!("read_my_history に失敗しました: {e}")),
        }
    }
}

/// 生ログの範囲 `[from_id, to_id]` を 1 つの記憶として宣言する。
pub struct RecordMemoryUnitAction;

#[async_trait]
impl Action for RecordMemoryUnitAction {
    fn name(&self) -> &str {
        "record_memory_unit"
    }

    fn description(&self) -> &str {
        "自分の生ログの範囲 [from_id, to_id] を『1 つの記憶』として宣言する。title 必須・summary 任意・tags 任意。生ログは消さず、宣言は retract_memory_unit で取り消せる。重なり（1 範囲が複数の宣言に属する）は許される。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["from_id", "to_id", "title"],
            "properties": {
                "from_id": { "type": "integer", "description": "範囲の下端（生ログの id）" },
                "to_id": { "type": "integer", "description": "範囲の上端（生ログの id）" },
                "title": { "type": "string", "description": "この記憶のタイトル（必須）" },
                "summary": { "type": "string", "description": "要約（任意）" },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "付けるタグ名（任意・複数可・無い名前は新設）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let from_id = match args["from_id"].as_i64() {
            Some(v) => v,
            None => return ActionResult::error("from_id は integer で指定してください"),
        };
        let to_id = match args["to_id"].as_i64() {
            Some(v) => v,
            None => return ActionResult::error("to_id は integer で指定してください"),
        };
        let title = match args["title"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("title is required"),
        };
        let summary = args["summary"].as_str().unwrap_or("").to_string();
        let tags: Vec<String> = args["tags"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        // 範囲にこのエージェントの生ログが実在するか確認し、date_from/date_to を埋める
        // （他エージェントの id や空範囲を宣言させない）。
        let meta = match opencrab_db::queries::log_range_meta(&conn, &ctx.agent_id, from_id, to_id) {
            Ok(Some(m)) => m,
            Ok(None) => {
                return ActionResult::error(
                    "指定範囲にこのエージェントの生ログがありません（from_id / to_id を確認してください）",
                )
            }
            Err(e) => return ActionResult::error(&format!("範囲の確認に失敗しました: {e}")),
        };
        let now = chrono::Utc::now().to_rfc3339();
        let node = match opencrab_db::queries::record_memory_unit(
            &conn,
            &ctx.agent_id,
            &title,
            &summary,
            from_id,
            to_id,
            Some(&meta.min_created_at),
            Some(&meta.max_created_at),
            &now,
        ) {
            Ok(n) => n,
            Err(e) => return ActionResult::error(&format!("記憶の宣言に失敗しました: {e}")),
        };

        // タグ（任意）。宣言ノード id を topic_id として既存タグ基盤へ付与する。
        // 付与失敗は宣言自体を無効にしない（宣言は成立済み・agent が付け直せる）。
        let mut tag_error: Option<String> = None;
        if !tags.is_empty() {
            if let Err(e) =
                opencrab_db::queries::tag_topic(&conn, &ctx.agent_id, &node.id, &tags, &now)
            {
                tag_error = Some(format!("タグ付けに失敗しました: {e}"));
            }
        }

        ActionResult::success(json!({
            "unit_id": node.id,
            "short_id": node.short_id,
            "title": node.title,
            "from_id": from_id,
            "to_id": to_id,
            "date_from": node.date_from,
            "date_to": node.date_to,
            "logs_in_range": meta.count,
            "tags": tags,
            "tag_error": tag_error,
        }))
    }
}

/// 宣言ユニットを取り消す（宣言ノード + FTS + member のみ削除。生ログは不変）。
pub struct RetractMemoryUnitAction;

#[async_trait]
impl Action for RetractMemoryUnitAction {
    fn name(&self) -> &str {
        "retract_memory_unit"
    }

    fn description(&self) -> &str {
        "record_memory_unit で宣言した記憶を取り消す。宣言ノードと FTS 行と付けたタグの付与だけを消す。生ログには触らない（何度でもやり直せる）。宣言ユニット以外は消せない。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["unit_id"],
            "properties": {
                "unit_id": {
                    "type": "string",
                    "description": "取り消す宣言ユニットの short_id またはフル node_id"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let unit_id = match args["unit_id"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("unit_id is required"),
        };
        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        match opencrab_db::queries::retract_memory_unit(&conn, &ctx.agent_id, &unit_id) {
            Ok(full_id) => ActionResult::success(json!({
                "retracted": true,
                "unit_id": full_id,
            })),
            Err(e) => ActionResult::error(&format!("宣言の取り消しに失敗しました: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{ActionContext, CallerIdentity};
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

    fn seed_logs(ctx: &ActionContext, n: usize) {
        let conn = ctx.db.lock().unwrap();
        for i in 0..n {
            opencrab_db::queries::insert_session_log(
                &conn,
                &opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: "agent-1".to_string(),
                    session_id: "session-1".to_string(),
                    log_type: "message".to_string(),
                    content: format!("発話 {i}"),
                    speaker_id: None,
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                },
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn survey_empty_history() {
        let (_d, ctx) = test_context();
        let r = SurveyMyHistoryAction.execute(&json!({}), &ctx).await;
        assert!(r.success);
        let data = r.data.unwrap();
        assert_eq!(data["total_logs"], 0);
        assert_eq!(data["granularity"], "day");
    }

    #[tokio::test]
    async fn read_requires_a_range() {
        let (_d, ctx) = test_context();
        let r = ReadMyHistoryAction.execute(&json!({}), &ctx).await;
        assert!(!r.success);
    }

    #[tokio::test]
    async fn read_rejects_multiple_ranges() {
        let (_d, ctx) = test_context();
        let r = ReadMyHistoryAction
            .execute(&json!({"session_id": "s", "around_id": 1}), &ctx)
            .await;
        assert!(!r.success);
    }

    #[tokio::test]
    async fn record_read_and_retract_roundtrip() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 4);

        // record
        let r = RecordMemoryUnitAction
            .execute(
                &json!({"from_id": 1, "to_id": 4, "title": "所有権の話", "tags": ["Rust"]}),
                &ctx,
            )
            .await;
        assert!(r.success, "record failed: {:?}", r.error);
        let data = r.data.unwrap();
        let short_id = data["short_id"].as_str().unwrap().to_string();
        assert_eq!(data["logs_in_range"], 4);
        assert_eq!(data["tags"][0], "Rust");
        assert!(data["tag_error"].is_null());

        // read (id range)
        let rr = ReadMyHistoryAction
            .execute(&json!({"from_id": 1, "to_id": 4}), &ctx)
            .await;
        assert!(rr.success);
        assert_eq!(rr.data.unwrap()["returned"], 4);

        // retract by short_id
        let rt = RetractMemoryUnitAction
            .execute(&json!({"unit_id": short_id}), &ctx)
            .await;
        assert!(rt.success, "retract failed: {:?}", rt.error);
        assert_eq!(rt.data.unwrap()["retracted"], true);
    }

    #[tokio::test]
    async fn record_rejects_empty_range() {
        let (_d, ctx) = test_context();
        // 生ログが無いので範囲は空 → エラー。
        let r = RecordMemoryUnitAction
            .execute(&json!({"from_id": 1, "to_id": 4, "title": "x"}), &ctx)
            .await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("生ログ"));
    }

    #[tokio::test]
    async fn retract_rejects_missing_unit() {
        let (_d, ctx) = test_context();
        let r = RetractMemoryUnitAction
            .execute(&json!({"unit_id": "nope"}), &ctx)
            .await;
        assert!(!r.success);
    }
}
