//! transport が提供する「素のテキストを宛先へ送る」配送口（#157 S7）。
//!
//! ピアレビュー依頼（`request_peer_review`）の実体は gateway 非依存層
//! （`crates/server/src/peer_review.rs`）にあるが、**実際に送る**ことだけは transport に
//! しか作れない。`GatewayActions::a2ui_surface()`（#156 S3）と同じ流儀で、合成 gateway
//! （`SystemGatewayActions`）が transport からこの配送口を 1 度だけ引き、汎用層へ渡す。
//!
//! ここに置くのは**移設で transport 側に残ると判断した 4 つだけ**:
//! 1. 宛先トークンの妥当性検査（Discord は数値スノーフレーク）
//! 2. ユーザーメンションの記法（Discord は `<@id>`）
//! 3. 1 通に収める安全な文字数上限（Discord は 2000 未満）
//! 4. 送信そのもの（transport の SDK 直叩き）
//!
//! 分割の仕方・部分失敗の勘定（「N/M 通送信済み」）・本文の組み立ては汎用層の責務で、
//! この境界を越えない（抽象越しに失われやすい情報なので、意図的に呼び出し側に残す）。

use async_trait::async_trait;

/// transport の素テキスト配送口。
///
/// エラーは表示用の文字列で返す（呼び出し側がユーザー向け文言へ埋め込む）。
#[async_trait]
pub trait TextDelivery: Send + Sync {
    /// 宛先トークンがこの transport で有効か検査する。
    ///
    /// `Err` の文字列は**そのままツール結果の error になる**ので、transport 固有の
    /// 文言（例: `無効なchannel_id: xxx`）はここで組む。
    fn validate_target(&self, target: &str) -> Result<(), String>;

    /// この transport のユーザーメンション記法（Discord なら `<@123>`）。
    fn mention(&self, user_id: &str) -> String;

    /// 1 通に収める安全な文字数上限（分割の粒度）。
    fn chunk_limit(&self) -> usize;

    /// 1 通のテキストを宛先へ送る。
    async fn send_text(&self, target: &str, text: &str) -> Result<(), String>;
}
