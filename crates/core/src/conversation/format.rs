use super::refs::{external_origin_of, spawn_ack_subtask_id, ConversationRefs};
use super::sanitize::{
    inbound_relation_annotation, outgoing_relation_annotation, render_limit,
    scrub_identifiers_for_display, strip_inbound_meta_for_display, truncate_body,
};
use super::tool_result_fold::{fold_subtask_completed, result_reference};

pub fn format_single_log(log: &opencrab_db::queries::SessionLogRow) -> String {
    format_single_log_with_echo(log, None, None)
}
/// 完了済み tool_call の arguments を `{ref,digest,bytes}` に置換して読む。
/// 未決着 call は `completed_ids` に無いので全文のまま。`refs` があれば §9A の短縮参照
/// （u/e/c 番号・識別子排除・長文切り詰め）を適用する。None なら従来の生表示（単体整形・
/// live inbound 注入・テスト）。
pub fn format_single_log_with_echo(
    log: &opencrab_db::queries::SessionLogRow,
    completed_ids: Option<&std::collections::HashSet<String>>,
    refs: Option<&ConversationRefs>,
) -> String {
    let ts = log
        .created_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        // 括弧間スペースを出さない（`[u5][時刻]e3575:` 形・行数×毎ターンで効く・row295b）。
        .map(|dt| dt.format("[%Y-%m-%d %H:%M:%S]").to_string())
        .unwrap_or_default();

    match log.log_type.as_str() {
        "speech" => match refs {
            Some(r) => {
                let speaker = r.speaker_label(log.speaker_id.as_deref().unwrap_or(&log.agent_id));
                let eref = r.event_of(log).map(|n| format!("e{n}")).unwrap_or_default();
                // 関係注記（row295c）: リプライ/リアクション/リポストは種別を残す。ラベル全廃で
                // 「そもそもリアクションか」が失われた欠陥への対処。対象ノート(→e番号)は現状の
                // 受信転記に記録が無いため `→外部` の最小表記（真の →e番号 は target 記録の
                // データスライス後・報告参照）。素の投稿/メンションは注記なし。
                let relation = inbound_relation_annotation(log, r)
                    .or_else(|| outgoing_relation_annotation(log, r))
                    .unwrap_or_default();
                // 表示時に legacy メタ行・生識別子を剥がしてから切り詰める（row294b・保存は不変）。
                let cleaned = strip_inbound_meta_for_display(&log.content);
                let content = match render_limit(external_origin_of(log).as_deref()) {
                    Some(lim) => truncate_body(&cleaned, lim),
                    None => cleaned,
                };
                format!("[{speaker}]{ts}{eref}{relation}:\n{content}")
            }
            None => {
                let speaker = log.speaker_id.as_deref().unwrap_or(&log.agent_id);
                format!("[{}]{}:\n{}", speaker, ts, log.content)
            }
        },
        "tool_call" => {
            // §9A.2 / row318: 自分の話者行も **描画時に** 名前（くらぶ）へ。生 speaker_id（agent UUID）を
            // 文字列に入れる瞬間を作らない（後段スクラブで <uuid…> にしない）。refs 無し（単体表示）は
            // 従来どおり生 speaker（テスト・live 注入）。
            let raw_speaker = log.speaker_id.as_deref().unwrap_or(&log.agent_id);
            let speaker = match refs {
                Some(r) => r.speaker_label(raw_speaker),
                None => raw_speaker.to_string(),
            };
            if let Some(meta_json) = log.metadata_json.as_deref() {
                if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_json) {
                    // §9A.1 / row292: DI operation の call は arguments を verbatim 保持する
                    // （reply 本文が次ターンで消えない）。log 参照への短縮（→log:N）から除外する。
                    let preserve: std::collections::HashSet<&str> = meta
                        .get("preserve_arg_call_ids")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                        .unwrap_or_default();
                    if let Some(tool_calls_json) =
                        meta.get("tool_calls_json").and_then(|v| v.as_str())
                    {
                        if let Ok(tool_calls) =
                            serde_json::from_str::<serde_json::Value>(tool_calls_json)
                        {
                            if let Some(items) = tool_calls.as_array() {
                                let call_lines: Vec<String> = items
                                    .iter()
                                    .filter_map(|item| {
                                        let id = item.get("id")?.as_str()?;
                                        // 正準形状 {function:{name, arguments:"<json-string>"}} と
                                        // 旧形状 {name, arguments:<object>} の両方に対応する。
                                        let (name, args) = if let Some(func) = item.get("function")
                                        {
                                            let name = func.get("name")?.as_str()?;
                                            let args = func
                                                .get("arguments")
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.to_string())
                                                .unwrap_or_default();
                                            (name, args)
                                        } else {
                                            let name = item.get("name")?.as_str()?;
                                            let args = item
                                                .get("arguments")
                                                .map(|value| value.to_string())
                                                .unwrap_or_default();
                                            (name, args)
                                        };
                                        // 完了済み call の引数は本文を持ち越さない。以前は
                                        // {ref,digest,bytes} を出していたが digest はモデルに不要な
                                        // 内部整合値なので出さず、log 参照だけ短く残す（row295b・#707）。
                                        // ただし DI operation の call（preserve_arg_call_ids）は reply
                                        // 本文が次ターンで消えないよう verbatim 保持し短縮しない（§9A.1/row292）。
                                        let args = if completed_ids
                                            .is_some_and(|set| set.contains(id))
                                            && !preserve.contains(id)
                                        {
                                            format!("→log:{}", log.id.unwrap_or(0))
                                        } else {
                                            args
                                        };
                                        // §9A: call_id を c 番号へ短縮（call_ 生 ID を排除）。
                                        let call_ref = refs
                                            .and_then(|r| r.call_of(id))
                                            .map(|n| format!("c{n}"))
                                            .unwrap_or_else(|| format!("id={id}"));
                                        Some(format!("[{}]: {}({})", call_ref, name, args))
                                    })
                                    .collect();
                                if !call_lines.is_empty() {
                                    return format!(
                                        "[{}]{}:\n[tool_call]:\n{}",
                                        speaker,
                                        ts,
                                        call_lines.join("\n")
                                    );
                                }
                            }
                        }
                    }
                }
            }
            format!("[{}]{}:\n[tool_call]:\n{}", speaker, ts, log.content)
        }
        "tool_result" => {
            let meta = log
                .metadata_json
                .as_deref()
                .and_then(|meta_json| serde_json::from_str::<serde_json::Value>(meta_json).ok());
            let tool_call_id = meta
                .as_ref()
                .and_then(|value| value.get("tool_call_id").and_then(|v| v.as_str()))
                .unwrap_or("?");
            let tool_name = meta
                .as_ref()
                .and_then(|value| value.get("tool_name").and_then(|v| v.as_str()))
                .unwrap_or("unknown");
            // #707: **読みの本文は次のターンへ持ち越さない**。
            //
            // 以前はツール結果の JSON を丸ごと会話へ再生していた。実測（直近 100 件）では
            // tool_result 37 件 22KB に対し人と自分の発言は 13 件 2KB——**会話の 9 割が作業の
            // 残骸で、人の言葉は 5%**。この状態でコンパクションが走れば押し出されるのは古い
            // 人の発言になる（#284「ユーザー発言が 1 件も残らない」／#692 の捏造の下地）。
            //
            // 読みは**もう一度呼べば同じものが得られる**ので、会話には参照だけを残す。落とす
            // のは次のターン以降への持ち越しだけで、そのターンの中では従来どおり本文がモデル
            // へ渡る（ツール往復は会話再構成を通らない）。記録（DB）も完全なまま残す。
            let call_ref = refs
                .and_then(|r| r.call_of(tool_call_id))
                .map(|n| format!("c{n}"))
                .unwrap_or_else(|| format!("id={tool_call_id}"));
            // spawn 受理（`status=="spawned"`）は subtask_id を **描画時に** s 番号へ。生 UUID を
            // 結果本文へ載せない（row295b/row318・result_reference は spawn 受理を success 封筒無しと
            // 見て本文丸ごと返すため、ここで先に短縮形へ分岐する）。refs 無しは "subtask"。
            if let Some(sid) = spawn_ack_subtask_id(log) {
                let sref = refs
                    .and_then(|r| r.subtask_of(&sid))
                    .map(|n| format!("s{n}"))
                    .unwrap_or_else(|| "subtask".to_string());
                return format!(
                    "[tool_result]{ts}:\n[{call_ref}]: {tool_name} → subtask {sref} を起動（本文は会話に残していない）"
                );
            }
            // 失敗本文（握り潰し防止で丸ごと残す）にも生の長識別子が混じる（例: nostr_run 失敗の
            // "Event not found: <64hex>"）。本文表示は生識別子を短縮形へ落とす（row318・検知器が
            // 実データで捕捉した漏れ経路）。要約参照（読み/一覧/成功）には長物が無いので無影響。
            format!(
                "[tool_result]{}:\n[{}]: {} → {}",
                ts,
                call_ref,
                tool_name,
                scrub_identifiers_for_display(&result_reference(tool_name, &log.content))
            )
        }
        "tool_cancelled" => {
            let meta = log
                .metadata_json
                .as_deref()
                .and_then(|meta_json| serde_json::from_str::<serde_json::Value>(meta_json).ok());
            let tool_call_id = meta
                .as_ref()
                .and_then(|value| value.get("tool_call_id").and_then(|v| v.as_str()))
                .unwrap_or("?");
            let tool_name = meta
                .as_ref()
                .and_then(|value| value.get("tool_name").and_then(|v| v.as_str()))
                .unwrap_or("unknown");
            let call_ref = refs
                .and_then(|r| r.call_of(tool_call_id))
                .map(|n| format!("c{n}"))
                .unwrap_or_else(|| format!("id={tool_call_id}"));
            format!(
                "[tool_cancelled]{}:\n[{}]: {} がキャンセルされた\n{}",
                ts, call_ref, tool_name, log.content
            )
        }
        "system" => {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&log.content) {
                if let Some(kind) = value.get("type").and_then(|v| v.as_str()) {
                    // #713: `subtask_completed` **だけ**、入れ子 `result`（ツール実行の本文）を
                    // 会話へ持ち越さず参照へ畳む。他の system type は現状どおり丸ごと
                    // pretty-print（範囲外・塊の証拠なし）。**厳密一致**で 1 type だけ分岐し、
                    // 他の type を巻き込まない（設計 Q2 #8）。
                    if kind == "subtask_completed" {
                        return format_subtask_completed(&value, &log.content, &ts, refs);
                    }
                    let content = serde_json::to_string_pretty(&value)
                        .unwrap_or_else(|_| log.content.clone());
                    return format!("[system: {}]{}:\n{}", kind, ts, content);
                }
            }
            format!("[system]{}:\n{}", ts, log.content)
        }
        // catch-all: 未知の log_type は `content` を**丸ごと**運ぶ（設計 Q2 #9 の構造的盲点）。
        // 将来 log_type を足した人が本文の持ち越しに気づけるよう
        // `unknown_log_types_carry_full_body_through_catch_all` で固定する。
        other => format!("[{}]{}:\n{}", other, ts, log.content),
    }
}

/// `subtask_completed` の完了本文を会話行へ整形する（#713）。入れ子 `result`（ツール実行の本文）を
/// [`fold_subtask_completed`] で参照へ畳んでから、外側の封筒（`subtask_id` / `session_id` /
/// `exit_reason`）はそのまま pretty-print する——監査の相関（起動応答との突き合わせ・記録の在り処）を
/// 会話から消さない。畳めない形（失敗・散文・退避 notice 等）では `result` は原文のまま残るので、
/// 表示は従来の pretty-print と一致する（挙動を変えるのは畳めたときだけ）。
fn format_subtask_completed(
    value: &serde_json::Value,
    raw_content: &str,
    ts: &str,
    refs: Option<&ConversationRefs>,
) -> String {
    // `result` は文字列（`settle_completed` が `result_text` を JSON 文字列として載せる）。
    // 想定外に文字列でなければ触らず pretty-print に委ねる（fail-safe・稀）。
    let Some(result_str) = value.get("result").and_then(|v| v.as_str()) else {
        let pretty =
            serde_json::to_string_pretty(value).unwrap_or_else(|_| raw_content.to_string());
        return format!("[subtask 完了]{ts}:\n{pretty}");
    };

    let exit_reason = value
        .get("exit_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let subtask_id = value
        .get("subtask_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // subtask_id はセッション局所 s 番号へ（生 UUID を出さない・row295b）。refs 無し（単体表示）は
    // 採番できないので "subtask"。session_id/exit_reason 等の定型 field は会話に出さない。
    let label = refs
        .and_then(|r| r.subtask_of(subtask_id))
        .map(|n| format!("s{n}"))
        .unwrap_or_else(|| "subtask".to_string());

    // 本文は畳んだ result だけ（ツール結果 blob は要約・散文はそのまま・切り詰めは fold 内の不変条件）。
    let body = fold_subtask_completed(exit_reason, result_str);
    format!("[{label} 完了]{ts}:\n{body}")
}
