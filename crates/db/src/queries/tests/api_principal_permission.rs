// ---- 権限の表記（列挙型, #234） ----

/// 表記ゆれが型で起こりえないこと: DB に入る文字列は列挙型からしか作れず、
/// **全 variant がケバブケース**で、読み書きが往復する。
#[test]
fn permission_spelling_cannot_drift() {
    for p in API_PRINCIPAL_PERMISSIONS {
        let s = p.as_db_str();
        // アンダースコア表記は存在しない（#234 の食い違いはこれで起きた）。
        assert!(!s.contains('_'), "{s} はケバブケースでない");
        // 書いた表記はそのまま読み戻せる。
        assert_eq!(ApiPrincipalPermission::parse(s), Some(p));
        assert_eq!(ApiPrincipalPermission::from_db_str(s), p);
        // serde 表現（API の応答 / 設定側の CommandPermission と同じ規約）も同じ文字列。
        assert_eq!(serde_json::to_string(&p).unwrap(), format!("\"{s}\""));
    }
    // 表記は 3 つで全部（増えたらここが落ちる）。
    assert_eq!(
        API_PRINCIPAL_PERMISSIONS.map(|p| p.as_db_str()),
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
        assert_eq!(ApiPrincipalPermission::parse(bad), None, "{bad:?}");
        assert_eq!(
            ApiPrincipalPermission::from_db_str(bad),
            ApiPrincipalPermission::User,
            "{bad:?}"
        );
    }
}

/// 既定は `user`（登録 API の既定と揃っていること）。
#[test]
fn permission_defaults_to_user() {
    assert_eq!(
        ApiPrincipalPermission::default(),
        ApiPrincipalPermission::User
    );
}
