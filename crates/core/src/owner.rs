//! オーナー識別子の判定（全ゲートウェイ共通の 1 実装）。
//!
//! オーナーは「未設定」を取り得る。per-agent Discord 設定の DB 既定値が空文字
//! （`owner_discord_id TEXT NOT NULL DEFAULT ''`）であり、UI/API からオーナーを
//! 指定せずに作成された行が存在しうるため、素朴な `==` だと空の呼び出し元 ID が
//! オーナーと一致してしまう。TOML 側も `${OWNER_DISCORD_ID}` 参照が未定義のとき
//! 空文字に展開されるので同じことが起きる。
//!
//! **未設定は「オーナー無し」＝誰もオーナーではない**（fail-closed, #174）。
//! 「未設定なら制限しない」という緩め方はどの経路にも置かない。
//!
//! ここが下位クレート（`opencrab-core`）にあるのは、`crates/server` と
//! `crates/discord` の両方から同じ述語を使うため。以前は判定が
//! `crates/server/src/api/` にあり、依存方向の都合で discord 側からは使えず、
//! 生比較が別実装として残っていた（#174 のレビューで判明）。

/// オーナーが未設定か（空文字・空白のみは未設定）。
pub fn owner_is_unset(owner_id: &str) -> bool {
    owner_id.trim().is_empty()
}

/// `owner_id` と呼び出し元 ID が一致するか。
///
/// 未設定のオーナー ID は誰とも一致しない。空白のみの値も未設定として扱い、
/// 比較前に両辺を trim する。
pub fn is_owner_id(owner_id: &str, user_id: &str) -> bool {
    let owner = owner_id.trim();
    !owner.is_empty() && owner == user_id.trim()
}

#[cfg(test)]
mod tests {
    use super::{is_owner_id, owner_is_unset};

    #[test]
    fn matches_when_configured() {
        assert!(is_owner_id("123456789012345678", "123456789012345678"));
        assert!(!is_owner_id("123456789012345678", "987654321098765432"));
    }

    #[test]
    fn unset_owner_matches_nobody() {
        assert!(owner_is_unset(""));
        assert!(!is_owner_id("", ""));
        assert!(!is_owner_id("", "123456789012345678"));
        assert!(!is_owner_id("123456789012345678", ""));
    }

    #[test]
    fn whitespace_only_owner_is_unset() {
        assert!(owner_is_unset("  "));
        assert!(owner_is_unset(" \n\t"));
        assert!(!is_owner_id(" ", " "));
        assert!(!is_owner_id("\t", "\t"));
        assert!(!is_owner_id(" \n ", "123456789012345678"));
    }

    #[test]
    fn surrounding_whitespace_is_ignored_on_both_sides() {
        assert!(is_owner_id(" 123456789012345678 ", "123456789012345678"));
        assert!(is_owner_id("123456789012345678", " 123456789012345678\n"));
        assert!(!is_owner_id(" 123456789012345678 ", "987654321098765432"));
    }

    #[test]
    fn configured_owner_is_not_unset() {
        assert!(!owner_is_unset("123456789012345678"));
        assert!(!owner_is_unset(" 123456789012345678 "));
    }
}
