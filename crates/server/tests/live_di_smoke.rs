//! 実モデル DI スモーク harness（#902）。
//!
//! 稼働中の QC core（実 LLM）に対し、指定エージェントへ「ユーザー発話」を 1 件送って、その
//! ターンの観測点をテンプレ §1（TEMPLATE-TDD-INSTRUCTION.md §1）の語彙で標準出力に表で出す。
//! オーナー無しで発話クラス・CONTINUE・🤐(NO_REPLY) の実挙動を内部検証する。
//!
//! 2 経路を同じ表形式で出せる（`LIVE_MODE`）:
//! - `gate`（既定）: 偽の V3 ゲートを gate UDS に外部ゲートとして bind し、DI 発話 op（reply/
//!   reaction）とゲート反応（🤐/❌）まで観測する。`gate-client` の
//!   `InstanceClient::spawn_with_operations` と `discord-gateway::ops::operation_declarations()`
//!   の再利用のみ。kind=discord（nostr allow-store/anchor 非依存・generic admission）。invoke は
//!   外部へ出さず本文・種別を記録して ok を返すだけ（実 Discord/Nostr へ一切送信しない）。
//! - `rest`: `POST /api/agents/{id}/messages` を叩き、同期応答の `responses` を観測する（DI 発話
//!   op を持たない REST 経路。§5 内部スモークの plain3/noreply/1 問はこちら）。
//!
//! テンプレ §1 の観測境界に対応する出力列:
//! REST `responses` 件数/本文（rest）／ ゲート配送回数・本文（gate: op と say）／
//! memory_sessions 保存件数/本文 ／ LLM 呼び出し回数・イテレーション数 ／
//! 残留マーカー（NO_REPLY/CONTINUE）／ ゲート反応（🤐/❌）。
//!
//! QC core を汚さない: gate 経路は専用 instance/binding を毎回新規採番・agent は env で指定する
//! 専用テスト bot。のすたろう/くらぶの session には触れない。gate の admission は agent の discord
//! owner を said author（`LIVE_OWNER_ID`）に合わせ caller=Owner に解決させる。
//!
//! 実在の識別子（agent id・owner id・snowflake 等）はソースに埋めない。すべて env で与える
//! （既定値も置かない）。公開リポジトリの private-identifier 検査に通すため。
//!
//! `#[ignore]`（実 LLM 課金・稼働中 core 必須）。実行例:
//!
//! ```sh
//! OPENCRAB_GATE_OPERATOR_TOKEN="$(cat .../secrets/qc-gate-operator-token)" \
//!   LIVE_GATE_SOCK=".../runtime/gate.sock" \
//!   LIVE_AGENT_ID="<agent id>" LIVE_OWNER_ID="<owner id>" \
//!   LIVE_MODE=gate LIVE_SCENARIO=reply3 \
//!   cargo test -p opencrab-server --test live_di_smoke -- --ignored --nocapture
//! ```
//!
//! env（識別子は既定値なしで**必須**。環境固有の絶対パスもソースに埋めない）:
//! `LIVE_AGENT_ID`（対象 agent id・**必須**）、
//! `LIVE_OWNER_ID`（said author / REST user_id・**必須**）、
//! `OPENCRAB_GATE_OPERATOR_TOKEN`（gate admin Bearer・gate モードで**必須**）、
//! `LIVE_GATE_SOCK`（gate UDS の絶対パス・gate モードで**必須**）、
//! `LIVE_MODE`（gate | rest・既定 gate）、
//! `LIVE_CORE_HTTP`（既定 http://127.0.0.1:18700）、
//! `LIVE_SCENARIO`（reply3 | plain3 | noreply | oneq | sleep60・既定 reply3）、
//! `LIVE_PROMPT_FILE`（プロンプト差し替え・省略時はシナリオ既定文）、
//! `LIVE_TIMEOUT_SECS`（ターン観測の上限秒・省略時 sleep60=150 / その他=60）。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use opencrab_gate_client::client::{InstanceClient, LiveEvent, SaidOutcome};
use opencrab_gate_client::wire::Attachment;
use opencrab_gate_client::{InvokeHandler, InvokeOutcome, SayPolicy};

// ==================== env ヘルパ ====================

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// 既定値を持たない必須 env（実在の識別子はソースに埋めない）。未設定は分かりやすく panic。
fn req_env(key: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| panic!("{key} が未設定（識別子は既定値なしの env 必須）"))
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

// ==================== シナリオ ====================

struct Scenario {
    name: &'static str,
    prompt: &'static str,
    /// resume ターン（subtask）を待つ。true は最初の ended で切らず 2 回目 ended / deadline まで観測。
    expect_resume: bool,
}

fn scenario_for(name: &str) -> Scenario {
    match name {
        "reply3" => Scenario {
            name: "reply3",
            prompt: "このメッセージに、reply（返信）操作を使って3回に分けて返事してください。3つの短い返信に分けてください。",
            expect_resume: false,
        },
        "plain3" => Scenario {
            name: "plain3",
            prompt: "返信ツールを使わず、別々の投稿（メッセージ）として3回に分けて投稿して。1回目・2回目・3回目と番号を付けて。",
            expect_resume: false,
        },
        "noreply" => Scenario {
            name: "noreply",
            prompt: "（独り言・あなたへの依頼や質問ではありません。返事は不要です。）今日はいい天気ですね。",
            expect_resume: false,
        },
        "oneq" => Scenario {
            name: "oneq",
            prompt: "1たす1はいくつですか。ひとことで答えてください。",
            expect_resume: false,
        },
        "sleep60" => Scenario {
            name: "sleep60",
            prompt: "60秒 sleep して、終わったらそのことを教えてください。",
            expect_resume: true,
        },
        other => panic!("未知の LIVE_SCENARIO: {other}（reply3|plain3|noreply|oneq|sleep60）"),
    }
}

// ==================== 偽ゲートの invoke 記録 ====================

#[derive(Clone, Debug)]
struct RecordedInvoke {
    operation: String,
    payload: Value,
}

struct RecordingHandler {
    invokes: Arc<Mutex<Vec<RecordedInvoke>>>,
}

#[async_trait::async_trait]
impl InvokeHandler for RecordingHandler {
    async fn handle(
        &self,
        _call_id: &str,
        _binding_id: &str,
        operation: &str,
        payload: &Value,
    ) -> InvokeOutcome {
        self.invokes.lock().unwrap().push(RecordedInvoke {
            operation: operation.to_string(),
            payload: payload.clone(),
        });
        // 外部 API へは出さず「受理した」を返す（dry-run 相当）。null result は §10.2 で合法。
        InvokeOutcome::Ok(Value::Null)
    }

    /// #900: 発話クラス（reply/reaction/repost/say）の判定。本番ゲート（DiscordInvokeHandler）と
    /// パリティを取り、core 既知名を集約する `is_known_utterance_op` へ委ねる。これを実装しないと
    /// 既定 false で、reply だけのターンが沈黙扱い（CompletedNoReply=🤐）になり harness artifact に
    /// なる。utterance invoke が Ok で決着すると gate-client が saw_utterance を立て、🤐 を立てない。
    fn is_utterance(&self, operation: &str) -> bool {
        opencrab_gateway::is_known_utterance_op(operation)
    }
}

// ==================== HTTP（admin / REST）ヘルパ ====================

struct Http {
    client: reqwest::Client,
    base: String,
    token: String,
}

impl Http {
    async fn get_json(&self, path: &str) -> (reqwest::StatusCode, Value) {
        let resp = self
            .client
            .get(format!("{}{}", self.base, path))
            .send()
            .await
            .expect("GET 失敗");
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    async fn post_json(&self, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
        let resp = self
            .client
            .post(format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .expect("POST 失敗");
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    /// admin API は Bearer 必須。REST（agents/*）は不要だが付けても無害。
    async fn put_json(&self, path: &str, body: Value) -> (reqwest::StatusCode, String) {
        let resp = self
            .client
            .put(format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .expect("PUT 失敗");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        (status, text)
    }

    async fn patch_json(&self, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
        let resp = self
            .client
            .patch(format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .expect("PATCH 失敗");
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        (status, body)
    }
}

/// agent の discord owner を said author に合わせる。**config は enabled=false のまま owner だけ**
/// 設定する（caller=Owner の解決は `inbound.rs` load_origin_row が owner_discord_id だけを見るので
/// enabled は不要）。実 Discord gateway が起動し得る enabled=true を残さないため、既存行は PATCH
/// （enabled を触らない）、無ければ PUT（唯一の作成経路・一時的に enabled=true）で作った上で、
/// 最後に必ず `discord/stop` で enabled=false へ落とす。QC は discord subsystem 未登録なので stop
/// は DB フラグを false にするだけ（実 gateway 停止は no-op）。
async fn set_discord_owner_disabled(http: &Http, agent: &str, owner: &str) {
    // 既存 config があれば PATCH で owner だけ更新（enabled は保存値のまま）。
    let (st, body) = http
        .patch_json(
            &format!("/api/agents/{agent}/discord"),
            json!({ "owner_discord_id": owner }),
        )
        .await;
    let patched = st.is_success() && body.get("ok").and_then(|v| v.as_bool()) != Some(false);
    if !patched {
        // config 未作成。PUT は enabled=true 固定だが、唯一の作成経路。直後の stop で false へ戻す。
        let (pst, pbody) = http
            .put_json(
                &format!("/api/agents/{agent}/discord"),
                json!({"bot_token": "placeholder-not-used-by-v3", "owner_discord_id": owner}),
            )
            .await;
        assert!(pst.is_success(), "PUT discord config = {pst}: {pbody}");
    }
    // enabled=false を保証する（実 Discord gateway が起動し得る経路を残さない）。
    let (sst, sbody) = http
        .post_json(&format!("/api/agents/{agent}/discord/stop"), json!({}))
        .await;
    assert!(sst.is_success(), "POST discord/stop = {sst}: {sbody}");
}

/// agent の llm-logs の id 集合（直近 50 件）。ターン前後の差分で呼び出し回数を測る。
async fn llm_log_ids(http: &Http, agent: &str) -> HashSet<String> {
    let (st, body) = http
        .get_json(&format!("/api/agents/{agent}/llm-logs?limit=50"))
        .await;
    if !st.is_success() {
        return HashSet::new();
    }
    body.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// memory_sessions 保存行（log_type, content）。`GET /api/sessions/{id}/logs`。
async fn session_logs(http: &Http, session_id: &str) -> Vec<(String, String)> {
    let (st, body) = http
        .get_json(&format!("/api/sessions/{session_id}/logs?limit=50"))
        .await;
    if !st.is_success() {
        return Vec::new();
    }
    body.as_array()
        .map(|arr| {
            arr.iter()
                .map(|e| {
                    let lt = e
                        .get("log_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let c = e
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    (lt, c)
                })
                .collect()
        })
        .unwrap_or_default()
}

// ==================== 観測結果 ====================

#[derive(Default)]
struct Observed {
    /// gate: reply/reaction 等の invoke（op, 本文）。
    gate_ops: Vec<(String, String)>,
    /// gate: say（最終本文）。
    gate_says: Vec<String>,
    /// rest: `responses` の本文。
    rest_responses: Vec<String>,
    /// memory_sessions 保存行（log_type, content）。
    saved: Vec<(String, String)>,
    /// LLM 呼び出し回数（= 単一ターンではイテレーション数）。
    llm_calls: usize,
    /// gate: activity 遷移。
    activities: Vec<String>,
    completed_no_reply: bool,
    turn_failed: bool,
    /// 残留マーカー走査対象（say/reply/responses の全本文）。
    fn_bodies: Vec<String>,
}

impl Observed {
    fn residue(&self, needle: &str) -> bool {
        self.fn_bodies.iter().any(|s| s.contains(needle))
    }
}

// ==================== 本体 ====================

#[tokio::test]
#[ignore = "実 LLM 課金・稼働中 QC core（:18700）必須。手動で --ignored 実行する DI スモーク harness"]
async fn live_di_smoke() {
    let base = env_or("LIVE_CORE_HTTP", "http://127.0.0.1:18700");
    let mode = env_or("LIVE_MODE", "gate");
    // 実在の識別子はソースに埋めない（既定値なしの env 必須・private-identifier 検査対策）。
    let agent = req_env("LIVE_AGENT_ID");
    let owner = req_env("LIVE_OWNER_ID");
    let scenario = scenario_for(&env_or("LIVE_SCENARIO", "reply3"));
    // gate モードは admin Bearer 必須。rest モードは空でも可（REST は Bearer 不要）。
    let token = std::env::var("OPENCRAB_GATE_OPERATOR_TOKEN").unwrap_or_default();
    let timeout_secs: u64 = std::env::var("LIVE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if scenario.expect_resume { 150 } else { 60 });

    let prompt = match std::env::var("LIVE_PROMPT_FILE")
        .ok()
        .filter(|v| !v.is_empty())
    {
        Some(path) => std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("LIVE_PROMPT_FILE 読めない {path}: {e}")),
        None => scenario.prompt.to_string(),
    };

    let http = Http {
        client: reqwest::Client::new(),
        base: base.clone(),
        token,
    };

    let obs = match mode.as_str() {
        "gate" => run_gate(&http, &agent, &owner, &prompt, &scenario, timeout_secs).await,
        "rest" => run_rest(&http, &agent, &owner, &prompt).await,
        other => panic!("未知の LIVE_MODE: {other}（gate|rest）"),
    };

    print_table(&mode, &scenario, &agent, &base, &prompt, &obs);

    // 何も観測できなければ配線失敗として落とす（挙動の良否判定は人間 QC）。
    let any = !obs.gate_ops.is_empty()
        || !obs.gate_says.is_empty()
        || !obs.rest_responses.is_empty()
        || !obs.saved.is_empty()
        || !obs.activities.is_empty()
        || obs.llm_calls > 0;
    assert!(
        any,
        "観測点が全て空（配線失敗・mode={mode}・timeout={timeout_secs}s）"
    );
}

/// gate モード: 偽ゲートを bind して DI 発話 op / say / ゲート反応まで観測する。
async fn run_gate(
    http: &Http,
    agent: &str,
    owner: &str,
    prompt: &str,
    scenario: &Scenario,
    timeout_secs: u64,
) -> Observed {
    let sock = std::env::var("LIVE_GATE_SOCK")
        .ok()
        .filter(|v| !v.is_empty())
        .expect("gate モードは LIVE_GATE_SOCK（gate UDS の絶対パス）が必要（環境固有パスはソースに埋めない）");
    assert!(
        !http.token.is_empty(),
        "gate モードは OPENCRAB_GATE_OPERATOR_TOKEN（admin Bearer）が必要"
    );

    // 1) agent → subject_id。
    let (st, agent_json) = http.get_json(&format!("/api/agents/{agent}")).await;
    assert!(st.is_success(), "GET /api/agents/{agent} = {st}");
    let subject_id = agent_json["subject_id"]
        .as_i64()
        .unwrap_or_else(|| panic!("agent {agent} に subject_id が無い: {agent_json}"));

    // 2) admission 用に agent の discord owner を said author に合わせる（config は enabled=false の
    //    まま owner だけ設定・詳細は set_discord_owner_disabled の doc）。
    set_discord_owner_disabled(http, agent, owner).await;

    // 3) gate instance（kind=discord・delivery_mode=say）を新規採番して登録。
    //    guild/channel/self_bot は**合成 fixture**（実 Discord の ID ではない・ルーティング用の
    //    ダミー）。invoke は外部へ出ないので任意の合成値でよい。実在の識別子は使わない。
    let instance_id = uuid::Uuid::new_v4().to_string();
    let binding_id = uuid::Uuid::new_v4().to_string();
    let guild = "500"; // 合成 fixture
    let channel = "600"; // 合成 fixture
    let self_bot_id = "111"; // 合成 fixture
    let address = format!("discord-{agent}-{guild}-{channel}");
    let config_bytes = serde_json::to_vec(&json!({
        "agent_id": agent,
        "self_bot_id": self_bot_id,
        "name": "di-smoke-harness",
        "delivery_mode": "say",
    }))
    .unwrap();
    let config_b64 = opencrab_extgate::encode_config_b64(&config_bytes);
    let config_digest = opencrab_extgate::config_digest(&config_bytes);

    let (st, body) = http
        .put_json(
            &format!("/api/gate-instances/{instance_id}"),
            json!({
                "kind_id": "discord",
                "subject_id": subject_id,
                "enabled": true,
                "config_b64": config_b64,
            }),
        )
        .await;
    assert!(st.is_success(), "PUT gate-instance = {st}: {body}");

    // 4) binding（instance → address）。
    let (st, body) = http
        .put_json(
            &format!("/api/gate-bindings/{binding_id}"),
            json!({"instance_id": instance_id, "address": address}),
        )
        .await;
    assert!(st.is_success(), "PUT gate-binding = {st}: {body}");

    // 5) LLM 呼び出し回数の基準（ターン前の llm-logs id 集合）。
    let before_ids = llm_log_ids(http, agent).await;

    // 6) 偽ゲートを UDS 接続（discord 能力宣言つき）。invoke は記録して ok を返す。
    let invokes = Arc::new(Mutex::new(Vec::<RecordedInvoke>::new()));
    let handler = Arc::new(RecordingHandler {
        invokes: invokes.clone(),
    });
    let client = InstanceClient::spawn_with_operations(
        std::path::PathBuf::from(&sock),
        instance_id.clone(),
        1,
        self_bot_id.to_string(),
        config_digest,
        SayPolicy::AcceptToLiveQueue,
        Some(opencrab_discord_gateway::ops::operation_declarations()),
        handler,
    );

    // bind ack を待つ。
    let mut bound = false;
    for _ in 0..300 {
        if client.binding_for_address(&address).await.is_some() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        bound,
        "binding が ack されない（core 稼働・socket path・operator token を確認）"
    );

    // 7) 「ユーザー発話」を 1 件送る。origin は毎回ユニーク（dedup 回避）。author=owner で caller=Owner。
    let origin = format!("discord:message:v1:{channel}:{}", now_millis());
    let no_attachments: Vec<Attachment> = Vec::new();
    let said = client
        .post_said_with_author(&address, &origin, owner, prompt, &no_attachments)
        .await
        .expect("post_said（binding not ready?）");
    match &said {
        SaidOutcome::Accepted { .. } => {}
        other => panic!("said が admit されない: {other:?}（owner 設定 / admission を確認）"),
    }

    // 8) ターンを観測する。say は Message、reply/reaction は invoke（別記録）、ターン終了は
    //    activity ended、沈黙終端は CompletedNoReply、失敗は TurnFailed。
    let mut says: Vec<String> = Vec::new();
    let mut activities: Vec<String> = Vec::new();
    let mut completed_no_reply = false;
    let mut turn_failed = false;
    let mut ended_count = 0usize;

    let hard_deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut soft_deadline: Option<Instant> = None;

    loop {
        let now = Instant::now();
        if now >= hard_deadline {
            break;
        }
        if let Some(soft) = soft_deadline {
            if now >= soft {
                break;
            }
        }
        let per = Duration::from_secs(5).min(hard_deadline - now);
        match tokio::time::timeout(per, client.next_live(&address)).await {
            Ok(Some(LiveEvent::Message { text, .. })) => says.push(text),
            Ok(Some(LiveEvent::Activity { state, .. })) => {
                activities.push(state.clone());
                if state == "ended" {
                    ended_count += 1;
                    if !scenario.expect_resume {
                        soft_deadline = Some(Instant::now() + Duration::from_secs(2));
                    } else if ended_count >= 2 {
                        break;
                    }
                }
            }
            Ok(Some(LiveEvent::CompletedNoReply { .. })) => {
                completed_no_reply = true;
                if !scenario.expect_resume {
                    break;
                }
            }
            Ok(Some(LiveEvent::Completed { .. })) => {}
            Ok(Some(LiveEvent::TurnFailed { .. })) => {
                turn_failed = true;
                break;
            }
            Ok(Some(LiveEvent::Error { code, .. })) => {
                if code == "disconnect" {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {
                if ended_count >= 1 && !scenario.expect_resume {
                    break;
                }
            }
        }
    }

    // 9) LLM 差分・memory_sessions 保存・残留対象を集める。
    let after_ids = llm_log_ids(http, agent).await;
    let llm_calls = after_ids
        .iter()
        .filter(|id| !before_ids.contains(*id))
        .count();
    let session_id = opencrab_extgate::session_id_for_binding(&binding_id);
    let saved = session_logs(http, &session_id).await;

    let ops: Vec<(String, String)> = invokes
        .lock()
        .unwrap()
        .iter()
        .map(|inv| {
            let body = match inv.operation.as_str() {
                "reply" => inv.payload.get("text").and_then(|t| t.as_str()),
                "reaction" => inv.payload.get("emoji").and_then(|t| t.as_str()),
                _ => None,
            }
            .map(String::from)
            .unwrap_or_else(|| inv.payload.to_string());
            (inv.operation.clone(), body)
        })
        .collect();

    let mut fn_bodies: Vec<String> = says.clone();
    fn_bodies.extend(ops.iter().map(|(_, b)| b.clone()));

    Observed {
        gate_ops: ops,
        gate_says: says,
        rest_responses: Vec::new(),
        saved,
        llm_calls,
        activities,
        completed_no_reply,
        turn_failed,
        fn_bodies,
    }
}

/// rest モード: `POST /api/agents/{id}/messages` を叩き `responses` を観測する（DI op なし）。
async fn run_rest(http: &Http, agent: &str, owner: &str, prompt: &str) -> Observed {
    let before_ids = llm_log_ids(http, agent).await;

    // REST セッションは `agent-msg-{agent}-{user_id}`。run 毎に user_id へ nonce を足して
    // 新規セッションにし、memory_sessions 保存件数を「この 1 ターン分」に isolate する
    // （同一 owner を使い回すと DM 会話が累積して保存件数が読みにくくなる）。
    let user_id = format!("{owner}-{}", now_millis());

    let (st, body) = http
        .post_json(
            &format!("/api/agents/{agent}/messages"),
            json!({"content": prompt, "user_id": user_id}),
        )
        .await;
    assert!(st.is_success(), "POST messages = {st}: {body}");

    let rest_responses: Vec<String> = body
        .get("responses")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|r| {
                    // responses 要素は本文文字列 or {content:...} のどちらでも拾う。
                    r.as_str()
                        .map(String::from)
                        .or_else(|| r.get("content").and_then(|c| c.as_str()).map(String::from))
                        .unwrap_or_else(|| r.to_string())
                })
                .collect()
        })
        .unwrap_or_default();

    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("agent-msg-{agent}-{user_id}"));

    let after_ids = llm_log_ids(http, agent).await;
    let llm_calls = after_ids
        .iter()
        .filter(|id| !before_ids.contains(*id))
        .count();
    let saved = session_logs(http, &session_id).await;

    let fn_bodies = rest_responses.clone();
    Observed {
        rest_responses,
        saved,
        llm_calls,
        fn_bodies,
        ..Default::default()
    }
}

/// テンプレ §1 の観測境界に沿った結果表。
fn print_table(
    mode: &str,
    scenario: &Scenario,
    agent: &str,
    base: &str,
    prompt: &str,
    obs: &Observed,
) {
    let sep = "=".repeat(72);
    let sub = "-".repeat(72);
    println!("\n{sep}");
    println!(
        "DI スモーク結果  mode={}  scenario={}  agent={}  base={}",
        mode, scenario.name, agent, base
    );
    println!("{sep}");
    println!("prompt:\n  {}", prompt.replace('\n', "\n  "));
    println!("{sub}");
    println!("観測点（TEMPLATE-TDD-INSTRUCTION.md §1）");
    println!("{sub}");

    if mode == "rest" {
        println!("REST responses           : {} 件", obs.rest_responses.len());
        for (i, r) in obs.rest_responses.iter().enumerate() {
            println!("    [{i}] {r:?}");
        }
    } else {
        println!(
            "ゲート配送(op)           : {} 件（reply/reaction 等の invoke）",
            obs.gate_ops.len()
        );
        for (i, (op, b)) in obs.gate_ops.iter().enumerate() {
            println!("    [{i}] op={op:<9} body={b:?}");
        }
        println!("ゲート配送(say/最終本文) : {} 件", obs.gate_says.len());
        for (i, s) in obs.gate_says.iter().enumerate() {
            println!("    [{i}] {s:?}");
        }
    }
    println!("memory_sessions 保存     : {} 件", obs.saved.len());
    for (i, (lt, c)) in obs.saved.iter().enumerate() {
        println!("    [{i}] type={lt:<8} {c:?}");
    }
    println!(
        "LLM 呼び出し/イテレーション: {}（単一ターンでは = イテレーション数）",
        obs.llm_calls
    );
    println!(
        "残留マーカー             : NO_REPLY={}  CONTINUE={}",
        obs.residue("NO_REPLY"),
        obs.residue("CONTINUE")
    );
    if mode == "gate" {
        println!(
            "ゲート反応               : activity={:?}  CompletedNoReply(🤐)={}  TurnFailed(❌)={}",
            obs.activities, obs.completed_no_reply, obs.turn_failed
        );
    } else {
        println!("ゲート反応               : n/a（REST 経路はゲート反応を持たない）");
    }
    println!("{sep}\n");
}
