//! サブタスク lifecycle 通知の抽象境界（#175 S3）。
//!
//! サブタスクの開始 / 進捗 / 終了 / 中断、および走行中のツールイベントは、これまで
//! Discord の webhook 実装を `execute_spawn_subtask` などから直接呼ぶ形で埋め込まれて
//! いた。サブタスク生成ツールを gateway 非依存層へ移す（S4）には、この直接依存が
//! 邪魔になる。
//!
//! ここで切るのは**境界だけ**で、整形・配送・リトライ・宛先の解決順序はすべて
//! 実装側（Discord なら `crates/discord` の webhook 実装）に残る。`SubtaskCompletionSink`
//! （完了を親会話へ再注入する口）とは役割が別で、あちらは「決着をエンジンへ戻す」、
//! こちらは「外部へ実況を出す」。両者は独立に配線できる。
//!
//! # 使い方
//!
//! 1. 走行の開始時に [`SubtaskLifecycleNotifier::begin_run`] を 1 回だけ呼ぶ。実装は
//!    ここで宛先を解決し、配送ワーカーを起動する。解決に失敗したら
//!    [`NotifyTargetError`] を返し、呼び出し側は subtask を起動しない（fail-closed）。
//! 2. 返ってきた [`SubtaskRunNotifier`] を registry と対で保持し、開始 / 進捗 / 終了 /
//!    中断で呼ぶ。
//! 3. 通知が不要な経路（Discord 以外のゲートウェイなど）は [`NoopLifecycleNotifier`]
//!    を渡す。`NoopCompletionSink` と同じ流儀で、依存を満たすためだけの実装。

use std::sync::Arc;

use dashmap::DashMap;

use crate::bridge::ToolEventSink;

/// 1 回分のサブタスク走行を識別する情報。
///
/// 引数は既存の呼び出し箇所が実際に必要としているものだけに絞ってある:
/// - `agent_id` / `tool_args`: 宛先の解決（エージェント既定・ツール引数での明示指定）。
/// - `subtask_id` / `sub_session_id` / `label`: 通知本文に載る識別子。
/// - `parent_session_id`: 配送を諦めたときに親セッションログへ 1 件残すため。
pub struct SubtaskRunInfo<'a> {
    pub agent_id: &'a str,
    pub subtask_id: &'a str,
    /// subtask 自身のセッション ID。
    pub sub_session_id: &'a str,
    /// spawn 元の親セッション ID（空なら親ログ無し）。
    pub parent_session_id: &'a str,
    /// 人間可読ラベル。
    pub label: &'a str,
    /// 起動ツールの引数。宛先の明示指定を含み得るため実装へそのまま渡す。
    pub tool_args: &'a serde_json::Value,
}

/// 解決された通知先の可視性メタ（ツール結果 JSON と親セッションログに載る診断情報）。
///
/// 秘匿値は載せない。`redacted_url` は宛先を**伏字化した**表現で、生の URL や
/// トークンは境界を越えない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyTarget {
    /// 宛先の由来（実装が定義する識別子。未解決なら `None`）。
    pub source: Option<&'static str>,
    /// `ok` / `disabled` / `none` のいずれか。解決エラーは `Err` で返るためここには来ない。
    pub status: &'static str,
    /// 伏字化した宛先表現（無効・未設定なら `None`）。
    pub redacted_url: Option<String>,
}

impl NotifyTarget {
    /// 宛先が無い（通知しない）ことを表す。
    pub fn none() -> Self {
        Self {
            source: None,
            status: "none",
            redacted_url: None,
        }
    }
}

impl Default for NotifyTarget {
    fn default() -> Self {
        Self::none()
    }
}

/// 通知先の解決に失敗した（= subtask を起動してはいけない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyTargetError {
    /// 機械可読な失敗コード。
    pub code: String,
    /// 人間可読な説明。秘匿値を含めてはならない。
    pub message: String,
    /// どの由来の設定で失敗したか。
    pub source: &'static str,
}

/// [`SubtaskLifecycleNotifier::begin_run`] の戻り値。
pub struct SubtaskNotifySession {
    /// この走行の通知口。registry と対で保持する。
    pub notifier: Arc<dyn SubtaskRunNotifier>,
    /// 解決された宛先の可視性メタ。
    pub target: NotifyTarget,
}

/// サブタスク走行ごとの通知口を作る抽象（宛先解決 + 配送の準備）。
pub trait SubtaskLifecycleNotifier: Send + Sync {
    /// 走行を開始し、通知口と宛先メタを返す。
    ///
    /// 実装はここで宛先を解決し、必要なら配送ワーカーを起動する。設定が壊れている
    /// 場合は `Err` を返し、呼び出し側は subtask を起動しない。
    fn begin_run(
        &self,
        run: &SubtaskRunInfo<'_>,
    ) -> Result<SubtaskNotifySession, NotifyTargetError>;
}

/// サブタスク 1 走行ぶんの通知口。
///
/// すべて既定実装が「何もしない」なので、必要なイベントだけ実装すればよい。
/// 送るかどうかのフィルタ（イベント種別の購読設定など）は実装側の責務で、
/// 呼び出し側は起きた事実をそのまま伝える。
pub trait SubtaskRunNotifier: Send + Sync {
    /// 走行を開始した。`task` は依頼本文。
    fn on_started(&self, _task: &str) {}

    /// 進捗があった。`detail` は本文（ツール呼び出しの要約 or `report_progress` の本文）。
    fn on_progress(&self, _detail: &str) {}

    /// 走行が終わった。`exit_reason` は runtime の内部値（`completed` /
    /// `stopped_by_limit` / `error` / `timeout`）で、表示状態への写像は実装側が行う。
    fn on_finished(&self, _exit_reason: &str, _duration_ms: u64, _result_text: &str) {}

    /// 走行を外部から中断した（終了通知は来ないので、ここが唯一の終端）。
    fn on_cancelled(&self, _duration_ms: u64) {}

    /// 進捗通知を購読しているか。
    ///
    /// 純粋な最適化のためのヒント。`false` ならエンジンに進捗フックを挿さず、要約の
    /// 計算自体を省ける（購読していない宛先のために毎ツール呼び出しで文字列を組むのは
    /// 無駄）。`on_progress` 側にもフィルタはあるため、これを無視して常に呼んでも
    /// 送出内容は変わらない。
    fn wants_progress(&self) -> bool {
        false
    }

    /// 走行中のツールイベントを受ける sink（不要なら `None`）。
    ///
    /// executor に挿して、ツールの開始 / 完了 / 失敗 / 拒否を実況させるために使う。
    fn tool_event_sink(&self) -> Option<Arc<dyn ToolEventSink>> {
        None
    }
}

/// subtask_id → 走行中の通知口。registry と対で共有する（spawn で insert、
/// 決着 / 中断で remove）。
pub type SubtaskNotifiers = Arc<DashMap<String, Arc<dyn SubtaskRunNotifier>>>;

/// 何もしない [`SubtaskLifecycleNotifier`]（`NoopCompletionSink` と同じ流儀）。
///
/// 通知先を持たないゲートウェイ（および通知を配線しないテスト）が、通知の依存で
/// 詰まらずにサブタスクを起動できるようにするための実装。
pub struct NoopLifecycleNotifier;

impl SubtaskLifecycleNotifier for NoopLifecycleNotifier {
    fn begin_run(
        &self,
        run: &SubtaskRunInfo<'_>,
    ) -> Result<SubtaskNotifySession, NotifyTargetError> {
        tracing::debug!(
            agent_id = %run.agent_id,
            subtask_id = %run.subtask_id,
            session_id = %run.sub_session_id,
            "noop lifecycle notifier: subtask run begins (no notifications)"
        );
        Ok(SubtaskNotifySession {
            notifier: Arc::new(NoopRunNotifier),
            target: NotifyTarget::none(),
        })
    }
}

/// [`NoopLifecycleNotifier`] が返す通知口。全イベントを捨てる。
pub struct NoopRunNotifier;

impl SubtaskRunNotifier for NoopRunNotifier {}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_info<'a>(args: &'a serde_json::Value) -> SubtaskRunInfo<'a> {
        SubtaskRunInfo {
            agent_id: "agent-x",
            subtask_id: "st-1",
            sub_session_id: "subtask-st-1",
            parent_session_id: "discord-agent-x-111-222",
            label: "job",
            tool_args: args,
        }
    }

    /// Noop は必ず成功し、「宛先なし」のメタを返す（= 起動側は fail-closed に
    /// 引っかからない。S4 で gateway 非依存層から subtask を起動する前提）。
    #[test]
    fn noop_begin_run_succeeds_with_none_target() {
        let args = serde_json::json!({"task": "do it"});
        let session = NoopLifecycleNotifier
            .begin_run(&run_info(&args))
            .expect("noop は解決に失敗しない");
        assert_eq!(session.target, NotifyTarget::none());
        assert_eq!(session.target.status, "none");
        assert!(session.target.source.is_none());
        assert!(session.target.redacted_url.is_none());
    }

    /// Noop の通知口は全イベントで何もしない（呼んでも panic せず、進捗も購読しない）。
    #[test]
    fn noop_run_notifier_drops_every_event() {
        let args = serde_json::json!({});
        let session = NoopLifecycleNotifier.begin_run(&run_info(&args)).unwrap();
        let n = session.notifier;
        n.on_started("task body");
        n.on_progress("detail");
        n.on_finished("completed", 12, "result");
        n.on_cancelled(34);
        assert!(!n.wants_progress(), "購読しないので進捗フックは挿さない");
        assert!(n.tool_event_sink().is_none());
    }

    /// 既定の `NotifyTarget` は「宛先なし」。
    #[test]
    fn default_target_is_none() {
        assert_eq!(NotifyTarget::default(), NotifyTarget::none());
    }
}
