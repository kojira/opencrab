//! A2UI 送信ツール `send_ui` の gateway 非依存な実体（#156 S3 / #157）。
//!
//! 「UI を送ってユーザーの応答を待つ」機構は Discord gateway
//! （`crates/discord/src/gateway_actions/ui.rs`）にしか無かったため、Discord 経由の
//! ターンでしか露出しなかった（#157 の残件）。ここへ移すことで、A2UI の描画面
//! （[`A2uiSurface`]）を提供する transport すべてで同じ実装が使える。
//!
//! transport に残るのは 2 つだけ:
//! - 描画の実装（[`opencrab_core::a2ui::UiRenderer`]）
//! - 応答の受け口（[`UiResponseSink`]。`SubtaskCompletionSink` と同型）
//!
//! 保留状態（[`opencrab_core::a2ui::PendingInteraction`]）は**描画物を持たない**。
//! transport が後で必要とするもの（Discord の Form モーダルの入力欄など）は部品ツリーと
//! `surface_id` から再導出できるため、コアが transport の UI ライブラリの型を知る必要も、
//! それを避けるための型消去も要らない。
//!
//! ## 不変条件（移設で壊してはならないもの）
//! - **セッション必須（fail-closed）**: `session_id` が無い/空なら `""` で登録せず
//!   明示エラー（#36）。`Option` を素通しさせない。
//! - **オーナー限定ゲート**: 保留状態に載せるオーナー識別子は
//!   [`A2uiSurface::owner_id`]。未設定（空文字・空白のみ）なら**誰も操作できない**
//!   （fail-closed, #174）ので、配線側が空文字を渡すと UI が誰にも応答しなくなる。
//!   `owner_only` 引数は DB 列にだけ効く（移設前と同じ）。
//! - **sub-engine からの遮断**: `send_ui` の定義は `class.sub_engine == Blocked` を名乗る
//!   ので、depth>=1 の sub-engine から可視性・実行の両方で遮断される（多層防御）。
//!   `BridgedExecutor` が名前 → `ToolClass` 索引からこの属性を引く。
//! - **本文を運ばない**: 受け口へ渡す [`UiResponseEvent`] には応答本文を再注入する
//!   ための会話テキストを載せない（受け取り側が DB から読み直す）。

use std::sync::Arc;

use opencrab_core::a2ui::{
    A2uiComponent, A2uiSurface, A2uiUserAction, RenderTarget, UiResponseEvent,
};
use opencrab_gateway::{GatewayActionDef, GatewayActionResult, GatewayCallContext};
use serde_json::json;
use tracing::{debug, error, info};

/// `send_ui` のツール定義。
///
/// 名前と引数スキーマ（キー・型・required）は移設前（Discord gateway）から 1 バイトも
/// 変えない。`channel_id` の**説明文だけ**は #158 S2 で transport 中立にした
/// （宛先は今のやりとりのものを渡す、推測させない）。必須であることは変えていない。
pub fn send_ui_definition() -> GatewayActionDef {
    GatewayActionDef {
        name: "send_ui".to_string(),
        class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::Blocked, sharing: opencrab_gateway::ToolSharing::ConversationBound },
        description: "A2UIコンポーネントで構成されたUIを送信し、ユーザーの応答を待機する。\n\n使用例（ボタン）:\n{\"channel_id\": \"123456789\", \"components\": [{\"id\": \"txt1\", \"component\": \"Text\", \"text\": \"選んでください\"}, {\"id\": \"row1\", \"component\": \"Row\", \"children\": [\"btn1\", \"btn2\"]}, {\"id\": \"btn1\", \"component\": \"Button\", \"text\": \"選択A\", \"style\": \"primary\", \"action\": {\"name\": \"choose\", \"context\": {\"value\": \"A\"}}}, {\"id\": \"btn2\", \"component\": \"Button\", \"text\": \"選択B\", \"style\": \"secondary\", \"action\": {\"name\": \"choose\", \"context\": {\"value\": \"B\"}}}]}\n\n使用例（セレクトメニュー）:\n{\"channel_id\": \"123456789\", \"components\": [{\"id\": \"txt1\", \"component\": \"Text\", \"text\": \"モデルを選択\"}, {\"id\": \"col1\", \"component\": \"Column\", \"children\": [\"txt1\", \"sel1\"]}, {\"id\": \"sel1\", \"component\": \"SelectMenu\", \"placeholder\": \"モデルを選んでください\", \"options\": [{\"label\": \"GPT-4\", \"value\": \"gpt-4\"}, {\"label\": \"Claude\", \"value\": \"claude\"}], \"action\": {\"name\": \"select_model\"}}]}\n\n使用例（フォーム/モーダル）:\n{\"channel_id\": \"123456789\", \"components\": [{\"id\": \"col1\", \"component\": \"Column\", \"children\": [\"txt1\", \"row1\"]}, {\"id\": \"txt1\", \"component\": \"Text\", \"text\": \"設定を変更\"}, {\"id\": \"row1\", \"component\": \"Row\", \"children\": [\"trigger_btn\"]}, {\"id\": \"trigger_btn\", \"component\": \"Button\", \"text\": \"設定を開く\", \"style\": \"primary\", \"action\": {\"name\": \"open_form\"}}, {\"id\": \"form1\", \"component\": \"Form\", \"title\": \"設定変更\", \"children\": [\"input_name\", \"input_desc\"], \"action\": {\"name\": \"submit_form\"}}, {\"id\": \"input_name\", \"component\": \"TextInput\", \"label\": \"名前\", \"placeholder\": \"名前を入力\", \"style\": \"short\", \"required\": true}, {\"id\": \"input_desc\", \"component\": \"TextInput\", \"label\": \"説明\", \"placeholder\": \"説明を入力\", \"style\": \"paragraph\", \"required\": false}]}\n\n注意: Rowのchildrenで参照するButton/SelectMenuはトップレベルのcomponents配列に定義する。各Buttonには一意のidとaction（name + context）を設定する。SelectMenuの選択結果はaction.contextにselected_valuesとして返される。Formはモーダル表示用でトリガーボタンが必要。".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "channel_id": {
                    "type": "string",
                    "description": "送信先の宛先ID。今のやりとりの宛先をそのまま渡すこと（推測した識別子を渡してはならない）。"
                },
                "components": {
                    "type": "array",
                    "description": "A2UI v0.9 コンポーネント配列",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "component": { "type": "string", "enum": ["Text", "Button", "Row", "Column", "SelectMenu", "TextInput", "Form"] },
                            "text": { "type": "string" },
                            "variant": { "type": "string" },
                            "label": { "type": "string", "description": "TextInputのラベル" },
                            "title": { "type": "string", "description": "Formのタイトル" },
                            "action": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" },
                                    "context": { "type": "object" }
                                }
                            },
                            "style": { "type": "string" },
                            "emoji": { "type": "string" },
                            "children": { "type": "array", "items": { "type": "string" } },
                            "options": {
                                "type": "array",
                                "description": "SelectMenuの選択肢",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string" },
                                        "value": { "type": "string" },
                                        "description": { "type": "string" },
                                        "emoji": { "type": "string" },
                                        "default": { "type": "boolean" }
                                    },
                                    "required": ["label", "value"]
                                }
                            },
                            "placeholder": { "type": "string" },
                            "min_values": { "type": "integer" },
                            "max_values": { "type": "integer" },
                            "min_length": { "type": "integer" },
                            "max_length": { "type": "integer" },
                            "required": { "type": "boolean" }
                        },
                        "required": ["id", "component"]
                    }
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "タイムアウト秒数（デフォルト: 300）"
                },
                "owner_only": {
                    "type": "boolean",
                    "description": "オーナーのみ操作可能か（デフォルト: true）"
                }
            },
            "required": ["channel_id", "components"]
        }),
    }
}

/// `send_ui` の実体（gateway 非依存）。
///
/// 手順は移設前と同一: セッション検査 → 引数検査 → DB 挿入 → 描画 → message_id 書き戻し
/// → 保留登録 + タイムアウト監視の spawn。
pub async fn send_ui(
    db: &opencrab_db::Db,
    surface: &A2uiSurface,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    // セッション必須（fail-closed）: インタラクション応答のルーティングが
    // session_id に依存するため、不明なまま "" で登録しない（#36）。
    let session_id = match ctx.session_id.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some(
                    "send_ui はセッション文脈でのみ実行できます（session_id 不明）".to_string(),
                ),
            }
        }
    };
    let channel_id = match args.get("channel_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("channel_idパラメータが必要です".to_string()),
            }
        }
    };

    let components_value = match args.get("components") {
        Some(v) => v,
        None => {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("componentsパラメータが必要です".to_string()),
            }
        }
    };

    let components: Vec<A2uiComponent> = match serde_json::from_value(components_value.clone()) {
        Ok(c) => c,
        Err(e) => {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("コンポーネントのパースに失敗: {}", e)),
            }
        }
    };

    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(300)
        .clamp(10, 3600);

    let owner_only = args
        .get("owner_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let interaction_id = uuid::Uuid::new_v4().to_string();
    let surface_id = format!("interaction:{}", interaction_id);

    // Serialize components for DB storage
    let components_json = match serde_json::to_string(&components) {
        Ok(j) => j,
        Err(e) => {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("コンポーネントのシリアライズに失敗: {}", e)),
            }
        }
    };

    // Insert into DB
    {
        let conn = db.lock().unwrap();
        if let Err(e) = opencrab_db::queries::insert_pending_interaction(
            &conn,
            &interaction_id,
            &ctx.agent_id,
            // 再開先のセッション。ここが空だと DB 行から会話へ戻せず、プロセス再起動を
            // 挟んだ応答が宙に浮く（#196）。上の fail-closed 検査を通った値を必ず使う。
            &session_id,
            channel_id,
            None,
            &surface.platform,
            &surface_id,
            &components_json,
            owner_only,
            timeout_secs as i64,
        ) {
            error!("DB insert_pending_interaction failed: {e}");
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("DB error: {}", e)),
            };
        }
    }

    // Render
    let target = RenderTarget {
        channel_id: channel_id.to_string(),
        platform: surface.platform.clone(),
    };

    let rendered = match surface
        .renderer
        .render(&surface_id, &components, &target)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("A2UI render failed: {e}");
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("レンダリングエラー: {}", e)),
            };
        }
    };

    // Update DB with message_id
    if let Some(ref msg_id) = rendered.message_id {
        let conn = db.lock().unwrap();
        if let Err(e) =
            opencrab_db::queries::set_pending_interaction_message_id(&conn, &interaction_id, msg_id)
        {
            error!("DB update message_id failed: {e}");
        }
    }

    // 保留登録 + タイムアウト監視は、応答を受け取れる transport でのみ行う。
    // `pending` が無い構成（イベントループを持たない配線）は描画だけで終わる
    // ＝移設前に `pending_interaction_registry` 未配線だったときと同じ挙動。
    if let Some(pending_surface) = &surface.pending {
        let pending = opencrab_core::a2ui::PendingInteraction {
            session_id: session_id.clone(),
            agent_id: ctx.agent_id.clone(),
            target: target.clone(),
            surface_id: surface_id.clone(),
            a2ui_components: components.clone(),
            // owner の識別子は描画面が保持する値を使う（args 経由では注入されない）。
            // 未設定（空文字・空白のみ）なら誰も操作できない（fail-closed, #174）。
            // ここが空のまま登録すると、その UI は誰の操作にも応答しない。
            owner_id: surface.owner_id.clone(),
            // この UI を描いた run の呼び出し元をそのまま保持する（#298 / #302）。
            // 応答（クリック・タイムアウト）の resume はここから引き継ぐ。
            // 応答者から導出すると、`channel_id` が自由引数で描画先と resume 先が
            // 独立しているせいで昇格経路になる。
            caller: crate::traits::CallerIdentity::from(&ctx.caller),
            created_at: chrono::Utc::now(),
            timeout_secs,
            rendered_message: rendered.clone(),
        };
        pending_surface
            .registry
            .insert(interaction_id.clone(), pending);

        info!(
            interaction_id = %interaction_id,
            surface_id = %surface_id,
            channel_id = %channel_id,
            timeout_secs = %timeout_secs,
            "A2UI interaction registered"
        );

        // Spawn timeout task
        let registry_clone = pending_surface.registry.clone();
        let renderer_clone: Arc<dyn opencrab_core::a2ui::UiRenderer> = surface.renderer.clone();
        let sink_clone = pending_surface.sink.clone();
        let interaction_id_clone = interaction_id.clone();
        let surface_id_clone = surface_id.clone();

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)).await;
            if let Some((_, pending)) = registry_clone.remove(&interaction_id_clone) {
                debug!(
                    interaction_id = %interaction_id_clone,
                    "A2UI interaction timed out"
                );
                let _ = renderer_clone
                    .update_on_timeout(&pending.rendered_message)
                    .await;
                sink_clone.on_ui_response(UiResponseEvent {
                    interaction_id: interaction_id_clone,
                    session_id: pending.session_id.clone(),
                    agent_id: pending.agent_id.clone(),
                    target: pending.target.clone(),
                    response: A2uiUserAction {
                        surface_id: surface_id_clone,
                        component_id: "_timeout".into(),
                        action_name: "timeout".into(),
                        context: None,
                        responder_id: "system".into(),
                    },
                    // タイムアウトも「UI を描いた run の続き」なので、保留エントリの
                    // caller をそのまま運ぶ（#302）。ここで `Agent` へ倒すと、オーナー発の
                    // ターンが「誰も押さなかった」だけで降格する = #298 と同じ症状。
                    // 元が `Agent` のターンなら `Agent` のまま（昇格経路にならない）。
                    caller: pending.caller.clone(),
                });
            }
        });
    }

    GatewayActionResult {
        success: true,
        data: Some(json!({
            "interaction_id": interaction_id,
            "surface_id": surface_id,
            "status": "pending",
            "message": "UIを送信しました。ユーザーの応答を待機中..."
        })),
        error: None,
    }
}

#[cfg(test)]
mod tests;
