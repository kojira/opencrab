//! 経路横断で共有する subtask registry の保持（#169 / 実体は #168 で下位層へ移動）。
//!
//! 実装は gateway 非依存層 [`opencrab_actions::subtask_registries`] にある。
//! server → nostr の依存方向のため、Nostr ゲートウェイからも同じ型を使えるように
//! するには registry 本体（`SubtaskRegistry`）と同じ crate に置く必要があった。
//! ここは既存の呼び出し元（`AppState` / web gateway / REST / heartbeat / E2E）が
//! 使っているパスを保つための再エクスポート。
//!
//! 加えて、同じ「経路横断で共有する subtask 実行状態」の層として
//! [`ProgressDebounce`]（`report_progress` の世代カウンタ / #175 S1）を置く。

use dashmap::DashMap;

pub use opencrab_actions::subtask_registries::SubtaskRegistries;

/// `report_progress` のデバウンス世代カウンタ（parent_session_id → 最新世代 / #175 S1）。
///
/// 短時間に複数回 `report_progress` が呼ばれても、**最後の 1 回だけ**がメインエンジンの
/// 再呼び出し（完了 sink への `SettleKind::Progress` 通知）を発火するようにする。
///
/// # なぜ `AppState` 側に置くのか（`SystemGatewayActions` のフィールドにしてはいけない）
///
/// `SystemGatewayActions` は `run_agent_response` の**実行ごとに生成される**
/// （`crates/server/src/process.rs`）。デバウンス状態をそのフィールドに持たせると、
/// 呼び出しを跨いだ瞬間に世代カウンタが 0 から張り直され、「最後の 1 回だけ」という
/// 間引きが常に成立してしまう＝**全ての進捗報告が発火する**。サブタスクが数秒間に
/// 数回 `report_progress` する典型ケースで、同数の LLM 再呼び出し（コスト増・
/// チャンネルスパム）になる。`AppState`（プロセス寿命の共有状態）に置くことで、
/// 生成し直される gateway を跨いでデバウンスが効く。
///
/// エントリの GC は行わない: 発火時（[`Self::claim_latest`]）にキーを除去するため、
/// 残るのは「発火待ちの最新世代」だけで有界。
#[derive(Default)]
pub struct ProgressDebounce {
    /// parent_session_id → 最新世代番号。
    generations: DashMap<String, u64>,
}

impl ProgressDebounce {
    pub fn new() -> Self {
        Self::default()
    }

    /// キーの世代を 1 進め、呼び出し元の世代番号を返す。
    ///
    /// 返った番号を持って待機し、待機後に [`Self::claim_latest`] で「自分がまだ最新か」
    /// を確認する。後続の `report_progress` が来ていれば世代が進んでいるので発火しない。
    pub fn bump(&self, key: &str) -> u64 {
        let mut gen = self.generations.entry(key.to_string()).or_insert(0);
        *gen += 1;
        *gen
    }

    /// 自分の世代がまだ最新なら発火権を得る（true を返し、エントリを除去する）。
    ///
    /// 除去まで含めて 1 メソッドにしているのは、「最新か確認 → 除去 → 発火」の間に
    /// 別の `bump` が入り込む窓を作らないため。
    pub fn claim_latest(&self, key: &str, generation: u64) -> bool {
        let is_latest = self
            .generations
            .get(key)
            .map(|g| *g == generation)
            .unwrap_or(false);
        if is_latest {
            self.generations.remove(key);
        }
        is_latest
    }

    /// キーの世代エントリを破棄し、待機中のデバウンスを不発にする。
    ///
    /// サブタスクの決着時に呼ぶ（#175 S4）。これが無いと、終了イベントの後に遅延
    /// progress（0〜3 秒窓）が届いて完了返信の直後に余計な推論・重複返信が走る。
    /// 同一親セッションの兄弟サブタスクの保留 progress も巻き添えで消えるが、
    /// progress は advisory であり次の `report_progress` で再アームされる。
    pub fn clear(&self, key: &str) {
        self.generations.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 世代は呼び出しごとに進み、最後の 1 回だけが発火権を得る（バーストの間引き）。
    #[test]
    fn only_the_latest_generation_claims() {
        let debounce = ProgressDebounce::new();
        let g1 = debounce.bump("parent-1");
        let g2 = debounce.bump("parent-1");
        let g3 = debounce.bump("parent-1");
        assert_eq!((g1, g2, g3), (1, 2, 3));
        assert!(!debounce.claim_latest("parent-1", g1));
        assert!(!debounce.claim_latest("parent-1", g2));
        assert!(debounce.claim_latest("parent-1", g3));
        // 発火した世代はエントリごと消える（同じ世代で二重発火しない）。
        assert!(!debounce.claim_latest("parent-1", g3));
    }

    /// キー（親セッション）ごとに独立（別会話の進捗が互いを間引かない）。
    #[test]
    fn keys_are_independent() {
        let debounce = ProgressDebounce::new();
        let a = debounce.bump("parent-a");
        let b = debounce.bump("parent-b");
        assert!(debounce.claim_latest("parent-a", a));
        assert!(debounce.claim_latest("parent-b", b));
    }
}
