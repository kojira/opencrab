use super::*;

#[test]
fn test_trusted_user_display_name_round_trip() {
    let conn = setup();
    add_trusted_user(
        &conn,
        TRUSTED_PLATFORM_DISCORD,
        "id-1",
        "a1",
        "42",
        TrustedUserPermission::CoAgent,
        "owner",
        "2026-01-01",
        "Crab B",
    )
    .unwrap();
    let row = get_trusted_user(&conn, TRUSTED_PLATFORM_DISCORD, "42", "a1").unwrap();
    assert_eq!(row.display_name, "Crab B");
    assert_eq!(row.permission, TrustedUserPermission::CoAgent);

    assert!(update_trusted_user_display_name(&conn, "id-1", "Crab B2").unwrap());
    let rows = list_trusted_users(&conn, "a1").unwrap();
    assert_eq!(rows[0].display_name, "Crab B2");

    // v3 以前の行（display_name / platform とも列 DEFAULT）も読み出せる
    conn.execute(
        "INSERT INTO trusted_users (id, user_id, agent_id, permission, created_by, created_at) \
         VALUES ('id-2', '43', 'a1', 'user', 'owner', '2026-01-01')",
        [],
    )
    .unwrap();
    let row = get_trusted_user(&conn, TRUSTED_PLATFORM_DISCORD, "43", "a1").unwrap();
    assert_eq!(row.display_name, "");
    // 列追加前からある行は従来の経路（discord）として生きる（#214）
    assert_eq!(row.platform, TRUSTED_PLATFORM_DISCORD);
}

// ---- 経路（identity platform）で識別子空間が分かれること（#214） ----

/// 1 件登録するテストヘルパ。
fn add_trusted(conn: &Connection, platform: &str, row_id: &str, user_id: &str, agent_id: &str) {
    add_trusted_user(
        conn,
        platform,
        row_id,
        agent_id,
        user_id,
        TrustedUserPermission::User,
        "owner",
        "2026-01-01",
        "",
    )
    .unwrap();
}

/// 同じ識別子でも経路が違えば別扱い（信頼が経路をまたいで引き継がれない）。
#[test]
fn trust_does_not_cross_platforms() {
    let conn = setup();
    // Discord 経路に "42" を登録する。
    add_trusted(&conn, TRUSTED_PLATFORM_DISCORD, "row-d", "42", "a1");
    assert!(is_trusted_user(&conn, TRUSTED_PLATFORM_DISCORD, "42", "a1"));
    // 同じ文字列を web / REST の識別子として名乗っても、その経路では信頼されない。
    assert!(!is_trusted_user(&conn, TRUSTED_PLATFORM_WEB, "42", "a1"));
    assert!(!is_trusted_user(&conn, TRUSTED_PLATFORM_REST, "42", "a1"));
    assert!(get_trusted_user(&conn, TRUSTED_PLATFORM_WEB, "42", "a1").is_none());

    // 逆向きも同じ: web 経路の登録は Discord 経路へ漏れない。
    add_trusted(&conn, TRUSTED_PLATFORM_WEB, "row-w", "dash-user", "a1");
    assert!(is_trusted_user(
        &conn,
        TRUSTED_PLATFORM_WEB,
        "dash-user",
        "a1"
    ));
    assert!(!is_trusted_user(
        &conn,
        TRUSTED_PLATFORM_DISCORD,
        "dash-user",
        "a1"
    ));
}

/// 登録件数の判定も経路で切られている
/// （ある経路に登録があっても、別経路から見れば「0 件」）。
#[test]
fn trusted_user_count_is_scoped_by_platform() {
    let conn = setup();
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_DISCORD, "a1"), 0);

    add_trusted(&conn, TRUSTED_PLATFORM_WEB, "row-w", "dash-user", "a1");
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_WEB, "a1"), 1);
    // web に 1 件あっても Discord から見れば未登録（= owner のみ許可の段が生きる）。
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_DISCORD, "a1"), 0);
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_REST, "a1"), 0);

    add_trusted(&conn, TRUSTED_PLATFORM_DISCORD, "row-d", "42", "a1");
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_DISCORD, "a1"), 1);
    // エージェントでも切れている
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_DISCORD, "a2"), 0);
}

/// 互換読みの撤去（#159）で**何が変わったか**を明示する。
///
/// 撤去前: 従来経路（`discord`）の行しか無いユーザーも web / REST で信頼されていた。
/// 撤去後: 自経路の行が無ければ引けない = そのユーザーは web / REST で権限を失う。
/// ここが緑のままなら、互換読みが別名で復活していないということ。
#[test]
fn legacy_discord_rows_no_longer_grant_trust_on_other_platforms() {
    let conn = setup();
    add_trusted(&conn, TRUSTED_PLATFORM_DISCORD, "row-d", "42", "a1");

    // 従来経路の行は自経路（discord）でだけ効く。
    assert!(get_trusted_user(&conn, TRUSTED_PLATFORM_DISCORD, "42", "a1").is_some());
    // web / REST から同じ識別子で来ても引けない（＝移行前のユーザーは信頼を失う）。
    assert!(get_trusted_user(&conn, TRUSTED_PLATFORM_WEB, "42", "a1").is_none());
    assert!(get_trusted_user(&conn, TRUSTED_PLATFORM_REST, "42", "a1").is_none());

    // 経路ごとの行を登録し直せば、その経路でだけ信頼が戻る。
    add_trusted(&conn, TRUSTED_PLATFORM_WEB, "row-w", "dash-user", "a1");
    let own = get_trusted_user(&conn, TRUSTED_PLATFORM_WEB, "dash-user", "a1").expect("web row");
    assert_eq!(own.platform, TRUSTED_PLATFORM_WEB);
    assert!(get_trusted_user(&conn, TRUSTED_PLATFORM_DISCORD, "dash-user", "a1").is_none());
}

/// 登録 API が受け付ける経路の集合＝読み出し側が引く経路の集合。
///
/// `nostr` は #319 で読み出し側（Nostr 受信ターンの呼び出し元解決）が引くように
/// なったので、登録 API も受け付ける。
#[test]
fn known_platforms_are_exactly_the_read_paths() {
    assert!(is_known_trusted_platform(TRUSTED_PLATFORM_DISCORD));
    assert!(is_known_trusted_platform(TRUSTED_PLATFORM_WEB));
    assert!(is_known_trusted_platform(TRUSTED_PLATFORM_REST));
    assert!(is_known_trusted_platform(TRUSTED_PLATFORM_NOSTR));
    assert!(is_known_trusted_platform(TRUSTED_PLATFORM_EXTGATE));
    // 綴り間違い・未定義の経路は弾く（登録できても誰とも一致しない行になるため）。
    assert!(!is_known_trusted_platform("Discord"));
    assert!(!is_known_trusted_platform("Nostr"));
    assert!(!is_known_trusted_platform("mastodon"));
    assert!(!is_known_trusted_platform(""));
}

/// ロスターも経路で切られている（#159: 受理ゲートと揃えた）。
#[test]
fn co_agent_roster_is_scoped_by_platform() {
    let conn = setup();
    add_trusted_user(
        &conn,
        TRUSTED_PLATFORM_DISCORD,
        "row-d",
        "a1",
        "42",
        TrustedUserPermission::CoAgent,
        "owner",
        "2026-01-01",
        "Crab D",
    )
    .unwrap();
    add_trusted_user(
        &conn,
        TRUSTED_PLATFORM_WEB,
        "row-w",
        "a1",
        "dash-user",
        TrustedUserPermission::CoAgent,
        "owner",
        "2026-01-01",
        "Crab W",
    )
    .unwrap();

    let discord = list_co_agent_reviewers(&conn, TRUSTED_PLATFORM_DISCORD, "a1").unwrap();
    assert_eq!(discord.len(), 1);
    assert_eq!(discord[0].display_name, "Crab D");

    let web = list_co_agent_reviewers(&conn, TRUSTED_PLATFORM_WEB, "a1").unwrap();
    assert_eq!(web.len(), 1);
    assert_eq!(web[0].display_name, "Crab W");

    assert!(list_co_agent_reviewers(&conn, TRUSTED_PLATFORM_REST, "a1")
        .unwrap()
        .is_empty());

    // permission と agent_id の絞り込みは維持されている。
    add_trusted(&conn, TRUSTED_PLATFORM_DISCORD, "row-u", "43", "a1");
    assert_eq!(
        list_co_agent_reviewers(&conn, TRUSTED_PLATFORM_DISCORD, "a1")
            .unwrap()
            .len(),
        1
    );
    assert!(
        list_co_agent_reviewers(&conn, TRUSTED_PLATFORM_DISCORD, "a2")
            .unwrap()
            .is_empty()
    );
}

// ---- 権限の表記（列挙型, #234） ----

/// 表記ゆれが型で起こりえないこと: DB に入る文字列は列挙型からしか作れず、
/// **全 variant がケバブケース**で、読み書きが往復する。
#[test]
fn permission_spelling_cannot_drift() {
    for p in TRUSTED_USER_PERMISSIONS {
        let s = p.as_db_str();
        // アンダースコア表記は存在しない（#234 の食い違いはこれで起きた）。
        assert!(!s.contains('_'), "{s} はケバブケースでない");
        // 書いた表記はそのまま読み戻せる。
        assert_eq!(TrustedUserPermission::parse(s), Some(p));
        assert_eq!(TrustedUserPermission::from_db_str(s), p);
        // serde 表現（API の応答 / 設定側の CommandPermission と同じ規約）も同じ文字列。
        assert_eq!(serde_json::to_string(&p).unwrap(), format!("\"{s}\""));
    }
    // 表記は 3 つで全部（増えたらここが落ちる）。
    assert_eq!(
        TRUSTED_USER_PERMISSIONS.map(|p| p.as_db_str()),
        ["owner", "user", "co-agent"]
    );
}

/// 未知の表記は**入口で通らない**。かつて寛容に受け入れていた綴りも通らない。
/// 読み出しは最小権限（`user`）へ倒れる（fail-closed、行の判定は従来と同じ）。
#[test]
fn unknown_permission_spellings_are_rejected_at_the_gate() {
    for bad in [
        "co_agent", "coagent", "CoAgent", "Owner", "trusted", "", " user",
    ] {
        assert_eq!(TrustedUserPermission::parse(bad), None, "{bad:?}");
        assert_eq!(
            TrustedUserPermission::from_db_str(bad),
            TrustedUserPermission::User,
            "{bad:?}"
        );
    }
}

/// 既定は `user`（登録 API の既定と揃っていること）。
#[test]
fn permission_defaults_to_user() {
    assert_eq!(
        TrustedUserPermission::default(),
        TrustedUserPermission::User
    );
}

/// 選択肢の定義が 2 箇所に分かれてドリフトしないこと（#234）。
///
/// ダッシュボードの `TRUSTED_USER_PERMISSIONS` はこの列挙型の写しでしかない。
/// 独立した文字列配列だった頃、UI は `co-agent`・判定は `co_agent` で、
/// **ダッシュボードからの登録が黙って無効**になっていた。片方だけ変えたらここが落ちる。
#[test]
fn dashboard_permission_options_match_the_enum() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../web/src/api/trusted_users.ts"
    );
    let src = std::fs::read_to_string(path).expect("ダッシュボードの API 定義を読めること");
    let (_, rest) = src
        .split_once("export const TRUSTED_USER_PERMISSIONS = [")
        .expect("TRUSTED_USER_PERMISSIONS の定義があること");
    let (list, _) = rest.split_once(']').expect("配列が閉じていること");
    let from_dashboard: Vec<String> = list
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let from_enum: Vec<String> = TRUSTED_USER_PERMISSIONS
        .iter()
        .map(|p| p.as_db_str().to_string())
        .collect();
    assert_eq!(from_dashboard, from_enum);
}
