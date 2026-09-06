use super::super::*;
use opencrab_gateway::GatewayCaller;

// ------------------------------------------------------------------
// #268: nostr_run 薄い passthrough（server-own / caller 制限なし・#303）
// ------------------------------------------------------------------

/// `nostr_run` の委譲先を検証する fake passthrough capability。
/// 呼ばれた (agent_id, subcommand, args) を記録し、固定文字列 or エラーを返す。
// #654: nostr_run は nostr feature 依存（#651）。off ではツールが無く、この検証群と
// その helper は意味を持たないので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[derive(Default)]
struct RecordingPassthrough {
    calls: std::sync::Mutex<Vec<(String, String, Vec<String>)>>,
    fail: bool,
}

#[cfg(feature = "nostr")]
#[async_trait]
impl opencrab_actions::GatewayNostrPassthrough for RecordingPassthrough {
    async fn run(
        &self,
        agent_id: &str,
        subcommand: &str,
        args: &[String],
    ) -> anyhow::Result<String> {
        self.calls.lock().unwrap().push((
            agent_id.to_string(),
            subcommand.to_string(),
            args.to_vec(),
        ));
        if self.fail {
            anyhow::bail!("passthrough boom");
        }
        Ok(format!("ran {subcommand}"))
    }
}

/// NOSTR 種別で `nostr_passthrough` capability だけを提供する fake gateway。
#[cfg(feature = "nostr")]
struct FakeNostrGateway {
    passthrough: Arc<RecordingPassthrough>,
}

#[cfg(feature = "nostr")]
#[async_trait]
impl opencrab_actions::AgentGatewayLifecycle for FakeNostrGateway {
    fn kind(&self) -> &'static str {
        opencrab_actions::gateway_kinds::NOSTR
    }
    async fn start(&self, _agent_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&self, _agent_id: &str) {}
    fn is_running(&self, _agent_id: &str) -> bool {
        false
    }
    async fn restore_all(&self) {}
    async fn shutdown_all(&self) {}
    fn nostr_passthrough(&self) -> Option<Arc<dyn opencrab_actions::GatewayNostrPassthrough>> {
        Some(self.passthrough.clone())
    }
}

#[cfg(feature = "nostr")]
fn register_fake_nostr(state: &AppState, fail: bool) -> Arc<RecordingPassthrough> {
    let passthrough = Arc::new(RecordingPassthrough {
        fail,
        ..Default::default()
    });
    state.gateways.register(Arc::new(FakeNostrGateway {
        passthrough: passthrough.clone(),
    }));
    passthrough
}

/// `nostr_run`（薄い nostaro passthrough / #268）は**そもそも使えないように**撤去した
/// （オーナー裁定）。own 定義に無く、名前指定で呼んでも fail-close で拒否される。
///
/// 返信は core の say 一本（gateway が対象ノートへの nostaro reply として投稿する / #840）、
/// 独立投稿は nostr_post。ここは feature の有無に依らず「露出されない」ことを固定する。
#[tokio::test]
async fn nostr_run_is_unexposed_and_fail_closes() {
    let names: Vec<String> = SystemGatewayActions::own_definitions()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert!(
        !names.contains(&"nostr_run".to_string()),
        "nostr_run は露出撤去したので own 定義に無いこと"
    );

    // 名前指定で呼んでも fail-close（黙って成功に見せない）。gateway 未登録でも拒否理由は
    // 「未構成」ではなく「撤去」であること（fail-close が passthrough を引く前に効く）。
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let ctx = GatewayCallContext::new(GatewayCaller::Owner, "agent-x");
    let r = actions
        .execute("nostr_run", &json!({"subcommand": "post"}), &ctx)
        .await;
    assert!(!r.success, "nostr_run は成功してはいけない");
    let msg = r.error.unwrap();
    assert!(msg.contains("撤去"), "拒否理由に「撤去」を含む: {msg}");
}

/// fail-close は passthrough capability が登録されていても**委譲しない**（#268 の委譲配線が
/// 生き残って露出撤去が骨抜きにならないことの回帰）。
// #654: fake passthrough helper（register_fake_nostr 等）は nostr feature 依存なので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn nostr_run_does_not_delegate_even_with_passthrough() {
    let state = crate::test_app_state();
    let rec = register_fake_nostr(&state, false);
    let actions = SystemGatewayActions::new(state, None, None, None);
    // caller=Agent（Nostr 受信ターン相当）でも通らない。
    let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-268");
    let r = actions
        .execute(
            "nostr_run",
            &json!({"subcommand": "timeline", "args": ["--limit", "5"]}),
            &ctx,
        )
        .await;
    assert!(
        !r.success,
        "露出撤去した nostr_run は passthrough があっても拒否される"
    );
    assert!(r.error.unwrap().contains("撤去"));
    // capability は一度も呼ばれていない（委譲していない）。
    assert!(
        rec.calls.lock().unwrap().is_empty(),
        "fail-close なのに passthrough capability が呼ばれている（委譲配線が生き残っている）"
    );
}
