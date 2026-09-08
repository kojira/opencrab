    // ---- #394: 次回の窓（境界と広さ）を本人が決める ----

    fn pref(ctx: &ActionContext) -> Option<opencrab_db::queries::DeclareWindowPref> {
        let conn = ctx.db.lock().unwrap();
        opencrab_db::queries::get_memory_declare_window(&conn, &ctx.agent_id).unwrap()
    }

    /// 位置と広さを書ける。指定しなかった項目は**前の指定を消さない**（部分更新）。
    #[tokio::test]
    async fn plan_window_records_and_merges_fields() {
        let (_d, ctx) = test_context();

        // 位置だけ表明する。
        let r = PlanNextMemoryWindowAction
            .execute(
                &json!({"next_from_id": 23_600, "note": "この出来事はまだ続いている"}),
                &ctx,
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        let p = pref(&ctx).expect("希望が保存される");
        assert_eq!(p.next_from_id, Some(23_600));
        assert_eq!(p.window_size, None);
        assert_eq!(p.note.as_deref(), Some("この出来事はまだ続いている"));
        assert!(p.updated_at.is_some());

        // 広さだけ表明しても、位置は消えない（部分更新）。
        let r = PlanNextMemoryWindowAction
            .execute(&json!({"window_size": 450}), &ctx)
            .await;
        assert!(r.success, "{:?}", r.error);
        let p = pref(&ctx).unwrap();
        assert_eq!(p.next_from_id, Some(23_600), "位置が消えてはいけない");
        assert_eq!(p.window_size, Some(450));
    }

    /// 広さの**下限だけ**道具が丸め、**上限は丸めない**（上限は運用の `max_logs` との `max` で
    /// 決まり、config を持つのはラン側だけ）。ここで 600 に丸めると、`max_logs` を 600 超に
    /// している運用で本人が表明した瞬間に窓が**狭まる**（黙っていれば広いままだった）。
    #[tokio::test]
    async fn plan_window_raises_to_min_but_does_not_cap_at_max() {
        let (_d, ctx) = test_context();

        // 上限より大きい希望は**そのまま記録する**（ラン側が config を見て丸める）。
        let r = PlanNextMemoryWindowAction
            .execute(&json!({"window_size": DECLARE_WINDOW_MAX + 400}), &ctx)
            .await;
        assert!(r.success);
        let data = r.data.unwrap();
        assert_eq!(data["window_size"], json!(DECLARE_WINDOW_MAX + 400));
        assert_eq!(data["window_size_raised_to_min"], json!(false));
        assert_eq!(
            pref(&ctx).unwrap().window_size,
            Some(DECLARE_WINDOW_MAX + 400),
            "道具が上限で丸めると、広い max_logs の運用で窓がむしろ狭まる"
        );
        // 上限の既定は伝える（本人が「そのまま通る」と誤解しないように）。
        assert_eq!(data["window_size_max_default"], json!(DECLARE_WINDOW_MAX));

        // 下限は config に依らないので道具が丸め、丸めたことを伝える。
        let r = PlanNextMemoryWindowAction
            .execute(&json!({"window_size": 1}), &ctx)
            .await;
        assert!(r.success);
        let data = r.data.unwrap();
        assert_eq!(data["window_size"], json!(DECLARE_WINDOW_MIN));
        assert_eq!(data["window_size_raised_to_min"], json!(true));
        assert_eq!(pref(&ctx).unwrap().window_size, Some(DECLARE_WINDOW_MIN));

        // 範囲内はそのまま。
        let r = PlanNextMemoryWindowAction
            .execute(&json!({"window_size": 200}), &ctx)
            .await;
        let data = r.data.unwrap();
        assert_eq!(data["window_size"], json!(200));
        assert_eq!(data["window_size_raised_to_min"], json!(false));
    }

    /// 全プロパティを空値で埋めてくるモデル（#388 の癖）は「指定なし」として拒否する。
    /// `0` を位置として呑むと、本人が意図しない巻き戻しの希望が立ってしまう。
    #[tokio::test]
    async fn plan_window_rejects_all_empty_values() {
        let (_d, ctx) = test_context();
        let r = PlanNextMemoryWindowAction
            .execute(
                &json!({"next_from_id": 0, "window_size": 0, "note": "  "}),
                &ctx,
            )
            .await;
        assert!(!r.success);
        assert_eq!(pref(&ctx), None, "何も書かれてはいけない");
    }

    /// 範囲が本当に該当なしなら、空であること（range_total=0）が返る（「なぜ空か」を伝える）。
    #[tokio::test]
    async fn read_id_range_genuinely_empty_reports_zero_total() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 20); // id 1..20

        // 100..200 には生ログが無い（両側とも意味を持つが該当なし）。
        let args = with_override(all_props_filled(), json!({"from_id": 100, "to_id": 200}));
        let r = ReadMyHistoryAction.execute(&args, &ctx).await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(
            data["range_total"], 0,
            "該当なしは range_total=0 で明示する"
        );
        assert_eq!(data["returned"], 0);
    }
