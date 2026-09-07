use super::*;
use crate::traits::*;
use serde_json::json;

fn test_context() -> (tempfile::TempDir, ActionContext) {
    let conn = opencrab_db::init_memory().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
    let ctx = ActionContext {
        agent_id: "agent-1".to_string(),
        agent_name: "Test Agent".to_string(),
        session_id: Some("session-1".to_string()),
        db: opencrab_db::Db::from_connection(conn),
        workspace: std::sync::Arc::new(ws),
        last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
        model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
        current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
        runtime_info: std::sync::Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
            default_model: "mock:test-model".to_string(),
            active_model: None,
            available_providers: vec!["mock".to_string()],
            gateway: "test".to_string(),
        })),
        caller: CallerIdentity::Owner,
    };
    (dir, ctx)
}

#[tokio::test]
async fn test_ws_write_and_read() {
    let (_dir, ctx) = test_context();
    let write_result = WsWriteAction
        .execute(&json!({"path": "test.txt", "content": "hello"}), &ctx)
        .await;
    assert!(write_result.success);

    let read_result = WsReadAction
        .execute(&json!({"path": "test.txt"}), &ctx)
        .await;
    assert!(read_result.success);
    let data = read_result.data.unwrap();
    assert_eq!(data["content"].as_str(), Some("hello"));
}

#[tokio::test]
async fn test_ws_read_missing() {
    let (_dir, ctx) = test_context();
    let result = WsReadAction
        .execute(&json!({"path": "nonexistent.txt"}), &ctx)
        .await;
    assert!(!result.success);
}

#[tokio::test]
async fn test_ws_list() {
    let (_dir, ctx) = test_context();
    WsWriteAction
        .execute(&json!({"path": "listed.txt", "content": "data"}), &ctx)
        .await;

    let result = WsListAction.execute(&json!({"path": ""}), &ctx).await;
    assert!(result.success);
    let data = result.data.unwrap();
    let entries = data["entries"].as_array().unwrap();
    let names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(names.contains(&"listed.txt"));
}

#[tokio::test]
async fn test_ws_edit() {
    let (_dir, ctx) = test_context();
    WsWriteAction
        .execute(&json!({"path": "edit.txt", "content": "old content"}), &ctx)
        .await;

    let edit_result = WsEditAction
        .execute(
            &json!({"path": "edit.txt", "old_string": "old", "new_string": "new"}),
            &ctx,
        )
        .await;
    assert!(edit_result.success);

    let read_result = WsReadAction
        .execute(&json!({"path": "edit.txt"}), &ctx)
        .await;
    assert!(read_result.success);
    let data = read_result.data.unwrap();
    assert_eq!(data["content"].as_str(), Some("new content"));
}

#[tokio::test]
async fn test_ws_delete() {
    let (_dir, ctx) = test_context();
    WsWriteAction
        .execute(&json!({"path": "todelete.txt", "content": "bye"}), &ctx)
        .await;

    let del_result = WsDeleteAction
        .execute(&json!({"path": "todelete.txt"}), &ctx)
        .await;
    assert!(del_result.success);

    let read_result = WsReadAction
        .execute(&json!({"path": "todelete.txt"}), &ctx)
        .await;
    assert!(!read_result.success);
}

#[tokio::test]
async fn test_ws_mkdir() {
    let (_dir, ctx) = test_context();
    let mkdir_result = WsMkdirAction
        .execute(&json!({"path": "newdir"}), &ctx)
        .await;
    assert!(mkdir_result.success);

    let list_result = WsListAction.execute(&json!({"path": ""}), &ctx).await;
    assert!(list_result.success);
    let data = list_result.data.unwrap();
    let entries = data["entries"].as_array().unwrap();
    let names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(names.contains(&"newdir"));
}

/// 範囲指定なしは従来どおり全文を返す（後方互換）。規模メタ情報（バイト）が付く。
#[tokio::test]
async fn test_ws_read_no_range_returns_full_content() {
    let (_dir, ctx) = test_context();
    WsWriteAction
        .execute(
            &json!({"path": "f.txt", "content": "line1\nline2\nline3"}),
            &ctx,
        )
        .await;

    let r = WsReadAction.execute(&json!({"path": "f.txt"}), &ctx).await;
    assert!(r.success);
    let d = r.data.unwrap();
    assert_eq!(d["content"].as_str(), Some("line1\nline2\nline3"));
    assert_eq!(d["total_bytes"].as_u64(), Some(17));
    assert_eq!(d["has_more"].as_bool(), Some(false));
    // #617: バイト系フィールド（returned_bytes / next_offset）は廃止した。
    assert!(d.get("returned_bytes").is_none());
    assert!(d.get("next_line").is_none());
}

/// start_line / line_count（行）で行範囲だけを返し、next_line で続きを辿れる。grep の行番号を
/// そのまま start_line に渡せる。
#[tokio::test]
async fn test_ws_read_line_range_returns_lines_and_paging_info() {
    let (_dir, ctx) = test_context();
    WsWriteAction
        .execute(
            &json!({"path": "f.txt", "content": "l1\nl2\nl3\nl4\nl5"}),
            &ctx,
        )
        .await;

    // 2 行目から 2 行。
    let r = WsReadAction
        .execute(
            &json!({"path": "f.txt", "start_line": 2, "line_count": 2}),
            &ctx,
        )
        .await;
    assert!(r.success);
    let d = r.data.unwrap();
    assert_eq!(d["content"].as_str(), Some("l2\nl3"));
    assert_eq!(d["start_line"].as_u64(), Some(2));
    assert_eq!(d["has_more"].as_bool(), Some(true));
    assert_eq!(d["next_line"].as_u64(), Some(4));

    // next_line から続きを読む。
    let r2 = WsReadAction
        .execute(
            &json!({"path": "f.txt", "start_line": 4, "line_count": 2}),
            &ctx,
        )
        .await;
    let d2 = r2.data.unwrap();
    assert_eq!(d2["content"].as_str(), Some("l4\nl5"));
    assert_eq!(d2["has_more"].as_bool(), Some(false), "末尾まで読んだ");
    assert!(d2.get("next_line").is_none());
}

/// start_line がファイル末尾を越えたら空・has_more=false（無限ページングにならない / テスト 1）。
#[tokio::test]
async fn test_ws_read_start_line_past_end_is_empty() {
    let (_dir, ctx) = test_context();
    WsWriteAction
        .execute(&json!({"path": "f.txt", "content": "a\nb\nc"}), &ctx)
        .await;

    let r = WsReadAction
        .execute(&json!({"path": "f.txt", "start_line": 100}), &ctx)
        .await;
    let d = r.data.unwrap();
    assert_eq!(d["content"].as_str(), Some(""));
    assert_eq!(d["has_more"].as_bool(), Some(false));
    assert!(d.get("next_line").is_none());
}

/// テスト 2: 単独で長い 1 行（改行の無い base64/ミニファイド）でも、行は 512 文字で切られ
/// **最低 1 行**返る。切られた行には ` …⟨+M文字⟩` の標識が付く。続く行があれば next_line は
/// 必ず start_line より前進する（暴走ページング防止 / #567 の趣旨を行版で保つ）。
#[tokio::test]
async fn test_ws_read_overlong_line_truncates_and_advances() {
    let (_dir, ctx) = test_context();
    // 1 行 5,000 文字（改行なし）を単独ファイルに。#707 で 1 行の上限が 2,000 文字に
    // なったので、「切られて標識が付く」ことを見るには素材もそれを超える必要がある。
    let huge = "a".repeat(5_000);
    WsWriteAction
        .execute(&json!({"path": "one.txt", "content": huge}), &ctx)
        .await;

    let r = WsReadAction
        .execute(&json!({"path": "one.txt", "start_line": 1}), &ctx)
        .await;
    assert!(r.success);
    let d = r.data.unwrap();
    let content = d["content"].as_str().unwrap();
    // 2,000 文字で切られ、標識が付く。切った文字数 M = 5,000 - 2,000 = 3,000。
    assert!(content.starts_with(&"a".repeat(2_000)));
    assert!(
        content.contains("…⟨+3000文字⟩"),
        "切った行に標識が付く: {content}"
    );
    // 単一行なので続きは無い。標識自体が「切られた」ことを伝える。
    assert_eq!(d["has_more"].as_bool(), Some(false));

    // 長い行のあとに別の行がある場合、その行は次ページへ回り next_line が前進する。
    let two = format!("{}\nsecond", "b".repeat(5_000));
    WsWriteAction
        .execute(&json!({"path": "two.txt", "content": two}), &ctx)
        .await;
    let r2 = WsReadAction
        .execute(
            &json!({"path": "two.txt", "start_line": 1, "line_count": 1}),
            &ctx,
        )
        .await;
    let d2 = r2.data.unwrap();
    assert!(d2["content"].as_str().unwrap().contains("…⟨+3000文字⟩"));
    assert_eq!(d2["has_more"].as_bool(), Some(true));
    assert_eq!(
        d2["next_line"].as_u64(),
        Some(2),
        "next_line は start_line(1) より前進する"
    );
}

/// テスト 7: 512 文字 1 行の**最悪密度**（4 バイト文字連続）でも、推定トークンは 2,048 以下に
/// 収まる（o200k はバイト BPE で 4 バイト文字は最悪 4 トークンまで割れるが 512×4=2048<2100）。
/// #707 の直接の検証: **2,000 行のファイルが 1 回で読める**。
///
/// 修正前は 1 往復 2,000 トークン（上限 2,500）しか運べず、700 行の設計文書で 9 往復して
/// も読み終わらなかった。1 往復ごとにモデルの推論（本番実測 100〜130 秒）が挟まるため、
/// サブタスクが読解だけで 1,700 秒の制限に達し commit ゼロで終わった。
///
/// 変異確認: 読みの上限を `TOOL_RESULT_TOKEN_LIMIT`（2,500）に戻すとこのテストは赤くなる。
#[tokio::test]
async fn test_ws_read_2000_lines_in_one_call() {
    let (_dir, ctx) = test_context();
    // 典型的なソース相当（1 行 40 文字 × 2,000 行 ≒ 20,000 トークン）。
    let line = "    let value = compute(argument);   // note";
    let src: String = std::iter::repeat_n(line, 2_000)
        .collect::<Vec<_>>()
        .join("\n");
    WsWriteAction
        .execute(&json!({"path": "src.rs", "content": src}), &ctx)
        .await;

    // 範囲を指定しない＝エージェントが普通に読む形。
    let r = WsReadAction.execute(&json!({"path": "src.rs"}), &ctx).await;
    assert!(r.success);
    let d = r.data.unwrap();
    assert_eq!(
        d["content"].as_str().unwrap().lines().count(),
        2_000,
        "2,000 行が 1 回で返らない（往復が増える＝#707 の状態）"
    );
    assert_eq!(
        d["has_more"].as_bool(),
        Some(false),
        "1 回で読み切れているなら続きは無い"
    );
    assert!(
        d["estimated_tokens"].as_u64().unwrap() <= READ_TOOL_RESULT_TOKEN_LIMIT as u64,
        "上限内＝退避されない（元がファイルなのに複製を作らない）: {}",
        d["estimated_tokens"]
    );
}

#[tokio::test]
async fn test_ws_read_worst_density_line_under_token_ceiling() {
    let (_dir, ctx) = test_context();
    // U+20000（4 バイト）を 2,000 文字ちょうど＝最悪密度の 1 行（#707 で 1 行上限が
    // 2,000 文字になったので、その境界を突く）。overflow は出ない。
    let dense = "𠀀".repeat(2_000);
    WsWriteAction
        .execute(&json!({"path": "dense.txt", "content": dense}), &ctx)
        .await;

    let r = WsReadAction
        .execute(&json!({"path": "dense.txt", "start_line": 1}), &ctx)
        .await;
    let d = r.data.unwrap();
    assert_eq!(d["content"].as_str().unwrap().chars().count(), 2_000);
    assert!(
        d["estimated_tokens"].as_u64().unwrap() <= 8_192,
        "最悪密度でも 2,000 文字は 8,192 トークン以下: {}",
        d["estimated_tokens"]
    );
    // ページ天井（2,100）未満＝再退避されない。
    assert!(d["estimated_tokens"].as_u64().unwrap() < READ_TOOL_RESULT_TOKEN_LIMIT as u64);
}

/// テスト 6: 旧 offset / limit（バイト）は未知キーとして無視され、全文読みへ落ちる。
#[tokio::test]
async fn test_ws_read_legacy_offset_limit_falls_to_full_read() {
    let (_dir, ctx) = test_context();
    WsWriteAction
        .execute(&json!({"path": "f.txt", "content": "l1\nl2\nl3"}), &ctx)
        .await;

    let r = WsReadAction
        .execute(&json!({"path": "f.txt", "offset": 3, "limit": 4}), &ctx)
        .await;
    let d = r.data.unwrap();
    // 未知キーは無視される。#707 で経路が 1 本になったので、3 行のファイルは
    // 1 ページ目に全部入り、続きは無い（行メタは付く＝ページとして返るため）。
    assert_eq!(d["content"].as_str(), Some("l1\nl2\nl3"));
    assert_eq!(d["has_more"].as_bool(), Some(false));
    assert!(
        d.get("next_line").is_none(),
        "続きが無ければ next_line も無い"
    );
}

/// 512 文字切り＋**標識付き**（overflow > 0）でも、返りページの推定トークンは
/// RANGE_CONTENT_TOKEN_CEILING(2,100) 未満に収まる（＝再退避しない）。overflow=0 の 512
/// ちょうどだけでなく、標識込みの最悪ケースも天井内であることを固定する。
#[tokio::test]
async fn test_ws_read_truncated_marked_line_under_ceiling() {
    let (_dir, ctx) = test_context();
    // 4 バイト文字を 2,500 文字。2,000 で切られ overflow=500 の標識が付く＝最悪密度＋標識
    // （#707 で 1 行上限が 2,000 文字になったので、素材もそれを超える必要がある）。
    let dense = "𠀀".repeat(2_500);
    WsWriteAction
        .execute(&json!({"path": "d.txt", "content": dense}), &ctx)
        .await;

    let r = WsReadAction
        .execute(&json!({"path": "d.txt", "start_line": 1}), &ctx)
        .await;
    let d = r.data.unwrap();
    let content = d["content"].as_str().unwrap();
    assert!(
        content.contains("…⟨+500文字⟩"),
        "切られた標識が付く: {content:.64}"
    );
    assert!(
        d["estimated_tokens"].as_u64().unwrap() < RANGE_CONTENT_TOKEN_CEILING as u64,
        "標識込みでもページ天井（読み上限−400）未満: {}",
        d["estimated_tokens"]
    );
}

/// テスト 3: 大きなファイルでも行範囲指定なら返す本文は inline 上限を超えず、退避
/// （自己ループ / #564）を断つ。範囲指定なしの全文は上限を超え、退避される旨のヒントが付く。
#[tokio::test]
async fn test_ws_read_line_range_stays_under_inline_limit() {
    let (_dir, ctx) = test_context();
    // 読みの上限（30,000 tok / #707）を確実に超える大きさ。1 行 80 文字 × 5,000 行 ≒ 10 万トークン。
    let line = "x".repeat(80);
    let big: String = std::iter::repeat_n(line.as_str(), 5_000)
        .collect::<Vec<_>>()
        .join("\n");
    WsWriteAction
        .execute(&json!({"path": "big.txt", "content": big}), &ctx)
        .await;

    // #707: 範囲指定なしも 1 ページ目として返す（全文を返して退避する経路は廃止）。
    let full = WsReadAction
        .execute(&json!({"path": "big.txt"}), &ctx)
        .await;
    let fd = full.data.unwrap();
    assert!(
        fd["estimated_tokens"].as_u64().unwrap() <= READ_TOOL_RESULT_TOKEN_LIMIT as u64,
        "範囲指定なしでも上限内（＝退避されない）: {}",
        fd["estimated_tokens"]
    );
    assert_eq!(
        fd["has_more"].as_bool(),
        Some(true),
        "10 万トークンのファイルは 1 ページに収まらないので続きがある"
    );

    // 行範囲指定あり: 返す本文は上限未満（＝退避されない＝自己ループが起きない）。
    let ranged = WsReadAction
        .execute(&json!({"path": "big.txt", "start_line": 1}), &ctx)
        .await;
    let rd = ranged.data.unwrap();
    assert!(
        rd["estimated_tokens"].as_u64().unwrap() <= READ_TOOL_RESULT_TOKEN_LIMIT as u64,
        "行範囲読みの本文は inline 上限を超えない"
    );
    assert_eq!(
        rd["has_more"].as_bool(),
        Some(true),
        "予算で頭打ち＝続きがある"
    );
    assert!(rd.get("hint").is_none(), "範囲指定時はヒントを出さない");
}

/// テスト 4: 密テキスト（日本語, 1 文字 ≒ 1 トークン）の複数行で、ページはトークン天井
/// （[`RANGE_CONTENT_TOKEN_CEILING`] ≒ 2,100）に達する直前で止まる。返り本文は inline 上限を
/// 超えず（再退避しない）、next_line で続きを辿れる。
#[tokio::test]
async fn test_ws_read_page_ceiling_binds_on_dense_text() {
    let (_dir, ctx) = test_context();
    // 1 行 100 文字の日本語 × 400 行（総計 ~40,000 トークン ≫ ページ天井 29,600）。
    // #707 で天井が上がったので、天井が効くことを見るには素材もそれを超える必要がある
    // （100 行 ≒ 1 万トークンは今や 1 回で読める＝それがこの修正の狙い）。
    let line = "あ".repeat(100);
    let big: String = std::iter::repeat_n(line.as_str(), 400)
        .collect::<Vec<_>>()
        .join("\n");
    WsWriteAction
        .execute(&json!({"path": "jp.txt", "content": big}), &ctx)
        .await;

    let r = WsReadAction
        .execute(&json!({"path": "jp.txt", "start_line": 1}), &ctx)
        .await;
    let d = r.data.unwrap();
    assert!(
        d["estimated_tokens"].as_u64().unwrap() <= READ_TOOL_RESULT_TOKEN_LIMIT as u64,
        "日本語でもページ本文は inline 上限を超えない（自己ループ防止）"
    );
    // 400 行すべては載らず、天井で切られて続きの導線が付く。
    assert_eq!(d["has_more"].as_bool(), Some(true));
    let next = d["next_line"].as_u64().unwrap();
    assert!(
        next > 1 && next <= 400,
        "next_line が範囲内で前進する: {next}"
    );
}

/// テスト 5: #567: 行範囲読みは同期 CPU を `spawn_blocking` に逃がすので、単一スレッド
/// runtime でも executor（他タスク）を塞がない。裏で 1ms ごとに進むタスクが read 中も前進する
/// ことを見る。range logic が execute 内でインライン実行されていれば、単一スレッド runtime では
/// このカウンタは read が終わるまで 1 も進まない（インライン化への変異検出）。
#[tokio::test]
async fn test_ws_read_does_not_block_executor() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc as StdArc;

    let (_dir, ctx) = test_context();
    // そこそこ大きい（=読み取り＋トークナイズに実 CPU 時間がかかる）ファイル（1 行 80 文字）。
    let line = "x".repeat(80);
    let big: String = std::iter::repeat_n(line.as_str(), 25_000)
        .collect::<Vec<_>>()
        .join("\n");
    WsWriteAction
        .execute(&json!({"path": "big.txt", "content": big}), &ctx)
        .await;

    let ticks = StdArc::new(AtomicU64::new(0));
    let ticks2 = ticks.clone();
    let ticker = tokio::spawn(async move {
        for _ in 0..1000 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            ticks2.fetch_add(1, Ordering::Relaxed);
        }
    });

    let r = WsReadAction
        .execute(&json!({"path": "big.txt", "start_line": 1}), &ctx)
        .await;
    assert!(r.success);
    // read の await 中に executor が ticker を進められていれば > 0。spawn_blocking に
    // 入っていない（インライン CPU）と、単一スレッド runtime では 0 のまま。
    assert!(
        ticks.load(Ordering::Relaxed) > 0,
        "read 中も別タスクが前進する（executor を塞いでいない）"
    );
    ticker.abort();
}

/// #856 発見3・作業1.1: **再帰ループ閉包の構造証明**（実 offload → 実 ws_read → 実 inline 判定）。
///
/// 「大結果を畳む → レシピで読み戻す → その読み出しがまた畳まれて読めないループ」（#856
/// 発見3・八意本番で観測）が閉じていることを、トートロジーでない実チェーンで固定する:
///
/// 1. `execute_shell` の大出力（offload 閾値 2,500 tok を大幅超過）を**実 sanitize**で退避
///    → workspace/tmp へ実ファイル書き込み＋回収レシピ付き notice に化ける。
/// 2. notice のレシピどおり退避パスを**実 `ws_read`** で読み戻す（先頭ページ）。
/// 3. その ws_read 結果の封筒（`ActionResult` を production と同じく `serde_json` 直列化）を
///    **実 `sanitize_tool_result_for_llm("ws_read", …)`** に通す → **再 offload されず verbatim**。
///
/// これが「読み戻しがループしない」＝ ws_read が自分の出力を READ inline 上限
/// （[`READ_TOOL_RESULT_TOKEN_LIMIT`] = 30,000）以下に構造的にキャップする
/// （[`RANGE_CONTENT_TOKEN_CEILING`] = 上限 −400）ことの機械証明。
///
/// 逆 revert（`RANGE_CONTENT_TOKEN_CEILING` を READ 上限以上へ／ws_read の自己キャップ撤去）で
/// この assert は FAIL する（ws_read 結果が再 offload され notice へ化ける）。
#[tokio::test]
async fn test_offload_then_ws_read_result_not_reoffloaded_loop_closed() {
    use opencrab_core::tool_result_log::sanitize_tool_result_for_llm;

    let (dir, ctx) = test_context();
    let root = dir.path();

    // (1) execute_shell の大出力（>2,500 tok）を模した結果封筒。行のある生テキストで、
    // 退避本文はヘッダ無しの verbatim（exit_code==0 かつ stderr 空）になる。
    // 各行は高密度（~150 文字 ≒ 40 tok）にして、返りページが **行数既定（2,000 行）ではなく
    // トークン天井（[`RANGE_CONTENT_TOKEN_CEILING`]）で頭打ちになる**ようにする。こうすると
    // 天井を revert（無効化）したとき返りが 2,000 行（≒ 8 万 tok）に膨らんで READ 上限を超え、
    // ws_read 結果が再 offload されて (4) の assert が FAIL する（＝非トートロジー）。
    const MARK: &str = "LOOPCLOSED-mark grep spiral payload row";
    let line = "src/foo.rs:99:    let value = compute(argument, second_argument, third); \
                // dense row so the token ceiling binds before the 2000-line default cap";
    let big_stdout: String = std::iter::once(MARK.to_string())
        .chain(std::iter::repeat_n(line.to_string(), 5_000))
        .collect::<Vec<_>>()
        .join("\n");
    let shell_env = json!({
        "success": true,
        "data": { "stdout": big_stdout, "stderr": "", "exit_code": 0 }
    })
    .to_string();

    // (2) 実 sanitize: 閾値超で workspace/tmp へ退避し notice へ差し替え。
    let notice =
        sanitize_tool_result_for_llm("execute_shell", &shell_env, "sessLC", "tcSh", Some(root));
    assert!(
        notice.contains("Tool result withheld"),
        "大出力が offload されない（前提崩れ）: {notice:.200}"
    );
    // notice の ``written in full to `tmp/…txt``` から退避パスを取り出す（回収レシピの
    // 複合トークン `grep -n <pattern> tmp/…` 等ではなく、単独の rel トークンを拾う）。
    let rel = notice
        .split('`')
        .find(|t| t.starts_with("tmp/") && t.ends_with(".txt"))
        .expect("notice に退避パス（tmp/…txt）が無い")
        .to_string();

    // (3) 実 ws_read でレシピどおり読み戻す（先頭ページ）。
    let read = WsReadAction
        .execute(&json!({ "path": rel, "start_line": 1 }), &ctx)
        .await;
    assert!(read.success, "退避ファイルを ws_read で読めない: {read:?}");
    let content = read.data.as_ref().unwrap()["content"].as_str().unwrap();
    assert!(
        content.contains("LOOPCLOSED-mark"),
        "読み戻した本文にマーカーが無い（実際に読めていない）: {content:.120}"
    );
    // 大出力なので 1 ページに収まらず続きがある（実 offload を実 ws_read でページ読みしている）。
    assert_eq!(
        read.data.as_ref().unwrap()["has_more"].as_bool(),
        Some(true),
        "5,000 行超の退避が 1 ページに収まる（前提崩れ＝実 offload を通していない）"
    );

    // (4) 実 inline 判定: ws_read 結果封筒（production と同じ直列化）を sanitize に通す →
    //     再 offload されず verbatim。ここが閉包の要（読み戻しがループしない）。
    let read_env = serde_json::to_string(&read).unwrap();
    let after = sanitize_tool_result_for_llm("ws_read", &read_env, "sessLC", "tcRead", Some(root));
    assert_eq!(
        after, read_env,
        "ws_read 結果が再 offload された＝読み戻しがループする（#856 発見3 が閉じていない）"
    );
    assert!(
        !after.contains("Tool result withheld"),
        "ws_read 結果に offload notice が出た＝再 offload: {after:.200}"
    );
}
