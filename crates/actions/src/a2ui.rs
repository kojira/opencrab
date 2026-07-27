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
//!   [`A2uiSurface::owner_id`]。空文字だと判定が無効（誰でも操作可）になるため、
//!   配線側が空文字を渡さないこと。`owner_only` 引数は DB 列にだけ効く（移設前と同じ）。
//! - **sub-engine からの遮断**: `send_ui` は `SUB_ENGINE_ALLOWED_ACTIONS`（許可リスト）
//!   に無く、`DISCORD_ACTIONS`（深さ拒否リスト・名前ベース）に載る。多層防御は移設後も
//!   名前で効く。
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
            // 空文字だと owner 判定が無効になる点は移設前と同じ。
            owner_id: surface.owner_id.clone(),
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
mod tests {
    use super::*;
    use opencrab_core::a2ui::{
        PendingUiSurface, RenderError, RenderedMessage, UiRenderer, UiResponseSink,
        UserActionResponse,
    };
    use opencrab_gateway::GatewayCaller;
    use std::sync::Mutex;

    /// 最小の `UiRenderer` フェイク。描画要求を記録するだけ。
    struct FakeRenderer {
        rendered: Mutex<Vec<(String, String)>>,
    }

    #[async_trait::async_trait]
    impl UiRenderer for FakeRenderer {
        async fn render(
            &self,
            surface_id: &str,
            _components: &[A2uiComponent],
            channel: &RenderTarget,
        ) -> Result<RenderedMessage, RenderError> {
            self.rendered
                .lock()
                .unwrap()
                .push((surface_id.to_string(), channel.channel_id.clone()));
            Ok(RenderedMessage {
                platform: channel.platform.clone(),
                message_id: Some("msg-1".into()),
                channel_id: channel.channel_id.clone(),
            })
        }

        async fn update_on_response(
            &self,
            _rendered: &RenderedMessage,
            _response: &UserActionResponse,
        ) -> Result<(), RenderError> {
            Ok(())
        }

        async fn update_on_timeout(&self, _rendered: &RenderedMessage) -> Result<(), RenderError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<UiResponseEvent>>,
    }

    impl UiResponseSink for RecordingSink {
        fn on_ui_response(&self, ev: UiResponseEvent) {
            self.events.lock().unwrap().push(ev);
        }
    }

    fn surface(with_pending: bool, owner_id: &str) -> (A2uiSurface, Arc<RecordingSink>) {
        let sink = Arc::new(RecordingSink::default());
        let s = A2uiSurface {
            renderer: Arc::new(FakeRenderer {
                rendered: Mutex::new(Vec::new()),
            }),
            platform: "fake".to_string(),
            owner_id: owner_id.to_string(),
            pending: with_pending.then(|| PendingUiSurface {
                registry: Arc::new(dashmap::DashMap::new()),
                sink: sink.clone(),
            }),
        };
        (s, sink)
    }

    fn ctx_with_session() -> GatewayCallContext {
        GatewayCallContext::new(GatewayCaller::Owner, "a1").with_session_id("sess-1")
    }

    fn text_component() -> serde_json::Value {
        json!([{ "id": "t1", "component": "Text", "text": "hi" }])
    }

    #[tokio::test]
    async fn send_ui_without_session_fails_closed() {
        let db = opencrab_db::Db::memory().unwrap();
        let (s, _sink) = surface(true, "owner1");
        let ctx = GatewayCallContext::new(GatewayCaller::Owner, "a1");
        let r = send_ui(
            &db,
            &s,
            &json!({"channel_id": "1", "components": text_component()}),
            &ctx,
        )
        .await;
        assert!(!r.success);
        assert_eq!(
            r.error.unwrap(),
            "send_ui はセッション文脈でのみ実行できます（session_id 不明）"
        );

        // 空文字の session_id も同じく拒否する（"" で登録しない）。
        let ctx = GatewayCallContext::new(GatewayCaller::Owner, "a1").with_session_id("");
        let r = send_ui(
            &db,
            &s,
            &json!({"channel_id": "1", "components": text_component()}),
            &ctx,
        )
        .await;
        assert!(!r.success);
    }

    #[tokio::test]
    async fn send_ui_requires_channel_and_components() {
        let db = opencrab_db::Db::memory().unwrap();
        let (s, _sink) = surface(true, "owner1");
        let ctx = ctx_with_session();

        let r = send_ui(&db, &s, &json!({"components": text_component()}), &ctx).await;
        assert_eq!(r.error.unwrap(), "channel_idパラメータが必要です");

        let r = send_ui(&db, &s, &json!({"channel_id": "1"}), &ctx).await;
        assert_eq!(r.error.unwrap(), "componentsパラメータが必要です");
    }

    #[tokio::test]
    async fn send_ui_registers_pending_with_core_types_only() {
        let db = opencrab_db::Db::memory().unwrap();
        let (s, _sink) = surface(true, "owner-42");
        let ctx = ctx_with_session();

        let r = send_ui(
            &db,
            &s,
            &json!({"channel_id": "555", "components": text_component()}),
            &ctx,
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        let interaction_id = data["interaction_id"].as_str().unwrap().to_string();
        assert_eq!(
            data["surface_id"].as_str().unwrap(),
            format!("interaction:{interaction_id}")
        );
        assert_eq!(data["status"], "pending");
        assert_eq!(
            data["message"],
            "UIを送信しました。ユーザーの応答を待機中..."
        );

        let reg = &s.pending.as_ref().unwrap().registry;
        let pending = reg.get(&interaction_id).expect("registered");
        assert_eq!(pending.session_id, "sess-1");
        assert_eq!(pending.agent_id, "a1");
        assert_eq!(pending.target.channel_id, "555");
        assert_eq!(pending.target.platform, "fake");
        // owner 識別子は描画面の値。空文字を渡すと判定が無効になるので固定する。
        assert_eq!(pending.owner_id, "owner-42");

        // DB へは platform 列付きで永続化され、message_id が書き戻る。
        let conn = db.lock().unwrap();
        let row = opencrab_db::queries::get_pending_interaction(&conn, &interaction_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.platform, "fake");
        assert_eq!(row.channel_id, "555");
        assert_eq!(row.message_id.as_deref(), Some("msg-1"));
        // 再開先のセッションは DB 行にも入る（#196）。ここが空だとプロセス再起動後に
        // 「どの会話へ戻すか」が引けない。
        assert_eq!(row.session_id, "sess-1");
        assert!(row.owner_only);
        assert_eq!(row.timeout_secs, 300);
    }

    #[tokio::test]
    async fn send_ui_without_pending_surface_only_renders() {
        let db = opencrab_db::Db::memory().unwrap();
        let (s, _sink) = surface(false, "owner1");
        let ctx = ctx_with_session();
        let r = send_ui(
            &db,
            &s,
            &json!({"channel_id": "9", "components": text_component()}),
            &ctx,
        )
        .await;
        assert!(r.success);
        assert!(s.pending.is_none());
    }

    #[tokio::test]
    async fn send_ui_clamps_timeout_and_reads_owner_only() {
        let db = opencrab_db::Db::memory().unwrap();
        let (s, _sink) = surface(true, "o");
        let ctx = ctx_with_session();
        let r = send_ui(
            &db,
            &s,
            &json!({
                "channel_id": "1",
                "components": text_component(),
                "timeout_secs": 999999,
                "owner_only": false,
            }),
            &ctx,
        )
        .await;
        let id = r.data.unwrap()["interaction_id"]
            .as_str()
            .unwrap()
            .to_string();
        let reg = &s.pending.as_ref().unwrap().registry;
        assert_eq!(reg.get(&id).unwrap().timeout_secs, 3600);
        let conn = db.lock().unwrap();
        let row = opencrab_db::queries::get_pending_interaction(&conn, &id)
            .unwrap()
            .unwrap();
        assert!(!row.owner_only);
        assert_eq!(row.timeout_secs, 3600);
        // `owner_only=false` でも保留状態の owner 識別子は落とさない（移設前と同じ）。
        assert_eq!(reg.get(&id).unwrap().owner_id, "o");
    }

    /// 保留状態は**描画物を持たず部品ツリーを持つ**。transport（Discord の Form
    /// モーダル等）は応答時にここから描画物を組み直せるので、コアが transport の UI
    /// ライブラリの型を知る必要も、型消去も要らない。
    #[tokio::test]
    async fn pending_state_keeps_the_component_tree_not_a_render() {
        let db = opencrab_db::Db::memory().unwrap();
        let (s, _sink) = surface(true, "o");
        let ctx = ctx_with_session();
        let components = json!([
            { "id": "b1", "component": "Button", "text": "open", "action": { "name": "go" } },
            { "id": "f1", "component": "Form", "title": "T", "children": ["i1"], "action": { "name": "go" } },
            { "id": "i1", "component": "TextInput", "label": "L" },
        ]);
        let r = send_ui(
            &db,
            &s,
            &json!({"channel_id": "1", "components": components}),
            &ctx,
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let id = r.data.unwrap()["interaction_id"]
            .as_str()
            .unwrap()
            .to_string();
        let reg = &s.pending.as_ref().unwrap().registry;
        let pending = reg.get(&id).unwrap();

        // 再導出の材料が揃っている: 部品ツリーと surface_id。
        assert_eq!(pending.surface_id, format!("interaction:{id}"));
        let ids: Vec<&str> = pending
            .a2ui_components
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, vec!["b1", "f1", "i1"]);
        assert!(matches!(
            pending
                .a2ui_components
                .iter()
                .find(|c| c.id == "f1")
                .map(|c| &c.component_type),
            Some(opencrab_core::a2ui::A2uiComponentType::Form { .. })
        ));
    }

    #[tokio::test]
    async fn timeout_fires_sink_and_removes_registration() {
        let db = opencrab_db::Db::memory().unwrap();
        let (s, sink) = surface(true, "o");
        let ctx = ctx_with_session();
        let r = send_ui(
            &db,
            &s,
            // clamp の下限 10 秒まで縮められるが、テストでは登録解除を手で行う。
            &json!({"channel_id": "77", "components": text_component(), "timeout_secs": 10}),
            &ctx,
        )
        .await;
        let id = r.data.unwrap()["interaction_id"]
            .as_str()
            .unwrap()
            .to_string();

        // タイムアウト経路と同じ内容を直接検証する（sleep させない）。
        let reg = s.pending.as_ref().unwrap().registry.clone();
        let (_, pending) = reg.remove(&id).unwrap();
        sink.on_ui_response(UiResponseEvent {
            interaction_id: id.clone(),
            session_id: pending.session_id.clone(),
            agent_id: pending.agent_id.clone(),
            target: pending.target.clone(),
            response: A2uiUserAction {
                surface_id: pending.surface_id.clone(),
                component_id: "_timeout".into(),
                action_name: "timeout".into(),
                context: None,
                responder_id: "system".into(),
            },
        });
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id, "sess-1");
        assert_eq!(events[0].target.channel_id, "77");
        assert_eq!(events[0].response.action_name, "timeout");
        assert_eq!(events[0].response.responder_id, "system");
        assert!(reg.get(&id).is_none());
    }

    /// プロセス再起動を模す: メモリ上の登録簿は空、DB には `pending` の行だけがある。
    ///
    /// この状態で起動時の掃除を走らせると、行は**期限切れとして明示的に閉じられ**、
    /// 閉じた記録から再開先のセッションが引ける（#196）。ボタン押下がどこにも届かない
    /// まま行が `pending` で残り続けることはない。
    #[tokio::test]
    async fn stale_rows_are_closed_with_their_session_after_a_restart() {
        let db = opencrab_db::Db::memory().unwrap();
        let (s, _sink) = surface(true, "o");
        let ctx = ctx_with_session();
        let r = send_ui(
            &db,
            &s,
            &json!({"channel_id": "42", "components": text_component()}),
            &ctx,
        )
        .await;
        let id = r.data.unwrap()["interaction_id"]
            .as_str()
            .unwrap()
            .to_string();

        // 再起動 = メモリ上の登録簿が消える（DB 行だけが残る）。
        let registry = s.pending.as_ref().unwrap().registry.clone();
        registry.clear();
        assert!(registry.get(&id).is_none());

        let conn = db.lock().unwrap();
        let closed = opencrab_db::queries::cleanup_stale_pending_interactions(&conn).unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, id);
        // ここが #196 の要: 閉じた行から再開先のセッションが引ける。
        assert_eq!(closed[0].session_id, "sess-1");
        assert_eq!(closed[0].agent_id, "a1");
        assert_eq!(closed[0].platform, "fake");
        assert_eq!(closed[0].channel_id, "42");

        let row = opencrab_db::queries::get_pending_interaction(&conn, &id)
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "timeout");
    }

    #[test]
    fn send_ui_definition_is_stable() {
        let def = send_ui_definition();
        assert_eq!(def.name, "send_ui");
        assert!(def
            .description
            .starts_with("A2UIコンポーネントで構成されたUIを送信し"));
        assert_eq!(
            def.parameters["required"],
            json!(["channel_id", "components"])
        );
    }

    /// `A2uiUserAction` の型が `UiResponseEvent` の一部として運ばれることの確認と、
    /// **本文（再注入テキスト）を持たない**ことの構造的な固定（#152 の二重返信対策と
    /// 同じ契約）。フィールドを足すとこのテストのフィールド網羅が落ちる。
    #[test]
    fn ui_response_event_carries_no_reply_body() {
        let ev = UiResponseEvent {
            interaction_id: "i".into(),
            session_id: "s".into(),
            agent_id: "a".into(),
            target: RenderTarget {
                channel_id: "c".into(),
                platform: "p".into(),
            },
            response: A2uiUserAction {
                surface_id: "sf".into(),
                component_id: "cid".into(),
                action_name: "an".into(),
                context: None,
                responder_id: "r".into(),
            },
        };
        // 分解束縛で全フィールドを列挙する。本文フィールドを足すとここが落ちる。
        let UiResponseEvent {
            interaction_id,
            session_id,
            agent_id,
            target,
            response,
        } = ev;
        assert_eq!(interaction_id, "i");
        assert_eq!(session_id, "s");
        assert_eq!(agent_id, "a");
        assert_eq!(target.platform, "p");
        assert_eq!(response.action_name, "an");
    }
}
