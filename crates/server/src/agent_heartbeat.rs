//! エージェントが**自分自身の**ハートビート設定を読み書きするツール（#247 / #336 / #456）。
//!
//! - `get_my_heartbeat`: 現在の有効/無効・間隔・次回発火時刻（`next_fire_at`）、境界値
//!   （下限・上限・既定）を返す。enabled なのに発火しない理由があればそれも返す。
//! - `set_my_heartbeat`: 有効/無効と間隔を更新する。
//! - `run_my_heartbeat`（#599）: 時間を待たずに手動発火する（テスト用）。**オーナー / co_agent 限定**
//!   （bridge の `OWNER_ONLY_ACTIONS`）。**時間発火とまったく同じ経路**
//!   （[`crate::heartbeat_fire::run_one_heartbeat`]）を通り、`last_fired_at` は更新しない。
//!
//! # セッション単位に一本化（#456・設計 §13）
//!
//! **スコープは存在しない。** かつては `scope="agent"`（`agent_heartbeat_config`）と
//! `scope="channel"`（`discord_channel_config`）の二択があり、エージェントに設定を頼むと
//! 「セッションスコープと agent スコープを取り違えて混乱する」のが #456 の発端だった。
//! PR3 でスコープ引数・応答フィールド・説明文からスコープの二重性を一掃し、**常に「いま
//! 話しているセッション」**（`ctx.session_id`）に対して設定・照会する。エージェントは
//! `enabled` と `interval_secs` だけを指定する（選ぶべきスコープが無い）。
//!
//! 発火先（Nostr broadcast / Discord channel）は `session_id` の接頭辞から導く（設計 §3.6）。
//! 発火経路を持つのは **`nostr-` / `discord-` セッションだけ**なので、それ以外の種別
//! （`web-` / `heartbeat-` / `agent-msg-` 等）で呼ばれたら**明示エラーで拒否**する
//! （fail-closed）。「enabled にできたのに永遠に発火しない行」を作らせない（発端が UX
//! なので「設定できたのに発火しない」は解決になっていない・設計 §13.1）。
//!
//! # 「自分のだけ」をどう保証しているか
//!
//! **引数に `agent_id` が無い。** 対象は常に `ctx.agent_id`（bridge が実行境界で組む
//! 呼び出し文脈）。さらに `agent_id`（および紛らわしい別名）や**廃止したスコープ引数**
//! （`scope` / `channel_id` / `guild_id`）が引数に現れたら**明示エラーで拒否**する。
//! 無視して黙って現在セッションへ書くと、エージェントは「別のチャンネル / 別スコープを
//! 設定した」と思い込んだままになる（#456 が潰したい混乱そのもの）。
//!
//! # 権限
//!
//! **オーナー限定にしない**（自分の設定を自分で触れることがこの機能の目的）。ただし素の
//! `Agent`（未信頼の外部ユーザー由来ターン）からは見えないよう `TRUSTED_ONLY_ACTIONS` に
//! 入れ、ハンドラ内でも同じ検査をする（多層防御）。指示文（`update_heartbeat_instructions`）
//! はオーナー限定のまま（「いつ動くか」と「動いたとき何をするか」は別・#247）。

use serde_json::json;

use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};

use crate::AppState;

/// 呼び出し元権限の検査（多層防御）。bridge の `TRUSTED_ONLY_ACTIONS` と同じ範囲。
fn ensure_trusted(ctx: &GatewayCallContext) -> Option<GatewayActionResult> {
    if matches!(
        ctx.caller,
        GatewayCaller::Owner | GatewayCaller::CoAgent { .. } | GatewayCaller::TrustedUser
    ) {
        return None;
    }
    Some(GatewayActionResult {
        success: false,
        data: None,
        error: Some("このアクションは信頼済みの呼び出し元のみ実行できます".to_string()),
    })
}

/// 呼び出し元権限の検査（多層防御）。bridge の `OWNER_ONLY_ACTIONS` と同じ範囲
/// （オーナー / co_agent のみ）。`run_my_heartbeat`（#599）用。**新しいゲートではなく**、
/// bridge の owner_only ポリシーを handler 側でも同じ範囲で二重に確認するだけ。
fn ensure_owner_or_coagent(ctx: &GatewayCallContext) -> Option<GatewayActionResult> {
    if matches!(
        ctx.caller,
        GatewayCaller::Owner | GatewayCaller::CoAgent { .. }
    ) {
        return None;
    }
    Some(GatewayActionResult {
        success: false,
        data: None,
        error: Some("このアクションはオーナーまたは co_agent のみ実行できます".to_string()),
    })
}

/// 他エージェントを指そうとする引数を拒否する。
///
/// このツールは `ctx.agent_id` しか見ないので、指定しても**効かない**。効かない指定を
/// 黙って捨てると「他のエージェントの設定を変えた」という誤解が残るため、明示的に落とす。
fn reject_foreign_target(args: &serde_json::Value) -> Option<GatewayActionResult> {
    for key in ["agent_id", "target_agent_id", "agent"] {
        if args.get(key).is_some() {
            return Some(GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!(
                    "{key}は指定できません（このツールは呼び出し元エージェント自身の設定だけを扱います）"
                )),
            });
        }
    }
    None
}

/// 廃止したスコープ引数（`scope` / `channel_id` / `guild_id`）を拒否する（#456 / 設計 §13）。
///
/// スコープは撤廃され、対象は常に**現在のセッション**になった。旧習慣でこれらを渡されたら
/// 黙殺せず**明示エラーで新しい振る舞いへ誘導する**（黙って現在セッションへ書くと「別
/// チャンネルを設定した」と誤解が残る＝#456 が潰したい混乱そのもの）。
fn reject_removed_scope_args(args: &serde_json::Value) -> Option<GatewayActionResult> {
    for key in ["scope", "channel_id", "guild_id"] {
        if args.get(key).is_some() {
            return Some(GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!(
                    "{key}は廃止されました。ハートビートは常に「いま話しているセッション」に対して設定・照会されます（enabled と interval_secs だけ指定してください）。"
                )),
            });
        }
    }
    None
}

/// `error` だけを持つ失敗レスポンスを組む短縮子。
fn err(msg: impl Into<String>) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg.into()),
    }
}

/// `enabled` 引数を解釈する。`None` = 未指定 / `Some(b)` = 明示。型違いは Err。
fn parse_enabled_arg(args: &serde_json::Value) -> Result<Option<bool>, GatewayActionResult> {
    match args.get("enabled") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(err("enabledは真偽値で指定してください")),
    }
}

/// `interval_secs` 引数を解釈する。
/// - `None` = 未指定（現在値を保つ）
/// - `Some(None)` = 明示 null（= 運用者既定に戻す）
/// - `Some(Some(secs))` = 明示値（下限・上限・正整数を検証済み）
///
/// 下限より短い / 上限より長い / 非正整数は**拒否**する（丸めない）。丸めると、エージェントは
/// 要求した値で動いていると思い込んだまま別の間隔で走る。
fn parse_interval_arg(
    args: &serde_json::Value,
    min: u64,
    max: u64,
) -> Result<Option<Option<i64>>, GatewayActionResult> {
    match args.get("interval_secs") {
        None => Ok(None),
        Some(serde_json::Value::Null) => Ok(Some(None)),
        Some(v) => match v.as_i64() {
            Some(secs) if secs > 0 => {
                if (secs as u64) < min {
                    return Err(err(format!(
                        "interval_secsが短すぎます（最小{min}秒。指定値: {secs}秒）"
                    )));
                }
                if (secs as u64) > max {
                    return Err(err(format!(
                        "interval_secsが長すぎます（最大{max}秒。指定値: {secs}秒）"
                    )));
                }
                Ok(Some(Some(secs)))
            }
            _ => Err(err("interval_secsは正の整数（秒）で指定してください")),
        },
    }
}

/// 現在のセッションを発火先へ解決する（#628: transport 登録簿へ問い合わせる）。
///
/// `ctx.session_id` が無い（セッション文脈なし）→ fail-closed エラー。発火経路が無い種別
/// （登録済み descriptor がどれも名乗らない）→ fail-closed エラー。発火（scheduler）と同じ
/// 登録簿を引くので「設定できたのに永遠に発火しない行」を作らせない（設計 §13.1）。
fn current_session_target(
    state: &AppState,
    ctx: &GatewayCallContext,
) -> Result<(String, opencrab_actions::FireTarget), GatewayActionResult> {
    let session_id = match ctx.session_id.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => {
            // 理由だけでなく **remedy（次に何をすればよいか）** を書く（#456 の発端は「混乱」
            // なので、拒否で詰まらせると混乱を別の形にすり替えるだけになる・M-b）。
            return Err(err(
                "このセッションからはハートビートを設定・照会できません（セッション文脈がありません）。設定したい対象のセッション——Discord のチャンネル、または Nostr の自発投稿——で実行してください。",
            ));
        }
    };
    match state.timed_fire_router.resolve_target(session_id, &ctx.agent_id) {
        Some(target) => Ok((session_id.to_string(), target)),
        // 理由（発火経路が無い種別）＋ remedy（どこで実行すればよいか）を 1 読で示す（M-b）。
        None => Err(err(
            "このセッションからはハートビートを設定・照会できません（このセッション種別には発火経路がありません）。設定したい対象のセッション——Discord のチャンネル、または Nostr の自発投稿——で実行してください。",
        )),
    }
}

/// 現在セッションの設定 + 境界値 + 次回発火時刻を 1 つの JSON にまとめる。
///
/// 読み出しも更新後も同じ形（session_payload に一本化・旧 agent/channel の 2 系統を廃止）。
/// `next_fire_at` は列を持たず**照会時に算出**する（設計 §4.3 の `heartbeat_next_fire_at`）。
/// enabled なのに発火しない理由（`gated` / `gated_reason`）も併せて返す（#394 / #4）。
fn session_payload(
    state: &AppState,
    conn: &rusqlite::Connection,
    agent_id: &str,
    session_id: &str,
    target: &opencrab_actions::FireTarget,
) -> serde_json::Value {
    let limits = state.heartbeat_limits;
    let row = opencrab_db::queries::get_session_heartbeat_config(conn, agent_id, session_id)
        .ok()
        .flatten();

    let enabled = row.as_ref().map(|r| r.enabled).unwrap_or(false);
    let configured = row.as_ref().and_then(|r| r.interval_secs);
    // 実効間隔（fail-closed 解決）。None = 壊れた値（0 以下）で発火しない。
    let effective = opencrab_db::queries::resolve_session_interval_secs(
        configured,
        limits.default_interval_secs,
        limits.min_interval_secs,
    );
    let anchor_at = row.as_ref().and_then(|r| r.anchor_at.clone());
    let last_fired_at = row.as_ref().and_then(|r| r.last_fired_at.clone());

    // next_fire_at（#439-4）: 無効・発火経路なし・壊れた間隔・起点なしは null。
    // 起点（anchor/last_fired）から算出する（列は持たない・真実は再計算・設計 §4.3）。
    let next_fire_at = if enabled {
        effective.and_then(|interval| {
            let anchor = anchor_at
                .as_ref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc));
            let last = last_fired_at
                .as_ref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc));
            opencrab_db::queries::heartbeat_next_fire_at(anchor, last, interval)
                .map(|t| t.to_rfc3339())
        })
    } else {
        None
    };

    // enabled なのに発火しない理由（本人に見せる・#394）。**whitelist は現行 HB 発火経路に
    // ゲートとして存在しないので理由に含めない**（含めると発火に影響しない嘘の理由になる・
    // 設計 §5 N3 / §13.1 訂正）。実際の非発火ゲートは (a) 壊れた間隔・(b) G ゲート対象 transport
    // （Discord）の live G。**「Discord か」ではなく「G ゲート対象か」を descriptor に問う**（#628）。
    let live_g = state.heartbeat_config_rx.borrow().enabled;
    let g_gated = state
        .timed_fire_router
        .descriptor(target.kind)
        .map(|d| d.is_g_gated())
        .unwrap_or(false);
    let gated_reason: Option<String> = if enabled {
        if effective.is_none() {
            Some(
                "設定された間隔が不正（0 以下）なため発火しません。有効な間隔（秒）を指定し直してください。"
                    .to_string(),
            )
        } else if g_gated && !live_g {
            Some(
                "グローバルのハートビートが無効化（[agent] heartbeat_enabled=false）されているため、現在このセッションは発火しません（運用者設定）。"
                    .to_string(),
            )
        } else {
            None
        }
    } else {
        None
    };

    json!({
        "session_id": session_id,
        "enabled": enabled,
        "interval_secs": effective,
        "configured_interval_secs": configured,
        "anchor_at": anchor_at,
        "last_fired_at": last_fired_at,
        "next_fire_at": next_fire_at,
        "gated": gated_reason.is_some(),
        "gated_reason": gated_reason,
        "min_interval_secs": limits.effective_min(),
        "max_interval_secs": crate::config::HeartbeatLimits::MAX_INTERVAL_SECS,
        "default_interval_secs": limits.default_interval_secs,
    })
}

/// 自分のハートビート設定を読み出す（常に現在のセッションが対象）。
pub(crate) fn get_my_heartbeat(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    if let Some(denied) = ensure_trusted(ctx) {
        return denied;
    }
    if let Some(denied) = reject_foreign_target(args) {
        return denied;
    }
    if let Some(denied) = reject_removed_scope_args(args) {
        return denied;
    }

    let (session_id, target) = match current_session_target(state, ctx) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let conn = state.db.lock().unwrap();
    GatewayActionResult {
        success: true,
        data: Some(session_payload(
            state,
            &conn,
            &ctx.agent_id,
            &session_id,
            &target,
        )),
        error: None,
    }
}

/// 自分のハートビート設定を更新する（常に現在のセッションが対象）。
///
/// 下限より短い間隔は**拒否**する（丸めない）。エラーには下限を載せるので、同じターンで
/// 有効な値に直して呼び直せる（このツールを inline 分類にしている理由でもある）。更新後は
/// 中央スケジューラを起こして**再起動なしで即時反映**する（#437・設計 §3.5a）。
pub(crate) fn set_my_heartbeat(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    if let Some(denied) = ensure_trusted(ctx) {
        return denied;
    }
    if let Some(denied) = reject_foreign_target(args) {
        return denied;
    }
    if let Some(denied) = reject_removed_scope_args(args) {
        return denied;
    }

    let limits = state.heartbeat_limits;
    let min = limits.effective_min();
    let max = crate::config::HeartbeatLimits::MAX_INTERVAL_SECS;

    let enabled_arg = match parse_enabled_arg(args) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let interval_arg = match parse_interval_arg(args, min, max) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if enabled_arg.is_none() && interval_arg.is_none() {
        return err("enabled か interval_secs のどちらかが必要です");
    }

    let (session_id, target) = match current_session_target(state, ctx) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let conn = state.db.lock().unwrap();
    let existing =
        opencrab_db::queries::get_session_heartbeat_config(&conn, &ctx.agent_id, &session_id)
            .ok()
            .flatten();

    // 行が無いときの土台は**無効**（#240: 設定を作っただけで自律実行が始まらない）。
    let new_enabled =
        enabled_arg.unwrap_or_else(|| existing.as_ref().map(|r| r.enabled).unwrap_or(false));
    let new_interval: Option<i64> = match interval_arg {
        Some(v) => v,
        None => existing.as_ref().and_then(|r| r.interval_secs),
    };

    // アンカー / 発火記録の向き（#605 で §4.4 を実態へ修正）:
    //
    //   - **`last_fired_at` は設定変更で触らない**。これは「実際に発火した時刻」の事実で、
    //     進めてよいのは発火経路（`set_session_last_fired`・発火成功時のみ）だけ。以前は
    //     有効化のたびに `NULL` へ落としていたため、調整のたびに発火記録が消え、次回発火が
    //     `anchor+interval` へ戻って**永久に先送り**されていた（#605）。**常に existing を保持**する
    //     （upsert も CONFLICT では `last_fired_at` を触らないので二重に担保）。
    //
    //   - **`anchor_at` は起点が無いときだけ `now` を打つ**（`existing.anchor_at.or(now)`）。
    //     起点（anchor/last_fired）が既にあれば据え置く。理由: 起点があるのに `now` へ張り直すと、
    //     `last_fired` が効かなくなる or 位相が毎回先送りされる。起点が無い**初回有効化**でだけ
    //     `now` を打ち、最初の発火を `now+interval` にする（enable 直後の即発火を避ける）。
    //
    //   - **無効化 / 無効のまま**（enabled=false）→ anchor/last_fired は保存（再有効化まで位相を保つ）。
    //
    // 結果: 既存行の**再有効化・間隔変更後の next_fire は `last_fired.or(anchor) + interval`**。
    // これが過ぎていれば即発火する（§4.4 が本来言っていた「ユーザ起点の短縮は密になってよい」が
    // ここで実現される。`3h→15min` に縮めたら 15 分待たされるのではなく、過ぎていれば直ちに発火）。
    // **即発火は「発火済み」に限らない**: まだ一度も発火しておらず（last_fired=None）、`anchor` が
    // 古いまま間隔を短縮した場合も `anchor+interval` が過ぎて即発火する（例: T0 に 3h で有効化 →
    // 初回発火前に 15min へ短縮 → anchor+15min が過去 → 即発火）。据え置いた起点を基準に密になる、
    // という一貫した挙動で、**初回有効化（起点を now に打つ）だけが「interval をまるごと待つ」**。
    // 「初回は待つ / 再有効化・短縮は即発火しうる」の非対称は set_my_heartbeat のツール説明にも書く。
    let now = chrono::Utc::now().to_rfc3339();
    // last_fired_at は事実。設定変更では常に保持（新規行なら None＝未発火）。
    let last_fired_at = existing.as_ref().and_then(|r| r.last_fired_at.clone());
    let anchor_at = if new_enabled {
        // 起点が無いとき（初回有効化）だけ now。既存の起点は据え置く。
        existing
            .as_ref()
            .and_then(|r| r.anchor_at.clone())
            .or_else(|| Some(now.clone()))
    } else {
        // 無効化 / 無効のまま: 起点を保存（再有効化まで位相を保つ）。
        existing.as_ref().and_then(|r| r.anchor_at.clone())
    };

    let row = opencrab_db::queries::SessionHeartbeatConfigRow {
        agent_id: ctx.agent_id.clone(),
        session_id: session_id.clone(),
        enabled: new_enabled,
        interval_secs: new_interval,
        anchor_at,
        last_fired_at,
    };

    if let Err(e) = opencrab_db::queries::upsert_session_heartbeat_config(&conn, &row) {
        return err(format!("ハートビート設定の保存に失敗: {e}"));
    }

    tracing::info!(
        agent_id = %ctx.agent_id,
        session_id = %session_id,
        caller = %ctx.caller.label(),
        old_enabled = existing.as_ref().map(|r| r.enabled),
        old_interval_secs = existing.as_ref().and_then(|r| r.interval_secs),
        new_enabled = row.enabled,
        new_interval_secs = row.interval_secs,
        "セッション単位のハートビート設定を更新した"
    );

    // #437: 中央スケジューラを起こして再起動なしで即時反映する（opt-in の反映が最大
    // MAX_SLEEP 遅れる問題を PR3 で解消）。payload は載せない（取りこぼしても次ウェイクで
    // 収束する自己回復・設計 §3.5）。
    state.scheduler_wake.notify_one();

    let mut payload = session_payload(state, &conn, &ctx.agent_id, &session_id, &target);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("success".to_string(), json!(true));
    }
    GatewayActionResult {
        success: true,
        data: Some(payload),
        error: None,
    }
}

/// ハートビートを**時間を待たずに手動発火**する（#599・オーナー / co_agent 限定）。
///
/// # 時間発火とまったく同じ経路
///
/// scheduler の時刻発火と**同じ関数**（[`crate::heartbeat_fire::run_one_heartbeat`]）を呼ぶ。
/// テスト用の別経路は作らない（別経路だとテストで通っても本番で挙動が割れる・#599）。発火先の
/// 解決（session_id → `FireTarget`）も設定ツールと同じ transport 登録簿を引く（#628）。
///
/// # 対象セッション
///
/// 引数 `session_id` を渡せばそのセッション、省略すれば**現在のセッション**を発火する。現在
/// セッション以外を指定できるのはオーナー / co_agent 限定だから（テスト用途）。発火経路の無い
/// 種別（`web-` 等）は fail-closed で拒否する。
///
/// # `last_fired_at` は更新しない
///
/// 手動発火は時間発火の位相をずらさないため `last_fired_at` を刻まない（`run_one_heartbeat` は
/// そもそも刻まない。刻むのはスケジューラの発火ループだけ）。発火の記録（`heartbeat_log`）は残す。
///
/// # 自己デッドロックを避ける（設計時に判明・#599）
///
/// このツールは呼び出しターンの中で走り、そのターンは既に現在セッションの直列化ロックを保持して
/// いる。`TimedFire` は**イベントを送るだけ**でロックを取らないので送信自体は詰まらないが、受け取った
/// ループは同じセッションのロックを取ろうとして待つ。したがって発火は `spawn` して**即座に「投げた」を
/// 返し**、実際のターンは今のターンが終わってから走る（スケジューラと同じ構造）。
pub(crate) fn run_my_heartbeat(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    // owner_only（bridge の OWNER_ONLY_ACTIONS と同ポリシー）を handler でも確認する（多層防御）。
    if let Some(denied) = ensure_owner_or_coagent(ctx) {
        return denied;
    }
    if let Some(denied) = reject_foreign_target(args) {
        return denied;
    }
    if let Some(denied) = reject_removed_scope_args(args) {
        return denied;
    }

    // 対象セッション: 明示 session_id 引数（テスト用）、無ければ現在のセッション。どちらも
    // 発火経路の無い種別は fail-closed で拒否する（transport 登録簿と同じ解決・#628）。
    let (session_id, target) = match args.get("session_id") {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => {
            match state.timed_fire_router.resolve_target(s, &ctx.agent_id) {
                Some(t) => (s.to_string(), t),
                None => {
                    return err(format!(
                        "session_id「{s}」には発火経路がありません（発火できるのは discord- / nostr- セッションだけです）。"
                    ));
                }
            }
        }
        Some(serde_json::Value::Null) | None => match current_session_target(state, ctx) {
            Ok(v) => v,
            Err(e) => return e,
        },
        Some(_) => return err("session_id は文字列で指定してください"),
    };

    // 受け口の有無を先に確認する（UX: ゲートウェイ未稼働なら黙って spawn せず即エラーで返す）。
    // これは発火経路ではなく事前検証（読み取りのみ）。実際の発火は run_one_heartbeat が回す。
    // 受け口の kind は発火先の transport（descriptor が名乗った kind）そのもの（#628）。
    let kind = target.kind;
    if !state.timed_fire_router.has_sink_for_kind(kind) {
        return err(format!(
            "{kind} のゲートウェイが稼働していないため発火できません（受け口が未登録）。ゲートウェイの起動を確認してください。"
        ));
    }

    // 時間発火とまったく同じ経路を spawn で回す（呼び出しターンが現在セッションのロックを
    // 持っているので、受信ループのターンは現在のターン終了後に走る）。ツールは即座に返す。
    let fire_state = state.clone();
    let fire_agent_id = ctx.agent_id.clone();
    tokio::spawn(async move {
        crate::heartbeat_fire::run_one_heartbeat(&fire_state, &fire_agent_id, &target).await;
    });

    tracing::info!(
        agent_id = %ctx.agent_id,
        session_id = %session_id,
        caller = %ctx.caller.label(),
        "run_my_heartbeat: 手動でハートビートを発火した（#599・last_fired_at は更新しない）"
    );

    GatewayActionResult {
        success: true,
        data: Some(json!({
            "fired": true,
            "session_id": session_id,
            "note": "ハートビートを発火しました。実際のターンは今のターンが終わってから同じセッションで走ります（last_fired_at は更新しません）。",
        })),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(caller: GatewayCaller) -> GatewayCallContext {
        GatewayCallContext::new(caller, "agent-x")
    }

    /// #599: `run_my_heartbeat` の handler ゲートは owner / co_agent のみ通す（bridge の
    /// `OWNER_ONLY_ACTIONS` と同ポリシーを多層防御で確認）。trusted user・素の Agent は弾く。
    #[test]
    fn run_gate_allows_only_owner_and_coagent() {
        assert!(ensure_owner_or_coagent(&ctx(GatewayCaller::Owner)).is_none());
        assert!(ensure_owner_or_coagent(&ctx(GatewayCaller::CoAgent {
            agent_id: "peer".to_string()
        }))
        .is_none());
        // trusted user は get/set は使えるが（`ensure_trusted`）、手動発火は owner_only。
        assert!(ensure_owner_or_coagent(&ctx(GatewayCaller::TrustedUser)).is_some());
        assert!(ensure_owner_or_coagent(&ctx(GatewayCaller::Agent)).is_some());
    }

    /// bridge の分類と一致すること: `run_my_heartbeat` は owner_only かつ inline。
    #[test]
    fn run_my_heartbeat_is_owner_only_and_inline() {
        assert!(
            opencrab_actions::OWNER_ONLY_ACTIONS.contains(&"run_my_heartbeat"),
            "owner_only ゲートに入っていない（外部ユーザーから手動発火できてしまう）"
        );
        assert!(
            opencrab_actions::default_non_dispatch_tools().contains("run_my_heartbeat"),
            "inline 分類に入っていない（発火の spawn を background 化する意味は無い）"
        );
    }
}
