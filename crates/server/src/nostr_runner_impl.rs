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

    fn list_enabled_nostr_configs(
        &self,
    ) -> anyhow::Result<Vec<opencrab_db::queries::AgentNostrConfigRow>> {
        let conn = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("db lock for enabled Nostr configuration list"))?;
        opencrab_db::queries::list_enabled_agent_nostr_configs(&conn)
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

    fn list_session_watches_for_agent(
        &self,
        agent_id: &str,
    ) -> anyhow::Result<Vec<opencrab_db::queries::SessionWatchRow>> {
        let conn = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("session_watches: DB ロックが poison"))?;
        opencrab_db::queries::list_session_watches_for_agent(&conn, agent_id)
    }

    fn get_session_policy_json(&self, session_id: &str) -> anyhow::Result<Option<String>> {
        let conn = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("policy_json: DB ロックが poison"))?;
        opencrab_db::queries::get_session_policy_json(&conn, session_id)
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
#[path = "nostr_runner_impl/caller_tests.rs"]
mod caller_tests;

#[cfg(test)]
#[path = "nostr_runner_impl/tests.rs"]
mod tests;
