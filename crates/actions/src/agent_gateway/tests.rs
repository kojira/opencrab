use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 復元が呼ばれた順を記録する共有ログ（PR5 の順序検査用）。
type OrderLog = Arc<std::sync::Mutex<Vec<&'static str>>>;

/// ネットワークに出ない偽マネージャ（呼ばれた回数だけ数える）。
struct FakeGateway {
    kind: &'static str,
    running: Vec<String>,
    started: AtomicUsize,
    stopped: AtomicUsize,
    restored: AtomicUsize,
    shutdown: AtomicUsize,
    /// `restore_all` が呼ばれた順を記録する先（未設定なら記録しない）。
    restore_order: Option<OrderLog>,
}

impl FakeGateway {
    fn new(kind: &'static str, running: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            kind,
            running: running.iter().map(|s| s.to_string()).collect(),
            started: AtomicUsize::new(0),
            stopped: AtomicUsize::new(0),
            restored: AtomicUsize::new(0),
            shutdown: AtomicUsize::new(0),
            restore_order: None,
        })
    }

    /// 復元順を共有ログへ記録する偽マネージャ。
    fn with_order_log(kind: &'static str, log: &OrderLog) -> Arc<Self> {
        Arc::new(Self {
            kind,
            running: vec![],
            started: AtomicUsize::new(0),
            stopped: AtomicUsize::new(0),
            restored: AtomicUsize::new(0),
            shutdown: AtomicUsize::new(0),
            restore_order: Some(log.clone()),
        })
    }
}

#[async_trait]
impl AgentGatewayLifecycle for FakeGateway {
    fn kind(&self) -> &'static str {
        self.kind
    }
    async fn start(&self, _agent_id: &str) -> Result<()> {
        self.started.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn stop(&self, _agent_id: &str) {
        self.stopped.fetch_add(1, Ordering::SeqCst);
    }
    fn is_running(&self, agent_id: &str) -> bool {
        self.running.iter().any(|a| a == agent_id)
    }
    async fn restore_all(&self) {
        self.restored.fetch_add(1, Ordering::SeqCst);
        if let Some(log) = &self.restore_order {
            log.lock().unwrap().push(self.kind);
        }
    }
    async fn shutdown_all(&self) {
        self.shutdown.fetch_add(1, Ordering::SeqCst);
    }
}

/// 登録した順で引ける（順序が保たれる = PR5 が既存の起動順を再現できる）。
#[test]
fn keeps_registration_order() {
    let registry = AgentGatewayRegistry::new();
    registry.register(FakeGateway::new(kinds::DISCORD, &[]));
    registry.register(FakeGateway::new(kinds::NOSTR, &[]));
    assert_eq!(registry.kinds(), vec![kinds::DISCORD, kinds::NOSTR]);
    assert_eq!(registry.all().len(), 2);
    assert!(registry.get(kinds::DISCORD).is_some());
    assert!(registry.get("mcp").is_none(), "MCP は登録簿に入れない");
}

/// 同じ種別の再登録は置き換え（重複して 2 本走らせない）。順序は変わらない。
#[test]
fn re_registering_same_kind_replaces_in_place() {
    let registry = AgentGatewayRegistry::new();
    registry.register(FakeGateway::new(kinds::DISCORD, &["a"]));
    registry.register(FakeGateway::new(kinds::NOSTR, &[]));
    registry.register(FakeGateway::new(kinds::DISCORD, &["b"]));

    assert_eq!(registry.kinds(), vec![kinds::DISCORD, kinds::NOSTR]);
    assert!(!registry.is_running(kinds::DISCORD, "a"), "古い方は捨てる");
    assert!(registry.is_running(kinds::DISCORD, "b"));
}

/// **未登録の種別は false**（共有ゲートウェイが処理を続ける側へ倒す）。
#[test]
fn is_running_is_false_for_unregistered_kind() {
    let registry = AgentGatewayRegistry::new();
    assert!(
        !registry.is_running(kinds::DISCORD, "crab"),
        "未登録で true に倒すと二重処理になる"
    );
    registry.register(FakeGateway::new(kinds::NOSTR, &["crab"]));
    assert!(!registry.is_running(kinds::DISCORD, "crab"));
    assert!(registry.is_running(kinds::NOSTR, "crab"));
    assert!(
        !registry.is_running(kinds::NOSTR, "other"),
        "稼働していないエージェントも false"
    );
}

/// **ロックが poison しても登録簿は答え続ける。**
///
/// 生存確認はルーティング判定なので、ここで panic すると受信処理が止まる
/// （「共有側が続ける」ではなく「誰も処理しない」）。`unwrap()` だとこのテストは
/// 落ちる。
#[test]
fn survives_a_poisoned_lock() {
    let registry = Arc::new(AgentGatewayRegistry::new());
    registry.register(FakeGateway::new(kinds::NOSTR, &["crab"]));

    // 書きガードを持ったまま panic させて poison させる。
    let poisoner = registry.clone();
    let joined = std::thread::spawn(move || {
        let _guard = poisoner.gateways.write().unwrap();
        panic!("ロック下で panic");
    })
    .join();
    assert!(joined.is_err(), "poison させるために panic させている");
    assert!(registry.gateways.is_poisoned());

    // 読み取り系はすべて答え続ける。
    assert!(registry.is_running(kinds::NOSTR, "crab"));
    assert!(!registry.is_running(kinds::DISCORD, "crab"));
    assert_eq!(registry.kinds(), vec![kinds::NOSTR]);
    assert_eq!(registry.all().len(), 1);
    assert!(registry.get(kinds::NOSTR).is_some());
    // 追加登録（書き込み）も通る。
    registry.register(FakeGateway::new(kinds::DISCORD, &[]));
    assert_eq!(registry.kinds(), vec![kinds::NOSTR, kinds::DISCORD]);
}

/// 「設定どおり起動しなかった」失敗を、本当の起動失敗と**取り違えない**。
///
/// 呼び出し側はこの判定で「以前と同じく黙って何もしない」を保つ（起動条件を
/// 満たさないだけの見送りを error ログや異常扱いにしない）。素の `anyhow` エラーが
/// 誤って `true` になると、本物の起動失敗が握り潰される。
#[test]
fn start_declined_is_distinguishable_from_a_real_failure() {
    let declined = StartDeclined::err(kinds::DISCORD, "crab", "enabled=false");
    assert!(is_start_declined(&declined));
    let text = declined.to_string();
    assert!(
        text.contains("crab"),
        "どのエージェントか分かること: {text}"
    );
    assert!(
        text.contains("enabled=false"),
        "どの条件で弾いたか分かること: {text}"
    );

    let real_failure = anyhow::anyhow!("connection refused");
    assert!(
        !is_start_declined(&real_failure),
        "本物の起動失敗を見送り扱いにすると、起動できない状態が無音になる"
    );
}

/// **capability を実装しない transport は `None` を返す**（既定が拒否側）。
///
/// `FakeGateway` は 2 つの accessor をどちらも override していない。上位はここが
/// `None` のとき「その機能は無い」として扱う（ツール実行の実体なし / 鍵を作れない）。
/// 既定を `Some` 相当にすると、持たない transport が呼ばれて異常終了する。
#[test]
fn capability_accessors_default_to_none() {
    let registry = AgentGatewayRegistry::new();
    registry.register(FakeGateway::new(kinds::NOSTR, &["crab"]));
    let gw = registry.get(kinds::NOSTR).unwrap();

    assert!(
        gw.gateway_actions_for("crab").is_none(),
        "実装しない transport はツール実行の実体を持たない"
    );
    assert!(
        gw.key_provisioning().is_none(),
        "実装しない transport は鍵を払い出せない"
    );
    assert!(
        gw.nostr_passthrough().is_none(),
        "実装しない transport は CLI passthrough を持たない"
    );
}

/// **未登録の種別からは capability も引けない**（受け口が無い構成で正しく失敗する）。
///
/// 名指しフィールドが `None` のときと同じ形（`Option` が `None`）に落ちることを
/// 固定する。ここが `Some` に化けると、稼働していない transport のツールが
/// 生えたり、鍵の払い出しが黙って別経路に流れる。
#[test]
fn unregistered_kind_yields_no_capability() {
    let registry = AgentGatewayRegistry::new();
    assert!(registry.get(kinds::DISCORD).is_none());
    assert!(registry
        .get(kinds::DISCORD)
        .and_then(|gw| gw.gateway_actions_for("crab"))
        .is_none());
    assert!(registry
        .get(kinds::NOSTR)
        .and_then(|gw| gw.key_provisioning())
        .is_none());
}

/// 秘密値が `Debug` 出力に出ない（ログ・エラー文字列への漏洩を型で止める）。
#[test]
fn provisioned_key_debug_redacts_the_secret() {
    let key = ProvisionedKey {
        secret: "nsec1secretvalue".to_string(),
        public_id: "npub1public".to_string(),
        public_key_hex: "deadbeef".to_string(),
    };
    let rendered = format!("{key:?}");
    assert!(
        !rendered.contains("nsec1secretvalue"),
        "秘密鍵が Debug に出ている: {rendered}"
    );
    assert!(rendered.contains("npub1public"), "公開側は見えること");
    assert!(rendered.contains("deadbeef"));
}

// ------------------------------------------------------------------
// 復元の走査（#191 段階2 PR5）
//
// 起動処理から `restore_from_db` の名指しを消すための走査。**順序が仕様**なので、
// 「何が・どの順で・何回復元されるか」をここで固定する。緩むと、Discord の復元が
// 後ろへずれて heartbeat の HTTP クライアントが取れなくなる（= 移設前と挙動が変わる）。
// ------------------------------------------------------------------

/// **起動処理が実際に取る形**（復元位置が 2 つある）を再現し、走る対象を固定する。
///
/// 現状の起動処理は Discord を先に登録して共有ゲートウェイ起動後に復元し、その後
/// Nostr を登録してルータ構築の直前に復元する。走査を最後の 1 回に畳むと Discord の
/// 復元が後ろへずれるので、**その時点で未復元の分だけ**を各位置で復元する。
#[tokio::test]
async fn restore_pending_restores_each_gateway_at_its_own_point() {
    let registry = AgentGatewayRegistry::new();

    // 位置 1（共有ゲートウェイ起動後）: この時点で登録済みなのは Discord だけ。
    let discord = FakeGateway::new(kinds::DISCORD, &[]);
    registry.register(discord.clone());
    assert_eq!(registry.restore_pending().await, vec![kinds::DISCORD]);
    assert_eq!(discord.restored.load(Ordering::SeqCst), 1);

    // 位置 2（ルータ構築の直前）: Nostr を登録してから走査。**Discord は再復元しない。**
    let nostr = FakeGateway::new(kinds::NOSTR, &[]);
    registry.register(nostr.clone());
    assert_eq!(registry.restore_pending().await, vec![kinds::NOSTR]);
    assert_eq!(
        discord.restored.load(Ordering::SeqCst),
        1,
        "2 回目の走査が Discord を巻き込むと接続を張り直してしまう"
    );
    assert_eq!(nostr.restored.load(Ordering::SeqCst), 1);
}

/// 同じ位置に複数登録されていれば**登録順**で復元する（走査が順序を保つ）。
#[tokio::test]
async fn restore_pending_follows_registration_order() {
    let log: OrderLog = Arc::new(std::sync::Mutex::new(vec![]));
    let registry = AgentGatewayRegistry::new();
    registry.register(FakeGateway::with_order_log(kinds::DISCORD, &log));
    registry.register(FakeGateway::with_order_log(kinds::NOSTR, &log));

    let restored = registry.restore_pending().await;
    assert_eq!(restored, vec![kinds::DISCORD, kinds::NOSTR]);
    assert_eq!(
        *log.lock().unwrap(),
        vec![kinds::DISCORD, kinds::NOSTR],
        "実際に復元が走った順も登録順であること"
    );
}

/// 復元は**起動時 1 回だけ**（周期的な自己修復は持たない）。走査を呼び直しても
/// 何も起きない。
#[tokio::test]
async fn restore_pending_is_a_one_shot_per_gateway() {
    let registry = AgentGatewayRegistry::new();
    let nostr = FakeGateway::new(kinds::NOSTR, &[]);
    registry.register(nostr.clone());

    assert_eq!(registry.restore_pending().await, vec![kinds::NOSTR]);
    assert!(registry.restore_pending().await.is_empty());
    assert!(registry.restore_pending().await.is_empty());
    assert_eq!(nostr.restored.load(Ordering::SeqCst), 1);
    assert!(registry.is_restored(kinds::NOSTR));
}

/// **Discord を落とした構成**（`--no-default-features`）でも同じ形で通る。
///
/// 位置 1 の走査ごと消えるので、残った 1 回が Nostr を復元する。
#[tokio::test]
async fn restore_pending_works_without_discord_registered() {
    let registry = AgentGatewayRegistry::new();
    assert!(
        registry.restore_pending().await.is_empty(),
        "空の登録簿でも安全に呼べる"
    );

    let nostr = FakeGateway::new(kinds::NOSTR, &[]);
    registry.register(nostr.clone());
    assert_eq!(registry.restore_pending().await, vec![kinds::NOSTR]);
    assert_eq!(nostr.restored.load(Ordering::SeqCst), 1);
    assert!(
        !registry.is_restored(kinds::DISCORD),
        "未登録は復元済みでない"
    );
}

/// 同じ種別を置き換えたら復元済みの印も落ちる（新しいマネージャは未復元）。
#[tokio::test]
async fn re_registering_clears_the_restored_mark() {
    let registry = AgentGatewayRegistry::new();
    registry.register(FakeGateway::new(kinds::NOSTR, &[]));
    assert_eq!(registry.restore_pending().await, vec![kinds::NOSTR]);

    let replacement = FakeGateway::new(kinds::NOSTR, &[]);
    registry.register(replacement.clone());
    assert_eq!(registry.restore_pending().await, vec![kinds::NOSTR]);
    assert_eq!(replacement.restored.load(Ordering::SeqCst), 1);
}

/// トレイトオブジェクト越しに 5 操作すべてを呼べる（`dyn` として使える形か）。
#[tokio::test]
async fn all_operations_are_callable_through_the_trait_object() {
    let registry = AgentGatewayRegistry::new();
    registry.register(FakeGateway::new(kinds::NOSTR, &[]));

    let gw = registry.get(kinds::NOSTR).unwrap();
    gw.start("crab").await.unwrap();
    gw.stop("crab").await;
    let _ = gw.is_running("crab");
    gw.restore_all().await;
    gw.shutdown_all().await;
    assert_eq!(gw.kind(), kinds::NOSTR);

    // 走査（PR5 の一般化が取る形）もロックを跨がずにできる。
    for gw in registry.all() {
        gw.shutdown_all().await;
    }
}
