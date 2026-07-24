pub mod agents;
pub mod agents_messages;
pub mod allowed_commands;
pub mod analytics;
pub mod channel_configs;
pub mod co_agents;
pub mod daily_log_index;
pub mod import;
pub mod import_sync;
pub mod llm;
pub mod llm_logs;
pub mod mcp;
pub mod memory;
pub mod nostr;
pub mod providers;
pub mod sessions;
pub mod setup;
pub mod skills;
pub mod sleep;
pub mod system;
pub mod trusted_users;
pub mod workspace;

/// `owner_discord_id` と呼び出し元 ID が一致するか判定する。
///
/// `owner_discord_id` は設定で省略できる（= オーナー未設定）。設定値は
/// `${OWNER_DISCORD_ID}` のような環境変数参照で与えられ、未定義なら空文字に
/// 展開されるため、素朴な `==` だと空の呼び出し元 ID が owner と一致してしまう。
/// 空のオーナー ID は「オーナー無し」として誰とも一致させない（安全側）。
pub fn is_owner_id(owner_discord_id: &str, user_id: &str) -> bool {
    !owner_discord_id.is_empty() && owner_discord_id == user_id
}

#[cfg(test)]
mod tests {
    use super::is_owner_id;

    #[test]
    fn owner_id_matches_when_configured() {
        assert!(is_owner_id("123456789012345678", "123456789012345678"));
        assert!(!is_owner_id("123456789012345678", "987654321098765432"));
    }

    #[test]
    fn unset_owner_id_matches_nobody() {
        // オーナー未設定（環境変数未定義で空文字に展開されたケース）では、
        // 空の呼び出し元 ID を含め誰も owner と判定されない。
        assert!(!is_owner_id("", ""));
        assert!(!is_owner_id("", "123456789012345678"));
        assert!(!is_owner_id("", "web-user"));
    }
}
