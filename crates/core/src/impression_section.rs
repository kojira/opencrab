//! 会話コンテキストへ前置する `[Impressions]` セクションのビルダ。
//!
//! `update_impression` で書いた人物像（`impressions`）を、**いま話している相手の分だけ**
//! 読み出して会話へ載せる（#314）。人物像は agent スコープなので、Discord で書いた
//! 人物像が Nostr のセッションでも同じ相手なら出る。
//!
//! 台帳（[`crate::task_ledger`]）や [`crate::memory_index`] と同じく「動的な状態は
//! system prompt ではなく会話側」（system は 1h キャッシュされるため）。
//!
//! **全員分は載せない。** セッションの参加者が増えると際限なく膨らむので、直近の
//! 発話者から [`IMPRESSION_MAX_TARGETS`] 人までに絞り、各フィールドも
//! [`IMPRESSION_FIELD_MAX_CHARS`] で切り詰める（切り詰めは
//! [`crate::llm_text::truncate_chars`] — 台帳と同じ機構）。

use anyhow::Result;
use opencrab_db::queries;
use rusqlite::Connection;

use crate::llm_text::truncate_chars;

/// セクションに載せる相手の人数上限（直近に話した順）。
pub const IMPRESSION_MAX_TARGETS: usize = 3;

/// 「直近の発話者」を拾う窓（ユーザー発言の件数）。
///
/// 直近 1 件だけだと、相手が複数いる場では自分の応答の直前に割り込んだ 1 人しか
/// 出ない。窓を少し広げて、その場に居る相手を [`IMPRESSION_MAX_TARGETS`] 人まで拾う。
const IMPRESSION_SPEAKER_WINDOW: usize = 20;

/// 1 フィールドの描画上限（chars）。
const IMPRESSION_FIELD_MAX_CHARS: usize = 300;

/// 見出しの識別子（`target_name` / `target_id`）の描画上限（chars）。
///
/// 本文フィールドと同じく**描画上の上限**であって、書ける内容の制約ではない
/// （DB に入る値には手を付けない）。`target_name` は `update_impression` の引数が
/// そのまま入るので、これが無いと本文だけ 300 字で丸めても見出しが無制限に伸びる。
const IMPRESSION_LABEL_MAX_CHARS: usize = 64;

/// 直近の発話者（新しい順・重複除去）を返す。
fn recent_speaker_ids(conn: &Connection, agent_id: &str, session_id: &str) -> Result<Vec<String>> {
    let logs = queries::list_recent_user_speech_logs(
        conn,
        session_id,
        agent_id,
        IMPRESSION_SPEAKER_WINDOW,
    )?;
    let mut out: Vec<String> = Vec::new();
    for log in logs {
        let Some(speaker) = log.speaker_id.filter(|s| !s.is_empty()) else {
            continue;
        };
        if !out.contains(&speaker) {
            out.push(speaker);
        }
        if out.len() >= IMPRESSION_MAX_TARGETS {
            break;
        }
    }
    Ok(out)
}

/// 1 件の人物像を描画する。中身が全て空なら `None`（見出しだけの行を出さない）。
fn render_impression(imp: &queries::ImpressionRow) -> Option<String> {
    let fields = [
        ("personality", &imp.personality),
        ("style", &imp.communication_style),
        ("recent", &imp.recent_behavior),
        ("stance", &imp.agreement),
        ("notes", &imp.notes),
    ];
    let lines: Vec<String> = fields
        .iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(label, v)| {
            format!(
                "  {label}: {}",
                truncate_chars(v.trim(), IMPRESSION_FIELD_MAX_CHARS)
            )
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    let raw_name = if imp.target_name.trim().is_empty() {
        imp.target_id.trim()
    } else {
        imp.target_name.trim()
    };
    let name = truncate_chars(raw_name, IMPRESSION_LABEL_MAX_CHARS);
    let id = truncate_chars(imp.target_id.trim(), IMPRESSION_LABEL_MAX_CHARS);
    Some(format!("- {name} ({id})\n{}", lines.join("\n")))
}

/// いま話している相手の `[Impressions]` セクションを組み立てる。
///
/// 相手が居ない / 該当する人物像が 1 件も無い場合は `Ok(None)`（セクションを出さない）。
pub fn build_impression_section(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
) -> Result<Option<String>> {
    let speakers = recent_speaker_ids(conn, agent_id, session_id)?;
    if speakers.is_empty() {
        return Ok(None);
    }

    let mut entries: Vec<String> = Vec::new();
    for speaker in speakers {
        if let Some(imp) = queries::get_impression(conn, agent_id, &speaker)? {
            if let Some(rendered) = render_impression(&imp) {
                entries.push(rendered);
            }
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }

    Ok(Some(format!(
        "[Impressions]\n(your own notes on the people you are talking with, kept per person across sessions)\n{}",
        entries.join("\n")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        opencrab_db::init_memory().expect("init")
    }

    fn insert_speech(conn: &Connection, session_id: &str, speaker_id: &str) {
        queries::insert_session_log(
            conn,
            &queries::SessionLogRow {
                id: None,
                agent_id: speaker_id.to_string(),
                session_id: session_id.to_string(),
                log_type: "speech".to_string(),
                content: "hello".to_string(),
                speaker_id: Some(speaker_id.to_string()),
                turn_number: Some(1),
                metadata_json: None,
                created_at: Some("2026-01-01T00:00:00+00:00".to_string()),
            },
        )
        .expect("insert log");
    }

    fn impression(agent_id: &str, session_id: &str, target_id: &str) -> queries::ImpressionRow {
        queries::ImpressionRow {
            id: format!("imp-{target_id}"),
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            target_id: target_id.to_string(),
            target_name: format!("name-{target_id}"),
            personality: "curious".to_string(),
            communication_style: String::new(),
            recent_behavior: String::new(),
            agreement: "中立".to_string(),
            notes: String::new(),
            last_updated_turn: 0,
        }
    }

    /// 別セッションで書いた人物像が、いま話しているセッションで載る（agent スコープ）。
    #[test]
    fn section_uses_impression_written_in_another_session() {
        let conn = setup();
        queries::upsert_impression(&conn, &impression("a1", "discord-1", "u1")).unwrap();
        insert_speech(&conn, "nostr-1", "u1");

        let section = build_impression_section(&conn, "a1", "nostr-1")
            .unwrap()
            .expect("section");
        assert!(section.starts_with("[Impressions]"));
        assert!(section.contains("name-u1"));
        assert!(section.contains("curious"));
    }

    /// 相手の人物像が無ければセクションを出さない（壊れない）。
    #[test]
    fn no_section_when_speaker_has_no_impression() {
        let conn = setup();
        insert_speech(&conn, "s1", "u1");
        assert!(build_impression_section(&conn, "a1", "s1")
            .unwrap()
            .is_none());
    }

    /// 発話者が居ないセッションではセクションを出さない。
    #[test]
    fn no_section_without_speakers() {
        let conn = setup();
        queries::upsert_impression(&conn, &impression("a1", "s1", "u1")).unwrap();
        assert!(build_impression_section(&conn, "a1", "s1")
            .unwrap()
            .is_none());
    }

    /// 話していない相手の人物像は載せない（全員分を常に載せない）。
    #[test]
    fn section_omits_people_not_in_the_conversation() {
        let conn = setup();
        queries::upsert_impression(&conn, &impression("a1", "s1", "u1")).unwrap();
        queries::upsert_impression(&conn, &impression("a1", "s1", "absent")).unwrap();
        insert_speech(&conn, "s1", "u1");

        let section = build_impression_section(&conn, "a1", "s1")
            .unwrap()
            .expect("section");
        assert!(section.contains("name-u1"));
        assert!(!section.contains("name-absent"));
    }

    /// 相手が多くても上限人数までしか載せない。
    #[test]
    fn section_caps_number_of_targets() {
        let conn = setup();
        for i in 0..(IMPRESSION_MAX_TARGETS + 2) {
            let target = format!("u{i}");
            queries::upsert_impression(&conn, &impression("a1", "s1", &target)).unwrap();
            insert_speech(&conn, "s1", &target);
        }
        let section = build_impression_section(&conn, "a1", "s1")
            .unwrap()
            .expect("section");
        assert_eq!(
            section.matches("- name-").count(),
            IMPRESSION_MAX_TARGETS,
            "at most {IMPRESSION_MAX_TARGETS} people are listed"
        );
    }

    /// 長いフィールドは既存の切り詰め機構で丸められる。
    #[test]
    fn section_truncates_long_fields() {
        let conn = setup();
        let mut imp = impression("a1", "s1", "u1");
        imp.personality = "あ".repeat(IMPRESSION_FIELD_MAX_CHARS * 3);
        queries::upsert_impression(&conn, &imp).unwrap();
        insert_speech(&conn, "s1", "u1");

        let section = build_impression_section(&conn, "a1", "s1")
            .unwrap()
            .expect("section");
        assert!(section.contains('…'));
        assert!(section.chars().count() < IMPRESSION_FIELD_MAX_CHARS * 3);
    }

    /// 見出しの `target_name` / `target_id` も描画時に丸められる
    /// （本文フィールドだけでなく、見出しにも上限が掛かっている）。
    #[test]
    fn section_truncates_long_target_name_and_id() {
        let conn = setup();
        let long_id = "i".repeat(IMPRESSION_LABEL_MAX_CHARS * 4);
        let mut imp = impression("a1", "s1", &long_id);
        imp.target_name = "な".repeat(IMPRESSION_LABEL_MAX_CHARS * 4);
        queries::upsert_impression(&conn, &imp).unwrap();
        insert_speech(&conn, "s1", &long_id);

        let section = build_impression_section(&conn, "a1", "s1")
            .unwrap()
            .expect("section");
        let head = section.lines().nth(2).expect("entry head line");
        // 見出し行は「名前 + id + 装飾」なので、上限 2 本ぶんに収まる。
        assert!(
            head.chars().count() <= IMPRESSION_LABEL_MAX_CHARS * 2 + 16,
            "head line is capped: {head}"
        );
        assert!(!head.contains(&"な".repeat(IMPRESSION_LABEL_MAX_CHARS + 1)));
        assert!(!head.contains(&"i".repeat(IMPRESSION_LABEL_MAX_CHARS + 1)));
    }

    /// 中身が全部空の人物像は行を作らない。
    #[test]
    fn section_skips_empty_impression() {
        let conn = setup();
        let mut imp = impression("a1", "s1", "u1");
        imp.personality = String::new();
        imp.agreement = String::new();
        queries::upsert_impression(&conn, &imp).unwrap();
        insert_speech(&conn, "s1", "u1");

        assert!(build_impression_section(&conn, "a1", "s1")
            .unwrap()
            .is_none());
    }
}
