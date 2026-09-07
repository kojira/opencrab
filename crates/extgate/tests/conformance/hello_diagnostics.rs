// ==================== #894: 永続宣言 digest の照合撤去＋hello 拒否の fail-loud ====================
//
// 方針（オーナー裁定）: 永続 operation_declaration_digest の「照合」を撤去する。core は hello
// ごとに宣言を fresh parse して projection に使うので、永続照合は drift を防がず、説明文編集の
// たびに既存 instance の hello を operation_declaration_mismatch で殺すだけだった。digest の永続
// 保存も止める（列は残すが書かない）。あわせて hello 拒否の理由をサーバ／クライアント双方で
// fail-loud にする（他の拒否理由—config_digest 等—の可視化のため）。

use tracing_subscriber::layer::{Context as TracingContext, SubscriberExt};
use tracing_subscriber::Layer as TracingLayer;

#[derive(Clone, Debug, Default)]
struct CapturedLog {
    target: String,
    message: String,
    fields: std::collections::BTreeMap<String, String>,
}

static LOG_BUFFER: std::sync::OnceLock<Arc<Mutex<Vec<CapturedLog>>>> = std::sync::OnceLock::new();
static LOG_INIT: std::sync::Once = std::sync::Once::new();

/// グローバル subscriber を 1 回だけ張り、共有バッファを返す。tracing の thread-local
/// `with_default` は `tokio::spawn` の別スレッド（serve_uds / client read_loop）へ伝播しない
/// ため、fail-loud ログを拾うにはグローバル default が必須（qc_harness_e2e と同型）。
fn install_log_capture() -> Arc<Mutex<Vec<CapturedLog>>> {
    let buf = LOG_BUFFER
        .get_or_init(|| Arc::new(Mutex::new(Vec::new())))
        .clone();
    LOG_INIT.call_once(|| {
        let layer = LogCaptureLayer { buf: buf.clone() };
        let subscriber = tracing_subscriber::registry().with(layer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
    buf
}

struct LogCaptureLayer {
    buf: Arc<Mutex<Vec<CapturedLog>>>,
}

#[derive(Default)]
struct FieldVisitor {
    fields: std::collections::BTreeMap<String, String>,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        // `%`（Display）/ message は format_args で来る。`{:?}` は引用符なしの Display 文字列。
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

impl<S> TracingLayer<S> for LogCaptureLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: TracingContext<'_, S>) {
        // WARN 以上だけ拾う（fail-loud の対象。tracing の Level Ord は ERROR<WARN<INFO なので
        // `> WARN` は INFO/DEBUG/TRACE）。
        if *event.metadata().level() > tracing::Level::WARN {
            return;
        }
        let mut v = FieldVisitor::default();
        event.record(&mut v);
        let message = v.fields.remove("message").unwrap_or_default();
        // 共有バッファは並列テストで共有される。panic 由来の poison でも記録は続ける。
        let mut buf = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        buf.push(CapturedLog {
            target: event.metadata().target().to_string(),
            message,
            fields: v.fields,
        });
    }
}

/// poison 耐性で現在のログをスナップショットする（panic メッセージ用にも使う）。
fn snapshot_logs(buf: &Arc<Mutex<Vec<CapturedLog>>>) -> Vec<CapturedLog> {
    buf.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

async fn wait_for_log(
    buf: &Arc<Mutex<Vec<CapturedLog>>>,
    pred: impl Fn(&CapturedLog) -> bool,
) -> bool {
    for _ in 0..200 {
        if snapshot_logs(buf).iter().any(&pred) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// operations 付き hello を生ストリームで送り、応答フレームを返す。config_digest は明示する。
async fn hello_ops_raw(
    s: &mut UnixStream,
    id: &str,
    instance_id: &str,
    revision: u64,
    config_digest_val: &str,
    operations: Value,
) -> Value {
    write_frame(
        s,
        &json!({
            "id": id,
            "m": "hello",
            "protocol": 2,
            "instance_id": instance_id,
            "revision": revision,
            "config_digest": config_digest_val,
            "operations": operations,
        }),
    )
    .await;
    read_frame(s).await
}

/// 宣言 1 個。`field_type` で arg schema 構造、`desc` で説明文（prose）を変えられる。
fn ops_one(field_type: &str, desc: &str) -> Value {
    json!([{
        "name": "doit",
        "description": desc,
        "input_schema": {"type": "object", "properties": {"emoji": {"type": field_type}}},
        "output_schema": null,
        "callback_schema": null,
        "class": {"sub_engine": "allowed", "sharing": "conversation_bound"}
    }])
}

#[tokio::test]
async fn hello_accepts_description_change_without_mismatch() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;

    // establish（説明文 X）。
    let mut s1 = h.connect().await;
    let ok1 = hello_ops_raw(
        &mut s1,
        "h1",
        &instance_id,
        1,
        &config_digest(),
        ops_one("string", "旧い説明文"),
    )
    .await;
    assert_eq!(ok1["m"], "ok", "first hello establish 失敗: {ok1}");
    drop(s1);
    wait_not_live(&h, &instance_id).await;

    // 説明文だけ変えて再 hello → 受理される（従来は mismatch で拒否）。
    let mut s2 = h.connect().await;
    let resp = hello_ops_raw(
        &mut s2,
        "h2",
        &instance_id,
        1,
        &config_digest(),
        ops_one("string", "全然違う新しい説明文"),
    )
    .await;
    assert_eq!(resp["m"], "ok", "説明文変更で hello が拒否された: {resp}");
    drop(s2);
}

#[tokio::test]
async fn hello_accepts_schema_change_and_reflects_new_declaration() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;

    // establish（arg schema type=string）。
    let mut s1 = h.connect().await;
    let ok1 = hello_ops_raw(
        &mut s1,
        "h1",
        &instance_id,
        1,
        &config_digest(),
        ops_one("string", "説明"),
    )
    .await;
    assert_eq!(ok1["m"], "ok", "first hello establish 失敗: {ok1}");
    drop(s1);
    wait_not_live(&h, &instance_id).await;

    // 引数 schema を変えて再 hello → 受理され、新宣言が live snapshot（projection の素）へ反映。
    let mut s2 = h.connect().await;
    let resp = hello_ops_raw(
        &mut s2,
        "h2",
        &instance_id,
        1,
        &config_digest(),
        ops_one("number", "説明"),
    )
    .await;
    assert_eq!(resp["m"], "ok", "schema 変更で hello が拒否された: {resp}");

    let reflected = {
        let reg = h.state.lock_registry().unwrap();
        let e = reg.get(&instance_id).expect("再 hello 後に live でない");
        e.declarations
            .first()
            .map(|d| d.input_schema.clone())
            .expect("宣言が空")
    };
    assert_eq!(
        reflected["properties"]["emoji"]["type"],
        json!("number"),
        "新宣言が projection の素（live snapshot）へ反映されていない: {reflected}"
    );
    drop(s2);
}

#[tokio::test]
async fn hello_config_digest_mismatch_logs_server_warn() {
    let logs = install_log_capture();
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;

    // 妥当形式（64 桁 hex）だが値の違う config_digest → config_digest_mismatch で拒否。
    let wrong = "a".repeat(64);
    assert_ne!(wrong, config_digest(), "誤 digest が偶然一致");
    let mut s = h.connect().await;
    let resp = hello_ops_raw(
        &mut s,
        "h1",
        &instance_id,
        1,
        &wrong,
        ops_one("string", "x"),
    )
    .await;
    assert_eq!(resp["m"], "err", "config_digest 不一致は拒否のはず: {resp}");
    assert_eq!(resp["code"], "config_digest_mismatch");

    // fail-loud: サーバは拒否理由コード＋instance_id を WARN する。
    let found = wait_for_log(&logs, |l| {
        l.target.contains("extgate::listen")
            && l.fields.get("reason").map(String::as_str) == Some("config_digest_mismatch")
            && l.fields.get("instance_id").map(String::as_str) == Some(instance_id.as_str())
    })
    .await;
    assert!(
        found,
        "server WARN(reason=config_digest_mismatch, instance_id) が無い: logs={:?}",
        snapshot_logs(&logs)
    );
}

#[tokio::test]
async fn client_hello_failure_logs_reason_code() {
    let logs = install_log_capture();
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;

    // 実 gate-client を誤 config_digest で hello → サーバは config_digest_mismatch を err_frame
    // で返し接続を閉じる。client は EOF を disconnect に潰さず真コードを reason= で報告する。
    // config_digest は 64 桁小文字 hex（parse_digest）でないと frame 検証で bad_request になるので、
    // 形式は妥当で値だけ実 config_digest と異なる digest を使う。
    let wrong_digest = "a".repeat(64);
    assert_ne!(wrong_digest, config_digest(), "誤 digest が偶然一致");
    let res = InstanceClient::connect(
        &h.sock,
        instance_id.clone(),
        1,
        "agent-1".to_string(),
        wrong_digest,
    )
    .await;
    assert!(res.is_err(), "誤 config_digest の hello は失敗するはず");

    let found = wait_for_log(&logs, |l| {
        l.target.contains("gate_client::client")
            && l.message.contains("hello failed")
            && l.fields.get("reason").map(String::as_str) == Some("config_digest_mismatch")
            && l.fields.get("instance_id").map(String::as_str) == Some(instance_id.as_str())
    })
    .await;
    assert!(
        found,
        "client WARN 'hello failed reason=config_digest_mismatch' が無い: logs={:?}",
        snapshot_logs(&logs)
    );
}
