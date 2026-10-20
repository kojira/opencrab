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

/// 1 ターン分の活動 stream。clone しても同じキューを指す。
#[derive(Clone)]
pub struct ActivityEmitter {
    tx: mpsc::UnboundedSender<Value>,
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
            ev.status.as_str(),
            ev.label,
            ev.task,
            ev.detail,
            ev.duration_ms,
            ev.result_text,
        );
        self.push(frame);
    }

    /// キューへ積む。受信側が居ない（writer task が終了している）ときは捨てるだけで、
    /// 走行には影響しない。
    fn push(&self, frame: Value) {
        let _ = self.tx.send(frame);
    }
}

impl ToolEventSink for ActivityEmitter {
    fn on_event(&self, event: &ToolEvent<'_>) {
        let terminal = !matches!(event.status, ToolEventStatus::Started);
        let frame = tool_activity_frame(
            &self.binding_id,
            &self.activity_id(),
            event.tool_call_id,
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
    mut rx: mpsc::UnboundedReceiver<Value>,
) {
    while let Some(frame) = rx.recv().await {
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
