//! `nostaro watch --json` が stdout に1行1件で吐く Nostr イベントの表現。
//!
//! nostaro（自作 CLI）の watch を「Discord webhook 専用」から「JSONL を stdout に
//! 出す汎用モード」へ改造した前提のスキーマ。契約は `docs/nostaro-interface.md`。

use serde::{Deserialize, Serialize};

/// 受信した Nostr イベント（nostaro の JSON 出力1件）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NostrEvent {
    /// イベント ID（hex, 64桁）。
    pub id: String,
    /// 投稿者の公開鍵（hex）。
    pub pubkey: String,
    /// 投稿者の npub（bech32）。返信先表示・メンションに使う。
    #[serde(default)]
    pub npub: Option<String>,
    /// note bech32（note1...）。`nostr_reply` の対象指定に使う。
    #[serde(default)]
    pub note_id: Option<String>,
    /// 投稿者の表示名（プロフィール由来・任意）。
    #[serde(default)]
    pub author_name: Option<String>,
    pub created_at: i64,
    pub kind: u32,
    #[serde(default)]
    pub content: String,
    /// タグ配列（`[["p", "..."], ["e", "..."]]` 等）。
    #[serde(default)]
    pub tags: Vec<Vec<String>>,
}

impl NostrEvent {
    /// 返信の宛先に使う識別子（note_id 優先、無ければ hex id）。
    pub fn reply_target(&self) -> &str {
        self.note_id.as_deref().unwrap_or(&self.id)
    }

    /// 表示用の著者ラベル（author_name 優先、無ければ npub、無ければ短縮 pubkey）。
    pub fn author_label(&self) -> String {
        if let Some(name) = self.author_name.as_deref().filter(|s| !s.is_empty()) {
            return name.to_string();
        }
        if let Some(npub) = self.npub.as_deref().filter(|s| !s.is_empty()) {
            return npub.to_string();
        }
        let short: String = self.pubkey.chars().take(12).collect();
        format!("{short}…")
    }
}

/// nostaro watch の stdout 1行を [`NostrEvent`] にパースする。
/// JSON でない行（ログ・空行）は `None`（呼び出し側でスキップ）。
pub fn parse_watch_line(line: &str) -> Option<NostrEvent> {
    let t = line.trim();
    if !t.starts_with('{') {
        return None;
    }
    match serde_json::from_str::<NostrEvent>(t) {
        Ok(ev) => Some(ev),
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse nostaro watch line as NostrEvent");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_full_event() {
        let line = r#"{"id":"abc123","pubkey":"deadbeef","npub":"npub1xxx","note_id":"note1yyy","author_name":"kojira","created_at":1700000000,"kind":1,"content":"hello opencrab","tags":[["p","deadbeef"]]}"#;
        let ev = parse_watch_line(line).expect("parsed");
        assert_eq!(ev.id, "abc123");
        assert_eq!(ev.kind, 1);
        assert_eq!(ev.content, "hello opencrab");
        assert_eq!(ev.reply_target(), "note1yyy");
        assert_eq!(ev.author_label(), "kojira");
    }

    #[test]
    fn test_parse_minimal_event_defaults() {
        // 任意フィールド欠落でもパースでき、フォールバックが効く。
        let line = r#"{"id":"id1","pubkey":"0011223344556677","created_at":1,"kind":1}"#;
        let ev = parse_watch_line(line).expect("parsed");
        assert_eq!(ev.content, "");
        assert!(ev.tags.is_empty());
        assert_eq!(ev.reply_target(), "id1"); // note_id 無し → hex id
        assert_eq!(ev.author_label(), "001122334455…"); // 名前も npub も無し → 短縮
    }

    #[test]
    fn test_non_json_lines_skipped() {
        assert!(parse_watch_line("").is_none());
        assert!(parse_watch_line("[watch] connected to wss://yabu.me").is_none());
        assert!(parse_watch_line("  ").is_none());
    }

    #[test]
    fn test_malformed_json_skipped_not_panicked() {
        assert!(parse_watch_line(r#"{"id": "x""#).is_none());
    }
}
