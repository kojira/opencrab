//! A2UI send_ui アクション

use opencrab_core::a2ui::*;
use opencrab_gateway::GatewayActionResult;
use serde_json::json;
use tracing::{debug, error, info};

use super::DiscordGatewayActions;
use crate::renderer::DiscordRenderer;

impl DiscordGatewayActions {
    pub(crate) async fn execute_send_ui(&self, args: &serde_json::Value) -> GatewayActionResult {
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

        let components: Vec<A2uiComponent> = match serde_json::from_value(components_value.clone())
        {
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
            let conn = self.db.lock().unwrap();
            if let Err(e) = opencrab_db::queries::insert_pending_interaction(
                &conn,
                &interaction_id,
                &self.agent_id,
                "", // session_id - will be set from context
                channel_id,
                None,
                "discord",
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
        let renderer = DiscordRenderer::new(self.http.clone());
        let target = RenderTarget {
            channel_id: channel_id.to_string(),
            platform: "discord".to_string(),
        };

        let rendered = match renderer.render(&surface_id, &components, &target).await {
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
            let conn = self.db.lock().unwrap();
            if let Err(e) = conn.execute(
                "UPDATE pending_interactions SET message_id = ?1, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![msg_id, interaction_id],
            ) {
                error!("DB update message_id failed: {e}");
            }
        }

        // Register in PendingInteractionRegistry and spawn timeout
        if let Some(ref registry) = self.pending_interaction_registry {
            let channel_id_num: u64 = channel_id.parse().unwrap_or(0);

            let pending = crate::PendingInteraction {
                session_id: args
                    .get("__session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                agent_id: self.agent_id.clone(),
                channel_id: channel_id_num,
                channel_id_str: channel_id.to_string(),
                is_dm: false,
                surface_id: surface_id.clone(),
                a2ui_components: components.clone(),
                owner_discord_id: args
                    .get("__owner_discord_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                created_at: chrono::Utc::now(),
                timeout_secs,
                rendered_message: rendered.clone(),
                event_tx: self.event_tx.clone().unwrap(),
            };
            registry.insert(interaction_id.clone(), pending);

            info!(
                interaction_id = %interaction_id,
                surface_id = %surface_id,
                channel_id = %channel_id,
                timeout_secs = %timeout_secs,
                "A2UI interaction registered"
            );

            // Spawn timeout task
            let registry_clone = registry.clone();
            let renderer_clone = DiscordRenderer::new(self.http.clone());
            let interaction_id_clone = interaction_id.clone();
            let surface_id_clone = surface_id.clone();
            let event_tx = self.event_tx.clone().unwrap();

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
                    let _ = event_tx.send(crate::message_loop::LoopEvent::InteractionResponse {
                        interaction_id: interaction_id_clone,
                        session_id: pending.session_id.clone(),
                        agent_id: pending.agent_id.clone(),
                        channel_id: pending.channel_id,
                        channel_id_str: pending.channel_id_str.clone(),
                        response: A2uiUserAction {
                            surface_id: surface_id_clone,
                            component_id: "_timeout".into(),
                            action_name: "timeout".into(),
                            context: None,
                            responder_id: "system".into(),
                        },
                        is_dm: pending.is_dm,
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
}
