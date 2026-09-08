/// ツール結果から、会話へ残す**参照**を組む（#709）。本文は載せない。
///
/// #707 では「読み直せるか」で分け、`ws_read` / `ws_list` だけを参照化した。**その軸が誤り**
/// だった——落とすのは**会話履歴からだけ**で、記録（`memory_sessions`）には完全な本文が残る。
/// 読み直せないもの（`execute_shell` の出力）も失われない。
///
/// 実害（本番実測 2026-08-21）: エージェントBのプロンプトが 46 万文字に達し、cursor(grok) が空応答を
/// 返して沈黙した。しきい値は実測 20〜30 万文字。tool_result の内訳は execute_shell 10 万・
/// inner_voice 6 万・read_my_history 3.2 万…で、**読みだけを参照化しても 5.7% しか減らない**。
///
/// **ターン内の挙動は変わらない**（ツール往復は会話再構成を通らない）。落とすのは次のターン
/// 以降への持ち越しだけで、往復は増えない。
///
/// **失敗した結果は本文を残す**（#707 / #709 レビュー指摘）。参照へ潰すとエラー理由が消え、
/// 成功したように読める文字列に化ける——握り潰しであり #692 / #284 の理念に反する。ツール層の
/// 失敗（`success: false`）だけでなく、**コマンドの非ゼロ終了**も対象（`execute_shell` は非ゼロ
/// でも `success: true` を返すため、それだけでは捕まらない）。
///
/// 成功した結果の本文は記録（`memory_sessions`）に残る。退避しきい値を超えた大きい結果は
/// `workspace/tmp` にも残り、平文の退避 notice（非 JSON）はここを素通しするので読み方の案内が
/// 壊れない。**しきい値未満の結果はエージェントからは読み直せない**ので、再取得を案内するのは
/// 同じものが返ると保証できるとき（読み・一覧）だけにする。
///
/// **参照が本文より短くならないなら潰さない**（#709 レビュー指摘1）。会話を軽くするための
/// 仕組みが会話を重くするのは本末転倒——`ws_write` などの `{"path":"...","written":true}`
/// のような数十文字の結果は、参照へ化けさせると逆に長くなり、識別子（path）まで消える。長さの
/// 不変条件は入口の `result_reference` が一括で掛け、参照が本文以上なら本文をそのまま残す。
///
/// **失敗は必ず本文ごと残す**（#709 レビュー指摘2）。この系の不変条件——失敗は `success: false`
/// または `data.exit_code != 0` のいずれかでしか表されない——を `signals_failure` に集約する。
/// catch-all の中立文言は成功を主張しないが、それだけでは将来この不変条件を破る新ツールの失敗が
/// 黙って要約されて消える。判定を一箇所に集めることで `failures_are_never_summarized_as_success`
/// テストがどちらの経路が落ちても落ちる。
pub(crate) fn result_reference(tool_name: &str, result_json: &str) -> String {
    let reference = build_result_reference(tool_name, result_json);
    shorter_of_reference_or_body(reference, result_json)
}

/// 長さの不変条件（#709 レビュー指摘1）を 1 箇所に集約する: 参照が本文より短くならないなら
/// 潰す意味がないので本文をそのまま残す。会話を軽くするための仕組みが会話を重くしては本末転倒。
/// `tool_result`（[`result_reference`]）と `subtask_completed`（[`fold_subtask_completed`]）が
/// **同じ判断**を共有し、同じ形の判断を 2 箇所に書かない（[[same-shaped-bugs-mean-one-missing-thing]]）。
/// 小さな結果や、参照器が本文をそのまま返した（失敗・非 JSON）ケースもここを通る。
fn shorter_of_reference_or_body(reference: String, body: &str) -> String {
    if reference.chars().count() >= body.chars().count() {
        body.to_string()
    } else {
        reference
    }
}

/// 失敗を表す形か（#709 レビュー指摘2）。この系の不変条件を一箇所に集約する:
/// **失敗は `success: false` または `data.exit_code != 0` のいずれかでしか表されない**。
/// 失敗した結果を参照へ潰すと「成功した」ように読める文字列へ化ける（握り潰し・#692 / #284）ので
/// 本文を丸ごと残す。`execute_shell` は非ゼロ終了でもツール層では `success: true` を返すため、
/// `success` 判定だけでは捕まらない——両経路をここで見る。将来この不変条件を破る新ツール
/// （`success: true` のまま data の中で失敗を表す等）が入ると失敗が catch-all で「結果 N 文字」へ
/// 潰れて黙って消えるので、判定をここへ集約し `failures_are_never_summarized_as_success` で固定する。
pub(crate) fn signals_failure(v: &serde_json::Value, d: &serde_json::Value) -> bool {
    if v.get("success").and_then(|x| x.as_bool()) != Some(true) {
        return true;
    }
    matches!(d.get("exit_code").and_then(|x| x.as_i64()), Some(code) if code != 0)
}

/// `exit_code` を持つツール結果（＝モデルが読んで次手を決めるコマンド出力・現状 `execute_shell`
/// のみ）の参照を組む。**stdout 本文を畳まず、そのまま会話へ残す**。
///
/// なぜ畳まないか（くらぶ暴走の根因）: `execute_shell` は inline 集合に無いため常に背景 subtask
/// 化され（#152/#671）、その完了本文は #713 で会話再構成時に参照へ畳まれていた。#713 の安全前提
/// 「同ターン内は本文がモデルに渡る・落とすのは次ターン以降」は inline ツールでのみ成立する。
/// `execute_shell` は同ターン往復が無いので、畳むと stdout をどのターンでも読めず、モデルは
/// 出力を取り直そうとして待機宣言を連投する（本番実機で再現）。切り離した subtask の結果は
/// 非冪等・再取得不能なので「もう一度実行して見る」も副作用ループの引き金＝誤り。だから
/// **畳まずに残す**のが唯一正しい。
///
/// なぜ「畳み判定を封筒でなく stdout 実体で」やるか（統括の精緻化・欠陥2）: 旧経路は参照（畳んだ
/// 要約）を作ってから [`shorter_of_reference_or_body`] で **JSON 封筒全体**（`{"success":…,
/// "data":{…}}`）と長さを比べていた。37 字の stdout が封筒込みで 80 字に見え、要約（stdout を
/// 含まない）の方が短い→要約が採用され stdout が消える、という測定ミスだった。ここは stdout
/// **実体**を残すので、封筒との長さ比較を通っても——封筒は同じ stdout を丸ごと含む以上——stdout が
/// 失われることはない。
///
/// なぜ切り詰めず・墓標も付けないか（欠陥3・取得ハンドルの無い墓標を作らない）: この関数へ **JSON で**
/// 来る結果は必ず offload しきい値以下（`tool_result_log::sanitize_tool_result_for_log` が
/// `inline_limit_for_tool` 超過分を会話へ届く前に workspace へ退避し、本文を**非 JSON の退避 notice**
/// ——`ws_read`/`head -c` の回収レシピ＝実体のある resolve ハンドル付き——へ差し替える。#551）。その
/// 退避 notice は [`fold_subtask_completed`] の非 JSON 分岐が**素通し**するのでここには来ない。
/// つまり「大出力＝ハンドル付きで退避」は上流が既に担っており、ここへ来るのは会話に収まる小出力
/// だけ＝丸ごと残して安全。したがってここで中途半端に切り詰めて「全文は記録に残る」とだけ書く
/// **回収不能な墓標**は作らない（作る必要が無い）。
///
/// 非ゼロ終了は [`signals_failure`] が本文ごと残すので、ここへ来るのは成功（`exit_code==0`）だけ。
/// stderr は成功時は付随情報（警告・進捗）が主で判断の主材料になりにくく、無制限に載せると嵩むので
/// 規模だけ添える（失敗時＝非ゼロ終了は上流で stderr 本文ごと残る）。
fn shell_output_reference(d: &serde_json::Value) -> String {
    let code = d.get("exit_code").and_then(|x| x.as_i64()).unwrap_or(0);
    debug_assert_eq!(
        code, 0,
        "非ゼロ終了は signals_failure が本文ごと残すはず（#709 の不変条件が破れている）"
    );
    let out = d.get("stdout").and_then(|x| x.as_str()).unwrap_or("");
    let err = d.get("stderr").and_then(|x| x.as_str()).unwrap_or("");
    let stderr_note = if err.is_empty() {
        String::new()
    } else {
        format!("・stderr {} 文字", err.chars().count())
    };
    format!("終了コード {code}・出力: {out}{stderr_note}")
}

/// 会話へ残す参照本体を組む。長さの不変条件（参照が本文以上なら本文を残す）は入口の
/// `result_reference` が掛けるので、ここでは形ごとの参照を作ることに集中する。
fn build_result_reference(tool_name: &str, result_json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(result_json) {
        Ok(v) => v,
        // 判断材料が無いので推測で捨てず、そのまま残す。
        Err(_) => return result_json.to_string(),
    };
    let null = serde_json::Value::Null;
    let d = v.get("data").unwrap_or(&null);

    // 失敗は参照へ潰さず本文を丸ごと残す（握り潰し防止）。判定は signals_failure に集約。
    if signals_failure(&v, d) {
        return result_json.to_string();
    }

    // ディレクトリ一覧: 件数と正しい再取得ツール。
    if let Some(entries) = d.get("entries").and_then(|x| x.as_array()) {
        let path = d.get("path").and_then(|x| x.as_str()).unwrap_or("?");
        return format!(
            "{path} を一覧した（{} 件・内容は会話に残していない。必要ならもう一度 {tool_name} で見る）",
            entries.len()
        );
    }

    // ファイルの読み: 元のファイル名がそのまま参照になる。
    if let Some(path) = d.get("path").and_then(|x| x.as_str()) {
        if d.get("content").is_some() {
            let start = d.get("start_line").and_then(|x| x.as_u64());
            let lines = d
                .get("content")
                .and_then(|x| x.as_str())
                .map(|c| c.lines().count());
            let range = match (start, lines) {
                (Some(st), Some(n)) if n > 0 => format!("{st}〜{} 行目", st as usize + n - 1),
                (Some(st), _) => format!("{st} 行目から"),
                (None, Some(n)) => format!("{n} 行"),
                _ => "全体".to_string(),
            };
            let tokens = d
                .get("estimated_tokens")
                .and_then(|x| x.as_u64())
                .map(|t| format!("・約 {t} トークン"))
                .unwrap_or_default();
            let more = if d.get("has_more").and_then(|x| x.as_bool()) == Some(true) {
                "・続きあり"
            } else {
                ""
            };
            return format!(
                "{path} の {range} を読んだ{tokens}{more}（本文は会話に残していない。必要ならもう一度 {tool_name} で読む）"
            );
        }
    }

    // コマンド実行（成功のみ・`exit_code` を持つのは現状 `execute_shell` だけ）: **stdout 本文を
    // 畳まず会話へ残す**。モデルがこの出力を読んで次手を決めるツールで、opencrab では常に背景
    // subtask 化される（#152/#671）ため同ターン往復が無く、畳むと stdout をどのターンでも読めず
    // 待機宣言を連投する（くらぶ暴走の根因）。非ゼロ終了は上の signals_failure が本文ごと残すので
    // ここへ来るのは成功したコマンドだけ——`cargo build` / `cargo test` / pytest / jest の失敗詳細
    // （stderr にも stdout にも出る）は非ゼロ終了として丸ごと残る。判定・文言は subtask 側の
    // `build_subtask_single_reference` と `shell_output_reference` で共有する。
    if d.get("exit_code").is_some() {
        return shell_output_reference(d);
    }

    // それ以外（記憶・検索・内なる声・mutation など）: 規模だけ残す。何を呼んだかは tool_name が示す。
    //
    // **path があれば参照に含める**（#709 レビュー指摘1）。`ws_write` / `ws_edit` / `ws_delete` /
    // `ws_mkdir` などは `{"path":"...","written":true}` を返す——「何をしたか」の対象を消さない。
    //
    // **「もう一度呼ぶ」とは言わない**（#709 レビュー指摘）。ここへ落ちるツールには非冪等なものが
    // 混じる——`generate_inner_voice` を呼び直すと**別の思考**が生成され、過去のそれは回収できない。
    // 回収できないものを回収できることにする誘導は、失敗を成功に見せるのと同じ質の嘘になる。
    // 再取得を案内するのは、同じものが返ると保証できるとき（読み・一覧）だけにする。
    let size = serde_json::to_string(d)
        .map(|t| t.chars().count())
        .unwrap_or(0);
    match d.get("path").and_then(|x| x.as_str()) {
        Some(path) => {
            format!("{path} を {tool_name} した（結果 {size} 文字・本文は会話に残していない）")
        }
        None => format!("結果 {size} 文字（本文は会話に残していない）"),
    }
}

/// `subtask_completed` の内側 `result`（ツール実行の本文）を会話へ持ち越さない形へ畳む（#713）。
///
/// opencrab は「ツールを常に切り離す」ため、実運用ではツール結果の主経路が **`tool_result` では
/// なく完了本文の入れ子 `result`** になる。#709 が `tool_result` で塞いだのと同じ塊がここに現れる
/// （本番実測でツール結果 JSON が 150 件 / 281,347 文字・subtask_completed 全体の 86%）。
///
/// **判定の核**——`signals_failure`（失敗は本文ごと残す）と長さ不変条件——は [`result_reference`] と
/// **共有**し、**文言だけ** subtask 用に分ける（[[single-source-of-truth-no-parallel-paths]]）。
/// `tool_result` と決定的に違うのは**再取得できないこと**（切り離したサブタスクの結果は取り直せない
/// ＝非冪等・罠3）。だから参照は「もう一度読む」と**約束しない**。代わりに監査の在り処——完了本文の
/// `session_id`（サブセッション）——を指し、全文が記録に残ることだけ伝える。
///
/// 返すのは会話へ載せる `result` の中身。畳めない/畳むべきでない形——生産者A（サブエージェント）の
/// 散文応答・timeout / error の平文・退避 notice・失敗・想定外 JSON——は `result_str` を**そのまま
/// 返す**（握り潰さない・fail-safe）。生産者A の散文は「エージェントが出した答え・報告書」であって
/// 塊ではなく、参照化しない（#713 決定B）。
///
/// **受容した制限（決定B のサイレントな誤分類・#716 レビュー指摘2）**: 生産者A と B を分ける唯一の
/// 信号は**値の形**（`success` 封筒の有無）だけで、完了本文に由来（切り離しツール／サブエージェント
/// 会話）を示すフィールドは無い（`settle_completed` は `engine_result.response` をそのまま `result` へ
/// 載せる）。したがってサブエージェントが最終応答として `{"success":true,"data":{…}}` 形の JSON を
/// 返すと生産者B と区別できず**畳まれる**。これは受容する: 長さ不変条件で小さい答えは残る／DB に全文が
/// 残る／`session_id` ポインタも残る／本番実測で該当例は未発見。由来フィールドを生産者側へ足すのは
/// スコープ外（別 issue）。将来この誤分類が実害化したときここから辿れるよう明記しておく。
pub(super) fn fold_subtask_completed(exit_reason: &str, result_str: &str) -> String {
    // ゲート1（外側）: completed 以外は本文を丸ごと残す。timeout / error / stopped_by_limit は
    // 「プロセス完了＝clean」ではない。**completed を成功と読み替えない**ための belt——completed でも
    // 下のゲート2でさらに中身を見る（罠1）。
    if exit_reason != "completed" {
        return result_str.to_string();
    }

    // ゲート2（内側・データの形で分岐）。
    let inner: serde_json::Value = match serde_json::from_str(result_str) {
        Ok(v) => v,
        // 非 JSON = 生産者A の散文 / timeout・error の平文 / 退避 notice（読み方レシピ入り）。
        // #709 が非 JSON を素通しするのと同じ——推測で捨てない（罠4・fail-safe）。
        Err(_) => return result_str.to_string(),
    };

    let reference = match &inner {
        // 単一ツール: `{"success":bool,"data":{…}}`（`tool_result` と同一形）。
        serde_json::Value::Object(_) => {
            // 生産者B のツール結果は `success` の封筒を持つ。その封筒でない任意の JSON
            // オブジェクト（生産者A がたまたま JSON を返した等）は「答え・成果物」であって
            // ツール結果ではない——畳まず残す（決定B・fail-safe）。※封筒が無い場合は下の
            // `signals_failure` も失敗側へ倒すが、意図を明示するためここで先に切る。
            if inner.get("success").is_none() {
                return result_str.to_string();
            }
            let null = serde_json::Value::Null;
            let d = inner.get("data").unwrap_or(&null);
            // 失敗は参照へ潰さず本文を丸ごと残す。判定は #709 と共有（success:false または
            // data.exit_code!=0）。stdout / stderr を選り分けない（罠2）。
            if signals_failure(&inner, d) {
                return result_str.to_string();
            }
            build_subtask_single_reference(d)
        }
        // 複数ツール（batch）: `[{"tool":…,"tool_call_id":…,"result":<value>}, …]`。
        serde_json::Value::Array(items) => {
            // どれか 1 要素でも失敗なら配列を丸ごと残す（罠2）。判定は要素の `result` に
            // 同じ `signals_failure` を掛ける（非オブジェクト要素は失敗側＝本文保持へ倒す）。
            if items.iter().any(batch_entry_signals_failure) {
                return result_str.to_string();
            }
            build_subtask_batch_reference(items)
        }
        // 想定外の JSON（scalar 等）: 推測で捨てず残す（fail-safe）。
        _ => return result_str.to_string(),
    };

    // 長さ不変条件（#709 と共有）: 参照が本文以上なら本文を残す。
    shorter_of_reference_or_body(reference, result_str)
}

/// 単一 dispatch ツールの成功結果を resume 会話へ残す本文にする。ここへ来るのは
/// `signals_failure` を通った成功だけなので終了コードは 0。ヘッダ `[s{n} 完了]` が在り処を示すので
/// 生 UUID は出さない。
///
/// **#880: #877（exit_code 限定）を全 dispatch ツールへ拡張**。dispatch ツールは常に背景 subtask
/// 化され（#152/#671）同ターン往復が無いので、結果本文を畳むと resume がどのターンでも読めず——
/// 「済んだ」と読めないため——モデルが再 dispatch する（症状B・連投燃料）。だから exit_code の有無に
/// 関わらず本文を会話へ残す:
/// - `exit_code` あり（現状 `execute_shell`）→ stdout を素の形で（`shell_output_reference`・#877）。
/// - `exit_code` 無し（`ws_write` / `learn_*` / `summarize_and_save` 等）→ `data` payload をそのまま。
///
/// **有界性**は上流 offload が担保する: `sanitize_tool_result_for_log`（`subtask.rs:1406/1478`）が
/// inline 上限（`inline_limit_for_tool`）超過を会話へ届く前に workspace へ退避し、回収レシピ付きの
/// **非 JSON 退避 notice** へ差し替える（`fold_subtask_completed` の非 JSON 分岐が素通し）。だから
/// ここへ JSON で来るのは会話に収まる小結果だけ＝丸ごと残して安全（長い＝先頭+切り詰め+resolve
/// レシピは上流が済ませている）。**封筒でなく payload で測る**: 返した payload は呼び出し元の
/// [`shorter_of_reference_or_body`] が封筒（`{"success":…,"data":…}`）と比べ、payload の方が短いので
/// payload が採られる。本文を残すので「本文は会話に残していない」の監査後置きは付けない（#877 が
/// shell で外したのと同じ）。
fn build_subtask_single_reference(d: &serde_json::Value) -> String {
    if d.get("exit_code").is_some() {
        return shell_output_reference(d);
    }
    // exit_code 無しの dispatch ツール結果は `data` payload をそのまま残す（#880）。
    serde_json::to_string(d).unwrap_or_default()
}

/// batch（複数ツール）の成功結果。全要素成功のときだけここへ来る（どれか失敗なら上流で配列を
/// 丸ごと保持）。**#880: 要素ごとに exit_code/payload 判定**して単一と同じ規則
/// （[`build_subtask_single_reference`]）を掛け、tool 名を添えて本文を残す（畳むと resume が読めず
/// 再 dispatch の燃料になる・症状B）。結合本文の上限超過は上流 offload（`subtask.rs:1478`）が退避
/// notice 化済みなのでここは有界。長さ不変条件（呼び出し元の `shorter_of_reference_or_body`）で
/// 配列本文以上に長くなるなら本文（配列）を残すので、どちらでも各要素の本文は会話に残る。
fn build_subtask_batch_reference(items: &[serde_json::Value]) -> String {
    let null = serde_json::Value::Null;
    let mut out = format!("{} 件のツール結果", items.len());
    for entry in items {
        let tool = entry.get("tool").and_then(|x| x.as_str()).unwrap_or("?");
        let d = entry
            .get("result")
            .and_then(|r| r.get("data"))
            .unwrap_or(&null);
        out.push('\n');
        out.push_str(&format!("[{tool}] {}", build_subtask_single_reference(d)));
    }
    out
}

/// batch 要素（`{"tool":…,"tool_call_id":…,"result":<value>}`）が失敗を表すか。
/// 要素の `result` に #709 の `signals_failure` を掛ける。`result` が非オブジェクト
/// （パースできず String で入った・欠落した等）は失敗側へ倒す＝配列全体を本文保持（fail-safe）。
fn batch_entry_signals_failure(entry: &serde_json::Value) -> bool {
    let null = serde_json::Value::Null;
    let result = entry.get("result").unwrap_or(&null);
    let d = result.get("data").unwrap_or(&null);
    signals_failure(result, d)
}
