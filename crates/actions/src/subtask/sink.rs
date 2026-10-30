use std::sync::Arc;

use super::{CallerIdentity, SettleKind};
use crate::bridge::ToolEventSink;

/// subtask の settle を親セッションへ通知するための最小ペイロード。
///
/// 本文（result）は運搬しない（RFC §1.3）。ここには resume 判断と返信配送に要る
/// 最小情報だけを持つ。
#[derive(Debug, Clone)]
pub struct SubtaskSettled {
    /// 親セッション ID（resume 対象）。
    pub session_id: String,
    /// 親セッションのエージェント ID。
    pub agent_id: String,
    /// settle した subtask の ID。
    pub subtask_id: String,
    /// 決着理由（completed / error / timeout / stopped_by_limit など）。
    /// 種別は `kind` が持つ（progress の二重定義回避）。
    pub exit_reason: String,
    /// 決着の種別（完了 or 進捗）。
    pub kind: SettleKind,
    /// gateway 不透明な返信ルーティング token（#167 / RFC §3.1(4)）。
    ///
    /// ランタイム（`settle_completed`）が registry の `SpawnedSubtask.reply_target`
    /// を引いて載せる。session_id から返信先を復元できない gateway（Nostr の
    /// event id など）のための経路であり、Discord のように session_id から導出
    /// できる sink は `None` のままでよい（無視する）。`None` は「返信配送先の
    /// 指定なし」を意味し、sink 側の既存挙動を変えない。
    pub reply_target: Option<String>,
    /// **この subtask を生んだ親 run の呼び出し元**（#298）。
    ///
    /// resume する sink（Discord / web）はこの値で `RunRequest` を組む。以前は
    /// resume 側が `CallerIdentity::Agent` をハードコードしていたため、オーナー発の
    /// ターンでも subtask が決着した瞬間に権限が降格し、owner/trusted のツールが
    /// list_tools からも dispatch からも丸ごと消えていた（`report_progress` を
    /// 呼ぶと自分の権限が落ちる、という自爆的な挙動）。
    ///
    /// ランタイム（`settle_completed` / `cancel_subtask`）が registry のエントリ
    /// （`SpawnedSubtask.caller`）から読んで載せる。**引き継ぐだけ**で、権限を昇格
    /// させる経路ではない（元が `Agent` のターンは `Agent` のまま）。registry から
    /// 引けなかった場合は最小権限（`Agent`）へ倒す（fail-closed）。
    pub caller: CallerIdentity,
}

/// 走行 1 件の中立なライフサイクル状態（D-1087）。
///
/// 表示語彙（絵文字・見出し・配送先）は実装側が持つ。ここは「起きたこと」だけを表す。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubtaskLifecycleStatus {
    Started,
    Progress,
    Completed,
    Failed,
    TimedOut,
    Aborted,
}

impl SubtaskLifecycleStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SubtaskLifecycleStatus::Started => "started",
            SubtaskLifecycleStatus::Progress => "progress",
            SubtaskLifecycleStatus::Completed => "completed",
            SubtaskLifecycleStatus::Failed => "failed",
            SubtaskLifecycleStatus::TimedOut => "timed_out",
            SubtaskLifecycleStatus::Aborted => "aborted",
        }
    }

    /// runtime の内部 `exit_reason` を状態へ写像する。表示への写像は実装側の責務なので、
    /// ここでは「正常終了か、失敗か、時間切れか」だけを決める。
    pub fn from_exit_reason(exit_reason: &str) -> Self {
        match exit_reason {
            "timeout" => SubtaskLifecycleStatus::TimedOut,
            "error" => SubtaskLifecycleStatus::Failed,
            // "completed" / "stopped_by_limit" など
            _ => SubtaskLifecycleStatus::Completed,
        }
    }
}

/// 走行 1 件のライフサイクル 1 件（D-1087）。
///
/// 宛先・整形・配送は実装（gateway）の責務。core は値をそのまま渡すだけ。
/// 各項目は「その時点で分かっているもの」だけを埋める（例: progress は `detail`）。
pub struct SubtaskLifecycleEvent<'a> {
    /// subtask 自身の ID。
    pub subtask_id: &'a str,
    /// subtask 自身のセッション ID（走行の相関キー。親とは別の値）。
    pub sub_session_id: &'a str,
    /// spawn 元の親セッション ID。
    pub parent_session_id: &'a str,
    /// 親セッションのエージェント ID。
    pub agent_id: &'a str,
    pub status: SubtaskLifecycleStatus,
    /// 人間可読ラベル（registry の `label`）。不明なら空。
    pub label: &'a str,
    /// 依頼本文。started 以外では空（実装が started で保持する）。
    pub task: &'a str,
    /// progress の本文。
    pub detail: Option<&'a str>,
    /// 終端（completed / failed / timed_out / aborted）だけ。
    pub duration_ms: Option<u64>,
    /// 終端の結果本文。
    pub result_text: Option<&'a str>,
}

/// subtask 完了通知の抽象（`LoopEvent` 直依存を置換する）。
///
/// ランタイムは `Arc<dyn SubtaskCompletionSink>` を保持し、**DB 永続化の後に**
/// `on_subtask_settled` を呼ぶだけで、`LoopEvent` を知らない。sink 実装が
/// 「resume ＋ その gateway の配送口」を担う（gateway=セッション宛ての発話 / REST=保存して取得 /
/// heartbeat=次 tick 拾い or 保存）。
pub trait SubtaskCompletionSink: Send + Sync {
    /// この transport が持つ**親セッション ID の接頭辞**（例 `discord-` / `web-` / `agent-msg-`）。
    ///
    /// 継続を起こすかの判断（[`dispatch_settled`]）が使う。sink は spawn 時に transport ごとへ
    /// 選ばれるが、ネストした subtask（`subtask-*`）や heartbeat（`heartbeat-*`）の決着が同じ
    /// sink を通ることがあるため、**自分の親セッションかどうか**は判断側で確かめる。
    ///
    /// **既定実装を与えない**（#638）。新しい transport を足したとき、コンパイラがここで止める。
    ///
    /// **接頭辞は transport 間で互いに素であること**（現状 `discord-` / `web-` / `nostr-` /
    /// `agent-msg-` / `heartbeat-` / `subtask-` は重ならない）。重なると、判断
    /// （[`dispatch_settled`]）が親セッションを取り違えて継続を誤配送する——コンパイラは
    /// ここを強制できないので、transport を足すときに既存の接頭辞と衝突しないことを確かめる。
    fn session_prefix(&self) -> &'static str;

    /// この sink が握る**親セッションそのものか**（[`dispatch_settled`] が継続配送の可否に使う）。
    ///
    /// 既定は [`Self::session_prefix`] の接頭辞一致——大半の transport は「自分の接頭辞で始まる
    /// session は自分のもの」で正しい（Discord / web / REST / heartbeat）。
    ///
    /// **接頭辞では自分の session を選り分けられない transport だけ** override する。extgate の
    /// セッションは gateway が決めるキーそのもので接頭辞を持たないため、接頭辞判定では
    /// **全決着を門前払い**していた（resume 不発 / #838 の row284）。この sink は spawn 時に
    /// 握った実 session を保持しており、それと等値比較すれば取り違えなく判定できる。
    fn owns_parent_session(&self, session_id: &str) -> bool {
        session_id.starts_with(self.session_prefix())
    }

    /// **進捗（[`SettleKind::Progress`]）も継続として配送するか**（Discord だけ `true`）。
    ///
    /// `report_progress` のデバウンス発火が「進捗実況」としてメインエンジンを呼び直す Discord
    /// 固有の機能。web / Nostr / REST は完了だけを配送する（進捗で resume すると、まだ走って
    /// いる run の途中で二重に応答してしまう）。**transport ごとに違うのはここだけ**なので、
    /// 判断ではなく**性質**として名乗らせる（#638）。
    ///
    /// **既定実装を与えない**（#638）。
    fn forwards_progress(&self) -> bool;

    /// 継続ターンを**配送する**（transport の「違うところ」）。
    ///
    /// ここへ来た時点で「継続を起こす」判断は [`dispatch_settled`] が済ませている——kind の
    /// 検査も、親セッションが自分のものかの検査も、**実装側でやり直さない**。実装がするのは
    /// 「自分の口で継続ターンを回して結果を届ける」ことだけ（Discord=イベントループへ /
    /// web=SSE / Nostr=セッションへ転記 / REST=セッションログへ永続化）。
    ///
    /// **既定実装を与えない**（#638）。継続を実装し忘れた transport はコンパイルが通らない。
    fn deliver_continuation(&self, ev: SubtaskSettled);

    /// `cancel_subtask` で subtask が停止したときの通知（`kind = Cancelled`）。
    ///
    /// **完了経路とは別メソッド**にしてある。停止は「resume して返信する」イベント
    /// ではなく、`on_subtask_settled` に流すと resume する sink（Discord / web /
    /// Nostr）が「止めたのに返信する」ことになる。既定実装は debug ログのみで、
    /// 停止で状態整合が必要な sink（REST の `sessions.status`）だけが override する。
    ///
    /// これにより「停止の到達性」が `cancel_subtask` の 1 箇所に閉じる（各経路が
    /// cancel 後に個別に後始末する必要がない）。
    fn on_subtask_cancelled(&self, ev: SubtaskSettled) {
        tracing::debug!(
            session_id = %ev.session_id,
            agent_id = %ev.agent_id,
            subtask_id = %ev.subtask_id,
            exit_reason = %ev.exit_reason,
            "subtask cancelled (sink has no cancel-time reconciliation)"
        );
    }

    /// 走行のライフサイクル（開始 / 進捗 / 終了 / 中断）を 1 件ずつ通知する（D-1087）。
    ///
    /// [`Self::deliver_continuation`] とは役割が違う。あちらは**決着を親会話へ戻す**
    /// （resume）core の責務で、配送可否の判断を伴う。こちらは**起きた事実**を実装へ
    /// 渡すだけで、resume も配送も起こさない。実装が購読していなければ何もしない
    /// （既定実装は空）ので、呼び出し側は常に呼んでよい。
    ///
    /// 宛先の解決・整形・配送・購読フィルタは実装側の責務。core は宛先も URL も持たない。
    fn on_subtask_lifecycle(&self, _ev: &SubtaskLifecycleEvent<'_>) {}

    /// 走行中のツールイベントを受け取る sink（D-1087）。
    ///
    /// `spawn_subtask` はこの値で副実行の `RunRequest` を組むので、実装は自分の
    /// 活動 sink を返せばよい（副実行も同じ活動の流れに載る）。既定は `None`（購読なし）。
    fn tool_event_sink(&self) -> Option<Arc<dyn ToolEventSink>> {
        None
    }
}

/// **継続ターンを起こすかどうかの判断（#638・唯一の実装）**。
///
/// 以前は transport ごとの sink が同じ判断をそれぞれ書いていた（Discord / web / Nostr の 3 本）。
/// 同型実装が 3 つあると「3 箇所とも直さないと直らない」うえ、実際に不一致が生まれていた——
/// **REST には継続そのものが無く**（`POST /api/agents/{id}/messages` で subtask を投げると、
/// 完了が届いても続きが走らない / #631 の実測）、web / Nostr は完了 1 本ごとに継続するのに
/// REST だけ「未完がゼロのとき」というゲートを持っていた。判断をここへ集約し、transport は
/// [`SubtaskCompletionSink::deliver_continuation`]（配送）と
/// [`SubtaskCompletionSink::forwards_progress`]（性質）だけを答える。
///
/// 判断は 3 つだけ:
/// 1. **kind**: `Completed` は常に継続する。`Progress` は `forwards_progress()` が `true` の
///    transport にだけ配送する（Discord の進捗実況）。それ以外（`Cancelled` 等）は配送しない
///    ——停止は [`SubtaskCompletionSink::on_subtask_cancelled`] の役目で、ここへ流すと
///    「止めたのに返信する」ことになる。
/// 2. **親セッションが自分のものか**: `owns_parent_session()` で確かめる（既定は
///    `session_prefix()` の接頭辞一致・extgate だけ実 session と等値比較へ override）。
///    ネストした subtask や heartbeat の決着が同じ sink を通り得るため（正常系なので debug に留める）。
/// 3. 上を通ったら配送へ渡す。**「他に走行中の subtask があるか」は見ない**——複数 subtask の
///    ドリブルは実測で再現せず（3 本を順に走らせた結果、継続が全部拾って最後にまとめて答えた）、
///    未完ゼロを待つと「最後の 1 本が終わるまで何も返さない」ことになる（#638 の裁定）。
///
/// `settle_completed`（完了）と `report_progress` のデバウンス発火（進捗）は、sink のメソッドを
/// 直接呼ばず**必ずここを通る**。これで判断が 1 箇所に閉じ、transport 側にコピーが生まれない。
pub fn dispatch_settled(sink: &dyn SubtaskCompletionSink, ev: SubtaskSettled) {
    match ev.kind {
        SettleKind::Completed => {}
        SettleKind::Progress => {
            if !sink.forwards_progress() {
                tracing::debug!(
                    session_id = %ev.session_id,
                    "subtask progress: transport does not forward progress, skipping continuation"
                );
                return;
            }
        }
        other => {
            tracing::debug!(
                session_id = %ev.session_id,
                kind = ?other,
                "subtask settled: not a continuation trigger, skipping"
            );
            return;
        }
    }
    if !sink.owns_parent_session(&ev.session_id) {
        // ネストした subtask（`subtask-*`）や heartbeat（`heartbeat-*`）の決着、または
        // この sink が握っていない別セッションの決着。正常系。
        tracing::debug!(
            session_id = %ev.session_id,
            prefix = sink.session_prefix(),
            "subtask settled: parent session belongs to another transport, skipping continuation"
        );
        return;
    }
    sink.deliver_continuation(ev);
}

/// 何もしない `SubtaskCompletionSink`（debug ログのみ / #167）。
///
/// `RunRequest::with_dispatch` は sink を必須とするため（`Some(sink)` のときだけ
/// dispatcher を注入する）、「**auto-dispatch だけ有効化して即時の再注入はしない**」
/// 経路にはプレースホルダの sink が必要になる。この sink を渡せば、dispatch した
/// ツールは background 化され、完了本文は `settle_completed` が親セッションログ
/// （DB）へ永続化するだけで終わる。
///
/// 想定用途は heartbeat のように「次 tick で `build_conversation_string` が DB から
/// 完了ログを読み直す」経路（#169）。即時 resume が不要なので、sink 実装を書かずに
/// dispatch を有効化できる。逆に「完了時に即返信したい」gateway ではこれを使わず、
/// 固有 sink を実装する（Discord=`LoopEvent` / Nostr=`reply`）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopCompletionSink;

impl SubtaskCompletionSink for NoopCompletionSink {
    /// どの親セッションにも属さない（配送しないので接頭辞は空＝全ての session に一致する）。
    /// 一致しても [`Self::deliver_continuation`] が debug ログだけで終わる。
    fn session_prefix(&self) -> &'static str {
        ""
    }
    /// 何も配送しないので進捗も転送しない。
    fn forwards_progress(&self) -> bool {
        false
    }
    fn deliver_continuation(&self, ev: SubtaskSettled) {
        tracing::debug!(
            session_id = %ev.session_id,
            agent_id = %ev.agent_id,
            subtask_id = %ev.subtask_id,
            exit_reason = %ev.exit_reason,
            kind = ?ev.kind,
            reply_target = ?ev.reply_target,
            "noop completion sink: subtask settled (no re-injection)"
        );
    }
}
