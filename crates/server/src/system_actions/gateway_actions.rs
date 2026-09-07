fn err(msg: String) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg),
    }
}

impl SystemGatewayActions {
    /// own 定義と inner 定義を name で dedup してマージする（own 優先）。
    ///
    /// nostr watch ループ稼働時は inner=NostrGatewayActions も nostr_generate_key を
    /// 定義するため、ここで dedup しないと
    /// ツール一覧に同名が2つ並ぶ（provider が拒否しうる）。`definitions()` の実体を
    /// 静的関数に切り出し、AppState 無しで dedup 契約を単体テストできるようにする（#161）。
    fn merge_definitions(
        mut own: Vec<GatewayActionDef>,
        inner: Option<&Arc<dyn GatewayActions>>,
    ) -> Vec<GatewayActionDef> {
        if let Some(inner) = inner {
            let own_names: std::collections::HashSet<String> =
                own.iter().map(|d| d.name.clone()).collect();
            for d in inner.definitions() {
                if !own_names.contains(&d.name) {
                    own.push(d);
                }
            }
        }
        own
    }
}

#[async_trait]
impl GatewayActions for SystemGatewayActions {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        Self::merge_definitions(
            Self::own_definitions_with_a2ui(self.a2ui.is_some()),
            self.inner.as_ref(),
        )
    }

    async fn execute(
        &self,
        name: &str,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        match name {
            "configure_llm_provider" => self.configure_llm_provider(args, ctx).await,
            "manage_allowed_commands" => self.manage_allowed_commands(args, ctx).await,
            // PR-1B: Nostr の会話ゲートツール群は nostr feature の内側。外した構成では
            // これらの arm も descriptor も消え、既定 `_ =>` の inner 委譲へ落ちる。
            #[cfg(feature = "nostr")]
            "configure_nostr" => self.configure_nostr(args, ctx).await,
            "configure_self" => self.configure_self(args, ctx).await,
            "configure_mcp_server" => self.configure_mcp_server(args, ctx).await,
            // bootstrap 鍵生成（鍵未設定でも露出）。inner より先に own が処理する。
            #[cfg(feature = "nostr")]
            "nostr_generate_key" => self.nostr_generate_key(args, ctx).await,
            // bootstrap 鍵一覧（鍵未設定でも露出）。生成鍵の npub のみ返す（nsec 非返却）。
            #[cfg(feature = "nostr")]
            "nostr_list_keys" => Self::nostr_list_keys(ctx),
            // bootstrap identity 採用（鍵未設定でも露出）。未接続なら自分宛のみを購読する
            // 設定で自動接続する。inner より先に own が処理する（#264）。
            #[cfg(feature = "nostr")]
            "nostr_switch_identity" => self.nostr_switch_identity(args, ctx).await,
            // `nostr_run`（薄い nostaro passthrough / #268）は撤去した（オーナー裁定）。定義から
            // 外したのでモデルは通常ここへ来ないが、名前指定で呼ばれても fail-close で拒否する
            // （黙って成功に見せない・feature の有無に依らず塞ぐ）。返信は say、独立投稿は nostr_post。
            "nostr_run" => err(
                "nostr_run は撤去されました（返信は say、投稿は nostr_post を使ってください）"
                    .to_string(),
            ),
            // 記憶インデックスの全再構築（#175 S4）。inner へは委譲しない。
            "rebuild_memory_index" => self.rebuild_memory_index(ctx).await,
            // 汎用エージェント管理ツール（#157 S1）。Discord 側の実装は撤去済みなので
            // inner へは委譲しない（委譲パターンにすると二重定義を招く）。許可コマンドは
            // **DB のみ**を更新する。グローバルな実行許可設定へは書かない（他エージェントへ
            // 漏れるため / #202）。次の run が `process::resolve_run_tools_config` で
            // DB から拾い直す。
            "update_memory_index_config" => {
                crate::agent_management::update_memory_index_config(&self.state, args, ctx)
            }
            "add_allowed_command" => {
                crate::agent_management::add_allowed_command(&self.state, args, ctx)
            }
            "list_allowed_commands" => {
                crate::agent_management::list_allowed_commands(&self.state, ctx)
            }
            "remove_allowed_command" => {
                crate::agent_management::remove_allowed_command(&self.state, args, ctx)
            }
            // スキル生成（#157 S6）。Discord 側の実装は撤去済みなので inner へは委譲しない
            // （委譲パターンにすると二重定義を招く）。core の `create_my_skill` とは別ツール
            // として**両方**残す（統廃合は #157 の範囲外）。
            "create_skill" => crate::agent_management::create_skill(&self.state, args, ctx),
            // ハートビート指示ツール（#157 S3）。Discord 側の実装は撤去済みなので
            // inner へは委譲しない（委譲パターンにすると二重定義を招く）。
            "update_heartbeat_instructions" => {
                crate::heartbeat_instructions::update_heartbeat_instructions(&self.state, args, ctx)
            }
            "read_heartbeat_instructions" => {
                crate::heartbeat_instructions::read_heartbeat_instructions(&self.state, args, ctx)
            }
            // エージェント自身の Nostr 転記設定（#252 段階 C）。対象は常に
            // `ctx.agent_id` で、引数から他エージェントを指す経路は無い。
            #[cfg(feature = "nostr")]
            "get_my_nostr_relay" => {
                crate::agent_nostr_relay::get_my_nostr_relay(&self.state, args, ctx)
            }
            #[cfg(feature = "nostr")]
            "set_my_nostr_relay" => {
                crate::agent_nostr_relay::set_my_nostr_relay(&self.state, args, ctx)
            }
            // エージェント自身のハートビート設定（#247 段階 2）。対象は常に
            // `ctx.agent_id` で、引数から他エージェントを指す経路は無い。
            "get_my_heartbeat" => crate::agent_heartbeat::get_my_heartbeat(&self.state, args, ctx),
            "set_my_heartbeat" => crate::agent_heartbeat::set_my_heartbeat(&self.state, args, ctx),
            // #599: 時間を待たずに手動発火（オーナー / co_agent 限定・OWNER_ONLY_ACTIONS）。
            "run_my_heartbeat" => crate::agent_heartbeat::run_my_heartbeat(&self.state, args, ctx),
            // エージェント自身の定時実行スケジュール（#455）。対象は常に ctx.session_id。
            "get_my_schedules" => crate::agent_schedule::get_my_schedules(&self.state, args, ctx),
            "set_my_schedule" => crate::agent_schedule::set_my_schedule(&self.state, args, ctx),
            // 更新・削除（#477）。id 指定で、ctx.agent_id＋現在セッションの所属チェックを通った行だけ。
            "update_my_schedule" => {
                crate::agent_schedule::update_my_schedule(&self.state, args, ctx)
            }
            "delete_my_schedule" => {
                crate::agent_schedule::delete_my_schedule(&self.state, args, ctx)
            }
            // 通知先（webhook）の管理ツール（#157 S5）。Discord 側の実装は撤去済みなので
            // inner へは委譲しない（委譲パターンにすると二重定義を招く）。設定ファイル
            // 由来のフォールバックは `AppState::default_subtask_webhook` から読むので、
            // Discord 機能の有無に関わらず同じ既定へ到達する。
            //
            // `ensure_webhook` / `ensure_subtask_webhook` はここに**無い**（Discord に
            // 残した webhook 新規作成つきのツール）。既定の `_ =>` で inner へ委譲される。
            "get_default_subtask_webhook" => {
                crate::webhook_targets::get_default_subtask_webhook(&self.state, args, ctx)
            }
            "set_default_subtask_webhook" => {
                crate::webhook_targets::set_default_subtask_webhook(&self.state, args, ctx)
            }
            "list_subtask_webhooks" => {
                crate::webhook_targets::list_subtask_webhooks(&self.state, args, ctx)
            }
            "get_default_webhook" => {
                crate::webhook_targets::get_default_webhook(&self.state, args, ctx)
            }
            "set_default_webhook" => {
                crate::webhook_targets::set_default_webhook(&self.state, args, ctx)
            }
            "list_webhooks" => crate::webhook_targets::list_webhooks(&self.state, args, ctx),
            // subtask 起動（#175 S4）。transport 非依存の唯一の実装（Discord 側の実装は
            // 撤去済み）。inner へは委譲しない。
            "spawn_subtask" => {
                let res = crate::subtask_spawn::spawn_subtask(
                    &self.state,
                    self.subtask_registry.as_ref(),
                    self.completion_sink.clone(),
                    // sub-engine の inner は「自分を包む合成 gateway」。`BridgedExecutor`
                    // が注入したハンドルを辿ることで、許可リスト内の server ツール
                    // （`report_progress` / `nostr_generate_key`）へ到達できる。
                    ctx.root_gateway.clone(),
                    args,
                    ctx,
                )
                .await;
                // #431: 起動が成立したときだけ「このターンは次の行動を選んだ」と数える。
                // `spawn_subtask` は登録簿へ insert し終えてから `success: true` を返し、
                // 手前の失敗（task 引数なし / session 不明 / 登録簿未配線）は全て
                // `success: false` なので、success ⟺ 登録済み ⟺ 完了で resume が来る。
                // 起動に失敗したターンは resume が来ない＝そのターンが最後の発話なので、
                // ここで数えないのが正しい（🏁 は付く）。
                if res.success {
                    if let Some(c) = &self.subtask_starts {
                        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                res
            }
            // A2UI 送信（#156 S3）。Discord 側の実装は撤去済みなので inner へは委譲しない
            // （委譲パターンにすると二重定義を招く）。描画面が無い transport では
            // `definitions()` に出ないが、モデルが名前で呼んだ場合に備えて明示エラーを返す
            // （fail-closed。黙って inner へ落とさない）。
            "send_ui" => match &self.a2ui {
                Some(surface) => {
                    opencrab_actions::send_ui(&self.state.db, surface, args, ctx).await
                }
                None => GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(
                        "send_ui はこのゲートウェイでは利用できません（UI を描画できません）"
                            .to_string(),
                    ),
                },
            },
            // ピアレビュー依頼（#157 S7）。Discord 側の実装は撤去済みなので inner へは
            // 委譲しない（委譲パターンにすると二重定義を招く）。配送口を持たない
            // transport でも**定義には出す**（#157 の目的）ので、無いときは黙って inner へ
            // 落とさず明示エラーを返す（fail-closed）。
            "request_peer_review" => match &self.text_delivery {
                Some(delivery) => {
                    crate::peer_review::request_peer_review(
                        &self.state.db,
                        delivery.as_ref(),
                        args,
                        ctx,
                    )
                    .await
                }
                None => GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(
                        "request_peer_review はこのゲートウェイでは利用できません（メッセージを送信できません）。\
                         このターンの transport はテキストを送れないため、ピアレビュー依頼は省略して先へ進んでよい。"
                            .to_string(),
                    ),
                },
            },
            // subtask 停止（#161 / #157 S2）。transport 非依存の唯一の実装（Discord 側の
            // 実装は撤去済み）。**inner へは委譲しない**: 委譲パターンのままにすると、
            // Discord が誤って `cancel_subtask` を再定義したときに own の実装（lifecycle
            // 通知 + 部分結果ログ + sink 通知）が黙ってバイパスされる。
            "cancel_subtask" => self.cancel_subtask(args, ctx),
            // 走行中 subtask への追加指示（steer / #647）。cancel と同じく neutral 実装へ委ねる。
            "steer_subtask" => self.steer_subtask(args, ctx),
            // subtask 進捗報告（#175 S1）。**唯一残る委譲パターン**（cancel_subtask は
            // #157 S2 で委譲を撤去した）。
            // transport 固有 gateway（Discord）が report_progress を実装しているなら、
            // その固有の後処理（lifecycle webhook への progress 送出）を保つため inner
            // へ委譲する＝ Discord 経路は挙動不変。実装していない transport
            // （web/Nostr/REST/heartbeat）では own が処理する。
            "report_progress" => {
                let inner_handles = self.inner.as_ref().is_some_and(|inner| {
                    inner
                        .definitions()
                        .iter()
                        .any(|d| d.name == "report_progress")
                });
                if inner_handles {
                    self.inner.as_ref().unwrap().execute(name, args, ctx).await
                } else {
                    self.report_progress(args, ctx).await
                }
            }
            // 自分が扱わないツールは inner gateway へ委譲する。
            _ => match &self.inner {
                Some(inner) => inner.execute(name, args, ctx).await,
                None => GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("Unknown action: {name}")),
                },
            },
        }
    }

    /// transport の A2UI 描画面を**そのまま外へ通す**（#156 S3）。
    ///
    /// 本番の sub-engine 配線は入れ子（`spawn_subtask` が `ctx.root_gateway` = この合成
    /// gateway を子へ渡し、子の `run_agent_response` がそれを `inner` にして**もう 1 段**
    /// 合成 gateway を作り、`SubEngineGatewayActions` で包む）。ここで転送しないと、
    /// 内側の合成 gateway は描画面を得られず `send_ui` を定義しないため、sub-engine から
    /// 名前指定で呼ばれたときの拒否が「権限拒否（`rejected:`）」ではなく
    /// 「Unknown gateway action」に変わる（遮断自体は保たれるが分類が変わる）。
    /// 移設前は Discord gateway が最内まで `inner` として届いていたので `send_ui` は
    /// 常に「実在するが許可外」だった。その分類を保つための転送。
    fn a2ui_surface(&self) -> Option<Arc<opencrab_core::a2ui::A2uiSurface>> {
        self.a2ui.clone()
    }

    /// transport の素テキスト配送口を**そのまま外へ通す**（#157 S7）。
    ///
    /// `a2ui_surface()` の転送と同じ理由: 本番の sub-engine 配線は合成 gateway の入れ子
    /// なので、ここで転送しないと内側の合成 gateway が配送口を失い、`request_peer_review`
    /// が「定義には出るが必ず失敗する」状態になる（sub-engine では深さ拒否が先に効くため
    /// 実害は無いが、能力を黙って落とさない）。
    fn text_delivery(&self) -> Option<Arc<dyn opencrab_core::text_delivery::TextDelivery>> {
        self.text_delivery.clone()
    }
}
