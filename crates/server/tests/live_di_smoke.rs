//! 実モデル DI スモーク harness（#902）。
//!
//! 稼働中の QC core（実 LLM）に偽の V3 ゲートを外部ゲートとして bind し、指定エージェントへ
//! 「ユーザー発話」を 1 件送って、そのターンで観測した4項目を標準出力に表で出す:
//! ゲートに届いた op 呼び出し（reply / reaction / resolve …と本文）、最終本文（say）、
//! LLM 呼び出し回数（llm-logs 差分）、CONTINUE / NO_REPLY の残留有無。オーナー無しで
//! 発話クラス・CONTINUE・🤐(NO_REPLY) の実挙動を内部検証する。
//!
//! 偽ゲート実体は既存資産の再利用のみ:
//! `opencrab_gate_client::client::InstanceClient::spawn_with_operations`（UDS 接続・hello・
//! bind ack・said 送信・say/activity 受信・invoke ディスパッチ）と
//! `opencrab_discord_gateway::ops::operation_declarations()`（reply/reaction/resolve の能力宣言）。
//! kind は `discord`（nostr の allow-store / anchor に依存しない・generic admission）。invoke は
//! 外部へ出さず本文・種別を記録して ok を返すだけ（実 Discord/Nostr へは一切送信しない）。
//!
//! QC core を汚さない: 専用の gate instance / binding を毎回新規採番し、agent は `e2e-test-bot`。
//! のすたろう/くらぶの session には触れない。admission のため agent の discord owner を `LIVE_OWNER`
//! に設定し、said の author を同じ id にして caller=Owner に解決させる。
//!
//! `#[ignore]`（実 LLM 課金・稼働中 core 必須）。実行例:
//!
//! ```sh
//! OPENCRAB_GATE_OPERATOR_TOKEN="$(cat .../secrets/qc-gate-operator-token)" \
//!   LIVE_SCENARIO=reply3 \
//!   cargo test -p opencrab-server --test live_di_smoke -- --ignored --nocapture
//! ```
//!
//! env（全て任意・既定は QC 統合構成）:
//! `OPENCRAB_GATE_OPERATOR_TOKEN`（gate admin Bearer・**必須**）、
//! `LIVE_CORE_HTTP`（既定 http://127.0.0.1:18700）、
//! `LIVE_GATE_SOCK`（既定 QC runtime/gate.sock）、
//! `LIVE_AGENT`（既定 e2e-test-bot）、
//! `LIVE_SCENARIO`（reply3 | plain3 | noreply | sleep60・既定 reply3）、
//! `LIVE_PROMPT_FILE`（プロンプト差し替え・省略時はシナリオ既定文）、
//! `LIVE_OWNER`（said author = agent discord owner・既定 900000000000000001）、
//! `LIVE_TIMEOUT_SECS`（ターン観測の上限秒・省略時 sleep60=150 / その他=60）。

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

fn default_gate_sock() -> String {
    "/Volumes/2TB/openclaw/.claude-scratch/qc-transplant/runtime/gate.sock".to_string()
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
            prompt: "このメッセージに、reply（返信）操作は使わず、通常の発言を3回に分けて返してください。3つの短い発言に分けてください。",
            expect_resume: false,
        },
        "noreply" => Scenario {
            name: "noreply",
            prompt: "（独り言・あなたへの依頼や質問ではありません。返事は不要です。）今日はいい天気ですね。",
            expect_resume: false,
        },
        "sleep60" => Scenario {
            name: "sleep60",
            prompt: "60秒 sleep して、終わったらそのことを教えてください。",
            expect_resume: true,
        },
        other => panic!("未知の LIVE_SCENARIO: {other}（reply3|plain3|noreply|sleep60）"),
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
    async fn handle(&self, _binding_id: &str, operation: &str, payload: &Value) -> InvokeOutcome {
        self.invokes.lock().unwrap().push(RecordedInvoke {
            operation: operation.to_string(),
            payload: payload.clone(),
        });
        // 外部 API へは出さず「受理した」を返す（dry-run 相当）。null result は §10.2 で合法。
        InvokeOutcome::Ok(Value::Null)
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
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

// ==================== 本体 ====================

#[tokio::test]
#[ignore = "実 LLM 課金・稼働中 QC core（:18700）必須。手動で --ignored 実行する DI スモーク harness"]
async fn live_di_smoke() {
    let base = env_or("LIVE_CORE_HTTP", "http://127.0.0.1:18700");
    let sock = env_or("LIVE_GATE_SOCK", &default_gate_sock());
    let agent = env_or("LIVE_AGENT", "e2e-test-bot");
    let scenario = scenario_for(&env_or("LIVE_SCENARIO", "reply3"));
    let owner = env_or("LIVE_OWNER", "900000000000000001");
    let token = std::env::var("OPENCRAB_GATE_OPERATOR_TOKEN")
        .expect("OPENCRAB_GATE_OPERATOR_TOKEN 未設定（gate admin Bearer が必要）");
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

    // 1) agent → subject_id。
    let (st, agent_json) = http.get_json(&format!("/api/agents/{agent}")).await;
    assert!(st.is_success(), "GET /api/agents/{agent} = {st}");
    let subject_id = agent_json["subject_id"]
        .as_i64()
        .unwrap_or_else(|| panic!("agent {agent} に subject_id が無い: {agent_json}"));

    // 2) admission 用に agent の discord owner を said author に合わせる（enabled=true にされるが
    //    QC は discord subsystem 未登録なので実 gateway は起動しない＝実 Discord へは出ない）。
    let (st, body) = http
        .put_json(
            &format!("/api/agents/{agent}/discord"),
            json!({"bot_token": "placeholder-not-used-by-v3", "owner_discord_id": owner}),
        )
        .await;
    assert!(st.is_success(), "PUT discord config = {st}: {body}");

    // 3) gate instance（kind=discord・delivery_mode=say）を新規採番して登録。
    let instance_id = uuid::Uuid::new_v4().to_string();
    let binding_id = uuid::Uuid::new_v4().to_string();
    let guild = "500";
    let channel = "600";
    let address = format!("discord-{agent}-{guild}-{channel}");
    let config_bytes = serde_json::to_vec(&json!({
        "agent_id": agent,
        "self_bot_id": "111",
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
    let before_ids = llm_log_ids(&http, &agent).await;

    // 6) 偽ゲートを UDS 接続（discord 能力宣言つき）。invoke は記録して ok を返す。
    let invokes = Arc::new(Mutex::new(Vec::<RecordedInvoke>::new()));
    let handler = Arc::new(RecordingHandler {
        invokes: invokes.clone(),
    });
    let client = InstanceClient::spawn_with_operations(
        std::path::PathBuf::from(&sock),
        instance_id.clone(),
        1,
        "111".to_string(),
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
        .post_said_with_author(&address, &origin, &owner, &prompt, &no_attachments)
        .await
        .expect("post_said（binding not ready?）");
    match &said {
        SaidOutcome::Accepted { .. } => {}
        other => panic!("said が admit されない: {other:?}（owner 設定 / admission を確認）"),
    }

    // 8) ターンを観測する。say は LiveEvent::Message、reply/reaction は invoke（別記録）、
    //    ターン終了は activity ended、沈黙終端は CompletedNoReply、失敗は TurnFailed。
    let mut says: Vec<String> = Vec::new();
    let mut activities: Vec<String> = Vec::new();
    let mut completed_no_reply = false;
    let mut turn_failed = false;
    let mut ended_count = 0usize;

    let hard_deadline = Instant::now() + Duration::from_secs(timeout_secs);
    // 最初の ended を見たあと、trailing（CompletedNoReply 等）を拾う猶予。
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
                        // trailing を 2 秒だけ拾って締める。
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
                // per-event タイムアウト。ended 済みなら締め、未 ended なら継続（deadline まで）。
                if ended_count >= 1 && !scenario.expect_resume {
                    break;
                }
            }
        }
    }

    // 9) LLM 呼び出し回数（ターン後 − 基準）。
    let after_ids = llm_log_ids(&http, &agent).await;
    let llm_calls = after_ids
        .iter()
        .filter(|id| !before_ids.contains(*id))
        .count();

    // 10) CONTINUE / NO_REPLY 残留（say + reply 本文を走査）。
    let recorded = invokes.lock().unwrap().clone();
    let reply_texts: Vec<String> = recorded
        .iter()
        .filter_map(|inv| {
            inv.payload
                .get("text")
                .and_then(|t| t.as_str())
                .map(String::from)
        })
        .collect();
    let residue = |needle: &str| -> bool {
        says.iter().any(|s| s.contains(needle)) || reply_texts.iter().any(|s| s.contains(needle))
    };
    let continue_residue = residue("CONTINUE");
    let no_reply_residue = residue("NO_REPLY");

    // ==================== 結果表 ====================
    let sep = "=".repeat(72);
    println!("\n{sep}");
    println!(
        "DI スモーク結果  scenario={}  agent={}  base={}",
        scenario.name, agent, base
    );
    println!("{sep}");
    println!("prompt:\n  {}", prompt.replace('\n', "\n  "));
    println!("{}", "-".repeat(72));
    println!(
        "届いた op 呼び出し（ゲートに invoke されたもの）: {} 件",
        recorded.len()
    );
    for (i, inv) in recorded.iter().enumerate() {
        let body = match inv.operation.as_str() {
            "reply" => inv
                .payload
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            "reaction" => inv
                .payload
                .get("emoji")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            _ => inv.payload.to_string(),
        };
        println!("  [{i}] op={:<9} body={:?}", inv.operation, body);
    }
    println!("{}", "-".repeat(72));
    println!("最終本文（say / 通常発言）: {} 件", says.len());
    for (i, s) in says.iter().enumerate() {
        println!("  [{i}] {:?}", s);
    }
    println!("{}", "-".repeat(72));
    println!("LLM 呼び出し回数（llm-logs 差分）: {}", llm_calls);
    println!("activity 遷移: {:?}", activities);
    println!("CompletedNoReply（沈黙終端）: {}", completed_no_reply);
    println!("TurnFailed（ターン失敗）      : {}", turn_failed);
    println!("CONTINUE 残留 : {}", continue_residue);
    println!("NO_REPLY 残留 : {}", no_reply_residue);
    println!("{sep}\n");

    // 観測できていれば harness としては成功（挙動の良否判定は人間 QC が読む）。
    // ただし何も起きなかった（op も say も activity も 0）のは配線失敗として落とす。
    assert!(
        !recorded.is_empty() || !says.is_empty() || !activities.is_empty(),
        "ターンで op も say も activity も観測できなかった（配線失敗・timeout={timeout_secs}s）"
    );
}

/// agent の llm-logs の id 集合（直近 50 件）。ターン前後の差分で呼び出し回数を測る。
async fn llm_log_ids(http: &Http, agent: &str) -> std::collections::HashSet<String> {
    let (st, body) = http
        .get_json(&format!("/api/agents/{agent}/llm-logs?limit=50"))
        .await;
    if !st.is_success() {
        return std::collections::HashSet::new();
    }
    body.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}
