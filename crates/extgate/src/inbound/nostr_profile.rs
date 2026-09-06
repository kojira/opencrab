use crate::protocol::Said;
use crate::registry::ExtgateState;

use super::binding::OriginRow;

pub(super) fn fire_nostr_relay(state: &ExtgateState, row: &OriginRow, said: &Said) {
    let body = nostr_renderer_body(&said.text);
    let (_, label) = nostr_renderer_meta(&said.author_id, &said.text);
    let author = nostr_author_label(&said.author_id);
    let text = format!("[Nostr / {label}] {author}\n{body}");
    state.relay_nostr_inbound(&row.agent_id, text);
}

pub(super) fn recorded_said_text(
    state: &ExtgateState,
    row: &OriginRow,
    said: &Said,
    session_id: &str,
) -> String {
    if row.kind_id != "nostr" {
        return said.text.clone();
    }
    let renderer = nostr_renderer_body(&said.text);
    opencrab_actions::sanitize_tool_result_for_log(
        "nostr_inbound",
        renderer,
        session_id,
        &said.origin,
        state.nostr_workspace_root(&row.agent_id).as_deref(),
    )
}

const V1_PREFIX: &str = "[NOSTRGATE/V1 ";

const BUNDLE_PREFIX: &str = "[NOSTRBUNDLE/V1 ";

pub(super) fn nostr_renderer_body(text: &str) -> &str {
    let Some(first) = text.lines().next() else {
        return text;
    };
    if !first.starts_with(V1_PREFIX) {
        return text;
    }
    let rest = text.get(first.len()..).unwrap_or("");
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let Some(second) = rest.lines().next() else {
        return rest;
    };
    if second.starts_with(BUNDLE_PREFIX) {
        let after = rest.get(second.len()..).unwrap_or("");
        return after.strip_prefix('\n').unwrap_or(after);
    }
    rest
}

/// accept_inbound へ渡す kind ラベル。nostr は実 kind から導出し（メンション/リプライ/リアクション/
/// リポスト/長文/DM）、それ以外の transport は従来どおり "said"。
///
/// watch 車線経由の said は `accept_inbound` の権限デバウンス（`watch_hold_interval_secs` →
/// `AGREED_IMMEDIATE_KINDS`）でこのラベルを見る。"said" 固定だと owner/followee のメンション・
/// リプライ・リアクションでも即応にならず保留される（QC #10・Defect B）。
pub(super) fn inbound_kind_label(row: &OriginRow, said: &Said) -> &'static str {
    if row.kind_id != "nostr" {
        return "said";
    }
    let (_, label) = nostr_renderer_meta(&said.author_id, &said.text);
    label
}

pub(super) fn nostr_prompt_suffix(author_id: &str, text: &str) -> String {
    // §9A / DI-16 / row292: 普通の投稿は本文をそのまま書く（standalone publish・post 関数はない）。
    // 返信は明示 reply(e番号, 本文)、リアクションは reaction(e番号)、リポストは repost(e番号)。
    // 生 ID（pubkey/note）はプロンプトへ出さない（会話は u/e/c 番号で参照する）。
    let (kind, label) = nostr_renderer_meta(author_id, text);
    let author = nostr_author_label(author_id);
    format!(
        "[Nostr] {author} さんの投稿（kind:{kind}／{label}）への応答です。\n\
         普通の投稿は本文をそのまま書いてください（新規ノートとして publish されます）。\n\
         この投稿へ返信するなら reply(e番号, 本文)、リアクションは reaction(e番号)、\
         リポストは repost(e番号) を使ってください。会話に出ている e番号 で対象を指定します。\n\
         反応が不要なら NO_REPLY とだけ答えてください。",
    )
}

fn nostr_author_label(author_id: &str) -> String {
    let short: String = author_id.chars().take(12).collect();
    format!("{short}…")
}

fn nostr_renderer_meta(_author_id: &str, text: &str) -> (u32, &'static str) {
    let renderer = nostr_renderer_body(text);
    if let Some(meta) = parse_renderer_line(renderer) {
        return meta;
    }
    let (kind, _event_id) =
        parse_v1_kind_and_event(text).expect("admitted nostr said has a V1 anchor");
    (kind, nostr_kind_label(kind))
}

/// history 行 `[Nostr kind:{kind} {label}]` から種別だけを取る（§9A.2 で from=/target= は撤去）。
fn parse_renderer_line(renderer: &str) -> Option<(u32, &'static str)> {
    let line = renderer.lines().last()?;
    let rest = line.strip_prefix("[Nostr kind:")?;
    let inner = rest.strip_suffix(']')?;
    let (kind_s, label) = inner.split_once(' ')?;
    let kind: u32 = kind_s.parse().ok()?;
    Some((kind, nostr_kind_from_label(label)))
}

fn parse_v1_kind_and_event(text: &str) -> Option<(u32, String)> {
    let line = text.lines().next()?.trim();
    let rest = line.strip_prefix(V1_PREFIX)?;
    let json = rest.strip_suffix(']')?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let kind = value.get("kind")?.as_u64()? as u32;
    let event_id = value.get("event_id")?.as_str()?.to_string();
    Some((kind, event_id))
}

/// V1 anchor の `reply_to`（対象ノート event_id・null/欠落は None）。row295c 6b。
pub(super) fn parse_v1_reply_to(text: &str) -> Option<String> {
    let line = text.lines().next()?.trim();
    let rest = line.strip_prefix(V1_PREFIX)?;
    let json = rest.strip_suffix(']')?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value
        .get("reply_to")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn nostr_kind_from_label(label: &str) -> &'static str {
    match label {
        "DM" => "DM",
        "リアクション" => "リアクション",
        "長文" => "長文",
        "リプライ" => "リプライ",
        "リポスト" => "リポスト",
        _ => "メンション",
    }
}

fn nostr_kind_label(kind: u32) -> &'static str {
    match kind {
        4 | 1059 => "DM",
        7 => "リアクション",
        6 | 16 => "リポスト",
        30023 => "長文",
        _ => "メンション",
    }
}
