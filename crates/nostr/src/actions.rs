//! エージェントに露出する Nostr 送信ツール（`nostr_post` / `nostr_reply` /
//! `nostr_dm` / `nostr_zap` / `nostr_upload`）。`GatewayActions` 実装なので
//! `BridgedExecutor` がツール一覧にマージし、LLM から呼べる。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use opencrab_gateway::{GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext};
use serde_json::{json, Value};

use crate::cli::NostaroCli;

/// Nostr 送信アクション群。実際の送信は nostaro CLI（per-agent 鍵）へ委譲する。
///
/// `sent` は「このターンで明示的に送信（post/reply/dm/zap）した」フラグ。ループ側が
/// これを見て暗黙返信の二重送信を防ぐ。
pub struct NostrGatewayActions {
    cli: NostaroCli,
    sent: Arc<AtomicBool>,
}

impl NostrGatewayActions {
    pub fn new(cli: NostaroCli) -> Self {
        Self {
            cli,
            sent: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 「送信済み」フラグを共有取得する（ループが暗黙返信の抑制に使う）。
    pub fn sent_flag(&self) -> Arc<AtomicBool> {
        self.sent.clone()
    }

    fn mark_sent(&self) {
        self.sent.store(true, Ordering::SeqCst);
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
            GatewayActionDef {
                name: "nostr_generate_key".to_string(),
                description: "新しい Nostr 鍵（keypair）を生成する。任意で vanity prefix（npub の \
                              npub1 以降・bech32 文字のみ・最大3文字）を指定できる。返るのは公開情報の \
                              npub / pubkey のみ。**秘密鍵(nsec)はサーバ内に安全に保存され、あなた（LLM）\
                              には渡されない**（セキュリティのため）。これは新規 keypair を作るユーティリティ\
                              であり、あなた自身の送信用アイデンティティは変更しない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "prefix": {"type": "string", "description": "任意。npub の npub1 以降に前置したい bech32 文字列（最大3文字, 例: cat）。"}
                    }
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
                    Ok(out) => {
                        self.mark_sent();
                        ok(json!({"result": out}))
                    }
                    Err(e) => err(format!("nostr_post 失敗: {e}")),
                }
            }
            "nostr_reply" => {
                let (Some(target), Some(text)) = (arg_str(args, "target"), arg_str(args, "text"))
                else {
                    return err("target と text パラメータが必要です");
                };
                match self.cli.reply(agent_id, target, text).await {
                    Ok(out) => {
                        self.mark_sent();
                        ok(json!({"result": out}))
                    }
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
                    Ok(out) => {
                        self.mark_sent();
                        ok(json!({"result": out}))
                    }
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
                    Ok(out) => {
                        self.mark_sent();
                        ok(json!({"result": out}))
                    }
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
            "nostr_generate_key" => {
                // prefix は任意。未指定/空ならランダム鍵。検証は cli.vanity 側で行う。
                let prefix = arg_str(args, "prefix").unwrap_or("");
                match self.cli.vanity(prefix).await {
                    Ok(k) => {
                        // 秘密鍵(nsec)は**LLM に返さない**。サーバ内に 0600 で保存し、
                        // npub/pubkey のみ返す（mark_sent は呼ばない＝送信ではない）。
                        match NostaroCli::save_generated_key(agent_id, &k) {
                            Ok(_) => ok(json!({
                                "npub": k.npub,
                                "pubkey": k.pubkey,
                                "note": "新しい鍵を生成しました。秘密鍵(nsec)はサーバ内に安全に保存済みで、セキュリティ上あなた（LLM）には渡していません。共有・言及してよいのは npub までです。",
                            })),
                            Err(e) => err(format!("鍵は生成しましたが保存に失敗しました: {e}")),
                        }
                    }
                    Err(e) => err(format!("nostr_generate_key 失敗: {e}")),
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
            "nostr_generate_key",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
    }

    /// ドリフト検出: `TRUSTED_ONLY_ACTIONS` の nostr_* は実在の nostr アクションを
    /// 指していること（片方をリネームするとゲートが死に名を指して黙って無効化される）。
    #[test]
    fn test_trusted_only_nostr_names_are_live() {
        let a = NostrGatewayActions::new(NostaroCli::new());
        let names: Vec<String> = a.definitions().into_iter().map(|d| d.name).collect();
        for n in opencrab_actions::TRUSTED_ONLY_ACTIONS
            .iter()
            .filter(|n| n.starts_with("nostr_"))
        {
            assert!(
                names.contains(&n.to_string()),
                "{n} は TRUSTED_ONLY だが nostr gateway definitions に無い"
            );
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
