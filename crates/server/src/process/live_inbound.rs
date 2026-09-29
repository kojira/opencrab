/// 1 回の poll で注入する steer の上限。溢れた分は次のイテレーションで拾う。
const LIVE_INBOUND_POLL_LIMIT: usize = 20;

/// 走行中サブタスクへ届いた steer（追加指示）を反復の合間に注入する実体（#647）。
///
/// サブタスクは `run_agent_response` を depth+1 で再入し、親ターンと同じ engine ループ
/// （毎イテレーション `LiveInboundSource::poll_new_messages` を引く / #289）を通る。steer は
/// この既存機構をそのままサブへ通したもので、steer 専用の注入口である。sub-session（`subtask-{id}`）に `steer_subtask` が積んだ
/// `log_type='steer'` の行だけを watermark 差分で読み、次の LLM 呼び出しへ user メッセージ
/// として足す。
///
/// - 対象 `log_type` は `steer`（`STEER_LOG_TYPE`）。発話者フィルタは無い
///   （steer は親/オーナーの明示指示であり、送り主は認可済み）。
/// - depth>0（サブタスク）だけで配線する。親ターン（depth==0）には走行中の注入口は無い。
///
/// 重複注入防止は `watermark`（取得済み最大 log id）。初期値は engine 起動時点の最新 id
/// なので、以後に届いた steer だけが注入される。
pub(super) struct SubtaskSteerInbound {
    db: opencrab_db::Db,
    /// サブタスク自身のセッション ID（`subtask-{id}`）。steer はここへ積まれる。
    sub_session_id: String,
    /// 取得済みの最大 log id。これより後の steer 行だけを次回返す。
    watermark: std::sync::atomic::AtomicI64,
}

impl SubtaskSteerInbound {
    /// watermark 初期値を **0（セッション先頭）** にして組み立てる。
    ///
    /// steer の宛先は **spawn したばかりの新規 sub-session**で、
    /// engine が動き出す前に steer が積まれることは無い（過去の steer が存在しない）。
    /// 「最新 id」で初期化すると、spawn 直後〜この `new()` までの窓に届いた
    /// steer を取りこぼす（`steer_subtask` は Accepted を返したのに読まれない）。リプレイの
    /// 心配が無い場所なので 0 から読む方が正しく、「Accepted なのに読まれない」を settle
    /// race（doc 明記済みの許容窓）だけに絞れる。
    pub(super) fn new(db: opencrab_db::Db, sub_session_id: &str) -> Self {
        Self {
            db,
            sub_session_id: sub_session_id.to_string(),
            watermark: std::sync::atomic::AtomicI64::new(0),
        }
    }
}

impl opencrab_core::LiveInboundSource for SubtaskSteerInbound {
    fn poll_new_messages(&self) -> Vec<String> {
        use std::sync::atomic::Ordering;

        let after_id = self.watermark.load(Ordering::Relaxed);
        let conn = match self.db.lock() {
            Ok(conn) => conn,
            // ロックが取れないだけで反復を落とさない（次のイテレーションで拾える）。
            Err(_) => return Vec::new(),
        };
        let rows = match opencrab_db::queries::list_steer_logs_after(
            &conn,
            &self.sub_session_id,
            after_id,
            LIVE_INBOUND_POLL_LIMIT,
        ) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(session_id = %self.sub_session_id, "steer inbound poll failed: {e}");
                return Vec::new();
            }
        };
        drop(conn);

        if rows.is_empty() {
            return Vec::new();
        }
        // 返した行まで watermark を進める（＝同じ steer を二度注入しない）。
        if let Some(max_id) = rows.iter().filter_map(|r| r.id).max() {
            self.watermark.store(max_id, Ordering::Relaxed);
        }
        rows.iter()
            .map(|r| format_steer_inbound(&r.content))
            .collect()
    }
}

/// 走行中サブへ届いた steer を LLM へ見せる形に整える（#647）。
///
/// 親/オーナーからの**明示の追加指示**であることを明記し、受領/反映を親へ返すよう促す。
/// ただし tool 呼び出しを system レベルで強制はしない（「足すだけ・応答は判断に委ねる」
/// 方針 / #288。steer は指示の性質が強いので促し文言を添える）。
fn format_steer_inbound(message: &str) -> String {
    format!(
        "[追加指示 (steer): 親/オーナーからの指示が、あなたがこのタスクを実行している間に届きました]\n\
         {message}\n\
         （この指示を踏まえて以後の方針を調整し、受領した旨と反映内容を report_progress で親へ返してください。）"
    )
}

/// 変動コンテキストを最後のuserメッセージに前置するヘルパー（実体は
/// [`opencrab_core::runtime_context`] / #190 S2）。
///
/// 純関数なので下位層へ移した。transport 側のクレートが
/// `crates/server` を参照せずに使えるようにするため。既存の呼び出し元
/// （`process::prepend_runtime_context(..)`）を変えずに済むよう再エクスポートを残す。
pub use opencrab_core::runtime_context::prepend_runtime_context;

/// 外部メッセージ識別子を含む変動コンテキストを前置するヘルパー。
pub fn prepend_runtime_context_with_message_id(
    user_message: &str,
    session_theme: &str,
    message_id: &str,
) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z");
    let tz_name = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string());
    let now = format!("{now} ({tz_name})");
    format!(
        "[Context]\nCurrent date and time: {now}\nCurrent discussion topic: {session_theme}\nExternal message_id: {message_id}\n\n{user_message}"
    )
}
