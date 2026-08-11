//! nostr ゲートウェイがエージェント実行に必要とする最小 runner 境界。
//!
//! discord の `AgentRunner`（巨大・Discord 固有）に依存しないよう、必要なメソッド
//! だけを切り出したトレイト。`crates/server` の `AppState` が実装し、既存の
//! `run_agent_response` / セッション/転記ヘルパへ委譲する。
//!
//! ゲートウェイ非依存な実行・セッション管理は [`opencrab_actions::AgentRuntime`] が
//! 持つ（#156 S1）。ここには Nostr の語彙（イベント・pubkey・設定行）を含むものだけを
//! 宣言する。

use anyhow::Result;

use opencrab_actions::{AgentRuntime, CallerIdentity};
use opencrab_db::queries::AgentNostrConfigRow;

pub trait NostrAgentRunner: AgentRuntime {
    /// 受信イベントの発言者から呼び出し元の権限を決める（#319）。
    ///
    /// Discord の `resolve_caller` と同じ形: **オーナーなら `Owner`、そのエージェントの
    /// Nostr 経路の信頼済みユーザーならその権限、どちらでもなければ `Agent`**（最小権限）。
    /// 以前はここが `CallerIdentity::Agent` 固定で、オーナーが話しかけても外部の誰かが
    /// 話しかけても同じ扱いだった（＝エージェントが自分の設定を一切変更できなかった）。
    ///
    /// 契約:
    /// - **オーナー未設定なら誰もオーナーにならない**（fail-closed）。
    /// - `author_pubkey` は npub / hex のどちらで来ても同じ鍵として扱う（実装側で正規化）。
    /// - 照合するのは **Nostr 経路の行だけ**（Discord の識別子空間と混ぜない）。
    /// - 「発言者がオーナー」以外の昇格経路を作らない。
    fn resolve_nostr_caller(&self, agent_id: &str, author_pubkey: &str) -> CallerIdentity;

    // 転記（受信イベント / エージェント返信）は [`AgentRuntime`] が持つ（#158 S3）。
    // `record_inbound_message` / `record_outbound_reply` を
    // `TranscriptSource::Nostr` で呼ぶ。Discord と行の形が同じなので、gateway ごとに
    // 宣言を分ける理由が無い。

    /// enabled な per-agent Nostr 設定一覧（起動時 restore 用）。
    fn list_enabled_nostr_configs(&self) -> Vec<AgentNostrConfigRow>;

    /// エージェントの Nostr 設定行を取得する（identity 切替で relays 継承に使う）。
    fn get_nostr_config(&self, agent_id: &str) -> Option<AgentNostrConfigRow>;

    /// 本鍵（secret_key）だけを差し替える（identity 切替。relays/filter/enabled は保持）。
    fn set_nostr_secret_key(&self, agent_id: &str, secret_key: &str) -> Result<()>;

    /// この agent 自身の Nostr pubkey（64 桁小文字 hex）を co_agent 逆引き表へ保存する（#489）。
    ///
    /// **呼んでよいのは自己 pubkey を自 secret_key から導出した文脈だけ**（gateway 起動時 /
    /// identity 切替の新 pubkey）。受信イベントの著者 pubkey からは決して呼ばない
    /// （外部が「pubkey ↔ agent UUID」を仕込めると任意ユーザーが co_agent に化ける）。
    /// 書けなくても致命ではない（逆引き不可 → co_agent は fail-closed）ので、呼び出し側は
    /// best-effort で扱ってよい。
    fn set_nostr_self_pubkey(&self, agent_id: &str, self_pubkey: &str) -> Result<()>;

    /// `agent_nostr_config` 行を丸ごと書き込む（自己ブートストラップの採用時 / #264）。
    ///
    /// 未設定エージェントが自力で鍵を採用するとき、secret_key＋relays＋フィルタ（既存が無ければ
    /// 空＝自分宛のみ / #271）を
    /// **enabled=false で先に書く**（起動成功後に [`Self::set_nostr_enabled`] で有効化する
    /// 順序ガードのため）。既存行があれば上書きする（upsert）。
    fn upsert_nostr_config(&self, cfg: &AgentNostrConfigRow) -> Result<()>;

    /// `agent_nostr_config` の enabled フラグだけを立て下げする（#264）。
    ///
    /// **起動が成功してから `true` にする**（失敗時に「enabled だが未稼働」の不整合を
    /// 残さない）。この順序は呼び出し側（採用 capability）が守る。
    fn set_nostr_enabled(&self, agent_id: &str, enabled: bool) -> Result<()>;

    /// エージェント宛の Nostr 受信を転記する宛先を解決する（issue #252 段階 A）。
    ///
    /// エージェント単位設定（`agent_nostr_relay_config`）を**同期 DB 読み**で引き、有効かつ
    /// 宛先が妥当なときだけ [`WebhookConfig`] を返す。未設定 / 無効 / 不正はすべて `None`
    /// （fail-closed = 転記しない）。受信ループから直接呼ぶので、実装は軽い読み 1 回に留め、
    /// await しない。
    ///
    /// 戻り値は actions 層の gateway 非依存な [`WebhookConfig`] で、Nostr crate は Discord
    /// 固有の型に触れない（#191 の筋 / issue #252 の層制約）。
    ///
    /// [`WebhookConfig`]: opencrab_actions::webhook_target::WebhookConfig
    fn resolve_nostr_relay_target(
        &self,
        agent_id: &str,
    ) -> Option<opencrab_actions::webhook_target::WebhookConfig>;

    /// 解決済みの宛先へ 1 件の転記本文を**非ブロック**で送る（issue #252 段階 A）。
    ///
    /// 送信は実装側で fire-and-forget（受信ループを止めない）。送信失敗は**ログのみ**で、
    /// 応答生成や他セッションの受信を巻き込まない。宛先型は actions 層の共通口
    /// （[`WebhookConfig`]）で、Nostr crate は Discord を名指ししない。
    ///
    /// [`WebhookConfig`]: opencrab_actions::webhook_target::WebhookConfig
    fn relay_inbound_notification(
        &self,
        target: &opencrab_actions::webhook_target::WebhookConfig,
        text: String,
    );

    /// このエージェントのワークスペースのルート（#570: 超大受信本文の退避先）。
    ///
    /// 大きい Nostr 受信本文を、tool_result と同じ仕組み
    /// （[`opencrab_actions::sanitize_tool_result_for_log`]）で退避する。退避ファイルは
    /// `<root>/tmp/` に置かれ、エージェントは `ws_read` で読み返せる。したがって
    /// **`ws_read` と同じ resolver でルートを解決すること**（`{agent_id}` を展開した
    /// 実パスを返す。テンプレートのまま返すと #571 の「`{agent_id}` 未展開」になり
    /// 読み返せない）。解決できない agent_id は `None`（退避せず案内だけ残す fail-safe
    /// で、閾値以下の受信は `None` でも従来どおり素通り）。
    ///
    /// 受信ループから同期で呼ぶ（DB も I/O も伴わない純粋なパス解決）。#551 と同じく
    /// 「`workspace_root` を持たない受信側ではなく runner に解決させる」判断に従う。
    fn agent_workspace_root(&self, agent_id: &str) -> Option<std::path::PathBuf>;
}
