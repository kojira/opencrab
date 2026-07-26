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
            Ok(conn) => {
                // 経路は Discord 固定（この trait は Discord ゲートウェイ専用）。
                // 信頼の判定も登録件数の判定も同じ経路で切る（#214）。件数を経路で
                // 切らないと、web / REST に 1 件登録があるだけでここが「登録あり」に
                // 化けて、Discord 側の owner-only フォールバックが黙って外れる。
                let any_trusted = agent_ids.iter().any(|aid| {
                    opencrab_db::queries::is_trusted_user(
                        &conn,
                        opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
                        sender_id,
                        aid,
                    )
                });
                let any_registered = agent_ids.iter().any(|aid| {
                    opencrab_db::queries::trusted_user_count(
                        &conn,
                        opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
                        aid,
                    ) > 0
                });
                if any_registered {
                    any_trusted
                } else {
                    // 信頼ユーザー登録が全く無い場合は owner のみ許可（owner 未設定なら全許可）。
                    // owner 未設定時に全許可へ倒れる既存挙動。fail-closed への統一は #174。
                    //
                    // 空判定は trim して行う（空白のみの owner は未設定と同じ扱い、という
                    // `is_owner_id` と同じ不変条件にそろえる）。owner が非空なら上の
                    // `is_owner_id` で既に判定済みなので、ここでの生比較は不要。
                    owner_discord_id.trim().is_empty()
                }
            }
            // DB接続取得失敗時は fail-closed。
            Err(_) => false,
        }
    }

    fn dm_allowed(&self, sender_id: &str, agent_id: &str, owner_discord_id: &str) -> bool {
        if crate::api::is_owner_id(owner_discord_id, sender_id) {
            return true;
        }
        match self.db.lock() {
            Ok(conn) => {
                // 経路は Discord 固定。件数判定も同じ経路で切る（理由は `dm_allowed_any`）。
                if opencrab_db::queries::trusted_user_count(
                    &conn,
                    opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
                    agent_id,
                ) > 0
                {
                    opencrab_db::queries::is_trusted_user(
                        &conn,
                        opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
                        sender_id,
                        agent_id,
                    )
                } else {
                    // このエージェントに信頼ユーザー登録が無い場合は owner のみ許可。
                    // owner 未設定時に全許可へ倒れる既存挙動。fail-closed への統一は #174。
                    // 空判定を trim で行う理由は `dm_allowed_any` と同じ。
                    owner_discord_id.trim().is_empty()
                }
            }
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
        match trust_info {
            Some(u) if u.permission == "co_agent" => opencrab_actions::CallerIdentity::CoAgent {
                agent_id: sender_id.to_string(),
            },
            Some(u) if u.permission == "owner" => opencrab_actions::CallerIdentity::Owner,
            Some(_) => opencrab_actions::CallerIdentity::TrustedUser,
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

    fn served_by_dedicated_gateway(&self, agent_id: &str) -> bool {
        // DB の enabled フラグではなく manager の liveness で判定する（#40）。
        // enabled=1 でもゲートウェイが起動失敗/停止していれば false → 共有側が
        // フォールバックとして処理を続け、「誰も応答しない」状態を作らない。
        self.discord_manager
            .as_ref()
            .map(|m| m.is_running(agent_id))
            .unwrap_or(false)
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

    /// 信頼ユーザー未登録時のフォールバック（owner 未設定なら全許可）で、
    /// 「空白のみの owner は未設定と同じ」という不変条件が守られる。
    ///
    /// 修正前は生比較 `sender_id == owner_discord_id` が残っていたため、
    /// owner が `" "` のとき「`" "` を送った送信者だけ許可、他は拒否」という
    /// `is_owner_id` と矛盾する挙動になっていた。
    #[test]
    fn dm_fallback_treats_whitespace_only_owner_as_unset() {
        let state = test_state();
        let agents = ["agent-1".to_string()];

        // owner 未設定（空文字）: 誰でも通る（既存の fail-open 挙動、#174 で見直し）。
        assert!(state.dm_allowed("someone-else", "agent-1", ""));
        assert!(state.dm_allowed_any("someone-else", &agents, ""));

        // owner が空白のみ: 空文字と同じ扱いになる（「空白を送った人だけ通る」ではない）。
        assert!(state.dm_allowed("someone-else", "agent-1", " "));
        assert!(state.dm_allowed_any("someone-else", &agents, " \n"));
        assert!(state.dm_allowed(" ", "agent-1", " "));
    }

    /// owner が設定済みで信頼ユーザー未登録なら、owner 以外の DM は拒否される
    /// （フォールバックの trim 化で「常に全許可」へ緩んでいないことの確認）。
    #[test]
    fn dm_fallback_denies_non_owner_when_owner_is_set() {
        let state = test_state();
        let agents = ["agent-1".to_string()];
        assert!(!state.dm_allowed("987654321098765432", "agent-1", "123456789012345678"));
        assert!(!state.dm_allowed_any("987654321098765432", &agents, "123456789012345678"));
        // owner 本人（前後空白付きの設定値でも）は通る。
        assert!(state.dm_allowed("123456789012345678", "agent-1", " 123456789012345678 "));
        assert!(state.dm_allowed_any("123456789012345678", &agents, " 123456789012345678 "));
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
            "user",
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

    /// #214: 登録件数の判定も経路で切られていること。
    ///
    /// web に 1 件あるだけで Discord 側が「登録あり」に化けると、owner-only の段が
    /// 黙って別の分岐に入る。ここでは Discord 側の件数が 0 のままであることを、
    /// 「owner 未設定なら全許可」という既存フォールバックが生きることで観測する。
    #[test]
    fn dm_registration_count_is_scoped_to_discord() {
        let state = test_state();
        let agents = ["agent-1".to_string()];

        // web 経路にだけ登録がある（Discord から見れば 0 件のまま）。
        register_trusted(
            &state,
            opencrab_db::queries::TRUSTED_PLATFORM_WEB,
            "dash-user",
            "agent-1",
        );

        // owner 未設定 → Discord 側は「登録 0 件」の段に入る（既存の fail-open、#174）。
        assert!(state.dm_allowed("someone-else", "agent-1", ""));
        assert!(state.dm_allowed_any("someone-else", &agents, ""));

        // owner 設定済みなら owner のみ許可（緩みっぱなしではない）。
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
