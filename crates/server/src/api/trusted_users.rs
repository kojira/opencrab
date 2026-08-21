use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use opencrab_db::queries::TrustedUserPermission;

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct TrustedUserDto {
    pub id: String,
    /// その経路でのユーザー識別子（#159 で `discord_user_id` から改名）。
    /// どの経路の識別子かは同じ行の [`Self::platform`]。
    pub user_id: String,
    pub agent_id: String,
    /// 与えられている権限。**キーは据え置き、値はケバブケース**（#234）。
    /// 素の文字列だった頃はダッシュボードの `co-agent` と判定側の `co_agent` が
    /// 食い違い、登録が黙って無効になっていた。列挙型の serde 表現がそのまま出る。
    pub permission: TrustedUserPermission,
    pub created_by: String,
    pub created_at: String,
    pub display_name: String,
    /// `user_id` がどの経路の識別子か（`discord` / `web` / legacy `rest`, #159）。
    ///
    /// **応答に出す**: 互換読みの撤去後は「どの経路の行か」が権限そのものを決めるため、
    /// 一覧に出ていないと運用者は「登録したのに効かない」行を見分けられない。
    pub platform: String,
}

fn row_to_dto(r: opencrab_db::queries::TrustedUserRow) -> TrustedUserDto {
    TrustedUserDto {
        id: r.id,
        user_id: r.user_id,
        agent_id: r.agent_id,
        permission: r.permission,
        created_by: r.created_by,
        created_at: r.created_at,
        display_name: r.display_name,
        platform: r.platform,
    }
}

pub async fn list_trusted_users(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Json<Vec<TrustedUserDto>> {
    let conn = state.db.lock().unwrap();
    let rows = opencrab_db::queries::list_trusted_users(&conn, &agent_id).unwrap_or_default();
    Json(rows.into_iter().map(row_to_dto).collect())
}

#[derive(Debug, Deserialize)]
pub struct AddTrustedUserRequest {
    /// その経路でのユーザー識別子。旧キー `discord_user_id` も受け付ける（後方互換）。
    #[serde(alias = "discord_user_id")]
    pub user_id: String,
    pub permission: Option<String>,
    /// ロスター表示用の名前（ピアレビュアー一覧等）。省略時は空。
    pub display_name: Option<String>,
    /// `user_id` がどの経路の識別子か（`discord` / `web` / legacy `rest`, #159）。
    ///
    /// **省略時は `discord`**（#214 以前からの登録リクエストがそのまま動く）。
    pub platform: Option<String>,
}

/// 信頼済みユーザーを 1 件登録する。
///
/// `platform` で識別子空間を選ぶ（#159）。省略時は従来どおり `discord`。ダッシュボード
/// 利用者は `web` で登録する。`rest` は撤去済みの direct-message REST に属する既存行との
/// 互換のため受け付ける（互換読みの撤去後、他経路の行はその経路の権限を与えない）。
///
/// 未定義の経路は 400 で弾く（登録できても誰とも一致しない行になり、「登録したのに
/// 効かない」が黙って残るため）。一意制約 `(user_id, agent_id)` の衝突は 409
/// （同じ識別子を別経路で二重に持てるようにする制約の作り直しは非可逆なので #159 に残す。
/// 移行は旧行を DELETE してから登録し直す）。
///
/// **未定義の権限も同じ理由で 400**（#234）。ここが受け取った文字列を検証も正規化も
/// せず保存していたのが、ダッシュボードの `co-agent` が黙って無効な行になっていた
/// 直接の原因。`co_agent` のような別表記も**受け入れない** — 通す表記をひとつに
/// 保たないと、同じ食い違いがまた生える。
pub async fn add_trusted_user(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<AddTrustedUserRequest>,
) -> Result<Json<TrustedUserDto>, StatusCode> {
    let platform = req
        .platform
        .unwrap_or_else(|| opencrab_db::queries::TRUSTED_PLATFORM_DISCORD.to_string());
    if !opencrab_db::queries::is_known_trusted_platform(&platform) {
        return Err(StatusCode::BAD_REQUEST);
    }
    // `platform='nostr'` の識別子は保存前に canonical 小文字 hex へ正規化する。読み出し側
    // （[`crate::nostr_runner_impl::resolve_nostr_caller_identity`]）は canonical hex /
    // npub で exact-match するため、大文字 hex / 大文字 npub / 前後空白のまま保存すると
    // その信頼ユーザーが読み出しで一致しない。正規化できない値は保存せず 400
    // （設定できたように見えて永久に誰とも一致しない行を作らせない）。入口 `configure_nostr`
    // / REST の owner_pubkey と同じ扱いで、新しい制約ではなく既存の入口正規化の網羅。
    // 他の値（discord / web / legacy rest）の識別子は素通し（挙動を変えない）。
    // PR-1B: 公開鍵の正規化は Nostr クレートの実装なので nostr feature の内側。nostr を
    // 外した構成では `platform='nostr'` の信頼ユーザーは正規化できないため**受け付けず
    // 400 で拒否**する（丸めず・素通しの平文保存もしない＝暗黙のフォールバックを作らない）。
    let user_id = if platform == opencrab_db::queries::TRUSTED_PLATFORM_NOSTR {
        #[cfg(feature = "nostr")]
        {
            opencrab_nostr::normalize_pubkey(&req.user_id).ok_or(StatusCode::BAD_REQUEST)?
        }
        #[cfg(not(feature = "nostr"))]
        {
            return Err(StatusCode::BAD_REQUEST);
        }
    } else {
        req.user_id
    };
    let permission = parse_permission(req.permission.as_deref())?;
    let conn = state.db.lock().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let display_name = req.display_name.unwrap_or_default();

    opencrab_db::queries::add_trusted_user(
        &conn,
        &platform,
        &id,
        &agent_id,
        &user_id,
        permission,
        "owner",
        &now,
        &display_name,
    )
    .map_err(|e| {
        if is_unique_violation(&e) {
            // 同じ識別子が別経路で既に登録されている（制約は #159 で作り直す）。
            StatusCode::CONFLICT
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    })?;

    Ok(Json(TrustedUserDto {
        id,
        user_id,
        agent_id,
        permission,
        created_by: "owner".to_string(),
        created_at: now,
        display_name,
        platform,
    }))
}

/// リクエストの権限を列挙型へ。省略時は既定（`user`）、未知の値は 400（#234）。
///
/// **入口の検証であって認可の判定ではない。** ここを通った値だけが DB に入るので、
/// 表記ゆれの行は以後作られない。
fn parse_permission(raw: Option<&str>) -> Result<TrustedUserPermission, StatusCode> {
    match raw {
        None => Ok(TrustedUserPermission::default()),
        Some(s) => TrustedUserPermission::parse(s).ok_or(StatusCode::BAD_REQUEST),
    }
}

/// 一意制約違反か（＝運用者が直せる衝突で、サーバの故障ではない）。
fn is_unique_violation(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

#[derive(Debug, Deserialize)]
pub struct UpdateTrustedUserRequest {
    pub permission: Option<String>,
    pub display_name: Option<String>,
}

/// 権限を更新する。**未知の権限は 400**（登録と同じ入口の検証, #234）。
/// 権限を省略した更新（表示名だけ）は従来どおり権限に触らない。
pub async fn update_trusted_user(
    State(state): State<AppState>,
    Path((_agent_id, user_id)): Path<(String, String)>,
    Json(req): Json<UpdateTrustedUserRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let permission = match req.permission.as_deref() {
        None => None,
        Some(s) => Some(TrustedUserPermission::parse(s).ok_or(StatusCode::BAD_REQUEST)?),
    };
    let conn = state.db.lock().unwrap();
    // 2フィールドの更新は不可分にする（片方だけ永続化されて 500 を返さない）
    let update = || -> anyhow::Result<bool> {
        let tx = conn.unchecked_transaction()?;
        let mut updated = false;
        if let Some(permission) = permission {
            updated |=
                opencrab_db::queries::update_trusted_user_permission(&tx, &user_id, permission)?;
        }
        if let Some(ref display_name) = req.display_name {
            updated |= opencrab_db::queries::update_trusted_user_display_name(
                &tx,
                &user_id,
                display_name,
            )?;
        }
        tx.commit()?;
        Ok(updated)
    };
    let updated = update().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "updated": updated })))
}

pub async fn delete_trusted_user(
    State(state): State<AppState>,
    Path((_agent_id, user_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let deleted = opencrab_db::queries::remove_trusted_user(&conn, &user_id).unwrap_or(false);
    Json(serde_json::json!({ "deleted": deleted }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_db::queries::{
        TRUSTED_PLATFORM_DISCORD, TRUSTED_PLATFORM_REST, TRUSTED_PLATFORM_WEB,
    };

    fn req(user_id: &str, platform: Option<&str>) -> AddTrustedUserRequest {
        AddTrustedUserRequest {
            user_id: user_id.to_string(),
            permission: None,
            display_name: None,
            platform: platform.map(str::to_string),
        }
    }

    /// 権限を明示する登録リクエスト。
    fn req_with_permission(
        user_id: &str,
        platform: &str,
        permission: &str,
    ) -> AddTrustedUserRequest {
        AddTrustedUserRequest {
            user_id: user_id.to_string(),
            permission: Some(permission.to_string()),
            display_name: Some("Crab B".to_string()),
            platform: Some(platform.to_string()),
        }
    }

    /// 経路を省略した登録は従来どおり `discord`（#214 以前のリクエストが動き続ける）。
    #[tokio::test]
    async fn platform_defaults_to_discord() {
        let state = crate::test_app_state();
        let dto = add_trusted_user(
            State(state.clone()),
            Path("agent-1".to_string()),
            Json(req("42", None)),
        )
        .await
        .expect("add")
        .0;
        assert_eq!(dto.platform, TRUSTED_PLATFORM_DISCORD);

        let conn = state.db.lock().unwrap();
        assert!(opencrab_db::queries::get_trusted_user(
            &conn,
            TRUSTED_PLATFORM_DISCORD,
            "42",
            "agent-1"
        )
        .is_some());
    }

    /// 経路を指定すればその経路の行になる。legacy `rest` も既存行との互換用に受け付ける。
    #[tokio::test]
    async fn platform_is_taken_from_the_request() {
        let state = crate::test_app_state();
        for (platform, user_id) in [
            (TRUSTED_PLATFORM_WEB, "dash-user"),
            (TRUSTED_PLATFORM_REST, "rest-user"),
        ] {
            let dto = add_trusted_user(
                State(state.clone()),
                Path("agent-1".to_string()),
                Json(req(user_id, Some(platform))),
            )
            .await
            .expect("add")
            .0;
            assert_eq!(dto.platform, platform);

            let conn = state.db.lock().unwrap();
            assert!(
                opencrab_db::queries::get_trusted_user(&conn, platform, user_id, "agent-1")
                    .is_some()
            );
            // 他経路へは漏れない。
            assert!(opencrab_db::queries::get_trusted_user(
                &conn,
                TRUSTED_PLATFORM_DISCORD,
                user_id,
                "agent-1"
            )
            .is_none());
        }
    }

    /// 未定義の経路は弾く（登録できても誰とも一致しない行を作らせない）。
    #[tokio::test]
    async fn unknown_platform_is_rejected() {
        let state = crate::test_app_state();
        // `nostr` は #319 で読み出し側が引く経路になったので、ここには置けない。
        for bad in ["mastodon", "Nostr", "Web", " web", ""] {
            let err = add_trusted_user(
                State(state.clone()),
                Path("agent-1".to_string()),
                Json(req("42", Some(bad))),
            )
            .await
            .expect_err("unknown platform");
            assert_eq!(err, StatusCode::BAD_REQUEST, "{bad:?}");
        }
        let conn = state.db.lock().unwrap();
        assert!(opencrab_db::queries::list_trusted_users(&conn, "agent-1")
            .unwrap()
            .is_empty());
    }

    /// 一意制約はまだ `(user_id, agent_id)`（#159 に残した非可逆な変更）。
    /// 同じ識別子を別経路で二重に持てないことを、500 ではなく 409 で示す。
    #[tokio::test]
    async fn same_identifier_on_another_platform_conflicts_until_the_constraint_is_rebuilt() {
        let state = crate::test_app_state();
        let _first = add_trusted_user(
            State(state.clone()),
            Path("agent-1".to_string()),
            Json(req("42", Some(TRUSTED_PLATFORM_DISCORD))),
        )
        .await
        .expect("first add");

        let err = add_trusted_user(
            State(state.clone()),
            Path("agent-1".to_string()),
            Json(req("42", Some(TRUSTED_PLATFORM_WEB))),
        )
        .await
        .expect_err("unique violation");
        assert_eq!(err, StatusCode::CONFLICT);
    }

    /// 一覧は経路を出す（どの行がどの経路で効くのか運用者が見分けられる）。
    #[tokio::test]
    async fn list_exposes_the_platform_of_each_row() {
        let state = crate::test_app_state();
        let _added = add_trusted_user(
            State(state.clone()),
            Path("agent-1".to_string()),
            Json(req("dash-user", Some(TRUSTED_PLATFORM_WEB))),
        )
        .await
        .expect("add");

        let rows = list_trusted_users(State(state.clone()), Path("agent-1".to_string()))
            .await
            .0;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].platform, TRUSTED_PLATFORM_WEB);
        // 経路で絞らない一覧であることは維持（運用者は全経路を見られる）。
        assert_eq!(rows[0].user_id, "dash-user");
    }

    // ---- nostr の書き込み正規化（#319） ----

    /// **非正規表現（大文字 hex / 大文字 npub / 前後空白）で登録しても、canonical
    /// 小文字 hex で保存され、hex の発言者で読み出しに引き当たる。**
    ///
    /// 読み出し側は canonical hex / npub で exact-match するため、正規化せず素通しで
    /// 保存すると（この修正前の挙動）その信頼ユーザーが受信ターンで一致しない。
    #[cfg(feature = "nostr")]
    #[tokio::test]
    async fn nostr_user_id_is_normalized_on_write_and_matches_the_speaker() {
        use opencrab_db::queries::TRUSTED_PLATFORM_NOSTR;
        // ダミー鍵（実在の pubkey は書かない）。
        const HEX: &str = "0000000000000000000000000000000000000000000000000000000000000009";
        let npub = opencrab_nostr::to_npub(HEX).unwrap();
        for raw in [
            format!("  {}\n", HEX.to_ascii_uppercase()),
            format!(" {} ", npub.to_ascii_uppercase()),
        ] {
            let state = crate::test_app_state();
            let dto = add_trusted_user(
                State(state.clone()),
                Path("agent-1".to_string()),
                Json(req(&raw, Some(TRUSTED_PLATFORM_NOSTR))),
            )
            .await
            .expect("add")
            .0;
            // 保存形は canonical 小文字 hex。
            assert_eq!(dto.user_id, HEX, "非正規 {raw:?} が正規化されていない");
            // 受信ターンの発言者解決（読み出し側）で hex の発言者に一致する。
            let conn = state.db.lock().unwrap();
            assert_eq!(
                crate::nostr_runner_impl::resolve_nostr_caller_identity(&conn, "agent-1", HEX),
                opencrab_actions::CallerIdentity::TrustedUser,
                "非正規 {raw:?} で登録した nostr 信頼ユーザーが読み出しで一致しない"
            );
        }
    }

    /// 正規化できない nostr の識別子は 400（誰とも一致しない行を作らせない）。
    /// 他経路は素通しなので、この検証は `platform='nostr'` のときだけ効く。
    #[tokio::test]
    async fn malformed_nostr_user_id_is_rejected() {
        use opencrab_db::queries::TRUSTED_PLATFORM_NOSTR;
        let state = crate::test_app_state();
        for bad in ["not-a-key", "npub1broken", "abcd", ""] {
            let err = add_trusted_user(
                State(state.clone()),
                Path("agent-1".to_string()),
                Json(req(bad, Some(TRUSTED_PLATFORM_NOSTR))),
            )
            .await
            .expect_err("malformed nostr id");
            assert_eq!(err, StatusCode::BAD_REQUEST, "{bad:?}");
        }
        // 弾かれた登録は 1 行も残らない。
        let conn = state.db.lock().unwrap();
        assert!(opencrab_db::queries::list_trusted_users(&conn, "agent-1")
            .unwrap()
            .is_empty());
    }

    // ---- 権限の表記（#234） ----

    /// 権限を省略した登録は `user`（従来の既定と同じ）。
    #[tokio::test]
    async fn permission_defaults_to_user() {
        let state = crate::test_app_state();
        let dto = add_trusted_user(
            State(state.clone()),
            Path("agent-1".to_string()),
            Json(req("42", None)),
        )
        .await
        .expect("add")
        .0;
        assert_eq!(dto.permission, TrustedUserPermission::User);
    }

    /// **未知の権限は 400。** ここが検証していなかったのが #234 の直接原因。
    /// 旧いアンダースコア表記も通さない（通す表記をひとつに保つ）。
    #[tokio::test]
    async fn unknown_permission_is_rejected() {
        let state = crate::test_app_state();
        for bad in ["co_agent", "coagent", "CoAgent", "trusted", "admin", ""] {
            let err = add_trusted_user(
                State(state.clone()),
                Path("agent-1".to_string()),
                Json(req_with_permission("42", TRUSTED_PLATFORM_WEB, bad)),
            )
            .await
            .expect_err("unknown permission");
            assert_eq!(err, StatusCode::BAD_REQUEST, "{bad:?}");
        }
        // 弾かれた登録は 1 行も残らない（「登録できたのに効かない」行を作らせない）。
        let conn = state.db.lock().unwrap();
        assert!(opencrab_db::queries::list_trusted_users(&conn, "agent-1")
            .unwrap()
            .is_empty());
    }

    /// 更新も同じ入口で弾く。弾かれたら**元の権限のまま**（部分適用しない）。
    #[tokio::test]
    async fn unknown_permission_is_rejected_on_update() {
        let state = crate::test_app_state();
        let dto = add_trusted_user(
            State(state.clone()),
            Path("agent-1".to_string()),
            Json(req_with_permission("42", TRUSTED_PLATFORM_WEB, "co-agent")),
        )
        .await
        .expect("add")
        .0;

        let err = update_trusted_user(
            State(state.clone()),
            Path(("agent-1".to_string(), dto.id.clone())),
            Json(UpdateTrustedUserRequest {
                permission: Some("co_agent".to_string()),
                display_name: None,
            }),
        )
        .await
        .expect_err("unknown permission");
        assert_eq!(err, StatusCode::BAD_REQUEST);

        let rows = list_trusted_users(State(state.clone()), Path("agent-1".to_string()))
            .await
            .0;
        assert_eq!(rows[0].permission, TrustedUserPermission::CoAgent);
    }

    /// **ダッシュボードから協働エージェントとして登録した行が実際に機能すること**（#234 の本題）。
    ///
    /// UI が送る表記（`co-agent`）で登録した行が、
    /// - 呼び出し元の判定で `CoAgent` になり、
    /// - 相互レビューの名簿に載る。
    ///
    /// 表記が食い違っていた頃は、どちらも「ただの信頼済みユーザー」に落ちていた。
    #[tokio::test]
    async fn co_agent_registered_from_the_dashboard_actually_works() {
        let state = crate::test_app_state();
        let dto = add_trusted_user(
            State(state.clone()),
            Path("agent-1".to_string()),
            Json(req_with_permission(
                "dash-user",
                TRUSTED_PLATFORM_WEB,
                "co-agent",
            )),
        )
        .await
        .expect("add")
        .0;
        // 応答のキーは据え置き、値はケバブケース。
        assert_eq!(dto.permission, TrustedUserPermission::CoAgent);
        assert_eq!(
            serde_json::to_value(&dto).unwrap()["permission"],
            serde_json::json!("co-agent")
        );

        let conn = state.db.lock().unwrap();
        // 呼び出し元の判定
        assert_eq!(
            crate::caller_identity::resolve_caller_identity(
                &conn,
                TRUSTED_PLATFORM_WEB,
                "dash-user",
                "agent-1",
            ),
            opencrab_actions::CallerIdentity::CoAgent {
                agent_id: "dash-user".to_string()
            }
        );
        // 相互レビューの名簿
        let roster =
            opencrab_db::queries::list_co_agent_reviewers(&conn, TRUSTED_PLATFORM_WEB, "agent-1")
                .unwrap();
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].user_id, "dash-user");
    }
}
