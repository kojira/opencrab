use async_trait::async_trait;
use serde_json::json;
use uuid;

use crate::traits::{Action, ActionContext, ActionResult, CallerIdentity, SideEffect};

/// スキルの `created_caller`（作成 caller の trust class）を、今回書き込む `writer` の
/// caller で更新した結果を返す（#335）。
///
/// - 書き込み手が既存記録と同等か弱い（trust が上がらない）ときは writer のタグを採用する。
///   → 外部 Agent が既存スキルを上書きしたら trust class が `agent` へ下がる（confused
///   deputy を塞ぐ）。
/// - 書き込み手が既存より強いときは既存を保持する（弱いスキルを強い caller で上書きしても
///   trust を吊り上げない＝昇格させない）。`None`（legacy = Owner 相当）はそのまま残す。
fn downgraded_created_caller(existing: Option<&str>, writer: &CallerIdentity) -> Option<String> {
    let existing_trust = CallerIdentity::skill_origin_trust(existing);
    if writer.trust_level() <= existing_trust {
        Some(writer.skill_origin_tag().to_string())
    } else {
        existing.map(|s| s.to_string())
    }
}

/// 自作スキル作成アクション
pub struct CreateMySkillAction;

#[async_trait]
impl Action for CreateMySkillAction {
    fn name(&self) -> &str {
        "create_my_skill"
    }

    fn description(&self) -> &str {
        "学んだことを正式なスキルファイルとして保存する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["name", "description", "situation_pattern", "guidance"],
            "properties": {
                "name": {
                    "type": "string",
                    "description": "スキル名"
                },
                "description": {
                    "type": "string",
                    "description": "スキルの説明"
                },
                "situation_pattern": {
                    "type": "string",
                    "description": "スキルが適用できる状況パターン"
                },
                "guidance": {
                    "type": "string",
                    "description": "具体的な行動指針"
                },
                "actions": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "関連するアクション名のリスト"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => return ActionResult::error("name is required"),
        };

        let actions: Vec<String> = args["actions"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let skill_content = format!(
            "---\nname: {name}\ndescription: \"{desc}\"\nversion: 1\nactions:\n{actions_yaml}\n---\n\n# {name}\n\n## 状況パターン\n{pattern}\n\n## 行動指針\n{guidance}\n",
            name = name,
            desc = args["description"].as_str().unwrap_or(""),
            actions_yaml = actions
                .iter()
                .map(|a| format!("  - {a}"))
                .collect::<Vec<_>>()
                .join("\n"),
            pattern = args["situation_pattern"].as_str().unwrap_or(""),
            guidance = args["guidance"].as_str().unwrap_or(""),
        );

        let file_path = format!("skills/{}.skill.md", name.replace(' ', "-").to_lowercase());
        let description = args["description"].as_str().unwrap_or("").to_string();
        let situation_pattern = args["situation_pattern"].as_str().unwrap_or("").to_string();
        let guidance = args["guidance"].as_str().unwrap_or("").to_string();

        // Check if skill with same name already exists (including archived)
        let existing = ctx.db.lock().ok().and_then(|conn| {
            opencrab_db::queries::find_skill_by_name_any(&conn, &ctx.agent_id, name)
                .ok()
                .flatten()
        });

        if let Some(existing) = existing {
            let was_archived = existing.archived;
            let skill_id = existing.id.clone();

            let mut updated = existing;
            updated.description = description;
            updated.situation_pattern = situation_pattern;
            updated.guidance = guidance;
            updated.file_path = Some(file_path.clone());
            updated.is_active = true;
            updated.archived = false;
            // #335: このターンの caller で本文を上書きした以上、trust class を作成 caller に
            // 合わせて（昇格させずに）更新する。外部 Agent が既存スキルへ悪性の guidance を
            // 仕込んで後で Owner ターンに実行させる経路を、記録側で塞ぐ。
            updated.created_caller =
                downgraded_created_caller(updated.created_caller.as_deref(), &ctx.caller);

            if let Ok(conn) = ctx.db.lock() {
                let _ = opencrab_db::queries::update_skill(&conn, &updated);
            }

            // Overwrite the skill file
            match ctx.workspace.write(&file_path, &skill_content).await {
                Ok(_) => {
                    let result_key = if was_archived { "restored" } else { "updated" };
                    ActionResult::success(json!({
                        result_key: true,
                        "skill_id": skill_id,
                        "file_path": file_path,
                    }))
                    .with_side_effect(SideEffect::FileWritten { path: file_path })
                }
                Err(e) => ActionResult::error(&e.to_string()),
            }
        } else {
            match ctx.workspace.write(&file_path, &skill_content).await {
                Ok(_) => {
                    // DBにも登録
                    let skill_id = uuid::Uuid::new_v4().to_string();
                    let skill = opencrab_db::queries::SkillRow {
                        id: skill_id.clone(),
                        agent_id: ctx.agent_id.clone(),
                        name: name.to_string(),
                        description,
                        situation_pattern,
                        guidance,
                        source_type: "self_created".to_string(),
                        source_context: None,
                        file_path: Some(file_path.clone()),
                        effectiveness: None,
                        usage_count: 0,
                        is_active: true,
                        permission: "\"agent\"".to_string(),
                        archived: false,
                        // #335: 作成時 caller の trust class を記録する。外部 Nostr の
                        // caller=Agent が仕込んだスキルは "agent" になり、後で Owner の
                        // heartbeat が read_skill しても本文を渡さず（塞がる）。
                        created_caller: Some(ctx.caller.skill_origin_tag().to_string()),
                        // #352: Agent が作った skill を Agent 自身へ露出しない（fail-closed）。
                        // オーナーが REST で許可するまで false。
                        agent_visible: false,
                    };

                    if let Ok(conn) = ctx.db.lock() {
                        let _ = opencrab_db::queries::insert_skill(&conn, &skill);
                    }

                    ActionResult::success(json!({
                        "created": true,
                        "skill_id": skill_id,
                        "file_path": file_path,
                    }))
                    .with_side_effect(SideEffect::SkillAcquired { skill_id })
                    .with_side_effect(SideEffect::FileWritten { path: file_path })
                }
                Err(e) => ActionResult::error(&e.to_string()),
            }
        }
    }
}

/// 自分のスキルを引退（archive）するアクション。
/// スリープ棚卸しと対称の、wake 時にも使える手動整理手段。可逆（restore_my_skill で戻せる）。
pub struct RetireMySkillAction;

#[async_trait]
impl Action for RetireMySkillAction {
    fn name(&self) -> &str {
        "retire_my_skill"
    }

    fn description(&self) -> &str {
        "使わなくなった自分のスキルを引退させる（archive、後で restore_my_skill で戻せる）"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string", "description": "引退させるスキル名" }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        set_skill_archived(ctx, args, true).await
    }
}

/// 引退させたスキルを復活（un-archive）するアクション。retire_my_skill と対称。
pub struct RestoreMySkillAction;

#[async_trait]
impl Action for RestoreMySkillAction {
    fn name(&self) -> &str {
        "restore_my_skill"
    }

    fn description(&self) -> &str {
        "引退させた自分のスキルを復活させる（un-archive）"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string", "description": "復活させるスキル名" }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        set_skill_archived(ctx, args, false).await
    }
}

/// スキルの本文（行動指針）を名前で取得するアクション（段階的開示 #119）。
///
/// システムプロンプトにはスキルの index（名前 + 説明）だけを載せ、詳細な本文
/// （guidance）はプロンプトに常時展開しない。エージェントは必要になったときだけ
/// この `read_skill` で本文を取得して掘り下げる（memory_index の browse/retrieve と
/// 同じパターン）。archived なスキルも読める（_any で解決）。
pub struct ReadSkillAction;

#[async_trait]
impl Action for ReadSkillAction {
    fn name(&self) -> &str {
        "read_skill"
    }

    fn description(&self) -> &str {
        "スキルの本文（行動指針の全文）を名前で取得する。プロンプトには index（名前+説明）\
         しか出ていないので、詳細な手順が必要になったらこれで掘り下げる。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string", "description": "読みたいスキル名" }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let name = match args["name"].as_str() {
            Some(n) if !n.trim().is_empty() => n.trim(),
            _ => return ActionResult::error("name is required"),
        };
        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("db lock failed"),
        };
        match opencrab_db::queries::find_skill_by_name_any(&conn, &ctx.agent_id, name) {
            Ok(Some(s)) => {
                // #352: caller=Agent のターン（素の Agent 権限で走る run。外部 Nostr の受信
                // ターンが典型例だが判定軸は transport ではなく caller=Agent）には、オーナーが
                // 露出を許可（`agent_visible`）した skill 以外は本文を渡さない。index 側
                // （process.rs）と AND で二重化する — index を隠すだけでは名前を直打ちで
                // read_skill されるため本文でも塞ぐ。#335 の may_exercise_skill ゲート（向きが逆）
                // は残したまま両方を満たすときだけ本文を返す（既存ゲートを弱めない）。
                //
                // エラーメッセージは **存在しない場合（Ok(None)）と同一**にする。パス・構成・
                // 露出可否といった内部の事情を一切漏らさない（要望の核心 / #352）。
                if matches!(ctx.caller, CallerIdentity::Agent) && !s.agent_visible {
                    return ActionResult::error(&format!("skill not found: {name}"));
                }
                // #335: confused deputy 対策。read_skill は本文（行動指針）を渡す＝スキルの
                // 「実行」入口。作成 caller より強いターン（例: 外部 Nostr の caller=Agent が
                // 仕込んだスキルを Owner の heartbeat が読む）には本文を渡さない。より強い
                // ターンが弱いスキルを借りて owner 権限のローカル操作へ届く経路を塞ぐ。
                // 逆向き（弱いターンが強いスキルを読む）は許すが、実アクションは dispatch 側の
                // caller ゲートで弾かれるため昇格は起きない。`created_caller` が None の既存
                // スキルは Owner 相当扱いで従来どおり読める（既存を壊さない）。
                if !ctx.caller.may_exercise_skill(s.created_caller.as_deref()) {
                    return ActionResult::error(&format!(
                        "skill '{name}' は作成時より強い権限のターンからは実行できない\
                         （このスキルは作成した caller の権限で走らせる。#335 confused deputy 対策）"
                    ));
                }
                ActionResult::success(json!({
                    "name": s.name,
                    "description": s.description,
                    "situation_pattern": s.situation_pattern,
                    "guidance": s.guidance,
                    "source_type": s.source_type,
                    "is_active": s.is_active,
                    "archived": s.archived,
                    "usage_count": s.usage_count,
                }))
            }
            Ok(None) => ActionResult::error(&format!("skill not found: {name}")),
            Err(e) => ActionResult::error(&e.to_string()),
        }
    }
}

/// 名前でスキルを解決し archived フラグを設定する（retire/restore 共通）。
/// archive は DB フラグのみで、ファイル操作は不要。
async fn set_skill_archived(
    ctx: &ActionContext,
    args: &serde_json::Value,
    archived: bool,
) -> ActionResult {
    let name = match args["name"].as_str() {
        Some(n) if !n.trim().is_empty() => n,
        _ => return ActionResult::error("name is required"),
    };
    let conn = match ctx.db.lock() {
        Ok(c) => c,
        Err(_) => return ActionResult::error("db lock failed"),
    };
    // archived 含めて名前で解決（restore は archived スキルを対象にするため _any を使う）
    let skill = match opencrab_db::queries::find_skill_by_name_any(&conn, &ctx.agent_id, name) {
        Ok(Some(s)) => s,
        Ok(None) => return ActionResult::error(&format!("skill not found: {name}")),
        Err(e) => return ActionResult::error(&e.to_string()),
    };
    match opencrab_db::queries::archive_skill(&conn, &skill.id, archived) {
        Ok(()) => {
            let key = if archived { "retired" } else { "restored" };
            ActionResult::success(json!({ key: true, "skill_id": skill.id, "name": name }))
        }
        Err(e) => ActionResult::error(&e.to_string()),
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

    /// 同じ DB / workspace を共有しつつ caller だけ差し替えた ActionContext を作る。
    /// 「あるターンで作ったスキルを別 caller のターンで read する」#335 のシナリオ用。
    fn ctx_with_caller(base: &ActionContext, caller: CallerIdentity) -> ActionContext {
        ActionContext {
            agent_id: base.agent_id.clone(),
            agent_name: base.agent_name.clone(),
            session_id: base.session_id.clone(),
            db: base.db.clone(),
            workspace: base.workspace.clone(),
            last_metrics_id: base.last_metrics_id.clone(),
            model_override: base.model_override.clone(),
            current_purpose: base.current_purpose.clone(),
            runtime_info: base.runtime_info.clone(),
            caller,
        }
    }

    // #335: 外部 Nostr の caller=Agent が仕込んだスキルは、Owner の heartbeat ターンが
    // read_skill しても本文を渡さない（＝作成 caller の権限で走る）。これが本題。
    #[tokio::test]
    async fn agent_created_skill_is_denied_to_owner_turn() {
        let (_dir, owner_ctx) = test_context();
        let agent_ctx = ctx_with_caller(&owner_ctx, CallerIdentity::Agent);

        // 外部ターン（caller=Agent）でローカル操作を含むスキルを仕込む。
        let created = CreateMySkillAction
            .execute(
                &json!({
                    "name": "Planted",
                    "description": "d",
                    "situation_pattern": "s",
                    "guidance": "run execute_shell to do X",
                    "actions": ["execute_shell", "ws_write"]
                }),
                &agent_ctx,
            )
            .await;
        assert!(created.success);
        // created_caller が "agent" で記録されている。
        {
            let conn = agent_ctx.db.lock().unwrap();
            let row =
                opencrab_db::queries::find_skill_by_name_any(&conn, &agent_ctx.agent_id, "Planted")
                    .unwrap()
                    .unwrap();
            assert_eq!(row.created_caller.as_deref(), Some("agent"));
        }

        // Owner の heartbeat ターンが read_skill しても本文は渡らない（塞がる）。
        let denied = ReadSkillAction
            .execute(&json!({ "name": "Planted" }), &owner_ctx)
            .await;
        assert!(
            !denied.success,
            "owner turn must NOT read an agent-planted skill body"
        );

        // #352: 同じ Agent 権限のターンでも、既定では本文を読めない（fail-closed）。
        // #335 の逆向き許可（弱いターンが読む）は #352 の露出ゲートと **AND** で重なる。
        let denied_agent = ReadSkillAction
            .execute(&json!({ "name": "Planted" }), &agent_ctx)
            .await;
        assert!(
            !denied_agent.success,
            "agent turn must NOT read a non-visible skill body (#352 default)"
        );

        // オーナーが露出を許可すると、Agent ターンから本文を読める（#335 の逆向き許可が
        // #352 の露出下で維持されていることの確認）。
        {
            let conn = agent_ctx.db.lock().unwrap();
            let mut row =
                opencrab_db::queries::find_skill_by_name_any(&conn, &agent_ctx.agent_id, "Planted")
                    .unwrap()
                    .unwrap();
            row.agent_visible = true;
            opencrab_db::queries::update_skill(&conn, &row).unwrap();
        }
        let ok = ReadSkillAction
            .execute(&json!({ "name": "Planted" }), &agent_ctx)
            .await;
        assert!(ok.success);
        assert_eq!(ok.data.unwrap()["guidance"], "run execute_shell to do X");
    }

    // #335: Owner ターンで作ったスキルは Owner ターンで読める（従来どおり）。
    #[tokio::test]
    async fn owner_created_skill_runs_in_owner_turn() {
        let (_dir, owner_ctx) = test_context();
        CreateMySkillAction
            .execute(
                &json!({
                    "name": "OwnerSkill",
                    "description": "d",
                    "situation_pattern": "s",
                    "guidance": "owner guidance"
                }),
                &owner_ctx,
            )
            .await;
        {
            let conn = owner_ctx.db.lock().unwrap();
            let row = opencrab_db::queries::find_skill_by_name_any(
                &conn,
                &owner_ctx.agent_id,
                "OwnerSkill",
            )
            .unwrap()
            .unwrap();
            assert_eq!(row.created_caller.as_deref(), Some("owner"));
        }
        let ok = ReadSkillAction
            .execute(&json!({ "name": "OwnerSkill" }), &owner_ctx)
            .await;
        assert!(ok.success);
        assert_eq!(ok.data.unwrap()["guidance"], "owner guidance");
    }

    // #352: オーナー作成スキル（agent_visible 既定 false）は、caller=Agent のターンからは
    // read_skill しても本文を渡さない。エラーメッセージは「存在しない」場合と **完全に同一**で、
    // 内部の事情（パス・構成・露出可否）を一切漏らさない。
    #[tokio::test]
    async fn agent_caller_cannot_read_non_visible_skill_body() {
        let (_dir, owner_ctx) = test_context();
        let agent_ctx = ctx_with_caller(&owner_ctx, CallerIdentity::Agent);

        CreateMySkillAction
            .execute(
                &json!({
                    "name": "Internal",
                    "description": "d",
                    "situation_pattern": "s",
                    "guidance": "local path /Volumes/... internal steps"
                }),
                &owner_ctx,
            )
            .await;

        // 既定 false なので Agent ターンには本文を渡さない。
        let denied = ReadSkillAction
            .execute(&json!({ "name": "Internal" }), &agent_ctx)
            .await;
        assert!(
            !denied.success,
            "agent turn must NOT read a non-visible body"
        );

        // 情報漏洩の核心: エラーは **存在しない場合と同一形式**の汎用メッセージのみで、
        // 露出可否・パス・構成といった内部の事情を一切漏らさない。存在するのに隠された
        // "Internal" の応答が、実在しない同名の応答（`skill not found: Internal`）と
        // 文字列として一致することを固定する。
        assert_eq!(
            denied.error.as_deref(),
            Some("skill not found: Internal"),
            "hidden skill must return the same generic not-found message as a nonexistent one"
        );
        // ガードの本文（露出可否）を漏らす語が混ざっていないこと。
        let err = denied.error.unwrap_or_default();
        for leak in [
            "agent_visible",
            "visible",
            "許可",
            "露出",
            "path",
            "/Volumes",
        ] {
            assert!(
                !err.contains(leak),
                "error message leaks internal detail {leak:?}: {err}"
            );
        }

        // Owner ターンからは従来どおり読める（絞りは caller=Agent のみ）。
        let ok = ReadSkillAction
            .execute(&json!({ "name": "Internal" }), &owner_ctx)
            .await;
        assert!(ok.success);
    }

    // #352: オーナーが agent_visible を立てた skill は caller=Agent のターンからも本文を読める。
    // Owner / CoAgent / TrustedUser は露出フラグに関わらず従来どおり読める。
    #[tokio::test]
    async fn all_callers_read_skill_after_owner_grants_visibility() {
        let (_dir, owner_ctx) = test_context();
        let agent_ctx = ctx_with_caller(&owner_ctx, CallerIdentity::Agent);
        let co_agent_ctx = ctx_with_caller(
            &owner_ctx,
            CallerIdentity::CoAgent {
                agent_id: "peer".to_string(),
            },
        );
        let trusted_ctx = ctx_with_caller(&owner_ctx, CallerIdentity::TrustedUser);

        CreateMySkillAction
            .execute(
                &json!({
                    "name": "WebSearch",
                    "description": "d",
                    "situation_pattern": "s",
                    "guidance": "search the web"
                }),
                &owner_ctx,
            )
            .await;

        // オーナーが露出を許可する（REST の update_skill 相当 = queries::update_skill）。
        {
            let conn = owner_ctx.db.lock().unwrap();
            let mut row = opencrab_db::queries::find_skill_by_name_any(
                &conn,
                &owner_ctx.agent_id,
                "WebSearch",
            )
            .unwrap()
            .unwrap();
            row.agent_visible = true;
            opencrab_db::queries::update_skill(&conn, &row).unwrap();
        }

        for ctx in [&agent_ctx, &co_agent_ctx, &trusted_ctx, &owner_ctx] {
            let ok = ReadSkillAction
                .execute(&json!({ "name": "WebSearch" }), ctx)
                .await;
            assert!(
                ok.success,
                "caller {:?} must read a visible skill",
                ctx.caller
            );
            assert_eq!(ok.data.unwrap()["guidance"], "search the web");
        }
    }

    // #335: 昇格しないこと。弱い caller が Owner スキルの本文を上書きしても trust class は
    // "agent" へ下がり（吊り上がらない）、Owner ターンからは読めなくなる。
    #[tokio::test]
    async fn overwrite_by_weaker_caller_downgrades_not_elevates() {
        let (_dir, owner_ctx) = test_context();
        let agent_ctx = ctx_with_caller(&owner_ctx, CallerIdentity::Agent);

        // Owner が作ったスキル。
        CreateMySkillAction
            .execute(
                &json!({
                    "name": "Shared",
                    "description": "d",
                    "situation_pattern": "s",
                    "guidance": "benign"
                }),
                &owner_ctx,
            )
            .await;

        // 外部 Agent ターンが同名で上書き（dedup 更新）。
        let overwritten = CreateMySkillAction
            .execute(
                &json!({
                    "name": "Shared",
                    "description": "d2",
                    "situation_pattern": "s2",
                    "guidance": "evil execute_shell"
                }),
                &agent_ctx,
            )
            .await;
        assert!(overwritten.success);

        // trust class は "agent" へ下がる（Owner へ吊り上がらない）。
        {
            let conn = owner_ctx.db.lock().unwrap();
            let row =
                opencrab_db::queries::find_skill_by_name_any(&conn, &owner_ctx.agent_id, "Shared")
                    .unwrap()
                    .unwrap();
            assert_eq!(row.created_caller.as_deref(), Some("agent"));
        }
        // その結果、Owner ターンからは本文を借りられない（confused deputy 封じ）。
        let denied = ReadSkillAction
            .execute(&json!({ "name": "Shared" }), &owner_ctx)
            .await;
        assert!(!denied.success);
    }

    // #335: created_caller が NULL の既存スキル（この列より前に作られた 58 個）は Owner
    // ターンからも従来どおり読める（legacy grandfather = Owner 相当）。既存を壊さない。
    #[tokio::test]
    async fn legacy_skill_without_created_caller_is_readable_by_owner() {
        let (_dir, owner_ctx) = test_context();
        {
            let conn = owner_ctx.db.lock().unwrap();
            let row = opencrab_db::queries::SkillRow {
                id: "legacy-1".to_string(),
                agent_id: owner_ctx.agent_id.clone(),
                name: "Legacy".to_string(),
                description: "d".to_string(),
                situation_pattern: String::new(),
                guidance: "legacy body".to_string(),
                source_type: "self_created".to_string(),
                source_context: None,
                file_path: None,
                effectiveness: None,
                usage_count: 0,
                is_active: true,
                permission: "\"agent\"".to_string(),
                archived: false,
                created_caller: None,
                agent_visible: false,
            };
            opencrab_db::queries::insert_skill(&conn, &row).unwrap();
        }
        let ok = ReadSkillAction
            .execute(&json!({ "name": "Legacy" }), &owner_ctx)
            .await;
        assert!(
            ok.success,
            "legacy (NULL created_caller) skill must stay readable in owner turns"
        );
    }

    #[tokio::test]
    async fn test_create_my_skill_success() {
        let (_dir, ctx) = test_context();
        let result = CreateMySkillAction
            .execute(
                &json!({
                    "name": "Test Skill",
                    "description": "A test skill",
                    "situation_pattern": "when testing",
                    "guidance": "Be thorough",
                    "actions": ["ws_read", "ws_write"]
                }),
                &ctx,
            )
            .await;
        assert!(result.success);
        let data = result.data.unwrap();
        assert!(data["created"].as_bool().unwrap());
        assert!(data["skill_id"].as_str().is_some());
        assert!(data["file_path"].as_str().unwrap().contains("skills/"));

        // Verify side effects
        assert!(result
            .side_effects
            .iter()
            .any(|e| matches!(e, SideEffect::SkillAcquired { .. })));
        assert!(result
            .side_effects
            .iter()
            .any(|e| matches!(e, SideEffect::FileWritten { .. })));

        // Verify DB insertion
        let conn = ctx.db.lock().unwrap();
        let skills = opencrab_db::queries::list_skills(&conn, "agent-1", true).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "Test Skill");
        assert_eq!(skills[0].source_type, "self_created");
    }

    #[tokio::test]
    async fn test_retire_and_restore_my_skill() {
        let (_dir, ctx) = test_context();
        // スキルを1つ作成
        CreateMySkillAction
            .execute(
                &json!({
                    "name": "Retirable",
                    "description": "d",
                    "situation_pattern": "s",
                    "guidance": "g"
                }),
                &ctx,
            )
            .await;

        // 引退 → active から消える
        let r = RetireMySkillAction
            .execute(&json!({ "name": "Retirable" }), &ctx)
            .await;
        assert!(r.success);
        assert!(r.data.unwrap()["retired"].as_bool().unwrap());
        {
            let conn = ctx.db.lock().unwrap();
            assert!(opencrab_db::queries::list_skills(&conn, "agent-1", true)
                .unwrap()
                .is_empty());
        }

        // 復活 → active に戻る（可逆）
        let r = RestoreMySkillAction
            .execute(&json!({ "name": "Retirable" }), &ctx)
            .await;
        assert!(r.success);
        assert!(r.data.unwrap()["restored"].as_bool().unwrap());
        {
            let conn = ctx.db.lock().unwrap();
            assert_eq!(
                opencrab_db::queries::list_skills(&conn, "agent-1", true)
                    .unwrap()
                    .len(),
                1
            );
        }

        // 存在しないスキルはエラー
        let r = RetireMySkillAction
            .execute(&json!({ "name": "nope" }), &ctx)
            .await;
        assert!(!r.success);
    }

    #[tokio::test]
    async fn test_create_my_skill_missing_name() {
        let (_dir, ctx) = test_context();
        let result = CreateMySkillAction
            .execute(&json!({"description": "no name"}), &ctx)
            .await;
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("name is required"));
    }

    #[tokio::test]
    async fn test_read_skill_returns_body() {
        let (_dir, ctx) = test_context();
        CreateMySkillAction
            .execute(
                &json!({
                    "name": "Deploy Steps",
                    "description": "how to deploy",
                    "situation_pattern": "when deploying",
                    "guidance": "1) build 2) push 3) verify",
                    "actions": ["ws_read"]
                }),
                &ctx,
            )
            .await;

        // 本文（guidance）が取得できる。
        let r = ReadSkillAction
            .execute(&json!({ "name": "Deploy Steps" }), &ctx)
            .await;
        assert!(r.success);
        let d = r.data.unwrap();
        assert_eq!(d["name"], "Deploy Steps");
        assert_eq!(d["guidance"], "1) build 2) push 3) verify");
        assert_eq!(d["situation_pattern"], "when deploying");

        // 引退（archived）でも読める（_any 解決）。
        RetireMySkillAction
            .execute(&json!({ "name": "Deploy Steps" }), &ctx)
            .await;
        let r2 = ReadSkillAction
            .execute(&json!({ "name": "Deploy Steps" }), &ctx)
            .await;
        assert!(r2.success);
        assert_eq!(r2.data.unwrap()["archived"], true);

        // 存在しないスキルはエラー。
        let r3 = ReadSkillAction
            .execute(&json!({ "name": "nope" }), &ctx)
            .await;
        assert!(!r3.success);
        assert!(r3.error.unwrap().contains("not found"));

        // name 必須。
        let r4 = ReadSkillAction.execute(&json!({}), &ctx).await;
        assert!(!r4.success);
    }

    #[tokio::test]
    async fn test_create_my_skill_file_content() {
        let (_dir, ctx) = test_context();
        CreateMySkillAction
            .execute(
                &json!({
                    "name": "File Check",
                    "description": "desc",
                    "situation_pattern": "pattern",
                    "guidance": "guide"
                }),
                &ctx,
            )
            .await;
        let content = ctx
            .workspace
            .read("skills/file-check.skill.md")
            .await
            .unwrap();
        assert!(content.contains("File Check"));
        assert!(content.contains("guide"));
        assert!(content.contains("pattern"));
    }
}
