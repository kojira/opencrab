/// DESIGN-DISCORD-GATE §8.1 の二重受信防止 lever の liveness 側。live かつ acked binding を持つ
/// instance の kind_id/agent を platform 非依存に照合する。DB enabled ではなく live registry が正。
#[tokio::test]
async fn agent_has_live_gateway_reflects_v3_liveness() {
    let h = Harness::start().await;
    // 生きた gateway が無いうちは false（共有側が処理を続ける）。
    assert!(!h.state.agent_has_live_gateway("agent-1", "discord"));

    // bind ack 済みの live gateway（kind=discord, agent=agent-1）を立てる。stream は保持する。
    let (_s, _instance_id, _binding_id) = ready_pair(&h).await;
    assert!(
        h.state.agent_has_live_gateway("agent-1", "discord"),
        "acked live discord gateway が true にならない"
    );
    // kind 違い・agent 違いは false（join が platform 非依存に効く）。
    assert!(!h.state.agent_has_live_gateway("agent-1", "nostr"));
    assert!(!h.state.agent_has_live_gateway("other-agent", "discord"));

    // 切断すると live entry が消え false へ戻る（enabled フラグではなく生死で判定・#40）。
    drop(_s);
    let mut gone = false;
    for _ in 0..100 {
        if !h.state.agent_has_live_gateway("agent-1", "discord") {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        gone,
        "切断後も live 扱いのまま（liveness が生死を反映しない）"
    );
}

