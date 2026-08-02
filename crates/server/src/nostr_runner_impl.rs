//! NostrAgentRunner trait implementation for AppState.
//!
//! nostr ゲートウェイ（crates/nostr）の最小 runner を、既存の process /
//! transcript ヘルパへ委譲して実装する（discord の AgentRunner impl と同型）。
//!
//! ゲートウェイ非依存なメソッドは `agent_runtime_impl.rs` の
//! [`opencrab_actions::AgentRuntime`] 実装が持つ（#156 S1）。転記（受信イベント /
//! エージェント返信）も同様にそちらへ移した（#158 S3）。

use crate::AppState;

/// Nostr 受信ターンの呼び出し元を解決する（#319 / DB 接続だけに依存する本体）。
///
/// トレイト実装からロックを剥がしただけの純粋な関数にしてある（`AppState` を組まずに
/// 実 DB で検証できるようにするため）。
///
/// 判定そのものは web / REST と同じ 1 実装
/// （[`crate::caller_identity::resolve_caller_identity_with_owner`]）に委譲する。
/// ここが持つのは **Nostr 固有の 2 点**だけ:
///
/// 1. **オーナー識別子の出どころ** — `agent_nostr_config.owner_pubkey`
///    （Discord の `agent_discord_config.owner_discord_id` に相当）。未設定なら
///    誰もオーナーにならない（fail-closed）。
/// 2. **表現の正規化** — 同じ鍵が npub と hex の 2 通りで現れる。比較の前に両辺を
///    hex へ寄せる。正規化できない発言者識別子は最小権限へ倒す（壊れた値をそのまま
///    突き合わせて偶然一致させない）。
///
/// 信頼済みユーザーの照合は `platform = 'nostr'` の行だけを見る（Discord の識別子
/// 空間と混ざらない）。行が npub で登録されていても引けるよう、hex と npub の両方の
/// 表現で引く。
pub(crate) fn resolve_nostr_caller_identity(
    conn: &rusqlite::Connection,
    agent_id: &str,
    author_pubkey: &str,
) -> opencrab_actions::CallerIdentity {
    // 正規化できない発言者は最小権限（壊れた値で照合しない）。
    let Some(hex) = opencrab_nostr::normalize_pubkey(author_pubkey) else {
        tracing::debug!(
            agent_id,
            "nostr: 発言者の pubkey を正規化できない。最小権限で扱う"
        );
        return opencrab_actions::CallerIdentity::Agent;
    };
    let npub = opencrab_nostr::to_npub(&hex);
    // 保存側も正規化済みだが、手で書き換えられた行を取りこぼさないよう読み出しでも通す。
    let owner = opencrab_db::queries::get_agent_nostr_owner_pubkey(conn, agent_id)
        .ok()
        .and_then(|v| opencrab_nostr::normalize_pubkey(&v))
        .unwrap_or_default();
    let mut ids: Vec<&str> = vec![hex.as_str()];
    if let Some(n) = npub.as_deref() {
        ids.push(n);
    }
    crate::caller_identity::resolve_caller_identity_with_owner(
        conn,
        opencrab_db::queries::TRUSTED_PLATFORM_NOSTR,
        &ids,
        agent_id,
        &owner,
    )
}

impl opencrab_nostr::NostrAgentRunner for AppState {
    /// 受信イベントの発言者から呼び出し元の権限を決める（#319）。
    ///
    /// 判定そのものは web / REST と同じ 1 実装
    /// （[`crate::caller_identity::resolve_caller_identity_with_owner`]）に委譲する。
    /// ここが持つのは **Nostr 固有の 2 点**だけ:
    ///
    /// 1. **オーナー識別子の出どころ** — `agent_nostr_config.owner_pubkey`
    ///    （Discord の `agent_discord_config.owner_discord_id` に相当）。未設定なら
    ///    誰もオーナーにならない（fail-closed）。
    /// 2. **表現の正規化** — 同じ鍵が npub と hex の 2 通りで現れる。比較の前に
    ///    両辺を hex へ寄せる。正規化できない発言者識別子は最小権限へ倒す
    ///    （壊れた値をそのまま突き合わせて偶然一致させない）。
    ///
    /// 信頼済みユーザーの照合は `platform = 'nostr'` の行だけを見る（Discord の
    /// 識別子空間と混ざらない）。行が npub で登録されていても引けるよう、hex と npub の
    /// 両方の表現で引く。
    fn resolve_nostr_caller(
        &self,
        agent_id: &str,
        author_pubkey: &str,
    ) -> opencrab_actions::CallerIdentity {
        // DB を引けなければ最小権限（fail-closed）。
        let Ok(conn) = self.db.lock() else {
            return opencrab_actions::CallerIdentity::Agent;
        };
        resolve_nostr_caller_identity(&conn, agent_id, author_pubkey)
    }

    fn list_enabled_nostr_configs(&self) -> Vec<opencrab_db::queries::AgentNostrConfigRow> {
        let conn = self.db.lock().unwrap();
        opencrab_db::queries::list_enabled_agent_nostr_configs(&conn).unwrap_or_default()
    }

    fn get_nostr_config(
        &self,
        agent_id: &str,
    ) -> Option<opencrab_db::queries::AgentNostrConfigRow> {
        let conn = self.db.lock().unwrap();
        opencrab_db::queries::get_agent_nostr_config(&conn, agent_id).unwrap_or(None)
    }

    fn set_nostr_secret_key(&self, agent_id: &str, secret_key: &str) -> anyhow::Result<()> {
        let conn = self.db.lock().unwrap();
        opencrab_db::queries::set_agent_nostr_config_secret_key(&conn, agent_id, secret_key)?;
        Ok(())
    }

    fn upsert_nostr_config(
        &self,
        cfg: &opencrab_db::queries::AgentNostrConfigRow,
    ) -> anyhow::Result<()> {
        let conn = self.db.lock().unwrap();
        opencrab_db::queries::upsert_agent_nostr_config(&conn, cfg)?;
        Ok(())
    }

    fn set_nostr_enabled(&self, agent_id: &str, enabled: bool) -> anyhow::Result<()> {
        let conn = self.db.lock().unwrap();
        opencrab_db::queries::set_agent_nostr_config_enabled(&conn, agent_id, enabled)?;
        Ok(())
    }

    /// エージェント宛の Nostr 受信を転記する宛先を解決する（issue #252 段階 A）。
    ///
    /// 同期 DB 読み 1 回。fail-closed（未設定 / 無効 / 不正 → `None`）の判定は actions 層の
    /// `resolve_nostr_relay_webhook` に集約してあるので、ここは委譲するだけ。
    fn resolve_nostr_relay_target(
        &self,
        agent_id: &str,
    ) -> Option<opencrab_actions::webhook_target::WebhookConfig> {
        let conn = self.db.lock().unwrap();
        opencrab_actions::webhook_target::resolve_nostr_relay_webhook(&conn, agent_id)
    }

    /// 解決済みの宛先へ転記本文を**非ブロック**で送る（issue #252 段階 A / #293）。
    ///
    /// 送信は常に **1 回**。Discord の content 上限（2000 文字）に収まればそのまま JSON、
    /// 超えるなら「出だしのプレビュー + 全文を添付ファイル」の multipart 1 通にする
    /// （#293。従来の分割連投はレート制限に当たりやすく、読みづらく、全文をコピーし
    /// づらかった）。
    ///
    /// **非ブロック性の担保**: 本文の整形・添付バイト列の生成（切り詰め含む）は spawn
    /// **前**に済ませ、HTTP は `tokio::spawn` の中だけで待つ。呼び出し元（Nostr 受信
    /// ループ）は即座に戻る。DB ロックは宛先解決時に閉じており、ここでは保持していない。
    /// 送信失敗は**ログのみ**で、応答生成や他セッションの受信を巻き込まない。
    /// 生 URL はログに出さない。
    fn relay_inbound_notification(
        &self,
        target: &opencrab_actions::webhook_target::WebhookConfig,
        text: String,
    ) {
        // 送信前に整形とサイズ確定を済ませる（巨大ボディをそのまま投げない）。
        let message = opencrab_actions::build_message_with_optional_attachment(
            &text,
            "nostr-inbound", // 静的な語彙のみ。相手の pubkey / 本文は名前に載せない。
        );
        spawn_relay_post(target.url.clone(), message);
    }
}

/// 転記 1 通を **1 回** POST する（fire-and-forget）。
///
/// 添付があれば multipart（`payload_json` + `files[0]`）、無ければ JSON。どちらも
/// `tokio::spawn` の中でだけ待つので**呼び出し元はブロックされない**。整形・添付バイト列の
/// 生成は呼び出し側で済んでいる前提（ここでは重い処理をしない）。失敗はログのみ。
fn spawn_relay_post(url: String, message: opencrab_actions::WebhookMessage) {
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        // allowed_mentions を必ず抑止して送る（mention 暴発対策）。
        // 詳細は webhook_target::build_relay_webhook_body の doc を参照。
        let body = opencrab_actions::webhook_target::build_relay_webhook_body(&message.content);
        let req = match &message.attachment {
            Some(att) => {
                // Discord webhook の multipart 仕様: 本体は payload_json、添付は files[0]。
                let part = reqwest::multipart::Part::bytes(att.data.clone())
                    .file_name(att.filename.clone())
                    .mime_str(&att.content_type)
                    .unwrap_or_else(|_| {
                        reqwest::multipart::Part::bytes(att.data.clone())
                            .file_name(att.filename.clone())
                    });
                let form = reqwest::multipart::Form::new()
                    .text("payload_json", body.to_string())
                    .part("files[0]", part);
                client
                    .post(&url)
                    .timeout(RELAY_SEND_TIMEOUT)
                    .multipart(form)
            }
            None => client.post(&url).timeout(RELAY_SEND_TIMEOUT).json(&body),
        };
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                tracing::warn!(
                    status = resp.status().as_u16(),
                    "Nostr 受信の Discord 転記が非成功ステータスで失敗（ログのみ）"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Nostr 受信の Discord 転記の送信に失敗（ログのみ）"
                );
            }
        }
    });
}

/// 転記 1 回あたりのハング上限。ここで必ず打ち切ることで、接続が黙って死んでも
/// spawn したタスクが永久に生き残らない。添付（最大 8 MiB）を遅い回線で送り切れる
/// 余裕として 60 秒（discord crate の配送 worker と同じ値）。
const RELAY_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// #319: Nostr 受信ターンの呼び出し元解決。
///
/// 実 DB（in-memory）で「オーナーだけが Owner になる」ことを固定する。
#[cfg(test)]
mod caller_tests {
    use super::resolve_nostr_caller_identity;
    use opencrab_actions::CallerIdentity;
    use opencrab_db::queries::{
        AgentNostrConfigRow, TrustedUserPermission, TRUSTED_PLATFORM_DISCORD,
        TRUSTED_PLATFORM_NOSTR,
    };
    use rusqlite::Connection;

    /// ダミー鍵（実在の pubkey は書かない）。
    const OWNER_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const STRANGER_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000002";
    const FRIEND_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000003";
    const AGENT: &str = "agent-1";

    /// Nostr 設定行だけがある DB（オーナーは未設定）。
    fn db_with_nostr_row() -> Connection {
        let conn = opencrab_db::init_memory().unwrap();
        opencrab_db::queries::upsert_agent_nostr_config(
            &conn,
            &AgentNostrConfigRow {
                agent_id: AGENT.to_string(),
                secret_key: "nsec1dummy".to_string(),
                relays_json: "[]".to_string(),
                filter_json: "{}".to_string(),
                enabled: false,
            },
        )
        .unwrap();
        conn
    }

    fn set_owner(conn: &Connection, pubkey: &str) {
        assert!(
            opencrab_db::queries::set_agent_nostr_owner_pubkey(conn, AGENT, pubkey).unwrap(),
            "オーナーの保存に失敗（設定行が無い）"
        );
    }

    fn register(conn: &Connection, platform: &str, user_id: &str, perm: TrustedUserPermission) {
        opencrab_db::queries::add_trusted_user(
            conn,
            platform,
            &format!("row-{platform}-{user_id}"),
            AGENT,
            user_id,
            perm,
            "owner",
            "2026-01-01",
            "",
        )
        .unwrap();
    }

    fn npub_of(hex: &str) -> String {
        opencrab_nostr::to_npub(hex).expect("npub へ変換できること")
    }

    /// **本丸**: オーナーの pubkey から来たターンは `Owner`。
    #[test]
    fn owner_pubkey_resolves_to_owner() {
        let conn = db_with_nostr_row();
        set_owner(&conn, OWNER_HEX);
        assert_eq!(
            resolve_nostr_caller_identity(&conn, AGENT, OWNER_HEX),
            CallerIdentity::Owner
        );
    }

    /// **本丸**: 他人の pubkey は `Agent` のまま（昇格しない）。
    #[test]
    fn other_pubkey_stays_agent() {
        let conn = db_with_nostr_row();
        set_owner(&conn, OWNER_HEX);
        assert_eq!(
            resolve_nostr_caller_identity(&conn, AGENT, STRANGER_HEX),
            CallerIdentity::Agent
        );
    }

    /// **fail-closed**: オーナー未設定なら誰も Owner にならない。
    #[test]
    fn unset_owner_grants_owner_to_nobody() {
        let conn = db_with_nostr_row();
        // 設定行はあるが owner_pubkey は既定の空。
        for pk in [OWNER_HEX, STRANGER_HEX] {
            assert_eq!(
                resolve_nostr_caller_identity(&conn, AGENT, pk),
                CallerIdentity::Agent,
                "オーナー未設定なのに Owner になった: {pk}"
            );
        }
        // 空白のみも未設定として扱う。
        set_owner(&conn, "   ");
        assert_eq!(
            resolve_nostr_caller_identity(&conn, AGENT, OWNER_HEX),
            CallerIdentity::Agent
        );
        // Nostr 設定行そのものが無いエージェントも同じ（オーナー未設定）。
        assert_eq!(
            resolve_nostr_caller_identity(&conn, "agent-without-nostr", OWNER_HEX),
            CallerIdentity::Agent
        );
    }

    /// **本丸**: npub で設定して hex で受信しても一致する（表現差で取りこぼさない）。
    #[test]
    fn owner_set_as_npub_matches_hex_speaker() {
        let conn = db_with_nostr_row();
        // 入口の正規化を通さず、npub のまま入っている行（手書き / 旧データ）でも拾う。
        set_owner(&conn, &npub_of(OWNER_HEX));
        assert_eq!(
            resolve_nostr_caller_identity(&conn, AGENT, OWNER_HEX),
            CallerIdentity::Owner,
            "npub で設定したオーナーが hex の受信で一致しない"
        );
    }

    /// 逆向き: hex で設定して npub で来ても一致する。
    #[test]
    fn owner_set_as_hex_matches_npub_speaker() {
        let conn = db_with_nostr_row();
        set_owner(&conn, OWNER_HEX);
        assert_eq!(
            resolve_nostr_caller_identity(&conn, AGENT, &npub_of(OWNER_HEX)),
            CallerIdentity::Owner,
            "hex で設定したオーナーが npub の発言者と一致しない"
        );
        // 別人の npub は依然として Agent。
        assert_eq!(
            resolve_nostr_caller_identity(&conn, AGENT, &npub_of(STRANGER_HEX)),
            CallerIdentity::Agent
        );
    }

    /// **識別子空間の分離**: `platform='discord'` の行は Nostr の照合に混ざらない。
    ///
    /// 一意制約が今も `(user_id, agent_id)`（#159 の残作業）なので、同じ識別子を
    /// 2 経路に同時登録できない。DB を分けて「同じ識別子でも経路が違えば効かない」
    /// ことを見る。
    #[test]
    fn discord_trusted_rows_do_not_leak_into_nostr() {
        let discord_only = db_with_nostr_row();
        register(
            &discord_only,
            TRUSTED_PLATFORM_DISCORD,
            FRIEND_HEX,
            TrustedUserPermission::User,
        );
        assert_eq!(
            resolve_nostr_caller_identity(&discord_only, AGENT, FRIEND_HEX),
            CallerIdentity::Agent,
            "Discord 経路の行が Nostr の照合に混ざった"
        );
        // co-agent 権限でも同じ（Discord の行から Nostr で CoAgent にならない）。
        let discord_coagent = db_with_nostr_row();
        register(
            &discord_coagent,
            TRUSTED_PLATFORM_DISCORD,
            FRIEND_HEX,
            TrustedUserPermission::CoAgent,
        );
        assert_eq!(
            resolve_nostr_caller_identity(&discord_coagent, AGENT, FRIEND_HEX),
            CallerIdentity::Agent
        );

        // 同じ識別子を Nostr 経路の行として登録すると、初めて信頼される。
        let nostr_row = db_with_nostr_row();
        register(
            &nostr_row,
            TRUSTED_PLATFORM_NOSTR,
            FRIEND_HEX,
            TrustedUserPermission::User,
        );
        assert_eq!(
            resolve_nostr_caller_identity(&nostr_row, AGENT, FRIEND_HEX),
            CallerIdentity::TrustedUser
        );
    }

    /// Nostr 経路の行が npub で登録されていても、hex の発言者で引き当たる。
    #[test]
    fn trusted_row_registered_as_npub_matches_hex_speaker() {
        let conn = db_with_nostr_row();
        register(
            &conn,
            TRUSTED_PLATFORM_NOSTR,
            &npub_of(FRIEND_HEX),
            TrustedUserPermission::User,
        );
        assert_eq!(
            resolve_nostr_caller_identity(&conn, AGENT, FRIEND_HEX),
            CallerIdentity::TrustedUser
        );
    }

    /// **昇格経路を新設しない**: 表の `owner` 権限では `Owner` にならない
    /// （Nostr で Owner になれるのは「オーナー pubkey と一致した」ときだけ）。
    #[test]
    fn trusted_row_with_owner_permission_does_not_become_owner() {
        let conn = db_with_nostr_row();
        register(
            &conn,
            TRUSTED_PLATFORM_NOSTR,
            STRANGER_HEX,
            TrustedUserPermission::Owner,
        );
        assert_eq!(
            resolve_nostr_caller_identity(&conn, AGENT, STRANGER_HEX),
            CallerIdentity::TrustedUser,
            "表の owner 権限から Owner へ上がる道ができている"
        );
    }

    /// 壊れた発言者識別子は最小権限（偶然一致させない）。
    #[test]
    fn malformed_speaker_is_least_privileged() {
        let conn = db_with_nostr_row();
        set_owner(&conn, OWNER_HEX);
        for bad in ["", "   ", "not-a-key", "npub1broken"] {
            assert_eq!(
                resolve_nostr_caller_identity(&conn, AGENT, bad),
                CallerIdentity::Agent,
                "壊れた識別子が最小権限に落ちない: {bad:?}"
            );
        }
    }

    /// オーナーは**そのエージェントの設定**で決まる（他エージェントへ波及しない）。
    #[test]
    fn owner_is_scoped_to_the_agent() {
        let conn = db_with_nostr_row();
        set_owner(&conn, OWNER_HEX);
        opencrab_db::queries::upsert_agent_nostr_config(
            &conn,
            &AgentNostrConfigRow {
                agent_id: "agent-2".to_string(),
                secret_key: "nsec1dummy2".to_string(),
                relays_json: "[]".to_string(),
                filter_json: "{}".to_string(),
                enabled: false,
            },
        )
        .unwrap();
        assert_eq!(
            resolve_nostr_caller_identity(&conn, "agent-2", OWNER_HEX),
            CallerIdentity::Agent,
            "別エージェントのオーナー設定が波及した"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// モックが記録した (content-type, body) の列。
    type Recorded = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// 依存を増やさない最小の HTTP モック。実 Discord には一切出さない。
    /// 受け取った (content-type, body) を記録し、`delay` 後に 204 を返す。
    async fn mock_webhook(delay: Duration) -> (String, Recorded) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let got: Recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let sink = sink.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let head_end = loop {
                        let n = match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break p + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
                    let header = |name: &str| -> Option<String> {
                        head.split("\r\n").find_map(|l| {
                            l.strip_prefix(&format!("{name}: "))
                                .map(|v| v.trim().to_string())
                        })
                    };
                    let len: usize = header("content-length")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    while buf.len() < head_end + len {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    sink.lock().unwrap().push((
                        header("content-type").unwrap_or_default(),
                        buf[head_end..].to_vec(),
                    ));
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 204 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (format!("http://{addr}/api/webhooks/1/tok"), got)
    }

    async fn wait_for(got: &Recorded, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if got.lock().unwrap().len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {n} request(s), got {}", got.lock().unwrap().len());
    }

    /// #293: 長い転記本文は分割連投せず、**1 回の multipart** で出る。
    /// allowed_mentions の抑止は payload_json 側に載ったままであること（#252 の担保）。
    #[tokio::test]
    async fn long_relay_text_is_one_multipart_post() {
        let (url, got) = mock_webhook(Duration::ZERO).await;
        let text = "N".repeat(6000);
        let msg = opencrab_actions::build_message_with_optional_attachment(&text, "nostr-inbound");
        spawn_relay_post(url, msg);
        wait_for(&got, 1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let reqs = got.lock().unwrap().clone();
        assert_eq!(reqs.len(), 1, "長文でも POST は 1 回だけ");
        assert!(
            reqs[0].0.starts_with("multipart/form-data"),
            "content-type: {}",
            reqs[0].0
        );
        let body = String::from_utf8_lossy(&reqs[0].1).to_string();
        assert!(body.contains("filename=\"nostr-inbound.txt\""));
        assert!(body.contains(&text), "添付が全文でない");
        // mention 暴発の抑止は multipart でも維持される。
        assert!(body.contains("allowed_mentions"), "mention 抑止が落ちた");
    }

    /// 短い転記は従来どおり JSON のみ（添付しない）。回帰テスト。
    #[tokio::test]
    async fn short_relay_text_stays_plain_json() {
        let (url, got) = mock_webhook(Duration::ZERO).await;
        let msg = opencrab_actions::build_message_with_optional_attachment("hi", "nostr-inbound");
        spawn_relay_post(url, msg);
        wait_for(&got, 1).await;
        let reqs = got.lock().unwrap().clone();
        assert_eq!(reqs[0].0, "application/json");
        let body = String::from_utf8_lossy(&reqs[0].1).to_string();
        assert!(body.contains(r#""content":"hi""#), "body: {body}");
        assert!(body.contains("allowed_mentions"));
    }

    /// 相手が遅くても呼び出し元（Nostr 受信ループ）は即座に戻る。
    #[tokio::test]
    async fn relay_post_never_blocks_the_caller() {
        let slow = Duration::from_millis(600);
        let (url, got) = mock_webhook(slow).await;
        let msg = opencrab_actions::build_message_with_optional_attachment(
            &"S".repeat(5000),
            "nostr-inbound",
        );
        let start = Instant::now();
        spawn_relay_post(url, msg);
        assert!(
            start.elapsed() < slow,
            "呼び出し元が配送に引きずられた: {:?}",
            start.elapsed()
        );
        wait_for(&got, 1).await;
    }

    /// 送信が失敗（宛先が居ない）しても panic せず、呼び出し元の後続処理は進む。
    #[tokio::test]
    async fn relay_post_failure_is_swallowed() {
        // 誰も listen していないポートへ投げる。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let msg = opencrab_actions::build_message_with_optional_attachment("boom", "nostr-inbound");
        spawn_relay_post(format!("http://{addr}/api/webhooks/1/tok"), msg);
        // 呼び出し元はそのまま進める。
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}
