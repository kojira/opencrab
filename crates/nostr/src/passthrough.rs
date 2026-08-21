//! Nostr の**薄い nostaro passthrough** capability（#268）。
//!
//! server-own の `nostr_run` は「鍵のエージェント間混同防止」と「nsec 隠蔽」の 2 点だけを
//! opencrab 側で担保し、Nostr 操作そのもの（投稿・kind:0 プロフィール・チャンネル・取得
//! 等）は nostaro にそのまま委ねる（再実装しない＝非劣化）。ここはその受け口を
//! [`opencrab_actions::GatewayNostrPassthrough`] の実装として置き、上位が「登録簿から
//! NOSTR を引いて capability があれば使う」形で書けるようにする（`NostrKeyProvisioning`
//! と同型）。
//!
//! deny（`init`/`watch`/`relay`）・config 固定（`agent_id` のもの）・未 materialize の明示エラー・
//! nsec マスクといった安全ガードは [`NostaroCli::run_passthrough`] の内側に閉じる。マネージャ
//! が持つ [`NostaroCli`] を clone して渡すので `binary_path` / timeout をそのまま継承する。

use opencrab_actions::GatewayNostrPassthrough;

use crate::cli::NostaroCli;

/// [`GatewayNostrPassthrough`] の Nostr 実装（nostaro サブコマンドの薄い通し）。
#[derive(Debug, Clone, Default)]
pub struct NostrPassthrough {
    cli: NostaroCli,
}

impl NostrPassthrough {
    /// マネージャの CLI 設定を引き継いで作る。
    pub fn new(cli: NostaroCli) -> Self {
        Self { cli }
    }
}

#[async_trait::async_trait]
impl GatewayNostrPassthrough for NostrPassthrough {
    async fn run(
        &self,
        agent_id: &str,
        subcommand: &str,
        args: &[String],
    ) -> anyhow::Result<String> {
        self.cli.run_passthrough(agent_id, subcommand, args).await
    }
}
