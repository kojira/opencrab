//! VC 参加/退出のゲートウェイアクション。
//!
//! - `join_voice_channel`: 指定 VC に参加し、音声対話を開始する。
//!   STT 結果は呼び出し元セッションのテキストチャンネル（省略時）または
//!   `text_channel_id` で指定したチャンネルへ通常メッセージとして注入される。
//! - `leave_voice_channel`: 現在のギルドの VC から退出する。
//!
//! 権限: owner / trusted_user のみ（VC 参加はサーバの他メンバーに直接
//! 聞こえる行為なので、勝手な参加を防ぐ）。sub-engine からは不可。

use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};
use serde_json::json;

use super::subtask_webhook::reject;
use super::DiscordGatewayActions;
use crate::message_loop::parse_discord_session;

fn err(msg: impl Into<String>) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg.into()),
    }
}

fn caller_allowed(caller: &GatewayCaller) -> bool {
    matches!(caller, GatewayCaller::Owner | GatewayCaller::TrustedUser)
}

impl DiscordGatewayActions {
    pub(crate) async fn execute_join_voice_channel(
        &self,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        if !caller_allowed(&ctx.caller) {
            return reject("join_voice_channel requires owner or trusted_user".to_string());
        }
        let Some(voice) = &self.voice else {
            return err("voice 機能が無効です（config.toml の [voice] enabled = true が必要）");
        };
        let Some(vc_channel_id) = args["channel_id"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| args["channel_id"].as_u64())
        else {
            return err("join_voice_channel: 'channel_id'（VCのID）が必要です");
        };
        // guild は呼び出し元セッションから復元（fail-closed）
        let Some((guild_str, text_channel)) = ctx
            .session_id
            .as_deref()
            .and_then(parse_discord_session)
            .map(|(g, c)| (g, c.to_string()))
        else {
            return err("join_voice_channel は Discord セッション文脈でのみ実行できます");
        };
        let Ok(guild_id) = guild_str.parse::<u64>() else {
            return err("DM からは VC に参加できません");
        };
        // 注入先テキストチャンネル: 明示指定 > 呼び出し元チャンネル
        let text_channel_id = args["text_channel_id"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or(text_channel);

        match voice
            .join(
                guild_id,
                vc_channel_id,
                Some(text_channel_id.clone()),
                &ctx.agent_id,
            )
            .await
        {
            Ok(()) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "status": "joined",
                    "guild_id": guild_id.to_string(),
                    "vc_channel_id": vc_channel_id.to_string(),
                    "text_channel_id": text_channel_id,
                    "note": "音声はユーザーごとに文字起こしされ、このチャンネルの会話として届きます。返信は自動で読み上げられます。",
                })),
                error: None,
            },
            Err(e) => err(format!("VC 参加に失敗: {e:#}")),
        }
    }

    pub(crate) async fn execute_leave_voice_channel(
        &self,
        _args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        if !caller_allowed(&ctx.caller) {
            return reject("leave_voice_channel requires owner or trusted_user".to_string());
        }
        let Some(voice) = &self.voice else {
            return err("voice 機能が無効です");
        };
        let Some(guild_id) = ctx
            .session_id
            .as_deref()
            .and_then(parse_discord_session)
            .and_then(|(g, _)| g.parse::<u64>().ok())
        else {
            return err("leave_voice_channel は Discord セッション文脈でのみ実行できます");
        };
        match voice.leave(guild_id).await {
            Ok(()) => GatewayActionResult {
                success: true,
                data: Some(json!({"status": "left"})),
                error: None,
            },
            Err(e) => err(format!("VC 退出に失敗: {e:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serenity::http::Http;
    use std::sync::Arc;

    /// voice 無効（`voice: None`）の gateway。権限ゲートは voice の有無より**手前**に
    /// あるので、これで「弾かれた理由」を区別できる。
    fn actions() -> (DiscordGatewayActions, opencrab_db::Db) {
        let db = opencrab_db::Db::memory().unwrap();
        // serenity の Http はダミートークンで組むだけ（ネットワークには出ない）。
        let http = Arc::new(Http::new("dummy-token"));
        let a = DiscordGatewayActions::new(http, db.clone(), "/tmp".to_string(), None);
        (a, db)
    }

    fn ctx(caller: GatewayCaller) -> GatewayCallContext {
        GatewayCallContext::new(caller, "test-agent").with_session_id("discord-test-agent-111-222")
    }

    fn is_permission_rejection(r: &GatewayActionResult) -> bool {
        !r.success
            && r.error.as_deref().is_some_and(|e| {
                e.starts_with(opencrab_actions::REJECTION_CODE_PREFIX)
                    && e.contains("requires owner or trusted_user")
            })
    }

    /// **VC 参加/退出は owner / trusted_user 以外を弾く**（#203 の一括点検）。
    ///
    /// VC 参加はサーバの他メンバーに直接聞こえる行為なので、エージェント本体や
    /// 別エージェント（co_agent）が勝手に入れてはならない。`caller_allowed` を
    /// 常に真へ書き換えても落ちるテストが 1 件も無く（ツール名の存在確認しか
    /// なかった）、この 2 本が Discord 側で唯一の実行時ゲートだった。
    ///
    /// **co_agent を明示的に含める**のが要点: bridge の `caller_is_trusted`
    /// （owner / co_agent / trusted_user）より**狭い**のがこのゲートの意図なので、
    /// うっかり bridge 側の集合に揃えた変更を落とせる形にしてある。
    #[tokio::test]
    async fn voice_actions_reject_non_trusted_callers() {
        let (a, _db) = actions();
        let args = serde_json::json!({"channel_id": "333"});

        for caller in [
            GatewayCaller::Agent,
            GatewayCaller::CoAgent {
                agent_id: "other".to_string(),
            },
        ] {
            let label = caller.label();
            let joined = a
                .execute_join_voice_channel(&args, &ctx(caller.clone()))
                .await;
            assert!(
                is_permission_rejection(&joined),
                "join_voice_channel が {label} を権限で弾いていない: {:?}",
                joined.error
            );
            let left = a
                .execute_leave_voice_channel(&serde_json::json!({}), &ctx(caller))
                .await;
            assert!(
                is_permission_rejection(&left),
                "leave_voice_channel が {label} を権限で弾いていない: {:?}",
                left.error
            );
        }
    }

    /// 上のテストが「常に弾かれるから緑」になっていないことの対照。
    ///
    /// owner / trusted_user はゲートを**通り抜け**、その先の「voice 機能が無効」で
    /// 止まる（= 失敗理由が権限ではない）。ゲートを閉じ切る変異を入れると落ちる。
    #[tokio::test]
    async fn voice_actions_let_owner_and_trusted_user_past_the_gate() {
        let (a, _db) = actions();
        let args = serde_json::json!({"channel_id": "333"});

        for caller in [GatewayCaller::Owner, GatewayCaller::TrustedUser] {
            let label = caller.label();
            let joined = a
                .execute_join_voice_channel(&args, &ctx(caller.clone()))
                .await;
            assert!(
                !is_permission_rejection(&joined),
                "join_voice_channel が {label} を権限で弾いている: {:?}",
                joined.error
            );
            assert!(
                joined.error.as_deref().is_some_and(|e| e.contains("voice")),
                "{label}: ゲートの先（voice 無効）まで届いていない: {:?}",
                joined.error
            );
            let left = a
                .execute_leave_voice_channel(&serde_json::json!({}), &ctx(caller))
                .await;
            assert!(
                !is_permission_rejection(&left),
                "leave_voice_channel が {label} を権限で弾いている: {:?}",
                left.error
            );
        }
    }
}
