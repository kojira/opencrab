use super::super::*;
use super::support::*;
use opencrab_gateway::GatewayCaller;

// ================================================================================
// #157 S6: スキル生成（create_skill）の移植テスト
//
// 旧 Discord 実装（`crates/discord` の `gateway_actions/agent_management.rs`）にあった
// 3 テスト（基本 / 同名 dedup / 非 trusted 拒否）をそのまま持ってきたもの（1 件も
// 落としていない）＋ 移設の本題（非 Discord 構成でも定義に現れる）・inner へ委譲
// しないこと・レスポンス JSON / エラー文言 / `source_type` のリテラル固定。
// ================================================================================

fn co_agent_ctx() -> GatewayCallContext {
    GatewayCallContext::new(
        GatewayCaller::CoAgent {
            agent_id: "agent-peer".to_string(),
        },
        "agent-x",
    )
}

/// DB 上のスキル（アーカイブ済みも含む）を取得する。
fn db_skill(state: &AppState, name: &str) -> Option<opencrab_db::queries::SkillRow> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::find_skill_by_name_any(&conn, "agent-x", name).unwrap()
}

/// **#157 S6 の本題**: `create_skill` が own 定義にちょうど 1 件ある。
///
/// own 定義は transport の有無に依存しないため、これが `definitions()` に出ることは
/// 「web / Nostr / REST / heartbeat でも使える」ことと同義。own から消えると Discord
/// 専用に逆戻りする（それが #157 が報告している不具合そのもの）。
#[test]
fn create_skill_is_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    assert_eq!(
        defs.iter().filter(|d| d.name == "create_skill").count(),
        1,
        "create_skill は own 定義にちょうど 1 件必要（#157 S6）"
    );
}

/// **Discord 無効の構成でも露出する**（#157 S6 の証明）。
///
/// `inner = None` は「transport 固有 gateway が居ない」経路（web / REST / Nostr /
/// heartbeat、および Discord feature 無効ビルド）そのもの。移設前はこの構成で
/// `create_skill` が一切出なかった。
#[test]
fn create_skill_is_exposed_without_any_transport_gateway() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    assert!(
            names.contains(&"create_skill".to_string()),
            "transport gateway 無しの構成で create_skill が露出しない（#157 の不具合そのもの）: {names:?}"
        );
}

/// 定義（description / 引数スキーマ）を移設前（Discord 定義）と 1 バイトも変えない。
///
/// description は LLM がツールを選ぶ唯一の手がかりなので、文言が変わると挙動が変わる。
#[test]
fn create_skill_definition_matches_the_discord_original() {
    let defs = SystemGatewayActions::own_definitions();
    let d = defs.iter().find(|d| d.name == "create_skill").unwrap();
    assert_eq!(
            d.description,
            "ユーザーから「〇〇するスキルを作って」と言われたとき新しいスキルを作成する。guidanceにコマンド例・使い方を書くことで、LLMがexecute_shellで動的に実行できるようになる。同名スキルが存在する場合は更新される。"
        );
    assert_eq!(d.parameters["type"], json!("object"));
    assert_eq!(d.parameters["required"], json!(["name", "description"]));
    let props = d.parameters["properties"].as_object().unwrap();
    let mut keys: Vec<&str> = props.keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["description", "guidance", "name"]);
    for k in ["name", "description", "guidance"] {
        assert_eq!(props[k]["type"], json!("string"), "{k}");
    }
    assert_eq!(props["name"]["description"], json!("スキル名"));
    assert_eq!(props["description"]["description"], json!("スキルの説明"));
    assert_eq!(
        props["guidance"]["description"],
        json!("スキルのガイダンス（省略時は空文字列）")
    );
}

/// **inner へ委譲されない**（own が唯一の実装）。
///
/// 委譲パターンのまま残すと、Discord が誤って再定義したときに own の実装が黙って
/// バイパスされる（#155 の後退）。
#[tokio::test]
async fn create_skill_is_not_delegated_to_inner() {
    let state = crate::test_app_state();
    let inner = Arc::new(RecordingInner::new(&["create_skill"]));
    let actions = SystemGatewayActions::new(state.clone(), Some(inner.clone()), None, None);

    let r = actions
        .execute(
            "create_skill",
            &json!({"name": "天気確認", "description": "curl wttr.in で天気を確認する"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert!(
        inner.calls().is_empty(),
        "create_skill が inner へ委譲された: {:?}",
        inner.calls()
    );
    // own の実装が実際に走った証拠（inner のフェイクは DB を触らない）。
    assert!(db_skill(&state, "天気確認").is_some());
}

/// 移植: 基本の作成。レスポンス JSON のキーと `action` の値、DB に書く
/// `source_type` / `permission` / `situation_pattern` をリテラルで固定する。
#[tokio::test]
async fn create_skill_basic() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let result = actions
        .execute(
            "create_skill",
            &json!({
                "name": "天気確認",
                "description": "curl wttr.inで天気を確認する"
            }),
            &owner_ctx(),
        )
        .await;
    assert!(result.success, "create_skill should succeed");
    let data = result.data.unwrap();
    assert!(data["id"].is_string(), "should return id");
    assert_eq!(data["name"], json!("天気確認"));
    assert_eq!(data["action"], json!("created"));
    let mut keys: Vec<&str> = data
        .as_object()
        .unwrap()
        .keys()
        .map(|s| s.as_str())
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["action", "id", "name"]);

    // 記録される取得元（`source_type`）を移設で変えない。core の `create_my_skill` は
    // `"self_created"` を書く**別のツール**（#157 では統廃合しない）。
    let row = db_skill(&state, "天気確認").unwrap();
    assert_eq!(row.source_type, "acquired");
    assert_eq!(row.permission, "\"agent\"");
    assert_eq!(row.situation_pattern, "");
    assert_eq!(row.guidance, "", "guidance 省略時は空文字列");
    assert!(row.is_active);
    assert!(!row.archived);
}

/// 移植: 同名は upsert（`action="updated"`。行は増えない）。
#[tokio::test]
async fn create_skill_dedup() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let first = actions
        .execute(
            "create_skill",
            &json!({
                "name": "天気確認",
                "description": "first version"
            }),
            &owner_ctx(),
        )
        .await;
    assert!(first.success);
    let result2 = actions
        .execute(
            "create_skill",
            &json!({
                "name": "天気確認",
                "description": "updated version",
                "guidance": "curl wttr.in"
            }),
            &owner_ctx(),
        )
        .await;
    assert!(result2.success, "second create should succeed (dedup)");
    let data = result2.data.unwrap();
    assert_eq!(data["action"], json!("updated"));
    // 同じ行が更新される（id 不変・description / guidance だけ差し替わる）。
    assert_eq!(data["id"], first.data.unwrap()["id"]);
    let row = db_skill(&state, "天気確認").unwrap();
    assert_eq!(row.description, "updated version");
    assert_eq!(row.guidance, "curl wttr.in");
    let conn = state.db.lock().unwrap();
    let all = opencrab_db::queries::list_skills(&conn, "agent-x", false).unwrap();
    assert_eq!(all.len(), 1, "同名で行が増えてはならない");
}

/// アーカイブ済みの同名スキルは復活する（`action="restored"` / archived=false）。
#[tokio::test]
async fn create_skill_restores_archived_skill() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    assert!(
        actions
            .execute(
                "create_skill",
                &json!({"name": "天気確認", "description": "v1"}),
                &owner_ctx(),
            )
            .await
            .success
    );
    {
        let conn = state.db.lock().unwrap();
        let mut row = opencrab_db::queries::find_skill_by_name_any(&conn, "agent-x", "天気確認")
            .unwrap()
            .unwrap();
        row.archived = true;
        row.is_active = false;
        opencrab_db::queries::update_skill(&conn, &row).unwrap();
    }

    let r = actions
        .execute(
            "create_skill",
            &json!({"name": "天気確認", "description": "v2"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(r.data.unwrap()["action"], json!("restored"));
    let row = db_skill(&state, "天気確認").unwrap();
    assert!(!row.archived);
    assert!(row.is_active);
    assert_eq!(row.description, "v2");
}

/// 移植: 非 trusted（素の Agent）は拒否。**エラー文言はバイト単位で移設前と同一。**
///
/// このゲートは**二重構造**である: bridge の `TRUSTED_ONLY_ACTIONS` が可視性と実行の
/// 双方を（名前ベースで）ゲートし、ハンドラ内の `matches!` が多層防御として残る。
/// bridge 側は名前で引くので移設しても効き続ける（そのことをここで固定する）。
/// なお**ハンドラ側の拒否はマーカー無し**（`REJECTION_CODE_PREFIX` を付けない）で、
/// これも移設前と同じ形。
#[tokio::test]
async fn create_skill_rejected_for_non_owner() {
    assert!(
        opencrab_actions::TRUSTED_ONLY_ACTIONS.contains(&"create_skill"),
        "bridge 側の trusted ゲートが消えたら、ハンドラ内検査が唯一のゲートになる"
    );

    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let result = actions
        .execute(
            "create_skill",
            &json!({
                "name": "test",
                "description": "test"
            }),
            &agent_ctx(),
        )
        .await;
    assert!(!result.success);
    let err = result.error.unwrap();
    assert!(err.contains("trusted user"));
    assert_eq!(
        err, "このアクションはtrusted userのみ実行できます",
        "拒否文言は移設前と 1 バイトも変えない"
    );
    assert!(
        !err.starts_with(REJECTION_CODE_PREFIX),
        "マーカーの有無も移設前と同じ（付けない）"
    );
    // 副作用ゼロ。
    assert!(db_skill(&state, "test").is_none());
}

/// trusted_user / co_agent は実行できる（許可集合を移設で狭めない）。
#[tokio::test]
async fn create_skill_allowed_for_trusted_user_and_co_agent() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    for (i, ctx) in [trusted_ctx(), co_agent_ctx()].into_iter().enumerate() {
        let name = format!("skill-{i}");
        let r = actions
            .execute(
                "create_skill",
                &json!({"name": name, "description": "d"}),
                &ctx,
            )
            .await;
        assert!(
            r.success,
            "{:?} は実行できるべき: {:?}",
            ctx.caller, r.error
        );
        assert!(db_skill(&state, &name).is_some());
    }
}

/// 必須引数エラーの文言（英語のまま・マーカー無し）を固定する。
#[tokio::test]
async fn create_skill_missing_arguments_keep_original_messages() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);

    let r = actions
        .execute("create_skill", &json!({}), &owner_ctx())
        .await;
    assert!(!r.success);
    assert_eq!(r.error.as_deref(), Some("name is required"));

    let r = actions
        .execute("create_skill", &json!({"name": "n"}), &owner_ctx())
        .await;
    assert!(!r.success);
    assert_eq!(r.error.as_deref(), Some("description is required"));
}

/// 分類の所属を移設で変えない（Discord でも dispatchable だった）。分類の権威は
/// `own_definitions()` の `class.dispatch` 属性なので、それを直接見る。
#[test]
fn create_skill_stays_dispatchable() {
    use opencrab_gateway::DispatchMode;
    let defs = SystemGatewayActions::own_definitions();
    let class = defs
        .iter()
        .find(|d| d.name == "create_skill")
        .expect("create_skill が own_definitions() に無い")
        .class;
    assert_eq!(
        class.dispatch,
        DispatchMode::Dispatchable,
        "create_skill は移設前と同じく dispatch 対象に残す（結果を同ターンで使わない）"
    );
}
