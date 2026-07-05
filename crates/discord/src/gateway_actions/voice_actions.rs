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
