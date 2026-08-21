//! 素テキスト配送口の Nostr 実装（#246 段階3 PR-B）。
//!
//! Discord の [`crate`] 外にある `DiscordTextDelivery` と同じ流儀で、transport 固有の
//! 4 つ（宛先検査・メンション記法・1 通の上限・送信そのもの）だけをここに置く。分割の
//! 仕方・部分失敗の勘定（「N/M 通送信済み」）・本文の組み立ては汎用層
//! （`crates/server/src/peer_review.rs` など [`opencrab_core::text_delivery::TextDelivery`]
//! の呼び出し側）の責務で、この境界を越えない。
//!
//! Nostr の配送は**自発投稿（kind:1 broadcast）**である。宛先はリレー集合＝エージェント
//! 設定から nostaro が解決するため、`target` 引数は使わない（返信ではないので
//! [`NostaroCli::reply`] ではなく [`NostaroCli::post`] を呼ぶ）。ハートビート等の
//! transport 非依存な配送口（PR-A で新設予定）から、登録簿（`state.gateways`）経由で
//! この配送口が引かれ、Nostr へ自発発話できるようになる。

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use opencrab_core::text_delivery::TextDelivery;

use crate::cli::NostaroCli;

/// 1 通（kind:1 ノート）に収める安全な本文文字数上限。
///
/// **Nostr プロトコルには kind:1 の本文長に対する固定上限が無い。** 実際の制限は各リレーの
/// NIP-11 `limitation.max_content_length`（公開している場合）に依存し、値はリレーごとに
/// まちまち（数 KB 〜 64KB 程度）。本リポジトリにも nostaro 側にも本文長の定数は存在しない
/// （確認済み: `crates/nostr` / `docs/nostaro-interface.md` に該当なし）ため、
/// **どのリレーでも安全に通る保守的な既定値**を置く。
///
/// 1000 文字にする根拠: この上限は char 単位だが、リレーの max_content_length は byte 単位
/// で測られることが多い。日本語（UTF-8 で 1 文字 3 バイト）主体の本文でも 1000 文字 ≒
/// 3000 バイト前後に収まり、公開リレーで見られる下限クラス（8192 バイト程度）にも十分な
/// 余裕を持って収まる。自発発話（ハートビート等）は短文が主で、この値で分割が過剰になる
/// ことは実務上ほぼ無い。将来リレー設定から実値を解決できるようになれば差し替える。
pub const NOSTR_CHUNK_LIMIT: usize = 1000;

/// 自発 kind:1 投稿の実体（テスト時に差し替える注入点）。
///
/// [`NostaroCli`] は外部プロセス `nostaro` を spawn する具象型で、単体テストから実リレーへ
/// publish させたくない。配送口が依存するのはこの 1 メソッドだけなので、細い trait で切り出し
/// て注入可能にする（実運用は [`NostaroCli`]、テストは記録するだけの fake）。
#[async_trait]
pub(crate) trait NostrPoster: Send + Sync {
    /// 自発ノート（kind:1 broadcast）を publish する。宛先はエージェント設定のリレー集合。
    async fn post_note(&self, agent_id: &str, text: &str) -> Result<String>;
}

#[async_trait]
impl NostrPoster for NostaroCli {
    async fn post_note(&self, agent_id: &str, text: &str) -> Result<String> {
        // 自発投稿なので from=None（本鍵）で post。返信ではないため reply は使わない。
        self.post(agent_id, text, None).await
    }
}

/// Nostr の素テキスト配送口。agent_id と投稿口を焼き込む。
pub(crate) struct NostrTextDelivery {
    agent_id: String,
    poster: Arc<dyn NostrPoster>,
}

impl NostrTextDelivery {
    /// 稼働中の gateway 用に、agent_id と実投稿口（[`NostaroCli`]）を焼いて組む。
    pub(crate) fn new(agent_id: impl Into<String>, cli: NostaroCli) -> Self {
        Self {
            agent_id: agent_id.into(),
            poster: Arc::new(cli),
        }
    }
}

#[async_trait]
impl TextDelivery for NostrTextDelivery {
    /// 自発投稿（kind:1 broadcast）は宛先を取らない（リレー集合はエージェント設定から
    /// nostaro が解決する）。したがって target は常に受理する（空文字も含め検査しない）。
    fn validate_target(&self, _target: &str) -> Result<(), String> {
        Ok(())
    }

    /// Nostr のユーザーメンション記法（NIP-27: `nostr:<bech32>`）。
    ///
    /// サーバ側に既存のメンションヘルパは無い（確認済み）。bech32 の公開識別子
    /// （`npub1...` / `nprofile1...`）はそのまま `nostr:` を前置すると NIP-27 のメンション
    /// として解釈される。それ以外（hex pubkey 等、前置しても標準の参照にならない形）は
    /// 壊さずそのまま返す。
    fn mention(&self, user_id: &str) -> String {
        if user_id.starts_with("npub1") || user_id.starts_with("nprofile1") {
            format!("nostr:{user_id}")
        } else {
            user_id.to_string()
        }
    }

    fn chunk_limit(&self) -> usize {
        NOSTR_CHUNK_LIMIT
    }

    /// 1 通のテキストを自発ノートとして publish する。`target` は未使用（自発投稿は宛先を
    /// 取らない）。実際の送信は焼き込んだ agent_id で [`NostrPoster::post_note`] へ委譲する。
    async fn send_text(&self, _target: &str, text: &str) -> Result<(), String> {
        self.poster
            .post_note(&self.agent_id, text)
            .await
            .map(|_| ())
            // 文言は nostaro の失敗をそのまま表示用文字列へ（呼び出し側が「N/M 通送信済み」
            // の文へ埋める）。
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 実リレーへ出さず、渡された (agent_id, text) を記録するだけの投稿口。
    struct FakePoster {
        calls: Mutex<Vec<(String, String)>>,
        result: Result<String, String>,
    }

    impl FakePoster {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                result: Ok("event-id-1".to_string()),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                result: Err("relay refused".to_string()),
            })
        }
    }

    #[async_trait]
    impl NostrPoster for FakePoster {
        async fn post_note(&self, agent_id: &str, text: &str) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push((agent_id.to_string(), text.to_string()));
            self.result.clone().map_err(|e| anyhow::anyhow!(e))
        }
    }

    fn delivery(poster: Arc<dyn NostrPoster>) -> NostrTextDelivery {
        NostrTextDelivery {
            agent_id: "agent-x".to_string(),
            poster,
        }
    }

    /// 自発投稿は宛先を取らないので、空文字を含めどんな target でも受理する。
    #[test]
    fn validate_target_accepts_anything_including_empty() {
        let d = delivery(FakePoster::ok());
        assert!(d.validate_target("").is_ok());
        assert!(d.validate_target("note1abc").is_ok());
        assert!(d.validate_target("-1").is_ok());
    }

    /// メンションは NIP-27（`nostr:<bech32>`）。bech32 でない形は壊さずそのまま返す。
    #[test]
    fn mention_uses_nip27_for_bech32_and_passes_through_others() {
        let d = delivery(FakePoster::ok());
        assert_eq!(d.mention("npub1abc"), "nostr:npub1abc");
        assert_eq!(d.mention("nprofile1xyz"), "nostr:nprofile1xyz");
        // hex pubkey 等は前置しても標準参照にならないのでそのまま。
        assert_eq!(d.mention("deadbeef"), "deadbeef");
        assert_eq!(d.mention(""), "");
    }

    /// 1 通の上限は保守的な既定値（char 単位）。
    #[test]
    fn chunk_limit_is_the_safe_default() {
        assert_eq!(delivery(FakePoster::ok()).chunk_limit(), NOSTR_CHUNK_LIMIT);
        assert_eq!(NOSTR_CHUNK_LIMIT, 1000);
    }

    /// send_text は焼き込んだ agent_id と本文を投稿口へ**そのまま**渡す（宛先 target は無視）。
    #[tokio::test]
    async fn send_text_forwards_agent_id_and_text_to_poster() {
        let poster = FakePoster::ok();
        let d = delivery(poster.clone());

        let r = d.send_text("ignored-target", "こんにちは").await;
        assert!(r.is_ok());

        let calls = poster.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "自発投稿は 1 回だけ publish する");
        assert_eq!(calls[0].0, "agent-x", "焼き込んだ agent_id を渡す");
        assert_eq!(calls[0].1, "こんにちは", "本文をそのまま渡す");
    }

    /// 送信失敗は表示用文字列で返る（呼び出し側が「N/M 通送信済み」の文へ埋める）。
    #[tokio::test]
    async fn send_text_surfaces_poster_error_as_string() {
        let d = delivery(FakePoster::failing());
        let err = d.send_text("", "本文").await.unwrap_err();
        assert!(
            err.contains("relay refused"),
            "nostaro の失敗文言が載る: {err}"
        );
    }
}
