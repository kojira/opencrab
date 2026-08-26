pub mod agents;
pub mod allowed_commands;
pub mod analytics;
pub mod channel_configs;
pub mod co_agents;
pub mod daily_log_index;
pub mod hooks;
pub mod import;
pub mod import_sync;
pub mod llm;
pub mod llm_logs;
pub mod mcp;
pub mod memory;
pub mod model_pricing;
#[cfg(feature = "nostr")]
pub mod nostr;
#[cfg(feature = "nostr")]
pub mod nostr_relay;
pub mod providers;
pub mod schedules;
#[cfg(feature = "nostr")]
pub mod session_watches;
pub mod sessions;
pub mod setup;
pub mod skills;
pub mod sleep;
pub mod system;
pub mod tool_logs;
pub mod trusted_users;
pub mod web_conversations;
pub mod workspace;

/// `owner_discord_id` と呼び出し元 ID が一致するか判定する。
///
/// 実体は [`opencrab_core::owner::is_owner_id`]。判定を下位クレートの 1 実装へ
/// 集約し、server / discord の両方から同じ述語を使う（#174）。以前はここに実装が
/// あり、依存方向の都合で `crates/discord` からは使えず生比較が別実装として
/// 残っていた。この別名は既存の呼び出し元との互換のために残している。
///
/// 未設定（空文字・空白のみ）のオーナー ID は誰とも一致しない。
pub use opencrab_core::owner::is_owner_id;

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
