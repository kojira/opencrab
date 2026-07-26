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
pub mod web;
pub mod workspace;

/// `owner_discord_id` と呼び出し元 ID が一致するか判定する。
///
/// owner は「未設定」を取り得る。per-agent Discord 設定の DB 既定値が空文字
/// （`owner_discord_id TEXT NOT NULL DEFAULT ''`）であり、UI/API から owner を
/// 指定せずに作成された行が存在しうるため、素朴な `==` だと空の呼び出し元 ID が
/// owner と一致してしまう。TOML 側も `${OWNER_DISCORD_ID}` 参照が未定義のとき
/// 空文字に展開されるので同じことが起きる。
///
/// 空のオーナー ID は「オーナー無し」として誰とも一致させない（安全側）。
/// 空白のみの値も未設定として扱い、比較前に両辺を trim する。
pub fn is_owner_id(owner_discord_id: &str, user_id: &str) -> bool {
    let owner = owner_discord_id.trim();
    !owner.is_empty() && owner == user_id.trim()
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
        // オーナー未設定（DB 既定値の空文字 / 環境変数未定義で空文字に展開された
        // ケース）では、空の呼び出し元 ID を含め誰も owner と判定されない。
        assert!(!is_owner_id("", ""));
        assert!(!is_owner_id("", "123456789012345678"));
        assert!(!is_owner_id("", "web-user"));
    }

    #[test]
    fn owner_id_whitespace_only_is_treated_as_unset() {
        // 空白のみの owner 設定は未設定と同じ扱い。空白を送るだけで owner に
        // ならないこと。
        assert!(!is_owner_id(" ", " "));
        assert!(!is_owner_id("\t", "\t"));
        assert!(!is_owner_id("  ", ""));
        assert!(!is_owner_id("", " "));
        assert!(!is_owner_id(" \n ", "123456789012345678"));
    }

    #[test]
    fn owner_id_ignores_surrounding_whitespace() {
        // `.env` の値末尾に空白が混ざっても owner 判定は成立する。
        assert!(is_owner_id(" 123456789012345678 ", "123456789012345678"));
        assert!(is_owner_id("123456789012345678", " 123456789012345678\n"));
        assert!(!is_owner_id(" 123456789012345678 ", "987654321098765432"));
    }
}
