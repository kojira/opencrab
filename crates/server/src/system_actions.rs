//! エージェント（owner 限定）が OpenCrab 自体の設定を変更するためのサーバ内ツール源。
//!
//! `AppState`（db / llm_router / llm_config）を必要とするため、素の dispatcher
//! アクションでは配線できない。`GatewayActions` として実装し、`BridgedExecutor` の
//! 単一 `gateway_actions` スロットに載せる。既存の gateway（Discord/Nostr 等）を
//! `inner` として保持し、自分が扱わないツールは委譲する（composite）ことで、
//! transport 非依存に「設定ツール」を全ターンへ供給する。
//!
//! owner ゲートは bridge の `OWNER_ONLY_ACTIONS`（可視性 + 実行の双方）が担うが、
//! 多層防御として本ハンドラでも caller を確認する（fail-closed）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use opencrab_actions::{
    cancel_subtask as neutral_cancel_subtask, CancelOutcome, SettleKind, SubtaskCompletionSink,
    SubtaskRegistry, SubtaskSettled, REJECTION_CODE_PREFIX,
};
use opencrab_gateway::{
    GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext, GatewayCaller,
};
use opencrab_mcp::is_valid_server_name;
use serde_json::{json, Value};

use crate::AppState;

/// `report_progress` のデバウンス待機時間。Discord 実装（`execute_report_progress`）と同一。
///
/// この時間内に後続の `report_progress` が来たら世代が進み、古い方は発火しない。
const PROGRESS_DEBOUNCE_DELAY: Duration = Duration::from_secs(3);

/// `configure_llm_provider` などのサーバ内設定ツールを提供する `GatewayActions`。
pub struct SystemGatewayActions {
    state: AppState,
    /// transport 固有の gateway（Discord/Nostr 等）。自分が扱わないツールを委譲する。
    inner: Option<Arc<dyn GatewayActions>>,
    /// auto-dispatch した走行中 subtask の共有 registry（#161）。web/Nostr/REST でも
    /// `cancel_subtask` を露出するため server-neutral 層に配線する。`run_agent_response`
    /// が dispatcher へ渡すものと同一 Arc（Discord では gateway_actions の registry とも
    /// 同一）。`None` の場合は走行中 subtask が無く cancel は not found を返す。
    subtask_registry: Option<SubtaskRegistry>,
    /// 停止（`cancel_subtask`）を通知する完了 sink（この run の `RunRequest` と同一）。
    ///
    /// 停止は `on_subtask_cancelled`（既定は no-op）で通知するため resume は起きない。
    /// REST のように「最後の subtask の決着でセッションを完了にする」経路は、この通知
    /// を受けて `sessions.status` の整合を取る（無いと永久 `active` のまま残る）。
    completion_sink: Option<Arc<dyn SubtaskCompletionSink>>,
}

impl SystemGatewayActions {
    pub fn new(
        state: AppState,
        inner: Option<Arc<dyn GatewayActions>>,
        subtask_registry: Option<SubtaskRegistry>,
        completion_sink: Option<Arc<dyn SubtaskCompletionSink>>,
    ) -> Self {
        Self {
            state,
            inner,
            subtask_registry,
            completion_sink,
        }
    }

    /// 本ツール源が直接提供するツール定義。
    fn own_definitions() -> Vec<GatewayActionDef> {
        vec![
            GatewayActionDef {
                name: "configure_llm_provider".to_string(),
                description:
                    "LLM プロバイダの設定を即時適用する（owner 限定）。DB オーバーライドに\
                保存してルーターをホットスワップするため再起動は不要。codex/cursor は適用後に\
                起動確認（health_check）を行い、失敗した場合は自動的に直前の設定へロールバック\
                して結果で通知する。acp と API キー型は自動ロールバックの対象外（acp の起動確認は\
                ネットワーク依存で誤判定しうるため。ダッシュボードの接続テストで明示的に確認する）。\
                各フィールドは三値: 省略=変更しない / null=オーバーライド解除（TOML に戻す）/ 値=上書き。\
                api_key はこのツールでは変更できない（ダッシュボードから設定する）。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "provider": {
                            "type": "string",
                            "description": "対象プロバイダ名（例: acp, codex, cursor, openai）。"
                        },
                        "enabled": {
                            "type": ["boolean", "null"],
                            "description": "有効/無効。null で解除。"
                        },
                        "default_model": {
                            "type": ["string", "null"],
                            "description": "既定モデル。null で解除。"
                        },
                        "binary_path": {
                            "type": ["string", "null"],
                            "description": "起動バイナリ（subprocess）。空文字/null で解除。"
                        },
                        "args": {
                            "type": ["array", "null"],
                            "items": { "type": "string" },
                            "description": "起動引数（subprocess）。null で解除。"
                        },
                        "working_dir": {
                            "type": ["string", "null"],
                            "description": "作業ディレクトリ。空文字/null で解除。"
                        },
                        "timeout_secs": {
                            "type": ["integer", "null"],
                            "description": "タイムアウト秒。null で解除。"
                        },
                        "reasoning_effort": {
                            "type": ["string", "null"],
                            "description": "推論強度（low/medium/high 等）。空文字/null で解除。"
                        },
                        "base_url": {
                            "type": ["string", "null"],
                            "description": "API ベース URL。null で解除。"
                        }
                    },
                    "required": ["provider"]
                }),
            },
            GatewayActionDef {
                name: "manage_allowed_commands".to_string(),
                description:
                    "自分（このエージェント）が execute_shell で実行できる許可コマンドを\
                管理する（owner 限定）。許可コマンドの追加はシェル実行範囲を広げるため owner のみ。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "add", "remove"],
                            "description": "list=一覧 / add=追加 / remove=削除。"
                        },
                        "command": {
                            "type": "string",
                            "description": "add/remove 対象のコマンド（例: git, cargo）。list では不要。"
                        }
                    },
                    "required": ["action"]
                }),
            },
            GatewayActionDef {
                name: "configure_nostr".to_string(),
                description:
                    "自分の Nostr 連携設定（購読リレー・フィルタ authors/keywords/kinds・\
                有効/無効）を変更する（owner 限定）。秘密鍵は変更も取得もできない（鍵生成は別手段）。\
                省略したフィールドは現状維持。enabled=true にするには author か keyword が必要。\
                設定は保存と同時にマネージャへ反映（enabled なら起動 / 無効なら停止）。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "relays": {
                            "type": "array", "items": {"type": "string"},
                            "description": "購読リレー URL 一覧（例: wss://yabu.me）。"
                        },
                        "authors": {
                            "type": "array", "items": {"type": "string"},
                            "description": "購読する author（npub/hex）。"
                        },
                        "keywords": {
                            "type": "array", "items": {"type": "string"},
                            "description": "購読キーワード。"
                        },
                        "kinds": {
                            "type": "array", "items": {"type": "integer"},
                            "description": "購読する kind 番号。"
                        },
                        "enabled": {
                            "type": "boolean",
                            "description": "有効化して起動 / 無効化して停止。"
                        }
                    }
                }),
            },
            GatewayActionDef {
                name: "configure_self".to_string(),
                description:
                    "自分（このエージェント）の人格・モデル・推論強度・web 検索などの設定を変更する\
                （owner 限定）。model/reasoning_effort/web_search の変更は次ターン以降に反映される。\
                指示文の変更は update_instructions / update_heartbeat_instructions を使う。\
                省略したフィールドは変更しない。null で解除（既定に戻す。persona_name は解除不可）。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "persona_name": {"type": "string", "description": "ペルソナ名。"},
                        "personality": {"type": ["string", "null"], "description": "性格・思考スタイル。"},
                        "job_title": {"type": ["string", "null"], "description": "肩書き。"},
                        "organization": {"type": ["string", "null"], "description": "所属。"},
                        "model": {"type": ["string", "null"], "description": "既定モデル（provider:model）。次ターン以降に反映。"},
                        "reasoning_effort": {"type": ["string", "null"], "description": "推論強度（low/medium/high 等）。"},
                        "web_search": {"type": ["boolean", "null"], "description": "本文URL読取り/web 検索の有効化。"}
                    }
                }),
            },
            GatewayActionDef {
                name: "configure_mcp_server".to_string(),
                description:
                    "自分の MCP サーバ設定を管理する（owner 限定）。追加/更新・削除・有効切替が\
                でき、変更後は接続をバックグラウンドで貼り直す。env の値は結果に出さない（キー名のみ）。\
                add で env を省略すると既存の env を保持する。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "add", "remove", "set_enabled"],
                            "description": "list=一覧 / add=追加更新 / remove=削除 / set_enabled=有効切替。"
                        },
                        "name": {"type": "string", "description": "サーバ論理名（英数字・_・-、__ 不可）。list 以外で必須。"},
                        "command": {"type": "string", "description": "起動コマンド（add で必須、例: npx）。"},
                        "args": {"type": "array", "items": {"type": "string"}, "description": "起動引数。"},
                        "env": {"type": "object", "description": "追加環境変数（キー→値）。省略で既存保持。値は結果に出さない。"},
                        "trusted_only": {"type": "boolean", "description": "true で owner/trusted のターンのみ露出。"},
                        "enabled": {"type": "boolean", "description": "有効/無効。"}
                    },
                    "required": ["action"]
                }),
            },
            // bootstrap ツール（鍵不要）。送信系（nostr_post 等・鍵前提）とは分離し、
            // transport 非依存で全ターンに露出する。これにより「鍵を作るツールが鍵の
            // ある時しか出ない」循環依存（#141）を解消する。owner 限定にはしない
            // （nsec は返さず・送信もしないので Agent 呼び出しでも安全）。
            GatewayActionDef {
                name: "nostr_generate_key".to_string(),
                description: "新しい Nostr 鍵（keypair）を生成する。任意で vanity prefix（npub の \
                              npub1 以降・bech32 文字のみ。長さ上限は無いが、長いほど探索に時間が \
                              かかる＝3文字程度で即時、それ以上は徐々に長くなる）を指定できる。返るのは公開情報の \
                              npub / pubkey のみ。**秘密鍵(nsec)はサーバ内に安全に保存され、あなた（LLM）\
                              には渡されない**（セキュリティのため）。これは新規 keypair を作るユーティリティ\
                              であり、あなた自身の送信用アイデンティティは変更しない。"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "prefix": {"type": "string", "description": "任意。npub の npub1 以降に前置したい bech32 文字列（長さ上限なし。長いほど探索に時間がかかる, 例: cat）。"}
                    }
                }),
            },
            // 実行中の subtask を停止するツール（#161）。Discord gateway 実装だけに
            // あった cancel_subtask を server-neutral 層へ露出し、web/Nostr/REST でも
            // 自動 dispatch された subtask を停止できるようにする。認可（親セッション/
            // owner 限定）は共有 registry を引く実体（cancel_subtask）が担う。
            GatewayActionDef {
                name: "cancel_subtask".to_string(),
                description: "実行中のサブタスクをキャンセルします。キャンセルできるのは自分のセッションが親のサブタスクのみ（owner は制限なし）。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "subtask_id": {
                            "type": "string",
                            "description": "キャンセルするサブタスクのID（subtask_spawnedイベントから取得）"
                        }
                    },
                    "required": ["subtask_id"]
                }),
            },
            // サブタスクの進捗報告（#175 S1）。Discord gateway 実装だけにあった
            // report_progress を server-neutral 層へ露出し、web / Nostr / REST /
            // heartbeat でもサブエンジンが進捗を報告できるようにする。引数スキーマは
            // Discord 側の定義と同一（sub-engine の system prompt が「subtask_id は
            // 省略可」と案内している契約を保つ）。
            GatewayActionDef {
                name: "report_progress".to_string(),
                description: "サブエンジンからメインエンジンへ進捗を報告します。depth >= 1のサブエンジンのみ使用可能。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "進捗メッセージ"
                        },
                        "subtask_id": {
                            "type": "string",
                            "description": "このサブタスクのID（オプション）"
                        }
                    },
                    "required": ["message"]
                }),
            },
        ]
    }

    /// bootstrap 用の鍵生成（鍵未設定でも実行可能）。実体は `NostaroCli::vanity`
    /// （config 非依存）で、生成した nsec は**サーバ内に 0600 で保存**し LLM には返さない
    /// （npub/pubkey のみ）。process.rs の防御マスク（tool_name==nostr_generate_key）と
    /// bridge の nsec redaction が多層で秘密漏洩を防ぐ。
    async fn nostr_generate_key(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        let prefix = args
            .get("prefix")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .unwrap_or("");
        // 稼働中のマネージャの CLI（binary_path 等の設定を継承）を使う。無ければ既定。
        let cli = self
            .state
            .nostr_manager
            .as_ref()
            .map(|m| m.cli().clone())
            .unwrap_or_default();
        match cli.vanity(prefix).await {
            Ok(k) => match opencrab_nostr::NostaroCli::save_generated_key(&ctx.agent_id, &k) {
                Ok(_) => GatewayActionResult {
                    success: true,
                    // nsec は返さない（サーバ内 0600 保存済み）。npub/pubkey のみ。
                    data: Some(json!({
                        "npub": k.npub,
                        "pubkey": k.pubkey,
                        "note": "新しい鍵を生成しました。秘密鍵(nsec)はサーバ内に安全に保存済みで、セキュリティ上あなた（LLM）には渡していません。共有・言及してよいのは npub までです。",
                    })),
                    error: None,
                },
                Err(e) => err(format!("鍵は生成しましたが保存に失敗しました: {e}")),
            },
            Err(e) => err(format!("nostr_generate_key 失敗: {e}")),
        }
    }

    /// 実行中 subtask の停止（#161）。共有 `SubtaskRegistry` を引き、認可（親セッション/
    /// owner 限定）・abort・除去・親ログ記録を server-neutral の `cancel_subtask` に委ねる。
    /// registry 未配線（`None`）や不在は not found を返す。権限なしは REJECTION_CODE_PREFIX
    /// を付けて拒否として通知する（Discord 実装と同契約）。
    fn cancel_subtask(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        let Some(subtask_id) = args.get("subtask_id").and_then(|v| v.as_str()) else {
            return err("cancel_subtask: 'subtask_id' is required".to_string());
        };
        let Some(registry) = self.subtask_registry.as_ref() else {
            // dispatch 未配線（走行中 subtask を追跡していない）→ 不在扱い。
            return err(format!("cancel_subtask: subtask '{subtask_id}' not found"));
        };
        let is_owner = ctx.caller == GatewayCaller::Owner;
        match neutral_cancel_subtask(
            registry,
            &self.state.db,
            self.completion_sink.as_deref(),
            subtask_id,
            is_owner,
            ctx.session_id.as_deref(),
        ) {
            CancelOutcome::Cancelled => GatewayActionResult {
                success: true,
                data: Some(json!({ "cancelled": true, "subtask_id": subtask_id })),
                error: None,
            },
            CancelOutcome::NotFound => {
                err(format!("cancel_subtask: subtask '{subtask_id}' not found"))
            }
            CancelOutcome::Unauthorized => err(format!(
                "{REJECTION_CODE_PREFIX}cancel_subtask: subtask '{subtask_id}' をこのセッションからキャンセルする権限がありません（親セッションまたは owner のみ）"
            )),
        }
    }

    /// サブタスクの進捗報告（#175 S1）。Discord 実装（`execute_report_progress`）の
    /// transport 非依存な部分を移植したもの。Discord 固有の webhook 送出は Discord 側に
    /// 残る実装が担当する（この own 実装は inner が未実装の経路でのみ走る）。
    ///
    /// 手順は Discord と同一:
    /// 1. `message` 必須 / セッション必須（fail-closed）
    /// 2. 登録簿から subtask を引く（`subtask_id` 明示 → 無ければ session_id で逆引き）
    /// 3. 所有権ゲート（自分自身の subtask か、自分が親のもののみ）
    /// 4. 親セッションログへ `subtask_progress` を記録（本文の永続化はここだけ）
    /// 5. デバウンス後に完了 sink へ `SettleKind::Progress` を通知（メインエンジン再呼び出し）
    async fn report_progress(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        let Some(message) = args.get("message").and_then(|v| v.as_str()) else {
            return err("report_progress: 'message' is required".to_string());
        };
        let message = message.to_string();
        // セッション必須（fail-closed）: 親セッションの解決が session_id に依存する（#36）。
        let current_session_id = match ctx.session_id.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                return err(
                    "report_progress はセッション文脈でのみ実行できます（session_id 不明）"
                        .to_string(),
                );
            }
        };
        let subtask_id_arg = args
            .get("subtask_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let agent_id = ctx.agent_id.clone();

        // 登録簿から (subtask_id, session_id, parent_session_id) を引く。registry 未配線
        // （`None`）は「登録簿に無い」と同じ扱い（= 自己申告として親ログにだけ残す）。
        let subtask_entry: Option<(String, String, String)> =
            self.subtask_registry.as_ref().and_then(|registry| {
                if !subtask_id_arg.is_empty() {
                    registry.get(&subtask_id_arg).map(|e| {
                        (
                            subtask_id_arg.clone(),
                            e.session_id.clone(),
                            e.parent_session_id.clone(),
                        )
                    })
                } else {
                    registry
                        .iter()
                        .find(|e| e.session_id == current_session_id)
                        .map(|e| {
                            (
                                e.key().clone(),
                                e.value().session_id.clone(),
                                e.value().parent_session_id.clone(),
                            )
                        })
                }
            });

        // 所有権ゲート（#64）: subtask_id は LLM 由来の引数なので、呼び出し元セッションの
        // サブタスク（自分自身 = session_id 一致、または自分の子 = parent_session_id 一致）
        // 以外は拒否する。無検証だと他セッションへの進捗ログ書き込み・メインエンジン
        // 再呼び出しを誘発できてしまう。
        if let Some((id, session_id, parent_session_id)) = &subtask_entry {
            if session_id != &current_session_id && parent_session_id != &current_session_id {
                return err(format!(
                    "{REJECTION_CODE_PREFIX}report_progress: subtask '{id}' は呼び出し元セッションのサブタスクではありません"
                ));
            }
        }

        let subtask_id = subtask_entry
            .as_ref()
            .map(|(id, _, _)| id.clone())
            .unwrap_or(subtask_id_arg);
        let parent_session_id = subtask_entry
            .as_ref()
            .map(|(_, _, parent)| parent.clone())
            .unwrap_or_else(|| current_session_id.clone());

        // 進捗本文は親セッションログ（DB）へ永続化する。sink には本文を運ばない
        // （RFC §1.3）ので、受け口が未配線でも本文自体はここで残る。
        if !parent_session_id.is_empty() {
            if let Ok(conn) = self.state.db.lock() {
                let log = opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: agent_id.clone(),
                    session_id: parent_session_id.clone(),
                    log_type: "system".to_string(),
                    content: json!({
                        "type": "subtask_progress",
                        "subtask_id": subtask_id,
                        "message": message,
                        "timestamp": Utc::now().to_rfc3339(),
                    })
                    .to_string(),
                    speaker_id: None,
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                };
                opencrab_db::queries::insert_session_log_best_effort(&conn, &log);
            }
        }

        // 完了受け口が未配線の経路（`with_dispatch` していない run）では、デバウンス
        // タスクを**起動しない**。起動しても 3 秒後に通知先が無く黙って消えるだけで、
        // (a) 無駄な tokio タスクと (b) 世代カウンタの残骸を積むだけだからである。
        // 記録（上の親ログ）は済んでいるので、その旨を debug ログに残して成功を返す。
        let Some(sink) = self.completion_sink.clone() else {
            tracing::debug!(
                session_id = %current_session_id,
                parent_session_id = %parent_session_id,
                subtask_id = %subtask_id,
                "report_progress: completion sink not wired; progress logged to the parent session only (no main-engine notification)"
            );
            return GatewayActionResult {
                success: true,
                data: Some(json!({
                    "reported": true,
                    "message": message,
                    // 記録はしたが再注入はしていないことを呼び出し元に明示する。
                    "notified": false,
                })),
                error: None,
            };
        };

        // デバウンス: 3 秒待ってからメインエンジン再呼び出しを 1 回だけ発火する。
        // 世代カウンタは `AppState` 側（`ProgressDebounce`）にある。この構造体は run
        // ごとに作り直されるためフィールドに置くと毎回リセットされ、バースト時に同数の
        // LLM 再呼び出し（コスト増・チャンネルスパム）が起きる。
        let debounce = self.state.progress_debounce.clone();
        let my_generation = debounce.bump(&parent_session_id);
        let parent_session_clone = parent_session_id.clone();
        let subtask_id_clone = subtask_id.clone();
        let agent_id_clone = agent_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY).await;
            // 自分より後に report_progress が来ていたら（世代が進んでいたら）発火しない。
            if !debounce.claim_latest(&parent_session_clone, my_generation) {
                return;
            }
            sink.on_subtask_settled(SubtaskSettled {
                session_id: parent_session_clone,
                agent_id: agent_id_clone,
                subtask_id: subtask_id_clone,
                exit_reason: "progress".to_string(),
                kind: SettleKind::Progress,
                // 進捗の宛先は親セッション。返信先の復元は sink 側の責務（#167）。
                reply_target: None,
            });
        });

        GatewayActionResult {
            success: true,
            data: Some(json!({
                "reported": true,
                "message": message,
                "notified": true,
            })),
            error: None,
        }
    }

    async fn configure_llm_provider(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("configure_llm_provider requires owner".to_string());
        }
        let Some(provider) = args.get("provider").and_then(|v| v.as_str()) else {
            return err("provider is required".to_string());
        };
        let provider = provider.to_string();

        // LLM 由来の args から許可フィールドだけを抜き出して三値ボディを組む。
        // api_key は意図的に受け付けない（秘密情報を LLM 経路・ログに載せない）。
        let mut body = serde_json::Map::new();
        for key in [
            "enabled",
            "default_model",
            "binary_path",
            "args",
            "working_dir",
            "timeout_secs",
            "reasoning_effort",
            "base_url",
        ] {
            if let Some(v) = args.get(key) {
                body.insert(key.to_string(), v.clone());
            }
        }

        match crate::api::providers::apply_provider_override_with_rollback(
            &self.state,
            &provider,
            &body,
        )
        .await
        {
            Ok(outcome) => {
                let data = json!({
                    "provider": provider,
                    "applied": outcome.applied,
                    "test_ok": outcome.test_ok,
                    "rolled_back": outcome.rolled_back,
                });
                if outcome.rolled_back {
                    // 適用したが起動確認に失敗 → 元に戻した。エージェントに明示的に伝える。
                    GatewayActionResult {
                        success: false,
                        data: Some(data),
                        error: Some(format!(
                            "'{provider}' の設定を適用しましたが起動確認に失敗したため、\
                             直前の設定へ自動ロールバックしました。binary_path/args/working_dir を確認してください。"
                        )),
                    }
                } else {
                    GatewayActionResult {
                        success: true,
                        data: Some(data),
                        error: None,
                    }
                }
            }
            Err((_code, msg)) => err(msg),
        }
    }

    async fn manage_allowed_commands(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("manage_allowed_commands requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());

        let conn = match self.state.db.lock() {
            Ok(c) => c,
            Err(_) => return err("db lock failed".to_string()),
        };
        match action {
            "list" => match opencrab_db::queries::list_agent_allowed_commands(&conn, &agent_id) {
                Ok(cmds) => GatewayActionResult {
                    success: true,
                    data: Some(json!({ "commands": cmds })),
                    error: None,
                },
                Err(e) => err(e.to_string()),
            },
            "add" => {
                let Some(cmd) = command.filter(|s| !s.is_empty()) else {
                    return err("command is required for add".to_string());
                };
                match opencrab_db::queries::add_agent_allowed_command(
                    &conn, &agent_id, &cmd, "owner",
                ) {
                    Ok(added) => GatewayActionResult {
                        success: true,
                        data: Some(json!({ "command": cmd, "added": added })),
                        error: None,
                    },
                    Err(e) => err(e.to_string()),
                }
            }
            "remove" => {
                let Some(cmd) = command.filter(|s| !s.is_empty()) else {
                    return err("command is required for remove".to_string());
                };
                match opencrab_db::queries::remove_agent_allowed_command(&conn, &agent_id, &cmd) {
                    Ok(removed) => GatewayActionResult {
                        success: true,
                        data: Some(json!({ "command": cmd, "removed": removed })),
                        error: None,
                    },
                    Err(e) => err(e.to_string()),
                }
            }
            other => err(format!("unknown action: {other} (list/add/remove)")),
        }
    }

    async fn configure_nostr(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("configure_nostr requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();
        // 既存設定を partial 更新のベースにする（省略フィールドは現状維持）。
        let existing = {
            let conn = match self.state.db.lock() {
                Ok(c) => c,
                Err(_) => return err("db lock failed".to_string()),
            };
            opencrab_db::queries::get_agent_nostr_config(&conn, &agent_id).unwrap_or(None)
        };
        let Some(existing) = existing else {
            return err(
                "Nostr 設定が未作成です。先に鍵を生成してください（operator がダッシュボードで生成）"
                    .to_string(),
            );
        };
        let ef: Value = serde_json::from_str(&existing.filter_json).unwrap_or_else(|_| json!({}));
        // args の配列（文字列）を取り出す。無ければ None（＝現状維持）。
        let arg_strs = |k: &str| -> Option<Vec<String>> {
            args.get(k).and_then(|x| x.as_array()).map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                    .collect()
            })
        };
        let cur_strs = |v: &Value, k: &str| -> Vec<String> {
            v.get(k)
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let arg_or_cur_kinds = || -> Vec<u32> {
            let extract = |v: &Value| -> Vec<u32> {
                v.as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|n| n.as_u64().map(|v| v as u32))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            match args.get("kinds") {
                Some(v) => extract(v),
                None => ef.get("kinds").map(extract).unwrap_or_default(),
            }
        };

        let relays = arg_strs("relays")
            .unwrap_or_else(|| serde_json::from_str(&existing.relays_json).unwrap_or_default());
        let authors = arg_strs("authors").unwrap_or_else(|| cur_strs(&ef, "authors"));
        let keywords = arg_strs("keywords").unwrap_or_else(|| cur_strs(&ef, "keywords"));
        let kinds = arg_or_cur_kinds();
        let enabled = args
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(existing.enabled);

        match crate::api::nostr::apply_nostr_settings(
            &self.state,
            &agent_id,
            &relays,
            &authors,
            &keywords,
            &kinds,
            enabled,
            None,
        )
        .await
        {
            Ok(()) => GatewayActionResult {
                success: true,
                // secret_key は返さない。
                data: Some(json!({
                    "agent_id": agent_id,
                    "relays": relays,
                    "authors": authors,
                    "keywords": keywords,
                    "kinds": kinds,
                    "enabled": enabled,
                })),
                error: None,
            },
            Err((_code, msg)) => err(msg),
        }
    }

    async fn configure_self(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("configure_self requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();

        // 三値: キー欠落=変更しない / null=解除（Some(None）) / 値=設定（Some(Some(v))）。
        let tri_string = |k: &str| -> Option<Option<String>> {
            match args.get(k) {
                None => None,
                Some(Value::Null) => Some(None),
                Some(Value::String(s)) => Some(Some(s.clone())),
                _ => None,
            }
        };
        let tri_bool = |k: &str| -> Option<Option<bool>> {
            match args.get(k) {
                None => None,
                Some(Value::Null) => Some(None),
                Some(Value::Bool(b)) => Some(Some(*b)),
                _ => None,
            }
        };

        let patch = opencrab_db::queries::AgentPatch {
            // persona_name は Option<String>（解除不可）。文字列指定時のみ設定。
            persona_name: args
                .get("persona_name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            personality: tri_string("personality"),
            job_title: tri_string("job_title"),
            organization: tri_string("organization"),
            model: tri_string("model"),
            reasoning_effort: tri_string("reasoning_effort"),
            web_search: tri_bool("web_search"),
            ..Default::default()
        };

        let result = {
            let conn = match self.state.db.lock() {
                Ok(c) => c,
                Err(_) => return err("db lock failed".to_string()),
            };
            opencrab_db::queries::apply_agent_patch(&conn, &agent_id, &patch)
        };
        match result {
            Ok(true) => GatewayActionResult {
                success: true,
                data: Some(json!({ "agent_id": agent_id, "updated": true })),
                error: None,
            },
            Ok(false) => err(format!("agent not found: {agent_id}")),
            Err(e) => err(e.to_string()),
        }
    }

    async fn configure_mcp_server(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("configure_mcp_server requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());

        match action {
            "list" => {
                let servers = {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    match opencrab_db::queries::list_agent_mcp_servers(&conn, &agent_id) {
                        Ok(s) => s,
                        Err(e) => return err(e.to_string()),
                    }
                };
                // env の値は出さず、キー名のみ返す（秘密を LLM 経路に載せない）。
                let list: Vec<Value> = servers
                    .iter()
                    .map(|s| {
                        let env_keys: Vec<String> =
                            serde_json::from_str::<serde_json::Map<String, Value>>(&s.env_json)
                                .map(|m| m.keys().cloned().collect())
                                .unwrap_or_default();
                        let args_arr: Vec<String> =
                            serde_json::from_str(&s.args_json).unwrap_or_default();
                        json!({
                            "name": s.name,
                            "command": s.command,
                            "args": args_arr,
                            "env_keys": env_keys,
                            "trusted_only": s.trusted_only,
                            "enabled": s.enabled,
                        })
                    })
                    .collect();
                GatewayActionResult {
                    success: true,
                    data: Some(json!({ "servers": list })),
                    error: None,
                }
            }
            "add" => {
                let Some(name) = name.filter(|s| !s.is_empty()) else {
                    return err("name is required".to_string());
                };
                if !is_valid_server_name(&name) {
                    return err(
                        "サーバ名は英数字・_・-（1〜64文字、__ を含まない）にしてください"
                            .to_string(),
                    );
                }
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                if command.is_empty() {
                    return err("command が必要です".to_string());
                }
                let args_vec: Vec<String> = args
                    .get("args")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                // 既存を（env 保持・デフォルト継承のため）読む。
                let existing = {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    opencrab_db::queries::get_agent_mcp_server(&conn, &agent_id, &name)
                        .unwrap_or(None)
                };
                // env は空/未指定なら既存を保持（値を伏せているため無変更更新で消さない）。
                let env_json = match args.get("env") {
                    Some(Value::Object(m)) if !m.is_empty() => {
                        serde_json::to_string(m).unwrap_or_else(|_| "{}".to_string())
                    }
                    _ => existing
                        .as_ref()
                        .map(|e| e.env_json.clone())
                        .unwrap_or_else(|| "{}".to_string()),
                };
                let trusted_only = args
                    .get("trusted_only")
                    .and_then(|v| v.as_bool())
                    .unwrap_or_else(|| existing.as_ref().map(|e| e.trusted_only).unwrap_or(false));
                let enabled = args
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or_else(|| existing.as_ref().map(|e| e.enabled).unwrap_or(true));
                let row = opencrab_db::queries::AgentMcpServerRow {
                    agent_id: agent_id.clone(),
                    name: name.clone(),
                    command,
                    args_json: serde_json::to_string(&args_vec)
                        .unwrap_or_else(|_| "[]".to_string()),
                    env_json,
                    trusted_only,
                    enabled,
                };
                {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    if let Err(e) = opencrab_db::queries::upsert_agent_mcp_server(&conn, &row) {
                        return err(e.to_string());
                    }
                }
                crate::api::mcp::spawn_reload(&self.state, agent_id);
                GatewayActionResult {
                    success: true,
                    // env の値は返さない。
                    data: Some(json!({ "name": name, "upserted": true, "enabled": enabled })),
                    error: None,
                }
            }
            "remove" => {
                let Some(name) = name.filter(|s| !s.is_empty()) else {
                    return err("name is required".to_string());
                };
                let removed = {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    match opencrab_db::queries::delete_agent_mcp_server(&conn, &agent_id, &name) {
                        Ok(r) => r,
                        Err(e) => return err(e.to_string()),
                    }
                };
                crate::api::mcp::spawn_reload(&self.state, agent_id);
                GatewayActionResult {
                    success: true,
                    data: Some(json!({ "name": name, "removed": removed })),
                    error: None,
                }
            }
            "set_enabled" => {
                let Some(name) = name.filter(|s| !s.is_empty()) else {
                    return err("name is required".to_string());
                };
                let Some(enabled) = args.get("enabled").and_then(|v| v.as_bool()) else {
                    return err("enabled (bool) is required for set_enabled".to_string());
                };
                {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    if let Err(e) = opencrab_db::queries::set_agent_mcp_server_enabled(
                        &conn, &agent_id, &name, enabled,
                    ) {
                        return err(e.to_string());
                    }
                }
                crate::api::mcp::spawn_reload(&self.state, agent_id);
                GatewayActionResult {
                    success: true,
                    data: Some(json!({ "name": name, "enabled": enabled })),
                    error: None,
                }
            }
            other => err(format!(
                "unknown action: {other} (list/add/remove/set_enabled)"
            )),
        }
    }
}

fn err(msg: String) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg),
    }
}

impl SystemGatewayActions {
    /// own 定義と inner 定義を name で dedup してマージする（own 優先）。
    ///
    /// nostr watch ループ稼働時は inner=NostrGatewayActions も nostr_generate_key を、
    /// Discord では inner が cancel_subtask を定義するため、ここで dedup しないと
    /// ツール一覧に同名が2つ並ぶ（provider が拒否しうる）。`definitions()` の実体を
    /// 静的関数に切り出し、AppState 無しで dedup 契約を単体テストできるようにする（#161）。
    fn merge_definitions(
        mut own: Vec<GatewayActionDef>,
        inner: Option<&Arc<dyn GatewayActions>>,
    ) -> Vec<GatewayActionDef> {
        if let Some(inner) = inner {
            let own_names: std::collections::HashSet<String> =
                own.iter().map(|d| d.name.clone()).collect();
            for d in inner.definitions() {
                if !own_names.contains(&d.name) {
                    own.push(d);
                }
            }
        }
        own
    }
}

#[async_trait]
impl GatewayActions for SystemGatewayActions {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        Self::merge_definitions(Self::own_definitions(), self.inner.as_ref())
    }

    async fn execute(
        &self,
        name: &str,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        match name {
            "configure_llm_provider" => self.configure_llm_provider(args, ctx).await,
            "manage_allowed_commands" => self.manage_allowed_commands(args, ctx).await,
            "configure_nostr" => self.configure_nostr(args, ctx).await,
            "configure_self" => self.configure_self(args, ctx).await,
            "configure_mcp_server" => self.configure_mcp_server(args, ctx).await,
            // bootstrap 鍵生成（鍵未設定でも露出）。inner より先に own が処理する。
            "nostr_generate_key" => self.nostr_generate_key(args, ctx).await,
            // subtask 停止（#161）。transport 固有 gateway（Discord）が cancel_subtask を
            // 実装しているなら、その完全な後始末（aborted webhook 送出・session theme から
            // の task_description 解決）を保つため inner へ委譲する。実装していない
            // transport（web/Nostr/REST）では own が共有 registry を引いて停止する。
            // どちらも同一の共有 registry を叩くため二重にキャンセルされることはない。
            "cancel_subtask" => {
                let inner_handles = self.inner.as_ref().is_some_and(|inner| {
                    inner
                        .definitions()
                        .iter()
                        .any(|d| d.name == "cancel_subtask")
                });
                if inner_handles {
                    // inner が cancel_subtask を実装している（Discord）→ 委譲。
                    self.inner.as_ref().unwrap().execute(name, args, ctx).await
                } else {
                    self.cancel_subtask(args, ctx)
                }
            }
            // subtask 進捗報告（#175 S1）。cancel_subtask と同じ委譲パターン。
            // transport 固有 gateway（Discord）が report_progress を実装しているなら、
            // その固有の後処理（lifecycle webhook への progress 送出）を保つため inner
            // へ委譲する＝ Discord 経路は挙動不変。実装していない transport
            // （web/Nostr/REST/heartbeat）では own が処理する。
            "report_progress" => {
                let inner_handles = self.inner.as_ref().is_some_and(|inner| {
                    inner
                        .definitions()
                        .iter()
                        .any(|d| d.name == "report_progress")
                });
                if inner_handles {
                    self.inner.as_ref().unwrap().execute(name, args, ctx).await
                } else {
                    self.report_progress(args, ctx).await
                }
            }
            // 自分が扱わないツールは inner gateway へ委譲する。
            _ => match &self.inner {
                Some(inner) => inner.execute(name, args, ctx).await,
                None => GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("Unknown action: {name}")),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_definition_shape() {
        let defs = SystemGatewayActions::own_definitions();
        let d = defs
            .iter()
            .find(|d| d.name == "configure_llm_provider")
            .expect("configure_llm_provider must be defined");
        // provider は必須。
        let required = d.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "provider"));
        // 秘密情報 api_key は LLM ツールでは露出しない（ダッシュボード専用）。
        let props = d.parameters["properties"].as_object().unwrap();
        assert!(
            !props.contains_key("api_key"),
            "api_key must not be settable via the agent tool"
        );
        // 起動系フィールドは受け付ける。
        for key in ["binary_path", "args", "working_dir", "timeout_secs"] {
            assert!(props.contains_key(key), "missing property: {key}");
        }
    }

    /// Regression guard for #146: nostr_generate_key must be a *own* definition
    /// (bootstrap tool) so it is exposed on every turn regardless of whether the
    /// nostr watch loop / keys are configured. If someone moves it back into the
    /// key-gated inner NostrGatewayActions bundle, own_definitions loses it and
    /// this test fails — that is the "露出が二度と消えない" guard.
    #[test]
    fn nostr_generate_key_is_always_exposed() {
        let defs = SystemGatewayActions::own_definitions();
        let d = defs
            .iter()
            .find(|d| d.name == "nostr_generate_key")
            .expect("nostr_generate_key must be an own (always-exposed) definition (#146)");
        // vanity 用の任意 prefix パラメータを受ける。
        let props = d.parameters["properties"].as_object().unwrap();
        assert!(
            props.contains_key("prefix"),
            "nostr_generate_key must accept an optional vanity `prefix`"
        );
        // bootstrap ツールは required なし（引数なしでも鍵を作れる）。
        assert!(
            d.parameters.get("required").is_none(),
            "nostr_generate_key must not require any argument"
        );
    }

    /// definitions() dedups own vs inner by name: when the inner gateway also
    /// defines nostr_generate_key (nostr watch loop running), the merged tool
    /// list must still contain exactly one entry (providers reject duplicates).
    #[test]
    fn definitions_dedup_keeps_single_nostr_generate_key() {
        // own_definitions is the source that definitions() starts from; assert it
        // is unique there so the dedup contract holds.
        let defs = SystemGatewayActions::own_definitions();
        let count = defs
            .iter()
            .filter(|d| d.name == "nostr_generate_key")
            .count();
        assert_eq!(
            count, 1,
            "nostr_generate_key must be defined exactly once in own_definitions"
        );
    }

    /// Regression guard for #161: cancel_subtask must be an *own* (server-neutral)
    /// definition so web / Nostr / REST — not just Discord — expose the tool to
    /// stop auto-dispatched subtasks. If it is removed from own_definitions the
    /// tool disappears on every non-Discord transport again — that is the bug this
    /// guards against.
    #[test]
    fn cancel_subtask_is_exposed_in_own_definitions() {
        let defs = SystemGatewayActions::own_definitions();
        let d = defs
            .iter()
            .find(|d| d.name == "cancel_subtask")
            .expect("cancel_subtask must be an own (server-neutral) definition (#161)");
        // subtask_id は必須。
        let required = d.parameters["required"].as_array().unwrap();
        assert!(
            required.iter().any(|v| v == "subtask_id"),
            "cancel_subtask must require subtask_id"
        );
        // own に丁度1件（dedup の source が一意）。
        let count = defs.iter().filter(|d| d.name == "cancel_subtask").count();
        assert_eq!(
            count, 1,
            "cancel_subtask must be defined exactly once in own_definitions"
        );
    }

    /// **fail-closed な dispatch 分類ガード（#152）**。
    ///
    /// `own_definitions()` の全名が「非ブロック dispatch の除外集合（inline）」か
    /// 「意図的な dispatch 可リスト」のどちらか**ちょうど一方**に属することを要求する。
    ///
    /// この gateway は transport 非依存で web / REST / heartbeat の全ターンに載る
    /// （`crates/server/src/process.rs` の合成 executor）のに、Discord / Nostr / core と
    /// 違って分類ガードが無く、6 個中 5 個（`configure_llm_provider` /
    /// `manage_allowed_commands` / `configure_nostr` / `configure_self` /
    /// `configure_mcp_server`）が黙って background 化されていた。実装
    /// （`own_definitions()`）を起点に走査するので、新しい設定ツールを足すと分類を
    /// 明示するまでテストが落ちる。判定基準は
    /// `opencrab_actions::default_non_dispatch_tools` の doc。
    #[test]
    fn server_tools_are_classified_for_dispatch() {
        let names: Vec<String> = SystemGatewayActions::own_definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(!names.is_empty(), "own_definitions が空");
        let non_dispatch = opencrab_actions::default_non_dispatch_tools();

        for name in &names {
            let inline = non_dispatch.contains(name);
            let dispatchable =
                opencrab_actions::SERVER_DISPATCHABLE_ACTIONS.contains(&name.as_str());
            assert!(
                inline ^ dispatchable,
                "{name} の dispatch 分類が未定義（inline={inline}, dispatchable={dispatchable}）。\
                 新しいツールを追加したら opencrab_actions::SERVER_INLINE_ACTIONS か \
                 SERVER_DISPATCHABLE_ACTIONS のどちらかへ入れること（判定基準は \
                 default_non_dispatch_tools の doc / docs/DESIGN.md §4.4）"
            );
        }

        // 逆方向: 定数側に死名が無いこと。
        for name in opencrab_actions::SERVER_INLINE_ACTIONS {
            assert!(
                names.contains(&(*name).to_string()),
                "SERVER_INLINE_ACTIONS の {name} が own_definitions() に無い（死名）"
            );
        }
        for name in opencrab_actions::SERVER_DISPATCHABLE_ACTIONS {
            assert!(
                names.contains(&(*name).to_string()),
                "SERVER_DISPATCHABLE_ACTIONS の {name} が own_definitions() に無い（死名）"
            );
        }
        // 分類は own_definitions() を覆い尽くす。
        assert_eq!(
            opencrab_actions::SERVER_INLINE_ACTIONS.len()
                + opencrab_actions::SERVER_DISPATCHABLE_ACTIONS.len(),
            names.len(),
            "分類集合の合計が own_definitions() の数と一致しない（分類漏れ）"
        );
    }

    /// [P1 回帰] 設定変更ツールは inline（同ターンで結果を返す）。長時間の鍵探索だけが
    /// background。分類定数を経由せず `default_non_dispatch_tools()` の実効値を見る。
    #[test]
    fn config_tools_are_inline_and_key_generation_is_dispatched() {
        let non_dispatch = opencrab_actions::default_non_dispatch_tools();
        for name in [
            "configure_llm_provider",
            "manage_allowed_commands",
            "configure_nostr",
            "configure_self",
            "configure_mcp_server",
            "cancel_subtask",
        ] {
            assert!(
                non_dispatch.contains(name),
                "{name} は background 化してはならない（設定の共有状態書き込み / 一覧の即答）"
            );
        }
        assert!(
            !non_dispatch.contains("nostr_generate_key"),
            "nostr_generate_key は長時間の vanity 探索なので dispatch 対象に残す"
        );
    }

    /// #161: Discord のような inner が cancel_subtask を定義しても、merge 後は
    /// own の1件だけが残る（providers は同名重複を拒否しうる）。merge_definitions を
    /// 直接叩くことで AppState 無しに実コードの dedup 契約を検証する。
    #[test]
    fn merge_definitions_dedups_cancel_subtask_from_inner() {
        use opencrab_gateway::{GatewayActionResult, GatewayCallContext};

        /// cancel_subtask と固有ツールを定義する Discord 風 inner モック。
        struct InnerWithCancel;
        #[async_trait]
        impl GatewayActions for InnerWithCancel {
            fn definitions(&self) -> Vec<GatewayActionDef> {
                vec![
                    GatewayActionDef {
                        name: "cancel_subtask".to_string(),
                        description: "discord cancel".to_string(),
                        parameters: json!({"type": "object"}),
                    },
                    GatewayActionDef {
                        name: "discord_only_tool".to_string(),
                        description: "x".to_string(),
                        parameters: json!({"type": "object"}),
                    },
                ]
            }
            async fn execute(
                &self,
                _name: &str,
                _args: &Value,
                _ctx: &GatewayCallContext,
            ) -> GatewayActionResult {
                GatewayActionResult {
                    success: true,
                    data: None,
                    error: None,
                }
            }
        }

        let inner: Arc<dyn GatewayActions> = Arc::new(InnerWithCancel);
        let merged = SystemGatewayActions::merge_definitions(
            SystemGatewayActions::own_definitions(),
            Some(&inner),
        );
        let cancel_count = merged.iter().filter(|d| d.name == "cancel_subtask").count();
        assert_eq!(
            cancel_count, 1,
            "merge 後も cancel_subtask は1件（own 優先で dedup）"
        );
        // inner 固有ツールは通す（dedup は同名のみ）。
        assert!(merged.iter().any(|d| d.name == "discord_only_tool"));
    }

    // ---- #175 S1: report_progress の gateway 非依存化 ----

    /// Regression guard for #175 S1: report_progress must be an *own* (server-neutral)
    /// definition so web / Nostr / REST / heartbeat — not just Discord — can let a
    /// sub-engine report progress.
    #[test]
    fn report_progress_is_exposed_in_own_definitions() {
        let defs = SystemGatewayActions::own_definitions();
        let d = defs
            .iter()
            .find(|d| d.name == "report_progress")
            .expect("report_progress must be an own (server-neutral) definition (#175 S1)");
        // message は必須 / subtask_id は任意（sub-engine の system prompt の契約）。
        let required = d.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "message"));
        assert!(!required.iter().any(|v| v == "subtask_id"));
        let props = d.parameters["properties"].as_object().unwrap();
        assert!(props.contains_key("subtask_id"));
        // own に丁度 1 件（dedup の source が一意）。
        let count = defs.iter().filter(|d| d.name == "report_progress").count();
        assert_eq!(count, 1);
    }

    /// Discord のような inner が report_progress を定義しても、merge 後は own の
    /// 1 件だけが残る（provider は同名重複を拒否しうる）。
    #[test]
    fn merge_definitions_dedups_report_progress_from_inner() {
        let inner: Arc<dyn GatewayActions> = Arc::new(RecordingInner::new(&["report_progress"]));
        let merged = SystemGatewayActions::merge_definitions(
            SystemGatewayActions::own_definitions(),
            Some(&inner),
        );
        let count = merged
            .iter()
            .filter(|d| d.name == "report_progress")
            .count();
        assert_eq!(
            count, 1,
            "merge 後も report_progress は1件（own 優先で dedup）"
        );
    }

    /// 指定した名前のツールを定義し、`execute` の到達を記録する inner のフェイク。
    struct RecordingInner {
        names: Vec<String>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingInner {
        fn new(names: &[&str]) -> Self {
            Self {
                names: names.iter().map(|s| s.to_string()).collect(),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl GatewayActions for RecordingInner {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            self.names
                .iter()
                .map(|n| GatewayActionDef {
                    name: n.clone(),
                    description: format!("{n} (inner)"),
                    parameters: json!({"type": "object"}),
                })
                .collect()
        }
        async fn execute(
            &self,
            name: &str,
            _args: &Value,
            _ctx: &GatewayCallContext,
        ) -> GatewayActionResult {
            self.calls.lock().unwrap().push(name.to_string());
            GatewayActionResult {
                success: true,
                data: Some(json!({ "reached_inner": name })),
                error: None,
            }
        }
    }

    /// 受け取った settle を記録する `SubtaskCompletionSink`。
    #[derive(Default)]
    struct RecordingSink {
        settled: std::sync::Mutex<Vec<SubtaskSettled>>,
    }

    impl RecordingSink {
        fn settled(&self) -> Vec<SubtaskSettled> {
            self.settled.lock().unwrap().clone()
        }
    }

    impl SubtaskCompletionSink for RecordingSink {
        fn on_subtask_settled(&self, ev: SubtaskSettled) {
            self.settled.lock().unwrap().push(ev);
        }
    }

    /// 走行中扱いの subtask を 1 件だけ持つ registry。
    fn registry_with(
        subtask_id: &str,
        session_id: &str,
        parent_session_id: &str,
    ) -> SubtaskRegistry {
        let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
        registry.insert(
            subtask_id.to_string(),
            opencrab_actions::SpawnedSubtask {
                abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
                session_id: session_id.to_string(),
                parent_session_id: parent_session_id.to_string(),
                agent_id: "agent-x".to_string(),
                label: "job".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: None,
                lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            },
        );
        registry
    }

    fn sub_ctx(session_id: &str) -> GatewayCallContext {
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x")
            .with_session_id(session_id)
            .with_depth(1)
    }

    /// 親セッションログに記録された subtask_progress のメッセージ一覧。
    fn progress_messages(state: &AppState, parent_session_id: &str) -> Vec<String> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, parent_session_id)
            .unwrap()
            .into_iter()
            .filter_map(|row| {
                let v: Value = serde_json::from_str(&row.content).ok()?;
                if v.get("type").and_then(|t| t.as_str()) != Some("subtask_progress") {
                    return None;
                }
                Some(v.get("message")?.as_str()?.to_string())
            })
            .collect()
    }

    /// **非 Discord（inner なし）で report_progress が動く**（#175 S1 の主目的）。
    /// 親ログに本文が残り、デバウンス後に完了受け口へ `Progress` が届く。
    #[tokio::test(start_paused = true)]
    async fn report_progress_works_without_inner_gateway() {
        let state = crate::test_app_state();
        let registry = registry_with("st-1", "subtask-st-1", "web-parent-1");
        let sink = Arc::new(RecordingSink::default());
        let actions = SystemGatewayActions::new(
            state.clone(),
            None,
            Some(registry),
            Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
        );

        let r = actions
            .execute(
                "report_progress",
                &json!({ "message": "halfway there" }),
                &sub_ctx("subtask-st-1"),
            )
            .await;
        assert!(r.success, "error: {:?}", r.error);
        assert_eq!(r.data.as_ref().unwrap()["notified"], json!(true));

        // 本文は親セッションログへ永続化される（sink には運ばない / RFC §1.3）。
        assert_eq!(
            progress_messages(&state, "web-parent-1"),
            vec!["halfway there".to_string()]
        );

        // デバウンス満了後に Progress が 1 本届く。
        tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;
        let settled = sink.settled();
        assert_eq!(settled.len(), 1, "デバウンス後に Progress が 1 本届く");
        assert_eq!(settled[0].kind, SettleKind::Progress);
        assert_eq!(settled[0].session_id, "web-parent-1");
        assert_eq!(settled[0].subtask_id, "st-1");
        assert_eq!(settled[0].exit_reason, "progress");
    }

    /// **Discord（inner あり）では inner へ委譲される**（S1 で Discord 経路は挙動不変）。
    /// own 実装は走らない＝親ログを書かない。
    #[tokio::test]
    async fn report_progress_delegates_to_inner_when_inner_defines_it() {
        let state = crate::test_app_state();
        let inner = Arc::new(RecordingInner::new(&["report_progress", "spawn_subtask"]));
        let registry = registry_with("st-1", "subtask-st-1", "discord-parent-1");
        let sink = Arc::new(RecordingSink::default());
        let actions = SystemGatewayActions::new(
            state.clone(),
            Some(inner.clone() as Arc<dyn GatewayActions>),
            Some(registry),
            Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
        );

        let r = actions
            .execute(
                "report_progress",
                &json!({ "message": "from discord" }),
                &sub_ctx("subtask-st-1"),
            )
            .await;
        assert!(r.success);
        assert_eq!(
            r.data.unwrap()["reached_inner"],
            json!("report_progress"),
            "inner（Discord 実装）へ委譲されなければならない"
        );
        assert_eq!(inner.calls(), vec!["report_progress".to_string()]);
        // own 実装は走っていない（親ログも sink も触っていない）。
        assert!(progress_messages(&state, "discord-parent-1").is_empty());
        assert!(sink.settled().is_empty());
    }

    /// 所有権ゲート: 他人の subtask（自分の session でも親でもない）は拒否する。
    #[tokio::test]
    async fn report_progress_rejects_foreign_subtask() {
        let state = crate::test_app_state();
        let registry = registry_with("st-1", "subtask-st-1", "parent-of-someone-else");
        let sink = Arc::new(RecordingSink::default());
        let actions = SystemGatewayActions::new(
            state.clone(),
            None,
            Some(registry),
            Some(sink as Arc<dyn SubtaskCompletionSink>),
        );

        let r = actions
            .execute(
                "report_progress",
                &json!({ "message": "sneaky", "subtask_id": "st-1" }),
                &sub_ctx("some-other-session"),
            )
            .await;
        assert!(!r.success);
        let e = r.error.unwrap();
        assert!(
            e.starts_with(REJECTION_CODE_PREFIX),
            "権限拒否は構造的マーカー付き: {e}"
        );
        // 他セッションの親ログを汚さない。
        assert!(progress_messages(&state, "parent-of-someone-else").is_empty());
    }

    /// セッション必須ガード（fail-closed）: session_id が無い文脈では実行できない。
    #[tokio::test]
    async fn report_progress_requires_session_context() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);
        let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-x");
        let r = actions
            .execute("report_progress", &json!({ "message": "x" }), &ctx)
            .await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("session_id"));
    }

    /// message は必須。
    #[tokio::test]
    async fn report_progress_requires_message() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);
        let r = actions
            .execute("report_progress", &json!({}), &sub_ctx("subtask-st-1"))
            .await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("'message' is required"));
    }

    /// 完了受け口が未配線なら、記録だけして通知はしない（デバウンスタスクも起動しない）。
    /// 「黙って消える」のを避けるため、結果に `notified: false` を載せる。
    #[tokio::test(start_paused = true)]
    async fn report_progress_records_but_does_not_notify_without_sink() {
        let state = crate::test_app_state();
        let registry = registry_with("st-1", "subtask-st-1", "rest-parent-1");
        let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);

        let r = actions
            .execute(
                "report_progress",
                &json!({ "message": "no sink here" }),
                &sub_ctx("subtask-st-1"),
            )
            .await;
        assert!(r.success);
        assert_eq!(r.data.unwrap()["notified"], json!(false));
        // 記録は残る。
        assert_eq!(
            progress_messages(&state, "rest-parent-1"),
            vec!["no sink here".to_string()]
        );
        // デバウンスタスクを起動していない＝世代カウンタも進んでいない。
        tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;
        assert!(
            !state.progress_debounce.claim_latest("rest-parent-1", 1),
            "受け口未配線ではデバウンス世代を消費しない"
        );
    }

    /// **デバウンス状態が `AppState` 側にあることを固定する回帰テスト（#175 S1 の最重要点）**。
    ///
    /// `SystemGatewayActions` は run ごとに作り直される。デバウンス世代カウンタを
    /// この構造体のフィールドに置くと、2 回目の呼び出しで世代が 0 から張り直され、
    /// **両方の呼び出しが発火する**（＝バーストで LLM を無駄に呼ぶ）。ここでは
    /// 別インスタンスから 2 回報告し、届く `Progress` が 1 本だけであることを固定する。
    #[tokio::test(start_paused = true)]
    async fn progress_debounce_survives_gateway_recreation() {
        let state = crate::test_app_state();
        let registry = registry_with("st-1", "subtask-st-1", "web-parent-1");
        let sink = Arc::new(RecordingSink::default());

        // 1 回目: この run 用の gateway インスタンス。
        let first = SystemGatewayActions::new(
            state.clone(),
            None,
            Some(registry.clone()),
            Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
        );
        assert!(
            first
                .execute(
                    "report_progress",
                    &json!({ "message": "step 1" }),
                    &sub_ctx("subtask-st-1")
                )
                .await
                .success
        );
        drop(first);

        // 2 回目: 別の run（＝別インスタンス）。同じ AppState を共有する。
        let second = SystemGatewayActions::new(
            state.clone(),
            None,
            Some(registry),
            Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
        );
        assert!(
            second
                .execute(
                    "report_progress",
                    &json!({ "message": "step 2" }),
                    &sub_ctx("subtask-st-1")
                )
                .await
                .success
        );

        tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;

        // 本文は 2 件とも親ログへ残る（間引くのは通知だけ）。
        assert_eq!(
            progress_messages(&state, "web-parent-1"),
            vec!["step 1".to_string(), "step 2".to_string()]
        );
        // 通知は最後の 1 本だけ。デバウンス状態をインスタンスのフィールドに移すと 2 本届く。
        let settled = sink.settled();
        assert_eq!(
            settled.len(),
            1,
            "デバウンスは gateway の作り直しを跨いで効かなければならない（AppState 側に置く）。届いた: {settled:?}"
        );
        assert_eq!(settled[0].kind, SettleKind::Progress);
    }
}
