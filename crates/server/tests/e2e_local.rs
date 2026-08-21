//! ローカル専用 E2E テストハーネス（issue #162）。
//!
//! **CI 非対象**。稼働中の opencrab サーバを HTTP/SSE で叩き、実 LLM
//! （既定 `chatgpt:gpt-5.6-sol`、`~/.codex/auth.json` 認証）で
//! 「エージェントが判断してツールを呼ぶ/停止する」E2E 挙動を検証する。
//! in-process oneshot では検証できない外部プロセス kill・非同期再注入・
//! per-session 直列化という E2E 本質を、実プロセス越しに観測する。
//!
//! ## ゲート（二重）
//! - E2E シナリオは全て `#[ignore]`（通常の `cargo test` では走らない）。
//!   ハーネス設定の単体テスト（サーバも LLM も要らない）だけは通常実行される。
//! - さらに先頭で環境変数 `OPENCRAB_E2E=1` が無ければ即 `return`（skip）。
//!   `expect`/`panic` はしない（`real_llm_e2e.rs` の `expect` パターンは踏襲しない）。
//!
//! ## 設定（環境変数 / `.env`。冒頭で `dotenvy::dotenv()` を読む）
//! - `OPENCRAB_E2E`          — ゲート（"1" で有効）。
//! - `OPENCRAB_E2E_BASE_URL` — 既定 `http://localhost:8080`。
//! - `OPENCRAB_E2E_MODEL`    — 既定 `chatgpt:gpt-5.6-sol`。
//! - `OPENCRAB_E2E_OWNER_ID` — **必須**（既定なし）。認可判定に使う owner の
//!   user_id。ローカル固有情報なのでリポジトリには埋め込まず `.env` で与える。
//!   未設定の場合は各テストを skip する。
//! - `OPENCRAB_E2E_DB`       — 既定 `data/opencrab.db`（アサート用に read-only で読む）。
//!
//! いずれも**空文字/空白のみは「未設定」**として扱い既定値へ落とす（`.env.example` は
//! これらを空文字で出荷するため、`cp .env.example .env` のまま使っても壊れない）。
//!
//! ## 実行方法
//! ```sh
//! # 前提: ローカルでサーバ稼働（./dev.sh restart）+ ~/.codex/auth.json 認証済み
//! cp .env.example .env  # 必要なら値を調整
//! OPENCRAB_E2E=1 cargo test -p opencrab-server --test e2e_local -- --ignored --nocapture
//! ```

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

// ==================== 設定 / ゲート ====================

/// `OPENCRAB_E2E=1` が設定されていなければ false（＝各テストは即 skip）。
fn e2e_enabled() -> bool {
    std::env::var("OPENCRAB_E2E").ok().as_deref() == Some("1")
}

/// 「空文字/空白のみ = 未設定」として正規化する。
///
/// `.env.example` は全ての `OPENCRAB_E2E_*` を**空文字で**出荷する（README の
/// `cp .env.example .env` 手順どおりに使うとこれらが空値として読み込まれる）。
/// 素の `unwrap_or_else(|_| default)` は「設定されている空文字」を既定値へ落とせず、
/// base_url が空で全リクエストが失敗し、model が空だと `ensure_test_agent` の PATCH
/// が稼働中 DB のテストエージェントの model を空文字で上書きしてしまう。
/// `owner_id()` は元から空を弾いていたので、こちらへ流儀を揃える。
fn non_empty(raw: Option<String>) -> Option<String> {
    raw.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// 環境変数を「空/空白のみは未設定」として読む。
fn env_non_empty(key: &str) -> Option<String> {
    non_empty(std::env::var(key).ok())
}

const DEFAULT_BASE_URL: &str = "http://localhost:8080";
const DEFAULT_MODEL: &str = "chatgpt:gpt-5.6-sol";

/// 生の env 値 → 実効 base_url（テスト用に env から切り離した本体）。
fn base_url_from(raw: Option<String>) -> String {
    non_empty(raw).unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

fn base_url() -> String {
    base_url_from(std::env::var("OPENCRAB_E2E_BASE_URL").ok())
}

/// 生の env 値 → 実効 model（テスト用に env から切り離した本体）。
fn model_from(raw: Option<String>) -> String {
    non_empty(raw).unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

fn model() -> String {
    model_from(std::env::var("OPENCRAB_E2E_MODEL").ok())
}

/// 認可判定に使う owner の user_id。**既定値は持たない**（ローカル固有情報を
/// リポジトリに埋め込まないため）。未設定なら `None` を返し、呼び出し側は skip する。
fn owner_id() -> Option<String> {
    env_non_empty("OPENCRAB_E2E_OWNER_ID")
}

fn db_path() -> String {
    // 明示指定があれば最優先（空文字は未指定扱い）。
    if let Some(p) = env_non_empty("OPENCRAB_E2E_DB") {
        return p;
    }
    // `cargo test` はテストバイナリの cwd を crate ルート（crates/server）にするため、
    // 素朴な `data/opencrab.db`（ワークスペース root 前提）は解決できない。実在する
    // 候補を順に探す: cwd 相対 → ワークスペース root（CARGO_MANIFEST_DIR/../..）相対。
    let candidates = [
        "data/opencrab.db".to_string(),
        format!("{}/../../data/opencrab.db", env!("CARGO_MANIFEST_DIR")),
    ];
    for c in &candidates {
        if std::path::Path::new(c).exists() {
            return c.clone();
        }
    }
    candidates[candidates.len() - 1].clone()
}

/// テスト用エージェントの固定 ID / 名前（本番エージェントと衝突しない専用値）。
const TEST_AGENT_ID: &str = "e2e-test-bot";
const TEST_AGENT_NAME: &str = "e2e-test-bot";

/// 各テスト先頭で共通に呼ぶ: `.env` を読み、ゲートを確認する。
/// 無効なら `false` を返し、呼び出し側は skip する。
fn setup() -> bool {
    // 冪等（複数テストで呼んでも安全）。`.env` の実値・機密はリポジトリに入れない。
    let _ = dotenvy::dotenv();
    if !e2e_enabled() {
        eprintln!(
            "[e2e skip] OPENCRAB_E2E!=1 のため skip します（有効化するには OPENCRAB_E2E=1）。"
        );
        return false;
    }
    // owner の user_id はローカル固有情報なので既定値を持たない。未設定なら skip。
    if owner_id().is_none() {
        eprintln!(
            "[e2e skip] OPENCRAB_E2E_OWNER_ID が未設定のため skip します（.env に owner の user_id を設定してください）。"
        );
        return false;
    }
    true
}

/// `setup()` を通過した後にのみ呼ぶ（未設定なら skip 済みのため必ず値がある）。
fn owner_id_or_panic() -> String {
    owner_id().expect("setup() で OPENCRAB_E2E_OWNER_ID の存在を確認済み")
}

// ---- ハーネス設定の単体テスト（`#[ignore]` ではない = 通常の cargo test で走る） ----
//
// env そのものを書き換えると、`--ignored` 同時実行時に走行中の E2E から見える値が
// 変わってしまうため、判定本体（`non_empty` / `*_from`）を env から切り離して検査する。

#[test]
fn blank_env_values_are_treated_as_unset() {
    assert_eq!(non_empty(None), None);
    assert_eq!(
        non_empty(Some(String::new())),
        None,
        ".env.example の空文字"
    );
    assert_eq!(non_empty(Some("   ".to_string())), None, "空白のみ");
    assert_eq!(non_empty(Some("\t\n".to_string())), None);
    assert_eq!(non_empty(Some(" v ".to_string())), Some("v".to_string()));
}

/// `cp .env.example .env` が出荷する空文字でも既定値へ落ちる（空の base_url で
/// 全リクエストが失敗しない）。
#[test]
fn base_url_falls_back_to_default_when_blank() {
    assert_eq!(base_url_from(None), DEFAULT_BASE_URL);
    assert_eq!(base_url_from(Some(String::new())), DEFAULT_BASE_URL);
    assert_eq!(base_url_from(Some("  ".to_string())), DEFAULT_BASE_URL);
    assert_eq!(
        base_url_from(Some(" http://127.0.0.1:9999 ".to_string())),
        "http://127.0.0.1:9999",
        "明示値は trim して尊重する"
    );
}

/// model が空のまま `ensure_test_agent` の PATCH に載ると、稼働中 DB の
/// テストエージェントの model を空文字で上書きしてしまう。空は必ず既定値へ。
#[test]
fn model_falls_back_to_default_when_blank() {
    assert_eq!(model_from(None), DEFAULT_MODEL);
    assert_eq!(model_from(Some(String::new())), DEFAULT_MODEL);
    assert_eq!(model_from(Some(" \t".to_string())), DEFAULT_MODEL);
    assert_eq!(
        model_from(Some(" openai:gpt-4o ".to_string())),
        "openai:gpt-4o"
    );
    assert!(!model().is_empty(), "実効 model が空になってはならない");
}

/// db_path も空文字を「未指定」として候補探索へ落とす。
#[test]
fn db_path_ignores_blank_env_and_never_returns_empty() {
    assert!(!db_path().is_empty());
    assert!(!base_url().is_empty());
    assert_eq!(env_non_empty("OPENCRAB_E2E_DB_DEFINITELY_UNSET_KEY"), None);
}

// ==================== HTTP ヘルパ ====================

/// 一意な conversation_id を作る（衝突回避のため nanos サフィックス）。
fn unique_conversation(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{nanos}")
}

/// web session_id 規約: `web-{agent_id}-{conversation_id}`
/// （`opencrab_web_gateway::web_session_id` と同一）。
fn web_session_id(agent_id: &str, conversation_id: &str) -> String {
    format!("web-{agent_id}-{conversation_id}")
}

/// テスト用エージェントを冪等に用意する。
///
/// - 既に `TEST_AGENT_ID` が存在すれば再利用（本番エージェントには一切触れない）。
/// - 無ければ `POST /api/agents` で作成。
/// - いずれの場合も `PATCH /api/agents/{id}` で model と instructions を設定し直す
///   （非決定性を下げるため「ツール依頼に素直に従う」instructions を固定する）。
async fn ensure_test_agent(client: &reqwest::Client) -> anyhow::Result<String> {
    let base = base_url();

    // 存在確認（GET）。get_agent は存在時に agent オブジェクト、非存在時に null を返す。
    let existing = client
        .get(format!("{base}/api/agents/{TEST_AGENT_ID}"))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await
        .unwrap_or(serde_json::Value::Null);

    let exists = existing
        .get("agent_id")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    if !exists {
        let resp = client
            .post(format!("{base}/api/agents"))
            .json(&serde_json::json!({
                "id": TEST_AGENT_ID,
                "name": TEST_AGENT_NAME,
                "persona_name": "e2e-test-persona",
            }))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;
        if resp.get("error").is_some() {
            anyhow::bail!("create agent failed: {resp}");
        }
    }

    // model + instructions を設定（冪等）。AgentPatch は double-option なので
    // 値ありは Some(Some(..)) にデシリアライズされる。
    let instructions = "あなたは E2E テスト用のボットです。ユーザーが特定のツールの呼び出しを\
        依頼したら、余計な確認や前置きをせず、依頼されたツールを直ちにそのまま呼び出してください。\
        依頼が「今すぐ」であればすぐに実行します。";
    let patch = client
        .patch(format!("{base}/api/agents/{TEST_AGENT_ID}"))
        .json(&serde_json::json!({
            "model": model(),
            "instructions": instructions,
            "personality": "{\"trait\":\"obedient\",\"style\":\"follows tool requests literally\"}",
        }))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    if patch.get("updated").and_then(|v| v.as_bool()) != Some(true) {
        anyhow::bail!("patch agent failed: {patch}");
    }

    Ok(TEST_AGENT_ID.to_string())
}

/// `POST /api/agents/{id}/web/send` — inbound メッセージを送る。resp を返す。
async fn web_send(
    client: &reqwest::Client,
    agent_id: &str,
    conversation_id: &str,
    content: &str,
) -> anyhow::Result<serde_json::Value> {
    let base = base_url();
    let resp = client
        .post(format!("{base}/api/agents/{agent_id}/web/send"))
        .json(&serde_json::json!({
            "conversation_id": conversation_id,
            "content": content,
            "user_id": owner_id_or_panic(),
        }))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    Ok(resp)
}

// ==================== DB ヘルパ（アサート用に read-only で読む） ====================

/// アサート用に DB を開く（読み取り専用の用途だが READ_WRITE で開く）。
///
/// サーバは WAL モードで動作している。`SQLITE_OPEN_READ_ONLY` の接続は未 checkpoint の
/// WAL 内コミットを参照できず古いスナップショットを読んでしまう（-shm を扱えないため）。
/// アサートは「サーバが今書いた行」を見たいので、WAL を共有できる READ_WRITE で開く
/// （このハーネスは SELECT しか発行しないので書き込みはしない）。busy_timeout で競合を吸収する。
fn open_db() -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(
        db_path(),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.busy_timeout(Duration::from_secs(10))?;
    Ok(conn)
}

#[derive(Debug, Clone)]
struct LogRow {
    log_type: String,
    content: String,
    speaker_id: Option<String>,
    metadata: Option<String>,
}

/// memory_sessions（トランスクリプト）から当該 session の全ログを id 昇順で読む。
fn fetch_session_logs(conn: &Connection, session_id: &str) -> Vec<LogRow> {
    let mut stmt = match conn.prepare(
        "SELECT log_type, content, speaker_id, metadata_json
         FROM memory_sessions WHERE session_id = ?1 ORDER BY id ASC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([session_id], |r| {
        Ok(LogRow {
            log_type: r.get(0)?,
            content: r.get(1)?,
            speaker_id: r.get(2)?,
            metadata: r.get(3)?,
        })
    });
    match rows {
        Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// llm_logs から当該 session の prompt（生リクエスト JSON 文字列）を全て読む。
fn fetch_llm_prompts(conn: &Connection, session_id: &str) -> Vec<String> {
    let mut stmt = match conn.prepare("SELECT prompt FROM llm_logs WHERE session_id = ?1") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([session_id], |r| r.get::<_, String>(0));
    match rows {
        Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

// ==================== ポーリング ====================

/// `cond` が true になるまで（または timeout まで）ポーリングする。
/// 満たせば true、timeout すれば false。
async fn poll_until<F>(timeout: Duration, mut cond: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
}

/// 稼働中の `nostaro ... vanity ... <prefix>` プロセスを `ps` で探す。
/// 起動していれば true。`ps` 自体が失敗したら false（呼び出し側で skip 扱いにできる）。
fn nostaro_vanity_running(prefix: &str) -> bool {
    let out = match std::process::Command::new("ps").arg("ax").output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .any(|line| line.contains("vanity") && line.contains(prefix) && line.contains("nostaro"))
}

// ==================== シナリオ 1: 基本応答 ====================

/// web/send で挨拶 → 一定時間内に memory_sessions に user speech と agent speech が両方入る。
#[tokio::test]
#[ignore]
async fn e2e_basic_reply() {
    if !setup() {
        return;
    }
    let client = reqwest::Client::new();
    let agent_id = ensure_test_agent(&client)
        .await
        .expect("ensure_test_agent failed");
    let conv = unique_conversation("basic");
    let session_id = web_session_id(&agent_id, &conv);

    let resp = web_send(
        &client,
        &agent_id,
        &conv,
        "こんにちは。ひとことだけ挨拶を返してください。",
    )
    .await
    .expect("web_send failed");
    eprintln!("[e2e_basic_reply] send resp: {resp}");

    // user speech（speaker=owner）と agent speech（speaker=agent）が両方入るまで待つ。
    let ok = poll_until(Duration::from_secs(90), || {
        let Ok(conn) = open_db() else { return false };
        let logs = fetch_session_logs(&conn, &session_id);
        let has_user = logs.iter().any(|l| {
            l.log_type == "speech" && l.speaker_id.as_deref() == Some(owner_id_or_panic().as_str())
        });
        let has_agent = logs
            .iter()
            .any(|l| l.log_type == "speech" && l.speaker_id.as_deref() == Some(agent_id.as_str()));
        has_user && has_agent
    })
    .await;

    assert!(
        ok,
        "90s 以内に user speech と agent speech の両方が memory_sessions(session={session_id}) に入りませんでした"
    );
    eprintln!("[e2e_basic_reply] OK: user speech + agent speech が記録されました");
}

// ==================== シナリオ 2: 非ブロック dispatch ====================

/// execute_shell の非ブロック dispatch→再注入を検証する。
/// tool_result に status:"spawned"、その後 subtask 完了（system: subtask_completed）と最終応答。
#[tokio::test]
#[ignore]
async fn e2e_nonblocking_dispatch() {
    if !setup() {
        return;
    }
    let client = reqwest::Client::new();
    let agent_id = ensure_test_agent(&client)
        .await
        .expect("ensure_test_agent failed");
    let conv = unique_conversation("dispatch");
    let session_id = web_session_id(&agent_id, &conv);

    let resp = web_send(
        &client,
        &agent_id,
        &conv,
        "execute_shell ツールを使って、コマンド echo を引数 e2e-hello で今すぐ実行してください。\
         （command=\"echo\", args=[\"e2e-hello\"]）",
    )
    .await
    .expect("web_send failed");
    eprintln!("[e2e_nonblocking_dispatch] send resp: {resp}");

    // 1) tool_result に status:"spawned" が入る（＝ inline 実行ではなく background dispatch）。
    let spawned = poll_until(Duration::from_secs(90), || {
        let Ok(conn) = open_db() else { return false };
        fetch_session_logs(&conn, &session_id).iter().any(|l| {
            l.log_type == "tool_result"
                && l.content.contains("\"status\":\"spawned\"")
                && l.content.contains("execute_shell")
        })
    })
    .await;
    assert!(
        spawned,
        "90s 以内に execute_shell の tool_result(status:spawned) が入りませんでした \
         (session={session_id})。LLM がツールを呼ばなかった可能性があります。"
    );
    eprintln!("[e2e_nonblocking_dispatch] OK: status:spawned を確認");

    // 2) subtask 完了（system: subtask_completed, exit_reason:completed）が入る（＝非同期再注入の完了）。
    let completed = poll_until(Duration::from_secs(90), || {
        let Ok(conn) = open_db() else { return false };
        fetch_session_logs(&conn, &session_id).iter().any(|l| {
            l.log_type == "system"
                && l.content.contains("\"type\":\"subtask_completed\"")
                && l.content.contains("\"exit_reason\":\"completed\"")
        })
    })
    .await;
    assert!(
        completed,
        "90s 以内に subtask_completed(exit_reason:completed) が入りませんでした (session={session_id})"
    );
    eprintln!("[e2e_nonblocking_dispatch] OK: subtask_completed(completed) を確認");

    // 3) 完了後、**再注入ターンが実際に走った**ことを検証する。
    //
    // 「agent speech が 1 件以上ある」では不十分。エージェントは `status:"spawned"` を
    // 受け取って同ターン内で喋る（「実行を開始した」等）ので、その発話だけで条件が
    // 満たされてしまい、完了 sink を丸ごと無効化しても pass してしまう（空振り）。
    // resume は system prompt に `[subtask_completed: subtask_id=..., exit_reason=...]`
    // を必ず付ける（`crates/web-gateway/src/sink.rs`）ので、llm_logs の prompt に
    // このマーカーが現れることを resume ターンの直接証拠として使う。
    let resumed = poll_until(Duration::from_secs(90), || {
        let Ok(conn) = open_db() else { return false };
        fetch_llm_prompts(&conn, &session_id)
            .iter()
            .any(|p| p.contains("[subtask_completed: subtask_id="))
    })
    .await;
    assert!(
        resumed,
        "90s 以内に再注入ターン（llm_logs.prompt に [subtask_completed: subtask_id=] を含む \
         リクエスト）が現れませんでした (session={session_id})。完了 sink が resume を \
         起こしていない可能性があります。"
    );
    eprintln!("[e2e_nonblocking_dispatch] OK: 再注入ターンが走ったことを確認");

    // 参考: 再注入後は agent の発話が入る（マーカー検証の後なので空振りしない）。
    let replied = poll_until(Duration::from_secs(30), || {
        let Ok(conn) = open_db() else { return false };
        fetch_session_logs(&conn, &session_id)
            .iter()
            .any(|l| l.log_type == "speech" && l.speaker_id.as_deref() == Some(agent_id.as_str()))
    })
    .await;
    assert!(
        replied,
        "再注入は走ったが agent の発話が記録されていません (session={session_id})"
    );
}

// ==================== シナリオ 3: cancel が subtask を止める（#161 回帰） ====================

/// nostr_generate_key を prefix `sunny7` で dispatch → 外部プロセス起動を確認 →
/// cancel_subtask で停止 → tool_cancelled 記録 + 外部プロセス消滅を確認。
///
/// LLM 非決定性への配慮: 外部プロセス起動が確認できない場合は fail させず eprintln で
/// skip 理由を出す。ただし cancel が呼ばれた形跡（tool_cancelled）は検証する。
#[tokio::test]
#[ignore]
async fn e2e_cancel_stops_subtask() {
    if !setup() {
        return;
    }
    const PREFIX: &str = "sunny7";
    let client = reqwest::Client::new();
    let agent_id = ensure_test_agent(&client)
        .await
        .expect("ensure_test_agent failed");
    let conv = unique_conversation("cancel");
    let session_id = web_session_id(&agent_id, &conv);

    // 1) 鍵生成を dispatch させる。
    let resp = web_send(
        &client,
        &agent_id,
        &conv,
        "nostr_generate_key ツールを prefix=\"sunny7\" で今すぐ呼び出して、vanity 鍵を生成してください。",
    )
    .await
    .expect("web_send failed");
    eprintln!("[e2e_cancel_stops_subtask] send#1 resp: {resp}");

    // tool_result(status:spawned, nostr_generate_key) から subtask_id を取る。
    let mut subtask_id: Option<String> = None;
    let spawned = poll_until(Duration::from_secs(90), || {
        let Ok(conn) = open_db() else { return false };
        for l in fetch_session_logs(&conn, &session_id) {
            if l.log_type == "tool_result"
                && l.content.contains("\"status\":\"spawned\"")
                && l.content.contains("nostr_generate_key")
            {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&l.content) {
                    if let Some(id) = v.get("subtask_id").and_then(|x| x.as_str()) {
                        subtask_id = Some(id.to_string());
                        return true;
                    }
                }
            }
        }
        false
    })
    .await;
    assert!(
        spawned,
        "90s 以内に nostr_generate_key の tool_result(status:spawned) が入りませんでした \
         (session={session_id})。LLM がツールを呼ばなかった可能性があります。"
    );
    let subtask_id = subtask_id.expect("subtask_id");
    eprintln!("[e2e_cancel_stops_subtask] OK: spawned subtask_id={subtask_id}");

    // 2) 外部プロセス（nostaro vanity sunny7）の起動を確認（非決定性配慮で soft）。
    let proc_seen = poll_until(Duration::from_secs(30), || nostaro_vanity_running(PREFIX)).await;
    if proc_seen {
        eprintln!("[e2e_cancel_stops_subtask] OK: 外部プロセス nostaro vanity {PREFIX} を確認");
    } else {
        eprintln!(
            "[e2e_cancel_stops_subtask] NOTE: 外部プロセス nostaro vanity {PREFIX} を確認できず \
             （nostaro 未インストール/即時終了などの可能性）。プロセス消滅アサートは skip し、\
             cancel の形跡（tool_cancelled）のみ検証します。"
        );
    }

    // 3) cancel を依頼する（LLM が cancel_subtask を呼ぶ）。
    let resp2 = web_send(
        &client,
        &agent_id,
        &conv,
        &format!(
            "今動いている sunny7 の vanity 探索（subtask_id={subtask_id}）を、\
             cancel_subtask ツールで今すぐ止めてください。"
        ),
    )
    .await
    .expect("web_send#2 failed");
    eprintln!("[e2e_cancel_stops_subtask] send#2 resp: {resp2}");

    // 4) 親セッションに tool_cancelled（当該 subtask_id）が記録される。
    let cancelled = poll_until(Duration::from_secs(90), || {
        let Ok(conn) = open_db() else { return false };
        fetch_session_logs(&conn, &session_id).iter().any(|l| {
            l.log_type == "tool_cancelled"
                && l.metadata
                    .as_deref()
                    .map(|m| m.contains(&subtask_id))
                    .unwrap_or(false)
        })
    })
    .await;
    assert!(
        cancelled,
        "90s 以内に tool_cancelled(subtask_id={subtask_id}) が記録されませんでした \
         (session={session_id})。cancel_subtask が呼ばれなかった可能性があります。"
    );
    eprintln!("[e2e_cancel_stops_subtask] OK: tool_cancelled を確認");

    // 5) プロセスが確認できていた場合のみ、消滅までを検証する。
    if proc_seen {
        let gone = poll_until(Duration::from_secs(30), || !nostaro_vanity_running(PREFIX)).await;
        assert!(
            gone,
            "cancel 後 30s 以内に外部プロセス nostaro vanity {PREFIX} が消滅しませんでした"
        );
        eprintln!("[e2e_cancel_stops_subtask] OK: 外部プロセス消滅を確認");
    }
}

// ==================== シナリオ 4: nostr_generate_key 露出（#146 回帰） ====================

/// 鍵生成を依頼した turn の llm_logs.prompt に `nostr_generate_key` が含まれる（露出）ことを DB で確認。
#[tokio::test]
#[ignore]
async fn e2e_nostr_generate_key_exposed() {
    if !setup() {
        return;
    }
    let client = reqwest::Client::new();
    let agent_id = ensure_test_agent(&client)
        .await
        .expect("ensure_test_agent failed");
    let conv = unique_conversation("exposed");
    let session_id = web_session_id(&agent_id, &conv);

    let resp = web_send(
        &client,
        &agent_id,
        &conv,
        "新しい Nostr 鍵を生成したいです。nostr_generate_key ツールを今すぐ呼び出してください。",
    )
    .await
    .expect("web_send failed");
    eprintln!("[e2e_nostr_generate_key_exposed] send resp: {resp}");

    // llm_logs.prompt（ツール定義を含む生リクエスト）に nostr_generate_key が露出していること。
    let exposed = poll_until(Duration::from_secs(90), || {
        let Ok(conn) = open_db() else { return false };
        fetch_llm_prompts(&conn, &session_id)
            .iter()
            .any(|p| p.contains("nostr_generate_key"))
    })
    .await;
    assert!(
        exposed,
        "90s 以内に llm_logs.prompt へ nostr_generate_key が露出しませんでした (session={session_id})"
    );
    eprintln!("[e2e_nostr_generate_key_exposed] OK: プロンプトに nostr_generate_key が露出");
}
