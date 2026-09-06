use super::support::*;

// ===========================================================================
// #925 §13.4.3【V3 レーンの heartbeat 受け口（TimedFire）】TDD 赤先行。
//
// 赤の駆動（scheduler 実経路・偽ツールなし）:
//   scheduler.rs:152 が使う `TimedFireRouter::resolve_target(session_id, agent)` と
//   `heartbeat_fire::run_one_heartbeat` を、そのまま公開 API として呼ぶ。ハーネスの
//   `timed_fire_router` は空（`TimedFireRouter::new()`）で extgate descriptor が未登録なので、
//   `resolve_target("extgate-<binding_id>")` は **None → 発火せず**（fail-closed）。よって
//   heartbeat の配送・保存・🏁 は 0 になり、H1/H3 の「say 2」「🏁 1」等の期待が assert で赤に
//   なる（新型を test 本文に書かないので compile は通る＝assert 赤・§1）。
//   実装（`ExtgateFire` descriptor ＋ `ExtgateTimedFireSink` を production 配線へ登録）後は
//   `resolve_target` が `Some(target)` を返し `run_one_heartbeat` が発火して同じ assert が緑になる。
//   注: 中央スケジューラの列挙関数（`list_and_build_heartbeat_entries`）は bin 側で integration
//   test から不可視のため、それが握る seam（`resolve_target`）を直接呼ぶ。両者は同一関数（同 file:line）。
//
// 観測境界（Discord）: dry-run kind="say"（配送）・memory_sessions speech（保存）・
//   mock 呼び出し数（LLM 回数）・say 本文の残留（CONTINUE/NO_REPLY）・system_reaction 🏁/🤐・typing。
//   BUFFER は binary 内で共有・累積のため、heartbeat の各シナリオは専用チャンネルへ束ねて隔離する
//   （typing は scope key を持たないので channel で分ける）。memory_sessions は start_core ごとに
//   別 DB（`init_memory`）なので speech 件数はテスト間で隔離される。
// ===========================================================================

const CH_HB1: &str = "610";
const CH_HB2: &str = "611";
const CH_HB3: &str = "612";
const CH_HB4: &str = "613";

const M_HB1: &str = "HBONEMARK";
const HB1_B1: &str = "hb1-巡回して見つけたこと その1（heartbeat 途中発話）";
const HB1_B2: &str = "hb1-見つけたこと その2（heartbeat 最終）";
const M_HB2: &str = "HBTWOMARK";
const M_HB3: &str = "HBTHREEMARK";
const HB3_DECL: &str = "hb3-調べるね（heartbeat 宣言・execute_shell と同一生成）";
const HB3_REPORT: &str = "hb3-終わったよ（resume 完了報告）";
const HB3_ECHO: &str = "hb3-echo-即時stdout";
const HB4_FABRICATED: &str = "hb4-未接続なのに投稿された（捏造検知用・出たら不具合）";

/// agent 単位の heartbeat 指示文を設定する（`resolve_heartbeat_instructions` の "agent" ソース）。
/// これが `run_one_heartbeat` の system プロンプトへ載り、mock が heartbeat 起点のターンを識別する。
fn set_hb_instructions(core: &Core, text: &str) {
    let conn = core.extgate.db.lock().unwrap();
    conn.execute(
        "UPDATE agents SET heartbeat_instructions=?1 WHERE agent_id=?2",
        [text, AGENT_ID],
    )
    .unwrap();
}

/// heartbeat 設定行を seed する（このセッションが heartbeat 対象という**前提**を実データで置く）。
///
/// 本テストは受け口（`resolve_target` → `run_one_heartbeat`）を駆動するので、この行の**列挙**
/// （`list_and_build_heartbeat_entries`）と interval 計算は通らない（それは bin 側で integration
/// test から不可視・scheduler 単体テストの範囲）。ここでは「configured なセッションだけが発火先に
/// なる」という前提を明示するために置く（seam 駆動でも観測結果は変わらない）。
fn seed_hb_config(core: &Core, session_id: &str) {
    let conn = core.extgate.db.lock().unwrap();
    opencrab_db::queries::upsert_session_heartbeat_config(
        &conn,
        &opencrab_db::queries::SessionHeartbeatConfigRow {
            agent_id: AGENT_ID.into(),
            session_id: session_id.into(),
            enabled: true,
            interval_secs: Some(60),
            anchor_at: None,
            last_fired_at: None,
        },
    )
    .unwrap();
}

/// scheduler 実経路（`resolve_target` → `run_one_heartbeat`）で heartbeat を 1 回起こす。
/// extgate descriptor 未登録なら `resolve_target` が None を返し発火しない（現 tip の #925 未実装状態）。
async fn fire_heartbeat_via_scheduler_seam(core: &Core, session_id: &str) {
    if let Some(target) = core
        .state
        .timed_fire_router
        .resolve_target(session_id, AGENT_ID)
    {
        opencrab_server::heartbeat_fire::run_one_heartbeat(&core.state, AGENT_ID, &target).await;
    }
}

/// 専用チャンネルで instance+binding を張り gateway を起動して ack を待つ（heartbeat 用・binding_id を返す）。
async fn wire_hb(core: &Core, fixture: &Fixture, channel: &str) -> (Arc<InstanceClient>, String) {
    let instance_id = uuid::Uuid::new_v4().to_string();
    let binding_id = uuid::Uuid::new_v4().to_string();
    let config_bytes = discord_config();
    let config_b64 = opencrab_extgate::encode_config_b64(&config_bytes);
    let addr = format!("discord-{AGENT_ID}-{GUILD}-{channel}");

    put_instance(core, &instance_id, &config_b64).await;
    put_binding(core, &binding_id, &instance_id, &addr).await;

    let place = InstancePlacement {
        instance_id: instance_id.clone(),
        revision: 1,
        addresses: vec![addr.clone()],
        config_b64,
    };
    let overrides = HarnessOverrides {
        fake_events: Some(fixture.path.clone()),
        dry_run: true,
    };
    let client = spawn_instance(core.sock.clone(), &place, &config_bytes, None, overrides)
        .expect("spawn_instance");

    let mut bound = false;
    for _ in 0..250 {
        if client.binding_for_address(&addr).await.is_some() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(bound, "binding が ack されない（heartbeat ch {channel}）");
    (client, binding_id)
}

/// instance+binding を DB に置くが gateway は起動しない（H4: gateway 停止中＝binding 未接続）。
async fn provision_hb_no_gateway(core: &Core, channel: &str) -> String {
    let instance_id = uuid::Uuid::new_v4().to_string();
    let binding_id = uuid::Uuid::new_v4().to_string();
    let config_bytes = discord_config();
    let config_b64 = opencrab_extgate::encode_config_b64(&config_bytes);
    let addr = format!("discord-{AGENT_ID}-{GUILD}-{channel}");
    put_instance(core, &instance_id, &config_b64).await;
    put_binding(core, &binding_id, &instance_id, &addr).await;
    binding_id
}

struct HbTwoSayMock {
    plain_calls: std::sync::atomic::AtomicUsize,
    total: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for HbTwoSayMock {
    fn name(&self) -> &str {
        "mock"
    }
    fn sends_max_output_tokens(&self) -> bool {
        false
    }
    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.total.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let text = request_text(&request);
        // heartbeat 起点ターン（system に [ハートビート]＋M_HB1）で 本文1＋CONTINUE → 本文2 の 2 分割。
        if text.contains(M_HB1) && !has_tool_role(&request) {
            let n = self
                .plain_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(match n {
                0 => text_response(&format!("{HB1_B1}\nCONTINUE")),
                _ => text_response(HB1_B2),
            });
        }
        Ok(text_response(FILLER))
    }
}

// ---------------------------------------------------------------------------
// H1: heartbeat 起点で 2 件投稿（say 2・保存 2・🏁 は 2 件目のみ・CONTINUE/NO_REPLY 残留 0・typing）。
// ---------------------------------------------------------------------------
#[tokio::test]
async fn heartbeat_h1_two_posts_flag_only_on_last() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(HbTwoSayMock {
        plain_calls: std::sync::atomic::AtomicUsize::new(0),
        total: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, binding_id) = wire_hb(&core, &fixture, CH_HB1).await;
    let session_id = format!("extgate-{binding_id}");
    set_hb_instructions(
        &core,
        &format!("{M_HB1} 巡回して報告することがあれば 2 回に分けて投稿して"),
    );
    seed_hb_config(&core, &session_id);

    fire_heartbeat_via_scheduler_seam(&core, &session_id).await;

    // 現 tip では descriptor 未登録 → 発火せず → 本文2 が出ないのでこの wait は false（→ 下の count 赤）。
    let done = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == CH_HB1 && c.body.contains(HB1_B2))
        })
        .await
    };
    assert!(
        done,
        "heartbeat 最終投稿（本文2）が出ない（現 tip: extgate 受け口が未登録・#925）: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(600)).await;

    let say_count = |body: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && c.channel == CH_HB1 && c.body.contains(body))
            .count()
    };
    // 配送 2: 本文1・本文2 が各 1 件（Discord チャンネル投稿）。
    assert_eq!(
        say_count(HB1_B1),
        1,
        "heartbeat 途中投稿（本文1）が 1 件配送されない（#925）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        say_count(HB1_B2),
        1,
        "heartbeat 最終投稿（本文2）が 1 件配送されない（#925）: {:?}",
        captured(&buf)
    );
    // LLM 回数: 本文1＋CONTINUE と 本文2 で 2 回（1 ターン＋継続分）。
    assert_eq!(
        mock.total.load(Ordering::SeqCst),
        2,
        "heartbeat の LLM 呼び出しが 2 回でない（1 ターン＋継続分）"
    );
    // 残留 0: どの say 本文にも "CONTINUE"/"NO_REPLY" が出ない。
    assert!(
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "say"
                && c.channel == CH_HB1
                && (c.body.contains(HB1_B1) || c.body.contains(HB1_B2)))
            .all(|c| !c.body.contains("CONTINUE") && !c.body.contains("NO_REPLY")),
        "heartbeat の say に CONTINUE/NO_REPLY が残留: {:?}",
        captured(&buf)
    );
    // 保存 2 行（memory_sessions・per-core DB で隔離）。
    assert_eq!(
        own_speech_rows(&core),
        2,
        "heartbeat の自 speech 保存が 2 行でない"
    );
    // 🏁: 2 件目のみ 1・1 件目に 0（own message id で相関・say ごと付与なら赤）。
    let mid_of = |body: &str| -> Option<String> {
        captured(&buf)
            .iter()
            .find(|c| c.kind == "say" && c.channel == CH_HB1 && c.body.contains(body))
            .map(|c| c.message.clone())
            .filter(|m| !m.is_empty())
    };
    let m1 = mid_of(HB1_B1).expect("本文1 の message id");
    let m2 = mid_of(HB1_B2).expect("本文2 の message id");
    let completed_on = |id: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == id
            })
            .count()
    };
    assert_eq!(
        completed_on(&m1),
        0,
        "🏁 が heartbeat 途中投稿に誤付与（1 件目に付けない）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&m2),
        1,
        "🏁 が heartbeat 最終投稿に 1 件で付かない: {:?}",
        captured(&buf)
    );
    // typing（Discord のみ・§5.4）: activity started で入力中が立ち、ended で止まる。
    //
    // ② 開始: 発話ありターンで typing が観測される（CH_HB1 に 1 件以上）。
    assert!(
        count_kind_on_channel(&buf, "typing", CH_HB1) >= 1,
        "heartbeat ターンで typing（入力中）が観測されない（Discord・§5.4 開始）: {:?}",
        captured(&buf)
    );
    // ③ 停止（#915・DIRECTION-LOG 625「入力中が残る」不具合の観測点）: 最終投稿の後（activity
    // ended 後）は typing keepalive を **1 tick も打たない**。
    //
    // 決定化: typing tick は `spawn_channel_typing` の別 async タスクが打つため、say とのログ index
    // 順序は非決定的（順序 assert はフルスイートでフレークする・実測）。そこで index 比較ではなく
    // **「ended 以後 typing が増えない」**で評価する（team-lead 裁定の決定形）。keepalive の間隔は
    // `TYPING_REFRESH_INTERVAL=8s`。ターン完了（本文2 配送＋settle でこの時点は activity ended 済み）
    // 後の typing 数を基準に、8s を超えて待って tick が増えないことを確認する。keepalive が ended で
    // 止まっていなければ（#915 の残存）+8s で必ず 1 tick 増えるので、決定的に捕捉できる。
    let typing_after_turn = count_kind_on_channel(&buf, "typing", CH_HB1);
    tokio::time::sleep(Duration::from_secs(9)).await; // > TYPING_REFRESH_INTERVAL(8s)
    assert_eq!(
        count_kind_on_channel(&buf, "typing", CH_HB1),
        typing_after_turn,
        "最終投稿の後も typing keepalive が残っている（#915・DIRECTION-LOG 625: activity ended で停止していない）: {:?}",
        captured(&buf)
    );
}

// ---------------------------------------------------------------------------
// H2: NO_REPLY のみ（発火はするが沈黙。say 0・保存 0・🏁 0・🤐 0）。
//   赤の signal: 発火自体が起きないので LLM 呼び出しが 1 回に達しない（現 tip 0）。緑では発火して
//   NO_REPLY を返し say/保存は 0、沈黙 heartbeat に 🤐 を付けない（裁定 2）。
// ---------------------------------------------------------------------------
struct HbSilentMock {
    total: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for HbSilentMock {
    fn name(&self) -> &str {
        "mock"
    }
    fn sends_max_output_tokens(&self) -> bool {
        false
    }
    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.total.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let text = request_text(&request);
        if text.contains(M_HB2) && !has_tool_role(&request) {
            return Ok(text_response("NO_REPLY"));
        }
        Ok(text_response(FILLER))
    }
}

#[tokio::test]
async fn heartbeat_h2_no_reply_stays_silent() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(HbSilentMock {
        total: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, binding_id) = wire_hb(&core, &fixture, CH_HB2).await;
    let session_id = format!("extgate-{binding_id}");
    set_hb_instructions(&core, &format!("{M_HB2} 特に無ければ何もしないで"));
    seed_hb_config(&core, &session_id);

    fire_heartbeat_via_scheduler_seam(&core, &session_id).await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 発火して 1 ターンは走る（緑）: 現 tip は発火しないので 0 → 赤。
    assert_eq!(
        mock.total.load(Ordering::SeqCst),
        1,
        "heartbeat が発火して 1 ターン走らない（現 tip: extgate 受け口が未登録・#925）"
    );
    // 沈黙: 配送 0・保存 0。
    assert_eq!(
        count_kind_on_channel(&buf, "say", CH_HB2),
        0,
        "NO_REPLY のみの heartbeat で say が配送された: {:?}",
        captured(&buf)
    );
    assert_eq!(
        own_speech_rows(&core),
        0,
        "NO_REPLY のみの heartbeat で speech が保存された"
    );
    // 🏁 0・🤐 0（沈黙 heartbeat には 🤐 を付けない・裁定 2・発端メッセージが無い）。
    let sys_on_ch = |emoji: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(emoji) && c.channel == CH_HB2
            })
            .count()
    };
    assert_eq!(sys_on_ch(SYS_COMPLETED), 0, "沈黙 heartbeat に 🏁 が付いた");
    assert_eq!(
        sys_on_ch("🤐"),
        0,
        "沈黙 heartbeat に 🤐 が付いた（裁定 2）"
    );
}

// ---------------------------------------------------------------------------
// H3: 宣言 → subtask（execute_shell）→ 完了報告。宣言に 🏁 0・報告に 🏁 1（§13.4.1 型）。
// ---------------------------------------------------------------------------
struct HbDeclMock {
    emitted: std::sync::atomic::AtomicBool,
    total: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for HbDeclMock {
    fn name(&self) -> &str {
        "mock"
    }
    fn sends_max_output_tokens(&self) -> bool {
        false
    }
    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.total.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // spawn ターンの ack（合成 "spawned" 結果＝tool role）→ NO_REPLY で追加投稿せず閉じる。
        if has_tool_role(&request) {
            return Ok(text_response("NO_REPLY"));
        }
        // 初回（heartbeat 起点・tool role 無し・1 回だけ）→ 宣言本文＋execute_shell を同一生成（holding）。
        if !self.emitted.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(shell_with_content_response(HB3_DECL, "echo", &[HB3_ECHO]));
        }
        // subtask 決着後の resume ターン → 完了報告 say。
        Ok(text_response(HB3_REPORT))
    }
}

#[tokio::test]
async fn heartbeat_h3_declaration_then_subtask_then_report() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(HbDeclMock {
        emitted: std::sync::atomic::AtomicBool::new(false),
        total: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let (_client, binding_id) = wire_hb(&core, &fixture, CH_HB3).await;
    let session_id = format!("extgate-{binding_id}");
    set_hb_instructions(
        &core,
        &format!("{M_HB3} 宣言してから subtask で作業して、終わったら報告して"),
    );
    seed_hb_config(&core, &session_id);

    fire_heartbeat_via_scheduler_seam(&core, &session_id).await;

    let both = {
        let buf = buf.clone();
        wait_until(move || {
            let caps = captured(&buf);
            caps.iter()
                .any(|c| c.kind == "say" && c.channel == CH_HB3 && c.body.contains(HB3_DECL))
                && caps
                    .iter()
                    .any(|c| c.kind == "say" && c.channel == CH_HB3 && c.body.contains(HB3_REPORT))
        })
        .await
    };
    assert!(
        both,
        "heartbeat の宣言 say と resume 完了報告 say が揃わない（現 tip: extgate 受け口が未登録・#925）: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(600)).await;

    let say_count = |body: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && c.channel == CH_HB3 && c.body.contains(body))
            .count()
    };
    assert_eq!(
        say_count(HB3_DECL),
        1,
        "heartbeat 宣言が 1 件配送されない: {:?}",
        captured(&buf)
    );
    assert_eq!(
        say_count(HB3_REPORT),
        1,
        "heartbeat 完了報告が 1 件配送されない: {:?}",
        captured(&buf)
    );
    assert!(
        mock.total.load(Ordering::SeqCst) >= 2,
        "heartbeat の宣言ターン＋resume ターンが走らない"
    );
    // ack の NO_REPLY は投稿しない（このチャンネルの say は宣言＋報告の 2 件だけ）。
    assert_eq!(
        count_kind_on_channel(&buf, "say", CH_HB3),
        say_count(HB3_DECL) + say_count(HB3_REPORT),
        "heartbeat で宣言/報告以外の say（NO_REPLY 等）が配送された: {:?}",
        captured(&buf)
    );
    // 保存 2 行（宣言＋報告）。
    assert_eq!(
        own_speech_rows(&core),
        2,
        "heartbeat の自 speech 保存が宣言＋報告の 2 行でない"
    );
    // 🏁: 宣言に 0（進行中）・報告に 1。
    let mid_of = |body: &str| -> Option<String> {
        captured(&buf)
            .iter()
            .find(|c| c.kind == "say" && c.channel == CH_HB3 && c.body.contains(body))
            .map(|c| c.message.clone())
            .filter(|m| !m.is_empty())
    };
    let decl_mid = mid_of(HB3_DECL).expect("宣言 say の message id");
    let report_mid = mid_of(HB3_REPORT).expect("報告 say の message id");
    let completed_on = |id: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == id
            })
            .count()
    };
    assert_eq!(
        completed_on(&decl_mid),
        0,
        "🏁 が heartbeat 宣言 say に誤付与（進行中は付けない）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&report_mid),
        1,
        "🏁 が heartbeat 完了報告 say に 1 件で付かない: {:?}",
        captured(&buf)
    );
}

// ---------------------------------------------------------------------------
// H4: binding 未接続（gateway 停止中）に時刻到来 → **warn 1**・配送 0・LLM 0・投稿捏造 0（fail-loud）。
//   binding 行は DB にあるが instance が live でない（spawn しない）。赤先行の観測境界は
//   §2.1 の **warn 1**: 現 tip は descriptor 未登録 → resolve_target None → seam が skip（warn 0）→ 赤。
//   実装後は resolve_target が解決し、sink の live 判定で fail-loud（session_id 付き warn 1・無配送・
//   ターン未実行）→ 緑。配送 0・LLM 0・捏造 0 は「未接続でも投稿を捏造しない」否定側ガード。
// ---------------------------------------------------------------------------
struct HbDownMock {
    total: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for HbDownMock {
    fn name(&self) -> &str {
        "mock"
    }
    fn sends_max_output_tokens(&self) -> bool {
        false
    }
    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.total.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // 未接続なのに呼ばれたら捏造。出たら不具合として検知する。
        Ok(text_response(HB4_FABRICATED))
    }
}

#[tokio::test]
async fn heartbeat_h4_unbound_gateway_fires_nothing() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(HbDownMock {
        total: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    // gateway は起動しない（binding 行だけ DB に置く＝未接続）。
    let binding_id = provision_hb_no_gateway(&core, CH_HB4).await;
    let session_id = format!("extgate-{binding_id}");
    set_hb_instructions(&core, "HBFOURMARK 未接続時の発火を検証する");
    seed_hb_config(&core, &session_id);

    fire_heartbeat_via_scheduler_seam(&core, &session_id).await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 赤の signal（§2.1 warn 1・§1.5 fail-loud）: 未接続 binding へ時刻が到来したら、発火せず
    // **この session_id を持つ warn を 1 件**残す。現 tip は extgate descriptor 未登録で
    // resolve_target が None → seam が黙って skip（warn 0）→ 赤。実装後は resolve_target は解決し、
    // sink が binding 未接続（instance が live でない）を検知して発火を諦め warn 1 → 緑。
    assert_eq!(
        warns_with_session(&session_id),
        1,
        "未接続 binding の時刻到来で fail-loud warn（session_id 付き）が 1 件出ない（§1.5・現 tip は skip で warn 0）"
    );
    // 配送 0（捏造投稿 0）。
    assert_eq!(
        count_kind_on_channel(&buf, "say", CH_HB4),
        0,
        "未接続 binding へ heartbeat が投稿を捏造した: {:?}",
        captured(&buf)
    );
    assert!(
        captured(&buf)
            .iter()
            .all(|c| !c.body.contains(HB4_FABRICATED)),
        "未接続 binding で捏造本文が配送された: {:?}",
        captured(&buf)
    );
    // LLM 0（ターンを走らせない・fail-loud）。
    assert_eq!(
        mock.total.load(Ordering::SeqCst),
        0,
        "未接続 binding で LLM ターンが走った（fail-loud で走らせない）"
    );
    // 保存 0。
    assert_eq!(
        own_speech_rows(&core),
        0,
        "未接続 binding で speech が保存された"
    );
}
