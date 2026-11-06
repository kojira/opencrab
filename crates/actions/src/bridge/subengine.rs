use std::sync::Arc;

use async_trait::async_trait;
use opencrab_gateway::GatewayActions;

use super::gateway_reject;

/// sub-engine（サブタスク）用の gateway。サブタスクもエージェント本人なので（D-1119・
/// オーナー裁定「サブタスクも本人なのに」）、親ターンと同じ gateway 操作を見せて実行させる。
/// 除くのは定義が `sub_engine == Blocked` を名乗るものだけ（[`sub_engine_may_use`]）。
/// 旧仕様（#63 / RFC #152 S2）は `Allowed` だけを通す最小権限で、gateway の投稿・返信・
/// 読み取りがサブタスクから消えていた。
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

/// サブタスク（depth>=1）がこの属性の操作を使えるか。可視性（`definitions`）・実行
/// （`execute`）・`BridgedExecutor` の depth ゲートが共有する唯一の規則（D-1119）。
pub fn sub_engine_may_use(access: opencrab_gateway::SubEngineAccess) -> bool {
    access != opencrab_gateway::SubEngineAccess::Blocked
}

#[async_trait]
impl GatewayActions for SubEngineGatewayActions {
    fn definitions(&self) -> Vec<opencrab_gateway::GatewayActionDef> {
        self.inner
            .definitions()
            .into_iter()
            .filter(|d| sub_engine_may_use(d.class.sub_engine))
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
            // `Blocked` 以外は親ターンと同じく inner へ委譲する。
            Some(d) if sub_engine_may_use(d.class.sub_engine) => {
                self.inner.execute(name, args, ctx).await
            }
            // 実在するが `Blocked` → 権限拒否（rejected: マーカー）。
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
