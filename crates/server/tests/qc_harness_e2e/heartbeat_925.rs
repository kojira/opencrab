// ===========================================================================
// #925 §13.4.3【V3 heartbeat 受け口・Nostr レーン】TDD 赤先行。
//
// §2.1（裁定・§1.6）: 配送・保存・LLM・残留は Discord と同一の数値。差は「🏁/🤐/typing は
// Discord のみ・Nostr は対象なし」だけ。よって Nostr 側は H1 の say 2 件が **standalone
// timeline post**（`deliver_say`・DI-16）として出て、保存 2 行・残留 0 になることを固定する
// （🏁/🤐/typing は Nostr に無いので assert しない）。
//
// 赤の駆動は Discord と同一 seam（`resolve_target` → `run_one_heartbeat`）。ハーネスの
// `timed_fire_router` は空なので extgate descriptor 未登録 → 発火せず → standalone 0 → 赤。
// 観測境界: dry-run kind="standalone"（配送）・memory_sessions speech（保存）・mock 呼び出し数。
// ===========================================================================

const M_HBN: &str = "HBNOSTRMARK";
const HBN_B1: &str = "hbn-巡回して見つけたこと その1（nostr heartbeat 途中発話）";
const HBN_B2: &str = "hbn-見つけたこと その2（nostr heartbeat 最終）";

fn hb_set_instructions(core: &Core, text: &str) {
    let conn = core.extgate.db.lock().unwrap();
    conn.execute(
        "UPDATE agents SET heartbeat_instructions=?1 WHERE agent_id=?2",
        [text, AGENT_ID],
    )
    .unwrap();
}

fn hb_seed_config(core: &Core, session_id: &str) {
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

fn hb_own_speech_rows(core: &Core) -> i64 {
    let conn = core.extgate.db.lock().unwrap();
    conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech' AND speaker_id='{AGENT_ID}'"
        ),
        [],
        |r| r.get(0),
    )
    .unwrap()
}

/// scheduler 実経路（scheduler.rs:152 と同一の `resolve_target`）→ `run_one_heartbeat`。
/// extgate descriptor 未登録なら None を返し発火しない（現 tip の #925 未実装状態）。
async fn hb_fire_via_scheduler_seam(core: &Core, session_id: &str) {
    if let Some(target) = core
        .state
        .timed_fire_router
        .resolve_target(session_id, AGENT_ID)
    {
        opencrab_server::heartbeat_fire::run_one_heartbeat(&core.state, AGENT_ID, &target).await;
    }
}

struct HbNostrTwoSayMock {
    plain_calls: std::sync::atomic::AtomicUsize,
    total: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for HbNostrTwoSayMock {
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
        if text.contains(M_HBN) && !has_tool_role(&request) {
            let n = self
                .plain_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(match n {
                0 => text_response(HBN_B1),
                _ => text_response(&format!("{HBN_B2}\nNO_REPLY")),
            });
        }
        Ok(text_response("NO_REPLY"))
    }
}

// ---------------------------------------------------------------------------
// H1（Nostr）: heartbeat 起点で 2 件が standalone timeline post として配送・保存 2 行・残留 0。
//   🏁/🤐/typing は Nostr に無いので assert しない（対象なし・裁定 3）。
// ---------------------------------------------------------------------------
#[tokio::test]
async fn heartbeat_h1_nostr_two_standalone_posts() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(HbNostrTwoSayMock {
        plain_calls: std::sync::atomic::AtomicUsize::new(0),
        total: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;
    hb_set_instructions(
        &core,
        &format!("{M_HBN} 巡回して報告することがあれば 2 回に分けて投稿して"),
    );
    hb_seed_config(&core, &session_id);

    hb_fire_via_scheduler_seam(&core, &session_id).await;

    // 現 tip では descriptor 未登録 → 発火せず → 最終投稿が出ないのでこの wait は false（→ 下の count 赤）。
    let done = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "standalone" && c.body.contains(HBN_B2))
        })
        .await
    };
    assert!(
        done,
        "nostr heartbeat 最終投稿（本文2）が standalone post に出ない（現 tip: extgate 受け口が未登録・#925）: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(600)).await;

    let standalone_count = |body: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "standalone" && c.body.contains(body))
            .count()
    };
    // 配送 2: 本文1・本文2 が各 1 件、いずれも standalone timeline post（DI-16）。
    assert_eq!(
        standalone_count(HBN_B1),
        1,
        "nostr heartbeat 途中投稿（本文1）が standalone post 1 件で出ない（#925）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        standalone_count(HBN_B2),
        1,
        "nostr heartbeat 最終投稿（本文2）が standalone post 1 件で出ない（#925）: {:?}",
        captured(&buf)
    );
    // LLM 回数: 本文1＋継続 と 本文2 で 2 回（Discord と同一）。
    assert_eq!(
        mock.total.load(Ordering::SeqCst),
        2,
        "nostr heartbeat の LLM 呼び出しが 2 回でない（1 ターン＋継続分・Discord と同一）"
    );
    // 残留 0: どの standalone 本文にも 継続/NO_REPLY が出ない。
    assert!(
        captured(&buf)
            .iter()
            .filter(
                |c| c.kind == "standalone" && (c.body.contains(HBN_B1) || c.body.contains(HBN_B2))
            )
            .all(|c| !c.body.contains("継続") && !c.body.contains("NO_REPLY")),
        "nostr heartbeat の standalone post に 継続/NO_REPLY が残留: {:?}",
        captured(&buf)
    );
    // 保存 2 行（Discord と同一・per-core DB で隔離）。
    assert_eq!(
        hb_own_speech_rows(&core),
        2,
        "nostr heartbeat の自 speech 保存が 2 行でない（Discord と同一）"
    );
}
