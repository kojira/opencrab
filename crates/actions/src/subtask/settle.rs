use super::sink::{dispatch_settled, SubtaskCompletionSink, SubtaskSettled};
use super::{SettleKind, SubtaskLifecycle, SubtaskRegistry};

/// `settle_completed` が subtask_completed ログの記録と sink 発火に用いる文脈。
///
/// 本文（result）は別引数で受け取る。DB へは本文込みで永続化するが、sink へ渡す
/// `SubtaskSettled` には本文を載せない（RFC §1.3）。
pub struct SettleContext {
    /// 親セッション ID（resume 対象。空なら DB 記録をスキップ）。
    pub parent_session_id: String,
    /// 実行エージェント ID。
    pub agent_id: String,
    /// settle した subtask の ID。
    pub subtask_id: String,
    /// subtask 自身のセッション ID（ログの session フィールドに載せる）。
    pub sub_session_id: String,
    /// 決着理由（completed / error / timeout / stopped_by_limit）。
    pub exit_reason: String,
    /// 停止/決着の排他ラッチ（`SpawnedSubtask.lifecycle` のクローン）。
    ///
    /// `settle_completed` は **DB 永続化の前に** `claim_settle()` を試み、失敗した
    /// （= `cancel_subtask` が先に停止を主張した）場合は DB 記録も sink 発火も行わない。
    /// registry へ登録しない一発呼び（テスト等）は `SubtaskLifecycle::new()` を渡せば
    /// 常に claim が成功し、従来と同じ挙動になる。
    pub lifecycle: SubtaskLifecycle,
}

/// subtask 完了の中核処理（gateway 非依存 / RFC §4 S1）。
///
/// この関数が **二重回答の順序契約**（RFC §6 受け入れ基準）を 1 箇所で保証する:
///   1. `subtask_completed` を親セッションログ（DB）へ永続化する（本文 `result_text`
///      を含む）。
///   2. registry から当該 subtask を除去し、**その際に取り出したエントリから**
///      `reply_target` を読み出す（#167）。除去してから引き直すことはできない
///      （エントリが消えているため）ので、`remove` の戻り値を使う。これは
///      shard ロック下の 1 操作なので「読んでから消す」間の TOCTOU も無い。
///   3. sink を発火する（本文は運ばない。手順 1 で DB 永続化済み）。`reply_target`
///      は手順 2 で得た値を載せる。
///
/// **DB 永続化（1）は必ず sink 発火（3）より前**に行う。sink 実装（例: Discord）は
/// この後に親セッションを resume し、`build_conversation_string` が DB から会話を
/// 再構築するため、完了ログが先に着地している必要がある。
///
/// gateway 固有の後始末（webhook terminal 送出・progress debounce 除去・随伴構造の
/// 掃除など）は本関数の呼び出し**前**に呼び出し側で行う。それらは DB 永続化とも
/// sink 発火とも順序依存が無い（webhook は非同期配送・別マップ）ため、載せ替えても
/// 観測可能な挙動は変わらない。
pub fn settle_completed(
    registry: &SubtaskRegistry,
    db: &opencrab_db::Db,
    sink: &dyn SubtaskCompletionSink,
    ctx: SettleContext,
    result_text: &str,
) {
    // 0. 停止と決着の排他（DB 永続化より前）。`cancel_subtask` が先に停止を主張して
    //    いたら、ここでは **DB 記録も registry 除去も sink 発火もしない**。
    //    ツール完走〜DB INSERT の窓で cancel が入ったとき、`cancelled:true` を返した
    //    のに完了ログが着地して sink が resume する（＝止めたのに返信が届く）のを防ぐ。
    if !ctx.lifecycle.claim_settle() {
        tracing::debug!(
            session_id = %ctx.parent_session_id,
            subtask_id = %ctx.subtask_id,
            exit_reason = %ctx.exit_reason,
            "subtask was cancelled before settling; skipping persistence and sink"
        );
        return;
    }

    // 1. 完了本文を DB へ永続化する（sink 発火より前 = 順序契約）。
    if !ctx.parent_session_id.is_empty() {
        if let Ok(conn) = db.lock() {
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: ctx.agent_id.clone(),
                session_id: ctx.parent_session_id.clone(),
                log_type: "system".to_string(),
                content: serde_json::json!({
                    "type": "subtask_completed",
                    "subtask_id": ctx.subtask_id,
                    "session_id": ctx.sub_session_id,
                    "exit_reason": ctx.exit_reason,
                    "result": result_text,
                })
                .to_string(),
                speaker_id: None,
                turn_number: None,
                metadata_json: None,
                created_at: None,
            };
            opencrab_db::queries::insert_session_log_best_effort(&conn, &log);
        }
    }

    // 1b. #553: subtask セッションの死活を永続化する。決着したら sub-session の
    //     `sessions.status` を `exit_reason`（completed / error / timeout / stopped_by_limit）
    //     へ遷移させ、`status='active'` のままにしない。これで再起動を跨いだ起動時リコンサイル
    //     （active=孤児）と併せ、死活が永続状態から判定できる。sink 発火より前の DB 書き込み。
    //     sub-session 行を持たない自動 dispatch では 0 行更新で無害。
    if !ctx.sub_session_id.is_empty() {
        if let Ok(conn) = db.lock() {
            let _ = opencrab_db::queries::set_session_status(
                &conn,
                &ctx.sub_session_id,
                &ctx.exit_reason,
            );
        }
    }

    // 2. registry から除去し、除去したエントリから reply_target と caller を回収する。
    //    remove 後は引けないため、remove の戻り値から読み出す（#167 / #298）。
    let removed = registry.remove(&ctx.subtask_id).map(|(_, subtask)| subtask);
    let reply_target = removed.as_ref().and_then(|s| s.reply_target.clone());
    // registry に載っていない一発呼び（テスト等）は最小権限へ倒す（fail-closed）。
    //
    // 本番でここに来るのは「insert した registry と settle に渡す registry の食い違い」
    // ＝配線バグで、#298 が直した降格（owner/trusted のツールが resume の瞬間に
    // 消える）が無言で復活する。黙って倒さず必ず記録する（#302）。
    let caller = match removed {
        Some(subtask) => subtask.caller,
        None => {
            tracing::warn!(
                subtask_id = %ctx.subtask_id,
                session_id = %ctx.parent_session_id,
                "subtask registry entry missing at settlement; resuming with least privilege \
                 (registry wiring mismatch?)"
            );
            crate::traits::CallerIdentity::Agent
        }
    };

    // 3. sink を発火する（本文は運ばない = DB 永続化済み）。継続を起こすかの判断は
    //    `dispatch_settled`（#638・唯一の実装）が持つ——sink のメソッドを直接呼ばない。
    dispatch_settled(
        sink,
        SubtaskSettled {
            session_id: ctx.parent_session_id,
            agent_id: ctx.agent_id,
            subtask_id: ctx.subtask_id,
            exit_reason: ctx.exit_reason,
            kind: SettleKind::Completed,
            reply_target,
            caller,
        },
    );
}
