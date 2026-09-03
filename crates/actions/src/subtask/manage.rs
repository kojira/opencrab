use super::sink::{SubtaskCompletionSink, SubtaskSettled};
use super::{CallerIdentity, SettleKind, SpawnedSubtask, SubtaskRegistry};

/// 走行中 subtask に対する管理操作（cancel / steer）の共有認可述語（#331 / #647）。
///
/// `caller` が owner 等価なら常に許可。そうでなければ「呼び出し元セッションが親
/// （`parent_session_id == caller_session_id`）」**かつ**「呼び出し元の信頼度が
/// subtask を spawn した親ターンの呼び出し元（`s.caller`）以上」を要する。後者が無いと、
/// セッション 1 本化（#323）で「セッション一致」だけでは素の Agent ターンから Owner 由来の
/// subtask を操作できてしまう（#331）。
///
/// cancel と steer は「走行中サブへ外から手を出す」点で同じ権限境界を持つため、判定を
/// **1 つの関数に集約**する（steer 用に別の認可を発明しない / #647 裁定）。呼び出し側は
/// shard ロック下（`remove_if` の述語内）や `get` 参照下でこれを評価する。所有権フィールド
/// （`parent_session_id` / `caller`）は insert 後不変なので TOCTOU は無い。
pub(crate) fn caller_can_manage_subtask(
    caller: &CallerIdentity,
    caller_session_id: Option<&str>,
    s: &SpawnedSubtask,
) -> bool {
    if caller.is_owner_equivalent() {
        return true;
    }
    matches!(caller_session_id, Some(cs) if !cs.is_empty() && s.parent_session_id == cs)
        && caller.can_manage_subtask_of(&s.caller)
}

/// `cancel_subtask` の結果種別（gateway 非依存 / #161）。
///
/// gateway 別の戻り値整形（`GatewayActionResult` の success/error）は呼び出し側が
/// 行う。ここでは「停止した / 不在 / 権限なし」の三値だけを型で返し、認可と registry
/// 操作を 1 箇所（`cancel_subtask`）に集約する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// 対象を abort し registry から除去した。
    Cancelled,
    /// 対象 `subtask_id` が registry に存在しない。
    NotFound,
    /// 存在するが呼び出し元に権限が無い（親セッション/owner 以外）。
    Unauthorized,
}

/// 走行中 subtask を停止する中核処理（gateway 非依存 / #161・#157 S2）。
///
/// web / Nostr / REST など Discord 以外の transport でも `cancel_subtask` ツールを
/// 露出できるよう、認可・abort・registry 除去・親ログ記録・lifecycle 通知を
/// server-neutral 層へ集約する。**停止の実装はこの 1 関数だけ**で、transport 固有の
/// 実装は持たない（#157 S2 で Discord 実装を撤去し、その固有の後始末をここへ取り込んだ）。
///
/// # Discord 実装から取り込んだ 2 点（#157 S2）
///
/// 1. **中断の通知送出**: `notifiers`（registry と対の随伴マップ）から通知口を引いて
///    `on_cancelled(duration_ms)` を呼び、マップから外す。abort すると spawned closure は
///    中断されて終了通知が来ないため、ここが lifecycle 通知の唯一の終端になる（RFC §1.5）。
///    **順序契約との関係**: この通知は親ログ INSERT より前だが、実装（Discord の
///    `SubtaskWebhookNotifier::on_cancelled`）は webhook 配送キューへ 1 通積むだけで
///    応答生成を起動しない。「記録 → registry 除去 → sink 発火」という二重返信防止の
///    順序契約に触れるのは resume する `sink` 側だけで、そちらは従来どおり INSERT の後。
/// 2. **停止ログの説明文の解決順序**: sub-session の `sessions.theme`（`Subtask: ` prefix を
///    除去）を第一候補にし、引けない/空のときだけ registry の `label`
///    （例: `execute_shell(...)`）へフォールバックする。明示的な `spawn_subtask` は
///    人間可読なテーマを持つが、自動 dispatch は sub-session の行を作らないため theme を
///    引けず、そのままだと親ログが `subtask '' was cancelled` になる（#176）。
///
/// 認可（#64 / #331）: `caller` が Owner なら常に許可。そうでなければ「呼び出し元
/// セッションが親（`parent_session_id == caller_session_id`）」**かつ**「呼び出し元の
/// 信頼度が subtask を spawn した親ターンの呼び出し元（`s.caller`）以上」の subtask のみ
/// 停止できる（自己/兄弟/他セッションのもの、および格上の呼び出し元が spawn したものは
/// 不可）。後者の caller 判定はセッション 1 本化（#323）で「セッション一致」だけでは素の
/// Agent ターンから Owner 由来の subtask を止められてしまうため（#331）。`remove_if` は
/// shard ロック下で述語を評価するため、「認可確認 → 削除」の間にエントリが差し替わる
/// TOCTOU が無い（所有権フィールドは insert 後不変）。
///
/// 成功時: **停止を主張（`claim_cancel`）** → `abort_handle.abort()` → registry から
/// 除去 → 通知口へ `on_cancelled` → 親セッションログへ `tool_cancelled` を best-effort
/// 記録 → sink へ `on_subtask_cancelled`（`exit_reason="cancelled"`）を通知する。
/// この順序は旧 Discord 実装（通知が親ログより先）と neutral 実装（親ログが sink より
/// 先）の両方を満たす。
///
/// 停止の主張は registry 除去と同じ shard ロック下（`remove_if` の述語内）で行う。
/// `abort()` は「ツール本体を await 中」なら効くが、既に完走して `settle_completed`
/// へ入っている場合は効かない。そこでラッチで排他し、cancel が勝ったときは
/// `settle_completed` 側が DB 記録も sink 発火も諦める（＝完了イベント無し）。
/// 逆に settle が先に主張していた場合は停止できないので `NotFound` を返す
/// （その subtask は通常完了として通知される）。
///
/// `sink` を渡すと停止も 1 箇所から通知でき、経路側（REST の `sessions.status` 等）が
/// cancel 後に個別に整合を取る必要がなくなる。既定実装は debug ログのみなので、
/// resume する sink（Discord / web / Nostr）の挙動は変わらない。
#[allow(clippy::too_many_arguments)]
pub fn cancel_subtask(
    registry: &SubtaskRegistry,
    db: &opencrab_db::Db,
    sink: Option<&dyn SubtaskCompletionSink>,
    notifiers: Option<&crate::subtask_notify::SubtaskNotifiers>,
    subtask_id: &str,
    caller: CallerIdentity,
    caller_session_id: Option<&str>,
) -> CancelOutcome {
    // #485: co_agent は owner 等価。owner（等価）はセッションをまたいで subtask を停止できる
    // （唯一の源は is_owner_equivalent）。co_agent が owner 由来の subtask を管理できないと協働
    // にならない。非 owner 等価（trusted_user / agent）は従来どおりセッション一致 + trust 序列。
    // 認可は cancel / steer 共有の `caller_can_manage_subtask` に委ねる（#647）。

    // 述語は shard ロック下で評価される。認可 → 停止の主張（CAS）→ 除去を 1 操作に
    // まとめるため、認可も claim も述語内で行う（claim に失敗＝決着済みなら除去しない）。
    match registry.remove_if(subtask_id, |_, s| {
        caller_can_manage_subtask(&caller, caller_session_id, s) && s.lifecycle.claim_cancel()
    }) {
        Some((_, subtask)) => {
            subtask.abort_handle.abort();

            // 中断を lifecycle 通知口へ伝え、随伴マップから外す（旧 Discord 実装から移設
            // / RFC §1.5）。abort で spawned closure は中断されるため終了通知は来ない
            // → ここが唯一の終端。親ログ INSERT より**前**に呼ぶ（旧実装と同順序）。
            if let Some(notifiers) = notifiers {
                if let Some((_, notifier)) = notifiers.remove(subtask_id) {
                    notifier.on_cancelled(subtask.started_at.elapsed().as_millis() as u64);
                }
            }

            // 親セッションログへ subtask_cancelled を best-effort 記録する。
            //
            // **部分結果も残す**（#152 レビュー P2）。1 バッチ = 1 subtask なので、
            // 停止時に `settle_completed` を丸ごと抑止するとラベルしか残らず「3 ファイル
            // 書いた後に止めた」ときにどこまで進んだか分からない。完走済み call を本文へ
            // 列挙し（人が読む/会話へ再注入される）、構造は metadata に載せる。
            let parent = subtask.parent_session_id.clone();
            if !parent.is_empty() {
                let completed = subtask.lifecycle.completed_calls();
                if let Ok(conn) = db.lock() {
                    // 停止対象の説明は sub-session の theme を第一候補にする（旧 Discord
                    // 実装から移設 / #176）。明示的な `spawn_subtask` はここに人間可読な
                    // テーマを持つが、自動 dispatch は sub-session の行を作らないため
                    // theme を引けない。引けない/空のときは registry の label
                    // （例: `execute_shell(...)`）へフォールバックする。
                    let task_description =
                        opencrab_db::queries::get_session(&conn, &subtask.session_id)
                            .ok()
                            .flatten()
                            .map(|session| {
                                session
                                    .theme
                                    .strip_prefix("Subtask: ")
                                    .unwrap_or(&session.theme)
                                    .to_string()
                            })
                            .filter(|desc| !desc.is_empty())
                            .unwrap_or_else(|| subtask.label.clone());
                    let content = if completed.is_empty() {
                        format!("subtask '{task_description}' was cancelled")
                    } else {
                        let partial =
                            serde_json::to_string(&completed).unwrap_or_else(|_| "[]".to_string());
                        format!(
                            "subtask '{}' was cancelled after {} completed tool call(s): {partial}",
                            task_description,
                            completed.len()
                        )
                    };
                    let log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: subtask.agent_id.clone(),
                        session_id: parent.clone(),
                        log_type: "tool_cancelled".to_string(),
                        content,
                        speaker_id: None,
                        turn_number: None,
                        metadata_json: Some(
                            // `task` は旧 Discord 実装のキー、`label` / `completed_calls`
                            // は neutral 実装のキー。統合後は**両方**載せる（どちらの
                            // 読み手も壊さない）。`tool_name` は固定値ではなく
                            // **実際に停止したツール名**（#184）。
                            serde_json::json!({
                                "tool_call_id": subtask_id,
                                "tool_name": subtask.tool_name,
                                "task": task_description,
                                "label": subtask.label,
                                "completed_calls": completed,
                            })
                            .to_string(),
                        ),
                        created_at: None,
                    };
                    opencrab_db::queries::insert_session_log_best_effort(&conn, &log);
                    // #553: subtask セッションの死活を永続化する（cancelled）。settle_completed
                    // の終端化と対をなす。sub-session 行が無ければ 0 行更新で無害。
                    let _ = opencrab_db::queries::set_session_status(
                        &conn,
                        &subtask.session_id,
                        "cancelled",
                    );
                }
            }

            // 停止を sink へ通知する（完了経路とは別メソッド = resume しない）。
            // これで「最後の subtask が cancel されたのに誰もセッションを完了に
            // しない」（REST が永久 active）が起きない。
            if let Some(sink) = sink {
                sink.on_subtask_cancelled(SubtaskSettled {
                    session_id: parent,
                    agent_id: subtask.agent_id.clone(),
                    subtask_id: subtask_id.to_string(),
                    exit_reason: "cancelled".to_string(),
                    kind: SettleKind::Cancelled,
                    reply_target: subtask.reply_target.clone(),
                    caller: subtask.caller.clone(),
                });
            }
            CancelOutcome::Cancelled
        }
        None => {
            // remove_if の None は「不在」「権限なし」「既に決着（settle）済み」。
            // 所有権フィールドは insert 後不変なので contains_key で不在と区別でき、
            // 残っていて claim に失敗した場合（決着済み = もう停止できない）は
            // NotFound として扱う（停止対象として存在しない）。
            match registry.get(subtask_id) {
                Some(entry) if !entry.lifecycle.is_settling() => CancelOutcome::Unauthorized,
                Some(_) => CancelOutcome::NotFound,
                None => CancelOutcome::NotFound,
            }
        }
    }
}

/// steer 指示を sub-session へ記録するときの `log_type`（#647）。
///
/// 通常発話（`speech`）とも system（`system`）とも別の値にすることで、後から
/// 「途中で親/オーナーが方向を変えた」と履歴上で判別できる（#647 受け入れ・記録要件）。
/// 走行中注入の `SubtaskSteerInbound`（server 側）もこの値だけを watermark 差分読みする。
pub const STEER_LOG_TYPE: &str = "steer";

/// `steer_subtask` の結果種別（gateway 非依存 / #647）。
///
/// gateway 別の戻り値整形は呼び出し側が行う。ここは「届いた / 権限なし / steer 不可 /
/// 既に決着済み / 不在」を型で返し、**黙って捨てない**（#647 受け入れ条件 3・4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerOutcome {
    /// sub-session へ steer を記録した。走行中サブが次の反復の合間に読む。
    Accepted,
    /// 存在するが呼び出し元に権限が無い（親セッション/owner 以外）。
    Unauthorized,
    /// 存在するが steer を読む主体がいない（auto-dispatch = LLM ループ無し）。
    NotSteerable,
    /// 既に決着（完了）または停止していた。読む主体がもういない。
    AlreadySettled,
    /// 対象 `subtask_id` が存在しない（registry にも DB のサブセッションにも無い）。
    NotFound,
    /// 対象は正当だが、steer ログ（＝記録かつ配送の唯一のアーティファクト）の書き込みに
    /// 失敗した。記録できなければ配送もされないので、Accepted を返さず失敗を呼び出し側へ
    /// 見せる（fail loud）。
    RecordFailed,
}

/// 走行中 subtask へ追加指示（steer）を届ける中核処理（gateway 非依存 / #647）。
///
/// **設計の核**: サブタスクは独自 engine を組まず `run_agent_response` を depth+1 で
/// 再入し、親ターンと同じ engine ループ（`LiveInboundSource` で反復の合間に新着を
/// 注入する仕組み / #289）を通る。steer はこの既存機構を**そのままサブへ通す**:
/// ここでは sub-session（`subtask-{subtask_id}`）へ `log_type=STEER_LOG_TYPE` の
/// session_log を **1 本書くだけ**。走行中サブの engine に配線された
/// `SubtaskSteerInbound`（server 側）がその行を watermark 差分で読み、次の反復の
/// user メッセージへ足す。
///
/// この 1 本のログが**記録と配送を兼ねる唯一のアーティファクト**である（記録用の
/// 書き込みと配送用のキューを別に持たない = 並行実装を作らない）。RUNNING のまま
/// 受け取り、新しい状態は設けない（溜まった steer は log_type=steer の未読行として
/// あるだけで、lifecycle 状態機械は不変）。
///
/// 認可は cancel と同じ `caller_can_manage_subtask`（owner 等価 or 親セッション一致
/// + trust 序列）。steer 用に別の判定は発明しない（#647 裁定）。
///
/// 戻り値で「届いた / 権限なし / steer 不可 / 既に決着 / 不在」を区別し、決着済みや
/// auto-dispatch へは**黙って捨てず**その旨を返す（#647 受け入れ条件 3・4）:
/// - registry に在り steerable かつ未決着 → 記録して `Accepted`
/// - registry に在るが権限なし → `Unauthorized`
/// - registry に在るが auto-dispatch（`steerable=false`）→ `NotSteerable`
/// - registry に在るが既に settle/cancel を主張済み → `AlreadySettled`
/// - registry に無い → sub-session（`subtask-{id}`）の `status` を引き、
///   completed/cancelled なら `AlreadySettled`、行が無ければ `NotFound`
///
/// registry 参照は `get`（shard 読みロック）で行い、認可・steerable・lifecycle 判定を
/// その参照下で評価してから記録する。参照解放後に settle が入る競合はあり得るが、その
/// 場合 steer ログは書かれても engine が読む前にサブが終わるだけで、無害（決着と同時刻の
/// steer は届かないが、対象はもう居ない）。
pub fn steer_subtask(
    registry: &SubtaskRegistry,
    db: &opencrab_db::Db,
    subtask_id: &str,
    message: &str,
    caller: CallerIdentity,
    caller_session_id: Option<&str>,
) -> SteerOutcome {
    // registry を read ロックで引く。所有権フィールドは insert 後不変なので、参照下で
    // 認可・steerable・lifecycle を評価してよい（cancel の remove_if 述語と同じ不変条件）。
    if let Some(entry) = registry.get(subtask_id) {
        if !caller_can_manage_subtask(&caller, caller_session_id, &entry) {
            return SteerOutcome::Unauthorized;
        }
        // 決着/停止を主張済み（登録簿からの除去が未反映の窓）なら読む主体がもういない。
        if entry.lifecycle.is_settling() || entry.lifecycle.is_cancelled() {
            return SteerOutcome::AlreadySettled;
        }
        // auto-dispatch は LLM ループが無く sub-session 行も作らないので読む主体がいない。
        if !entry.steerable {
            return SteerOutcome::NotSteerable;
        }
        let sub_session_id = entry.session_id.clone();
        let sub_agent_id = entry.agent_id.clone();
        // 参照（shard ロック）を持ったまま DB ロックを取ると deadlock の温床になるため、
        // 記録に必要な値を取り出してから参照を落とす。
        drop(entry);
        // 記録＝配送。書けなければ届かないので Accepted を返さず RecordFailed で失敗を見せる。
        return if record_steer(
            db,
            &sub_session_id,
            &sub_agent_id,
            subtask_id,
            message,
            caller_session_id,
        ) {
            SteerOutcome::Accepted
        } else {
            SteerOutcome::RecordFailed
        };
    }

    // registry に無い: 既に決着/停止して除去されたか、そもそも存在しないか。
    // sub-session id は `subtask-{id}` に固定なので DB の `status` で区別できる（#553 で
    // settle/cancel が status を永続化する）。区別できない場合は fail-closed で NotFound。
    let sub_session_id = format!("subtask-{subtask_id}");
    match db.lock() {
        Ok(conn) => match opencrab_db::queries::get_session(&conn, &sub_session_id) {
            Ok(Some(session)) if matches!(session.status.as_str(), "completed" | "cancelled") => {
                SteerOutcome::AlreadySettled
            }
            // active のまま registry に無いのは通常起きない（走行中は必ず登録簿に居る）。
            // 起きたら「読む主体がいない」＝これ以上追えないので NotFound に倒す。
            _ => SteerOutcome::NotFound,
        },
        Err(_) => SteerOutcome::NotFound,
    }
}

/// steer をサブセッションの履歴へ 1 本記録する（#647）。書けたら `true`。
///
/// `log_type=STEER_LOG_TYPE` で通常発話と区別し、送り主を metadata に残す。**記録＝配送**の
/// 唯一のアーティファクトなので best-effort にはしない: DB ロック失敗も INSERT 失敗も
/// `false` を返し、呼び出し側が `RecordFailed`（fail loud）へ倒せるようにする。
fn record_steer(
    db: &opencrab_db::Db,
    sub_session_id: &str,
    sub_agent_id: &str,
    subtask_id: &str,
    message: &str,
    from_session: Option<&str>,
) -> bool {
    let Ok(conn) = db.lock() else {
        tracing::warn!(
            subtask_id = %subtask_id,
            "steer_subtask: db lock 取得失敗のため記録できず（steer は届かない）"
        );
        return false;
    };
    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: sub_agent_id.to_string(),
        session_id: sub_session_id.to_string(),
        log_type: STEER_LOG_TYPE.to_string(),
        content: message.to_string(),
        speaker_id: None,
        turn_number: None,
        metadata_json: Some(
            serde_json::json!({
                "subtask_id": subtask_id,
                "from_session": from_session,
            })
            .to_string(),
        ),
        created_at: None,
    };
    match opencrab_db::queries::insert_session_log(&conn, &log) {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(
                subtask_id = %subtask_id,
                "steer_subtask: steer ログの INSERT 失敗（steer は届かない）: {e}"
            );
            false
        }
    }
}
