//! 時刻起因のターン発火（#588 TimedFire）。
//!
//! scheduler が「時刻が来たら、このセッションで・このプロンプトで 1 ターン回して」と各ゲートウェイの
//! **既存のターンループ**へ渡すための、transport 中立な口。狙いは「入り口だけ特別、その先は通常ルート」:
//! ハートビートも #455 定時実行もアラームも、違うのは**いつ送るか**だけで、受ける側は同じ。したがって
//! このイベントは**種別を知らない**（`prompt` と宛先だけを運ぶ）。
//!
//! 受け口（各ゲートウェイの [`TimedFireSink`] 実装）は**薄く保つ**: 受けて自分の既存 turn を回すだけで、
//! 配送・ロック・記録・リアクション・継続ターンは全部ゲートウェイの既存実装が担う。これで
//! ハートビート専用の配送（旧 `heartbeat_delivery.rs`）や専用ターン（旧 `run_one_heartbeat`）が不要になる。

use crate::CallerIdentity;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// ログ用のプロンプト先頭プレビュー。全文は長いので先頭だけ・改行は空白に潰して 1 行にする
/// （時刻発火の送信側 scheduler と受信側の各ループが**同じ形**で出し、grep で突き合わせられるように）。
pub fn prompt_preview(prompt: &str) -> String {
    const MAX_CHARS: usize = 80;
    let one_line = prompt.replace(['\n', '\r'], " ");
    let head: String = one_line.chars().take(MAX_CHARS).collect();
    if one_line.chars().count() > MAX_CHARS {
        format!("{head}…")
    } else {
        head
    }
}

/// 時刻起因でセッションのターンを 1 回回す要求（transport 中立）。
pub struct TimedFireRequest {
    /// 発火先＝実会話セッション（`nostr-{agent}` / `discord-{agent}-{guild}-{channel}`）。
    pub session_id: String,
    pub agent_id: String,
    /// transport 固有のチャンネル token（Discord は数値文字列、Nostr broadcast は空）。
    pub channel_id: String,
    /// Discord のギルド ID（DM とギルドの判別に使う。Nostr は空）。transport 固有だが、
    /// 発火先セッションの一部（`discord-{agent}-{guild}-{channel}`）なので scheduler が持っている。
    pub guild_id: String,
    /// その回に渡すプロンプト。受け口は**system プロンプトへ足す**（会話ログに「発言」として残さない）。
    pub prompt: String,
    /// 実行権限。時刻発火は本人の自己実行なので通常 `Owner`。
    pub caller: CallerIdentity,
}

/// ゲートウェイのループが実装する受け口。**受けて既存の turn を回すだけ**（薄く保つ）。
///
/// 実装は自分のループへイベント/ジョブを 1 本流すだけ（Discord は `LoopEvent::TimedFire`、Nostr は
/// 対応する `ResponseJob`）。非ブロック（発火側＝scheduler を塞がない）。
pub trait TimedFireSink: Send + Sync {
    fn fire_timed_turn(&self, req: TimedFireRequest);
}

/// `agent → 受け口` の登録簿（#588 TimedFire の**唯一の新規機構**）。
///
/// 各ゲートウェイのループが起動時に自分の受け口を登録し、scheduler が発火時に
/// [`TimedFireRouter::resolve`] で引く。**per-agent ゲートウェイ（自分のボット名で出る・#400）を優先**し、
/// 無ければ共有（TOML）ゲートウェイへ落ちる。この per-agent→共有の解決は、撤去した
/// `HeartbeatDiscordHttp`（http ハンドルの per-agent→共有）を「送り先ループの選択」へ置き換えたもの
/// ＝送信者の同一性（#400）はループが自分の gateway で送ることで自然に保たれる。
/// `(transport kind, agent_id)` → per-agent の受け口。
type PerAgentSinks = Mutex<HashMap<(&'static str, String), Arc<dyn TimedFireSink>>>;
/// transport kind → 共有（TOML）ゲートウェイの受け口。
type SharedSinks = Mutex<HashMap<&'static str, Arc<dyn TimedFireSink>>>;

#[derive(Default)]
pub struct TimedFireRouter {
    /// per-agent ゲートウェイの受け口（そのボット名で出る）。
    per_agent: PerAgentSinks,
    /// 共有（TOML）ゲートウェイの受け口（per-agent が無いエージェントの落ち先）。
    shared: SharedSinks,
}

impl TimedFireRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// per-agent ゲートウェイの受け口を登録する（そのボット名で出る）。
    pub fn register_per_agent(
        &self,
        kind: &'static str,
        agent_id: &str,
        sink: Arc<dyn TimedFireSink>,
    ) {
        self.per_agent
            .lock()
            .unwrap()
            .insert((kind, agent_id.to_string()), sink);
    }

    /// per-agent ゲートウェイの受け口を解除する（そのゲートウェイの停止時）。
    ///
    /// 解除すると [`TimedFireRouter::resolve`] は共有（TOML）ゲートウェイの受け口へ落ちる。これで
    /// 「専用ゲートウェイが動いている間はその体で、止まっていれば共有で」という #400 の動的
    /// フォールバックを保つ（旧 `HeartbeatDiscordHttp` が配送時に `get_http_for_agent` の None で
    /// 落としていたのと同じ挙動）。停止後も登録が残ると死んだループへ発火が消えるので必ず解除する。
    pub fn unregister_per_agent(&self, kind: &'static str, agent_id: &str) {
        self.per_agent
            .lock()
            .unwrap()
            .remove(&(kind, agent_id.to_string()));
    }

    /// 共有（TOML）ゲートウェイの受け口を登録する（per-agent が無いエージェントの落ち先）。
    pub fn register_shared(&self, kind: &'static str, sink: Arc<dyn TimedFireSink>) {
        self.shared.lock().unwrap().insert(kind, sink);
    }

    /// 発火先の受け口を引く（per-agent 優先 → 共有）。無ければ `None`（送れないので発火を諦める）。
    pub fn resolve(&self, kind: &'static str, agent_id: &str) -> Option<Arc<dyn TimedFireSink>> {
        if let Some(s) = self
            .per_agent
            .lock()
            .unwrap()
            .get(&(kind, agent_id.to_string()))
        {
            return Some(s.clone());
        }
        self.shared.lock().unwrap().get(kind).cloned()
    }

    /// その kind に per-agent か共有のいずれかの受け口が**1 つでも**登録されているか
    /// （起動時セルフチェック用・#603）。有効な受信ゲートウェイがあるのにこれが false なら、
    /// その transport の時刻発火はどこにも届かない（配線漏れ / 起動失敗）。
    pub fn has_sink_for_kind(&self, kind: &str) -> bool {
        self.per_agent
            .lock()
            .unwrap()
            .keys()
            .any(|(k, _)| *k == kind)
            || self.shared.lock().unwrap().contains_key(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSink(Arc<AtomicUsize>);
    impl TimedFireSink for CountingSink {
        fn fire_timed_turn(&self, _req: TimedFireRequest) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// ログ用プレビュー: 短文はそのまま・改行は空白へ・80 文字超は … で切る（送受で同形）。
    #[test]
    fn prompt_preview_is_one_line_and_truncated() {
        assert_eq!(prompt_preview("短いプロンプト"), "短いプロンプト");
        // 改行/復帰は 1 行に潰す（grep で 1 行に収まる）。
        assert_eq!(
            prompt_preview("一行目\n二行目\r三行目"),
            "一行目 二行目 三行目"
        );
        // 80 文字ちょうどは切らない、超えたら … を付ける。
        let exactly_80 = "あ".repeat(80);
        assert_eq!(prompt_preview(&exactly_80), exactly_80);
        let over = "あ".repeat(81);
        let preview = prompt_preview(&over);
        assert!(preview.ends_with('…'), "超過は … で切る: {preview}");
        assert_eq!(preview.chars().count(), 81, "先頭 80 文字 + …");
    }

    /// per-agent を優先し、無ければ共有へ落ちる（#400 の per-agent→共有と同型）。
    #[test]
    fn resolves_per_agent_first_then_shared() {
        let router = TimedFireRouter::new();
        let per = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(AtomicUsize::new(0));
        router.register_shared("discord", Arc::new(CountingSink(shared.clone())));
        router.register_per_agent("discord", "crab", Arc::new(CountingSink(per.clone())));

        // crab は per-agent がある → per-agent を引く。
        router
            .resolve("discord", "crab")
            .unwrap()
            .fire_timed_turn(req("crab"));
        assert_eq!(
            (per.load(Ordering::SeqCst), shared.load(Ordering::SeqCst)),
            (1, 0)
        );

        // other は per-agent が無い → 共有へ落ちる。
        router
            .resolve("discord", "other")
            .unwrap()
            .fire_timed_turn(req("other"));
        assert_eq!(
            (per.load(Ordering::SeqCst), shared.load(Ordering::SeqCst)),
            (1, 1)
        );

        // 登録の無い transport は None（送れない）。
        assert!(router.resolve("nostr", "crab").is_none());
    }

    /// 起動時セルフチェック（#603）: per-agent でも共有でも 1 つあれば true、無ければ false。
    #[test]
    fn has_sink_for_kind_detects_missing_registration() {
        let router = TimedFireRouter::new();
        let counter = Arc::new(AtomicUsize::new(0));
        // 何も登録していないので全 kind false（これが #602 の「受け口が 0」状態）。
        assert!(!router.has_sink_for_kind("discord"));
        assert!(!router.has_sink_for_kind("nostr"));

        // per-agent を 1 つ登録 → その kind だけ true。
        router.register_per_agent("nostr", "crab", Arc::new(CountingSink(counter.clone())));
        assert!(router.has_sink_for_kind("nostr"));
        assert!(!router.has_sink_for_kind("discord"));

        // 共有だけでも true（per-agent 不在でも落ち先がある）。
        router.register_shared("discord", Arc::new(CountingSink(counter)));
        assert!(router.has_sink_for_kind("discord"));
    }

    fn req(agent: &str) -> TimedFireRequest {
        TimedFireRequest {
            session_id: format!("discord-{agent}-1-2"),
            agent_id: agent.to_string(),
            channel_id: "2".to_string(),
            guild_id: "1".to_string(),
            prompt: "p".to_string(),
            caller: CallerIdentity::Owner,
        }
    }
}
