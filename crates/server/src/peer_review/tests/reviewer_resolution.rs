    // ---- レビュアー解決（幻覚 id への誤送信防止） ----

    #[test]
    fn resolve_reviewer_registered_only() {
        let conn = opencrab_db::init_memory().unwrap();
        let delivery = FakeDelivery::new();
        opencrab_db::queries::add_trusted_user(
            &conn,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
            "row-1",
            "agent-1",
            "42",
            TrustedUserPermission::CoAgent,
            "owner",
            "2026-01-01",
            "Crab B",
        )
        .unwrap();
        // 数値の display_name（id 解釈に食われないこと）
        opencrab_db::queries::add_trusted_user(
            &conn,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
            "row-2",
            "agent-1",
            "77",
            TrustedUserPermission::CoAgent,
            "owner",
            "2026-01-01",
            "2026",
        )
        .unwrap();
        // co_agent でない行はロスター外
        opencrab_db::queries::add_trusted_user(
            &conn,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
            "row-3",
            "agent-1",
            "44",
            TrustedUserPermission::User,
            "owner",
            "2026-01-01",
            "Human",
        )
        .unwrap();

        // display_name 一致（大文字小文字無視）が最優先
        assert_eq!(
            resolve_reviewer(&conn, &delivery, "agent-1", "crab b").unwrap(),
            "42"
        );
        assert_eq!(
            resolve_reviewer(&conn, &delivery, "agent-1", "2026").unwrap(),
            "77"
        );
        // 登録済み id / <@id> 形式
        assert_eq!(
            resolve_reviewer(&conn, &delivery, "agent-1", "42").unwrap(),
            "42"
        );
        assert_eq!(
            resolve_reviewer(&conn, &delivery, "agent-1", "<@42>").unwrap(),
            "42"
        );
        // 未登録の任意 id は拒否（幻覚 id のゴーストメンション防止）
        let err = resolve_reviewer(&conn, &delivery, "agent-1", "999").unwrap_err();
        assert!(err.contains("Crab B"));
        // 一覧のメンション記法は transport が組む
        assert!(err.contains("Crab B (<@42>)"), "{err}");
        // 非 co_agent はロスター外
        let err = resolve_reviewer(&conn, &delivery, "agent-1", "Human").unwrap_err();
        assert!(err.contains("Crab B"));
        assert!(!err.contains("Human"));
    }

    /// #159: 名簿の経路と受理ゲートの経路が一致していること。
    ///
    /// ずれると「依頼は飛ぶが返信を受理されない」相手を指名できてしまう。
    /// `TranscriptSource` に由来が増えたら下の `match` が非網羅でコンパイルできず、
    /// 名簿側（[`REVIEWER_PLATFORM`]）の見直しを強制する。
    #[test]
    fn roster_platform_matches_the_harvestable_platforms() {
        let all = [
            TranscriptSource::Discord,
            TranscriptSource::Nostr,
            TranscriptSource::External,
        ];
        for source in all {
            let expected = match source {
                TranscriptSource::Discord => Some(REVIEWER_PLATFORM),
                TranscriptSource::Nostr => None,
                TranscriptSource::External => None,
            };
            assert_eq!(trusted_platform_for(source), expected, "{source:?}");
        }
        let harvestable: std::collections::BTreeSet<&str> =
            all.into_iter().filter_map(trusted_platform_for).collect();
        assert_eq!(
            harvestable,
            std::collections::BTreeSet::from([REVIEWER_PLATFORM]),
            "受理できる経路の集合＝名簿を引く経路であること"
        );
    }

    /// 受理できない経路の co_agent は指名できない（依頼だけ飛ぶ状態を作らない）。
    #[test]
    fn resolve_reviewer_ignores_other_platform_co_agents() {
        let conn = opencrab_db::init_memory().unwrap();
        let delivery = FakeDelivery::new();
        opencrab_db::queries::add_trusted_user(
            &conn,
            opencrab_db::queries::TRUSTED_PLATFORM_WEB,
            "row-w",
            "agent-1",
            "77",
            TrustedUserPermission::CoAgent,
            "owner",
            "2026-01-01",
            "Web Crab",
        )
        .unwrap();
        // 表示名でも id でも解決しない。
        assert!(resolve_reviewer(&conn, &delivery, "agent-1", "Web Crab").is_err());
        assert!(resolve_reviewer(&conn, &delivery, "agent-1", "77").is_err());
        // 一覧にも出さない（モデルに存在しない宛先を示唆しない）。
        let err = resolve_reviewer(&conn, &delivery, "agent-1", "nobody").unwrap_err();
        assert!(!err.contains("Web Crab"), "{err}");
    }

    #[test]
    fn resolve_reviewer_lists_registration_hint_when_roster_is_empty() {
        let conn = opencrab_db::init_memory().unwrap();
        let delivery = FakeDelivery::new();
        let err = resolve_reviewer(&conn, &delivery, "agent-1", "nobody").unwrap_err();
        assert_eq!(
            err,
            "(なし — trusted-users API で permission=co-agent + display_name を登録してください)"
        );
    }

