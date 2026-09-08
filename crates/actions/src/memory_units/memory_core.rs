// ============================================
// 記憶の凝縮（3 段目 / issue #411）
// ============================================
//
// 凝縮ランが使う 3 道具。ユニット（記憶の 2 段目）を俯瞰して抽出した「原則」を
// `node_type='meta'` として記録・更新・取消する。**根拠のユニットへ必ずリンクさせる**
// （具体を失った凝縮は平均化 / #411 原則3）ので、record / update は sources に自分の宣言
// ユニットの short_id を要る。全て **TRUSTED_ONLY**（宣言道具と同じ論拠: caller=Agent の
// 会話流入で人格の核をスパムさせない）。凝縮ラン（caller=Owner）からのみ使う。

/// ユニットを俯瞰して抽出した「原則」を 1 件記録する（`node_type='meta'`）。
pub struct RecordMemoryCoreAction;

#[async_trait]
impl Action for RecordMemoryCoreAction {
    fn name(&self) -> &str {
        "record_memory_core"
    }

    fn description(&self) -> &str {
        "自分のユニット（宣言した記憶）を俯瞰して見えた『大事なこと』を 1 件、人格の核として刻む。axis（どんな視点か = 軸ラベル）と body（本文）必須。sources には**その原則の根拠になった自分の宣言ユニットの short_id**（例 u42）を最低 1 つ挙げる（根拠の無い凝縮は平均化なので受け付けない）。retract_memory_core で取り消せる。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["axis", "body", "sources"],
            "properties": {
                "axis": { "type": "string", "description": "この原則をどんな視点で見たか（軸ラベル）。例に縛られず自分の言葉でよい" },
                "body": { "type": "string", "description": "原則の本文（何が大事だと分かったか）" },
                "sources": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "根拠になった自分の宣言ユニットの short_id（例 u42）。最低 1 つ。複数可"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let axis = match args["axis"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("axis is required"),
        };
        let body = match args["body"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("body is required"),
        };
        let sources = parse_source_refs(&args["sources"]);
        if sources.is_empty() {
            return ActionResult::error(
                "sources に根拠となる宣言ユニットの short_id を最低 1 つ指定してください",
            );
        }

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        let now = chrono::Utc::now().to_rfc3339();
        match opencrab_db::queries::record_memory_core(
            &conn,
            &ctx.agent_id,
            &axis,
            &body,
            &sources,
            &now,
        ) {
            Ok(r) => ActionResult::success(json!({
                "core_id": r.node.id,
                "short_id": r.node.short_id,
                "axis": r.node.title,
                "sources": r.sources,
                "unresolved_sources": r.unresolved,
                "start_log_id": r.node.start_log_id,
                "end_log_id": r.node.end_log_id,
            })),
            Err(e) => ActionResult::error(&format!("凝縮の記録に失敗しました: {e}")),
        }
    }
}

/// 既存の凝縮を更新する（新規追加より更新を優先する / #411 原則4）。
pub struct UpdateMemoryCoreAction;

#[async_trait]
impl Action for UpdateMemoryCoreAction {
    fn name(&self) -> &str {
        "update_memory_core"
    }

    fn description(&self) -> &str {
        "既存の凝縮（人格の核）を書き直す。同じ趣旨のものを新しく足すより、既にあるものを更新する方を優先する。axis と body 必須。sources を渡すと根拠ユニットを差し替え、省くと既存の根拠を維持する。凝縮（node_type='meta'）以外は更新できない。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["core_id", "axis", "body"],
            "properties": {
                "core_id": { "type": "string", "description": "更新する凝縮の short_id（例 m3）またはフル node_id" },
                "axis": { "type": "string", "description": "軸ラベル（視点）" },
                "body": { "type": "string", "description": "原則の本文" },
                "sources": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "根拠ユニットの short_id を差し替える（省くと既存の根拠を維持）。渡すなら最低 1 つ"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let core_id = match args["core_id"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("core_id is required"),
        };
        let axis = match args["axis"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("axis is required"),
        };
        let body = match args["body"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("body is required"),
        };
        // sources を明示的に渡したときだけ差し替える（キー未指定 = None = 根拠維持）。
        let sources: Option<Vec<String>> = args
            .get("sources")
            .filter(|v| !v.is_null())
            .map(parse_source_refs);

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        match opencrab_db::queries::update_memory_core(
            &conn,
            &ctx.agent_id,
            &core_id,
            &axis,
            &body,
            sources.as_deref(),
        ) {
            Ok(r) => ActionResult::success(json!({
                "updated": true,
                "core_id": r.node.id,
                "short_id": r.node.short_id,
                "axis": r.node.title,
                "sources": r.sources,
                "unresolved_sources": r.unresolved,
            })),
            Err(e) => ActionResult::error(&format!("凝縮の更新に失敗しました: {e}")),
        }
    }
}

/// 凝縮を取り消す（凝縮ノード + FTS のみ削除。生ログにも元ユニットにも触らない）。
pub struct RetractMemoryCoreAction;

#[async_trait]
impl Action for RetractMemoryCoreAction {
    fn name(&self) -> &str {
        "retract_memory_core"
    }

    fn description(&self) -> &str {
        "record_memory_core で刻んだ凝縮を取り消す。凝縮ノードと FTS 行だけを消す。生ログにも元ユニットにも触らない。凝縮（node_type='meta'）以外は消せない。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["core_id"],
            "properties": {
                "core_id": {
                    "type": "string",
                    "description": "取り消す凝縮の short_id またはフル node_id"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let core_id = match args["core_id"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("core_id is required"),
        };
        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        match opencrab_db::queries::retract_memory_core(&conn, &ctx.agent_id, &core_id) {
            Ok(full_id) => ActionResult::success(json!({
                "retracted": true,
                "core_id": full_id,
            })),
            Err(e) => ActionResult::error(&format!("凝縮の取り消しに失敗しました: {e}")),
        }
    }
}

/// `sources` 引数（文字列配列）を short_id/id 参照の並びへ正規化する。
fn parse_source_refs(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

