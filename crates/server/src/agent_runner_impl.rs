//! AgentRunner trait implementation for AppState.
//!
//! Bridges the discord crate's AgentRunner trait to the server's
//! process module, breaking the circular dependency.
//!
//! ゲートウェイ非依存なメソッド（応答生成・会話履歴・トークン予算・セッション/
//! インタラクション管理）は `agent_runtime_impl.rs` の
//! [`opencrab_actions::AgentRuntime`] 実装が持つ（#156 S1）。転記（#42）も同様に
//! そちらへ移した（#158 S3）。

use crate::AppState;
use opencrab_db::queries::TrustedUserPermission;

impl opencrab_discord::AgentRunner for AppState {
    fn db(&self) -> &opencrab_db::Db {
        &self.db
    }

    fn workspace_base(&self) -> &str {
        &self.workspace_base
    }

    // ---- 判定（#43） ----

    fn is_channel_writable(&self, channel_id: &str) -> bool {
        self.db
            .lock()
            .map(|conn| opencrab_db::queries::is_channel_writable(&conn, channel_id))
            .unwrap_or(false)
    }

    fn is_channel_whitelisted_for_agent(&self, channel_id: &str, agent_id: &str) -> bool {
        self.db
            .lock()
            .map(|conn| {
                opencrab_db::queries::is_channel_whitelisted_for_agent(&conn, channel_id, agent_id)
            })
            .unwrap_or(false)
    }

    /// DM 受付判定（いずれかのエージェントが送信者を信頼していれば通す事前ゲート）。
    ///
    /// 許可されるのは **owner か、Discord 経路の信頼ユーザーだけ**（#174）。
    /// 以前はここに「そのエージェントに信頼ユーザー登録が 1 件も無ければ owner のみ
    /// 許可、**owner 未設定ならさらに全許可**」という二段のフォールバックがあった。
    /// 「設定が無ければ制限しない」は、
    ///
    /// - 権限モデルが黙って反転する（オーナー欄を空のまま運用すると、意図せず
    ///   誰でも DM できる状態になる）
    /// - #159 で信頼済みの行が別経路にしか無いエージェントが現れると、Discord から
    ///   見て「登録 0 件」に落ち、**それまで登録者限定だった DM が全開放になる**
    ///
    /// ため、拒否側に統一した。登録 0 件の分岐が消えたことで、そこで使っていた
    /// `trusted_user_count`（#214 で経路を切った件数判定）も不要になった。件数を
    /// 見ずに `is_trusted_user`（同じく Discord 経路固定）だけで判定するので、
    /// 別経路の登録がこの判定に混入する余地は構造的に無くなっている。
    ///
    /// owner 未設定の環境ではこのゲートを誰も通れない。無音で機能が変わらないよう、
    /// ゲートウェイ起動時に警告を出す（`opencrab_discord::owner_warning`）。
    fn dm_allowed_any(
        &self,
        sender_id: &str,
        agent_ids: &[String],
        owner_discord_id: &str,
    ) -> bool {
        if crate::api::is_owner_id(owner_discord_id, sender_id) {
            return true;
        }
        match self.db.lock() {
            Ok(conn) => agent_ids.iter().any(|aid| {
                // 経路は Discord 固定（この trait は Discord ゲートウェイ専用、#214）。
                opencrab_db::queries::is_trusted_user(
                    &conn,
                    opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
                    sender_id,
                    aid,
                )
            }),
            // DB接続取得失敗時は fail-closed。
            Err(_) => false,
        }
    }

    /// エージェント個別の DM 受付判定。許可条件は `dm_allowed_any` と同じ
    /// （owner か、そのエージェントの Discord 経路の信頼ユーザーのみ）。
    fn dm_allowed(&self, sender_id: &str, agent_id: &str, owner_discord_id: &str) -> bool {
        if crate::api::is_owner_id(owner_discord_id, sender_id) {
            return true;
        }
        match self.db.lock() {
            Ok(conn) => opencrab_db::queries::is_trusted_user(
                &conn,
                opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
                sender_id,
                agent_id,
            ),
            Err(_) => false,
        }
    }

    fn resolve_caller(
        &self,
        sender_id: &str,
        agent_ids: &[String],
        owner_discord_id: &str,
    ) -> opencrab_actions::CallerIdentity {
        if crate::api::is_owner_id(owner_discord_id, sender_id) {
            return opencrab_actions::CallerIdentity::Owner;
        }
        // DB接続取得失敗時は trust_info=None（＝最小権限の Agent 扱い）。
        let trust_info = match self.db.lock() {
            Ok(conn) => agent_ids.iter().find_map(|aid| {
                // 経路は Discord 固定（#214）。互換読みは不要 — 従来の行が Discord 経路そのもの。
                opencrab_db::queries::get_trusted_user(
                    &conn,
                    opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
                    sender_id,
                    aid,
                )
            }),
            Err(_) => None,
        };
        // 権限は列挙型（#234）。variant を足したらここが網羅性で落ちる＝
        // 「新しい権限が黙って TrustedUser 扱いになる」が起きない。
        match trust_info.map(|u| u.permission) {
            Some(TrustedUserPermission::CoAgent) => opencrab_actions::CallerIdentity::CoAgent {
                agent_id: sender_id.to_string(),
            },
            Some(TrustedUserPermission::Owner) => opencrab_actions::CallerIdentity::Owner,
            Some(TrustedUserPermission::User) => opencrab_actions::CallerIdentity::TrustedUser,
            None => opencrab_actions::CallerIdentity::Agent,
        }
    }

    // ---- per-agent ゲートウェイ（#40） ----

    fn list_enabled_discord_configs(&self) -> Vec<opencrab_db::queries::AgentDiscordConfigRow> {
        match self
            .db
            .lock()
            .map_err(anyhow::Error::from)
            .and_then(|conn| {
                opencrab_db::queries::list_enabled_agent_discord_configs(&conn)
                    .map_err(anyhow::Error::from)
            }) {
            Ok(configs) => configs,
            Err(e) => {
                // 起動時の復元経路で使われるため、失敗を黙って空にしない。
                tracing::warn!(error = %e, "Failed to load agent discord configs from DB");
                Vec::new()
            }
        }
    }

    fn get_discord_config(
        &self,
        agent_id: &str,
    ) -> Option<opencrab_db::queries::AgentDiscordConfigRow> {
        let conn = self.db.lock().ok()?;
        opencrab_db::queries::get_agent_discord_config(&conn, agent_id).unwrap_or(None)
    }

    fn served_by_dedicated_gateway(&self, agent_id: &str) -> bool {
        // DB の enabled フラグではなく manager の liveness で判定する（#40）。
        // enabled=1 でもゲートウェイが起動失敗/停止していれば false → 共有側が
        // フォールバックとして処理を続け、「誰も応答しない」状態を作らない。
        //
        // 登録簿経由（#191 段階2 PR3）。**未登録も false** に倒れるので、名指し
        // フィールドが `None` だったときと同じ側（共有ゲートウェイが処理を続ける）に
        // 落ちる。true に倒すと二重処理、panic させると受信そのものが止まる。
        self.gateways
            .is_running(opencrab_actions::gateway_kinds::DISCORD, agent_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_actions::CallerIdentity;
    use opencrab_discord::AgentRunner;

    /// 最小構成の `AppState`（in-memory DB、LLM プロバイダ 0 件）。
    /// `resolve_caller` の owner 判定は DB とプロバイダに依存しないので十分。
    fn test_state() -> AppState {
        crate::test_app_state()
    }

    #[test]
    fn resolve_caller_grants_owner_when_owner_matches() {
        let state = test_state();
        let caller = state.resolve_caller(
            "123456789012345678",
            &["agent-1".to_string()],
            "123456789012345678",
        );
        assert_eq!(caller, CallerIdentity::Owner);
    }

    #[test]
    fn resolve_caller_does_not_grant_owner_when_owner_unset_and_sender_empty() {
        // owner 未設定（空文字）＋ 送信者 ID も空。ここで Owner に昇格しないこと。
        let state = test_state();
        let caller = state.resolve_caller("", &["agent-1".to_string()], "");
        assert_eq!(caller, CallerIdentity::Agent);
    }

    #[test]
    fn resolve_caller_does_not_grant_owner_when_owner_unset() {
        let state = test_state();
        let caller = state.resolve_caller("123456789012345678", &["agent-1".to_string()], "");
        assert_eq!(caller, CallerIdentity::Agent);
    }

    #[test]
    fn resolve_caller_treats_whitespace_only_owner_as_unset() {
        // 空白のみの owner 設定で、空白を送るだけでは Owner にならない。
        let state = test_state();
        assert_eq!(
            state.resolve_caller(" ", &["agent-1".to_string()], " "),
            CallerIdentity::Agent
        );
    }

    /// #174: owner 未設定 ＋ 信頼ユーザー登録 0 件 → **拒否**（従来は全許可）。
    ///
    /// これが本 issue の本体。「設定が無ければ制限しない」を残すと、#159 で
    /// 信頼済みの行が別経路にしか無いエージェントが現れた瞬間に、Discord から見て
    /// 「登録 0 件」に落ちて DM が全開放になる。
    #[test]
    fn dm_denied_when_owner_unset_and_no_trusted_users() {
        let state = test_state();
        let agents = ["agent-1".to_string()];

        assert!(!state.dm_allowed("someone-else", "agent-1", ""));
        assert!(!state.dm_allowed_any("someone-else", &agents, ""));

        // 空の送信者 ID が空の owner と一致して通ってしまわないこと。
        assert!(!state.dm_allowed("", "agent-1", ""));
        assert!(!state.dm_allowed_any("", &agents, ""));
    }

    /// #174: 空白のみの owner は未設定と同じ扱い（＝誰も通さない）。
    ///
    /// 「`" "` を送った送信者だけ許可」のような生比較が復活していないことの確認。
    #[test]
    fn dm_denied_when_owner_is_whitespace_only() {
        let state = test_state();
        let agents = ["agent-1".to_string()];

        assert!(!state.dm_allowed("someone-else", "agent-1", " "));
        assert!(!state.dm_allowed_any("someone-else", &agents, " \n"));
        assert!(!state.dm_allowed(" ", "agent-1", " "));
        assert!(!state.dm_allowed_any(" ", &agents, " "));
    }

    /// owner 設定済み ＋ 信頼ユーザー登録 0 件 → owner のみ許可（従来どおり）。
    #[test]
    fn dm_allows_only_owner_when_owner_is_set_and_no_trusted_users() {
        let state = test_state();
        let agents = ["agent-1".to_string()];
        assert!(!state.dm_allowed("987654321098765432", "agent-1", "123456789012345678"));
        assert!(!state.dm_allowed_any("987654321098765432", &agents, "123456789012345678"));
        // owner 本人（前後空白付きの設定値でも）は通る。
        assert!(state.dm_allowed("123456789012345678", "agent-1", " 123456789012345678 "));
        assert!(state.dm_allowed_any("123456789012345678", &agents, " 123456789012345678 "));
    }

    /// 信頼ユーザー登録があるときの挙動は従来どおり（owner 未設定でも信頼済みは通る）。
    ///
    /// fail-closed 化が「登録済みユーザーまで締め出す」方向に効いていないことの確認。
    #[test]
    fn dm_allows_trusted_user_even_when_owner_is_unset() {
        let state = test_state();
        let agents = ["agent-1".to_string()];
        register_trusted(
            &state,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
            "42",
            "agent-1",
        );

        assert!(state.dm_allowed("42", "agent-1", ""));
        assert!(state.dm_allowed_any("42", &agents, ""));
        // 登録が無い送信者は通らない。
        assert!(!state.dm_allowed("someone-else", "agent-1", ""));
        assert!(!state.dm_allowed_any("someone-else", &agents, ""));
    }

    #[test]
    fn resolve_caller_ignores_surrounding_whitespace_on_owner() {
        let state = test_state();
        assert_eq!(
            state.resolve_caller(
                "123456789012345678",
                &["agent-1".to_string()],
                " 123456789012345678\n"
            ),
            CallerIdentity::Owner
        );
    }

    /// #214: 別経路（web）の登録が Discord の DM 許可へ漏れないこと。
    ///
    /// 修正前は識別子空間が平坦だったため、web 経路に `"dash-user"` を登録すると
    /// (a) 件数判定が「登録あり」に化け、(b) 同じ文字列を名乗る Discord の送信者が
    /// そのまま信頼済みとして通っていた。
    fn register_trusted(state: &AppState, platform: &str, user_id: &str, agent_id: &str) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::add_trusted_user(
            &conn,
            platform,
            &format!("row-{platform}-{user_id}"),
            agent_id,
            user_id,
            TrustedUserPermission::User,
            "owner",
            "2026-01-01",
            "",
        )
        .unwrap();
    }

    #[test]
    fn dm_allow_does_not_inherit_trust_from_another_platform() {
        let state = test_state();
        let agents = ["agent-1".to_string()];
        let owner = "123456789012345678";

        // web 経路にだけ登録がある状態。
        register_trusted(
            &state,
            opencrab_db::queries::TRUSTED_PLATFORM_WEB,
            "dash-user",
            "agent-1",
        );

        // 同じ文字列を名乗る Discord 送信者は通らない（信頼が経路をまたがない）。
        assert!(!state.dm_allowed("dash-user", "agent-1", owner));
        assert!(!state.dm_allowed_any("dash-user", &agents, owner));

        // Discord 経路に登録すれば通る（判定そのものは壊れていない）。
        register_trusted(
            &state,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
            "42",
            "agent-1",
        );
        assert!(state.dm_allowed("42", "agent-1", owner));
        assert!(state.dm_allowed_any("42", &agents, owner));
    }

    /// #214 / #159 / #174: 別経路にしか登録が無くても DM は緩まない。
    ///
    /// 元は「登録件数も経路で切られている」ことを、owner 未設定時の fail-open が
    /// 生きることで観測していたテスト。#174 で件数による分岐そのものを撤去したので、
    /// 観測対象を**その分岐が守ろうとしていた結果**に置き換える: 信頼済みの行が
    /// 別経路（web）にしか無いエージェントで、Discord の DM が誰にも開かないこと。
    ///
    /// これは #159（経路ごとに行が書けるようになる）で発火するはずだった事故そのもの。
    #[test]
    fn dm_is_not_opened_by_registrations_on_another_platform() {
        let state = test_state();
        let agents = ["agent-1".to_string()];

        // web 経路にだけ登録がある（Discord から見れば 0 件のまま）。
        register_trusted(
            &state,
            opencrab_db::queries::TRUSTED_PLATFORM_WEB,
            "dash-user",
            "agent-1",
        );

        // owner 未設定でも全許可には落ちない（#174 の本体）。
        assert!(!state.dm_allowed("someone-else", "agent-1", ""));
        assert!(!state.dm_allowed_any("someone-else", &agents, ""));
        // web に登録された当人を名乗っても Discord からは通らない（#214）。
        assert!(!state.dm_allowed("dash-user", "agent-1", ""));
        assert!(!state.dm_allowed_any("dash-user", &agents, ""));

        // owner 設定済みなら owner のみ許可。
        assert!(!state.dm_allowed("someone-else", "agent-1", "123456789012345678"));
        assert!(!state.dm_allowed_any("someone-else", &agents, "123456789012345678"));
    }

    /// #214: `resolve_caller` も経路で切られている（別経路の permission を継がない）。
    #[test]
    fn resolve_caller_does_not_inherit_permission_from_another_platform() {
        let state = test_state();
        register_trusted(
            &state,
            opencrab_db::queries::TRUSTED_PLATFORM_WEB,
            "dash-user",
            "agent-1",
        );
        assert_eq!(
            state.resolve_caller("dash-user", &["agent-1".to_string()], "owner-id"),
            CallerIdentity::Agent
        );
    }
}
