//! web / REST の呼び出し元の権限判定（1 実装）。
//!
//! web ゲートウェイ（[`crate::web_runner_impl`]）と REST
//! （[`crate::api::agents_messages`]）は、申告された経路（`platform`）が違うだけで
//! まったく同じ手順で呼び出し元を導出する: 信頼済みユーザーの表を
//! `(経路, 識別子, エージェント)` で引き、無ければ Discord 設定の owner と突き合わせる。
//! 2 箇所に写経されていると片方だけ緩められる余地が残るので、ここに閉じる。
//!
//! ## 動かしてはいけない線
//! - **owner の判定に使う設定は Discord の owner のまま**（web / REST 専用の owner を
//!   新設しない）。最小権限固定の経路に認可の判定を新設すると、そこが権限の昇格経路になる。
//! - **fail-closed**: DB を引けない・オーナー未設定は最小権限（`Agent`）へ倒れる
//!   （空のオーナー識別子は誰とも一致しない — [`opencrab_core::owner::is_owner_id`]）。
//! - **経路をまたがない**: 引くのは自経路の行だけ。#214 の互換読み（自経路の行が無ければ
//!   従来の `discord` 経路も見る）は #159 で撤去した。撤去で権限を失う行を運用者が
//!   見つけられるよう、判定が最小権限に落ちたときだけ [`warn_legacy_row_no_longer_read`]
//!   が旧経路の行の有無を**ログのためだけに**確認する（判定には一切使わない）。

use opencrab_actions::CallerIdentity;
use opencrab_db::queries::TrustedUserPermission;

/// `(経路, 識別子, エージェント)` から呼び出し元の権限を導出する。
///
/// 優先順: 自経路の信頼済みユーザー行 → Discord 設定の owner → `Agent`（最小権限）。
pub fn resolve_caller_identity(
    conn: &rusqlite::Connection,
    platform: &str,
    user_id: &str,
    agent_id: &str,
) -> CallerIdentity {
    match opencrab_db::queries::get_trusted_user(conn, platform, user_id, agent_id)
        .map(|u| u.permission)
    {
        Some(TrustedUserPermission::CoAgent) => CallerIdentity::CoAgent {
            agent_id: user_id.to_string(),
        },
        // **表の `owner` はここでは Owner にしない**（従来どおり）。web / REST の
        // owner 判定は Discord 設定の owner だけが決める（上の「動かしてはいけない線」）。
        // 列挙型にしたので、この意図的な差は網羅的な match として明示できる。
        Some(TrustedUserPermission::Owner) | Some(TrustedUserPermission::User) => {
            CallerIdentity::TrustedUser
        }
        None => {
            let cfg = opencrab_db::queries::get_agent_discord_config(conn, agent_id);
            let is_owner = matches!(cfg, Ok(Some(ref c)) if crate::api::is_owner_id(&c.owner_discord_id, user_id));
            if is_owner {
                CallerIdentity::Owner
            } else {
                // ここへ落ちた＝この呼び出し元は信頼されない。互換読みの時代なら
                // 通っていたかもしれないので、そのときだけ移行の手掛かりを出す。
                warn_legacy_row_no_longer_read(conn, platform, user_id, agent_id);
                CallerIdentity::Agent
            }
        }
    }
}

/// 撤去した互換読み（#214→#159）に依存していた行を運用者へ知らせる。警告したら `true`。
///
/// 「自経路の行は無いが、同じ識別子の従来経路（`discord`）の行はある」= 互換読みが
/// 生きていた頃は信頼されていた呼び出し元。**この結果は判定に使わない**（返り値は
/// テスト用で、呼び出し元は捨てる）。ここを判定へ配線し直すと撤去した互換読みの復活
/// になるので、戻り値を権限へつなげないこと。
///
/// 実際に権限を失った呼び出しでだけ出るので、無関係な環境では 1 行も出ない。
/// 逆に出続ける場合は移行が終わっていないということなので、抑制はしない。
pub fn warn_legacy_row_no_longer_read(
    conn: &rusqlite::Connection,
    platform: &str,
    user_id: &str,
    agent_id: &str,
) -> bool {
    if platform == opencrab_db::queries::TRUSTED_PLATFORM_DISCORD {
        return false;
    }
    if opencrab_db::queries::get_trusted_user(
        conn,
        opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
        user_id,
        agent_id,
    )
    .is_none()
    {
        return false;
    }
    tracing::warn!(
        agent_id = %agent_id,
        user_id = %user_id,
        platform = %platform,
        "trusted user row exists only on the legacy 'discord' platform, so this caller is NO LONGER \
         trusted on '{platform}' (the cross-platform compatibility read was removed in #159). \
         Re-register this user for its own platform: DELETE the legacy row, then POST \
         /api/agents/{{id}}/trusted-users with {{\"platform\": \"{platform}\", \"user_id\": \"{user_id}\"}} \
         (delete first: the unique constraint is still (user_id, agent_id)). \
         Until then this caller runs with the least privilege.",
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_db::queries::{TRUSTED_PLATFORM_DISCORD, TRUSTED_PLATFORM_REST};
    use std::io;
    use std::sync::{Arc, Mutex};

    /// `f` の実行中に出た tracing 出力（WARN 以上）を返す。
    /// 「警告条件を満たす」ではなく「実際に warn イベントが出る」ことを見るため。
    fn captured_logs(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct CaptureWriter(Arc<Mutex<Vec<u8>>>);
        impl io::Write for CaptureWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
            type Writer = CaptureWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        let buf: Arc<Mutex<Vec<u8>>> = Arc::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(CaptureWriter(buf.clone()))
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = buf.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    fn register(
        conn: &rusqlite::Connection,
        platform: &str,
        user_id: &str,
        permission: TrustedUserPermission,
    ) {
        opencrab_db::queries::add_trusted_user(
            conn,
            platform,
            &format!("row-{platform}-{user_id}"),
            "agent-1",
            user_id,
            permission,
            "owner",
            "2026-01-01",
            "",
        )
        .unwrap();
    }

    /// 従来経路の行しか無いユーザーは、別経路では最小権限に落ちる（撤去で変わった点）。
    #[test]
    fn legacy_discord_row_no_longer_grants_trust() {
        let conn = opencrab_db::init_memory().unwrap();
        register(
            &conn,
            TRUSTED_PLATFORM_DISCORD,
            "42",
            TrustedUserPermission::User,
        );
        assert_eq!(
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_REST, "42", "agent-1"),
            CallerIdentity::Agent
        );
    }

    /// その落ち方が運用者に見えること（警告が出る条件と本文）。
    #[test]
    fn removal_is_visible_to_the_operator() {
        let conn = opencrab_db::init_memory().unwrap();
        register(
            &conn,
            TRUSTED_PLATFORM_DISCORD,
            "42",
            TrustedUserPermission::User,
        );
        let logs = captured_logs(|| {
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_REST, "42", "agent-1");
        });
        assert!(logs.contains("WARN"), "warn レベルで出ること: {logs}");
        assert!(
            logs.contains("NO LONGER"),
            "信頼されなくなったと書いてあること: {logs}"
        );
        assert!(
            logs.contains("trusted-users"),
            "直し方（登録し直し）が書いてあること: {logs}"
        );
        assert!(
            logs.contains("42") && logs.contains("agent-1"),
            "誰が権限を失ったか分かること: {logs}"
        );
    }

    /// 旧行が無ければ黙る（未登録の一般ユーザーで警告が溢れない）。
    #[test]
    fn no_warning_without_a_legacy_row() {
        let conn = opencrab_db::init_memory().unwrap();
        assert!(!warn_legacy_row_no_longer_read(
            &conn,
            TRUSTED_PLATFORM_REST,
            "999",
            "agent-1"
        ));
        // 従来経路そのものは移行の対象外（自経路の行が無いだけ）。
        register(
            &conn,
            TRUSTED_PLATFORM_DISCORD,
            "42",
            TrustedUserPermission::User,
        );
        assert!(!warn_legacy_row_no_longer_read(
            &conn,
            TRUSTED_PLATFORM_DISCORD,
            "42",
            "agent-1"
        ));
        assert!(warn_legacy_row_no_longer_read(
            &conn,
            TRUSTED_PLATFORM_REST,
            "42",
            "agent-1"
        ));
    }

    /// 自経路の行は今までどおり効く（permission の写し方も含めて）。
    #[test]
    fn own_platform_rows_decide_the_identity() {
        let conn = opencrab_db::init_memory().unwrap();
        register(
            &conn,
            TRUSTED_PLATFORM_REST,
            "rest-user",
            TrustedUserPermission::User,
        );
        register(
            &conn,
            TRUSTED_PLATFORM_REST,
            "rest-bot",
            TrustedUserPermission::CoAgent,
        );
        assert_eq!(
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_REST, "rest-user", "agent-1"),
            CallerIdentity::TrustedUser
        );
        assert_eq!(
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_REST, "rest-bot", "agent-1"),
            CallerIdentity::CoAgent {
                agent_id: "rest-bot".to_string()
            }
        );
    }

    /// オーナー未設定は誰とも一致しない（#174 の fail-closed を維持）。
    #[test]
    fn unset_owner_matches_nobody() {
        let conn = opencrab_db::init_memory().unwrap();
        // Discord 設定そのものが無い（= オーナー未設定）
        assert_eq!(
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_REST, "", "agent-1"),
            CallerIdentity::Agent
        );
        assert_eq!(
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_REST, "anyone", "agent-1"),
            CallerIdentity::Agent
        );
    }
}
