//! NostrAgentRunner trait implementation for AppState.
//!
//! nostr ゲートウェイ（crates/nostr）の最小 runner を、既存の process /
//! transcript ヘルパへ委譲して実装する（discord の AgentRunner impl と同型）。
//!
//! ゲートウェイ非依存なメソッドは `agent_runtime_impl.rs` の
//! [`opencrab_actions::AgentRuntime`] 実装が持つ（#156 S1）。転記（受信イベント /
//! エージェント返信）も同様にそちらへ移した（#158 S3）。

use crate::AppState;

/// #620: DB へ Nostr 本鍵を書く前に at-rest 暗号化する（冪等）。
///
/// - 空 / 空白のみ、または既に暗号文（`enc:v1:…`）ならそのまま返す（二重暗号化しない・
///   round-trip の upsert を壊さない）。
/// - マスターキー未設定（`None`）なら**平文のまま**（暗号化を有効化していない構成の従来
///   挙動）。本番で Nostr サブシステムが動くときは必ず `Some`。
fn encrypt_at_rest(
    master: &Option<opencrab_nostr::MasterKey>,
    secret_key: &str,
) -> anyhow::Result<String> {
    if secret_key.trim().is_empty() || opencrab_core::secret_box::is_encrypted(secret_key) {
        return Ok(secret_key.to_string());
    }
    match master {
        Some(mk) => opencrab_core::secret_box::encrypt(secret_key.as_bytes(), mk),
        None => Ok(secret_key.to_string()),
    }
}

/// Nostr 受信ターンの呼び出し元を解決する（#319 / DB 接続だけに依存する本体）。
///
/// トレイト実装からロックを剥がしただけの純粋な関数にしてある（`AppState` を組まずに
/// 実 DB で検証できるようにするため）。
///
/// 判定そのものは web と同じ 1 実装
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

/// #698 元栓の許可源のうち **DB 由来**（owner / co_agent / trusted_users(platform=nostr)）の
/// 生 pubkey をまとめて読む（DB 接続だけに依存する本体。`AppState` を組まずに実 DB で検証できる）。
///
/// ホットパス（未許可イベントのドロップ）をメモリ照合に保つため、**更新経路だけ**がこれを呼ぶ。
/// 材料は [`resolve_nostr_caller_identity`] と**同じ DB 表**（owner_pubkey / trusted_users /
/// trusted_co_agents）で、判定の単一源をずらさない。正規化は受け手（nostr crate の
/// `build_allow_sources`）が follow_key で一括して行うので、ここは生の文字列を返す。
///
/// **未登録は `Ok(空)`**（各 getter は行が無ければ空を返す）。**DB の失敗は `Err` で伝播**させる
/// （`.unwrap_or_default()` で握り潰さない）。owner/trusted が DB エラーで無音で消えるのを防ぐため、
/// 呼び出し側で fetch_following の Err と同じ「前回値保持」に合流させる。
pub(crate) fn nostr_gate_allow_keys_from_db(
    conn: &rusqlite::Connection,
    agent_id: &str,
) -> anyhow::Result<opencrab_nostr::NostrGateAllowKeys> {
    use anyhow::Context as _;
    // owner: `agent_nostr_config.owner_pubkey`（未設定なら空文字 = 誰も owner にならない）。
    // 行が無ければ getter 自身が Ok("") を返す（未登録）。DB エラーだけ `?` で伝播。
    let owner_pubkey = opencrab_db::queries::get_agent_nostr_owner_pubkey(conn, agent_id)
        .context("#698: owner_pubkey の読み出しに失敗")?;
    let owner: Vec<String> = if owner_pubkey.trim().is_empty() {
        Vec::new()
    } else {
        vec![owner_pubkey]
    };
    // trusted_users: **platform='nostr' の行だけ**（Discord の識別子空間と混ぜない）。permission は
    // 問わない（登録されていれば信頼＝許可。精密な権限は resolve_nostr_caller が別に決める）。
    let trusted_users: Vec<String> = opencrab_db::queries::list_trusted_users(conn, agent_id)
        .context("#698: trusted_users の読み出しに失敗")?
        .into_iter()
        .filter(|u| u.platform == opencrab_db::queries::TRUSTED_PLATFORM_NOSTR)
        .map(|u| u.user_id)
        .collect();
    // co_agent（owner 等価 / #485 #489）: `trusted_co_agents`（agent UUID 対）の相手の
    // `agent_nostr_config.self_pubkey`。self_pubkey は各 agent 自身の接続からしか書かれない
    // （鍵所有で本人性担保）ので、UUID→self_pubkey の向きでも安全。未接続で self_pubkey が
    // **空文字**（未登録）の co_agent は正当にスキップ。self_pubkey の**読み出し失敗**（DB エラー）は
    // 下で `?` で伝播（空にして黙って消さない）。
    let mut co_agents: Vec<String> = Vec::new();
    for row in opencrab_db::queries::list_trusted_co_agents(conn, agent_id)
        .context("#698: trusted_co_agents の読み出しに失敗")?
    {
        let pk = opencrab_db::queries::get_agent_nostr_self_pubkey(conn, &row.co_agent_id)
            .context("#698: co_agent の self_pubkey 読み出しに失敗")?;
        if !pk.trim().is_empty() {
            co_agents.push(pk);
        }
    }
    Ok(opencrab_nostr::NostrGateAllowKeys {
        owner,
        co_agents,
        trusted_users,
    })
}

impl opencrab_nostr::NostrAgentRunner for AppState {
    /// 受信イベントの発言者から呼び出し元の権限を決める（#319）。
    ///
    /// 判定そのものは web と同じ 1 実装
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

    /// #698 元栓の DB 由来の許可源（owner / co_agent / trusted_users(platform=nostr)）を読む。
    /// 更新経路だけがこれを呼び、ホットパスはメモリ照合に保つ。未登録は `Ok(空)`、**DB の失敗
    /// （lock poison / query Err）は `Err`** で伝播させる（黙って空に化けさせない。呼び出し側で
    /// fetch_following の Err と同じ「前回値保持」に合流する）。
    fn nostr_gate_allow_keys(
        &self,
        agent_id: &str,
    ) -> anyhow::Result<opencrab_nostr::NostrGateAllowKeys> {
        let conn = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("#698: DB ロックが poison（許可源を読めない）"))?;
        nostr_gate_allow_keys_from_db(&conn, agent_id)
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
        // #620: DB へ書く前に at-rest 暗号化する（読みは暗号文のまま流し、復号は本鍵
        // プロバイダ / spawn guard だけが行う）。冪等なので二重暗号化しない。
        let secret_key = encrypt_at_rest(&self.nostr_master_key, secret_key)?;
        let conn = self.db.lock().unwrap();
        opencrab_db::queries::set_agent_nostr_config_secret_key(&conn, agent_id, &secret_key)?;
        Ok(())
    }

    fn set_nostr_self_pubkey(&self, agent_id: &str, self_pubkey: &str) -> anyhow::Result<()> {
        let conn = self.db.lock().unwrap();
        opencrab_db::queries::set_agent_nostr_self_pubkey(&conn, agent_id, self_pubkey)?;
        Ok(())
    }

    fn upsert_nostr_config(
        &self,
        cfg: &opencrab_db::queries::AgentNostrConfigRow,
    ) -> anyhow::Result<()> {
        // #620: secret_key を at-rest 暗号化してから書く（round-trip の暗号文は冪等で素通し）。
        let mut row = cfg.clone();
        row.secret_key = encrypt_at_rest(&self.nostr_master_key, &cfg.secret_key)?;
        let conn = self.db.lock().unwrap();
        opencrab_db::queries::upsert_agent_nostr_config(&conn, &row)?;
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

    /// #570: 受信本文の退避先＝このエージェントのワークスペース。`ws_read` と同じ
    /// resolver（[`opencrab_core::workspace::resolve_agent_workspace`]）で `{agent_id}` を
    /// 展開した実パスを返すので、退避ファイルは `ws_read` でそのまま読み返せる
    /// （#571 の「テンプレート未展開」を避ける）。不正な agent_id は `None`（退避せず
    /// 案内だけ残す fail-safe）。
    fn agent_workspace_root(&self, agent_id: &str) -> Option<std::path::PathBuf> {
        opencrab_core::workspace::resolve_agent_workspace(&self.workspace_base, agent_id).ok()
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

    // ---- #489: co_agent の識別子逆引き ----

    const FRIEND_AGENT_UUID: &str = "friend-agent-uuid";

    /// 送信側 agent（`agent_id`）が `self_pubkey` を接続で書いた状態にする。
    fn set_self_pubkey(conn: &Connection, agent_id: &str, pubkey: &str) {
        opencrab_db::queries::upsert_agent_nostr_config(
            conn,
            &AgentNostrConfigRow {
                agent_id: agent_id.to_string(),
                secret_key: "nsec1sender".to_string(),
                relays_json: "[]".to_string(),
                filter_json: "{}".to_string(),
                enabled: false,
            },
        )
        .unwrap();
        assert!(
            opencrab_db::queries::set_agent_nostr_self_pubkey(conn, agent_id, pubkey).unwrap(),
            "self_pubkey の保存に失敗（設定行が無い）"
        );
    }

    /// AGENT が co_agent として FRIEND_AGENT_UUID を信頼登録する（owner 登録の模擬）。
    fn trust_co_agent(conn: &Connection, agent_id: &str, co_agent_uuid: &str) {
        opencrab_db::queries::insert_trusted_co_agent(
            conn,
            &opencrab_db::queries::TrustedCoAgentRow {
                id: format!("row-{agent_id}-{co_agent_uuid}"),
                agent_id: agent_id.to_string(),
                co_agent_id: co_agent_uuid.to_string(),
                allowed_actions: None,
                created_by: "owner".to_string(),
                created_at: "2026-01-01".to_string(),
            },
        )
        .unwrap();
    }

    /// **本丸（#489）**: UUID 対で登録した co_agent が、発言者 pubkey → UUID の逆引きで発火する。
    #[test]
    fn co_agent_resolves_via_reverse_lookup() {
        let conn = db_with_nostr_row();
        // 送信側 agent が自 pubkey（FRIEND_HEX）を接続で登録済み。
        set_self_pubkey(&conn, FRIEND_AGENT_UUID, FRIEND_HEX);
        // AGENT は FRIEND_AGENT_UUID を co_agent（owner 等価）として登録。
        trust_co_agent(&conn, AGENT, FRIEND_AGENT_UUID);
        // FRIEND_HEX から届いたターンは、逆引きで FRIEND_AGENT_UUID に解決され CoAgent。
        assert_eq!(
            resolve_nostr_caller_identity(&conn, AGENT, FRIEND_HEX),
            CallerIdentity::CoAgent {
                agent_id: FRIEND_AGENT_UUID.to_string()
            },
            "UUID 登録の co_agent が逆引きで発火しない（#489 の本体）"
        );
        // npub 表記で来ても同じ（正規化して逆引き）。
        assert_eq!(
            resolve_nostr_caller_identity(&conn, AGENT, &npub_of(FRIEND_HEX)),
            CallerIdentity::CoAgent {
                agent_id: FRIEND_AGENT_UUID.to_string()
            }
        );
    }

    /// **fail-closed（#489）**: 送信側が未接続で self_pubkey が空なら、co_agent にならない。
    #[test]
    fn co_agent_fail_closed_when_self_pubkey_absent() {
        let conn = db_with_nostr_row();
        // 登録はあるが、送信側 agent の self_pubkey は未設定（未接続 = 逆引き不可）。
        trust_co_agent(&conn, AGENT, FRIEND_AGENT_UUID);
        assert_eq!(
            resolve_nostr_caller_identity(&conn, AGENT, FRIEND_HEX),
            CallerIdentity::Agent,
            "self_pubkey 空でも co_agent に化けた（fail-closed 違反）"
        );
    }

    /// **fail-closed（#489）**: 逆引きは成立するが、その UUID を co_agent 登録していなければ Agent。
    #[test]
    fn co_agent_fail_closed_when_reverse_maps_to_untrusted_uuid() {
        let conn = db_with_nostr_row();
        // 送信側は自 pubkey を登録済み（逆引きは成立）だが、AGENT は誰も co_agent 登録していない。
        set_self_pubkey(&conn, FRIEND_AGENT_UUID, FRIEND_HEX);
        assert_eq!(
            resolve_nostr_caller_identity(&conn, AGENT, FRIEND_HEX),
            CallerIdentity::Agent,
            "未登録の UUID が co_agent になった（誤許可）"
        );
    }

    // ---- #698: 元栓の DB 由来許可源の材料化 ----

    /// **本丸（#698）**: `nostr_gate_allow_keys_from_db` が owner / co_agent /
    /// trusted_users(platform=nostr) を材料化する。**discord 経路の trusted_user は混ぜない**。
    #[test]
    fn gate_allow_keys_materializes_owner_coagent_and_nostr_trusted_only() {
        let conn = db_with_nostr_row();
        set_owner(&conn, OWNER_HEX);
        // nostr の trusted_user（許可源に載る）。
        register(
            &conn,
            TRUSTED_PLATFORM_NOSTR,
            STRANGER_HEX,
            TrustedUserPermission::User,
        );
        // discord の trusted_user（経路が違うので**載らない**）。
        register(
            &conn,
            TRUSTED_PLATFORM_DISCORD,
            FRIEND_HEX,
            TrustedUserPermission::User,
        );
        // co_agent（UUID 対 → 相手の self_pubkey が FRIEND_HEX）。
        set_self_pubkey(&conn, FRIEND_AGENT_UUID, FRIEND_HEX);
        trust_co_agent(&conn, AGENT, FRIEND_AGENT_UUID);

        let keys = super::nostr_gate_allow_keys_from_db(&conn, AGENT).unwrap();
        assert_eq!(
            keys.owner,
            vec![OWNER_HEX.to_string()],
            "owner が材料化されない"
        );
        assert_eq!(
            keys.co_agents,
            vec![FRIEND_HEX.to_string()],
            "co_agent の self_pubkey が材料化されない"
        );
        assert_eq!(
            keys.trusted_users,
            vec![STRANGER_HEX.to_string()],
            "nostr の trusted_user だけを材料化していない（discord が混ざる / 抜ける）"
        );
    }

    /// fail-closed（#698）: owner 未設定・trusted/co_agent 無しなら全部空
    /// （フォロイー ∪ owner はフォローリスト側で担保され、ここが空でも allow-all にならない）。
    #[test]
    fn gate_allow_keys_empty_when_nothing_registered() {
        let conn = db_with_nostr_row();
        let keys = super::nostr_gate_allow_keys_from_db(&conn, AGENT).unwrap();
        assert!(keys.owner.is_empty());
        assert!(keys.co_agents.is_empty());
        assert!(keys.trusted_users.is_empty());
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
