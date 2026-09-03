use std::sync::Arc;

use async_trait::async_trait;
use opencrab_gateway::GatewayActions;

use super::gateway_reject;

/// sub-engine 専用の最小権限 gateway。`sub_engine == Allowed` のアクションだけを
/// inner 実装へ委譲する（#63 / RFC #152 S2）。
///
/// `inner` は合成 gateway（`SystemGatewayActions`）へのハンドル（RFC #152 S2）。
/// これにより sub-engine から server ツール（`nostr_generate_key` 等）へ到達できる。
/// 合成 gateway は自分が扱わないツール（`report_progress` 等）を transport gateway
/// （DiscordGatewayActions）へ委譲するため、registry 照合・デバウンス・完了イベント
/// 送信は親経由の呼び出しと同一に動く（transport は親と同一インスタンスを共有）。
///
/// root_gateway が未注入の経路（後方互換）では、呼び出し側が transport gateway 単体を
/// `Arc<dyn GatewayActions>` として渡す。
pub struct SubEngineGatewayActions {
    inner: Arc<dyn GatewayActions>,
}

impl SubEngineGatewayActions {
    pub fn new(inner: Arc<dyn GatewayActions>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl GatewayActions for SubEngineGatewayActions {
    fn definitions(&self) -> Vec<opencrab_gateway::GatewayActionDef> {
        self.inner
            .definitions()
            .into_iter()
            .filter(|d| d.class.sub_engine == opencrab_gateway::SubEngineAccess::Allowed)
            .collect()
    }

    async fn execute(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &opencrab_gateway::GatewayCallContext,
    ) -> opencrab_gateway::GatewayActionResult {
        // definitions() を 1 回だけ取って使い回す（許可判定と存在判定の両方に使う）。
        let defs = self.inner.definitions();
        let def = defs.iter().find(|d| d.name == name);
        match def {
            // `sub_engine == Allowed` のツールだけ inner へ委譲する。
            Some(d) if d.class.sub_engine == opencrab_gateway::SubEngineAccess::Allowed => {
                self.inner.execute(name, args, ctx).await
            }
            // 実在するが許可外 → 権限拒否（rejected: マーカー）。
            Some(_) => gateway_reject(format!("action '{name}' is not available in sub-engines")),
            // 未知の名前 → 通常の失敗（幻覚ツール名を Rejected に誤分類させない）。
            None => opencrab_gateway::GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("Unknown gateway action: {name}")),
            },
        }
    }
}

#[cfg(test)]
#[path = "tests/subengine.rs"]
mod tests;
