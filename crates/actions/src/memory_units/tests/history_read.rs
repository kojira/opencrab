    // ---- survey の返り値を上限内に収める（#386）----

    use opencrab_db::queries::{HistoryBucket, HistorySurvey};

    /// 種別内訳を詰めた「太い」バケットを `n` 件持つ survey を作る（新しい順を模す）。
    fn fat_survey(n: usize) -> HistorySurvey {
        let mut type_counts = std::collections::BTreeMap::new();
        type_counts.insert("speech".to_string(), 1234i64);
        type_counts.insert("system".to_string(), 987);
        type_counts.insert("tool_result".to_string(), 456);
        type_counts.insert("tool_call".to_string(), 321);
        type_counts.insert("inner_voice".to_string(), 210);
        let buckets: Vec<HistoryBucket> = (0..n)
            .map(|i| HistoryBucket {
                // 先頭ほど「新しい」ことにする（fit は先頭 keep 件を残す）。
                bucket: format!("2026-08-{:02}T{:02}", (n - i) / 24 % 28 + 1, (n - i) % 24),
                log_count: 300,
                session_count: 7,
                min_id: (i as i64) * 300,
                max_id: (i as i64) * 300 + 299,
                content_chars: 90_000,
                est_tokens: 60_000,
                type_counts: type_counts.clone(),
            })
            .collect();
        HistorySurvey {
            granularity: "hour".to_string(),
            total_logs: 300 * n as i64,
            total_sessions: 91,
            min_id: Some(0),
            max_id: Some(300 * n as i64),
            total_content_chars: 90_000 * n as i64,
            total_est_tokens: 60_000 * n as i64,
            total_buckets: n as i64,
            returned_buckets: n,
            truncated: false,
            buckets,
        }
    }

    fn tokens_of(s: &HistorySurvey) -> usize {
        opencrab_core::tokens::estimate_tokens(&serde_json::to_string(s).unwrap())
    }

    /// 大量・太いバケットでも、fit 後は予算に収まり、ラッパ込みでも #294 の上限未満。
    /// 集計メタ（総数・id 範囲・total_buckets）は落とさず、新しい側のバケットを残す。
    #[test]
    fn survey_fit_bounds_tokens_and_keeps_meta() {
        let mut survey = fat_survey(400);
        let head_before = survey.buckets[0].bucket.clone();
        assert!(
            tokens_of(&survey) > HISTORY_RESULT_TOKEN_BUDGET,
            "前提が崩れている（既に予算内）: {}",
            tokens_of(&survey)
        );

        fit_survey_to_budget(&mut survey, HISTORY_RESULT_TOKEN_BUDGET);

        // survey 単体で予算内。
        assert!(
            tokens_of(&survey) <= HISTORY_RESULT_TOKEN_BUDGET,
            "over budget: {}",
            tokens_of(&survey)
        );
        // ActionResult ラッパを被せても #294 の上限（2,500）未満。
        let wrapped = serde_json::to_string(&ActionResult::success(
            serde_json::to_value(&survey).unwrap(),
        ))
        .unwrap();
        assert!(
            opencrab_core::tokens::estimate_tokens(&wrapped)
                < opencrab_core::tool_result_log::TOOL_RESULT_TOKEN_LIMIT,
            "wrapped over inline limit: {}",
            opencrab_core::tokens::estimate_tokens(&wrapped)
        );
        // 集計メタは残る（バケットを削っても地図の骨格は保つ）。
        assert_eq!(survey.total_logs, 300 * 400);
        assert_eq!(survey.total_buckets, 400);
        assert_eq!(survey.min_id, Some(0));
        assert!(survey.truncated, "削ったら truncated が立つ");
        assert!(!survey.buckets.is_empty(), "地図が空になってはいけない");
        assert!(survey.buckets.len() < 400, "実際に削れている");
        assert_eq!(survey.returned_buckets, survey.buckets.len());
        // 新しい側（先頭）から残す。
        assert_eq!(survey.buckets[0].bucket, head_before);
    }

    /// 既に予算内の小さい survey は 1 バケットも削らない（truncated も立てない）。
    #[test]
    fn survey_fit_leaves_small_survey_untouched() {
        let mut survey = fat_survey(3);
        assert!(tokens_of(&survey) <= HISTORY_RESULT_TOKEN_BUDGET);
        fit_survey_to_budget(&mut survey, HISTORY_RESULT_TOKEN_BUDGET);
        assert_eq!(survey.buckets.len(), 3);
        assert!(!survey.truncated);
    }

    /// Action 経由（本番と同じ serialize 路）でも、最大バケット要求で上限内に収まる。
    #[tokio::test]
    async fn survey_action_fits_even_at_max_buckets() {
        let (_d, ctx) = test_context();
        // hour 粒度で沢山のバケットを作る（各時に 1 件ずつ、500 時間ぶん）。
        {
            let conn = ctx.db.lock().unwrap();
            for h in 0..500 {
                let ts = format!(
                    "2026-{:02}-{:02}T{:02}:00:00Z",
                    1 + h / 700,
                    1 + (h / 24) % 28,
                    h % 24
                );
                let content = format!("発話 {h} ").repeat(3);
                conn.execute(
                    "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, created_at) VALUES (?1,?2,?3,?4,?5)",
                    rusqlite::params!["agent-1", format!("s{h}"), "speech", content, ts],
                )
                .unwrap();
            }
        }
        let r = SurveyMyHistoryAction
            .execute(&json!({"granularity": "hour", "max_buckets": 400}), &ctx)
            .await;
        assert!(r.success);
        let wrapped = serde_json::to_string(&r).unwrap();
        let tokens = opencrab_core::tokens::estimate_tokens(&wrapped);
        assert!(
            tokens < opencrab_core::tool_result_log::TOOL_RESULT_TOKEN_LIMIT,
            "survey action over inline limit: {tokens}"
        );
        // 地図として最低限（総数と最低 1 バケット）は残る。
        let data = r.data.unwrap();
        assert!(data["total_logs"].as_i64().unwrap() >= 500);
        assert!(!data["buckets"].as_array().unwrap().is_empty());
        // サイズ地図として est_tokens と per-result 上限が載る（#386）。
        assert!(data["total_est_tokens"].as_i64().unwrap() > 0);
        assert_eq!(data["inline_limit_tokens"], json!(INLINE_LIMIT_TOKENS));
        assert!(data["buckets"][0]["est_tokens"].as_i64().is_some());
    }

    // ---- read: 取る前に大きさを知る / 1 ページを上限内に収める（#386）----

    /// 大量の生ログを積む（各 content は長め）。
    fn seed_big_logs(ctx: &ActionContext, n: usize, chars_each: usize) {
        let conn = ctx.db.lock().unwrap();
        let body = "あ".repeat(chars_each);
        for i in 0..n {
            conn.execute(
                "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, created_at)
                 VALUES ('agent-1', 's1', 'speech', ?1, ?2)",
                rusqlite::params![
                    format!("{body} {i}"),
                    format!("2026-08-01T00:00:{:02}Z", i % 60)
                ],
            )
            .unwrap();
        }
    }

    /// estimate_only は本文を返さず、件数・推定トークン・fits を返す。
    #[tokio::test]
    async fn read_estimate_only_reports_size_without_bodies() {
        let (_d, ctx) = test_context();
        seed_big_logs(&ctx, 60, 400); // 60 件 × ~400 文字 → 明らかに 2,500 トークン超

        let r = ReadMyHistoryAction
            .execute(
                &json!({"from_id": 1, "to_id": 60, "estimate_only": true}),
                &ctx,
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["estimate_only"], true);
        assert_eq!(data["range_total"], 60);
        assert!(data["estimated_tokens"].as_i64().unwrap() > INLINE_LIMIT_TOKENS as i64);
        assert_eq!(data["fits"], false);
        assert_eq!(data["inline_limit_tokens"], json!(INLINE_LIMIT_TOKENS));
        // 本文（rows）は返さない。
        assert!(data.get("rows").is_none());
        // 推定は、実際に取ったときのサイズと大きく食い違わない（同オーダー）。
        let full = ReadMyHistoryAction
            .execute(&json!({"from_id": 1, "to_id": 60}), &ctx)
            .await;
        let full_wrapped = serde_json::to_string(&full).unwrap();
        // 実結果は 1 ページに収まっている（下のテストで担保）が、推定は全 60 件ぶんなので
        // 実結果より大きいはず（推定 > 1 ページ）。少なくとも推定は正の値。
        assert!(data["estimated_tokens"].as_i64().unwrap() > 0);
        assert!(!full_wrapped.is_empty());
    }

    /// 通常の read は 1 ページを必ず inline 上限内に収め、estimated_tokens と上限を添える。
    #[tokio::test]
    async fn read_page_fits_inline_limit_and_reports_tokens() {
        let (_d, ctx) = test_context();
        seed_big_logs(&ctx, 60, 400);

        let r = ReadMyHistoryAction
            .execute(&json!({"from_id": 1, "to_id": 60}), &ctx)
            .await;
        assert!(r.success);
        // ラッパ込み（LLM が見る本文）で #294 の上限未満＝潰れない。
        let wrapped = serde_json::to_string(&r).unwrap();
        assert!(
            opencrab_core::tokens::estimate_tokens(&wrapped)
                < opencrab_core::tool_result_log::TOOL_RESULT_TOKEN_LIMIT,
            "read page over inline limit: {}",
            opencrab_core::tokens::estimate_tokens(&wrapped)
        );
        let data = r.data.unwrap();
        // トークンで打ち切ったので続きがある。
        assert_eq!(data["truncated"], true);
        assert!(data["next_from_id"].as_i64().is_some());
        assert!(data["returned"].as_u64().unwrap() < 60);
        assert!(data["estimated_tokens"].as_i64().unwrap() > 0);
        assert_eq!(data["inline_limit_tokens"], json!(INLINE_LIMIT_TOKENS));
    }

    // ---- #388: モデルは全プロパティを埋める。値ベース判定で正しいモードに解決する ----
    //
    // 既存の単体テストは引数を「そのモードのキーだけ」明示的に組むので、この失敗を
    // 再現しない（だから実験 2 回まで見つからなかった）。ここでは gpt-5.6-sol の実際の
    // 癖——スキーマの全プロパティを毎回 `""` / `0` で埋める——を模したうえで、意味の
    // ある値を 1 つだけ足し、各モードが正しく解決することを確認する。

    /// モデルが毎回埋めてくる「全プロパティが空値」の引数（read_my_history のスキーマ全て）。
    fn all_props_filled() -> serde_json::Value {
        json!({
            "session_id": "",
            "from_id": 0,
            "to_id": 0,
            "around_id": 0,
            "from_time": "",
            "to_time": "",
            "estimate_only": false,
            "cursor_from_id": 0
        })
    }

    /// `base` に `over` のキーを上書きした引数を作る。
    fn with_override(mut base: serde_json::Value, over: serde_json::Value) -> serde_json::Value {
        let obj = base.as_object_mut().unwrap();
        for (k, v) in over.as_object().unwrap() {
            obj.insert(k.clone(), v.clone());
        }
        base
    }

    /// 全プロパティが空値で埋まった状態＋意味のある `around_id` だけ → around モードで動く。
    /// これが #388 で 29 回連続拒否された、実際の呼び出しの形。
    #[tokio::test]
    async fn read_all_props_filled_plus_around_works() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 20); // id 1..20

        let args = with_override(all_props_filled(), json!({"around_id": 10}));
        let r = ReadMyHistoryAction.execute(&args, &ctx).await;
        assert!(
            r.success,
            "全プロパティ埋め＋around が『範囲は1つだけ』で拒否された: {:?}",
            r.error
        );
        assert!(r.data.unwrap()["returned"].as_u64().unwrap() > 0);
    }

    /// 各モードについて、他のキーが全部空値で埋まっていても正しく解決すること。
    #[tokio::test]
    async fn read_each_mode_resolves_when_others_are_empty_valued() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 20); // id 1..20 / session-1

        // session モード。
        let r = ReadMyHistoryAction
            .execute(
                &with_override(all_props_filled(), json!({"session_id": "session-1"})),
                &ctx,
            )
            .await;
        assert!(r.success, "session モードが拒否された: {:?}", r.error);
        assert!(r.data.unwrap()["returned"].as_u64().unwrap() > 0);

        // id 範囲モード。
        let r = ReadMyHistoryAction
            .execute(
                &with_override(all_props_filled(), json!({"from_id": 5, "to_id": 15})),
                &ctx,
            )
            .await;
        assert!(r.success, "id 範囲モードが拒否された: {:?}", r.error);
        assert_eq!(r.data.unwrap()["returned"], 11);

        // around モード。
        let r = ReadMyHistoryAction
            .execute(
                &with_override(all_props_filled(), json!({"around_id": 10})),
                &ctx,
            )
            .await;
        assert!(r.success, "around モードが拒否された: {:?}", r.error);
        assert!(r.data.unwrap()["returned"].as_u64().unwrap() > 0);

        // 時刻範囲モード（seed の created_at は「今」なので広い範囲で確実に捕まえる）。
        let r = ReadMyHistoryAction
            .execute(
                &with_override(
                    all_props_filled(),
                    json!({
                        "from_time": "2000-01-01T00:00:00Z",
                        "to_time": "2100-01-01T00:00:00Z"
                    }),
                ),
                &ctx,
            )
            .await;
        assert!(r.success, "時刻範囲モードが拒否された: {:?}", r.error);
        assert!(r.data.unwrap()["returned"].as_u64().unwrap() > 0);
    }

    /// 全プロパティ埋め＋意味のある値ゼロ → 「範囲が必要」で拒否される（誤って通さない）。
    #[tokio::test]
    async fn read_all_props_filled_but_no_meaningful_value_is_rejected() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 4);
        let r = ReadMyHistoryAction.execute(&all_props_filled(), &ctx).await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("範囲指定が必要"));
    }

    /// 意味のある値が 2 つあれば従来どおり排他で拒否する（排他は維持する / #388）。
    #[tokio::test]
    async fn read_two_meaningful_ranges_still_rejected() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 20);
        let r = ReadMyHistoryAction
            .execute(
                &with_override(
                    all_props_filled(),
                    json!({"session_id": "session-1", "around_id": 10}),
                ),
                &ctx,
            )
            .await;
        assert!(!r.success);
        let err = r.error.unwrap();
        assert!(err.contains("1 つだけ"));
        // 全プロパティを埋めるモデルが 1 回で復帰できるよう「他をどう消すか」を示す。
        assert!(
            err.contains("0 か空文字"),
            "拒否メッセージに復帰方法が無い: {err}"
        );
    }

    /// 全プロパティ埋め＋around＋estimate_only=true → 範囲判定を抜けて estimate が返る。
    /// 実験では estimate_only を 5 回渡したが、range 判定が先に落ちて一度も発火しなかった。
    #[tokio::test]
    async fn read_all_props_filled_estimate_only_now_reaches_estimate() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 20);
        let args = with_override(
            all_props_filled(),
            json!({"around_id": 10, "estimate_only": true}),
        );
        let r = ReadMyHistoryAction.execute(&args, &ctx).await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["estimate_only"], true);
        assert!(data.get("rows").is_none());
    }

    // ---- #388 追補: 片側だけの id 範囲を空結果にせず素直に読む ----

    /// `from_id` だけ意味あり（`to_id` は空値）→ そこから先を読む。全プロパティ埋めでも動く。
    #[tokio::test]
    async fn read_id_range_from_only_reads_onward() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 20); // id 1..20

        let args = with_override(all_props_filled(), json!({"from_id": 15}));
        let r = ReadMyHistoryAction.execute(&args, &ctx).await;
        assert!(
            r.success,
            "from_id 片側指定が拒否/空になった: {:?}",
            r.error
        );
        let data = r.data.unwrap();
        // id 15..20 の 6 件。0 を境界に使うと逆向き（1..15）になり返り値が変わる。
        assert_eq!(data["range_total"], 6, "from 15 以降を読むべき");
        assert_eq!(data["returned"], 6);
    }

    /// `to_id` だけ意味あり（`from_id` は空値）→ そこまでを読む。全プロパティ埋めでも動く。
    #[tokio::test]
    async fn read_id_range_to_only_reads_up_to() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 20); // id 1..20

        let args = with_override(all_props_filled(), json!({"to_id": 5}));
        let r = ReadMyHistoryAction.execute(&args, &ctx).await;
        assert!(r.success, "to_id 片側指定が拒否/空になった: {:?}", r.error);
        let data = r.data.unwrap();
        // id 1..5 の 5 件。
        assert_eq!(data["range_total"], 5, "5 まで読むべき");
        assert_eq!(data["returned"], 5);
    }

