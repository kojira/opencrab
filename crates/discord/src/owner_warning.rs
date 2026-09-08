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

use tracing::{error, info, warn};

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

/// 受信転送が連続で失敗しているときの警告（#284 P0-2）。警告したら `true`。
///
/// **沈黙して死ぬのが最悪**という前提でこの関数がある。以前は `recv()` が失敗すると
/// 転送タスクが黙って終了し、他のイベント（サブタスク完了など）は流れ続けるため
/// 「ボットは動いているのに人間の発言にだけ反応しない」状態が誰にも気づかれないまま
/// 続いた。再試行しても復旧しないことをここで表面化させる。
///
/// 配送手段が `warn!` なのは意図的で、`recv()` が壊れている局面ではゲートウェイ経由の
/// DM 送信も同時に壊れている可能性が高く、通知自体が黙って失敗しうるため。ログなら
/// 落ちない（このモジュールの他の警告と同じ扱い）。
pub fn warn_inbound_stalled(consecutive_failures: u32, secs_since_last_message: u64) -> bool {
    warn!(
        failures = consecutive_failures,
        secs_since_last_message,
        "Discord inbound receive has failed {consecutive_failures} times in a row and has not \
         delivered a message for {secs_since_last_message}s. The forwarder keeps retrying with \
         backoff, but user messages are NOT reaching agents while this lasts. Check the Discord \
         gateway connection / token."
    );
    true
}

/// ユーザー発言をセッションログに記録できなかったときの警告（#284 P0-3）。警告したら `true`。
///
/// 発言の欠落は「副作用の取りこぼし」ではない。記録されなかった発言は会話履歴に
/// 載らず、エージェントは**その指示を一度も見ないまま**応答する（#284 の症状そのもの）。
/// 本文は出さない（プライバシー）。切り分けに要るのは「どのセッションで落ちたか」。
pub fn warn_inbound_message_dropped(session_id: &str, sender_id: &str, text_len: usize) -> bool {
    warn!(
        session_id = %session_id,
        sender_id = %sender_id,
        text_len,
        "failed to persist an inbound user message after retries. The agent will answer WITHOUT \
         ever seeing this message. Check database health (disk full / locked / permissions)."
    );
    true
}

/// A2UI インタラクション受信が連続で失敗しているときの警告。警告したら `true`。
///
/// [`warn_inbound_stalled`] のインタラクション版。以前はインタラクション受信タスクは
/// `recv_interaction()` が `Err` を返した時点で `break` して黙って終了しており、以後
/// ボタン/セレクト/モーダルの応答が誰にも気づかれないまま一切届かなくなっていた
/// （受信メッセージ側 #284 と同型の非対称）。再試行しても復旧しないことをここで表面化させる。
pub fn warn_interaction_recv_stalled(consecutive_failures: u32, secs_since_last_ok: u64) -> bool {
    warn!(
        failures = consecutive_failures,
        secs_since_last_ok,
        "Discord interaction receive has failed {consecutive_failures} times in a row and has \
         not delivered an interaction for {secs_since_last_ok}s. The receiver keeps retrying with \
         backoff, but button / select / modal responses are NOT reaching agents while this lasts. \
         Check the Discord gateway connection."
    );
    true
}

/// serenity の client タスクが終了した（＝ Discord への接続が死んだ）ときの fail-loud。
/// エスカレーションを鳴らしたら `true`、意図した停止で鳴らさなかったら `false`。
///
/// **接続死のサイレント停止を防ぐのがこの関数の役目。** `DiscordGateway::start` は
/// `client.start()` を detach spawn で回すが、致命エラー（4004 invalid token /
/// 4014 disallowed intents など復旧不能）でこのタスクが終わっても、以前は ERROR ログを
/// 1 行出すだけで誰にもエスカレーションされなかった。受信転送側の stall 検知
/// （[`warn_inbound_stalled`]）は `recv()` が `Err` を返すことを発火条件にしているが、
/// その `recv()` の `tx` は `DiscordGateway` 構造体が保持しているため全 Sender drop が
/// 起きず `Err` にならない。結果「起動ログは出る → 以後メッセージが永久に来ない →
/// 警告もエスカレーションもゼロ」という、Nostr の watch 死亡サイレント停止と同型の
/// 事故になっていた。ここでタスク終了そのものを表面化させる。
///
/// `shutting_down` が真なら [`DiscordGateway::shutdown`] 由来の**意図した停止**なので
/// 鳴らさない（INFO のみ）。それ以外のタスク終了は、`Ok`（想定外の正常終了）も
/// `Err`（致命エラー）も等しく「接続が死んだ」ので ERROR + true で鳴らす。
///
/// 配送手段が `error!`（ログ）なのは [`warn_inbound_stalled`] と同じ理由で意図的:
/// client タスクが死んでいる局面ではゲートウェイ経由の DM 送信も同時に壊れているため、
/// 通知を Discord に載せると黙って失敗しうる。ログなら落ちない。深刻度は stall（WARN）
/// より上（復旧不能・恒久的な受信停止）なので ERROR にする。
pub fn warn_discord_client_task_exited(shutting_down: bool, outcome: &str) -> bool {
    if shutting_down {
        info!(
            outcome = %outcome,
            "Discord client task ended after an explicit shutdown (expected)."
        );
        return false;
    }
    error!(
        outcome = %outcome,
        "Discord client task exited WITHOUT a shutdown having been requested. The gateway will \
         NOT reconnect on its own: user messages, interactions, and timed fires stop reaching \
         this agent from now on, silently. Common causes are fatal gateway errors (4004 invalid \
         token / 4014 disallowed intents) that serenity cannot recover from. Restart the gateway \
         and check the Discord token / enabled intents."
    );
    true
}

/// テスト用: `tracing` 出力を文字列として捕まえるヘルパー。
///
/// 「警告条件を満たす」だけでなく「実際に warn イベントが出る」ことを検証するため。
/// per-agent 経路の起動前処理（`manager::prepare_owner_for_gateway`）のテストからも使う。
#[cfg(test)]
pub(crate) mod capture {
    use std::cell::RefCell;
    use std::io;
    use std::sync::{Arc, Mutex, Once};

    // ---- プロセスで 1 個の subscriber + スレッドローカルの捕捉先 ----
    //
    // **`tracing::subscriber::with_default` で捕捉してはいけない。** tracing は
    // callsite ごとの `Interest`（このイベントを組み立てるか）を**プロセス全体で
    // 1 度だけ**決めてキャッシュする。誰も subscriber を張っていないスレッドが先に
    // その callsite を踏むと `Interest::never()` が焼き付き、以後は誰が subscriber を
    // 張ってもそのイベントは組み立てられない（捕捉バッファが空になる）。
    //
    // `with_default` はスレッドローカルなのでこれを止められない: subscriber を作った
    // 瞬間にグローバルの最大レベルが WARN へ上がり（それまでは OFF で誰も callsite へ
    // 到達しない）、**その直後から**、同じ警告を subscriber 無しで呼ぶ並行テスト
    // （`shared_gateway_warning_fires_exactly_when_it_starts_without_owner` など）が
    // 捕捉側より先に登録してしまう競合が開く。
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
    pub(crate) fn captured_logs(f: impl FnOnce()) -> String {
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

    /// #284 P0-2: 受信が止まっていることが**沈黙ではなくログ**として出ること。
    /// 以前は転送タスクが黙って死に、「ボットは生きているのに人間の発言だけ届かない」
    /// 状態が誰にも見えなかった。
    #[test]
    fn inbound_stalled_warning_is_emitted() {
        let logs = captured_logs(|| {
            assert!(warn_inbound_stalled(5, 42));
        });
        assert!(logs.contains("WARN"), "warn レベルで出ること: {logs}");
        assert!(
            logs.contains("inbound receive has failed"),
            "本文が出ること: {logs}"
        );
        // 「何回失敗したか」「何秒受信が無いか」が無いと切り分けできない。
        assert!(logs.contains('5') && logs.contains("42"), "計測値: {logs}");
    }

    /// #284 P0-3: ユーザー発言を記録できなかったことが表面化すること。
    /// 本文（プライバシー）は出さず、どのセッションかだけ出す。
    #[test]
    fn dropped_inbound_message_warning_is_emitted() {
        let logs = captured_logs(|| {
            assert!(warn_inbound_message_dropped(
                "discord-crab-111-222",
                "user-1",
                12
            ));
        });
        assert!(logs.contains("WARN"), "warn レベルで出ること: {logs}");
        assert!(
            logs.contains("failed to persist an inbound user message"),
            "本文が出ること: {logs}"
        );
        assert!(logs.contains("discord-crab-111-222"), "session_id: {logs}");
    }

    /// #337: インタラクション受信が止まっていることも沈黙ではなくログとして出ること。
    #[test]
    fn interaction_stalled_warning_is_emitted() {
        let logs = captured_logs(|| {
            assert!(warn_interaction_recv_stalled(5, 42));
        });
        assert!(logs.contains("WARN"), "warn レベルで出ること: {logs}");
        assert!(
            logs.contains("interaction receive has failed"),
            "本文が出ること: {logs}"
        );
        assert!(logs.contains('5') && logs.contains("42"), "計測値: {logs}");
    }

    /// 接続死のサイレント停止を潰す本丸: client タスクが**意図しない**終了をしたら、
    /// ERROR で鳴り（`true`）、何が起きたか（恒久停止・要因・対処）が読み取れること。
    #[test]
    fn client_task_exit_without_shutdown_escalates() {
        let logs = captured_logs(|| {
            assert!(
                warn_discord_client_task_exited(false, "error: Gateway closed: 4004"),
                "shutdown 要求無しのタスク終了はエスカレーションするはず"
            );
        });
        assert!(logs.contains("ERROR"), "error レベルで出ること: {logs}");
        assert!(
            logs.contains("Discord client task exited"),
            "本文が出ること: {logs}"
        );
        assert!(
            logs.contains("will NOT reconnect"),
            "恒久停止だと分かること: {logs}"
        );
        // 要因（outcome）が切り分けのために残ること。
        assert!(logs.contains("4004"), "outcome が残ること: {logs}");
    }

    /// `Err` だけでなく**想定外の正常終了（`Ok`）**も接続死として同じく鳴らす
    /// （どちらも「以後メッセージが来ない」ことに変わりはない）。
    #[test]
    fn client_task_ok_exit_without_shutdown_also_escalates() {
        let logs = captured_logs(|| {
            assert!(warn_discord_client_task_exited(false, "ok"));
        });
        assert!(logs.contains("ERROR"), "error レベルで出ること: {logs}");
        assert!(logs.contains("Discord client task exited"), "本文: {logs}");
    }

    /// 意図した shutdown 由来の終了では**鳴らさない**（`false`）。誤エスカレーション防止。
    /// INFO は WARN 未満なので captured_logs（WARN 以上）には出ない＝ノイズにならない。
    #[test]
    fn client_task_exit_after_shutdown_is_silent() {
        let logs = captured_logs(|| {
            assert!(
                !warn_discord_client_task_exited(true, "ok"),
                "意図した停止で鳴らしてはいけない（誤エスカレーション）"
            );
        });
        assert!(
            !logs.contains("ERROR"),
            "意図した停止で ERROR を出してはいけない: {logs}"
        );
        assert!(
            !logs.contains("Discord client task exited"),
            "エスカレーション本文を出してはいけない: {logs}"
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
