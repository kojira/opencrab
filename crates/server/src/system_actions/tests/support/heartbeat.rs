use super::super::super::*;
use opencrab_gateway::GatewayCaller;

// ================================================================================
// #247 段階 2 / #456 PR3: エージェント自身のハートビート設定ツール（セッション単位）
// ================================================================================

/// 境界値を固定した state（下限 300 / 既定 1800）。live G は既定 false。
pub(crate) fn heartbeat_state() -> AppState {
    let mut state = crate::test_app_state();
    state.heartbeat_limits = crate::config::HeartbeatLimits {
        default_interval_secs: 1800,
        min_interval_secs: 300,
    };
    state
}

/// live G（global heartbeat kill-switch）を固定した state。`discord-` のゲート理由の検証用。
// #654: 使うのは discord_ctx を立てる G ゲート検証（discord feature 依存・#651）だけなので同じ cfg で囲む。
#[cfg(feature = "discord")]
pub(crate) fn heartbeat_state_with_g(g: bool) -> AppState {
    let mut state = heartbeat_state();
    state.heartbeat_config_rx =
        crate::disconnected_heartbeat_config_rx(opencrab_core::heartbeat::HeartbeatConfig {
            interval_secs: 7,
            enabled: g,
        });
    state
}

/// 現在セッションを Nostr（`nostr-{agent}`）にした ctx（信頼済み呼び出し元）。
/// agent_id `agent-x` はハイフンを含むが、resolve は保存済み agent_id で剥がすので割れない。
pub(crate) fn nostr_ctx() -> GatewayCallContext {
    let mut c = GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x");
    c.session_id = Some("nostr-agent-x".to_string());
    c
}

/// 現在セッションを Discord チャンネル（`discord-{agent}-{guild}-{channel}`）にした ctx。
// #654: discord セッションの発火経路（DiscordFire）は discord feature 時のみ登録される（#651）。
// この ctx を使う test は同じ cfg で囲まれているので helper も揃える。
#[cfg(feature = "discord")]
pub(crate) fn discord_ctx() -> GatewayCallContext {
    let mut c = GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x");
    c.session_id = Some("discord-agent-x-100-200".to_string());
    c
}
