//! Nostr の**鍵の払い出し** capability（#191 段階2 PR4）。
//!
//! 上位（`crates/server`）はこれまで `AppState` の名指しフィールドから
//! `NostrGatewayManager::cli()` / `generate_key()` を直接叩いていた。ここはその受け口を
//! [`opencrab_actions::GatewayKeyProvisioning`] の実装として置き、上位が
//! 「登録簿から NOSTR を引いて capability があれば使う」形で書けるようにする。
//!
//! 鍵の払い出しは**稼働中のゲートウェイを必要としない**（`nostaro vanity` は config を
//! 読まない）ので、マネージャとは独立に構築できる。マネージャが持つ [`NostaroCli`] を
//! clone して渡せば `binary_path` / timeout / vanity ゲートをそのまま継承する
//! （`NostaroCli` の clone は同じ `Arc<Semaphore>` を共有するので、HTTP 経由も
//! LLM ツール経由も同じ 1 本のゲートを通るという既存の性質が壊れない）。

use opencrab_actions::{GatewayKeyProvisioning, ProvisionedKey};

use crate::cli::{GeneratedKey, NostaroCli};

/// [`GatewayKeyProvisioning`] の Nostr 実装（nostaro の vanity 生成 + 0600 保存）。
#[derive(Debug, Clone, Default)]
pub struct NostrKeyProvisioning {
    cli: NostaroCli,
}

impl NostrKeyProvisioning {
    /// マネージャの CLI 設定を引き継いで作る。
    pub fn new(cli: NostaroCli) -> Self {
        Self { cli }
    }
}

/// nostaro 由来の鍵を transport 非依存の運び手へ移す。
///
/// `nsec` は移し替えるだけで、この経路のどこにも記録しない
/// （[`ProvisionedKey`] は `Debug` で伏せ、`Display` を持たない）。
fn to_provisioned(key: GeneratedKey) -> ProvisionedKey {
    ProvisionedKey {
        secret: key.nsec,
        public_id: key.npub,
        public_key_hex: key.pubkey,
    }
}

/// 保存側（`save_generated_key`）が nostaro の型を取るので戻す。
fn to_generated(key: &ProvisionedKey) -> GeneratedKey {
    GeneratedKey {
        nsec: key.secret.clone(),
        npub: key.public_id.clone(),
        pubkey: key.public_key_hex.clone(),
    }
}

#[async_trait::async_trait]
impl GatewayKeyProvisioning for NostrKeyProvisioning {
    /// vanity で新規鍵を生成する。同時実行の制限は [`NostaroCli`] 内のゲートで一元化
    /// （HTTP ルートも LLM ツールも同じゲートを通る）。prefix の検証は `vanity` 側。
    async fn generate_key(&self, prefix: &str) -> anyhow::Result<ProvisionedKey> {
        self.cli.vanity(prefix).await.map(to_provisioned)
    }

    /// 生成した nsec を per-agent ディレクトリへ 0600 で保存する。
    ///
    /// 戻り値の保存パスは**捨てる**（呼び出し側は保存できたかどうかしか要らず、
    /// パスを返すと秘密鍵の所在がツール結果経由で LLM へ渡る）。
    fn store_generated_key(&self, agent_id: &str, key: &ProvisionedKey) -> anyhow::Result<()> {
        // #620: 暗号化して保存する（cli が持つマスターキーを使う）。
        self.cli
            .save_generated_key(agent_id, &to_generated(key))
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 運び手への移し替えで公開/秘密の対応がズレない（nsec を npub 欄に入れない）。
    #[test]
    fn maps_fields_without_swapping_public_and_secret() {
        let provisioned = to_provisioned(GeneratedKey {
            nsec: "nsec1aaa".to_string(),
            npub: "npub1bbb".to_string(),
            pubkey: "ccc".to_string(),
        });
        assert_eq!(provisioned.secret, "nsec1aaa");
        assert_eq!(provisioned.public_id, "npub1bbb");
        assert_eq!(provisioned.public_key_hex, "ccc");

        let back = to_generated(&provisioned);
        assert_eq!(back.nsec, "nsec1aaa");
        assert_eq!(back.npub, "npub1bbb");
        assert_eq!(back.pubkey, "ccc");
    }
}
