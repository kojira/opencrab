use super::*;

// ==================== fixture（Discord Message JSONL） ====================

pub(crate) struct Fixture {
    pub(crate) path: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, "").unwrap();
        Self { path, _dir: dir }
    }

    pub(crate) fn append_message(&self, id: &str, content: &str) {
        self.append_message_ch(id, CHANNEL, content);
    }

    /// 指定チャンネルへ発端メッセージを積む（#915: typing 隔離テストが専用チャンネルを使う）。
    pub(crate) fn append_message_ch(&self, id: &str, channel: &str, content: &str) {
        use std::io::Write as _;
        let line = serde_json::json!({
            "id": id,
            "channel_id": channel,
            "guild_id": GUILD,
            "author": {"id": AUTHOR, "bot": false, "username": "owner"},
            "content": content,
        })
        .to_string();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }
}
