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
mod tests;
