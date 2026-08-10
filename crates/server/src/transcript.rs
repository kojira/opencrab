//! ターン/会話ログの転記サービス（#42）。
//!
//! 「`SessionLogRow` を組み立てて `insert_session_log` する」パターンが discord /
//! server に metadata を少しずつ変えて散在し、記録ポリシーの所有者が居なかった。
//! ターンイベント（ユーザー発言 / エージェント応答 / NO_REPLY / A2UI 応答）の
//! 行の形はこのモジュールが所有する。呼び出し側は行を組み立てず、型付きの
//! パラメータを渡す。
//!
//! 書き込みは全て best-effort（失敗は warn ログ、応答フローは止めない — #47）。
//! 各関数の行の形（agent_id / speaker_id / metadata のキー）は移行前の各サイトの
//! 形を正確に保存している。形を変えるときは会話ビルダー（build_conversation_string）
//! と web の消費側を確認すること。
//!
//! ## transport 中立化（#158 S3 / #156 S6）
//!
//! 以前は Discord 用の関数だけが `#[cfg(feature = "discord")]` 配下にあった。理由は
//! シグネチャが `opencrab_discord` の型（`DiscordReplyContext` / `InteractionRecord`）を
//! 引いていたことだけで、行の形自体は Nostr と同型だった。型を
//! `opencrab_actions::transcript` へ移したので機能フラグは不要になり、受信発話と
//! エージェント応答は **由来（[`TranscriptSource`]）を引数で受ける 1 つの関数**に統合した。
//!
//! `metadata_json` は統合前とバイト等価。根拠は 2 つ:
//!
//! - `source` / `triggered_by` の値は [`TranscriptSource`] と
//!   [`AgentReplyContext`] だけが持ち、統合前の文字列をそのまま出す（自由文字列で
//!   受けないので綴り違いが混入しない）。
//! - キーの有無は `Option` / 空判定で統合前と同じ条件に保つ（無い由来では出さない）。
//!   `serde_json` の `Map` は `preserve_order` 無効時 `BTreeMap` なので、出力は
//!   キー集合だけで決まり挿入順に依存しない。
//!
//! 下の `tests` が全経路の `metadata_json` をリテラル文字列で固定している。web の表示
//! （`SessionDetail` / `SessionCard`）は `source` の文字列比較で分岐しているため、
//! この不変条件が守られている限り web は無変更で動く。

use opencrab_actions::transcript::{
    AgentReplyContext, InboundMessageRecord, InteractionRecord, OutboundReplyRecord,
    TranscriptSource,
};
use opencrab_db::queries::{insert_session_log_best_effort, SessionLogRow};
use rusqlite::Connection;

/// 受信発言の記録をあきらめるまでの試行回数（#284 P0-3）。
///
/// 現実的な失敗は SQLITE_BUSY（同一 DB への並行書き込み）で、数十 ms 待てば通る。
/// ディスク満杯・権限のような回復しない失敗で長く粘っても意味は無いので 3 回で切り、
/// 呼び出し側にエスカレーションさせる。
const INBOUND_RECORD_ATTEMPTS: usize = 3;
/// 再試行の待機（n 回目の失敗後に `INBOUND_RETRY_DELAY * n` 待つ）。
const INBOUND_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// ゲートウェイから受信した発言（Discord / Nostr 共通）。記録できたら `true`。
///
/// 行の帰属: `agent_id` 列＝**受信側エージェント**（`record.recipient_agent_id`）、
/// `speaker_id` 列＝**送信者**（`record.sender_id`）。以前は両列に送信者を入れていたが、
/// 記憶索引・FTS 記憶検索は `WHERE agent_id = <当該エージェント>` で走査するため、
/// 受信を送信者名義にすると相手の発言が索引・検索へ一切載らなかった（#377）。相手の
/// 識別は `speaker_id` が担うので、`is_user_speech`（`speaker_id != <引数の agent_id>`）
/// や impressions（`speaker_id` で相手を引く）はこの変更で挙動が変わらない。
///
/// **ここだけは best-effort にしない**（#284 P0-3）。他の転記（応答・NO_REPLY）は
/// 落ちても会話は続くが、ユーザー発言が落ちるとその指示は**二度と会話履歴に現れず**、
/// エージェントは見ないまま応答する。実際に「オーナーの発言が DB に 1 件も無い」
/// 事故が起きており、当時のログには warn が 1 行残るだけだった。
/// 失敗は戻り値で呼び出し側へ返し、呼び出し側がオーナーへエスカレーションする。
pub fn record_inbound_message(
    conn: &Connection,
    source: TranscriptSource,
    record: &InboundMessageRecord<'_>,
) -> bool {
    let row = SessionLogRow {
        id: None,
        agent_id: record.recipient_agent_id.to_string(),
        session_id: record.session_id.to_string(),
        log_type: "speech".to_string(),
        content: record.text.to_string(),
        speaker_id: Some(record.sender_id.to_string()),
        turn_number: None,
        metadata_json: Some(inbound_metadata_json(source, record)),
        created_at: None,
    };
    for attempt in 1..=INBOUND_RECORD_ATTEMPTS {
        match opencrab_db::queries::insert_session_log(conn, &row) {
            Ok(_) => return true,
            Err(e) => {
                tracing::warn!(
                    session_id = %row.session_id,
                    attempt,
                    "inbound message insert failed: {e}"
                );
                if attempt < INBOUND_RECORD_ATTEMPTS {
                    std::thread::sleep(INBOUND_RETRY_DELAY * attempt as u32);
                }
            }
        }
    }
    tracing::error!(
        session_id = %row.session_id,
        attempts = INBOUND_RECORD_ATTEMPTS,
        "inbound user message could NOT be persisted; the agent will not see it"
    );
    false
}

/// NO_REPLY（沈黙の明示）。
pub fn record_agent_no_reply(conn: &Connection, agent_id: &str, session_id: &str) {
    insert_session_log_best_effort(
        conn,
        &SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "speech".to_string(),
            content: "NO_REPLY".to_string(),
            speaker_id: Some(agent_id.to_string()),
            turn_number: None,
            metadata_json: Some(serde_json::json!({"no_reply": true}).to_string()),
            created_at: None,
        },
    );
}

/// 自己重複判定で遡る speech 行の窓。自分の直近応答は普通ごく最近なので浅くてよいが、
/// 相手の発言や tool 由来の speech が挟まっても直近の自分の応答へ届く程度に取る。
const SELF_DUPLICATE_LOOKBACK: usize = 50;

/// 会話重複判定用の正規化: 前後空白の除去・連続空白の 1 個への畳み込み・小文字化のみ。
/// **fuzzy にしない**（同義言い換えや部分一致は見ない。誤検知と複雑さを避ける / #486）。
fn normalize_reply(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// 送ろうとしている応答が、**自分自身の直近の応答**と正規化後に完全一致するか（#486）。
///
/// **再回答を抑えるガードがコード上どこにも無いために入れる機械的バックストップ。**
/// モデルの従順性に一切依存しない — プロンプト層の自己重複ルール（案 A）は過去に無視された
/// 実績（本番プロンプト 936 本に旧ガード文が入っていたのに bot は返し合っていた）があるので、
/// 確実な抑止はこちら（案 B）が本命。判定材料は「自分の出力 vs 自分の過去の出力」だけで、
/// **相手が bot か人かは一切見ない**（理念: システムは相手が bot か判定しない）。相手が
/// 誰であれ、自分が直前に言ったのと同じ文面をもう一度送ろうとしたら真。
///
/// **これは退行の修正ではない**（症状は 3 月から存在し、むしろ 8 月頭の方が多かった）。
/// **根本原因である「再起動 × デデュープ不在」の増幅は #543 の管轄**で、この関数単体では
/// ループは無くならない（構造的バックストップにすぎない。これで直ったと見なさないこと）。
///
/// - 比較対象は**自分（`agent_id`）の直近の実応答 1 件**のみ。`NO_REPLY` の記録行
///   （[`record_agent_no_reply`] が書く content=="NO_REPLY"）は飛ばす（沈黙は「直近の応答」
///   ではない）。他者の発言は `speaker_id` で除外する。
/// - 一致は正規化後の**完全一致のみ**（[`normalize_reply`]）。話題が変わって間に別の応答を
///   挟めば「直近の応答」が変わるので、後のターンで同じ相槌が出ても抑止しない（過剰抑止回避）。
/// - 空応答は対象外（呼び出し側が別途 NO_REPLY 扱いする）。
pub fn is_self_duplicate(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
    candidate: &str,
) -> bool {
    let norm = normalize_reply(candidate);
    if norm.is_empty() {
        return false;
    }
    // id DESC（新しい順）で speech 行だけを引く。
    let recent = match opencrab_db::queries::list_recent_session_logs_of_type(
        conn,
        session_id,
        "speech",
        SELF_DUPLICATE_LOOKBACK,
    ) {
        Ok(rows) => rows,
        Err(_) => return false,
    };
    for row in recent {
        if row.speaker_id.as_deref() != Some(agent_id) {
            continue; // 他者の発言は見ない
        }
        if row.content.trim() == "NO_REPLY" {
            continue; // 沈黙の記録は「直近の応答」ではない
        }
        // 最初に見つかった「自分の実応答」= 直近応答。それとだけ比べる。
        return normalize_reply(&row.content) == norm;
    }
    false
}

/// エージェントがゲートウェイへ返した応答（Discord / Nostr 共通）。
///
/// metadata の `triggered_by` 等の差分は [`AgentReplyContext`] で表す。
pub fn record_outbound_reply(
    conn: &Connection,
    source: TranscriptSource,
    record: &OutboundReplyRecord<'_>,
) {
    insert_session_log_best_effort(
        conn,
        &SessionLogRow {
            id: None,
            agent_id: record.agent_id.to_string(),
            session_id: record.session_id.to_string(),
            log_type: "speech".to_string(),
            content: record.text.to_string(),
            speaker_id: Some(record.agent_id.to_string()),
            turn_number: None,
            metadata_json: Some(outbound_metadata_json(source, record)),
            created_at: None,
        },
    );
}

/// REST 経由のエージェント応答（sessions.rs / agents_messages.rs 共通の形）。
///
/// ゲートウェイ経由の応答と違い `source` を持たない（統合前からそう）。
pub fn record_rest_agent_reply(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
    text: &str,
    iterations: usize,
    tool_calls_made: usize,
) {
    insert_session_log_best_effort(
        conn,
        &SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "speech".to_string(),
            content: text.to_string(),
            speaker_id: Some(agent_id.to_string()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({
                    "iterations": iterations,
                    "tool_calls_made": tool_calls_made,
                })
                .to_string(),
            ),
            created_at: None,
        },
    );
}

/// A2UI インタラクション応答の記録。
pub fn record_interaction_response(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
    record: &InteractionRecord<'_>,
) {
    insert_session_log_best_effort(
        conn,
        &SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "interaction_response".to_string(),
            content: record.content.to_string(),
            speaker_id: Some("system".to_string()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({
                    "interaction_id": record.interaction_id,
                    "surface_id": record.surface_id,
                    "action_name": record.action_name,
                    "component_id": record.component_id,
                    "responder_id": record.responder_id,
                })
                .to_string(),
            ),
            created_at: None,
        },
    );
}

/// 受信発話の `metadata_json`。
///
/// キーの有無は統合前と同じ条件に保つ: Discord は `channel_id`（Nostr には無い）、
/// Nostr は `pubkey`（Discord には無い）、`user_avatar_url` / `image_urls` は
/// 値があるときだけ出す。
fn inbound_metadata_json(source: TranscriptSource, record: &InboundMessageRecord<'_>) -> String {
    let mut meta = serde_json::json!({
        "source": source.inbound(),
        "user_name": record.sender_name,
    });
    if let Some(channel_id) = record.channel_id {
        meta["channel_id"] = serde_json::json!(channel_id);
    }
    if let Some(pubkey) = record.pubkey {
        meta["pubkey"] = serde_json::json!(pubkey);
    }
    if let Some(url) = record.avatar_url {
        meta["user_avatar_url"] = serde_json::json!(url);
    }
    if !record.image_urls.is_empty() {
        meta["image_urls"] = serde_json::json!(record.image_urls);
    }
    meta.to_string()
}

/// エージェント応答の `metadata_json`。
fn outbound_metadata_json(source: TranscriptSource, record: &OutboundReplyRecord<'_>) -> String {
    let mut meta = serde_json::json!({ "source": source.reply() });
    if let Some(channel_id) = record.channel_id {
        meta["channel_id"] = serde_json::json!(channel_id);
    }
    match &record.context {
        // 起動要因を記録しない由来（Nostr）: `source` だけの形を保つ。
        None => {}
        Some(AgentReplyContext::Direct { tool_calls_made }) => {
            meta["tool_calls_made"] = serde_json::json!(tool_calls_made);
        }
        Some(AgentReplyContext::SubtaskCompleted) => {
            meta["triggered_by"] = serde_json::json!("subtask_completed");
        }
        Some(AgentReplyContext::InteractionResponse { interaction_id }) => {
            meta["triggered_by"] = serde_json::json!("interaction_response");
            meta["interaction_id"] = serde_json::json!(interaction_id);
        }
    }
    meta.to_string()
}

/// `metadata_json` のバイト等価をリテラルで固定するスナップショットテスト（#158 S3）。
///
/// 期待値は統合前の各サイト（`record_discord_user_message` /
/// `record_discord_agent_reply` / `record_nostr_user_message` /
/// `record_nostr_agent_reply`）が書いていた JSON をそのまま写したもの。web の表示は
/// `source` の文字列で分岐するので、ここが割れたら web も直す必要がある。
#[cfg(test)]
mod tests {
    use super::*;

    fn discord_inbound<'a>(
        avatar_url: Option<&'a str>,
        image_urls: &'a [String],
    ) -> InboundMessageRecord<'a> {
        InboundMessageRecord {
            session_id: "sess-1",
            recipient_agent_id: "agent-1",
            sender_id: "111",
            sender_name: "のすたろう",
            avatar_url,
            channel_id: Some("222"),
            pubkey: None,
            text: "こんにちは",
            image_urls,
        }
    }

    #[test]
    fn discord_inbound_metadata_is_byte_identical() {
        assert_eq!(
            inbound_metadata_json(TranscriptSource::Discord, &discord_inbound(None, &[])),
            r#"{"channel_id":"222","source":"discord","user_name":"のすたろう"}"#
        );
    }

    #[test]
    fn discord_inbound_metadata_keeps_avatar_and_images() {
        let images = [
            "https://cdn/a.png".to_string(),
            "https://cdn/b.png".to_string(),
        ];
        assert_eq!(
            inbound_metadata_json(
                TranscriptSource::Discord,
                &discord_inbound(Some("https://cdn/avatar.png"), &images)
            ),
            r#"{"channel_id":"222","image_urls":["https://cdn/a.png","https://cdn/b.png"],"source":"discord","user_avatar_url":"https://cdn/avatar.png","user_name":"のすたろう"}"#
        );
    }

    #[test]
    fn nostr_inbound_metadata_is_byte_identical() {
        let record = InboundMessageRecord {
            session_id: "sess-1",
            recipient_agent_id: "agent-1",
            sender_id: "npub-hex",
            sender_name: "だれか",
            avatar_url: None,
            channel_id: None,
            pubkey: Some("npub-hex"),
            text: "よろしく",
            image_urls: &[],
        };
        assert_eq!(
            inbound_metadata_json(TranscriptSource::Nostr, &record),
            r#"{"pubkey":"npub-hex","source":"nostr","user_name":"だれか"}"#
        );
    }

    fn discord_reply(context: Option<AgentReplyContext<'_>>) -> OutboundReplyRecord<'_> {
        OutboundReplyRecord {
            agent_id: "agent-1",
            session_id: "sess-1",
            channel_id: Some("222"),
            text: "はい",
            context,
        }
    }

    #[test]
    fn discord_direct_reply_metadata_is_byte_identical() {
        assert_eq!(
            outbound_metadata_json(
                TranscriptSource::Discord,
                &discord_reply(Some(AgentReplyContext::Direct { tool_calls_made: 3 }))
            ),
            r#"{"channel_id":"222","source":"discord_response","tool_calls_made":3}"#
        );
    }

    #[test]
    fn discord_subtask_completed_reply_metadata_is_byte_identical() {
        assert_eq!(
            outbound_metadata_json(
                TranscriptSource::Discord,
                &discord_reply(Some(AgentReplyContext::SubtaskCompleted))
            ),
            r#"{"channel_id":"222","source":"discord_response","triggered_by":"subtask_completed"}"#
        );
    }

    #[test]
    fn discord_interaction_reply_metadata_is_byte_identical() {
        assert_eq!(
            outbound_metadata_json(
                TranscriptSource::Discord,
                &discord_reply(Some(AgentReplyContext::InteractionResponse {
                    interaction_id: "int-9"
                }))
            ),
            r#"{"channel_id":"222","interaction_id":"int-9","source":"discord_response","triggered_by":"interaction_response"}"#
        );
    }

    #[test]
    fn nostr_reply_metadata_is_byte_identical() {
        let record = OutboundReplyRecord {
            agent_id: "agent-1",
            session_id: "sess-1",
            channel_id: None,
            text: "はい",
            context: None,
        };
        assert_eq!(
            outbound_metadata_json(TranscriptSource::Nostr, &record),
            r#"{"source":"nostr_response"}"#
        );
    }

    /// 宛先の識別子（web の表示が参照する `channel_id`）を落としていないこと。
    #[test]
    fn discord_rows_keep_destination_id() {
        assert!(
            inbound_metadata_json(TranscriptSource::Discord, &discord_inbound(None, &[]))
                .contains(r#""channel_id":"222""#)
        );
        assert!(outbound_metadata_json(
            TranscriptSource::Discord,
            &discord_reply(Some(AgentReplyContext::Direct { tool_calls_made: 0 }))
        )
        .contains(r#""channel_id":"222""#));
    }

    /// 書き込みまで通した行の形（agent_id / speaker_id / log_type / metadata）。
    ///
    /// 受信行は `agent_id`＝受信側エージェント（`recipient_agent_id`="agent-1"）、
    /// `speaker_id`＝送信者（"111"）。#377 以前は両列とも送信者だった。
    #[test]
    fn recorded_rows_match_pre_merge_shape() {
        let conn = opencrab_db::init_memory().unwrap();

        record_inbound_message(
            &conn,
            TranscriptSource::Discord,
            &discord_inbound(None, &[]),
        );
        record_outbound_reply(
            &conn,
            TranscriptSource::Discord,
            &discord_reply(Some(AgentReplyContext::SubtaskCompleted)),
        );

        let rows: Vec<(String, String, String, String)> = conn
            .prepare(
                "SELECT agent_id, speaker_id, log_type, metadata_json FROM memory_sessions \
                 WHERE session_id = 'sess-1' ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(
            rows,
            vec![
                (
                    "agent-1".to_string(),
                    "111".to_string(),
                    "speech".to_string(),
                    r#"{"channel_id":"222","source":"discord","user_name":"のすたろう"}"#
                        .to_string(),
                ),
                (
                    "agent-1".to_string(),
                    "agent-1".to_string(),
                    "speech".to_string(),
                    r#"{"channel_id":"222","source":"discord_response","triggered_by":"subtask_completed"}"#
                        .to_string(),
                ),
            ]
        );
    }

    /// #377: 受信行が「受信側エージェント名義」で入り、そのエージェントの
    /// 索引ビルド入力（`get_unindexed_session_logs`）と FTS 記憶検索に載る。
    /// 送信者名義では載らない（旧規約の回帰ガード）。
    #[test]
    fn inbound_is_indexed_and_searchable_under_the_recipient() {
        let conn = opencrab_db::init_memory().unwrap();
        let record = InboundMessageRecord {
            session_id: "nostr-agent-1",
            recipient_agent_id: "agent-1",
            sender_id: "peer-9",
            sender_name: "peer",
            avatar_url: None,
            channel_id: None,
            pubkey: Some("peer-9"),
            text: "how many grams does pasta weigh",
            image_urls: &[],
        };
        assert!(record_inbound_message(
            &conn,
            TranscriptSource::Nostr,
            &record
        ));

        // 索引ビルド入力（`WHERE agent_id = ?`）に受信側名義で入る。
        let for_agent =
            opencrab_db::queries::get_unindexed_session_logs(&conn, "agent-1", 0, 100).unwrap();
        let row = for_agent
            .iter()
            .find(|r| r.content == "how many grams does pasta weigh")
            .expect("受信行が受信側エージェントの索引入力に入っていない");
        assert_eq!(row.agent_id, "agent-1");
        assert_eq!(row.speaker_id.as_deref(), Some("peer-9"));

        // 送信者名義では索引入力に入らない（旧規約の回帰ガード）。
        let for_sender =
            opencrab_db::queries::get_unindexed_session_logs(&conn, "peer-9", 0, 100).unwrap();
        assert!(
            for_sender.is_empty(),
            "受信行が送信者名義で索引入力に残っている（#377 回帰）"
        );

        // FTS 記憶検索が受信側エージェントでヒットする（= 目的）。
        let hits =
            opencrab_db::queries::search_session_logs(&conn, "agent-1", "pasta", 10).unwrap();
        assert!(
            hits.iter()
                .any(|h| h.content == "how many grams does pasta weigh"),
            "受信側エージェントの FTS 検索で相手の発言が引けない"
        );
        // 送信者名義では FTS でも引けない。
        let sender_hits =
            opencrab_db::queries::search_session_logs(&conn, "peer-9", "pasta", 10).unwrap();
        assert!(sender_hits.is_empty());
    }

    // ---- #486: is_self_duplicate（自分の直近応答との完全一致で送信を止める）----

    fn speech(conn: &Connection, session_id: &str, speaker: &str, content: &str, no_reply: bool) {
        insert_session_log_best_effort(
            conn,
            &SessionLogRow {
                id: None,
                agent_id: "a1".to_string(),
                session_id: session_id.to_string(),
                log_type: "speech".to_string(),
                content: content.to_string(),
                speaker_id: Some(speaker.to_string()),
                turn_number: None,
                metadata_json: no_reply.then(|| r#"{"no_reply":true}"#.to_string()),
                created_at: None,
            },
        );
    }

    /// 今回の事象の再現: 自分が既に「とすて」と言った状態で、同じ「とすて」を返そうとしたら
    /// **送らない**（重複＝真）。別の文面なら真ではない。
    #[test]
    fn self_duplicate_detects_immediate_repeat() {
        let conn = opencrab_db::init_memory().unwrap();
        speech(&conn, "s1", "a1", "とすて", false);
        assert!(is_self_duplicate(&conn, "a1", "s1", "とすて"));
        assert!(!is_self_duplicate(&conn, "a1", "s1", "べつのはなし"));
    }

    /// 正規化は **trim / 連続空白の畳み込み / 大小のみ**。fuzzy にしない
    /// （句読点や語尾が違えば別物扱い）。
    #[test]
    fn self_duplicate_normalization_is_trim_space_case_only() {
        let conn = opencrab_db::init_memory().unwrap();
        speech(&conn, "s1", "a1", "Hello  World", false); // 元は大文字＋二重空白
        assert!(is_self_duplicate(&conn, "a1", "s1", "hello world")); // 大小＋空白畳み込み
        assert!(is_self_duplicate(&conn, "a1", "s1", "  Hello World  ")); // trim
        assert!(!is_self_duplicate(&conn, "a1", "s1", "Hello World!")); // 記号追加は別物
        assert!(!is_self_duplicate(&conn, "a1", "s1", "Hello")); // 部分一致も別物
    }

    /// 自分の NO_REPLY 記録を挟んでも、その直前の実応答と比べる（沈黙は「直近の応答」ではない）。
    #[test]
    fn self_duplicate_skips_no_reply_marker() {
        let conn = opencrab_db::init_memory().unwrap();
        speech(&conn, "s1", "a1", "とすて", false);
        speech(&conn, "s1", "a1", "NO_REPLY", true);
        assert!(is_self_duplicate(&conn, "a1", "s1", "とすて"));
    }

    /// 過剰抑止しない: 話題が変わり、間に別の応答を挟めば直近応答が変わるので、
    /// 昔と同じ短い相槌をもう一度返しても止めない。
    #[test]
    fn self_duplicate_allows_repeat_after_intervening_reply() {
        let conn = opencrab_db::init_memory().unwrap();
        speech(&conn, "s1", "a1", "はい", false);
        speech(&conn, "s1", "a1", "了解しました", false); // 直近応答はこちら
        assert!(!is_self_duplicate(&conn, "a1", "s1", "はい"));
    }

    /// 相手の種別を見ない: 判定は「自分の直近応答」だけで決まる。
    #[test]
    fn self_duplicate_ignores_peer_identity() {
        // 相手が人間 sim でも bot sim でも、自分の直近応答と一致すれば同じく重複。
        for peer in ["human-user-123", "bot-agent-x"] {
            let conn = opencrab_db::init_memory().unwrap();
            speech(&conn, "s1", peer, "とすて", false); // 相手の発言
            speech(&conn, "s1", "a1", "とすて", false); // 自分の応答
            assert!(
                is_self_duplicate(&conn, "a1", "s1", "とすて"),
                "peer={peer} でも自分の直近応答と一致すれば重複"
            );
        }
        // 相手が同じ文面を言っただけ（自分は未発言）では重複ではない。
        let conn = opencrab_db::init_memory().unwrap();
        speech(&conn, "s1", "bot-agent-x", "とすて", false);
        assert!(!is_self_duplicate(&conn, "a1", "s1", "とすて"));
    }
}
