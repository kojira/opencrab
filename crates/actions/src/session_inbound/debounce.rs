use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::CallerIdentity;

/// 権限毎デバウンスの内部バッファ。タイマーは [`PrivilegeFire`] が持つ。
#[derive(Debug)]
struct PrivilegeDebounce<T> {
    buckets: BTreeMap<u64, PrivilegeBucket<T>>,
}

#[derive(Debug)]
struct PrivilegeBucket<T> {
    items: Vec<T>,
    due: tokio::time::Instant,
}

impl<T> Default for PrivilegeDebounce<T> {
    fn default() -> Self {
        Self {
            buckets: BTreeMap::new(),
        }
    }
}

impl<T> PrivilegeDebounce<T> {
    fn push(&mut self, item: T, interval_secs: u64, now: tokio::time::Instant) {
        self.buckets
            .entry(interval_secs)
            .or_insert_with(|| PrivilegeBucket {
                items: Vec::new(),
                due: now + Duration::from_secs(interval_secs),
            })
            .items
            .push(item);
    }

    fn next_due(&self) -> Option<tokio::time::Instant> {
        self.buckets.values().map(|b| b.due).min()
    }

    #[cfg(test)]
    fn intervals(&self) -> Vec<u64> {
        self.buckets.keys().copied().collect()
    }

    fn take_ready(&mut self, now: tokio::time::Instant) -> Vec<(u64, Vec<T>)> {
        let keys: Vec<u64> = self
            .buckets
            .iter()
            .filter(|(_, b)| b.due <= now)
            .map(|(&k, _)| k)
            .collect();
        keys.into_iter()
            .filter_map(|k| self.buckets.remove(&k).map(|b| (k, b.items)))
            .collect()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.values().map(|b| b.items.len()).sum()
    }
}

struct PrivilegeHeld<T> {
    item: T,
    caller: CallerIdentity,
}

struct PrivilegeFireInner<T> {
    buf: Mutex<PrivilegeDebounce<PrivilegeHeld<T>>>,
    notify: tokio::sync::Notify,
    abort: Mutex<Option<tokio::task::AbortHandle>>,
}

impl<T> Drop for PrivilegeFireInner<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.abort.lock().unwrap().take() {
            handle.abort();
        }
    }
}

/// 権限デバウンスの core 側ランタイム。バッファと時限タスクを内包する。
///
/// ゲートは寿命を合わせ、時限到達で渡すクロージャ（ターン起動）だけを渡す。
/// `next_due` / 保留 / 間隔はゲートに出さない。
pub struct PrivilegeFire<T> {
    inner: Arc<PrivilegeFireInner<T>>,
}

impl<T> Clone for PrivilegeFire<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: Send + 'static> PrivilegeFire<T> {
    /// `on_due` は間隔到達時に core が呼ぶ。再 `accept_inbound` ではない。
    /// 渡す件がそのターンの文脈（読んだ事実）。ゲートはここで 👀 を付ける。
    pub fn new<F, Fut>(on_due: F) -> Self
    where
        F: Fn(Vec<(T, CallerIdentity)>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let inner = Arc::new(PrivilegeFireInner {
            buf: Mutex::new(PrivilegeDebounce::default()),
            notify: tokio::sync::Notify::new(),
            abort: Mutex::new(None),
        });
        let on_due = Arc::new(on_due);
        let worker = Arc::clone(&inner);
        let handle = tokio::spawn(async move {
            loop {
                let due = worker.buf.lock().unwrap().next_due();
                match due {
                    None => worker.notify.notified().await,
                    Some(at) => {
                        tokio::select! {
                            _ = tokio::time::sleep_until(at) => {
                                let groups = worker
                                    .buf
                                    .lock()
                                    .unwrap()
                                    .take_ready(tokio::time::Instant::now());
                                for (_interval, held) in groups {
                                    let items: Vec<(T, CallerIdentity)> = held
                                        .into_iter()
                                        .map(|h| (h.item, h.caller))
                                        .collect();
                                    if !items.is_empty() {
                                        on_due(items).await;
                                    }
                                }
                            }
                            _ = worker.notify.notified() => {}
                        }
                    }
                }
            }
        });
        *inner.abort.lock().unwrap() = Some(handle.abort_handle());
        Self { inner }
    }

    pub(crate) fn hold(&self, item: T, caller: CallerIdentity, interval_secs: u64) {
        self.inner.buf.lock().unwrap().push(
            PrivilegeHeld { item, caller },
            interval_secs,
            tokio::time::Instant::now(),
        );
        self.inner.notify.notify_one();
    }
}

impl<T> PrivilegeFire<T> {
    #[cfg(test)]
    pub(crate) fn held_intervals(&self) -> Vec<u64> {
        self.inner.buf.lock().unwrap().intervals()
    }

    #[cfg(test)]
    pub(crate) fn held_len(&self) -> usize {
        self.inner.buf.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CallerIdentity;

    #[test]
    fn privilege_debounce_flushes_each_interval() {
        let mut hold = PrivilegeDebounce::default();
        let now = tokio::time::Instant::now();
        hold.push("fast", 30, now);
        hold.push("slow", 300, now);
        assert_eq!(hold.intervals(), vec![30, 300]);
        let at_watch = hold.take_ready(now + Duration::from_secs(60));
        assert_eq!(at_watch.len(), 1);
        assert_eq!(at_watch[0].0, 30);
        assert_eq!(at_watch[0].1, vec!["fast"]);
        assert_eq!(hold.intervals(), vec![300]);
        let later = hold.take_ready(now + Duration::from_secs(300));
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].0, 300);
        assert_eq!(later[0].1, vec!["slow"]);
        assert!(hold.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn privilege_fire_emits_each_interval() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let fire = PrivilegeFire::new(move |items: Vec<(String, CallerIdentity)>| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(items);
            }
        });
        fire.hold("fast".into(), CallerIdentity::Owner, 30);
        fire.hold("slow".into(), CallerIdentity::Agent, 300);
        tokio::time::advance(Duration::from_secs(30)).await;
        let first = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("30s で fast が発火する")
            .expect("channel open");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, "fast");
        assert_eq!(fire.held_intervals(), vec![300]);
        tokio::time::advance(Duration::from_secs(270)).await;
        let later = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("300s で slow が発火する")
            .expect("channel open");
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].0, "slow");
        assert_eq!(fire.held_len(), 0);
    }
}
