//! ターン進行中に Discord の typing インジケータを維持する keepalive（設計 §5.4）。
//!
//! 背景: Discord の typing indicator は 1 回の broadcast で約 10 秒しか持たない。ツールループ
//! 込みの推論が 10 秒を超えると、応答（say）送信の前に typing が消える。ここでは core の
//! platform-neutral activity（started〜ended）を機械的に typing 開始/keepalive/停止へ写す:
//! `activity started` で keepalive を起こし、一定間隔（既定 8 秒 < 失効 10 秒）で打ち直し、
//! `activity ended` で止める。typing failure は best-effort とし、say / operation call /
//! delivery state を変更しない（設計 §5.4）。
//!
//! detach したタスクで tick を打ち、返り値の [`TypingKeepalive`] ガードが drop された瞬間に
//! 停止する。ガードは say consumer のローカル変数として保持し、ended（またはエラー）で drop
//! する。tick（typing を 1 回打つ処理）と interval を注入可能にしてあるので、実 Discord を
//! 叩かずに「started で打つ・ended で止まる・周期」をテストできる。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::transport::{DiscordTransport, TransportOutcome};

/// typing を打ち直す間隔。Discord の失効（約 10 秒）より短くとる（設計 §5.4）。
pub(crate) const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(8);

/// 生存中だけ typing を維持するガード。drop で keepalive タスクを停止する。
///
/// `#[must_use]`: 受け取ってすぐ捨てる（`let _ = ...`）と即 drop され keepalive が意味を為さない。
/// 必ず名前付きで束ね、ターン（activity started〜ended）の寿命に合わせて保持すること。
#[must_use = "drop すると typing keepalive が止まる。ターンの寿命に束ねて保持すること"]
pub(crate) struct TypingKeepalive {
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for TypingKeepalive {
    fn drop(&mut self) {
        // ベストエフォート。send しなくても Sender の drop で受信側 recv が解決するため keepalive
        // タスクは停止する。明示 send は即時停止のため（次の interval を待たない）。
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

/// `tick` を即時に 1 回、その後 `interval` ごとに呼び直す keepalive タスクを detach で起こし、
/// 停止用ガードを返す。
///
/// `tick` は「typing を 1 回打つ」非同期処理。1 回の失敗で打ち止めにしないよう、エラーは `tick`
/// 側で握りつぶしておくこと（本関数は結果を見ない）。
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
                _ = &mut stop_rx => break,
                _ = tokio::time::sleep(interval) => {}
            }
        }
    });
    TypingKeepalive {
        stop: Some(stop_tx),
    }
}

/// transport 経由で `channel_id` に typing を打つ keepalive を起こす。broadcast_typing の三結果は
/// best-effort として握りつぶす（失敗しても turn を壊さない・設計 §5.4）。
pub(crate) fn spawn_channel_typing(
    transport: Arc<dyn DiscordTransport>,
    channel_id: String,
    interval: Duration,
) -> TypingKeepalive {
    spawn_typing_keepalive(interval, move || {
        let transport = transport.clone();
        let channel_id = channel_id.clone();
        async move {
            match transport.broadcast_typing(&channel_id).await {
                TransportOutcome::Ok(_) => {}
                TransportOutcome::Rejected => {
                    tracing::debug!(channel = %channel_id, "typing rejected (non-fatal)")
                }
                TransportOutcome::Indeterminate => {
                    tracing::debug!(channel = %channel_id, "typing outcome unknown (non-fatal)")
                }
            }
        }
    })
}

/// activity state に応じて typing keepalive の寿命を更新する。say consumer が保持する `current`
/// を受け取り、更新後の値を返す。
///
/// - `"started"`: keepalive を起こす（既存があれば張り替え——古いガードは drop されて止まる）。
/// - それ以外（`"ended"` 等の終端）: 停止（`None` を返し、`current` を drop する）。
pub(crate) fn apply_activity_state(
    current: Option<TypingKeepalive>,
    state: &str,
    transport: &Arc<dyn DiscordTransport>,
    channel_id: &str,
    interval: Duration,
) -> Option<TypingKeepalive> {
    match state {
        "started" => {
            // 既存があれば張り替え——古いガードを止めてから新規を起こす。
            drop(current);
            Some(spawn_channel_typing(
                transport.clone(),
                channel_id.to_string(),
                interval,
            ))
        }
        // ended その他の終端: 進行中の keepalive を止める（ガード drop）。
        _ => {
            drop(current);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::testfake::RecordingTransport;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn keepalive_ticks_immediately_then_stops_on_drop() {
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        // 長い interval にして「即時 1 回」だけを観測（周期待ちを挟まない）。
        let guard = spawn_typing_keepalive(Duration::from_secs(3600), move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });
        // 即時 tick が走るのを待つ。
        for _ in 0..50 {
            if count.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(count.load(Ordering::SeqCst), 1, "started で即時 1 回打つ");
        drop(guard);
        // drop 後は増えない。
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(count.load(Ordering::SeqCst), 1, "drop で停止し打ち直さない");
    }

    #[tokio::test]
    async fn keepalive_reticks_on_interval() {
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        // 短い interval で周期打ちを観測（8 秒を待たずに検証できる形＝時間注入）。
        let guard = spawn_typing_keepalive(Duration::from_millis(10), move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });
        for _ in 0..100 {
            if count.load(Ordering::SeqCst) >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            count.load(Ordering::SeqCst) >= 3,
            "interval ごとに打ち直す（実測 {}）",
            count.load(Ordering::SeqCst)
        );
        drop(guard);
    }

    #[tokio::test]
    async fn apply_activity_started_broadcasts_then_ended_stops() {
        let rec = Arc::new(RecordingTransport::default());
        let transport: Arc<dyn DiscordTransport> = rec.clone();
        // started: keepalive 起動＝即時 broadcast_typing が走る。長い interval で「即時 1 回」だけ観測。
        let guard = apply_activity_state(
            None,
            "started",
            &transport,
            "12345",
            Duration::from_secs(3600),
        );
        assert!(guard.is_some(), "started で keepalive を起こす");
        for _ in 0..50 {
            if rec.typing_count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(rec.typing_count(), 1, "started で typing を 1 回打つ");
        // ended: 停止（None）。ガード drop で以降打たない。
        let guard = apply_activity_state(
            guard,
            "ended",
            &transport,
            "12345",
            Duration::from_secs(3600),
        );
        assert!(guard.is_none(), "ended で停止する");
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(rec.typing_count(), 1, "ended 後は打ち直さない");
    }
}
