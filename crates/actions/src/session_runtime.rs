//! セッション単位の直列化ランタイム（#190 S1 / 方針は #191）。
//!
//! ゲートウェイ（web / Nostr）は「1 セッション = 1 会話」の単位で応答生成を直列化する
//! 必要がある。inbound の応答生成と subtask 完了 resume の応答生成が同一セッションで
//! 並行すると、両者が同じ会話履歴から独立に返信を組み立てて**二重投稿**になる
//! （RFC #152 §6 の不変条件違反）。
//!
//! この直列化は web（`WebGateway`）と Nostr（`NostrSessionRuntime`）で文字レベルまで
//! ほぼ同一のコードとして二重に実装されていた。差は可視性と、web が SSE 配信チャンネルを
//! 同じ構造体に同居させていた点だけだった。同じ不変条件を守るコードが 2 箇所にあると
//! 片方だけ直る（= 片方だけ壊れる）ため、ここ 1 つに寄せる。
//!
//! 併せて dispatch 用の登録簿（[`SubtaskRegistries`]）も保持する。inbound と resume が
//! **同一 Arc の registry** を共有することが `cancel_subtask`（#161）が走行中 subtask に
//! 到達できる条件であり、直列化と同じ「セッション単位の実行状態」だからである。
//!
//! ここに置く理由（依存方向）: 依存は server → nostr であり、Nostr 側から server の型は
//! 参照できない。registry 本体（`SubtaskRegistry`）と同じ gateway 非依存層に置くことで、
//! どのゲートウェイからも同じ 1 実装を使える。
//!
//! Discord は受信ループ形のため「spawn 込みで応答を返さない」という別形だが、そのために
//! ロック表を二重に持つ理由はない。[`SessionRuntime::spawn_serialized`]（`run_serialized` を
//! `tokio::spawn` で包むだけの薄い入口）へ寄せ、ロックの生成・取得・回収の実装はここ 1 つに
//! なった（#156 S2）。

use std::future::Future;
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;

use crate::subtask::SubtaskRegistry;
use crate::subtask_registries::SubtaskRegistries;

/// セッション単位の直列化ロックと dispatch 登録簿を持つ共有ランタイム。
///
/// ゲートウェイはプロセス全体で 1 つ（`Arc<SessionRuntime>`）保持し、inbound 経路と
/// 完了 sink が同じ Arc を共有する。ロックの粒度は session_id なので、別の相手・別の
/// エージェント・別の会話は従来どおり並行する。
#[derive(Default)]
pub struct SessionRuntime {
    /// session_id → 直列化ロック。アイドルになったエントリは回収する（下記参照）。
    session_locks: DashMap<String, Arc<Mutex<()>>>,
    registries: SubtaskRegistries,
}

impl SessionRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// セッションの dispatch registry を取得する（無ければ生成し、inbound/resume で共有）。
    ///
    /// 同じ session_id には常に同じ `Arc` を返す。これが `cancel_subtask` の到達条件。
    pub fn registry_for(&self, session_id: &str) -> SubtaskRegistry {
        self.registries.registry_for(session_id)
    }

    /// セッションに走行中（未決着）の dispatch subtask があるか。
    ///
    /// `settle_completed` は sink 発火より前に registry から除去するため、決着後は false。
    pub fn has_running(&self, session_id: &str) -> bool {
        self.registries.has_running(session_id)
    }

    /// セッションのロックエントリが残っているか（回収の観測点）。
    ///
    /// 走行中は true、待機者がいなくなれば false。`session_locks` を private に保ったまま
    /// 「アイドルなロックが回収される」ことを外側の層からも検証できるようにするための
    /// 読み取り専用アクセサ（エントリを新規生成しない）。
    pub fn holds_lock_entry(&self, session_id: &str) -> bool {
        self.session_locks.contains_key(session_id)
    }

    fn lock_for(&self, session_id: &str) -> Arc<Mutex<()>> {
        self.session_locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// 待機者がいなければロックエントリを回収する（#39 相当のリーク防止）。
    ///
    /// マップ内の 1 本だけが残っている（`strong_count == 1`）なら誰も待っていない。
    /// `remove_if` はシャードロック下で判定するため、判定と削除の間に新しい待機者が
    /// 割り込むことはない。
    fn release_lock_if_idle(&self, session_id: &str) {
        self.session_locks
            .remove_if(session_id, |_, lock| Arc::strong_count(lock) == 1);
    }

    /// 同一セッションのロック下で `fut` を実行する（per-session 直列化）。
    ///
    /// - 同一セッションは直列（inbound と subtask 完了 resume を割り込ませない）。
    /// - 異なるセッション（別の相手 / 別エージェント / 別会話）は並行。
    ///
    /// 呼び出し側の注意: ゲートウェイ層はこれを**直接公開しない**。生の応答生成を
    /// private に閉じ、直列化込みの入口だけを公開する。直列化を呼び出し側の責務に
    /// すると 1 箇所の呼び忘れで不変条件が壊れ、テストでは検出できない。
    ///
    /// 閉じ込めの強度は実装によって差がある。web は**応答生成と完了受け口を別モジュール**に
    /// 置いている（兄弟モジュールは互いの private に届かない）ので、受け口から生の応答生成を
    /// 直呼びすると**コンパイルエラーになる**。Nostr は**同一モジュール内の private メソッド**
    /// なので、同じモジュールにある完了受け口から直呼びできてしまう（呼び忘れがコンパイルでは
    /// 止まらない）。新しいゲートウェイを作るときは web 側の形に倣うこと。
    pub async fn run_serialized<F, T>(&self, session_id: &str, fut: F) -> T
    where
        F: Future<Output = T>,
    {
        let lock = self.lock_for(session_id);
        let guard = lock.lock().await;
        let out = fut.await;
        drop(guard);
        drop(lock);
        self.release_lock_if_idle(session_id);
        out
    }

    /// `fut` を「同一セッションのロック下で走らせるタスク」として spawn する（結果は返さない）。
    ///
    /// 受信ループ形のゲートウェイ（Discord）向けの薄い入口。中身は [`Self::run_serialized`]
    /// を `tokio::spawn` で包むだけで、ロックの生成・取得・アイドル回収は同じ 1 実装を通る。
    ///
    /// **ロックの取得は spawn した中で行う**（呼び出し側では await しない）。受信ループが
    /// 取得を待つと、走行中セッションの推論が終わるまで**全チャンネル・全エージェント**の
    /// 受信が止まる（過去にこれでデッドロックを作った）。応答の値を返さないのも同じ理由で、
    /// 「呼び出し側は結果を待たない」ことを型で示している。
    ///
    /// 渡す `fut` には**受信の記録から応答の記録まで**を丸ごと入れること。ロック取得の後に
    /// 受信の記録と履歴構築が来る順序が崩れると、確定前の履歴から 2 本目の返信が組み立てられて
    /// 二重投稿になる（[`Self::run_serialized`] の注意書きと同じ不変条件）。
    ///
    /// 返す `JoinHandle` は破棄してよい（テストが完了を待てるように返しているだけ）。
    pub fn spawn_serialized<F>(
        self: &Arc<Self>,
        session_id: impl Into<String>,
        fut: F,
    ) -> tokio::task::JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let runtime = Arc::clone(self);
        let session_id = session_id.into();
        tokio::spawn(async move {
            runtime.run_serialized(&session_id, fut).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// 同一セッションの registry は同じ Arc（dispatcher と `cancel_subtask` が同じものを見る）。
    #[test]
    fn registry_is_shared_per_session() {
        let rt = SessionRuntime::new();
        let a = rt.registry_for("s-1");
        let b = rt.registry_for("s-1");
        assert!(Arc::ptr_eq(&a, &b));
        // 別セッションは独立。
        let c = rt.registry_for("s-2");
        assert!(!Arc::ptr_eq(&a, &c));
    }

    /// 未知のセッションは走行中でない（読み取りが registry を作らない）。
    #[test]
    fn has_running_is_false_without_dispatch() {
        let rt = SessionRuntime::new();
        assert!(!rt.has_running("s-none"));
        let _ = rt.registry_for("s-none");
        assert!(!rt.has_running("s-none"));
    }

    /// per-session 直列化: 同一セッションの並行実行は同時実行数 1 になる。
    #[tokio::test]
    async fn same_session_serializes() {
        let rt = Arc::new(SessionRuntime::new());
        let inflight = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let rt = rt.clone();
            let inflight = inflight.clone();
            let max_concurrent = max_concurrent.clone();
            handles.push(tokio::spawn(async move {
                rt.run_serialized("s-serial", async move {
                    let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    inflight.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "同一セッションは直列でなければならない（二重応答の防止）"
        );
    }

    /// 異なるセッションは互いをブロックしない。
    #[tokio::test]
    async fn different_sessions_run_concurrently() {
        let rt = Arc::new(SessionRuntime::new());
        let rt1 = rt.clone();
        let block = tokio::spawn(async move {
            rt1.run_serialized("s-blocking", async {
                tokio::time::sleep(Duration::from_millis(200)).await;
            })
            .await;
        });
        tokio::time::sleep(Duration::from_millis(10)).await;

        tokio::time::timeout(
            Duration::from_millis(100),
            rt.run_serialized("s-free", async {}),
        )
        .await
        .expect("別セッションは他セッションのロックで待たされてはならない");

        block.abort();
    }

    /// 待機者のいないロックエントリは回収される（リーク防止）。
    #[tokio::test]
    async fn idle_lock_is_reclaimed() {
        let rt = SessionRuntime::new();
        rt.run_serialized("s-reclaim", async {}).await;
        assert!(
            !rt.holds_lock_entry("s-reclaim"),
            "待機者がいないロックは回収される"
        );
        assert!(rt.session_locks.is_empty());
    }

    /// **解放後に到着した取得も、待機列に並んでいる先客の後ろに付く**。
    ///
    /// 回収の判定（待機者がいないときだけ外す）を「無条件に外す」に変えると、
    /// 先客が待っている最中にエントリが消え、後から来た取得が**別のロックを新規に作って**
    /// 先客と同時に走る（＝同一セッションで二重に応答する）。既存の直列テストは 5 本が
    /// ほぼ同時に取得を通るため「解放後に遅れて到着する取得」を一度も作らず、この壊れ方を
    /// 検知できない。ここで明示的にその順序を作る。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn late_arrival_serializes_behind_a_queued_waiter() {
        let rt = Arc::new(SessionRuntime::new());
        let inflight = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let sid = "s-late";

        let spawn_worker = |delay_ms: u64, hold_ms: u64| {
            let rt = rt.clone();
            let inflight = inflight.clone();
            let max_concurrent = max_concurrent.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                rt.run_serialized(sid, async {
                    let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(hold_ms)).await;
                    inflight.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            })
        };

        // A: 即座に取得して 60ms 保持 / B: 10ms 後に到着（A の待機列に並ぶ）
        // C: 80ms 後に到着（A は解放済み。回収が無条件なら別ロックを作って B と並走する）
        let a = spawn_worker(0, 60);
        let b = spawn_worker(10, 60);
        let c = spawn_worker(80, 20);

        // B が走行中（A 解放後）にエントリが残っていること。
        tokio::time::sleep(Duration::from_millis(70)).await;
        assert!(
            rt.holds_lock_entry(sid),
            "待機者が残っているあいだはロックエントリを回収してはならない"
        );

        for h in [a, b, c] {
            h.await.expect("worker panicked");
        }
        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "解放後に到着した取得が先客と並走している（同一セッションの直列化が破れている）"
        );
        assert!(!rt.holds_lock_entry(sid), "最後は回収される");
    }

    /// 走行中はロックエントリが残る（回収が早すぎないことの裏取り）。
    #[tokio::test]
    async fn lock_entry_exists_while_running() {
        let rt = Arc::new(SessionRuntime::new());
        let rt1 = rt.clone();
        let running = tokio::spawn(async move {
            rt1.run_serialized("s-running", async {
                tokio::time::sleep(Duration::from_millis(200)).await;
            })
            .await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(rt.holds_lock_entry("s-running"));
        running.abort();
    }

    /// spawn 形の入口は呼び出し側（受信ループ）をブロックしない。
    ///
    /// ロック取得を呼び出し側で await すると、走行中セッションの推論が終わるまで
    /// 受信そのものが止まる。ここでは「完了までブロックする future を渡しても
    /// 呼び出しが即座に返る」ことと、完了後にエントリが回収されることを見る。
    #[tokio::test]
    async fn spawn_serialized_does_not_block_caller() {
        let rt = Arc::new(SessionRuntime::new());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = rt.spawn_serialized("s-spawn", async move {
            let _ = rx.await;
        });
        // 呼び出し側はブロックされていない（この行に到達できることが検証）。
        tx.send(()).expect("task must be waiting");
        handle.await.expect("spawned task panicked");
        assert!(
            !rt.holds_lock_entry("s-spawn"),
            "完了後はロックエントリが回収される"
        );
    }

    /// spawn 形でも同一セッションは直列（割り込みによる二重返信の防止）。
    #[tokio::test]
    async fn spawn_serialized_same_session_serializes() {
        let rt = Arc::new(SessionRuntime::new());
        let inflight = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let inflight = inflight.clone();
            let max_concurrent = max_concurrent.clone();
            handles.push(rt.spawn_serialized("s-spawn-serial", async move {
                let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                max_concurrent.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                inflight.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.expect("spawned task panicked");
        }
        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "同一セッションの spawn は並行してはならない"
        );
    }

    /// spawn 形でも別セッションは長時間セッションに塞がれない。
    #[tokio::test]
    async fn spawn_serialized_different_sessions_run_concurrently() {
        let rt = Arc::new(SessionRuntime::new());
        let (hold_tx, hold_rx) = tokio::sync::oneshot::channel::<()>();
        let blocking = rt.spawn_serialized("s-spawn-block", async move {
            let _ = hold_rx.await;
        });
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        rt.spawn_serialized("s-spawn-free", async move {
            let _ = done_tx.send(());
        });
        tokio::time::timeout(Duration::from_secs(5), done_rx)
            .await
            .expect("別セッションは長時間セッションに塞がれてはならない")
            .expect("sender dropped");
        let _ = hold_tx.send(());
        blocking.await.expect("spawned task panicked");
    }
}
