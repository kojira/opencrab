    use super::*;
    use opencrab_gateway::GatewayCaller;
    use serde_json::json;

    /// テスト用: serenity Httpは不要だがDiscordGatewayActionsの構築に必要。
    /// channel_config系テストではHTTP呼び出しは発生しないのでダミーでOK。
    fn make_test_actions() -> (DiscordGatewayActions, opencrab_db::Db) {
        let db = opencrab_db::Db::memory().unwrap();
        // serenityのHttpはダミートークンで作成（API呼び出しはしない）
        let http = Arc::new(Http::new("dummy-token"));
        let actions = DiscordGatewayActions::new(http, db.clone(), "/tmp".to_string(), None);
        (actions, db)
    }

    /// テスト用の呼び出しコンテキスト。旧テストは `__caller` を JSON に混ぜていたが、
    /// #36 で型付き GatewayCallContext に移行した。session_id は Discord 形式の
    /// ダミーを既定で持たせる（セッション必須アクションの検証テストを通すため）。
    fn tctx(caller: GatewayCaller) -> GatewayCallContext {
        GatewayCallContext::new(caller, "test-agent").with_session_id("discord-test-agent-111-222")
    }

    // `cancel_subtask` の 8 テスト（認可 4 / セッション無し 1 / 停止ログの説明文 3）と
    // その registry ヘルパは #157 S2 で server 側（`crates/server/src/system_actions.rs`）
    // へ移植済み。停止処理は gateway 非依存層の唯一の実装になったので、この gateway は
    // registry も lifecycle 通知口マップも持たない。

    // ---- #63: SubEngineGatewayActions 許可リスト ----

    #[tokio::test]
    async fn test_sub_engine_gateway_allowlist() {
        use opencrab_actions::SubEngineGatewayActions;

        let (actions, _db) = make_test_actions();
        // 後方互換の経路（root_gateway 未注入）では transport gateway 単体を wrap する。
        let sub_gw = SubEngineGatewayActions::new(std::sync::Arc::new(actions.clone()));

        // 許可リストの 2 名（report_progress / nostr_generate_key）はいずれも server 側
        // の定義になったため（#175 S4）、Discord 単体を wrap すると露出は空になる。
        // = sub-engine から Discord のツールへは一切到達できない。合成 gateway 経由で
        // 許可ツールに到達できることは `crates/actions/src/bridge.rs` の S2 テストが固定する。
        let names: Vec<String> = sub_gw.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            names.is_empty(),
            "Discord 単体では許可ツールが無い: {names:?}"
        );

        let sub_ctx = GatewayCallContext::new(GatewayCaller::Agent, "test-agent")
            .with_session_id("subtask-s1")
            .with_depth(1);

        // 実在するが許可外 → rejected: マーカー（`spawn_subtask` / `cancel_subtask` /
        // `create_skill` / `send_ui` を含まないのは、Discord がもう定義していないため。
        // ネスト禁止の実効ゲートは許可リスト側）。
        for name in ["discord_channel_config", "discord_send_file"] {
            let result = sub_gw.execute(name, &json!({}), &sub_ctx).await;
            assert!(!result.success, "{name} should be blocked");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap()
                    .starts_with(opencrab_actions::REJECTION_CODE_PREFIX),
                "{name} should be a policy rejection"
            );
        }

        // 未知の名前 → 通常の失敗（Unknown gateway action）
        let result = sub_gw.execute("no_such_tool", &json!({}), &sub_ctx).await;
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("Unknown gateway action"));
        assert!(!err.starts_with(opencrab_actions::REJECTION_CODE_PREFIX));

        // 移設済みツールも Discord 単体経由では届かない（未知の名前として失敗する）。
        for moved in [
            "report_progress",
            "spawn_subtask",
            "cancel_subtask",
            "read_heartbeat_instructions",
            "update_heartbeat_instructions",
            "create_skill",
            // #156 S3: A2UI 送信も server 側（`SystemGatewayActions`）の own ツール。
            // 合成 gateway 経由で sub-engine から到達できないことは
            // `send_ui_is_blocked_in_sub_engine`（`crates/server/src/system_actions.rs`）
            // が固定する。
            "send_ui",
            // #157 S7: ピアレビュー依頼も同様（`request_peer_review_is_blocked_in_sub_engine`）。
            "request_peer_review",
        ] {
            let result = sub_gw
                .execute(moved, &json!({"message": "x"}), &sub_ctx)
                .await;
            assert!(!result.success, "{moved} は Discord 単体では実行できない");
        }
    }

    // ---- #36: セッション必須アクションの fail-closed ----
    //
    // この gateway にセッション必須アクションはもう残っていない:
    // `report_progress` / `spawn_subtask` は #175 S4、`send_ui` は #156 S3、
    // `request_peer_review` は #157 S7 で gateway 非依存層へ移設済み。同趣旨のガードは
    // それぞれ `crates/server/src/system_actions.rs` / `crates/actions/src/a2ui.rs` の
    // `send_ui_without_session_fails_closed` / `crates/server/src/peer_review.rs` の
    // `error_messages_are_byte_stable` にある。

    /// **分類属性の集合を固定する**（`dispatch` / `sub_engine` は既に他テスト・他ゲートが
    /// 覆うが、権威リストが消えた `dispatch` と `sharing` を「値を書き間違えたら落ちる」
    /// 状態にする）。権威リストが無いので `definitions()` の属性から集合を直接固定する。
    ///
    /// - **`Dispatchable` 集合 == 空**: Discord に残るツールは配送系 / 同ターン結果依存 /
    ///   run 内共有状態 / 純粋な読み取りのいずれかで全部 `Inline`。長時間ツールは無い。
    /// - **`ConversationBound` 集合 == {discord_add_reaction}**: 会話固有の一時ハンドル
    ///   （message_id）を必須に取る唯一のツール。全ゲート横断では
    ///   {discord_add_reaction, nostr_reply, send_ui} で残り 2 つは nostr / server 側が覆う。
    ///
    /// `sub_engine == Blocked`（配送系の深さ拒否）は `crates/server` の
    /// `send_ui_is_blocked_in_sub_engine` 等が挙動で覆うのでここでは固定しない。
    #[test]
    fn discord_tool_class_sets_are_fixed() {
        use opencrab_gateway::{DispatchMode, ToolSharing};
        let (actions, _db) = make_test_actions();
        let defs = actions.definitions();
        assert!(!defs.is_empty());

        let dispatchable: std::collections::BTreeSet<String> = defs
            .iter()
            .filter(|d| d.class.dispatch == DispatchMode::Dispatchable)
            .map(|d| d.name.clone())
            .collect();
        assert_eq!(
            dispatchable,
            std::collections::BTreeSet::new(),
            "discord ゲートの Dispatchable 集合がずれている（dispatch 属性の Inline/Dispatchable 取り違え）"
        );

        let conv_bound: std::collections::BTreeSet<String> = defs
            .iter()
            .filter(|d| d.class.sharing == ToolSharing::ConversationBound)
            .map(|d| d.name.clone())
            .collect();
        let expected: std::collections::BTreeSet<String> =
            std::iter::once("discord_add_reaction".to_string()).collect();
        assert_eq!(
            conv_bound, expected,
            "discord ゲートの ConversationBound 集合がずれている（sharing 属性の付け忘れ/誤り）"
        );
    }

    /// 配送系・同ターン結果依存・純粋な読み取りが inline（`class.dispatch == Inline`）で
    /// あること（#152 の実害）。分類の権威は各定義の属性なので、`definitions()` を実体で
    /// 呼んで属性を直接見る。
    ///
    /// 特に `send_ui` は「UI を送信しユーザーの応答を待機する」配送系で、background 化
    /// すると (a) UI 投稿と本文返信の順序が入れ替わり、(b) エージェントはインタラクション
    /// ID を扱えず、(c) クリック resume と subtask 決着 resume で返信が 2 通になる
    /// （send_ui は #156 S3 で server 側へ移設したのでここでは検証しない）。
    #[test]
    fn delivery_and_read_tools_are_inline() {
        use opencrab_gateway::DispatchMode;
        let (actions, _db) = make_test_actions();
        let defs = actions.definitions();
        let class_of = |name: &str| {
            defs.iter()
                .find(|d| d.name == name)
                .unwrap_or_else(|| panic!("{name} が definitions() に無い"))
                .class
        };
        for name in [
            "discord_send_file",
            "discord_add_reaction",
            // 同ターンで戻り値（URL / ID）を使う
            "ensure_webhook",
            "ensure_subtask_webhook",
            "discord_create_webhook",
            "discord_create_channel",
            // 純粋な読み取り
            "discord_list_channels",
            "discord_list_guilds",
        ] {
            assert_eq!(
                class_of(name).dispatch,
                DispatchMode::Inline,
                "{name} は inline に残すべき（dispatch 属性が Inline でない）"
            );
        }
    }

    // ---- definitions ----

    /// **リアクションだけで応じる道は description に書いてある**（#317）。
    ///
    /// ツールも権限も既に揃っていて（`discord_add_reaction` は inline 実行が保証済み、
    /// message_id は毎ターン会話の先頭に載る）、足りないのはエージェントがその使い方を
    /// 知る手段だけだった。この 1 文が消えると「テキストで返すほどでもない反応」は
    /// また全部テキストで返るようになる — コードは壊れないので他のテストでは検出できない。
    ///
    /// 置き場所も固定する: Discord 固有の運用なので、transport 非依存の共通プロンプト
    /// （`crates/server/src/process.rs` の Silent Reply 節）ではなくここに書く。
    #[test]
    fn add_reaction_tells_the_agent_it_can_answer_with_a_reaction_alone() {
        let (actions, _db) = make_test_actions();
        let defs = actions.definitions();
        let def = defs
            .iter()
            .find(|d| d.name == "discord_add_reaction")
            .expect("discord_add_reaction が定義から消えている");
        assert!(
            def.description.contains("NO_REPLY"),
            "リアクションだけで応じてよいことが description から消えている: {}",
            def.description
        );
        // `NO_REPLY` に触れているだけだと、文意が反転した書き換え（「NO_REPLY に
        // してはいけない」）でも通る。**許している側**の語も一緒に固定する。
        assert!(
            def.description.contains("リアクションだけ"),
            "「リアクションだけで応じてよい」が反転／消失している: {}",
            def.description
        );
        assert!(def.description.contains("1回の応答"));
        assert!(def.description.contains("呼び直す必要はない"));
    }

    #[test]
    fn test_definitions_returns_expected_count() {
        let (actions, _db) = make_test_actions();
        let defs = actions.definitions();
        assert_eq!(defs.len(), 11);

        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"discord_list_guilds"));
        assert!(names.contains(&"discord_list_channels"));
        assert!(names.contains(&"discord_channel_config"));
        assert!(names.contains(&"discord_add_reaction"));
        assert!(names.contains(&"discord_create_webhook"));
        assert!(names.contains(&"discord_create_channel"));
        assert!(names.contains(&"discord_send_file"));
        assert!(names.contains(&"join_voice_channel"));
        assert!(names.contains(&"leave_voice_channel"));
        // webhook 新規作成つきの 2 本だけが残る（#157 S5）。
        assert!(names.contains(&"ensure_subtask_webhook"));
        assert!(names.contains(&"ensure_webhook"));

        // #175 S4 / #157 S1・S2・S3・S5・S6・S7 / #155: サブタスク生成・進捗報告・**停止**・記憶
        // インデックス再構築と、汎用管理ツール（記憶インデックス設定・許可コマンド 3 種）・
        // ハートビート指示 2 種・通知先（webhook）の管理 6 種・スキル生成は gateway 非依存層
        // （server 側 `SystemGatewayActions`）へ移設済み。
        // Discord がこれらを再び定義すると `SystemGatewayActions` の dedup（own 優先）で
        // own 側に食われ、Discord 実装の後処理が黙って落ちる（#155 の後退）。
        for moved in [
            "spawn_subtask",
            "report_progress",
            "cancel_subtask",
            "rebuild_memory_index",
            "update_memory_index_config",
            "add_allowed_command",
            "list_allowed_commands",
            "remove_allowed_command",
            "update_heartbeat_instructions",
            "read_heartbeat_instructions",
            // #157 S5 で移設した通知先の管理 6 種。
            "get_default_subtask_webhook",
            "set_default_subtask_webhook",
            "list_subtask_webhooks",
            "get_default_webhook",
            "set_default_webhook",
            "list_webhooks",
            // #157 S6 で移設したスキル生成。
            "create_skill",
            // #157 S7 で移設したピアレビュー依頼（Discord に残るのは配送口と返信回収）。
            "request_peer_review",
        ] {
            assert!(
                !names.contains(&moved),
                "{moved} は server 側の実装だけであるべき"
            );
        }
    }

    #[test]
    fn test_definitions_have_valid_parameters() {
        let (actions, _db) = make_test_actions();
        for def in actions.definitions() {
            assert!(
                def.parameters.is_object(),
                "parameters should be object for {}",
                def.name
            );
            assert!(def.parameters["type"] == "object");
        }
    }

    // ---- request_peer_review ----
    //
    // 引数検査（content 必須 / 長さ上限 / 宛先の解決と検査）のテストは #157 S7 で
    // server 側（`crates/server/src/peer_review.rs`）へ移設済み。Discord に残る配送口の
    // テストは `super::text_delivery`。

    // ---- channel_config ----

