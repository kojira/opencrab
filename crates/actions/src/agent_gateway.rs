//! 受信を持つ transport の **エージェント単位ライフサイクル**境界と登録簿（#191 段階2）。
//!
//! 上位（`crates/server` の `AppState`）は Discord / Nostr / MCP / web を**名指しの
//! フィールド**で保持しており、共通操作（起動・停止・生存確認・DB からの復元・全停止）
//! の呼び出しが transport ごとに複製されている。しかも Discord は条件付きコンパイル
//! なので、同じ操作が `#[cfg(feature = "discord")]` の内と外に分かれている。
//! ここはその受け皿になる 1 つの契約を置く。
//!
//! ## 置き場所の理由（依存方向）
//!
//! 依存は server → 各ゲートウェイであり、ゲートウェイ側から server の型は参照できない。
//! かといって `crates/gateway` にも置けない: あちらは `default = ["discord"]` で
//! serenity / songbird を引きずるため、Discord を切った構成（`--no-default-features`）
//! でも必要なこの契約の置き場所としては重い。[`crate::subtask_registries`] や
//! [`crate::agent_runtime`] と同じ **gateway 非依存層**に置き、どの transport からも
//! 同じ 1 つの契約を使えるようにする。
//!
//! ## なぜ「起動」が資格情報を取らないか
//!
//! 資格情報の形は transport ごとに違う（Discord はトークンと owner、Nostr は秘密鍵と
//! フィルタ設定、MCP はサーバ設定の配列）。共通の引数に畳むと結局 transport 固有の型が
//! 契約へ漏れる。3 者とも**すでに DB から設定行を読んで起動している**ので、
//! [`AgentGatewayLifecycle::start`] は `agent_id` だけを取り、**設定の読み出しを実装側の
//! 責務**にする。これが唯一の共通化になる。
//!
//! ## なぜ MCP を登録簿に入れないか
//!
//! `crates/mcp` は**受信を持たない**。外部プロセスの道具をエージェントへ供給する側で、
//! transport ではない。道具の注入は**深さ 0（親ターン）限定**という遮断が効いており、
//! 「受信を持つ transport」と同じ登録簿に混ぜるとその前提が崩れる。MCP マネージャは
//! `AppState` の名指しフィールドのまま残す。

use std::sync::Arc;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use anyhow::Result;
use async_trait::async_trait;

/// poison を無視して読みガードを取る。
///
/// **`unwrap()` にしない理由はこの経路の性質。** 登録簿の読み取りは
/// [`AgentGatewayLifecycle::is_running`] を通じて「専用ゲートウェイが処理するか、共有側に
/// フォールバックするか」を決めるルーティング判定に載る。ここで panic すると、
/// 「分からないから共有側が続ける」ではなく**受信処理そのものが止まる**。
///
/// poison が起きうる現実的な経路はほぼ無い（ロック下で走るのは `Vec` の走査と
/// `Arc` の clone だけで、パニックしうる処理を挟まない）。だが `into_inner()` で
/// 復帰すれば「起きない」を**構造的に起きえない**にできる。中身は不整合になり得ない
/// （登録簿は `Arc` の並びだけで、途中で壊れた状態を作る書き込みが無い）。
fn read_or_recover<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

/// poison を無視して書きガードを取る（理由は [`read_or_recover`] と同じ）。
fn write_or_recover<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

/// transport の種別名（[`AgentGatewayLifecycle::kind`] の戻り値 / 登録簿のキー）。
///
/// 定数をここに置くのは、`--no-default-features`（Discord クレート自体が居ない構成）
/// でも参照側がキーを書けるようにするため。
pub mod kinds {
    /// per-agent Discord Bot ゲートウェイ。
    pub const DISCORD: &str = "discord";
    /// per-agent Nostr sub-gateway。
    pub const NOSTR: &str = "nostr";
}

/// 登録簿に入れる共有ハンドル。
pub type SharedAgentGateway = Arc<dyn AgentGatewayLifecycle>;

/// 受信を持つ transport の per-agent ライフサイクル管理。
///
/// 実装するのは**マネージャ**（`DiscordGatewayManager` / `NostrGatewayManager`）であって
/// 接続 1 本ではない。1 実装が「そのプロセスにおけるその transport 全エージェント分」を
/// 束ねる。
///
/// `Send + Sync + 'static`: `AppState` は clone されて各所（axum ハンドラ・spawn した
/// ループ）へ配られるため、`Arc<dyn AgentGatewayLifecycle>` が跨げる必要がある。
#[async_trait]
pub trait AgentGatewayLifecycle: Send + Sync + 'static {
    /// transport の種別名（登録簿のキー / ログの識別子）。[`kinds`] の定数を返す。
    fn kind(&self) -> &'static str;

    /// このエージェントのゲートウェイを **DB の設定を読んで**起動する。
    ///
    /// 資格情報を引数で受けない理由はモジュール doc 参照。設定行が無い / 起動に失敗した
    /// ときは `Err`。**`enabled` フラグの書き換えはここでは行わない**（DB の方針であって
    /// ライフサイクルではない。呼び出し側が起動の成否を見て決める）。
    ///
    /// すでに稼働中なら実装は「止めてから起動し直す」こと（既存の
    /// `start_agent_gateway` が両実装ともそうしている）。
    ///
    /// **入口の正規化を実装から落とさないこと。** 例えば Discord は owner 識別子を
    /// ここで trim して未設定を警告する。落とすと「DM は通るのに owner 専用 UI だけ
    /// 無言で拒否」が復活する。
    async fn start(&self, agent_id: &str) -> Result<()>;

    /// このエージェントのゲートウェイを停止する。稼働していなければ何もしない。
    async fn stop(&self, agent_id: &str);

    /// このエージェント専用のゲートウェイが**実際に稼働中**か。
    ///
    /// **死活監視ではなくルーティング判定**であることに注意（#40）。共有（TOML）
    /// ゲートウェイが「専用ゲートウェイに任せるか、自分がフォールバックとして処理を
    /// 続けるか」を per-message で決めるのに使う。判定は DB の `enabled` ではなく
    /// ゲートウェイの生死で行う: `enabled=1` でも起動失敗していれば `false` を返し、
    /// 共有側が処理を続ける（どのゲートウェイからも応答しない状態を作らない）。
    ///
    /// 同期メソッド: per-message の判定なので await を挟ませない。
    fn is_running(&self, agent_id: &str) -> bool;

    /// DB の enabled な設定を全件読んで起動する（プロセス起動時の復元）。
    ///
    /// 個々の失敗はログに残して次へ進む（1 エージェントの起動失敗で他を巻き込まない）。
    async fn restore_all(&self);

    /// この transport の全エージェント分を停止する（プロセス終了時）。
    async fn shutdown_all(&self);
}

/// 稼働中の transport マネージャの登録簿（#191 段階2）。
///
/// ## なぜ内部可変か
///
/// マネージャの生成順は**仕様になっている**。Discord のマネージャは共有ゲートウェイへ
/// 渡す `AppState` の clone より**前に**生成して配線しないと、共有ループが「専用
/// ゲートウェイが稼働中か」を参照できない。DB からの復元タイミングも transport ごとに
/// 違う（Discord は共有ゲートウェイ起動後、Nostr はルータ構築の直前）。
///
/// 登録簿を `AppState` の**不変フィールド**にすると「全マネージャが state 構築前に
/// 揃っていること」を要求してしまい、この順序と衝突する。内部可変にして**後から
/// 登録**できる形にすることで、順序依存を構造的に消す（`voice_runtime` と
/// `subtask_lifecycle_notifier` が同じ流儀で後差しになっている）。
///
/// ## 順序を保つ
///
/// 登録順を保持する（`Vec`）。復元順が transport ごとに違うという事実は消えないので、
/// 「登録した順＝現在の起動順」を保存しておき、走査への一般化（段階2 PR5）が既存の
/// 順序をそのまま再現できるようにする。
#[derive(Default)]
pub struct AgentGatewayRegistry {
    // std RwLock（tokio ではない）: `is_running` を同期メソッドに保つため。
    // ガードを await 跨ぎで保持しないこと（各メソッドで clone して閉じる）。
    gateways: RwLock<Vec<SharedAgentGateway>>,
}

impl AgentGatewayRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// マネージャを登録する。同じ種別が既にあれば**置き換える**（登録順は保つ）。
    pub fn register(&self, gateway: SharedAgentGateway) {
        let kind = gateway.kind();
        let mut gateways = write_or_recover(&self.gateways);
        match gateways.iter().position(|g| g.kind() == kind) {
            Some(i) => gateways[i] = gateway,
            None => gateways.push(gateway),
        }
    }

    /// 種別からマネージャを引く（未登録なら `None`）。
    pub fn get(&self, kind: &str) -> Option<SharedAgentGateway> {
        read_or_recover(&self.gateways)
            .iter()
            .find(|g| g.kind() == kind)
            .cloned()
    }

    /// 登録済みの全マネージャを**登録順**で返す。
    ///
    /// 返すのはスナップショット（ロックは戻る前に落ちる）。呼び出し側が `.await` を
    /// 挟んで走査してもロックを跨がない。
    pub fn all(&self) -> Vec<SharedAgentGateway> {
        read_or_recover(&self.gateways).clone()
    }

    /// 登録済みの種別名を**登録順**で返す。
    pub fn kinds(&self) -> Vec<&'static str> {
        read_or_recover(&self.gateways)
            .iter()
            .map(|g| g.kind())
            .collect()
    }

    /// その種別の専用ゲートウェイがこのエージェントで稼働中か。
    ///
    /// **未登録の種別は `false`**（`None` でも panic でもない）。これはルーティング
    /// 判定であって死活監視ではないので、「分からない」は**共有側が処理を続ける**方へ
    /// 倒す。`true` に倒すと二重処理、異常終了させると停止する。
    pub fn is_running(&self, kind: &str, agent_id: &str) -> bool {
        self.get(kind)
            .map(|g| g.is_running(agent_id))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// ネットワークに出ない偽マネージャ（呼ばれた回数だけ数える）。
    struct FakeGateway {
        kind: &'static str,
        running: Vec<String>,
        started: AtomicUsize,
        stopped: AtomicUsize,
        restored: AtomicUsize,
        shutdown: AtomicUsize,
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
}
