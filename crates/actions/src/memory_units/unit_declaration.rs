/// 生ログの範囲 `[from_id, to_id]` を 1 つの記憶として宣言する。
pub struct RecordMemoryUnitAction;

#[async_trait]
impl Action for RecordMemoryUnitAction {
    fn name(&self) -> &str {
        "record_memory_unit"
    }

    fn description(&self) -> &str {
        "自分の生ログの範囲 [from_id, to_id] を『1 つの記憶』として宣言する。title 必須・summary 任意・tags 任意。生ログは消さず、宣言は retract_memory_unit で取り消せる。重なり（1 範囲が複数の宣言に属する）は許される。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["from_id", "to_id", "title"],
            "properties": {
                "from_id": { "type": "integer", "description": "範囲の下端（生ログの id）" },
                "to_id": { "type": "integer", "description": "範囲の上端（生ログの id）" },
                "title": { "type": "string", "description": "この記憶のタイトル（必須）" },
                "summary": { "type": "string", "description": "要約（任意）" },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "付けるタグ名（任意・複数可・無い名前は新設）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let from_id = match args["from_id"].as_i64() {
            Some(v) => v,
            None => return ActionResult::error("from_id は integer で指定してください"),
        };
        let to_id = match args["to_id"].as_i64() {
            Some(v) => v,
            None => return ActionResult::error("to_id は integer で指定してください"),
        };
        let title = match args["title"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("title is required"),
        };
        let summary = args["summary"].as_str().unwrap_or("").to_string();
        let tags: Vec<String> = args["tags"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        // 範囲にこのエージェントの生ログが実在するか確認し、date_from/date_to を埋める
        // （他エージェントの id や空範囲を宣言させない）。
        let meta = match opencrab_db::queries::log_range_meta(&conn, &ctx.agent_id, from_id, to_id) {
            Ok(Some(m)) => m,
            Ok(None) => {
                return ActionResult::error(
                    "指定範囲にこのエージェントの生ログがありません（from_id / to_id を確認してください）",
                )
            }
            Err(e) => return ActionResult::error(&format!("範囲の確認に失敗しました: {e}")),
        };
        let now = chrono::Utc::now().to_rfc3339();
        let node = match opencrab_db::queries::record_memory_unit(
            &conn,
            &ctx.agent_id,
            &title,
            &summary,
            from_id,
            to_id,
            Some(&meta.min_created_at),
            Some(&meta.max_created_at),
            &now,
        ) {
            Ok(n) => n,
            Err(e) => return ActionResult::error(&format!("記憶の宣言に失敗しました: {e}")),
        };

        // タグ（任意）。宣言ノード id を topic_id として既存タグ基盤へ付与する。
        // 付与失敗は宣言自体を無効にしない（宣言は成立済み・agent が付け直せる）。
        let mut tag_error: Option<String> = None;
        if !tags.is_empty() {
            if let Err(e) =
                opencrab_db::queries::tag_topic(&conn, &ctx.agent_id, &node.id, &tags, &now)
            {
                tag_error = Some(format!("タグ付けに失敗しました: {e}"));
            }
        }

        ActionResult::success(json!({
            "unit_id": node.id,
            "short_id": node.short_id,
            "title": node.title,
            "from_id": from_id,
            "to_id": to_id,
            "date_from": node.date_from,
            "date_to": node.date_to,
            "logs_in_range": meta.count,
            "tags": tags,
            "tag_error": tag_error,
        }))
    }
}

/// 宣言ユニットを取り消す（宣言ノード + FTS + member のみ削除。生ログは不変）。
pub struct RetractMemoryUnitAction;

#[async_trait]
impl Action for RetractMemoryUnitAction {
    fn name(&self) -> &str {
        "retract_memory_unit"
    }

    fn description(&self) -> &str {
        "record_memory_unit で宣言した記憶を取り消す。宣言ノードと FTS 行と付けたタグの付与だけを消す。生ログには触らない（何度でもやり直せる）。宣言ユニット以外は消せない。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["unit_id"],
            "properties": {
                "unit_id": {
                    "type": "string",
                    "description": "取り消す宣言ユニットの short_id またはフル node_id"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let unit_id = match args["unit_id"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("unit_id is required"),
        };
        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        match opencrab_db::queries::retract_memory_unit(&conn, &ctx.agent_id, &unit_id) {
            Ok(full_id) => ActionResult::success(json!({
                "retracted": true,
                "unit_id": full_id,
            })),
            Err(e) => ActionResult::error(&format!("宣言の取り消しに失敗しました: {e}")),
        }
    }
}

/// 宣言ランの窓の広さ（生ログ件数）として本人が指定できる**下限**（#394）。
///
/// これより狭くすると材料が薄すぎて宣言が抽象論に落ちる（#313 の実測: 20 件では固有名詞の
/// 無い抽象タグしか出なかった）。加えて 1 ラン当たりの前進量が小さくなりすぎ、日次ゲートの
/// もとで生ログの流入に追いつかなくなる。
pub const DECLARE_WINDOW_MIN: i64 = 50;

/// 宣言ランの窓の広さ（生ログ件数）として本人が指定できる**上限**（#394）。
///
/// 上限の理由は 1 コールのプロンプト肥大。本番実測で**窓 300 のとき最終コールが 73k
/// トークン**だった（窓の中身は本人が `read_my_history` で読み進めるので、コンテキストは
/// 窓の広さに概ね比例する）。倍の 600 でおよそ 150k 級となり、200k コンテキストの内側に
/// 収まる最後のあたりになる。ここを超えると窓を広げた結果としてターンが途中で潰れ、
/// partial（位置据え置き）になって前へ進まない。
///
/// 運用側が `memory_declare.max_logs` にこれより大きい値を設定している場合は、そちらが
/// 上限になる（本人の指定が運用の既定より狭められることは無い）。**その `max` を取れるのは
/// config を持つラン側（`memory_declare::decide_declare`）だけ**なので、この道具は上限で
/// 丸めず**希望をそのまま記録する**。ここで 600 に丸めてしまうと、`max_logs = 1000` の運用で
/// 本人が 1000 と表明した瞬間に窓が 1000 → 600 へ**狭まる**（黙っていれば 1000 のままだった）。
/// 下限（[`DECLARE_WINDOW_MIN`]）は config に依らないので、こちらは道具の側でも丸めてよい。
pub const DECLARE_WINDOW_MAX: i64 = 600;

/// 次回の宣言ランの窓（開始位置と広さ）を本人が決める。
///
/// #394: 宣言ランは「どこからどこまでが 1 つの記憶かは本人が決める」設計なのに、**窓の
/// 境界と広さだけは機械が固定で決めていた**（カーソルは宣言内容と無関係に窓の終端へ進む）。
/// この道具は本人の希望を DB（`agent_memory_index_config.memory_declare_window`）へ書く。
/// **希望であって決定ではない**: ランの側が前進の下限・上限へ丸めてから使う（本人任せに
/// すると宣言ゼロ・同じ位置の指定で同じ窓を永久に再取得するループに入る / #374）。
pub struct PlanNextMemoryWindowAction;

#[async_trait]
impl Action for PlanNextMemoryWindowAction {
    fn name(&self) -> &str {
        "plan_next_memory_window"
    }

    fn description(&self) -> &str {
        "次に自分へ提示される「今回の範囲」（窓）の始まりと広さを自分で決める。まだ続いている\
         出来事の末尾を次回へ回したいときは next_from_id にその先頭の生ログ id を渡す（そこから\
         先は次の窓にもう一度現れる）。窓の終端を越えて宣言したときも next_from_id を宣言の\
         次の id にすれば、次の窓が宣言済みと重ならない。材料が薄い/濃いと感じたら window_size で\
         次回以降の窓の広さ（生ログ件数）を変えられる（この設定は変えるまで残る。ただし既定より\
         広げたまま何度もターンが潰れると、既定の広さへ自動で戻ることがある）。どちらも希望\
         として記録され、必ず前へ進むように丸められる。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "next_from_id": {
                    "type": "integer",
                    "description": "次回の窓をこの生ログ id から始める（この id 以降は次回へ回す）。今回の範囲の中を指せば末尾を持ち越し、範囲の外（先）を指せば宣言済みの続きから始まる。指定しない（0）なら今回の範囲の終わりまで進む。"
                },
                "window_size": {
                    "type": "integer",
                    "description": format!("次回以降の窓に入れる生ログ件数（下限 {DECLARE_WINDOW_MIN} / 上限は既定 {DECLARE_WINDOW_MAX}。運用の設定がそれより広ければその値）。一度決めると変えるまで効き続ける。ただし既定より広げた設定のまま、ターンが途中で潰れる（時間切れ・反復上限・エラー）ことが続いたら、既定の広さへ自動で戻る（そのときは自分で広げ直せる）。狭めた設定はそのまま残る。")
                },
                "note": {
                    "type": "string",
                    "description": "そう決めた理由（任意）。記録に残るだけで、機械は解釈しない。"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        // `0` / 空文字は「指定なし」として扱う（モデルは使わないキーも埋めてくる / #388）。
        let next_from_id = args["next_from_id"].as_i64().filter(|&v| v > 0);
        let requested_size = args["window_size"].as_i64().filter(|&v| v > 0);
        let note = args["note"]
            .as_str()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        if next_from_id.is_none() && requested_size.is_none() && note.is_none() {
            return ActionResult::error(
                "next_from_id / window_size / note のいずれかを指定してください",
            );
        }

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };

        // 既存の希望に**上書き**する（指定しなかった項目は残す）。window_size だけ変えたい
        // ときに、前に書いた next_from_id が消えないように。
        let mut pref = match opencrab_db::queries::get_memory_declare_window(&conn, &ctx.agent_id) {
            Ok(p) => p.unwrap_or_default(),
            Err(e) => return ActionResult::error(&format!("窓の希望の読み取りに失敗: {e}")),
        };
        if let Some(v) = next_from_id {
            pref.next_from_id = Some(v);
        }
        // 広さの**下限だけ**ここで丸める。下限は config に依らないので、丸めた値をその場で
        // 返せば本人が実際の設定を確認できる。**上限は丸めない**——上限は運用の `max_logs` と
        // の `max` で決まり（[`DECLARE_WINDOW_MAX`] の doc）、config を持つのはラン側だけ
        // だから。ここで 600 に丸めると `max_logs = 1000` の運用で本人が 1000 と表明した
        // 瞬間に窓が 1000 → 600 へ狭まる（黙っていれば 1000 のままだった）。
        let recorded_size = requested_size.map(|v| v.max(DECLARE_WINDOW_MIN));
        if let Some(v) = recorded_size {
            pref.window_size = Some(v);
        }
        if note.is_some() {
            pref.note = note;
        }
        pref.updated_at = Some(chrono::Utc::now().to_rfc3339());

        if let Err(e) =
            opencrab_db::queries::set_memory_declare_window(&conn, &ctx.agent_id, Some(&pref))
        {
            return ActionResult::error(&format!("窓の希望の保存に失敗: {e}"));
        }

        ActionResult::success(json!({
            "next_from_id": pref.next_from_id,
            "window_size": pref.window_size,
            "window_size_raised_to_min": requested_size.is_some() && requested_size != recorded_size,
            "window_size_min": DECLARE_WINDOW_MIN,
            "window_size_max_default": DECLARE_WINDOW_MAX,
            "note": pref.note,
            "applies_to": "next_run",
            "hint": format!(
                "next_from_id はこのランの終わりに一度だけ使われます（必ず前へ進むよう丸められます）。\
                 window_size は変えるまで効き続けます。広さの上限は既定 {DECLARE_WINDOW_MAX} 件\
                 （運用の設定がそれより広ければその値）で、実際に使われた広さは次回の\
                 「今回の範囲」に出ます。ただし既定より広げた設定のまま、ターンが途中で潰れる\
                 （時間切れ・反復上限・エラー）ことが続いたら、既定の広さへ自動で戻ります\
                 （そのときは自分で広げ直せます）。狭めた設定はそのまま残ります。"
            ),
        }))
    }
}

