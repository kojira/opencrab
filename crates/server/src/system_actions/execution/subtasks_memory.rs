impl SystemGatewayActions {
    /// 記憶インデックスの全再構築（#175 S4）。旧 Discord 実装
    /// （`DiscordGatewayActions::execute_rebuild_memory_index`・撤去済み）をそのまま
    /// 移設したもので、LLM クライアントは `AppState` のルーターから組む。
    async fn rebuild_memory_index(&self, ctx: &GatewayCallContext) -> GatewayActionResult {
        let llm_client = crate::llm_adapter::LlmRouterAdapter::new(self.state.llm_router.clone())
            .with_agent_id(&ctx.agent_id);

        let (config, persona_name, personality, effective_model) = {
            let Ok(conn) = self.state.db.lock() else {
                return err("db lock failed".to_string());
            };
            let config = opencrab_db::queries::get_memory_index_config(&conn, &ctx.agent_id)
                .unwrap_or_else(|_| opencrab_db::queries::AgentMemoryIndexConfig {
                    agent_id: ctx.agent_id.clone(),
                    batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
                    threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
                    updated_at: String::new(),
                });
            let (persona_name, personality) = opencrab_db::queries::get_agent(&conn, &ctx.agent_id)
                .ok()
                .flatten()
                .map(|a| (a.persona_name, a.personality))
                .unwrap_or_default();
            let effective_model = opencrab_db::queries::effective_model_for_agent(
                &conn,
                &ctx.agent_id,
                &self.state.default_model,
            )
            .unwrap_or_else(|_| self.state.default_model.clone());
            (config, persona_name, personality, effective_model)
        };

        match opencrab_core::memory_index::IndexBuilder::rebuild_index(
            &self.state.db,
            &ctx.agent_id,
            &llm_client,
            &effective_model,
            config.batch_size as usize,
            &persona_name,
            personality.as_deref(),
        )
        .await
        {
            Ok(result) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "agent_id": ctx.agent_id,
                    "logs_indexed": result.logs_indexed,
                    "nodes_created": result.nodes_created,
                    "message": format!(
                        "メモリインデックスを再構築しました（{}件のログ → {}ノード作成）",
                        result.logs_indexed, result.nodes_created,
                    ),
                })),
                error: None,
            },
            Err(e) => {
                tracing::error!("rebuild_memory_index failed: {e}");
                err(format!("メモリインデックスの再構築に失敗: {e}"))
            }
        }
    }

    /// 実行中 subtask の停止（#161・#157 S2）。共有 `SubtaskRegistry` を引き、認可
    /// （親セッション/owner 限定）・abort・除去・lifecycle 通知・親ログ記録・sink 通知を
    /// server-neutral の `cancel_subtask` に委ねる。**これが唯一の実装**で、transport 固有の
    /// 停止実装は無い（#157 S2 で Discord 実装を撤去し、その固有の後始末を neutral 層へ
    /// 取り込んだ）。registry 未配線（`None`）や不在は not found を返す。権限なしは
    /// `REJECTION_CODE_PREFIX` を付けて拒否として通知する（旧 Discord 実装と同契約）。
    fn cancel_subtask(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        let Some(subtask_id) = args.get("subtask_id").and_then(|v| v.as_str()) else {
            return err("cancel_subtask: 'subtask_id' is required".to_string());
        };
        let Some(registry) = self.subtask_registry.as_ref() else {
            // dispatch 未配線（走行中 subtask を追跡していない）→ 不在扱い。
            return err(format!("cancel_subtask: subtask '{subtask_id}' not found"));
        };
        // 停止の認可は caller で決める（#331）。Owner は常に許可、非オーナーは親セッション
        // 一致に加えて subtask を spawn したターンの呼び出し元以上の信頼度が要る。
        // `is_owner` bool ではなく caller を丸ごと渡すのは、1本化で「セッション一致」だけでは
        // 見知らぬ相手のターンから Owner 由来の subtask を止められてしまうため。
        let caller: opencrab_actions::CallerIdentity = (&ctx.caller).into();
        match neutral_cancel_subtask(
            registry,
            &self.state.db,
            self.completion_sink.as_deref(),
            // 中断の lifecycle 通知（旧 Discord 実装の後始末）はこのマップ経由で行う。
            // `spawn_subtask` が insert したものと同一 Arc（`AppState` 共有）。
            Some(&self.state.subtask_notifiers),
            subtask_id,
            caller,
            ctx.session_id.as_deref(),
        ) {
            CancelOutcome::Cancelled => GatewayActionResult {
                success: true,
                data: Some(json!({ "cancelled": true, "subtask_id": subtask_id })),
                error: None,
            },
            CancelOutcome::NotFound => {
                err(format!("cancel_subtask: subtask '{subtask_id}' not found"))
            }
            CancelOutcome::Unauthorized => err(format!(
                "{REJECTION_CODE_PREFIX}cancel_subtask: subtask '{subtask_id}' をこのセッションからキャンセルする権限がありません（親セッションまたは owner のみ）"
            )),
        }
    }

    /// 走行中 subtask への追加指示（steer / #647）。共有 `SubtaskRegistry` を引き、認可
    /// （cancel と同じ `caller_can_manage_subtask`）・steer 記録・不達判定を server-neutral の
    /// `steer_subtask` に委ねる。**これが唯一の実装**で、transport 固有の steer 実装は無い。
    /// registry 未配線（`None`）や不在は not found を返す。既に決着/停止したサブや
    /// auto-dispatch のサブへ送った場合は、**黙って捨てず**その旨をエラーで返す（#647
    /// 受け入れ条件 3・4）。権限なしは `REJECTION_CODE_PREFIX` を付けて拒否として通知する。
    fn steer_subtask(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        let Some(subtask_id) = args.get("subtask_id").and_then(|v| v.as_str()) else {
            return err("steer_subtask: 'subtask_id' is required".to_string());
        };
        let Some(message) = args.get("message").and_then(|v| v.as_str()) else {
            return err("steer_subtask: 'message' is required".to_string());
        };
        if message.trim().is_empty() {
            return err("steer_subtask: 'message' は空にできません".to_string());
        }
        let Some(registry) = self.subtask_registry.as_ref() else {
            // dispatch 未配線（走行中 subtask を追跡していない）→ 不在扱い。
            return err(format!("steer_subtask: subtask '{subtask_id}' not found"));
        };
        // 認可は cancel と同じ caller ベース（#331 / #647）。
        let caller: opencrab_actions::CallerIdentity = (&ctx.caller).into();
        match neutral_steer_subtask(
            registry,
            &self.state.db,
            subtask_id,
            message,
            caller,
            ctx.session_id.as_deref(),
        ) {
            SteerOutcome::Accepted => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "steered": true,
                    "subtask_id": subtask_id,
                    "note": "追加指示を記録しました。サブタスクは次の反復の合間にこれを読み、受領/反映を親へ返します。",
                })),
                error: None,
            },
            SteerOutcome::NotFound => {
                err(format!("steer_subtask: subtask '{subtask_id}' not found"))
            }
            SteerOutcome::AlreadySettled => err(format!(
                "steer_subtask: subtask '{subtask_id}' は既に完了または停止しているため追加指示を届けられません"
            )),
            SteerOutcome::NotSteerable => err(format!(
                "steer_subtask: subtask '{subtask_id}' は auto-dispatch（LLM ループを持たない）ため追加指示を読む主体がありません。止めるには cancel_subtask を使ってください"
            )),
            SteerOutcome::Unauthorized => err(format!(
                "{REJECTION_CODE_PREFIX}steer_subtask: subtask '{subtask_id}' へこのセッションから追加指示を送る権限がありません（親セッションまたは owner のみ）"
            )),
            SteerOutcome::RecordFailed => err(format!(
                "steer_subtask: subtask '{subtask_id}' への追加指示の記録に失敗しました（届いていません。時間をおいて再試行してください）"
            )),
        }
    }

    /// サブタスクの進捗報告（#175 S1）。Discord 実装（`execute_report_progress`）の
    /// transport 非依存な部分を移植したもの。Discord 固有の webhook 送出は Discord 側に
    /// 残る実装が担当する（この own 実装は inner が未実装の経路でのみ走る）。
    ///
    /// 手順は Discord と同一:
    /// 1. `message` 必須 / セッション必須（fail-closed）
    /// 2. 登録簿から subtask を引く（`subtask_id` 明示 → 無ければ session_id で逆引き）
    /// 3. 所有権ゲート（自分自身の subtask か、自分が親のもののみ）
    /// 4. 親セッションログへ `subtask_progress` を記録（本文の永続化はここだけ）
    /// 5. デバウンス後に完了 sink へ `SettleKind::Progress` を通知（メインエンジン再呼び出し）
    ///
    /// 5 の通知は**親会話の resume** を起こすので、親ターンの呼び出し元
    /// （`SpawnedSubtask.caller`）をそのまま載せる（#298）。`ctx.caller` は sub-engine
    /// 自身（最小権限）なので使えない。
    async fn report_progress(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        let Some(message) = args.get("message").and_then(|v| v.as_str()) else {
            return err("report_progress: 'message' is required".to_string());
        };
        let message = message.to_string();
        // セッション必須（fail-closed）: 親セッションの解決が session_id に依存する（#36）。
        let current_session_id = match ctx.session_id.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                return err(
                    "report_progress はセッション文脈でのみ実行できます（session_id 不明）"
                        .to_string(),
                );
            }
        };
        let subtask_id_arg = args
            .get("subtask_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let agent_id = ctx.agent_id.clone();

        // 登録簿から進捗の宛先と、resume に必要な親ターンの呼び出し元を引く。registry
        // 未配線（`None`）は「登録簿に無い」と同じ扱い（= 自己申告として親ログにだけ残す）。
        let subtask_entry: Option<ProgressSubtaskEntry> =
            self.subtask_registry.as_ref().and_then(|registry| {
                if !subtask_id_arg.is_empty() {
                    registry
                        .get(&subtask_id_arg)
                        .map(|e| ProgressSubtaskEntry::from_entry(subtask_id_arg.clone(), &e))
                } else {
                    registry
                        .iter()
                        .find(|e| e.session_id == current_session_id)
                        .map(|e| ProgressSubtaskEntry::from_entry(e.key().clone(), e.value()))
                }
            });

        // 所有権ゲート（#64 / #331）: subtask_id は LLM 由来の引数なので、呼び出し元
        // セッションのサブタスク（自分自身 = session_id 一致、または自分の子 =
        // parent_session_id 一致）以外は拒否する。無検証だと他セッションへの進捗ログ
        // 書き込み・メインエンジン再呼び出しを誘発できてしまう。
        if let Some(entry) = &subtask_entry {
            let is_self = entry.session_id == current_session_id;
            let is_parent = entry.parent_session_id == current_session_id;
            if !is_self && !is_parent {
                let id = &entry.subtask_id;
                return err(format!(
                    "{REJECTION_CODE_PREFIX}report_progress: subtask '{id}' は呼び出し元セッションのサブタスクではありません"
                ));
            }
            // 親経由の代理報告（`is_parent`）は、subtask を spawn したターンの呼び出し元
            // （`entry.caller`）を自分の権限で管理できるときだけ許す（#331）。セッションを
            // agent 単位で 1 本にした（#323）ため、`is_parent` だけだと見知らぬ相手
            // （caller=Agent）のターンから Owner 由来の subtask へ進捗を差し込み、親会話の
            // resume（メインエンジン再呼び出し）を誘発できてしまう。
            // **`is_self`（subtask 本人 = depth>=1 の自己申告）は無条件で許す** — ここに
            // caller 判定を掛けるとサブエージェント自身の進捗報告が壊れる（自セッションは
            // 本人しか名乗れないので攻撃経路にならない）。
            if !is_self {
                let caller: opencrab_actions::CallerIdentity = (&ctx.caller).into();
                if !caller.can_manage_subtask_of(&entry.caller) {
                    let id = &entry.subtask_id;
                    return err(format!(
                        "{REJECTION_CODE_PREFIX}report_progress: subtask '{id}' は別の権限で起動されたため、このターンからは進捗報告できません"
                    ));
                }
            }
        }

        let subtask_id = subtask_entry
            .as_ref()
            .map(|e| e.subtask_id.clone())
            .unwrap_or(subtask_id_arg);
        let parent_session_id = subtask_entry
            .as_ref()
            .map(|e| e.parent_session_id.clone())
            .unwrap_or_else(|| current_session_id.clone());
        // resume 時の呼び出し元（#298）。登録簿に無い自己申告は最小権限へ倒す。
        // ここで `ctx.caller`（= sub-engine 自身 = Agent）を使ってはならない。
        let resume_caller = subtask_entry
            .as_ref()
            .map(|e| e.caller.clone())
            .unwrap_or(opencrab_actions::CallerIdentity::Agent);

        // 進捗本文は親セッションログ（DB）へ永続化する。sink には本文を運ばない
        // （RFC §1.3）ので、受け口が未配線でも本文自体はここで残る。
        if !parent_session_id.is_empty() {
            if let Ok(conn) = self.state.db.lock() {
                let log = opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: agent_id.clone(),
                    session_id: parent_session_id.clone(),
                    log_type: "system".to_string(),
                    content: json!({
                        "type": "subtask_progress",
                        "subtask_id": subtask_id,
                        "message": message,
                        "timestamp": Utc::now().to_rfc3339(),
                    })
                    .to_string(),
                    speaker_id: None,
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                };
                opencrab_db::queries::insert_session_log_best_effort(&conn, &log);
            }
        }

        // 進捗を lifecycle 通知口へ流す（#175 S4）。通知口は登録簿と対の随伴マップ
        // （`AppState.subtask_notifiers`）から引く。旧 Discord 実装が webhook へ
        // progress を出していた経路の置き換えで、宛先の解決も整形も実装側に閉じている。
        if let Some(entry) = &subtask_entry {
            if let Some(notifier) = self.state.subtask_notifiers.get(&entry.subtask_id) {
                notifier.on_progress(&message);
            }
        }

        // 完了受け口が未配線の経路（`with_dispatch` していない run）では、デバウンス
        // タスクを**起動しない**。起動しても 3 秒後に通知先が無く黙って消えるだけで、
        // (a) 無駄な tokio タスクと (b) 世代カウンタの残骸を積むだけだからである。
        // 記録（上の親ログ）は済んでいるので、その旨を debug ログに残して成功を返す。
        let Some(sink) = self.completion_sink.clone() else {
            tracing::debug!(
                session_id = %current_session_id,
                parent_session_id = %parent_session_id,
                subtask_id = %subtask_id,
                "report_progress: completion sink not wired; progress logged to the parent session only (no main-engine notification)"
            );
            return GatewayActionResult {
                success: true,
                data: Some(json!({
                    "reported": true,
                    "message": message,
                    // 記録はしたが再注入はしていないことを呼び出し元に明示する。
                    "notified": false,
                })),
                error: None,
            };
        };

        // デバウンス: 3 秒待ってからメインエンジン再呼び出しを 1 回だけ発火する。
        // 世代カウンタは `AppState` 側（`ProgressDebounce`）にある。この構造体は run
        // ごとに作り直されるためフィールドに置くと毎回リセットされ、バースト時に同数の
        // LLM 再呼び出し（コスト増・チャンネルスパム）が起きる。
        let debounce = self.state.progress_debounce.clone();
        let my_generation = debounce.bump(&parent_session_id);
        let parent_session_clone = parent_session_id.clone();
        let subtask_id_clone = subtask_id.clone();
        let agent_id_clone = agent_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY).await;
            // 自分より後に report_progress が来ていたら（世代が進んでいたら）発火しない。
            if !debounce.claim_latest(&parent_session_clone, my_generation) {
                return;
            }
            // 継続を起こすかの判断は `dispatch_settled`（#638・唯一の実装）。進捗を配送するのは
            // `forwards_progress()` が true の transport（Discord）だけ——ここで分岐しない。
            opencrab_actions::dispatch_settled(
                &*sink,
                SubtaskSettled {
                    session_id: parent_session_clone,
                    agent_id: agent_id_clone,
                    subtask_id: subtask_id_clone,
                    exit_reason: "progress".to_string(),
                    kind: SettleKind::Progress,
                    // 進捗の宛先は親セッション。返信先の復元は sink 側の責務（#167）。
                    reply_target: None,
                    // 親ターンの呼び出し元を引き継ぐ（#298）。ここを Agent 固定にすると
                    // 「進捗を報告すると自分の権限が落ちる」自爆的な挙動になる。
                    caller: resume_caller,
                },
            );
        });

        GatewayActionResult {
            success: true,
            data: Some(json!({
                "reported": true,
                "message": message,
                "notified": true,
            })),
            error: None,
        }
    }
}
