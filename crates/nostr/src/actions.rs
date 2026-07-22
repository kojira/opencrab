//! エージェントに露出する Nostr 送信ツール（`nostr_post` / `nostr_reply` /
//! `nostr_dm` / `nostr_zap` / `nostr_upload`）。`GatewayActions` 実装なので
//! `BridgedExecutor` がツール一覧にマージし、LLM から呼べる。

use async_trait::async_trait;
use opencrab_gateway::{GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext};
use serde_json::{json, Value};

use crate::cli::NostaroCli;

/// Nostr 送信アクション群。実際の送信は nostaro CLI（per-agent 鍵）へ委譲する。
pub struct NostrGatewayActions {
    cli: NostaroCli,
}

impl NostrGatewayActions {
    pub fn new(cli: NostaroCli) -> Self {
        Self { cli }
    }
}

fn ok(data: Value) -> GatewayActionResult {
    GatewayActionResult {
        success: true,
        data: Some(data),
        error: None,
    }
}

fn err(msg: impl Into<String>) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg.into()),
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

#[async_trait]
impl GatewayActions for NostrGatewayActions {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        vec![
            GatewayActionDef {
                name: "nostr_post".to_string(),
                description: "Nostr に新規ノート（kind:1）を投稿する。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string", "description": "投稿本文。"}
                    },
                    "required": ["text"]
                }),
            },
            GatewayActionDef {
                name: "nostr_reply".to_string(),
                description: "Nostr の特定ノートに返信する。target は受信イベントの note_id（note1...）または hex id。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "target": {"type": "string", "description": "返信先ノートの note1.../hex id。"},
                        "text": {"type": "string", "description": "返信本文。"}
                    },
                    "required": ["target", "text"]
                }),
            },
            GatewayActionDef {
                name: "nostr_dm".to_string(),
                description: "Nostr DM（既定 NIP-17 暗号化）を送る。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "recipient": {"type": "string", "description": "宛先の npub または hex pubkey。"},
                        "text": {"type": "string", "description": "本文。"}
                    },
                    "required": ["recipient", "text"]
                }),
            },
            GatewayActionDef {
                name: "nostr_zap".to_string(),
                description: "Nostr で zap（Lightning 投げ銭）を送る。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "recipient": {"type": "string", "description": "宛先の npub または hex pubkey。"},
                        "amount": {"type": "integer", "description": "sats 単位の金額。"},
                        "message": {"type": "string", "description": "zap コメント（任意）。"}
                    },
                    "required": ["recipient", "amount"]
                }),
            },
            GatewayActionDef {
                name: "nostr_upload".to_string(),
                description: "ワークスペース内のファイルを Blossom にアップロードして URL を得る。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "アップロードするファイルパス。"}
                    },
                    "required": ["path"]
                }),
            },
        ]
    }

    async fn execute(
        &self,
        name: &str,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        let agent_id = &ctx.agent_id;
        match name {
            "nostr_post" => {
                let Some(text) = arg_str(args, "text") else {
                    return err("text パラメータが必要です");
                };
                match self.cli.post(agent_id, text).await {
                    Ok(out) => ok(json!({"result": out})),
                    Err(e) => err(format!("nostr_post 失敗: {e}")),
                }
            }
            "nostr_reply" => {
                let (Some(target), Some(text)) = (arg_str(args, "target"), arg_str(args, "text"))
                else {
                    return err("target と text パラメータが必要です");
                };
                match self.cli.reply(agent_id, target, text).await {
                    Ok(out) => ok(json!({"result": out})),
                    Err(e) => err(format!("nostr_reply 失敗: {e}")),
                }
            }
            "nostr_dm" => {
                let (Some(recipient), Some(text)) =
                    (arg_str(args, "recipient"), arg_str(args, "text"))
                else {
                    return err("recipient と text パラメータが必要です");
                };
                match self.cli.dm(agent_id, recipient, text).await {
                    Ok(out) => ok(json!({"result": out})),
                    Err(e) => err(format!("nostr_dm 失敗: {e}")),
                }
            }
            "nostr_zap" => {
                let Some(recipient) = arg_str(args, "recipient") else {
                    return err("recipient パラメータが必要です");
                };
                let Some(amount) = args.get("amount").and_then(|v| v.as_u64()) else {
                    return err("amount パラメータ（整数）が必要です");
                };
                let message = arg_str(args, "message");
                match self.cli.zap(agent_id, recipient, amount, message).await {
                    Ok(out) => ok(json!({"result": out})),
                    Err(e) => err(format!("nostr_zap 失敗: {e}")),
                }
            }
            "nostr_upload" => {
                let Some(path) = arg_str(args, "path") else {
                    return err("path パラメータが必要です");
                };
                match self.cli.upload(agent_id, path).await {
                    Ok(url) => ok(json!({"url": url})),
                    Err(e) => err(format!("nostr_upload 失敗: {e}")),
                }
            }
            other => err(format!("unknown nostr action: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_definitions_cover_all_actions() {
        let a = NostrGatewayActions::new(NostaroCli::new());
        let names: Vec<String> = a.definitions().into_iter().map(|d| d.name).collect();
        for expected in [
            "nostr_post",
            "nostr_reply",
            "nostr_dm",
            "nostr_zap",
            "nostr_upload",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
    }

    #[tokio::test]
    async fn test_missing_args_rejected_without_spawning() {
        let a = NostrGatewayActions::new(NostaroCli::new());
        let ctx = GatewayCallContext::for_agent("agent-1");
        // text 欠落 → nostaro を spawn せず即エラー。
        let r = a.execute("nostr_post", &json!({}), &ctx).await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("text"));

        let r = a
            .execute("nostr_zap", &json!({"recipient": "npub1x"}), &ctx)
            .await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("amount"));

        let r = a.execute("unknown_x", &json!({}), &ctx).await;
        assert!(!r.success);
    }
}
