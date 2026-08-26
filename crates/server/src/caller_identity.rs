//! web / Nostr の呼び出し元の権限判定（1 実装）。
//!
//! 旧 web 会話経路は Discord 設定の owner と突き合わせ、
//! 一致しなければ信頼済みユーザーの表を `(経路, 識別子, エージェント)` で引く。
//! 2 箇所に写経されていると片方だけ緩められる余地が残るので、ここに閉じる。判定本体は
//! Nostr（#319）と共有する [`resolve_caller_identity_with_owner`] の**1 実装**で、
//! [`resolve_caller_identity`] は Discord 設定の owner を取り出してそこへ委譲するだけ。
//!
//! ## 動かしてはいけない線
//! - **owner の判定に使う設定は Discord の owner のまま**（web 専用の owner を
//!   新設しない）。最小権限固定の経路に認可の判定を新設すると、そこが権限の昇格経路になる。
//! - **fail-closed**: DB を引けない・オーナー未設定は最小権限（`Agent`）へ倒れる
//!   （空のオーナー識別子は誰とも一致しない — [`opencrab_core::owner::is_owner_id`]）。
//! - **経路をまたがない**: 引くのは自経路の行だけ。#214 の互換読み（自経路の行が無ければ
//!   従来の `discord` 経路も見る）は #159 で撤去した。撤去で権限を失う行を運用者が
//!   見つけられるよう、判定が最小権限に落ちたときだけ [`warn_legacy_row_no_longer_read`]
//!   が旧経路の行の有無を**ログのためだけに**確認する（判定には一切使わない）。

use opencrab_actions::CallerIdentity;

/// `(経路, 識別子, エージェント)` から呼び出し元の権限を導出する。
///
/// **owner 識別子を Discord 設定から取り出して [`resolve_caller_identity_with_owner`] に
/// 委譲する**（Nostr と共有する 1 実装）。web は専用の owner を持たず、Discord
/// 設定の owner だけが owner 判定を決める（モジュール冒頭の「動かしてはいけない線」）。
///
/// 優先順は委譲先に従い **owner 判定が先** → 自経路の信頼済みユーザー行 → `Agent`
/// （最小権限）。owner が信頼済みユーザーとしても登録されていれば `Owner` を採る
/// （Discord の [`crate::agent_runner_impl`] の `resolve_caller` と同じ向き）。
///
/// 撤去した互換読み（#214→#159）の手掛かりは、判定が最小権限（`Agent`）に落ちた
/// ときだけ出す。`Agent` になるのは「owner でもなく自経路の行も無い」ときだけなので、
/// これは旧実装の `None` 分岐（表 miss かつ非 owner）と同じ条件。
pub fn resolve_caller_identity(
    conn: &rusqlite::Connection,
    platform: &str,
    user_id: &str,
    agent_id: &str,
) -> CallerIdentity {
    // web の owner 判定は Discord 設定の owner が決める（専用 owner を新設しない）。
    // 設定行が無い / 引けない = オーナー未設定（空文字は誰とも一致しない）。
    let owner_id = opencrab_db::queries::get_agent_discord_config(conn, agent_id)
        .ok()
        .flatten()
        .map(|c| c.owner_discord_id)
        .unwrap_or_default();
    let identity =
        resolve_caller_identity_with_owner(conn, platform, &[user_id], agent_id, &owner_id);
    if matches!(identity, CallerIdentity::Agent) {
        // ここへ落ちた＝この呼び出し元は信頼されない。互換読みの時代なら通っていた
        // かもしれないので、そのときだけ移行の手掛かりを出す（判定には使わない）。
        warn_legacy_row_no_longer_read(conn, platform, user_id, agent_id);
    }
    identity
}

/// オーナー識別子を呼び出し側が与える版（#319）。
///
/// Discord 設定の owner を見る [`resolve_caller_identity`] と違い、**その経路自身の
/// オーナー識別子**で判定する。Nostr のように識別子空間が Discord とまったく別の経路
/// 用（Nostr の pubkey が Discord の user_id と一致することはないので、Discord の owner を
/// 流用すると誰もオーナーになれない）。
///
/// 動かしてはいけない線は [`resolve_caller_identity`] と同じ:
/// - **fail-closed**: オーナー未設定（空 / 空白のみ）は誰とも一致しない
///   （[`opencrab_core::owner::is_owner_id`]）。
/// - **経路をまたがない**: 引くのは渡された `platform` の行だけ。
/// - **表の `owner` を `Owner` にしない**: この経路で `Owner` になれるのは
///   「発言者がオーナー識別子と一致した」ときだけ。表の権限から `Owner` へ上げると、
///   オーナー識別子を持たない相手が設定変更へ届く**別の昇格経路**を新設することになる。
///
/// 順序は Discord の `resolve_caller` に合わせて**オーナー判定が先**（オーナーが
/// 信頼済みユーザーとしても登録されていたら `Owner` を採る）。
///
/// `user_ids` は同じ呼び出し元を指す**表記ゆれ違いの識別子**（Nostr なら hex と npub）。
/// 先頭から順に引き、最初に見つかった行の権限を使う。先頭には正規化済みの表現を置くこと。
pub fn resolve_caller_identity_with_owner(
    conn: &rusqlite::Connection,
    platform: &str,
    user_ids: &[&str],
    agent_id: &str,
    owner_id: &str,
) -> CallerIdentity {
    opencrab_extgate::resolve_caller_identity_with_owner(
        conn, platform, user_ids, agent_id, owner_id,
    )
}

/// 発言者の生の識別子（経路依存）から、その識別子を**自分のもの**として接続した agent の
/// UUID を逆引きする（#489）。`trusted_co_agents`（agent UUID 対）と突合するための対応表。
///
/// 逆引き表は各 agent 自身の接続からしか書かれない（Discord: `bot_user_id` は
/// `get_current_user` / Nostr: `self_pubkey` は自 secret_key 由来）ため、ここで得た UUID は
/// 「その識別子の持ち主」であることが**接続で担保**されている。外部ユーザーが
/// 「識別子 ↔ UUID」を仕込む経路は存在しない。
///
/// - **Nostr**: `identifier` は 64 桁小文字 hex（呼び出し側が正規化済み）。
/// - **web**: agent の自己識別子を持たない経路なので常に `None`（fail-closed）。
/// - Discord の逆引きは同じ表（`agent_discord_config.bot_user_id`）を [`crate::agent_runner_impl`]
///   の `resolve_caller` が直接引く（Discord は本 1 実装を経由しない別経路のため）。ここでも
///   分岐を持たせて経路名で一貫させておく。
///
/// 逆引きできなければ `None`（曖昧なときに通さない）。
pub(crate) fn resolve_co_agent_uuid(
    conn: &rusqlite::Connection,
    platform: &str,
    identifier: &str,
) -> Option<String> {
    match platform {
        opencrab_db::queries::TRUSTED_PLATFORM_DISCORD => {
            opencrab_db::queries::resolve_agent_by_discord_bot_user_id(conn, identifier)
        }
        opencrab_db::queries::TRUSTED_PLATFORM_NOSTR => {
            opencrab_db::queries::resolve_agent_by_nostr_self_pubkey(conn, identifier)
        }
        // web は自己識別子の逆引き表を持たない（#489 の対象外）。
        _ => None,
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
    use opencrab_db::queries::{
        TrustedUserPermission, TRUSTED_PLATFORM_DISCORD, TRUSTED_PLATFORM_WEB,
    };
    use std::cell::RefCell;
    use std::io;
    use std::sync::{Arc, Mutex, Once};

    // ---- ログ捕捉（プロセスで 1 個の subscriber + スレッドローカルの捕捉先） ----
    //
    // **`tracing::subscriber::with_default` で捕捉してはいけない。** tracing は
    // callsite ごとの `Interest`（このイベントを組み立てるか）を**プロセス全体で
    // 1 度だけ**決めてキャッシュする。誰も subscriber を張っていないスレッドが先に
    // その callsite を踏むと `Interest::never()` が焼き付き、以後は誰が subscriber を
    // 張ってもそのイベントは組み立てられない。
    //
    // `with_default` はスレッドローカルなので、この焼き付けを止められない:
    // subscriber を作った瞬間にグローバルの最大レベルが WARN へ上がり（それまでは
    // OFF で誰も callsite へ到達しない）、**その直後から**、同じ callsite を踏む
    // 並行テスト（`legacy_discord_row_no_longer_grants_trust` /
    // `no_warning_without_a_legacy_row` 側）が捕捉側より先に
    // 登録してしまう競合が開く。実測で 200 回中 16〜19 回、捕捉バッファが空になった。
    //
    // なのでプロセス全体で subscriber を 1 個だけ張る。以後どのスレッドが先に踏んでも
    // `get_default` はその subscriber を返すので `Interest` は「出す」で焼き付く。
    // どのテストの出力を捕まえるかは**スレッドローカルの捕捉先**で切り替える。テストは
    // 1 本 1 スレッドで走るので、捕捉中でないスレッドの警告は捨てられ、干渉しない。
    //
    // **これでも窓は完全には閉じない。** `set_global_default` は内部で `Dispatch::new`
    // →（登録の副作用で）最大レベルを WARN へ上げてから、大域の受け口を差し込む。
    // その数命令の間に別スレッドが callsite を**初めて**踏むと、大域の受け口がまだ
    // 無いので従来と同じ焼き付きが起きる。窓はプロセスで 1 回きり・マイクロ秒未満だが、
    // 残すと「稀に落ちる」が残る。**張った直後に `rebuild_interest_cache()` を呼んで
    // 塞ぐ** — 窓の中で焼き付いた `Interest` も大域の受け口を基準に計算し直される。

    thread_local! {
        /// このスレッドが捕捉中なら書き込み先。捕捉していなければ `None`（捨てる）。
        static SINK: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
    }

    #[derive(Clone, Copy, Default)]
    struct ThreadLocalWriter;

    impl io::Write for ThreadLocalWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            SINK.with(|sink| {
                if let Some(sink) = sink.borrow().as_ref() {
                    sink.lock().unwrap().extend_from_slice(buf);
                }
            });
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadLocalWriter {
        type Writer = ThreadLocalWriter;
        fn make_writer(&'a self) -> Self::Writer {
            *self
        }
    }

    /// `f` の実行中に**このスレッドで**出た tracing 出力（WARN 以上）を返す。
    /// 「警告条件を満たす」ではなく「実際に warn イベントが出る」ことを見るため。
    fn captured_logs(f: impl FnOnce()) -> String {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_writer(ThreadLocalWriter)
                .with_ansi(false)
                .with_max_level(tracing::Level::WARN)
                .finish();
            // 張れなければ捕捉は成立しない（別の subscriber が先に居る = テストが
            // 意味を失う）ので、握りつぶさず落とす。
            tracing::subscriber::set_global_default(subscriber)
                .expect("捕捉用 subscriber を張れること（他に global default が居ない）");
            // 張る途中の窓（上のコメント）で焼き付いた `Interest` を計算し直す。
            tracing::callsite::rebuild_interest_cache();
        });

        /// `f` が panic しても捕捉先を残さない。
        struct Capturing;
        impl Drop for Capturing {
            fn drop(&mut self) {
                SINK.with(|sink| *sink.borrow_mut() = None);
            }
        }

        let buf: Arc<Mutex<Vec<u8>>> = Arc::default();
        SINK.with(|sink| *sink.borrow_mut() = Some(buf.clone()));
        let _capturing = Capturing;
        f();
        drop(_capturing);
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
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_WEB, "42", "agent-1"),
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
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_WEB, "42", "agent-1");
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
            TRUSTED_PLATFORM_WEB,
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
            TRUSTED_PLATFORM_WEB,
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
            TRUSTED_PLATFORM_WEB,
            "web-user",
            TrustedUserPermission::User,
        );
        register(
            &conn,
            TRUSTED_PLATFORM_WEB,
            "web-bot",
            TrustedUserPermission::CoAgent,
        );
        assert_eq!(
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_WEB, "web-user", "agent-1"),
            CallerIdentity::TrustedUser
        );
        assert_eq!(
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_WEB, "web-bot", "agent-1"),
            CallerIdentity::CoAgent {
                agent_id: "web-bot".to_string()
            }
        );
    }

    /// Discord 設定に owner を置く（web の owner 判定の出どころ）。
    fn set_discord_owner(conn: &rusqlite::Connection, agent_id: &str, owner_discord_id: &str) {
        opencrab_db::queries::upsert_agent_discord_config(
            conn,
            &opencrab_db::queries::AgentDiscordConfigRow {
                agent_id: agent_id.to_string(),
                bot_token: String::new(),
                owner_discord_id: owner_discord_id.to_string(),
                enabled: false,
            },
        )
        .unwrap();
    }

    /// owner 判定が先（委譲後の順序）。owner が信頼済みユーザーとしても登録されて
    /// いれば `Owner` を採る。旧実装（表が先）では `TrustedUser` / `CoAgent` に
    /// 止まっていた場面。owner を最小権限へ降格させないための固定。
    #[test]
    fn owner_takes_priority_over_a_trusted_row() {
        let conn = opencrab_db::init_memory().unwrap();
        set_discord_owner(&conn, "agent-1", "owner-id");
        // owner が自経路の行としても User / CoAgent で登録されている。
        register(
            &conn,
            TRUSTED_PLATFORM_WEB,
            "owner-id",
            TrustedUserPermission::User,
        );
        assert_eq!(
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_WEB, "owner-id", "agent-1"),
            CallerIdentity::Owner,
            "owner が表の行に隠れて降格した"
        );
        let conn = opencrab_db::init_memory().unwrap();
        set_discord_owner(&conn, "agent-1", "owner-id");
        register(
            &conn,
            TRUSTED_PLATFORM_WEB,
            "owner-id",
            TrustedUserPermission::CoAgent,
        );
        assert_eq!(
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_WEB, "owner-id", "agent-1"),
            CallerIdentity::Owner
        );
    }

    /// owner でない自経路の行は従来どおり効く（委譲で表照合が消えていない）。
    #[test]
    fn non_owner_trusted_row_is_unchanged_after_delegation() {
        let conn = opencrab_db::init_memory().unwrap();
        set_discord_owner(&conn, "agent-1", "owner-id");
        register(
            &conn,
            TRUSTED_PLATFORM_WEB,
            "someone-else",
            TrustedUserPermission::User,
        );
        assert_eq!(
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_WEB, "someone-else", "agent-1"),
            CallerIdentity::TrustedUser
        );
    }

    /// オーナー未設定は誰とも一致しない（#174 の fail-closed を維持）。
    #[test]
    fn unset_owner_matches_nobody() {
        let conn = opencrab_db::init_memory().unwrap();
        // Discord 設定そのものが無い（= オーナー未設定）
        assert_eq!(
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_WEB, "", "agent-1"),
            CallerIdentity::Agent
        );
        assert_eq!(
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_WEB, "anyone", "agent-1"),
            CallerIdentity::Agent
        );
    }
}
