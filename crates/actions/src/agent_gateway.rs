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
//! ## transport 固有の操作は capability の受け口で渡す（PR4）
//!
//! 起動・停止・生存確認は全 transport 共通だが、「ツール実行の実体を渡す」「鍵を作る」は
//! それを持つ transport にしか無い。これらを共通メソッドにすると持たない実装が
//! `unimplemented!()` を並べることになるので、**既定 `None` の capability accessor**
//! （[`AgentGatewayLifecycle::gateway_actions_for`] /
//! [`AgentGatewayLifecycle::key_provisioning`]）として足す。
//!
//! この形は新発明ではなく、[`opencrab_gateway::GatewayActions`] が
//! `a2ui_surface()` / `text_delivery()` で既に採っている流儀に倣ったもの
//! （「提供できる transport だけが `Some` を返す」「上位は有無で分岐するだけ」）。
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

/// 「設定が起動条件を満たさないので**起動しなかった**」ことを表す起動失敗（#191 段階2 PR3）。
///
/// [`AgentGatewayLifecycle::start`] のガード（有効フラグ / 資格情報の検査）が弾いたときに
/// 返す。異常ではなく**設定どおりの結果**なので、呼び出し側はこれを「起動失敗」と同じ
/// 重さで扱わなくてよい（[`is_start_declined`] で見分ける）。
///
/// ガードそのものを呼び出し側に置かないのがこの型の存在理由。判定は実装の中に閉じ、
/// 呼び出し側には「弾かれたのか、本当に落ちたのか」だけを渡す。呼び出し口が増えても
/// ガードの付け忘れが起きえない形になる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartDeclined {
    /// transport の種別名（[`kinds`]）。
    pub kind: &'static str,
    pub agent_id: String,
    /// 起動条件のどこを満たさなかったか（ログ・API 応答に出る）。
    pub reason: String,
}

impl StartDeclined {
    /// `anyhow::Error` として返すためのコンストラクタ（実装の `start` から使う）。
    pub fn err(kind: &'static str, agent_id: &str, reason: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self {
            kind,
            agent_id: agent_id.to_string(),
            reason: reason.into(),
        })
    }
}

impl std::fmt::Display for StartDeclined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ゲートウェイの起動条件を満たしていません（agent_id={}）: {}",
            self.kind, self.agent_id, self.reason
        )
    }
}

impl std::error::Error for StartDeclined {}

/// その起動失敗が [`StartDeclined`]（設定どおり起動しなかった）か。
///
/// 「起動条件を満たさないときは黙って何もしない」という既存の呼び出し側の挙動を、
/// ガードを実装へ持ち上げたあとも保つために使う（設定起因の見送りを error ログに
/// 出さない）。
pub fn is_start_declined(err: &anyhow::Error) -> bool {
    err.downcast_ref::<StartDeclined>().is_some()
}

/// transport が払い出した鍵（[`GatewayKeyProvisioning`] の戻り値 / #191 段階2 PR4）。
///
/// ## `Debug` を derive しない
///
/// `secret` は**秘密値**。derive すると `tracing` の `?key` や `format!("{e:?}")` 経由で
/// 平文が構造化ログへ落ちる。手実装で伏せ、`Display` は実装しない（表示できる形を
/// そもそも作らない）。この型は「払い出しから保存先へ渡すまでの一時的な運び手」であって、
/// 保持・記録する対象ではない。
#[derive(Clone)]
pub struct ProvisionedKey {
    /// 秘密鍵（Nostr なら nsec）。**保存先へ渡す以外に使わないこと。**
    pub secret: String,
    /// 公開識別子（Nostr なら npub）。
    pub public_id: String,
    /// 公開鍵の hex 表現（transport が返さなければ空）。
    pub public_key_hex: String,
}

impl std::fmt::Debug for ProvisionedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProvisionedKey")
            .field("secret", &"<redacted>")
            .field("public_id", &self.public_id)
            .field("public_key_hex", &self.public_key_hex)
            .finish()
    }
}

/// transport 固有の**鍵の払い出し**（capability / #191 段階2 PR4）。
///
/// 起動・停止・生存確認と違い、鍵を作れる transport は限られる。持つ実装だけが
/// [`AgentGatewayLifecycle::key_provisioning`] から `Some` を返す。
///
/// **DB への書き込みはここでやらない。** 「生成してから DB を書く」「起動が成功して
/// から有効化する」といった順序は呼び出し側（ハンドラ）の方針であり、capability は
/// 払い出しそのものに閉じる（ライフサイクル契約が起動と停止だけなのと同じ理由）。
#[async_trait]
pub trait GatewayKeyProvisioning: Send + Sync {
    /// 新しい鍵を払い出す。
    ///
    /// `prefix` は transport が定義する vanity prefix（空なら制約なし）。**書式の検証は
    /// 実装側**（無効な prefix で外部プロセスを起こさないため、呼び出し側が手前で
    /// 弾いてもよい）。
    async fn generate_key(&self, prefix: &str) -> Result<ProvisionedKey>;

    /// 払い出した鍵を transport の保管場所へ保存する。
    ///
    /// 保存先とパーミッションは transport の責務（Nostr は per-agent ディレクトリへ
    /// 0600 で書く）。**秘密値は戻り値にも保存パスにも出さない。**
    fn store_generated_key(&self, agent_id: &str, key: &ProvisionedKey) -> Result<()>;
}

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
    /// ## 起動条件のガードは**この中**にある（#191 段階2 PR3）
    ///
    /// 「有効フラグが立っているか」「資格情報が空白でないか」の検査は**実装の `start` の
    /// 中**で行い、満たさなければ [`StartDeclined`] を返す。呼び出し側の規約にすると
    /// 呼び出し口が増えるたびに忘れうる（＝無効にしたはずの設定や空白のトークンで
    /// 起動してしまう穴が開く）。このリポジトリが繰り返し採ってきた「忘れても安全」を
    /// 構造で作る方針に合わせ、拒否側へ倒すガードを型の内側へ閉じる。
    ///
    /// 判定の中身は transport ごとに違う（何が資格情報かも、有効フラグを見てよいかも
    /// 違う）ので、共通化するのは「弾いたら [`StartDeclined`] を返す」ところまで。
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
    ///
    /// **起動時 1 回だけ**呼ばれる。周期的に呼び直して落ちた接続を拾い直す仕組みは
    /// ここには無い（MCP マネージャが持つ 60 秒周期の自己修復とは別物で、あれは
    /// 登録簿の外にある）。1 回であることは
    /// [`AgentGatewayRegistry::restore_pending`] が構造的に保証する。
    async fn restore_all(&self);

    /// この transport の全エージェント分を停止する（プロセス終了時）。
    async fn shutdown_all(&self);

    /// この transport がこのエージェント向けの**ツール実行の実体**を提供するなら返す
    /// （capability / #191 段階2 PR4）。
    ///
    /// 稼働中の接続を持つ transport だけが `Some` を返す（接続が無ければツールは
    /// 実行できない）。REST 経由の会話は「専用ゲートウェイが稼働していればその
    /// transport のツールも使える」という既存の挙動をこれで表す。
    ///
    /// `agent_id` を取るのは、実体が**エージェント単位の接続**だから
    /// （[`GatewayActions::a2ui_surface`] のような接続 1 本に紐づく accessor と違い、
    /// マネージャは全エージェント分を束ねている）。
    ///
    /// 既定は `None`（ツール実行の実体を持たない transport）。
    ///
    /// [`GatewayActions::a2ui_surface`]: opencrab_gateway::GatewayActions::a2ui_surface
    fn gateway_actions_for(
        &self,
        _agent_id: &str,
    ) -> Option<Arc<dyn opencrab_gateway::GatewayActions>> {
        None
    }

    /// この transport が**鍵の払い出し**を提供するなら返す（capability / #191 段階2 PR4）。
    ///
    /// エージェント単位ではなくマネージャ単位（払い出しは外部コマンドの設定を継承する
    /// だけで、稼働中の接続を必要としない）。既定は `None`（鍵を作らない transport）。
    fn key_provisioning(&self) -> Option<Arc<dyn GatewayKeyProvisioning>> {
        None
    }
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
///
/// ## 復元の走査は「未復元の分だけ」（段階2 PR5）
///
/// 起動処理から `restore_from_db` の名指しを消すにあたり、**復元を 1 箇所の走査に畳む
/// ことはできない**。復元位置は transport ごとに違い、それが仕様だから:
///
/// - Discord は**共有（TOML）ゲートウェイの起動後**。起動直後の短い窓では共有側が
///   メッセージを処理し、専用ゲートウェイが上がり次第 per-message スキップが効く。
///   さらに直後の heartbeat 用 HTTP クライアントの取得が、この復元の完了に依存する。
/// - Nostr は**ルータ構築の直前**。
///
/// 全部を最後の 1 回に畳むと Discord の復元が後ろへずれ、上の 2 つが壊れる。そこで
/// [`Self::restore_pending`] は**呼ばれた時点で登録済みかつ未復元のものだけ**を
/// 登録順に復元する。既存の 2 つの復元位置でこれを呼べば、走る対象も順序も移設前と
/// 1 対 1 のまま、起動処理からは「どの transport をここで復元するか」という名指しが
/// 消える。新しい transport は「復元させたい位置より前に登録する」だけでよく、
/// 呼び出し口を足す必要が無い。
#[derive(Default)]
pub struct AgentGatewayRegistry {
    // std RwLock（tokio ではない）: `is_running` を同期メソッドに保つため。
    // ガードを await 跨ぎで保持しないこと（各メソッドで clone して閉じる）。
    gateways: RwLock<Vec<RegisteredGateway>>,
}

/// 登録簿の 1 エントリ（マネージャ + 復元済みの印）。
struct RegisteredGateway {
    gateway: SharedAgentGateway,
    /// [`AgentGatewayRegistry::restore_pending`] が既にこのマネージャの復元を回したか。
    ///
    /// ゲートウェイの復元は**起動時 1 回だけ**（MCP のような周期的な自己修復は持たない）。
    /// この印がその「1 回」を構造的に保証する。
    restored: bool,
}

impl AgentGatewayRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// マネージャを登録する。同じ種別が既にあれば**置き換える**（登録順は保つ）。
    ///
    /// 置き換えたときは復元済みの印を落とす（新しいマネージャはまだ DB から復元して
    /// いない）。起動処理は各種別を 1 度しか登録しないので、通常の経路では効かない。
    pub fn register(&self, gateway: SharedAgentGateway) {
        let kind = gateway.kind();
        let entry = RegisteredGateway {
            gateway,
            restored: false,
        };
        let mut gateways = write_or_recover(&self.gateways);
        match gateways.iter().position(|e| e.gateway.kind() == kind) {
            Some(i) => gateways[i] = entry,
            None => gateways.push(entry),
        }
    }

    /// 種別からマネージャを引く（未登録なら `None`）。
    pub fn get(&self, kind: &str) -> Option<SharedAgentGateway> {
        read_or_recover(&self.gateways)
            .iter()
            .find(|e| e.gateway.kind() == kind)
            .map(|e| e.gateway.clone())
    }

    /// 登録済みの全マネージャを**登録順**で返す。
    ///
    /// 返すのはスナップショット（ロックは戻る前に落ちる）。呼び出し側が `.await` を
    /// 挟んで走査してもロックを跨がない。
    pub fn all(&self) -> Vec<SharedAgentGateway> {
        read_or_recover(&self.gateways)
            .iter()
            .map(|e| e.gateway.clone())
            .collect()
    }

    /// 登録済みの種別名を**登録順**で返す。
    pub fn kinds(&self) -> Vec<&'static str> {
        read_or_recover(&self.gateways)
            .iter()
            .map(|e| e.gateway.kind())
            .collect()
    }

    /// **まだ復元していない**マネージャを、**登録順**に DB から復元する（段階2 PR5）。
    ///
    /// 起動処理の `restore_from_db` の名指しを置き換えるための走査。復元位置が
    /// transport ごとに違う（理由は型の doc 参照）ため、「登録済みの全部」ではなく
    /// **その時点で登録済みかつ未復元の分だけ**を対象にする。既存の復元位置でそのまま
    /// 呼べば、走る対象も順序も移設前と 1 対 1 になる。
    ///
    /// 戻り値は実際に復元した種別名（復元した順）。ログや検査に使う。
    ///
    /// ## ロックを await 跨ぎで持たない
    ///
    /// 対象の取り出しと印付けはロックの中で済ませ、実際の復元（`.await`）はロックを
    /// 落としてから回す。**印は await の前に付ける**: 後に付けると、復元中にもう一度
    /// この走査が走ったとき同じマネージャを二重に復元しうる（`restore_all` は
    /// 稼働中のゲートウェイを止めて起動し直すので、二重復元は接続の張り直しになる）。
    pub async fn restore_pending(&self) -> Vec<&'static str> {
        let pending: Vec<SharedAgentGateway> = {
            let mut gateways = write_or_recover(&self.gateways);
            gateways
                .iter_mut()
                .filter(|e| !e.restored)
                .map(|e| {
                    e.restored = true;
                    e.gateway.clone()
                })
                .collect()
        };

        let mut restored = Vec::with_capacity(pending.len());
        for gateway in pending {
            let kind = gateway.kind();
            tracing::debug!(kind, "restoring per-agent gateways from DB");
            gateway.restore_all().await;
            restored.push(kind);
        }
        restored
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

    /// その種別が既に復元済みか（検査用）。未登録なら `false`。
    #[cfg(test)]
    fn is_restored(&self, kind: &str) -> bool {
        read_or_recover(&self.gateways)
            .iter()
            .find(|e| e.gateway.kind() == kind)
            .map(|e| e.restored)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
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
        assert!(!registry.is_restored(kinds::DISCORD), "未登録は復元済みでない");
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
}
