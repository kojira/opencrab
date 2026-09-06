#[cfg(any(test, feature = "extgate-probe"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use opencrab_actions::AgentRuntime;
use rusqlite::{params, Transaction};

use crate::bundle::{apply_bundle_member, BundleApply, NostrBundleAdmit};
use crate::error::GateError;
use crate::protocol::Said;
use crate::registry::ExtgateState;
use crate::ResolveCallerFn;

use super::binding::OriginRow;
use super::nostr_profile::fire_nostr_relay;
use super::turn::enqueue_turn;
use super::SaidOutcome;

pub(super) struct BundleCtx<'a, R> {
    pub(super) state: &'a Arc<ExtgateState>,
    pub(super) runtime: &'a R,
    pub(super) resolve_caller: ResolveCallerFn,
    pub(super) row: &'a OriginRow,
    pub(super) said: &'a Said,
    pub(super) session_id: &'a str,
}

pub(super) fn conclude_unrecorded_bundle<R: AgentRuntime>(
    tx: Transaction<'_>,
    ctx: &BundleCtx<'_, R>,
    bundle: Option<&NostrBundleAdmit>,
) -> Result<SaidOutcome, GateError> {
    let Some(admit) = bundle else {
        tx.commit().map_err(|_| GateError::store())?;
        return Ok(SaidOutcome { seq: None });
    };
    let applied = apply_bundle_member(&tx, &ctx.said.binding_id, admit, false)?;
    finish_bundle(tx, ctx, applied, None)
}

pub(super) fn finish_bundle<R: AgentRuntime>(
    tx: Transaction<'_>,
    ctx: &BundleCtx<'_, R>,
    applied: BundleApply,
    seq: Option<i64>,
) -> Result<SaidOutcome, GateError> {
    let pending = if applied.enqueue {
        let origin = applied
            .trigger_origin
            .as_deref()
            .ok_or_else(GateError::store)?;
        let trigger = load_bundle_trigger(&tx, ctx.session_id, origin)?;
        Some((trigger, origin.to_string(), applied.new_admitted))
    } else {
        None
    };
    let drop_turn = pending.is_some() && !ctx.state.turn_queues.has_room(ctx.session_id);
    tx.commit().map_err(|_| GateError::store())?;
    if seq.is_some() {
        fire_nostr_relay(ctx.state, ctx.row, ctx.said);
    }
    if drop_turn {
        let dropped = ctx.state.turn_queues.note_dropped();
        #[cfg(any(test, feature = "extgate-probe"))]
        ctx.state
            .probe
            .turn_queue_dropped
            .fetch_add(1, Ordering::SeqCst);
        tracing::warn!(
            session_id = ctx.session_id,
            dropped_total = dropped,
            "extgate: session turn queue full; bundle turn dropped"
        );
        return Ok(SaidOutcome { seq });
    }
    if let Some((trigger, origin, n)) = pending {
        let suffix = bundle_prompt_suffix(n);
        let trigger_said = Said {
            id: String::new(),
            binding_id: ctx.said.binding_id.clone(),
            origin,
            author_id: trigger.0,
            text: trigger.1,
            attachments: trigger.2,
        };
        enqueue_turn(
            Arc::clone(ctx.state),
            ctx.runtime.clone(),
            ctx.resolve_caller,
            ctx.row,
            &trigger_said,
            ctx.session_id,
            &suffix,
            // #933: bundle は coordinator ターンで個別 said の畳み込み対象外。None で skip/prune 対象外。
            None,
            false, // bundle: 単一返信先が無い（gateway が standalone post で publish・row292）
        );
    }
    Ok(SaidOutcome { seq })
}

fn load_bundle_trigger(
    tx: &Transaction<'_>,
    session_id: &str,
    origin: &str,
) -> Result<(String, String, Vec<String>), GateError> {
    match tx.query_row(
        "SELECT speaker_id, content, metadata_json FROM memory_sessions
         WHERE session_id = ?1 AND json_extract(metadata_json, '$.external_origin') = ?2
         ORDER BY id DESC LIMIT 1",
        params![session_id, origin],
        |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        },
    ) {
        Ok((speaker, content, meta)) => {
            let images = meta
                .as_deref()
                .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                .and_then(|v| v.get("image_urls").cloned())
                .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
                .unwrap_or_default();
            Ok((speaker.unwrap_or_default(), content, images))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) | Err(_) => Err(GateError::store()),
    }
}

fn bundle_prompt_suffix(count: u32) -> String {
    format!(
        "[Nostr] タイムライン watch の束ね（{count} 件）です。窓内を 1 ターンの文脈に載せています。\
         心が動いた投稿にはエアリプ（本文をそのまま書けば独立した新規投稿として publish されます・\
         特定投稿への返信にはなりません）で触れてよい。特定の投稿に反応するなら reply(e番号, 本文)／\
         reaction(e番号)／repost(e番号) を使ってください。反応不要なら NO_REPLY とだけ答えてください。"
    )
}
