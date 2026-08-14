//! エージェントに露出する Nostr 送信ツール（`nostr_post` / `nostr_reply` /
//! `nostr_zap` / `nostr_upload`）。`GatewayActions` 実装なので
//! `BridgedExecutor` がツール一覧にマージし、LLM から呼べる。
//!
//! #514: `nostr_dm`（DM 送信）は撤去した。DM は暗号化されていても秘密鍵が漏れた時点で
//! 過去に遡って全部読めるため、「暗号化されているから private を書いてよい」という誤った
//! 安心を前提ごと無くす（オーナー決定）。送信は本ツールの撤去に加え、`nostr_run dm`
//! passthrough も塞いで二経路とも封じている（[`crate::cli::NostaroCli::PASSTHROUGH_DENIED_SUBCOMMANDS`]）。
//! private な話は Nostr でせず、Discord の DM か指定チャンネルを使うこと。

use std::sync::Arc;

use async_trait::async_trait;
use opencrab_gateway::{GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext};
use serde_json::{json, Value};

use crate::cli::NostaroCli;
use crate::identity::NostrIdentityAdmin;

/// Nostr 送信アクション群。実際の送信は nostaro CLI（per-agent 鍵）へ委譲する。
///
/// 配送はすべてエージェントがこれらのツールを呼んで行う（機構は暗黙返信しない・#588）。
/// `admin` は identity 切替（本鍵採用）の実体で、watch ループ稼働時のみ Some
/// （owner/trusted 限定ツール `nostr_switch_identity` から使う）。
pub struct NostrGatewayActions {
    cli: NostaroCli,
    admin: Option<Arc<dyn NostrIdentityAdmin>>,
    /// 素テキスト配送口（`text_delivery()`）が焼く agent_id（#246 段階3 PR-B）。
    ///
    /// `None` のときは配送口を提供しない（既定＝トレイトの `text_delivery()` が None を
    /// 返すのと同じ）。bridge 経路（`sink.rs`）は agent_id を焼かずに生成するので従来どおり
    /// None のまま。`gateway_actions_for`（稼働中の gateway 用）だけが agent_id を焼き、
    /// そのときだけ自発投稿の配送口が生える。
    agent_id: Option<String>,
}

impl NostrGatewayActions {
    pub fn new(cli: NostaroCli) -> Self {
        Self {
            cli,
            admin: None,
            agent_id: None,
        }
    }

    /// identity 切替の実体を注入する（watch ループが稼働時に渡す）。
    pub fn with_admin(mut self, admin: Arc<dyn NostrIdentityAdmin>) -> Self {
        self.admin = Some(admin);
        self
    }

    /// 素テキスト配送口（自発 kind:1 投稿）用の agent_id を焼き込む（#246 段階3 PR-B）。
    ///
    /// 稼働中の gateway に対して `gateway_actions_for(agent_id)` が呼ぶ。これを設定した
    /// 実体だけが `text_delivery()` で `Some` を返す（登録簿経由で「テキストを配れる
    /// gateway」として見えるようになる）。
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
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

/// 送信系ツール共通の任意 `from` パラメータ定義（マルチ identity 投稿）。
fn from_param() -> Value {
    json!({
        "type": "string",
        "description": "任意。nostr_generate_key で生成した鍵の npub を指定すると、本鍵ではなく\
                        その鍵で送信する（未指定なら本鍵で送信）。指定できるのは自分が生成した鍵のみ。"
    })
}

#[async_trait]
impl GatewayActions for NostrGatewayActions {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        vec![
            GatewayActionDef {
                name: "nostr_post".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "Nostr に新規ノート（kind:1）を投稿する。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string", "description": "投稿本文。"},
                        "from": from_param(),
                    },
                    "required": ["text"]
                }),
            },
            GatewayActionDef {
                name: "nostr_reply".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::ConversationBound },
                description: "Nostr の特定ノートに返信する。target は受信イベントの note_id（note1...）または hex id。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "target": {"type": "string", "description": "返信先ノートの note1.../hex id。"},
                        "text": {"type": "string", "description": "返信本文。"},
                        "from": from_param(),
                    },
                    "required": ["target", "text"]
                }),
            },
            // #514: `nostr_dm` はここに**定義しない**（DM 送信は禁止）。DM は暗号化
            // されていても秘密鍵が漏れた時点で過去に遡って全部読めるため、その前提ごと
            // 無くす（オーナー決定）。定義から外すのでモデルはこのツールを見ない。万一
            // 名前指定で呼ばれても `execute` が fail-closed で拒否する。将来「やっぱり DM を
            // 使いたい」なら、この定義と `execute` の分岐、`PASSTHROUGH_DENIED_SUBCOMMANDS`
            // の `dm`、`DM_KINDS` の受信破棄を戻せばよい（3 経路とも 1 か所ずつ）。
            GatewayActionDef {
                name: "nostr_zap".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "Nostr で zap（Lightning 投げ銭）を送る。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "recipient": {"type": "string", "description": "宛先の npub または hex pubkey。"},
                        "amount": {"type": "integer", "description": "sats 単位の金額。"},
                        "message": {"type": "string", "description": "zap コメント（任意）。"},
                        "from": from_param(),
                    },
                    "required": ["recipient", "amount"]
                }),
            },
            GatewayActionDef {
                name: "nostr_upload".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "ワークスペース内のファイルを Blossom にアップロードして URL を得る。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "アップロードするファイルパス。"},
                        "from": from_param(),
                    },
                    "required": ["path"]
                }),
            },
            GatewayActionDef {
                name: "nostr_generate_key".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Dispatchable, sub_engine: opencrab_gateway::SubEngineAccess::Allowed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "新しい Nostr 鍵（keypair）を生成する。任意で vanity prefix（npub の \
                              npub1 以降・bech32 文字のみ。長さ上限は無いが、長いほど探索に時間が \
                              かかる＝3文字程度で即時、それ以上は徐々に長くなる）を指定できる。返るのは公開情報の \
                              npub / pubkey のみ。**秘密鍵(nsec)はサーバ内に安全に保存され、あなた（LLM）\
                              には渡されない**（セキュリティのため）。これは新規 keypair を作るユーティリティ\
                              であり、あなた自身の送信用アイデンティティは変更しない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "prefix": {"type": "string", "description": "任意。npub の npub1 以降に前置したい bech32 文字列（長さ上限なし。長いほど探索に時間がかかる, 例: cat）。"}
                    }
                }),
            },
            GatewayActionDef {
                name: "nostr_list_keys".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分が nostr_generate_key で生成した鍵の一覧（npub のみ）を返す。\
                              nostr_switch_identity で本鍵に採用する候補を確認するのに使う。\
                              返るのは公開情報の npub だけで、**秘密鍵(nsec)は一切返らない**。"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            GatewayActionDef {
                name: "nostr_switch_identity".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分が nostr_generate_key で生成した鍵を、この Nostr ゲートウェイの\
                              **本鍵（送信・受信のアイデンティティ）として採用**する。以後の投稿は\
                              その鍵で行われる。npub には generated_key で作った鍵の npub を渡す。\
                              重要な操作なので owner（信頼ユーザー）からの依頼時のみ実行される。\
                              秘密鍵は扱わない（npub 参照のみ）。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "npub": {"type": "string", "description": "本鍵に採用する、生成済み鍵の npub。"}
                    },
                    "required": ["npub"]
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
                match self.cli.post(agent_id, text, arg_str(args, "from")).await {
                    Ok(out) => ok(json!({"result": out})),
                    Err(e) => err(format!("nostr_post 失敗: {e}")),
                }
            }
            "nostr_reply" => {
                let (Some(target), Some(text)) = (arg_str(args, "target"), arg_str(args, "text"))
                else {
                    return err("target と text パラメータが必要です");
                };
                match self
                    .cli
                    .reply(agent_id, target, text, arg_str(args, "from"))
                    .await
                {
                    Ok(out) => ok(json!({"result": out})),
                    Err(e) => err(format!("nostr_reply 失敗: {e}")),
                }
            }
            // #514: DM 送信は禁止。定義から外しているのでモデルは通常ここへ来ないが、
            // 名前指定で呼ばれても fail-closed で拒否する（黙って成功に見せない）。
            "nostr_dm" => err(
                "nostr_dm は廃止されました（#514）。Nostr の DM は秘密鍵が漏れると過去に遡って\
                 全部読めるため扱いません。private な話は Discord の DM か指定チャンネルを\
                 使ってください。",
            ),
            "nostr_zap" => {
                let Some(recipient) = arg_str(args, "recipient") else {
                    return err("recipient パラメータが必要です");
                };
                let Some(amount) = args.get("amount").and_then(|v| v.as_u64()) else {
                    return err("amount パラメータ（整数）が必要です");
                };
                let message = arg_str(args, "message");
                match self
                    .cli
                    .zap(agent_id, recipient, amount, message, arg_str(args, "from"))
                    .await
                {
                    Ok(out) => ok(json!({"result": out})),
                    Err(e) => err(format!("nostr_zap 失敗: {e}")),
                }
            }
            "nostr_upload" => {
                let Some(path) = arg_str(args, "path") else {
                    return err("path パラメータが必要です");
                };
                match self.cli.upload(agent_id, path, arg_str(args, "from")).await {
                    Ok(url) => ok(json!({"url": url})),
                    Err(e) => err(format!("nostr_upload 失敗: {e}")),
                }
            }
            "nostr_generate_key" => {
                // prefix は任意。未指定/空ならランダム鍵。検証は cli.vanity 側で行う。
                let prefix = arg_str(args, "prefix").unwrap_or("");
                match self.cli.vanity(prefix).await {
                    Ok(k) => {
                        // 秘密鍵(nsec)は**LLM に返さない**。サーバ内に暗号化して 0600 で
                        // 保存し、npub/pubkey のみ返す（鍵生成は送信ではないので配送系ではない）。
                        match self.cli.save_generated_key(agent_id, &k) {
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
            "nostr_list_keys" => {
                // 生成鍵の npub 一覧のみ返す（nsec は読まない・返さない）。
                match NostaroCli::list_generated_keys(agent_id) {
                    Ok(npubs) => ok(json!({
                        "npubs": npubs,
                        "note": "あなたが生成した鍵の npub 一覧です。nostr_switch_identity で本鍵に採用できます。秘密鍵(nsec)はサーバ内に安全に保存されており、ここには含まれません。",
                    })),
                    Err(e) => err(format!("nostr_list_keys 失敗: {e}")),
                }
            }
            "nostr_switch_identity" => {
                let Some(npub) = arg_str(args, "npub") else {
                    return err("npub パラメータが必要です");
                };
                let Some(admin) = self.admin.as_ref() else {
                    return err(
                        "この環境では identity 切替は利用できません（ゲートウェイ稼働時のみ）",
                    );
                };
                match admin.adopt_generated_identity(agent_id, npub).await {
                    Ok(adopted) => ok(json!({
                        "npub": adopted,
                        "note": "この鍵を本鍵として採用しました。以後の投稿・公開ノート受信はこの identity で行われます。秘密鍵は扱っていません。なお暗号化DMの受信を新 identity で行うにはゲートウェイの再起動が必要です。",
                    })),
                    Err(e) => err(format!("nostr_switch_identity 失敗: {e}")),
                }
            }
            other => err(format!("unknown nostr action: {other}")),
        }
    }

    /// 素テキストの配送口（#246 段階3 PR-B）。agent_id を焼いた実体だけが `Some` を返す。
    ///
    /// 返すのは Nostr への**自発投稿（kind:1 broadcast）**を行う配送口。宛先はエージェント
    /// 設定のリレー集合を nostaro が解決するため、`target` 引数は使わない。合成 gateway
    /// （`SystemGatewayActions`）がここから配送口を 1 度だけ引き、ハートビート等の
    /// transport 非依存な配送口や `request_peer_review` の汎用層へ渡す。
    fn text_delivery(&self) -> Option<Arc<dyn opencrab_core::text_delivery::TextDelivery>> {
        let agent_id = self.agent_id.clone()?;
        Some(Arc::new(crate::text_delivery::NostrTextDelivery::new(
            agent_id,
            self.cli.clone(),
        )))
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
            "nostr_zap",
            "nostr_upload",
            "nostr_generate_key",
            "nostr_list_keys",
            "nostr_switch_identity",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        // #514: nostr_dm は定義から外れている（DM 送信禁止）。
        assert!(
            !names.contains(&"nostr_dm".to_string()),
            "nostr_dm は #514 で撤去したので定義に無いこと"
        );
    }

    /// **sharing 属性の固定**: `sharing` には権威リストが無いので、Nostr ゲートの
    /// `ConversationBound` 集合をここで固定する（判定基準は `opencrab_gateway::ToolSharing`
    /// の doc）。会話固有の一時ハンドル（受信投稿の note id = `target`）を必須に取る
    /// `nostr_reply` のみ。全ゲート横断の `ConversationBound` は
    /// {discord_add_reaction, nostr_reply, send_ui}。
    ///
    /// dispatch / sub_engine の値は各定義の構築サイトで必須指定される（`ToolClass` に
    /// `Default` が無い）ので、値の正しさはコードレビューが担い、専用の照合テストは持たない
    /// （PR-2B で gateway 固有の権威リストと等価性テストを削除した）。
    #[test]
    fn nostr_tool_sharing_conversation_bound_set_is_fixed() {
        use opencrab_gateway::ToolSharing;
        let a = NostrGatewayActions::new(NostaroCli::new());
        let defs = a.definitions();
        assert!(!defs.is_empty());
        let conv_bound: std::collections::BTreeSet<String> = defs
            .iter()
            .filter(|d| d.class.sharing == ToolSharing::ConversationBound)
            .map(|d| d.name.clone())
            .collect();
        let expected: std::collections::BTreeSet<String> =
            std::iter::once("nostr_reply".to_string()).collect();
        assert_eq!(
            conv_bound, expected,
            "nostr ゲートの ConversationBound 集合がずれている（sharing 属性の付け忘れ/誤り）"
        );
    }

    /// [#514] `nostr_dm` は定義に無く、名前指定で呼んでも fail-closed で拒否される。
    /// 送信禁止の回帰防止（黙って成功に見せない）。
    #[tokio::test]
    async fn test_nostr_dm_is_removed_and_rejected() {
        let a = NostrGatewayActions::new(NostaroCli::new());
        let names: Vec<String> = a.definitions().into_iter().map(|d| d.name).collect();
        assert!(!names.contains(&"nostr_dm".to_string()));

        let ctx = GatewayCallContext::for_agent("agent-dm-block");
        let r = a
            .execute(
                "nostr_dm",
                &json!({"recipient": "npub1x", "text": "secret"}),
                &ctx,
            )
            .await;
        assert!(!r.success, "nostr_dm は成功してはいけない");
        let msg = r.error.unwrap();
        assert!(msg.contains("514"), "拒否理由に #514 を含む: {msg}");
        assert!(msg.contains("Discord"), "代替（Discord）へ誘導する: {msg}");
    }

    /// `nostr_list_keys` は生成鍵の npub 一覧のみ返し、nsec を応答に出さない。
    #[tokio::test]
    async fn test_list_keys_returns_npubs_without_nsec() {
        let agent = "agent-actions-list-keys";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());

        let a = NostrGatewayActions::new(NostaroCli::new());
        let ctx = GatewayCallContext::for_agent(agent);

        // 生成前は空配列。
        let r = a.execute("nostr_list_keys", &json!({}), &ctx).await;
        assert!(r.success);
        assert_eq!(
            r.data.as_ref().unwrap()["npubs"].as_array().unwrap().len(),
            0
        );

        // 鍵を 2 本保存してから列挙する。
        for npub in ["npub1keyone", "npub1keytwo"] {
            NostaroCli::new()
                .save_generated_key(
                    agent,
                    &crate::cli::GeneratedKey {
                        nsec: format!("nsec1verysecret-{npub}"),
                        npub: npub.to_string(),
                        pubkey: "deadbeef".to_string(),
                    },
                )
                .unwrap();
        }
        let r = a.execute("nostr_list_keys", &json!({}), &ctx).await;
        assert!(r.success);
        let data = r.data.unwrap();
        let npubs: Vec<&str> = data["npubs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(npubs.contains(&"npub1keyone"));
        assert!(npubs.contains(&"npub1keytwo"));
        // 応答全体（note 含む）に nsec の実値が漏れないこと。
        assert!(
            !serde_json::to_string(&data)
                .unwrap()
                .contains("nsec1verysecret"),
            "nsec が nostr_list_keys の応答に漏れている"
        );

        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    #[tokio::test]
    async fn test_switch_identity_without_admin_is_rejected() {
        // admin 未注入（＝ゲートウェイ非稼働）では切替不可。npub があってもエラー。
        let a = NostrGatewayActions::new(NostaroCli::new());
        let ctx = GatewayCallContext::for_agent("agent-1");
        let r = a
            .execute("nostr_switch_identity", &json!({"npub": "npub1x"}), &ctx)
            .await;
        assert!(!r.success);
        // npub 欠落も即エラー（spawn しない）。
        let r = a.execute("nostr_switch_identity", &json!({}), &ctx).await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("npub"));
    }

    /// ドリフト検出: `TRUSTED_ONLY_ACTIONS` の nostr_* は実在の nostr アクションを
    /// 指していること（片方をリネームするとゲートが死に名を指して黙って無効化される）。
    ///
    /// ただし **server-own の nostr_* は inner の `definitions()` には無い**（#268 の
    /// `nostr_run` は薄い passthrough として server 側だけに定義し、inner には持たせない）。
    /// server-own 名の liveness は server crate 側の
    /// `nostr_run_is_own_unrestricted_and_inline`（`own_definitions()` に実在することを
    /// 主張）が担保するので、ここでは対象外にする。
    ///
    /// `nostr_run` が `TRUSTED_ONLY_ACTIONS` から外れた（#303）今、この除外は空振りする。
    /// それでも定数を残すのは、server-own の nostr_* 名に将来 caller ゲートが付いたときに
    /// この drift 検出が「inner の definitions に無い」で誤検知しないようにするため。
    /// 名前から "TRUSTED" を落としたのは、除外の理由が trusted かどうかではなく
    /// **server-own かどうか**だから（#303）。
    const SERVER_OWN_NOSTR_ACTIONS: &[&str] = &["nostr_run"];
    #[test]
    fn test_trusted_only_nostr_names_are_live() {
        let a = NostrGatewayActions::new(NostaroCli::new());
        let names: Vec<String> = a.definitions().into_iter().map(|d| d.name).collect();
        for n in opencrab_actions::TRUSTED_ONLY_ACTIONS
            .iter()
            .filter(|n| n.starts_with("nostr_"))
            .filter(|n| !SERVER_OWN_NOSTR_ACTIONS.contains(n))
        {
            assert!(
                names.contains(&n.to_string()),
                "{n} は TRUSTED_ONLY だが nostr gateway definitions に無い"
            );
        }
    }

    /// #168: `cancel_subtask` を Nostr gateway 側で**定義しない**こと。
    ///
    /// `SystemGatewayActions` は「inner が cancel_subtask を定義していれば inner へ委譲」
    /// する。Nostr が定義してしまうと、`RunRequest::with_dispatch` で渡した共有 registry
    /// （`NostrSessionRuntime` が session 単位で貸すもの）ではなく inner 側の別 registry が
    /// 引かれ、走行中 subtask の停止が常に not found になる。定義しない＝server-neutral の
    /// 実装が共有 registry を引く、が正しい配線。
    #[test]
    fn test_nostr_gateway_does_not_define_cancel_subtask() {
        let names: Vec<String> = NostrGatewayActions::new(NostaroCli::new())
            .definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(
            !names.contains(&"cancel_subtask".to_string()),
            "cancel_subtask は server-neutral 実装（共有 registry を引く）に任せる"
        );
    }

    /// #168: Nostr 配送系は **非ブロック dispatch の対象外**（`class.dispatch == Inline`）
    /// であること。分類の権威は各定義の属性なので `definitions()` を実体で呼んで直接見る。
    ///
    /// 配送ツールは**戻り値（送信結果）を同ターンで使う**ので inline で走らせる。background 化
    /// すると run が返る時点でまだ送信されておらず、エージェントは同ターンで送信可否を確認できない。
    /// `nostr_generate_key`（長時間の鍵探索）だけは dispatch 対象に残す（S3a の主目的、
    /// E2E `e2e_cancel_stops_subtask` の前提）。
    #[test]
    fn nostr_delivery_actions_are_inline() {
        use opencrab_gateway::DispatchMode;
        let defs = NostrGatewayActions::new(NostaroCli::new()).definitions();
        let class_of = |name: &str| {
            defs.iter()
                .find(|d| d.name == name)
                .unwrap_or_else(|| panic!("{name} が nostr definitions() に無い"))
                .class
        };
        // 送信系（配送ツール）は inline。#514: nostr_dm は撤去済み。
        for name in [
            "nostr_post",
            "nostr_reply",
            "nostr_zap",
            "nostr_upload",
            "nostr_switch_identity",
            "nostr_list_keys",
        ] {
            assert_eq!(
                class_of(name).dispatch,
                DispatchMode::Inline,
                "{name} は配送系なので inline（戻り値を同ターンで使う）"
            );
        }
        // 長時間処理は dispatch 対象に残す。
        assert_eq!(
            class_of("nostr_generate_key").dispatch,
            DispatchMode::Dispatchable,
            "nostr_generate_key は background 化する（S3a の主目的）"
        );
    }

    /// [#246 段階3 PR-B] 配送口は **agent_id を焼いた実体だけ** が提供する。
    ///
    /// bridge 経路（`sink.rs`）は agent_id を焼かず生成するので `text_delivery()` は None
    /// （＝従来どおり配送口を出さない）。`gateway_actions_for` が焼いたときだけ Some。
    #[test]
    fn text_delivery_is_gated_on_baked_agent_id() {
        // 焼かなければ None（既存ターン処理を壊さない）。
        let plain = NostrGatewayActions::new(NostaroCli::new());
        assert!(
            plain.text_delivery().is_none(),
            "agent_id を焼かない実体は配送口を出さない"
        );

        // 焼けば Some（自発投稿の配送口が生える）。
        let baked = NostrGatewayActions::new(NostaroCli::new()).with_agent_id("agent-x");
        assert!(
            baked.text_delivery().is_some(),
            "agent_id を焼いた実体は配送口を出す"
        );
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
