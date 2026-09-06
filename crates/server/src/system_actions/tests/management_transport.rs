use super::super::*;
use super::support::*;

/// 移設した 4 ツールは **inner（Discord）へ委譲しない**。
///
/// `cancel_subtask` / `report_progress` は Discord 固有の後処理を保つため委譲する
/// が、この 4 つは Discord 側の実装を撤去したので own が処理しなければならない。
/// 委譲パターンで書くと、Discord が誤って再定義したときに own の実装が黙って
/// バイパスされる。
#[tokio::test]
async fn generic_management_tools_are_not_delegated_to_inner() {
    let state = state_with_shell(&[]);
    let inner = Arc::new(RecordingInner::new(&[
        "update_memory_index_config",
        "add_allowed_command",
        "list_allowed_commands",
        "remove_allowed_command",
    ]));
    let actions = SystemGatewayActions::new(
        state.clone(),
        Some(inner.clone() as Arc<dyn GatewayActions>),
        None,
        None,
    );

    for (name, args) in [
        ("update_memory_index_config", json!({"batch_size": 7})),
        ("add_allowed_command", json!({"command": "curl"})),
        ("list_allowed_commands", json!({})),
        ("remove_allowed_command", json!({"command": "curl"})),
    ] {
        let r = actions.execute(name, &args, &owner_ctx()).await;
        assert!(r.success, "{name}: {:?}", r.error);
        assert!(
            r.data.as_ref().unwrap().get("reached_inner").is_none(),
            "{name} が inner へ委譲されている（own が処理すべき）"
        );
    }
    assert!(
        inner.calls().is_empty(),
        "inner へ到達してはならない: {:?}",
        inner.calls()
    );
}

/// **transport gateway が inner に居ても（REST + Discord 構成）漏れないことの固定**。
///
/// このテストは**旧 `hot_reload_reaches_the_shared_config_even_with_a_transport_inner`
/// の反転**である。旧テストは「inner が居てもグローバル設定に反映される」ことを
/// 不変条件として固定していたが、それは #202 の漏れそのものだった。
///
/// 経緯（#197 との関係）: REST（`crate::api::agents_messages`）は Discord が有効な
/// とき `SystemGatewayActions { inner: Some(DiscordGatewayActions) }` を組む。移設前は
/// その Discord gateway へ `Arc::new(RwLock::new(state.tools_config.read().clone()))`
/// ＝**使い捨てのコピー**を渡していた。そのおかげで REST 経路は**偶然この漏れが
/// 無かった**。素朴に移設すると共有実体へ届いて漏れる側に揃ってしまうため、同じ
/// 変更でグローバル書き込みを撤去した。
///
/// #197 について構造面で言えることは、`DiscordGatewayActions::new` がもう実行許可
/// 設定を受け取らない（引数自体が消えた）＝**別インスタンスを作る余地がコンパイル時に
/// 無い**という点だけである。
#[tokio::test]
async fn add_allowed_command_does_not_leak_to_the_global_config_with_a_transport_inner() {
    let state = state_with_shell(&[]);
    // REST + Discord 相当: transport gateway が inner に居る構成。
    let inner = Arc::new(RecordingInner::new(&["discord_send_file"]));
    let actions = SystemGatewayActions::new(
        state.clone(),
        Some(inner as Arc<dyn GatewayActions>),
        None,
        None,
    );

    let r = actions
        .execute(
            "add_allowed_command",
            &json!({"command": "curl"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);

    // DB にだけ入る。
    assert_eq!(db_allowed_commands(&state, "agent-x"), vec!["curl"]);
    assert!(
        live_allowed_commands(&state).is_empty(),
        "inner の有無に関わらずグローバル設定へ書いてはならない（#202）: {:?}",
        live_allowed_commands(&state)
    );
}
