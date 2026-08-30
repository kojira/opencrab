//! 車線をまたぐ同一イベントの said 重複排除（TTL 付き seen セット）。
//!
//! 1 instance は default(メンション)車線と各 watch 車線がそれぞれ独立の `nostaro watch`
//! 購読を持つ。あるイベントがメンション条件と watch 条件の両方に当たると、両車線が同じ
//! event_id を line として吐き、それぞれ `said` する。origin は車線名を含む
//! （`…:default:<id>` と `…:watch:4:<id>`）ため下流の gate は別メッセージ扱いになり、
//! 会話重複・二重ターン・NO_REPLY 対を生む（#839 の観測: gateway seq3023/3024）。
//!
//! ここで event_id をキーに「最初に握った車線だけが送る」ようにする。gate の per-origin
//! dedup は同一車線の再購読（同じ origin）を既に潰すので、ここが埋めるのは **車線をまたぐ**
//! 取りこぼしだけ。TTL は「メンションを即時送出した後に、同じイベントを載せた watch bundle
//! が interval 後に flush される」窓を跨げるよう、最大 watch interval を上回る値にする。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// event_id → 失効時刻。`claim` 時に失効分を掃除する（別スレッド不要の遅延 GC）。
#[derive(Debug)]
pub struct SeenEvents {
    ttl: Duration,
    map: Mutex<HashMap<String, Instant>>,
}

impl SeenEvents {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// この event_id を初めて握ったら `true`（＝この車線が送ってよい）。
    /// TTL 内に既出なら `false`（別車線が既に送った/送る）。呼び出しは実際に送る直前に置く。
    pub fn claim(&self, event_id: &str) -> bool {
        self.claim_at(event_id, Instant::now())
    }

    fn claim_at(&self, event_id: &str, now: Instant) -> bool {
        let mut map = self.map.lock().expect("seen-events mutex poisoned");
        map.retain(|_, expiry| *expiry > now);
        if map.contains_key(event_id) {
            return false;
        }
        map.insert(event_id.to_string(), now + self.ttl);
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_claim_wins_second_is_dup() {
        let seen = SeenEvents::new(Duration::from_secs(600));
        assert!(seen.claim("aa"), "初回は握れる");
        assert!(!seen.claim("aa"), "同一 id の二度目は重複");
        assert!(seen.claim("bb"), "別 id は独立");
    }

    #[test]
    fn expired_entry_can_be_reclaimed() {
        let seen = SeenEvents::new(Duration::from_millis(10));
        let t0 = Instant::now();
        assert!(seen.claim_at("aa", t0));
        assert!(!seen.claim_at("aa", t0 + Duration::from_millis(5)), "TTL 内は重複");
        // TTL 経過後は再び握れる（掃除される）。
        assert!(seen.claim_at("aa", t0 + Duration::from_millis(20)));
    }

    #[test]
    fn claim_evicts_expired_entries() {
        let seen = SeenEvents::new(Duration::from_millis(10));
        let t0 = Instant::now();
        assert!(seen.claim_at("aa", t0));
        assert!(seen.claim_at("bb", t0));
        assert_eq!(seen.len(), 2);
        // 失効後に別 id を握ると、掃除で古い 2 件が消えて 1 件になる。
        assert!(seen.claim_at("cc", t0 + Duration::from_millis(20)));
        assert_eq!(seen.len(), 1, "失効エントリは claim 時に掃除される");
    }
}
