//! ターン進行中に「入力中…」を維持し続ける keepalive（#429）。
//!
//! 背景: Discord の typing indicator は 1 回の broadcast で約 10 秒しか持たない。
//! 推論（ツールループ込み）が 10 秒を超えると、応答送信の前に typing が消える。
//! 本番実測（直近14日・対人ターン）で 2 割強が 10 秒を超えていた。
//!
//! ここでは detach したタスクで一定間隔（既定 8 秒 < 失効 10 秒）に typing を打ち直し、
//! 返り値の [`TypingKeepalive`] ガードが drop された瞬間に停止する。ガードはターン本体の
//! future に move して束ねること。ターンが成功・空・`NO_REPLY`・エラー・panic いずれで
//! 終わってもガードが drop され、keepalive タスクは終了する（leak して永遠に typing を
//! 打ち続ける形を作らない）。keepalive はイベントループともターン本体とも別タスクなので、
//! どちらもブロックしない。
//!
//! なぜ serenity の `Typing` を使わず自前かは #429 参照: 「ターン終了・失敗経路で確実に
//! 止まる」ことを実 Discord を叩かずに検証できる形にするため、打つ処理（tick）を差し替え
//! 可能にしてある。中身の loop は serenity の `Typing` と同型（間隔を打ち直すだけ）。

use std::future::Future;
use std::time::Duration;

use tokio::sync::oneshot;

/// typing を打ち直す間隔。Discord の失効（約 10 秒）より短くとる（#429）。
pub(crate) const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(8);

/// 生存中だけ typing を維持するガード。drop で keepalive タスクを停止する。
///
/// `#[must_use]`: 受け取ってすぐ捨てる（`let _ = ...`）と即 drop され keepalive が
/// 意味を為さない。必ず名前付きで束ねてターンの寿命に合わせること。
#[must_use = "drop すると typing keepalive が止まる。ターン本体に束ねて保持すること"]
pub(crate) struct TypingKeepalive {
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for TypingKeepalive {
    fn drop(&mut self) {
        // ベストエフォート。send しなくても Sender の drop で受信側 recv が解決するため
        // keepalive タスクは停止する。明示 send は即時停止のため（次の interval を待たない）。
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

/// `tick` を即時に 1 回、その後 `interval` ごとに呼び直す keepalive タスクを detach で
/// 起こし、停止用ガードを返す。
///
/// `tick` は「typing を 1 回打つ」非同期処理（本番では `broadcast_typing`）。1 回の失敗で
/// 打ち止めにしないよう、エラーは `tick` 側で処理しておくこと（本関数は結果を見ない）。
pub(crate) fn spawn_typing_keepalive<F, Fut>(interval: Duration, mut tick: F) -> TypingKeepalive
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tick().await;
            tokio::select! {
                // ガード drop（明示 send もしくは Sender drop）で解決 → 即停止。
                // biased: 停止シグナルを sleep より先に確認し、drop 後に無駄打ちしない。
                biased;
                _ = &mut stop_rx => break,
                _ = tokio::time::sleep(interval) => {}
            }
        }
    });
    TypingKeepalive {
        stop: Some(stop_tx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // 実 Discord は叩かない。tick は「打った回数」を数えるだけのフェイク。
    fn counting_tick(counter: Arc<AtomicUsize>) -> impl FnMut() -> futures_noop::Ready {
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            futures_noop::Ready
        }
    }

    /// keepalive は 1 回きりではなく、間隔ごとに打ち直す。そしてガードを drop すると
    /// 停止し、以降タスクは 1 回も打たない（leak しない）。
    #[tokio::test]
    async fn refreshes_while_alive_then_stops_on_drop() {
        let counter = Arc::new(AtomicUsize::new(0));
        let interval = Duration::from_millis(15);
        let guard = spawn_typing_keepalive(interval, counting_tick(counter.clone()));

        // 生存中は打ち直す（>=2 で「1 回きりではない」ことを示す。実測の 1 回失効バグを
        // 再現しないことの担保）。
        tokio::time::sleep(Duration::from_millis(90)).await;
        let while_alive = counter.load(Ordering::SeqCst);
        assert!(
            while_alive >= 2,
            "生存中に打ち直していない（one-shot のまま）: {while_alive}"
        );

        // ガードを落としたら停止する。停止後は 2 回サンプルして「増えない」ことを見る
        // ＝ タスクが終了して leak していない（負荷に左右されにくい相対検証）。
        drop(guard);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let after_drop = counter.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let later = counter.load(Ordering::SeqCst);
        assert_eq!(
            after_drop, later,
            "drop 後も typing を打ち続けている（keepalive が leak）: {after_drop} -> {later}"
        );
    }

    /// 本番の束ね方の再現: keepalive ガードを保持したターン future が **エラーで終わっても**、
    /// future 完了時にガードが drop されて keepalive が止まる（失敗経路でも止まることの担保）。
    #[tokio::test]
    async fn stops_when_holding_turn_future_fails() {
        let counter = Arc::new(AtomicUsize::new(0));
        let interval = Duration::from_millis(15);
        let guard = spawn_typing_keepalive(interval, counting_tick(counter.clone()));

        // message_loop.rs の spawn_serialized 内での束ね方（`let _typing = guard;`）を模す。
        let turn = async move {
            let _typing_keepalive = guard;
            tokio::time::sleep(Duration::from_millis(60)).await;
            Err::<(), &'static str>("推論失敗を模擬")
        };
        let result = turn.await;
        assert!(result.is_err(), "ターンはエラーで終わる想定");

        // ターン完了 = ガード drop。以降は打たない。
        let after = counter.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let later = counter.load(Ordering::SeqCst);
        assert_eq!(
            after, later,
            "失敗経路でターンが終わったのに typing が止まらない: {after} -> {later}"
        );
    }

    // tick を 0 コスト・即完了にするための最小 Future（外部依存を増やさない）。
    mod futures_noop {
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        pub struct Ready;
        impl Future for Ready {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
                Poll::Ready(())
            }
        }
    }
}
