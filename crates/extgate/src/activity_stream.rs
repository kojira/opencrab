//! D-1087: 走行の活動（ツール実行とサブタスク走行のライフサイクル）を gateway へ流す。
//!
//! core が持つ「起きたこと」を、宛先も整形も持たない中立な形で 1 本の stream に書く。
//! 誰が読むか・どこへ送るかは gateway の設定で決まり、core は関知しない。読む者が居ない
//! （フレームを知らない gateway / writer 未接続）ときは書くだけで、走行の挙動も記録も
//! 一切変わらない。
//!
//! # 順序
//!
//! エンジンの `ToolEventSink` は同期 trait なので、書き込みを直接 await できない。そこで
//! 1 ターン 1 本の順序付きキューに入れ、1 本の consumer task が到着順に書く。これにより
//! ツール開始→終端、サブタスク開始→進捗→終端の**発生順**が stream 上でも保たれる
//! （複数 task から並行に `write_json` すると Mutex の取得順になり、順序が崩れる）。
//!
//! # フレーム
//!
//! [`tool_activity_frame`] / [`subtask_lifecycle_frame`]。どちらも id を持たないので、
//! 未知フレームを無視する準拠 gateway は write 0・keep で素通しする。

use std::sync::{Arc, Mutex};

use opencrab_actions::{SubtaskLifecycleEvent, ToolEvent, ToolEventSink, ToolEventStatus};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::protocol::{subtask_lifecycle_frame, tool_activity_frame, write_json};
use crate::registry::ExtgateState;

/// キューに載るもの。`Barrier` はそれまでの frame を書き終えたら知らせる（順序の同期）。
enum Queued {
    Frame(Value),
    Barrier(tokio::sync::oneshot::Sender<()>),
}

/// 1 ターン分の活動 stream。clone しても同じキューを指す。
#[derive(Clone)]
pub struct ActivityEmitter {
    tx: mpsc::UnboundedSender<Queued>,
    /// いま流れているターンの activity id（`activity` フレームと同じ値）。
    activity_id: Arc<Mutex<String>>,
    binding_id: Arc<str>,
}

impl ActivityEmitter {
    /// キューと writer task を起動する。`activity_id` はこのターンの `activity` フレームと
    /// 同じ値（同じターンの活動を gateway がまとめられるようにする）。
    pub fn spawn(
        state: Arc<ExtgateState>,
        instance_id: String,
        binding_id: String,
        activity_id: String,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(write_frames(state, instance_id, rx));
        Arc::new(Self {
            tx,
            activity_id: Arc::new(Mutex::new(activity_id)),
            binding_id: Arc::from(binding_id.as_str()),
        })
    }

    /// 以後のイベントを載せるターンの activity id を差し替える。
    ///
    /// 1 つの sink を複数のターンが使う（決着後の resume ターン）ため。ターンは
    /// セッション直列で回るので、同時に 2 つの値が要ることはない。
    pub fn set_activity_id(&self, activity_id: &str) {
        if let Ok(mut current) = self.activity_id.lock() {
            *current = activity_id.to_string();
        }
    }

    fn activity_id(&self) -> String {
        self.activity_id
            .lock()
            .map(|current| current.clone())
            .unwrap_or_default()
    }

    /// サブタスク走行のライフサイクル 1 件を流す。
    pub fn lifecycle(&self, ev: &SubtaskLifecycleEvent<'_>) {
        let frame = subtask_lifecycle_frame(
            &self.binding_id,
            ev.subtask_id,
            ev.sub_session_id,
            ev.status.as_str(),
            ev.label,
            ev.task,
            ev.detail,
            ev.duration_ms,
            ev.result_text,
        );
        self.push(frame);
    }

    /// それまでに積んだ frame を書き終えるまで待つ。
    ///
    /// ターンの終端（`activity` の `ended`）は別経路で書かれるので、待たないと
    /// 「ended が先、最後のツール活動が後」という順序になりうる。書けなかった場合も
    /// 待ち続けない（観測は best-effort で、走行を止めない）。
    pub async fn flush(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Queued::Barrier(tx)).is_err() {
            return;
        }
        let _ = rx.await;
    }

    /// キューへ積む。受信側が居ない（writer task が終了している）ときは捨てるだけで、
    /// 走行には影響しない。
    fn push(&self, frame: Value) {
        let _ = self.tx.send(Queued::Frame(frame));
    }
}

impl ToolEventSink for ActivityEmitter {
    fn on_event(&self, event: &ToolEvent<'_>) {
        let terminal = !matches!(event.status, ToolEventStatus::Started);
        let frame = tool_activity_frame(
            &self.binding_id,
            &self.activity_id(),
            event.tool_call_id,
            // 入れ子の走行だけが run の相関キーを運ぶ（親ターンの session は run のキーではない）。
            event.session_id.filter(|_| event.depth > 0),
            event.tool_name,
            status_str(event.status),
            event.depth,
            event.started_at,
            event.duration_ms.filter(|_| terminal),
            event.args,
            event.result.filter(|_| terminal),
            event.error.filter(|_| terminal),
        );
        self.push(frame);
    }
}

fn status_str(status: ToolEventStatus) -> &'static str {
    match status {
        ToolEventStatus::Started => "started",
        ToolEventStatus::Completed => "completed",
        ToolEventStatus::Failed => "failed",
        ToolEventStatus::Rejected => "rejected",
    }
}

/// キューを到着順に書き出す consumer。送信側が全部 drop されたら終わる。
async fn write_frames(
    state: Arc<ExtgateState>,
    instance_id: String,
    mut rx: mpsc::UnboundedReceiver<Queued>,
) {
    while let Some(queued) = rx.recv().await {
        let frame = match queued {
            Queued::Barrier(done) => {
                let _ = done.send(());
                continue;
            }
            Queued::Frame(frame) => frame,
        };
        let writer = {
            let Ok(reg) = state.lock_registry() else {
                continue;
            };
            let Some(live) = reg.get(&instance_id) else {
                continue;
            };
            let binding_id = frame["binding_id"].as_str().unwrap_or_default();
            if !live.acknowledged.contains(binding_id) {
                continue;
            }
            live.writer.clone()
        };
        // 失敗は走行を壊さない（観測の欠落だけ）。1 件ごとに記録する。
        if let Err(e) = write_json(&writer, &frame).await {
            tracing::debug!(
                code = e.code.as_str(),
                m = frame["m"].as_str().unwrap_or_default(),
                "activity frame write failed; ignored"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::time::Duration;

    use crate::operations::FinalDelivery;
    use crate::registry::LiveEntry;
    use tokio::io::AsyncReadExt;

    const BINDING: &str = "binding-1";

    /// A state whose live registry holds one binding, plus the read half that sees what the
    /// emitter writes. `_live` keeps the peer open so writes do not fail with EOF.
    async fn state_with_writer(
        acknowledged: bool,
    ) -> (
        Arc<ExtgateState>,
        Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
        tokio::net::unix::OwnedReadHalf,
        tokio::net::unix::OwnedWriteHalf,
    ) {
        let (lane, observer) = tokio::net::UnixStream::pair().unwrap();
        let (_lane_read, lane_write) = lane.into_split();
        let (observer_read, observer_write) = observer.into_split();
        let writer = Arc::new(tokio::sync::Mutex::new(lane_write));
        let state = Arc::new(ExtgateState::new(
            opencrab_db::Db::memory().unwrap(),
        ));
        state.lock_registry().unwrap().insert(
            "instance-1".to_string(),
            LiveEntry {
                identity: 1,
                revision: 1,
                writer: Arc::clone(&writer),
                acknowledged: if acknowledged {
                    HashSet::from([BINDING.to_string()])
                } else {
                    HashSet::new()
                },
                pending: Default::default(),
                declarations: Arc::new(Vec::new()),
                declaration_digest: String::new(),
                final_delivery: FinalDelivery::Automatic,
                run_env: Default::default(),
            },
        );
        (state, writer, observer_read, observer_write)
    }

    fn lifecycle_frame(run_id: &str) -> Value {
        subtask_lifecycle_frame(
            BINDING,
            run_id,
            "subtask-s1",
            "started",
            "label",
            "task",
            None,
            None,
            None,
        )
    }

    async fn read_line(reader: &mut tokio::net::unix::OwnedReadHalf) -> Option<String> {
        let mut buf = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            match reader.read_exact(&mut byte).await {
                Ok(_) => {}
                Err(_) => return None,
            }
            buf.push(byte[0]);
            if byte[0] == b'\n' {
                return String::from_utf8(buf).ok();
            }
        }
    }

    /// A frame written by the queue arrives as one LF-terminated JSON line, after `flush`.
    #[tokio::test]
    async fn flush_completes_after_the_frames_ahead_are_written() {
        let (state, writer, mut observer, _live) = state_with_writer(true).await;
        let emitter = ActivityEmitter::spawn(
            Arc::clone(&state),
            "instance-1".into(),
            BINDING.into(),
            "activity-1".into(),
        );

        // Hold the wire so the consumer is stuck on the frame ahead of the barrier …
        let held = writer.lock().await;
        emitter.push(lifecycle_frame("run-1"));
        let early = tokio::time::timeout(Duration::from_millis(50), emitter.flush()).await;
        assert!(early.is_err(), "flush must wait for the frame it follows");
        drop(held);

        // … and the barrier completes only once that frame is on the wire.
        tokio::time::timeout(Duration::from_secs(5), emitter.flush())
            .await
            .expect("flush after the wire is free");
        let line = tokio::time::timeout(Duration::from_secs(5), read_line(&mut observer))
            .await
            .expect("frame on the wire")
            .expect("json line");
        let frame: Value = serde_json::from_str(line.trim_end()).expect("json line");
        assert_eq!(frame["m"], "subtask_lifecycle");
        assert_eq!(frame["run_id"], "run-1");
    }

    /// D-1087: 入れ子の走行（depth > 0）で起きたツール実行の frame は、ライフサイクル frame と
    /// 同じ `sub_session_id`（run の相関キー）を運ぶ。親ターン（depth 0）では field ごと省く。
    #[tokio::test]
    async fn tool_activity_carries_the_run_session_key_only_for_nested_runs() {
        let (state, _writer, mut observer, _live) = state_with_writer(true).await;
        let emitter = ActivityEmitter::spawn(
            Arc::clone(&state),
            "instance-1".into(),
            BINDING.into(),
            "activity-1".into(),
        );
        for (tool, depth, session) in [
            ("nested_tool", 1u32, "subtask-s1"),
            ("parent_tool", 0, "sess-1"),
        ] {
            emitter.on_event(&ToolEvent {
                tool_name: tool,
                tool_call_id: "call-1",
                agent_id: "agent-1",
                session_id: Some(session),
                depth,
                status: ToolEventStatus::Started,
                started_at: "2026-10-20T00:00:00Z",
                duration_ms: None,
                args: &serde_json::json!({}),
                result: None,
                error: None,
            });
        }
        tokio::time::timeout(Duration::from_secs(5), emitter.flush())
            .await
            .expect("flush");

        let first = read_line(&mut observer).await.expect("nested frame");
        let first: Value = serde_json::from_str(first.trim_end()).expect("json line");
        assert_eq!(first["tool"], "nested_tool");
        assert_eq!(first["depth"], 1);
        assert_eq!(
            first["sub_session_id"], "subtask-s1",
            "nested run key: {first}"
        );

        let second = read_line(&mut observer).await.expect("parent frame");
        let second: Value = serde_json::from_str(second.trim_end()).expect("json line");
        assert_eq!(second["tool"], "parent_tool");
        assert_eq!(second["depth"], 0);
        assert!(
            second.get("sub_session_id").is_none(),
            "親ターンは run key を運ばない: {second}"
        );
    }

    /// No acknowledged binding: frames are dropped and nothing blocks or panics.
    #[tokio::test]
    async fn frames_without_an_acknowledged_binding_are_inert() {
        let (state, _writer, mut observer, _live) = state_with_writer(false).await;
        let emitter = ActivityEmitter::spawn(
            Arc::clone(&state),
            "instance-1".into(),
            BINDING.into(),
            "activity-1".into(),
        );
        emitter.push(lifecycle_frame("run-1"));
        tokio::time::timeout(Duration::from_secs(5), emitter.flush())
            .await
            .expect("flush with an unreachable gateway");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), read_line(&mut observer))
                .await
                .is_err(),
            "nothing is written for an unacknowledged binding"
        );
    }

    /// No live instance at all: the same, for a gateway that never connected.
    #[tokio::test]
    async fn frames_without_a_live_instance_are_inert() {
        let state = Arc::new(ExtgateState::new(
            opencrab_db::Db::memory().unwrap(),
        ));
        let emitter = ActivityEmitter::spawn(
            Arc::clone(&state),
            "missing-instance".into(),
            BINDING.into(),
            "activity-1".into(),
        );
        emitter.push(lifecycle_frame("run-1"));
        tokio::time::timeout(Duration::from_secs(5), emitter.flush())
            .await
            .expect("flush without a listener");
    }
}
