//! owner 未設定（`owner_discord_id` が空）を起動時に知らせる警告。
//!
//! Discord ゲートウェイの起動経路は 2 つある。
//!
//! - 共有（TOML）ゲートウェイ: `config/default.toml` の `[gateway.discord]`
//! - per-agent ゲートウェイ: `PUT /api/agents/{id}/discord`（ダッシュボード）で
//!   保存した設定から起動、およびサーバー再起動時の DB からの復元
//!
//! 配布テンプレートは共有ゲートウェイを無効にしているため、新規オンボーディングは
//! per-agent 経路を通る。両経路で同じ本文の警告を出せるよう、判定と文面をここに集約する。

use tracing::warn;

/// owner 未設定が招く結果（両経路で同じ内容を出す）。
///
/// 未設定時のフォールバックは拒否側に統一した（#174）。以前のように「黙って権限が
/// 緩む」のではなく「**黙って機能しない**」ので、何が動かなくなるかを具体的に書く。
/// 運用者が「なぜ DM に応答しないのか分からない」状態に落ちるのを防ぐのがこの警告の
/// 役目なので、ここの文面と実際のフォールバック挙動はセットで変えること。
///
/// (3) を「オーナー専用 UI」と書かないのは、操作者チェックが**すべての A2UI 描画面**に
/// 効くため（`owner_only` 引数は DB の列にしか効かず、ゲートは見ていない）。狭く書くと
/// 「オーナー専用じゃない UI は生きているはず」と誤読され、原因の切り分けを外す。
const CONSEQUENCES: &str = "Consequences (an unset owner now fails closed): (1) owner-only \
     features are unavailable because no one is recognized as owner; (2) for agents with no \
     trusted Discord users registered, DMs are REJECTED from everyone, so the agent will not \
     reply to any DM; (3) EVERY interactive UI the agent sends (forms/modals/buttons) cannot be \
     operated by anyone -- the operator check runs on every A2UI surface, not just the ones \
     marked owner-only.";

/// Discord ゲートウェイが**実際に起動する**条件（`enabled` かつトークンがある）。
///
/// 起動判定と下の警告条件を別々に書くと、両者がずれても誰も気づけない。実際に
/// ずれていた例: 警告側は `token.trim().is_empty()`、共有ゲートウェイの起動側は
/// 生の `is_empty()` だったため、`DISCORD_TOKEN=" "` では「ゲートウェイは起動する
/// のに owner 未設定の警告が出ない」取りこぼしになっていた。空白だけのトークンでは
/// Discord に接続できないので、起動しない側に揃える。
///
/// 起動判定を行う全経路（共有ゲートウェイの起動、per-agent 設定更新後の再起動）と
/// 警告条件がこの述語を参照することで、条件を 1 箇所に閉じる。
pub fn gateway_will_start(enabled: bool, token: &str) -> bool {
    enabled && !token.trim().is_empty()
}

/// 共有（TOML）ゲートウェイの owner 未設定を警告する。警告したら `true`。
///
/// ゲートウェイが**実際に起動する条件**（[`gateway_will_start`]）でだけ警告する。
/// 追跡ファイル `config/default.toml` は `enabled = true` なので、`DISCORD_TOKEN` を
/// 持たない開発者にまで出すと「常に出ている警告」になり信用されなくなる。
pub fn warn_if_shared_gateway_owner_unset(
    enabled: bool,
    token: &str,
    owner_discord_id: &str,
) -> bool {
    if !gateway_will_start(enabled, token) || !owner_discord_id.trim().is_empty() {
        return false;
    }
    warn!(
        "gateway.discord.owner_discord_id is empty (check OWNER_DISCORD_ID in .env). \
         {CONSEQUENCES} Set OWNER_DISCORD_ID in production."
    );
    true
}

/// per-agent ゲートウェイの owner 未設定を警告する。警告したら `true`。
///
/// per-agent 設定の owner 欄は任意（未入力なら空文字で保存される）ため、共有
/// ゲートウェイ側の警告条件ではこの経路を一度も拾えない。どのエージェントの設定を
/// 直せばよいか分かるよう `agent_id` を添える。
pub fn warn_if_agent_gateway_owner_unset(agent_id: &str, owner_discord_id: &str) -> bool {
    if !owner_discord_id.trim().is_empty() {
        return false;
    }
    warn!(
        agent_id = %agent_id,
        "per-agent Discord gateway is starting with an empty owner_discord_id. \
         {CONSEQUENCES} Set the owner for this agent from the dashboard \
         (or PUT /api/agents/{{id}}/discord)."
    );
    true
}

/// テスト用: `tracing` 出力を文字列として捕まえるヘルパー。
///
/// 「警告条件を満たす」だけでなく「実際に warn イベントが出る」ことを検証するため。
/// per-agent 経路の起動前処理（`manager::prepare_owner_for_gateway`）のテストからも使う。
#[cfg(test)]
pub(crate) mod capture {
    use std::io;
    use std::sync::{Arc, Mutex};

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

    /// `f` の実行中に出た tracing 出力（WARN 以上）を返す。
    pub(crate) fn captured_logs(f: impl FnOnce()) -> String {
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
}

#[cfg(test)]
mod tests {
    use super::capture::captured_logs;
    use super::*;

    #[test]
    fn blank_token_does_not_start_the_shared_gateway() {
        // 空白だけのトークンでは Discord に接続できないので「起動しない」に倒す。
        // 生の `is_empty()` に戻すと `DISCORD_TOKEN=" "` で起動判定だけが真になり、
        // owner 未設定の警告を取りこぼす（このテストが落ちる）。
        assert!(gateway_will_start(true, "bot-token"));
        assert!(!gateway_will_start(true, ""));
        assert!(!gateway_will_start(true, " "));
        assert!(!gateway_will_start(true, " \t\n"));
        // 無効なら起動しない。
        assert!(!gateway_will_start(false, "bot-token"));
    }

    #[test]
    fn shared_gateway_warning_fires_exactly_when_it_starts_without_owner() {
        // 「起動する条件」と「警告する条件」がずれていないことを網羅で固定する。
        for enabled in [true, false] {
            for token in ["bot-token", "", " ", " \t\n"] {
                for owner in ["", " ", "\n", "123456789012345678", " 1234 "] {
                    let expected = gateway_will_start(enabled, token) && owner.trim().is_empty();
                    assert_eq!(
                        warn_if_shared_gateway_owner_unset(enabled, token, owner),
                        expected,
                        "enabled={enabled} token={token:?} owner={owner:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn shared_gateway_warns_only_when_it_actually_starts() {
        // enabled + トークンあり + owner 空 → 警告
        assert!(warn_if_shared_gateway_owner_unset(true, "bot-token", ""));
        assert!(warn_if_shared_gateway_owner_unset(true, "bot-token", "   "));
        // トークン無し（＝共有ゲートウェイは起動しない）→ 警告しない
        assert!(!warn_if_shared_gateway_owner_unset(true, "", ""));
        assert!(!warn_if_shared_gateway_owner_unset(true, "  ", ""));
        // 無効 → 警告しない
        assert!(!warn_if_shared_gateway_owner_unset(false, "bot-token", ""));
        // owner 設定済み → 警告しない
        assert!(!warn_if_shared_gateway_owner_unset(
            true,
            "bot-token",
            "123456789012345678"
        ));
    }

    #[test]
    fn agent_gateway_warns_when_owner_is_blank() {
        assert!(warn_if_agent_gateway_owner_unset("crab", ""));
        assert!(warn_if_agent_gateway_owner_unset("crab", " \n"));
        assert!(!warn_if_agent_gateway_owner_unset(
            "crab",
            "123456789012345678"
        ));
    }

    #[test]
    fn agent_gateway_warning_is_emitted_with_agent_id() {
        let logs = captured_logs(|| {
            assert!(warn_if_agent_gateway_owner_unset("agent-under-test", ""));
        });
        assert!(logs.contains("WARN"), "warn レベルで出ること: {logs}");
        assert!(
            logs.contains("empty owner_discord_id"),
            "本文が出ること: {logs}"
        );
        assert!(
            logs.contains("agent-under-test"),
            "どのエージェントか分かること: {logs}"
        );
    }

    #[test]
    fn shared_gateway_warning_is_emitted() {
        let logs = captured_logs(|| {
            assert!(warn_if_shared_gateway_owner_unset(true, "bot-token", ""));
        });
        assert!(logs.contains("WARN"), "warn レベルで出ること: {logs}");
        assert!(
            logs.contains("gateway.discord.owner_discord_id is empty"),
            "本文が出ること: {logs}"
        );
    }

    /// #174: 警告本文が「拒否側に倒れる」ことを伝えていること。
    ///
    /// フォールバックを拒否側に統一した以上、症状は「権限が緩む」ではなく
    /// 「DM に応答しない・UI が動かない」。文面が旧挙動（全許可）のままだと、
    /// 運用者は警告を読んでも原因にたどり着けない。両経路とも同じ本文を出す。
    #[test]
    fn warning_explains_that_unset_owner_fails_closed() {
        for logs in [
            captured_logs(|| {
                warn_if_shared_gateway_owner_unset(true, "bot-token", "");
            }),
            captured_logs(|| {
                warn_if_agent_gateway_owner_unset("crab", "");
            }),
        ] {
            assert!(
                logs.contains("fails closed"),
                "拒否側に倒れると書いてあること: {logs}"
            );
            assert!(
                logs.contains("DMs are REJECTED from everyone"),
                "DM に応答しなくなると書いてあること: {logs}"
            );
            assert!(
                logs.contains("EVERY interactive UI"),
                "UI の影響範囲が全描画面だと書いてあること: {logs}"
            );
            assert!(
                logs.contains("cannot be operated by anyone"),
                "UI を誰も操作できないと書いてあること: {logs}"
            );
            assert!(
                logs.contains("not just the ones marked owner-only"),
                "オーナー専用に限らないと書いてあること（狭く読むと原因の切り分けを外す）: {logs}"
            );
        }
    }

    #[test]
    fn no_warning_is_emitted_when_owner_is_set() {
        let logs = captured_logs(|| {
            warn_if_shared_gateway_owner_unset(true, "bot-token", "123456789012345678");
            warn_if_agent_gateway_owner_unset("crab", "123456789012345678");
        });
        assert!(logs.trim().is_empty(), "余計な警告を出さないこと: {logs}");
    }
}
