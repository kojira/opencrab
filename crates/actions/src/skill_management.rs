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
        "学んだことを正式なスキルファイルとして保存する（同名があれば更新）。オーナーが「誰でも使える\
         ように」と頼んだときだけ anyone_can_use を true にする"
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
                    "description": "このスキルで使うツール名のリスト。anyone_can_use のスキルでは、\
                                    このスキルを read_skill で開いた会話でこれらが誰とでも使える"
                },
                "anyone_can_use": {
                    "type": "boolean",
                    "description": "オーナーが「このスキルは誰でも使えるように」と頼んだときだけ true に\
                                    する。true のスキルを read_skill で開いた会話では、相手が誰でも \
                                    actions のツールを使える。オーナーのターンでしか設定できない"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => return ActionResult::error("name is required"),
        };

        // `actions` は DB の `tools` 列にも入れる（D-1143）。許可として効くのは「誰でも使える」
        // 指定（`anyone_can_use`）のスキルだけで、その指定はオーナー等価のターンだけが立てられる。
        let actions_arg: Option<Vec<String>> = args["actions"].as_array().map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });
        let actions: Vec<String> = actions_arg.clone().unwrap_or_default();
        let anyone_can_use = args["anyone_can_use"].as_bool();
        let writer_is_owner = ctx.caller.is_owner_equivalent();

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
            // D-1143: 指定とツールの変更は 1 つの規則（`SkillRow::set_access`）で判定し、
            // 断るときは何も書かずに返す。
            if let Err(reason) = updated.set_access(anyone_can_use, actions_arg, writer_is_owner) {
                return ActionResult::error(reason);
            }
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
            // D-1143: オーナー等価でない書き手が書き換えたら「誰でも使える」指定を外す。
            let designation_cleared = updated.record_rewrite(writer_is_owner);

            if let Ok(conn) = ctx.db.lock() {
                let _ = opencrab_db::queries::update_skill(&conn, &updated);
            }

            // Overwrite the skill file
            match ctx.workspace.write(&file_path, &skill_content).await {
                Ok(_) => {
                    let result_key = if was_archived { "restored" } else { "updated" };
                    let mut data = json!({
                        result_key: true,
                        "skill_id": skill_id,
                        "file_path": file_path,
                        "anyone_can_use": updated.agent_visible,
                        "tools": updated.tools,
                    });
                    if designation_cleared {
                        data["owner_designation_cleared"] =
                            json!(opencrab_db::queries::OWNER_DESIGNATION_CLEARED_NOTE);
                    }
                    ActionResult::success(data)
                        .with_side_effect(SideEffect::FileWritten { path: file_path })
                }
                Err(e) => ActionResult::error(&e.to_string()),
            }
        } else {
            let skill_id = uuid::Uuid::new_v4().to_string();
            let mut skill = opencrab_db::queries::SkillRow {
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
                // #352 / D-1143: 既定は指定なし。オーナーのターンで anyone_can_use を
                // 頼まれたときだけ下の set_access で立てる。
                agent_visible: false,
                tools: actions,
            };
            if let Err(reason) = skill.set_access(anyone_can_use, None, writer_is_owner) {
                return ActionResult::error(reason);
            }
            match ctx.workspace.write(&file_path, &skill_content).await {
                Ok(_) => {
                    if let Ok(conn) = ctx.db.lock() {
                        let _ = opencrab_db::queries::insert_skill(&conn, &skill);
                    }

                    ActionResult::success(json!({
                        "created": true,
                        "skill_id": skill_id,
                        "file_path": file_path,
                        "anyone_can_use": skill.agent_visible,
                        "tools": skill.tools,
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

/// オーナー向けスキルの index を引く予約名（D-1058）。system の skill 一覧には、誰にでも
/// 見せてよい（`SkillRow::visible_to_any_caller`）のでない skill の名前を出さず、この名前の
/// 1 行だけを置く。オーナー等価のターンが `read_skill` でこの名前を引くと、その skill 群の名前と説明の一覧が返る。
pub const OWNER_SKILLS_INDEX_NAME: &str = "owner-skills";

/// オーナー等価でない caller が、露出許可の無い skill（とその index）を引いたときの拒否文。
/// ツールの owner_only と同じく「権限が要る」と返す（D-1058）。
fn owner_required(name: &str) -> ActionResult {
    ActionResult::error(&format!("skill '{name}' requires owner"))
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
        // D-1058: 予約名はオーナー向け skill の index。実在 skill の解決より先に扱う。
        if name == OWNER_SKILLS_INDEX_NAME {
            if !ctx.caller.is_owner_equivalent() {
                return owner_required(name);
            }
            let mut skills = match opencrab_db::queries::list_skills(&conn, &ctx.agent_id, true) {
                Ok(skills) => skills,
                Err(e) => return ActionResult::error(&e.to_string()),
            };
            skills.retain(|s| !s.visible_to_any_caller());
            skills.sort_by(|a, b| a.name.cmp(&b.name));
            let list: Vec<serde_json::Value> = skills
                .iter()
                .map(|s| json!({ "name": s.name, "description": s.description }))
                .collect();
            return ActionResult::success(json!({ "name": name, "skills": list }));
        }
        match opencrab_db::queries::find_skill_by_name_any(&conn, &ctx.agent_id, name) {
            Ok(Some(s)) => {
                // D-1058（#352 の露出制限を権限ゲートへ置き換え）: 誰にでも見せてよい skill
                // （`visible_to_any_caller`: オーナー指定か、学んだ skill。D-1144）でない skill の本文は、オーナー等価（Owner / CoAgent）の
                // ターンだけに渡す。それ以外はツールの owner_only と同じく「owner が要る」と
                // 返す。system の一覧はこの skill の名前を出さないので、名前を知る手段は
                // オーナー等価ターンの `owner-skills` だけ。#335 の may_exercise_skill ゲート
                // （向きが逆）はこの後に残し、両方を満たすときだけ本文を返す。
                if !s.visible_to_any_caller() && !ctx.caller.is_owner_equivalent() {
                    return owner_required(name);
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
                // D-1146: 本文を渡す＝スキルを使った。ここ（成功時）だけで数える。拒否・not found・
                // `owner-skills` の一覧は数えない。記録の失敗で本文の受け渡しは止めない。
                if let Err(e) = opencrab_db::queries::record_skill_use(
                    &conn,
                    &ctx.agent_id,
                    &s.id,
                    ctx.session_id.as_deref(),
                ) {
                    tracing::warn!(skill = %s.name, error = %e, "failed to record skill use");
                }
                let mut data = json!({
                    "name": s.name,
                    "description": s.description,
                    "situation_pattern": s.situation_pattern,
                    "guidance": s.guidance,
                    "source_type": s.source_type,
                    "is_active": s.is_active,
                    "archived": s.archived,
                    "usage_count": s.usage_count,
                });
                // D-1143: オーナーが「誰でも使える」に指定したスキルは、挙げたツールをこのターンの
                // 残りで誰の会話でも使えるようにする（実行前ゲートは bridge が持つ）。
                let granted = s.granted_tools().to_vec();
                if granted.is_empty() {
                    return ActionResult::success(data);
                }
                data["unlocked_tools"] = json!(granted);
                data["unlocked_tools_note"] = json!(format!(
                    "The owner designated this skill as usable by anyone. For the rest of this \
                     turn you may call these tools in this conversation: {}",
                    granted.join(", ")
                ));
                ActionResult::success(data)
                    .with_side_effect(SideEffect::ToolsGranted { tools: granted })
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
