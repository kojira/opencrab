//! plugd — プラグインの接続管理とプロトコルの読み書き（詳細§01）。
//!
//! core は plugd を知らない。plugd が core を知り、`port` の seam（`Transport`・`Notifier`・
//! `ToolHost`）を実装して core と繋ぐ。線（1 行 1 メッセージの UTF-8 JSON・プロトコル§00）の
//! 読み書きと接続の状態機械（詳細§02「プラグインの接続」）はここに閉じる。
//!
//! plugd は判断をしない — 線を `port` の値へ写し、core の判断（名寄せ・権限・発火）は core が行う。

use opencrab_port::*;
use opencrab_social_runtime::{
    EventReject, HelloReject, ReadEvent, ReadPage, ReadReject, System, PROTOCOL_V2,
    PROTOCOL_VERSION, READ_LIMIT_MAX,
};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

/// 要求の期限（プロトコル§00）。プラグインには宣言させない — core が抱える資源をプラグインに
/// 決めさせないため（詳細§02「注入しないもの」）。まだ確かめていない机上の見積もり（§10）。
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const BIND_TIMEOUT: Duration = Duration::from_secs(60);
const OPEN_TIMEOUT: Duration = Duration::from_secs(60);
const EFFECT_TIMEOUT: Duration = Duration::from_secs(300);
/// 1 メッセージの上限（プロトコル§00）。超えたら `too_large` を返して切る。
const MAX_LINE: usize = 1024 * 1024;

/// 線上のエラー（プロトコル§00）。`code` は増えるので、受け手は code で分岐しない。
#[derive(Clone, Debug)]
struct WireErr {
    code: String,
    at: Option<String>,
    detail: Option<String>,
}

impl WireErr {
    fn new(code: &str) -> WireErr {
        WireErr {
            code: code.into(),
            at: None,
            detail: None,
        }
    }
    fn at(code: &str, at: &str) -> WireErr {
        WireErr {
            code: code.into(),
            at: Some(at.into()),
            detail: None,
        }
    }
    fn at_detail(code: &str, at: &str, detail: &str) -> WireErr {
        WireErr {
            code: code.into(),
            at: Some(at.into()),
            detail: Some(detail.into()),
        }
    }
    fn to_json(&self, id: &str) -> serde_json::Value {
        let mut e = serde_json::Map::new();
        e.insert("code".into(), self.code.clone().into());
        if let Some(a) = &self.at {
            e.insert("at".into(), a.clone().into());
        }
        if let Some(d) = &self.detail {
            e.insert("detail".into(), d.clone().into());
        }
        serde_json::json!({ "id": id, "err": serde_json::Value::Object(e) })
    }
}

/// 1 つのプラグイン接続。線への書き出しと、core→plugin 要求の応答待ちを持つ。
struct Conn {
    /// Canonical kind/instance/revision/epoch after hello.
    gate: Mutex<Option<GateConnection>>,
    active: Mutex<bool>,
    /// 線へ書き出す 1 行（末尾改行なし）。writer タスクが受け取って改行を付けて流す。
    out: mpsc::UnboundedSender<String>,
    /// core→plugin の要求の応答待ち（id → 待ち手）。応答が来たら外す。
    /// 期限切れ・完了で外れた id への応答は「待ち手が居ない」ので捨てる（プロトコル§00）。
    pending: Mutex<HashMap<String, oneshot::Sender<Result<serde_json::Value, WireErr>>>>,
    next_id: AtomicU64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ToolOwner {
    instance_id: GateInstanceId,
    connection_epoch: u64,
}

enum RouteIssueError {
    NotReady,
    Stale,
    WriterUnavailable,
}

impl Conn {
    fn send_line(&self, v: &serde_json::Value) {
        let _ = self.out.send(v.to_string());
    }
    fn new_id(&self) -> String {
        self.next_id.fetch_add(1, Ordering::SeqCst).to_string()
    }
    /// core→plugin の要求を送り、応答を待つ登録をする。
    fn issue(
        &self,
        mut req: serde_json::Map<String, serde_json::Value>,
    ) -> Option<(
        String,
        oneshot::Receiver<Result<serde_json::Value, WireErr>>,
    )> {
        let id = self.new_id();
        req.insert("id".into(), id.clone().into());
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        if self
            .out
            .send(serde_json::Value::Object(req).to_string())
            .is_err()
        {
            self.pending.lock().unwrap().remove(&id);
            return None;
        }
        Some((id, rx))
    }
    fn issue_if_ready(
        &self,
        req: serde_json::Map<String, serde_json::Value>,
    ) -> Option<(
        String,
        oneshot::Receiver<Result<serde_json::Value, WireErr>>,
    )> {
        let active = self.active.lock().unwrap();
        if !*active {
            return None;
        }
        self.issue(req)
    }
    fn issue_for_route(
        &self,
        route: &GateRoute,
        req: serde_json::Map<String, serde_json::Value>,
    ) -> Result<
        (
            String,
            oneshot::Receiver<Result<serde_json::Value, WireErr>>,
        ),
        RouteIssueError,
    > {
        let active = self.active.lock().unwrap();
        if !*active {
            return Err(RouteIssueError::NotReady);
        }
        let current = self.gate.lock().unwrap();
        if !current.as_ref().is_some_and(|connection| {
            connection.connection_epoch == route.connection_epoch
                && connection.revision == route.revision
        }) {
            return Err(RouteIssueError::Stale);
        }
        self.issue(req).ok_or(RouteIssueError::WriterUnavailable)
    }
    fn send_if_ready(&self, value: &serde_json::Value) -> bool {
        let active = self.active.lock().unwrap();
        if !*active {
            return false;
        }
        self.out.send(value.to_string()).is_ok()
    }
    fn send_for_route(&self, route: &GateRoute, value: &serde_json::Value) -> bool {
        let active = self.active.lock().unwrap();
        if !*active {
            return false;
        }
        let current = self.gate.lock().unwrap();
        if !current.as_ref().is_some_and(|connection| {
            connection.connection_epoch == route.connection_epoch
                && connection.revision == route.revision
        }) {
            return false;
        }
        self.out.send(value.to_string()).is_ok()
    }
    fn drop_pending(&self, id: &str) {
        self.pending.lock().unwrap().remove(id);
    }
}

/// 応答待ちの登録を、待つ側の future が畳まれた瞬間に外す（じわ漏れ防止・レビュー §6）。
/// core が背景のツール呼びを中断（abort）すると `invoke` の future が drop される。ここで
/// 待ち行列の項目を外さないと、応答が来るか切断されるまで残り続ける。正常完了時は
/// `route_response` が既に外しているので、この drop は何もしない。
struct PendingGuard {
    conn: Arc<Conn>,
    id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.conn.drop_pending(&self.id);
    }
}

struct PlugdInner {
    sys: OnceLock<System>,
    /// 接続中のゲート名 → 接続。名乗りが通ってからここに入る。
    conns: Mutex<HashMap<GateInstanceId, Arc<Conn>>>,
    /// ツール名 → instance（route-selected operations may bypass this compatibility index）。
    tools: Mutex<HashMap<String, ToolOwner>>,
}

/// core と線の間を繋ぐハブ。`Transport`・`Notifier`・`ToolHost` を実装し、core に差し込む。
#[derive(Clone)]
pub struct Plugd(Arc<PlugdInner>);

impl Default for Plugd {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugd {
    pub fn new() -> Plugd {
        Plugd(Arc::new(PlugdInner {
            sys: OnceLock::new(),
            conns: Mutex::new(HashMap::new()),
            tools: Mutex::new(HashMap::new()),
        }))
    }

    /// core を結びつける（構築順の都合で後から一度だけ）。
    pub fn attach_system(&self, sys: System) {
        let _ = self.0.sys.set(sys);
    }

    fn sys(&self) -> &System {
        self.0.sys.get().expect("plugd: system not attached")
    }

    fn conn_for_gate(&self, gate: &GateName) -> Option<Arc<Conn>> {
        let matches: Vec<_> = self
            .0
            .conns
            .lock()
            .unwrap()
            .values()
            .filter(|conn| {
                conn.gate
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|active| active.spec.kind_id == *gate)
            })
            .cloned()
            .collect();
        (matches.len() == 1).then(|| matches[0].clone())
    }

    fn conn_for_instance(&self, instance: &GateInstanceId) -> Option<Arc<Conn>> {
        self.0.conns.lock().unwrap().get(instance).cloned()
    }

    /// 1 本の接続を回す。線を割り当てた運び方（子プロセス・ソケット・duplex）は問わない（§00）。
    /// `stream` は双方向のバイト列。読みと書きに分けて 2 つのタスクで回す。
    pub fn serve<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (read, write) = tokio::io::split(stream);
        let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
        let conn = Arc::new(Conn {
            gate: Mutex::new(None),
            active: Mutex::new(false),
            out: out_tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        // 書き出しタスク: out を受けて 1 行ずつ流す。接続が切れる（out が閉じる）と終わる。
        tokio::spawn(writer_loop(write, out_rx));
        // 読み取りタスク: 状態機械（詳細§02）を回す。
        let hub = self.clone();
        tokio::spawn(async move {
            hub.reader_loop(read, conn).await;
        });
    }

    async fn reader_loop<R>(&self, read: R, conn: Arc<Conn>)
    where
        R: AsyncRead + Unpin,
    {
        let mut lr = LineReader::new(read);
        // 状態: 接続済み（Connected）→ 使用可（Ready）→ 切断（Disconnected）。
        // Connected の間は hello 以外を受け取らない。hello は 10 秒以内（§02）。
        let mut ready = false;

        loop {
            let read_res = if ready {
                // 使用可の間は待ち続ける（応答が返らなくても切らない・§08）。
                lr.next_line().await
            } else {
                // 名乗りは 10 秒以内。答えられないなら遅いのではなく壊れている（§00）。
                match tokio::time::timeout(HELLO_TIMEOUT, lr.next_line()).await {
                    Ok(r) => r,
                    Err(_) => break, // 10 秒経過 → 切断（§02）
                }
            };
            let line = match read_res {
                Ok(Some(l)) => l,
                Ok(None) => break, // EOF = 回線が切れた → 切断（§02）
                Err(TooLarge) => {
                    // 1 MiB 超 → too_large を返して切る（§00）
                    conn.send_line(&WireErr::new("too_large").to_json(""));
                    break;
                }
                Err(Io) => break,
            };
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                // 線の枠組みが壊れている（1 行 1 JSON でない）。応答の宛先も分からない → 切断。
                Err(_) => break,
            };
            let obj = match v.as_object() {
                Some(o) => o,
                None => break, // JSON だが物体でない。枠組み違反 → 切断。
            };

            if !ready {
                // 接続済み: hello だけを受ける。それ以外は「名乗る前に喋った」→ 切断（§02）。
                if obj.get("m").and_then(|x| x.as_str()) != Some("hello") {
                    break;
                }
                match self.handle_hello(&conn, obj) {
                    Ok(gate) => {
                        eprintln!(
                            "opencrab-plugd: hello accepted: kind={} instance={} epoch={}",
                            gate.spec.kind_id, gate.instance_id, gate.connection_epoch
                        );
                        *conn.gate.lock().unwrap() = Some(gate.clone());
                        *conn.active.lock().unwrap() =
                            gate.spec.ingress_discovery == IngressDiscovery::Prebound;
                        self.0
                            .conns
                            .lock()
                            .unwrap()
                            .insert(gate.instance_id.clone(), conn.clone());
                        ready = true;
                        // （再）接続したので、このゲートに結ばれている場を core に結び直させる（プロトコル§08）。
                        // 接続イベントで駆動する（ポーリングしない）ので、繋ぎ直しを取りこぼさない。
                        // conns へ入れた後に起こすこと——rebind の bind 要求は conn を引いて送るため。
                        if gate.spec.ingress_discovery == IngressDiscovery::Prebound {
                            let sys = self.sys().clone();
                            let kind = gate.spec.kind_id.clone();
                            tokio::spawn(async move {
                                sys.rebind_gate(&kind).await;
                            });
                        }
                    }
                    // 名乗りが落ちた → err を返して切断（§01/§02）。
                    Err((id, e)) => {
                        eprintln!("opencrab-plugd: hello rejected: {}", e.code);
                        conn.send_line(&e.to_json(&id));
                        break;
                    }
                }
                continue;
            }

            // 使用可: event・応答を処理する。二度目の hello は切断（§02）。
            let id = obj
                .get("id")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            match obj.get("m").and_then(|x| x.as_str()) {
                Some("hello") => break, // 二度目の名乗り → 切断（§02）
                Some("ready") => {
                    let id = id.unwrap_or_default();
                    match self.handle_ready(&conn, obj) {
                        Ok(()) => conn.send_line(&serde_json::json!({"id":id,"ok":{}})),
                        Err(e) => conn.send_line(&e.to_json(&id)),
                    }
                }
                Some("failed") => {
                    let response_id = id.unwrap_or_default();
                    match self.handle_failed(&conn, obj) {
                        Ok(()) => {
                            // `failed` の commit と同時にこの接続の所有物を退役させる。
                            // reader_loop 末尾まで残すと、replacement hello が旧 cleanup と競合する。
                            self.disconnect(&conn);
                            conn.send_line(&serde_json::json!({"id":response_id,"ok":{}}));
                            // ack と replacement hello が旧 reader の return より先に進んでも、末尾の
                            // disconnect は Arc ownership check により replacement を消さない。
                            tokio::task::yield_now().await;
                            break;
                        }
                        Err(error) => {
                            conn.send_line(&error.to_json(&response_id));
                        }
                    }
                }
                Some("event") => {
                    let id = id.unwrap_or_default();
                    if !*conn.active.lock().unwrap() {
                        eprintln!(
                            "opencrab-plugd: inbound event rejected: id={id} code=instance_not_ready"
                        );
                        conn.send_line(&WireErr::new("instance_not_ready").to_json(&id));
                        continue;
                    }
                    match self.handle_event(&conn, obj) {
                        // 着火の元栓で捨てた出来事は seq を持たない（DESIGN-attention §1）。ack は返すが
                        // （ゲートは待たない）、seq は null——記録も採番もされていない（誰も観測できなくてよい）。
                        Ok(Some(seq)) => {
                            conn.send_line(&serde_json::json!({"id": id, "ok": {"seq": seq}}))
                        }
                        Ok(None) => conn.send_line(
                            &serde_json::json!({"id": id, "ok": {"seq": serde_json::Value::Null}}),
                        ),
                        Err(e) => {
                            let gate = conn.gate.lock().unwrap().clone();
                            eprintln!(
                                "opencrab-plugd: inbound event rejected: id={id} kind={} instance={} code={} at={} detail={}",
                                gate.as_ref()
                                    .map(|connection| connection.spec.kind_id.as_str())
                                    .unwrap_or("<unidentified>"),
                                gate.as_ref()
                                    .map(|connection| connection.instance_id.as_str())
                                    .unwrap_or("<unidentified>"),
                                e.code,
                                e.at.as_deref().unwrap_or("<none>"),
                                e.detail.as_deref().unwrap_or("<none>")
                            );
                            conn.send_line(&e.to_json(&id));
                        }
                    }
                }
                Some("read") => {
                    let id = id.unwrap_or_default();
                    match self.handle_read(&conn, obj) {
                        Ok(ok) => conn.send_line(&serde_json::json!({"id": id, "ok": ok})),
                        Err(e) => conn.send_line(&e.to_json(&id)),
                    }
                }
                Some(_other) => {
                    // 知らない m → err（§00）。読み飛ばさない・近いものに寄せない。切断はしない。
                    let id = id.unwrap_or_default();
                    conn.send_line(&WireErr::at("unknown_message", "m").to_json(&id));
                }
                None => {
                    // m が無い = core→plugin 要求への応答（ok/err）。id で対応づける。
                    self.route_response(&conn, obj);
                }
            }
        }
        // 切断の後始末（§08）: 結びを解き、名簿から消す。場・チャネル（DB）は残る。
        self.disconnect(&conn);
    }

    /// 応答を待ち手へ渡す。待ち手が居ない（期限切れ・完了・そもそも要求していない）id は捨てる（§00）。
    /// これが「終わった活動への応答」を握り潰さず、静かに落とす箇所。
    fn route_response(&self, conn: &Conn, obj: &serde_json::Map<String, serde_json::Value>) {
        let Some(id) = obj.get("id").and_then(|value| value.as_str()) else {
            conn.send_line(&WireErr::at("missing_field", "response.id").to_json(""));
            return;
        };
        let waiter = conn.pending.lock().unwrap().remove(id);
        let waiter = match waiter {
            Some(w) => w,
            None => return, // 期限後・未知の id → 捨てる（§00）
        };
        if let Some(bad) = unknown_field(obj, &["id", "ok", "err"]) {
            let _ = waiter.send(Err(WireErr::at(
                "unknown_field",
                &format!("response.{bad}"),
            )));
            return;
        }
        if let Some(ok) = obj.get("ok") {
            let _ = waiter.send(Ok(ok.clone()));
        } else if let Some(err) = obj.get("err") {
            let code = err
                .get("code")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown")
                .to_string();
            let _ = waiter.send(Err(WireErr::new(&code)));
        } else {
            let _ = waiter.send(Err(WireErr::new("response_invalid")));
        }
    }

    /// hello を検証し、GateSpec を組んで core へ登録する（プロトコル§01）。
    /// 知らない欄・知らない列挙値・欠けた必須の欄は err（§00）。近いもので埋めない。
    fn handle_hello(
        &self,
        conn: &Conn,
        obj: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<GateConnection, (String, WireErr)> {
        let id = obj
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let mkerr = |e: WireErr| (id.clone(), e);

        let protocol_value = obj
            .get("protocol")
            .and_then(|x| x.as_u64())
            .ok_or_else(|| mkerr(WireErr::at("missing_field", "hello.protocol")))?;
        let protocol = match protocol_value {
            value if value == u64::from(PROTOCOL_VERSION) => PROTOCOL_VERSION,
            value if value == u64::from(PROTOCOL_V2) => PROTOCOL_V2,
            _ => return Err(mkerr(WireErr::new("protocol_unsupported"))),
        };
        let allowed_v1 = [
            "id",
            "m",
            "protocol",
            "name",
            "address_form",
            "tools",
            "effects",
            "capabilities",
            "actions",
        ];
        let allowed_v2 = [
            "id",
            "m",
            "protocol",
            "kind_id",
            "instance_id",
            "revision",
            "origin_scope",
            "address_form",
            "ingress_discovery",
            "tools",
            "effects",
            "capabilities",
            "actions",
        ];
        let allowed = match protocol {
            PROTOCOL_VERSION => &allowed_v1[..],
            PROTOCOL_V2 => &allowed_v2[..],
            _ => return Err(mkerr(WireErr::new("protocol_unsupported"))),
        };
        if let Some(bad) = unknown_field(obj, allowed) {
            return Err(mkerr(WireErr::at("unknown_field", &format!("hello.{bad}"))));
        }

        let kind_text = obj
            .get(if protocol == PROTOCOL_VERSION {
                "name"
            } else {
                "kind_id"
            })
            .and_then(|x| x.as_str())
            .ok_or_else(|| {
                mkerr(WireErr::at(
                    "missing_field",
                    if protocol == PROTOCOL_VERSION {
                        "hello.name"
                    } else {
                        "hello.kind_id"
                    },
                ))
            })?;
        let kind = GateKindId::parse(kind_text.to_string())
            .map_err(|_| mkerr(WireErr::at("bad_kind_id", "hello.kind_id")))?;
        let (provided_instance, revision, origin_scope, ingress_discovery) =
            if protocol == PROTOCOL_VERSION {
                (
                    None,
                    1_u64,
                    OriginScope::Instance,
                    IngressDiscovery::Prebound,
                )
            } else {
                let instance = obj
                    .get("instance_id")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| mkerr(WireErr::at("missing_field", "hello.instance_id")))?;
                let instance = GateInstanceId::parse(instance.to_string())
                    .map_err(|_| mkerr(WireErr::at("bad_instance_id", "hello.instance_id")))?;
                let revision = obj
                    .get("revision")
                    .and_then(|x| x.as_u64())
                    .ok_or_else(|| mkerr(WireErr::at("missing_field", "hello.revision")))?;
                let origin_scope = obj
                    .get("origin_scope")
                    .and_then(|x| x.as_str())
                    .and_then(OriginScope::from_wire)
                    .ok_or_else(|| mkerr(WireErr::at("unknown_enum", "hello.origin_scope")))?;
                let discovery = obj
                    .get("ingress_discovery")
                    .and_then(|x| x.as_str())
                    .and_then(IngressDiscovery::from_wire)
                    .ok_or_else(|| mkerr(WireErr::at("unknown_enum", "hello.ingress_discovery")))?;
                (Some(instance), revision, origin_scope, discovery)
            };
        let address_form = obj
            .get("address_form")
            .and_then(|x| x.as_str())
            .ok_or_else(|| mkerr(WireErr::at("missing_field", "hello.address_form")))?
            .to_string();
        // address_form は RE2 の構文。全体一致で使うので、その形で構文検証する（§01）。
        if regex::Regex::new(&format!("^(?:{address_form})$")).is_err() {
            return Err(mkerr(WireErr::at("bad_address_form", "hello.address_form")));
        }

        // effects（閉じた列挙・空可・必須）。
        let effects = parse_enum_set(obj, "effects").map_err(&mkerr)?;
        // capabilities（版 1 は open のみ・空可・必須）。
        let capabilities = parse_caps(obj).map_err(&mkerr)?;
        // tools（空可・必須）。
        let tools = parse_tools(obj).map_err(&mkerr)?;
        // actions（平文アクションの宣言・省略可＝[]）。版は上げないので古いゲートは actions 欄が無い。
        let actions = parse_actions(obj).map_err(&mkerr)?;
        // 宣言 action の kind 整合（接続時に落とす・プロトコル§01「拒否は接続時にしか起きない」）:
        // 各 action の kind は、そのゲートが運べる効果（effects ∪ {Say}）に含まれていなければならない。
        // 含まれないと「メニューに出るが毎回 Denied→段3 エコー」の恒久の半端状態になる。Say は場に常在
        // （core が place_effects に無条件挿入）なので検査から除く。
        for a in &actions {
            if a.kind != EffectKind::Say && !effects.contains(&a.kind) {
                return Err(mkerr(WireErr::at_detail(
                    "action_kind_not_carried",
                    "hello.actions.kind",
                    a.kind.as_wire(),
                )));
            }
        }

        // v1 compatibility lookup is deliberately read-only and happens only after the
        // complete wire payload has been validated. A valid hello for an unconfigured gate
        // fails loudly; the connection path never seeds or repairs instances.
        let instance = match provided_instance {
            Some(instance) => instance,
            None => self
                .sys()
                .store()
                .compatibility_instance(&kind)
                .map_err(|_| mkerr(WireErr::new("store_error")))?
                .ok_or_else(|| mkerr(WireErr::new("instance_unknown")))?,
        };

        let spec = GateKindSpec {
            kind_id: kind.clone(),
            origin_scope,
            address_form,
            ingress_discovery,
            tools: tools.clone(),
            effects,
            capabilities,
            actions,
        };
        let connection = match self
            .sys()
            .start_gate_connection(instance, revision, protocol, spec)
        {
            Ok(connection) => connection,
            Err(HelloReject::ProtocolUnsupported) => {
                return Err(mkerr(WireErr::new("protocol_unsupported")))
            }
            Err(HelloReject::NameTaken) => return Err(mkerr(WireErr::new("name_taken"))),
            Err(HelloReject::ToolNameTaken) => return Err(mkerr(WireErr::new("tool_name_taken"))),
            // core 予約名・同一ゲートの action==tool 衝突（平文ツール行の設計）。
            Err(HelloReject::ReservedName) => return Err(mkerr(WireErr::new("reserved_name"))),
            Err(HelloReject::ActionToolCollision) => {
                return Err(mkerr(WireErr::new("action_tool_collision")))
            }
            Err(HelloReject::InstanceTaken) if protocol == PROTOCOL_VERSION => {
                return Err(mkerr(WireErr::new("name_taken")))
            }
            Err(HelloReject::InstanceTaken) => return Err(mkerr(WireErr::new("instance_active"))),
            Err(HelloReject::KindSpecMismatch) => {
                return Err(mkerr(WireErr::new("kind_spec_mismatch")))
            }
            Err(HelloReject::KindMismatch) => {
                return Err(mkerr(WireErr::new("instance_kind_mismatch")))
            }
            Err(HelloReject::InstanceDisabled) => {
                return Err(mkerr(WireErr::new("instance_disabled")))
            }
            Err(HelloReject::KindDeclarationMismatch) => {
                return Err(mkerr(WireErr::new("kind_declaration_mismatch")))
            }
            Err(HelloReject::InstanceUnknown) => {
                return Err(mkerr(WireErr::new("instance_unknown")))
            }
            Err(HelloReject::RevisionMismatch) => {
                return Err(mkerr(WireErr::new("revision_mismatch")))
            }
        };
        {
            let mut map = self.0.tools.lock().unwrap();
            for t in &tools {
                map.insert(
                    t.name.clone(),
                    ToolOwner {
                        instance_id: connection.instance_id.clone(),
                        connection_epoch: connection.connection_epoch,
                    },
                );
            }
        }
        if protocol == PROTOCOL_VERSION {
            self.sys()
                .ready_gate_connection(&connection)
                .map_err(|_| mkerr(WireErr::new("store_error")))?;
            conn.send_line(&serde_json::json!({"id": id, "ok": {"protocol": PROTOCOL_VERSION}}));
        } else {
            conn.send_line(&serde_json::json!({"id": id, "ok": {"protocol": PROTOCOL_V2, "connection_epoch": connection.connection_epoch}}));
        }
        Ok(connection)
    }

    fn handle_ready(
        &self,
        conn: &Conn,
        obj: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), WireErr> {
        if let Some(bad) = unknown_field(obj, &["id", "m", "connection_epoch"]) {
            return Err(WireErr::at("unknown_field", &format!("ready.{bad}")));
        }
        let epoch = obj
            .get("connection_epoch")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| WireErr::at("missing_field", "ready.connection_epoch"))?;
        let connection = conn
            .gate
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| WireErr::new("not_ready"))?;
        if connection.connection_epoch != epoch {
            return Err(WireErr::new("epoch_mismatch"));
        }
        self.sys()
            .ready_gate_connection(&connection)
            .map_err(|_| WireErr::new("store_error"))?;
        *conn.active.lock().unwrap() = true;
        Ok(())
    }

    fn handle_failed(
        &self,
        conn: &Conn,
        obj: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), WireErr> {
        if let Some(bad) = unknown_field(obj, &["id", "m", "connection_epoch", "code"]) {
            return Err(WireErr::at("unknown_field", &format!("failed.{bad}")));
        }
        obj.get("id")
            .and_then(|value| value.as_str())
            .ok_or_else(|| WireErr::at("missing_field", "failed.id"))?;
        let epoch = obj
            .get("connection_epoch")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| WireErr::at("missing_field", "failed.connection_epoch"))?;
        let code = obj
            .get("code")
            .and_then(|value| value.as_str())
            .ok_or_else(|| WireErr::at("missing_field", "failed.code"))?;
        let connection = conn
            .gate
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| WireErr::new("not_ready"))?;
        if connection.connection_epoch != epoch {
            return Err(WireErr::new("epoch_mismatch"));
        }
        let mut active = conn.active.lock().unwrap();
        self.sys()
            .fail_gate_connection(&connection, code)
            .map_err(|_| WireErr::new("store_error"))?;
        *active = false;
        Ok(())
    }

    /// event を GateEvent へ写して core へ渡す（プロトコル§03）。知らない欄・列挙値は err（§00）。
    /// 結んでいない住所への出来事は not_bound（§03）。
    fn handle_event(
        &self,
        conn: &Conn,
        obj: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Option<Seq>, WireErr> {
        const ALLOWED: &[&str] = &[
            "id",
            "m",
            "kind",
            "address",
            "author",
            "content",
            "mentions",
            "reply_to",
            "origin",
            "target",
            "symbol",
            "removed",
            "action",
            "attachments",
            "discovery",
            "metadata",
        ];
        if let Some(bad) = unknown_field(obj, ALLOWED) {
            return Err(WireErr::at("unknown_field", &format!("event.{bad}")));
        }
        let kind_s = obj
            .get("kind")
            .and_then(|x| x.as_str())
            .ok_or_else(|| WireErr::at("missing_field", "event.kind"))?;
        let kind = EventKind::from_wire(kind_s)
            .filter(|k| is_inbound_kind(*k))
            .ok_or_else(|| WireErr::at_detail("unknown_enum", "event.kind", kind_s))?;
        let address = obj
            .get("address")
            .and_then(|x| x.as_str())
            .ok_or_else(|| WireErr::at("missing_field", "event.address"))?
            .to_string();
        // author.id は必須。
        let author = obj
            .get("author")
            .and_then(|x| x.as_object())
            .ok_or_else(|| WireErr::at("missing_field", "event.author"))?;
        let author_external = author
            .get("id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| WireErr::at("missing_field", "event.author.id"))?
            .to_string();
        let author_display = author
            .get("display")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());

        // content.text（said 系）。reacted は symbol を content へ入れる。
        let mut content = Content::default();
        if let Some(c) = obj.get("content").and_then(|x| x.as_object()) {
            content.text = c
                .get("text")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
        }
        content.symbol = obj
            .get("symbol")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());

        let mentions = match obj.get("mentions") {
            Some(m) => {
                let arr = m
                    .as_array()
                    .ok_or_else(|| WireErr::at("unknown_field", "event.mentions"))?;
                let mut out = vec![];
                for e in arr {
                    let s = e
                        .as_str()
                        .ok_or_else(|| WireErr::at("unknown_field", "event.mentions"))?;
                    out.push(s.to_string());
                }
                out
            }
            None => vec![],
        };
        let reply_to = obj
            .get("reply_to")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let target = obj
            .get("target")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let origin = obj
            .get("origin")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());

        // 添付（DESIGN-images §1）。無ければ []（後方互換）。各要素は {kind,url,origin_author?}。
        // 未知の kind・欠けた url は err（近い型に寄せない・§00）——ゲートは拾えたものを正しい形で載せる。
        let attachments = match obj.get("attachments") {
            None => vec![],
            Some(v) => {
                let arr = v
                    .as_array()
                    .ok_or_else(|| WireErr::at("unknown_field", "event.attachments"))?;
                let mut out = Vec::with_capacity(arr.len());
                for a in arr {
                    let o = a
                        .as_object()
                        .ok_or_else(|| WireErr::at("unknown_field", "event.attachments[]"))?;
                    if let Some(bad) = unknown_field(o, &["kind", "url", "origin_author"]) {
                        return Err(WireErr::at("unknown_field", &format!("attachments.{bad}")));
                    }
                    let kind_s = o
                        .get("kind")
                        .and_then(|x| x.as_str())
                        .ok_or_else(|| WireErr::at("missing_field", "event.attachments[].kind"))?;
                    let kind = AttachmentKind::from_wire(kind_s).ok_or_else(|| {
                        WireErr::at_detail("unknown_enum", "event.attachments[].kind", kind_s)
                    })?;
                    let url = o
                        .get("url")
                        .and_then(|x| x.as_str())
                        .ok_or_else(|| WireErr::at("missing_field", "event.attachments[].url"))?
                        .to_string();
                    let origin_author = o
                        .get("origin_author")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string());
                    out.push(Attachment {
                        kind,
                        url,
                        origin_author,
                    });
                }
                out
            }
        };

        let gate = conn
            .gate
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| WireErr::new("not_ready"))?;
        let discovery = match gate.spec.ingress_discovery {
            IngressDiscovery::Prebound => {
                if obj.contains_key("discovery") {
                    return Err(WireErr::at("unknown_field", "event.discovery"));
                }
                None
            }
            IngressDiscovery::Membership => Some(parse_membership_discovery(obj)?),
        };
        let metadata = match obj.get("metadata") {
            None => serde_json::json!({}),
            Some(v) => v
                .as_object()
                .map(|_| v.clone())
                .ok_or_else(|| WireErr::at("unknown_field", "event.metadata"))?,
        };
        let ev = GateEvent {
            kind,
            address,
            author_external,
            author_display,
            content,
            mentions,
            reply_to,
            target,
            origin,
            attachments,
            discovery,
            metadata,
        };
        match self.sys().deliver_gate_event(&gate, ev) {
            // Some(seq)=追記/畳み・None=元栓で捨てた（DESIGN-attention §1・呼び手が seq null で ack）。
            Ok(outcome) => Ok(outcome),
            Err(EventReject::NotBound) => Err(WireErr::at("not_bound", "event.address")),
            Err(EventReject::Failed(m)) => Err(WireErr::at_detail("failed", "event", &m)),
        }
    }

    /// read を core へ渡し、ページを線の形（JSON）へ写す（プロトコル§02）。知らない欄は err（§00）。
    /// 結んでいない住所は not_bound。`from`/`limit` は任意（既定 `from=1`・`limit=上限`）。
    fn handle_read(
        &self,
        conn: &Conn,
        obj: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, WireErr> {
        const ALLOWED: &[&str] = &["id", "m", "address", "from", "limit"];
        if let Some(bad) = unknown_field(obj, ALLOWED) {
            return Err(WireErr::at("unknown_field", &format!("read.{bad}")));
        }
        let address = obj
            .get("address")
            .and_then(|x| x.as_str())
            .ok_or_else(|| WireErr::at("missing_field", "read.address"))?
            .to_string();
        // from/limit は任意。在れば整数であること（近い型に寄せない・§00）。
        let from = match obj.get("from") {
            None => 1,
            Some(v) => v
                .as_i64()
                .ok_or_else(|| WireErr::at("unknown_field", "read.from"))?,
        };
        let limit = match obj.get("limit") {
            None => READ_LIMIT_MAX,
            Some(v) => v
                .as_i64()
                .ok_or_else(|| WireErr::at("unknown_field", "read.limit"))?,
        };
        let gate = conn
            .gate
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| WireErr::new("not_ready"))?;
        if gate.spec.ingress_discovery == IngressDiscovery::Membership {
            return Err(WireErr::at("membership_read_unsupported", "read"));
        }
        match self
            .sys()
            .read_log(&gate.spec.kind_id, &address, from, limit)
        {
            Ok(page) => Ok(render_read_page(&page)),
            Err(ReadReject::NotBound) => Err(WireErr::at("not_bound", "read.address")),
            Err(ReadReject::Failed(m)) => Err(WireErr::at_detail("failed", "read", &m)),
        }
    }

    fn disconnect(&self, conn: &Arc<Conn>) {
        // Outgoing route checks hold this same lock through enqueue. Once false is visible,
        // no tool/activity/effect can be accepted by this connection.
        *conn.active.lock().unwrap() = false;
        let gate = conn.gate.lock().unwrap().clone();
        if let Some(g) = gate {
            let owned = {
                let mut conns = self.0.conns.lock().unwrap();
                if conns
                    .get(&g.instance_id)
                    .is_some_and(|current| Arc::ptr_eq(current, conn))
                {
                    conns.remove(&g.instance_id);
                    true
                } else {
                    false
                }
            };
            if !owned {
                conn.pending.lock().unwrap().clear();
                return;
            }
            let _ = self.sys().store().close_gate_connection(
                &g.instance_id,
                g.connection_epoch,
                None,
                0,
            );
            self.sys().unregister_gate_instance(&g.instance_id);
            let mut tools = self.0.tools.lock().unwrap();
            let retired = ToolOwner {
                instance_id: g.instance_id.clone(),
                connection_epoch: g.connection_epoch,
            };
            tools.retain(|_, owner| *owner != retired);
            let remaining: Vec<_> = self.0.conns.lock().unwrap().values().cloned().collect();
            for tool in &g.spec.tools {
                if tools.contains_key(&tool.name) {
                    continue;
                }
                if let Some(instance) = remaining.iter().find_map(|candidate| {
                    let candidate = candidate.gate.lock().unwrap();
                    candidate.as_ref().and_then(|candidate| {
                        candidate
                            .spec
                            .tools
                            .iter()
                            .any(|other| other.name == tool.name)
                            .then(|| ToolOwner {
                                instance_id: candidate.instance_id.clone(),
                                connection_epoch: candidate.connection_epoch,
                            })
                    })
                }) {
                    tools.insert(tool.name.clone(), instance);
                }
            }
        }
        // 未応答の待ち手は落ちる（oneshot が drop され、待っている Transport は失敗を受ける）。
        conn.pending.lock().unwrap().clear();
    }
}

// ---- core → plugin の要求（応答を伴う・プロトコル§02/§04）----

#[async_trait::async_trait]
impl Transport for Plugd {
    async fn compat_bind(&self, gate: &GateName, address: &str) -> Result<(), TransportError> {
        let mut req = serde_json::Map::new();
        req.insert("m".into(), "bind".into());
        req.insert("address".into(), address.into());
        self.request_ok(gate, req, BIND_TIMEOUT).await.map(|_| ())
    }

    async fn compat_unbind(&self, gate: &GateName, address: &str) -> Result<(), TransportError> {
        let mut req = serde_json::Map::new();
        req.insert("m".into(), "unbind".into());
        req.insert("address".into(), address.into());
        self.request_ok(gate, req, BIND_TIMEOUT).await.map(|_| ())
    }

    async fn compat_open(
        &self,
        gate: &GateName,
        under: &str,
        hint: Option<&str>,
    ) -> Result<String, TransportError> {
        let mut req = serde_json::Map::new();
        req.insert("m".into(), "open".into());
        req.insert("under".into(), under.into());
        if let Some(h) = hint {
            req.insert("hint".into(), h.into());
        }
        let ok = self.request_ok(gate, req, OPEN_TIMEOUT).await?;
        ok.get("address")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| TransportError("open ack lacks address".into()))
    }

    async fn compat_deliver_effect(
        &self,
        gate: &GateName,
        address: &str,
        effect: OutgoingEffect,
    ) -> Result<DeliveryAck, TransportError> {
        let mut payload = serde_json::Map::new();
        if let Some(t) = &effect.text {
            payload.insert("text".into(), t.clone().into());
        }
        if let Some(s) = &effect.symbol {
            payload.insert("symbol".into(), s.clone().into());
        }
        let mut req = serde_json::Map::new();
        req.insert("m".into(), "effect".into());
        req.insert("address".into(), address.into());
        req.insert("kind".into(), effect.kind.as_wire().into());
        req.insert("payload".into(), serde_json::Value::Object(payload));
        if let Some(t) = &effect.target_origin {
            req.insert("target".into(), t.clone().into());
        }
        // verb は素通し——core は不透明に運ぶだけで、ゲートが同じ kind の中で出し分ける材料にする
        // （zap→kind-9735 等・平文アクション文法）。散文 say や従来の効果では None なので載せない。
        if let Some(v) = &effect.verb {
            req.insert("verb".into(), v.clone().into());
        }
        let ok = self.request_ok(gate, req, EFFECT_TIMEOUT).await?;
        Ok(DeliveryAck {
            delivered: ok
                .get("delivered")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            origin: ok
                .get("origin")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
        })
    }

    async fn bind_route(&self, route: &GateRoute) -> Result<(), TransportError> {
        let mut req = serde_json::Map::new();
        req.insert("m".into(), "bind".into());
        req.insert("address".into(), route.address.clone().into());
        self.request_ok_instance(&route.instance_id, req, BIND_TIMEOUT)
            .await
            .map(|_| ())
    }

    async fn unbind_route(&self, route: &GateRoute) -> Result<(), TransportError> {
        let mut req = serde_json::Map::new();
        req.insert("m".into(), "unbind".into());
        req.insert("address".into(), route.address.clone().into());
        self.request_ok_instance(&route.instance_id, req, BIND_TIMEOUT)
            .await
            .map(|_| ())
    }

    async fn deliver_effect_route(
        &self,
        route: &GateRoute,
        _seq: Seq,
        effect: OutgoingEffect,
    ) -> TransportDeliveryResult {
        let Some(conn) = self.conn_for_instance(&route.instance_id) else {
            return TransportDeliveryResult::DefiniteFailure(TransportError(format!(
                "gate instance not connected: {}",
                route.instance_id
            )));
        };
        let mut payload = serde_json::Map::new();
        if let Some(text) = &effect.text {
            payload.insert("text".into(), text.clone().into());
        }
        if let Some(symbol) = &effect.symbol {
            payload.insert("symbol".into(), symbol.clone().into());
        }
        let mut req = serde_json::Map::new();
        req.insert("m".into(), "effect".into());
        req.insert("address".into(), route.address.clone().into());
        req.insert("kind".into(), effect.kind.as_wire().into());
        req.insert("payload".into(), serde_json::Value::Object(payload));
        if let Some(target) = &effect.target_origin {
            req.insert("target".into(), target.clone().into());
        }
        if let Some(verb) = &effect.verb {
            req.insert("verb".into(), verb.clone().into());
        }
        let mut rx = match conn.issue_for_route(route, req) {
            Ok((_id, rx)) => rx,
            Err(RouteIssueError::NotReady) => {
                return TransportDeliveryResult::DefiniteFailure(TransportError(format!(
                    "gate instance not ready: {}",
                    route.instance_id
                )))
            }
            Err(RouteIssueError::Stale) => {
                return TransportDeliveryResult::DefiniteFailure(TransportError(
                    "route connection epoch is no longer active".into(),
                ))
            }
            Err(RouteIssueError::WriterUnavailable) => {
                return TransportDeliveryResult::DefiniteFailure(TransportError(
                    "connection writer unavailable before effect acceptance".into(),
                ))
            }
        };
        match tokio::time::timeout(EFFECT_TIMEOUT, &mut rx).await {
            Ok(Ok(Ok(ok))) => {
                let Some(delivered) = ok.get("delivered").and_then(|value| value.as_bool()) else {
                    return TransportDeliveryResult::Indeterminate {
                        error: TransportError("effect ack lacks boolean delivered".into()),
                        late_observation: None,
                    };
                };
                let origin = match ok.get("origin") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(serde_json::Value::String(value)) => Some(value.clone()),
                    Some(_) => {
                        return TransportDeliveryResult::Indeterminate {
                            error: TransportError("effect ack has invalid origin".into()),
                            late_observation: None,
                        }
                    }
                };
                TransportDeliveryResult::DefiniteAck(DeliveryAck { delivered, origin })
            }
            Ok(Ok(Err(error))) => TransportDeliveryResult::DefiniteFailure(TransportError(
                format!("err: {}", error.code),
            )),
            Ok(Err(_)) => TransportDeliveryResult::Indeterminate {
                error: TransportError("connection dropped after effect acceptance".into()),
                late_observation: None,
            },
            Err(_) => TransportDeliveryResult::Indeterminate {
                error: TransportError("effect timed out after acceptance".into()),
                late_observation: Some(Box::pin(async move {
                    match rx.await {
                        Ok(Ok(value)) => serde_json::to_vec(&value).ok(),
                        Ok(Err(error)) => serde_json::to_vec(&error.to_json("")).ok(),
                        Err(_) => None,
                    }
                })),
            },
        }
    }
}

impl Plugd {
    /// 要求を送って ok を待つ。期限を過ぎたら失敗として扱い、接続は切らない（§00）。
    async fn request_ok(
        &self,
        gate: &GateName,
        req: serde_json::Map<String, serde_json::Value>,
        timeout: Duration,
    ) -> Result<serde_json::Value, TransportError> {
        let conn = self
            .conn_for_gate(gate)
            .ok_or_else(|| TransportError(format!("gate not connected: {gate}")))?;
        let (id, rx) = conn
            .issue_if_ready(req)
            .ok_or_else(|| TransportError("gate connection not ready".into()))?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(ok))) => Ok(ok),
            Ok(Ok(Err(e))) => Err(TransportError(format!("err: {}", e.code))),
            Ok(Err(_recv)) => Err(TransportError("connection dropped".into())),
            Err(_elapsed) => {
                // 期限切れ。待ち手を外す。後から応答が来ても捨てられる（§00）。
                conn.drop_pending(&id);
                Err(TransportError("timed out".into()))
            }
        }
    }

    async fn request_ok_instance(
        &self,
        instance: &GateInstanceId,
        req: serde_json::Map<String, serde_json::Value>,
        timeout: Duration,
    ) -> Result<serde_json::Value, TransportError> {
        let conn = self
            .conn_for_instance(instance)
            .ok_or_else(|| TransportError(format!("gate instance not connected: {instance}")))?;
        let (id, rx) = conn
            .issue_if_ready(req)
            .ok_or_else(|| TransportError(format!("gate instance not ready: {instance}")))?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(ok))) => Ok(ok),
            Ok(Ok(Err(error))) => Err(TransportError(format!("err: {}", error.code))),
            Ok(Err(_)) => Err(TransportError("connection dropped".into())),
            Err(_) => {
                conn.drop_pending(&id);
                Err(TransportError("timed out".into()))
            }
        }
    }
}

// ---- 活動の通知（core → plugin・応答なし・プロトコル§05）----

impl Notifier for Plugd {
    fn notify(&self, n: Notice) {
        let routed = match n.clone() {
            Notice::RoutedActivityStarted {
                route,
                activity,
                kind,
                label,
            } => {
                let mut body = serde_json::json!({"m":"activity","address":route.address.clone(),"activity_id":activity.to_string(),"kind":atag_wire(kind),"state":"started"});
                if let Some(label) = label {
                    body["label"] = label.into();
                }
                Some((route, body))
            }
            Notice::RoutedActivityProgress {
                route,
                activity,
                label,
            } => {
                let body = serde_json::json!({"m":"activity","address":route.address.clone(),"activity_id":activity.to_string(),"state":"progress","label":label});
                Some((route, body))
            }
            Notice::RoutedActivityEnded { route, activity } => {
                let body = serde_json::json!({"m":"activity","address":route.address.clone(),"activity_id":activity.to_string(),"state":"ended"});
                Some((route, body))
            }
            _ => None,
        };
        if let Some((route, body)) = routed {
            if let Some(conn) = self.conn_for_instance(&route.instance_id) {
                conn.send_for_route(&route, &body);
            }
            return;
        }
        // 通知は応答しない。描く手段が無いゲートは無視してよい（§05）。
        // 効果の配送は Transport で行うので、ここでは扱わない。
        let (place, body): (PlaceId, serde_json::Value) = match n {
            Notice::ActivityStarted {
                place,
                activity,
                kind,
                label,
            } => {
                let mut m = serde_json::json!({
                    "m": "activity",
                    "activity_id": activity.to_string(),
                    "kind": atag_wire(kind),
                    "state": "started",
                });
                if let Some(l) = label {
                    m["label"] = l.into();
                }
                (place, m)
            }
            Notice::ActivityProgress {
                place,
                activity,
                label,
            } => (
                place,
                serde_json::json!({
                    "m": "activity",
                    "activity_id": activity.to_string(),
                    "state": "progress",
                    "label": label,
                }),
            ),
            Notice::ActivityEnded { place, activity } => (
                place,
                serde_json::json!({
                    "m": "activity",
                    "activity_id": activity.to_string(),
                    "state": "ended",
                }),
            ),
            Notice::Effect { .. } => return, // 配送は Transport（§08）
            Notice::RoutedActivityStarted { .. }
            | Notice::RoutedActivityProgress { .. }
            | Notice::RoutedActivityEnded { .. } => unreachable!(),
        };
        // 活動は場の全チャネルへ描く。住所を添える（§05 の例）。
        let channels = match self.sys().store().channels_for_place(place) {
            Ok(c) => c,
            Err(_) => return,
        };
        for (gate, address) in channels {
            if let Some(conn) = self.conn_for_gate(&gate) {
                let mut m = body.clone();
                m["address"] = address.into();
                conn.send_if_ready(&m);
            }
        }
    }
}

// ---- ゲートのツールの実行（core → plugin・プロトコル§06）----

#[async_trait::async_trait]
impl ToolHost for Plugd {
    async fn invoke_route(
        &self,
        route: &GateRoute,
        call: &ToolCallSpec,
    ) -> Result<String, ToolError> {
        let conn = self.conn_for_instance(&route.instance_id).ok_or_else(|| {
            ToolError(format!(
                "gate instance not connected: {}",
                route.instance_id
            ))
        })?;
        let mut req = serde_json::Map::new();
        req.insert("m".into(), "tool".into());
        req.insert("name".into(), call.name.clone().into());
        req.insert("args".into(), call.args.clone());
        let (id, rx) = match conn.issue_for_route(route, req) {
            Ok(request) => request,
            Err(RouteIssueError::NotReady) => {
                return Err(ToolError(format!(
                    "gate instance not ready: {}",
                    route.instance_id
                )))
            }
            Err(RouteIssueError::Stale) => {
                return Err(ToolError(
                    "route connection epoch is no longer active".into(),
                ))
            }
            Err(RouteIssueError::WriterUnavailable) => {
                return Err(ToolError("connection writer unavailable".into()))
            }
        };
        let _guard = PendingGuard { conn, id };
        match rx.await {
            Ok(Ok(ok)) => ok
                .get("result")
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .ok_or_else(|| ToolError("tool ok lacks result".into())),
            Ok(Err(error)) => Err(ToolError(format!("tool err: {}", error.code))),
            Err(_) => Err(ToolError("connection dropped".into())),
        }
    }
}

// ---- 線の読み書き（プロトコル§00）----

async fn writer_loop<W>(mut write: W, mut out_rx: mpsc::UnboundedReceiver<String>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(line) = out_rx.recv().await {
        if write.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if write.write_all(b"\n").await.is_err() {
            break;
        }
        let _ = write.flush().await;
    }
}

enum ReadErr {
    TooLarge,
    Io,
}
use ReadErr::{Io, TooLarge};

/// 改行（0x0A）区切りの行読み。**まとめて読んでバッファ内で改行を探す**——1 バイトずつ読むと
/// 1 MiB のメッセージで約 100 万回の読みになり、攻撃の増幅にもなる（レビュー指摘 §7）。
/// 上限 1 MiB を超えたら TooLarge（§00）。EOF は `Ok(None)`。返す行に末尾の改行は含めない。
struct LineReader<R> {
    inner: R,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    fn new(inner: R) -> LineReader<R> {
        LineReader {
            inner,
            buf: Vec::new(),
        }
    }

    async fn next_line(&mut self) -> Result<Option<String>, ReadErr> {
        use tokio::io::AsyncReadExt;
        loop {
            // 溜まっている分に改行があれば、そこまでを 1 行として切り出す。
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                // 行そのものが上限を超えていれば too_large（改行が来ていても・§00）。
                if pos > MAX_LINE {
                    return Err(TooLarge);
                }
                let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
                line.pop(); // 末尾の '\n' を落とす
                return decode_line(line).map(Some);
            }
            // 改行が来ないまま上限を超えたら too_large（§00）。
            if self.buf.len() > MAX_LINE {
                return Err(TooLarge);
            }
            let mut chunk = [0u8; 64 * 1024];
            let n = self.inner.read(&mut chunk).await.map_err(|_| Io)?;
            if n == 0 {
                if self.buf.is_empty() {
                    return Ok(None); // EOF
                }
                // 末尾に改行なしで EOF。残りを 1 行として返す。
                let rest = std::mem::take(&mut self.buf);
                return decode_line(rest).map(Some);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// UTF-8 でなければ符号化違反として Io（枠組みが壊れている・§00「他は無い」）。
fn decode_line(raw: Vec<u8>) -> Result<String, ReadErr> {
    String::from_utf8(raw).map_err(|_| Io)
}

// ---- 小さな純関数（判断を含まない写し）----

/// read のページを線の形（JSON）へ写す（プロトコル§02）。判断はしない——core が組んだ値を並べるだけ。
/// `next` は続きがあるときだけ載せる。
fn render_read_page(page: &ReadPage) -> serde_json::Value {
    let events: Vec<serde_json::Value> = page.events.iter().map(render_read_event).collect();
    let mut ok = serde_json::Map::new();
    ok.insert("events".into(), serde_json::Value::Array(events));
    if let Some(n) = page.next {
        ok.insert("next".into(), n.into());
    }
    serde_json::Value::Object(ok)
}

/// read の 1 件を線の形へ（プロトコル§02）。無い欄は載せない（近いもので埋めない・§00）。
fn render_read_event(e: &ReadEvent) -> serde_json::Value {
    let mut author = serde_json::Map::new();
    if let Some(id) = &e.author_id {
        author.insert("id".into(), id.clone().into());
    }
    if let Some(d) = &e.author_display {
        author.insert("display".into(), d.clone().into());
    }
    let mut content = serde_json::Map::new();
    if let Some(t) = &e.content.text {
        content.insert("text".into(), t.clone().into());
    }
    if let Some(s) = &e.content.symbol {
        content.insert("symbol".into(), s.clone().into());
    }
    let mut m = serde_json::Map::new();
    m.insert("seq".into(), e.seq.into());
    m.insert("kind".into(), e.kind.as_str().into());
    m.insert("internal".into(), e.internal.into());
    m.insert("author".into(), serde_json::Value::Object(author));
    m.insert("content".into(), serde_json::Value::Object(content));
    if let Some(r) = e.reply_to {
        m.insert("reply_to".into(), r.into());
    }
    if let Some(o) = &e.origin {
        m.insert("origin".into(), o.clone().into());
    }
    serde_json::Value::Object(m)
}

fn unknown_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    allowed: &[&str],
) -> Option<String> {
    for k in obj.keys() {
        if !allowed.contains(&k.as_str()) {
            return Some(k.clone());
        }
    }
    None
}

fn parse_enum_set(
    obj: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<BTreeSet<EffectKind>, WireErr> {
    let arr = obj
        .get(field)
        .ok_or_else(|| WireErr::at("missing_field", &format!("hello.{field}")))?
        .as_array()
        .ok_or_else(|| WireErr::at("unknown_field", &format!("hello.{field}")))?;
    let mut set = BTreeSet::new();
    for e in arr {
        let s = e
            .as_str()
            .ok_or_else(|| WireErr::at("unknown_enum", &format!("hello.{field}")))?;
        let k = EffectKind::from_wire(s)
            .ok_or_else(|| WireErr::at_detail("unknown_enum", &format!("hello.{field}"), s))?;
        set.insert(k);
    }
    Ok(set)
}

fn parse_caps(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<BTreeSet<Capability>, WireErr> {
    let arr = obj
        .get("capabilities")
        .ok_or_else(|| WireErr::at("missing_field", "hello.capabilities"))?
        .as_array()
        .ok_or_else(|| WireErr::at("unknown_field", "hello.capabilities"))?;
    let mut set = BTreeSet::new();
    for e in arr {
        let s = e
            .as_str()
            .ok_or_else(|| WireErr::at("unknown_enum", "hello.capabilities"))?;
        let c = Capability::from_wire(s)
            .ok_or_else(|| WireErr::at_detail("unknown_enum", "hello.capabilities", s))?;
        set.insert(c);
    }
    Ok(set)
}

fn parse_tools(obj: &serde_json::Map<String, serde_json::Value>) -> Result<Vec<ToolDef>, WireErr> {
    let arr = obj
        .get("tools")
        .ok_or_else(|| WireErr::at("missing_field", "hello.tools"))?
        .as_array()
        .ok_or_else(|| WireErr::at("unknown_field", "hello.tools"))?;
    let mut out = vec![];
    for t in arr {
        let o = t
            .as_object()
            .ok_or_else(|| WireErr::at("unknown_field", "hello.tools"))?;
        let name = o
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or_else(|| WireErr::at("missing_field", "hello.tools.name"))?
            .to_string();
        let description = o
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let params = o.get("params").cloned().unwrap_or(serde_json::json!({}));
        out.push(ToolDef {
            name,
            description,
            params,
        });
    }
    Ok(out)
}

/// 平文アクションの宣言（hello の `actions`・平文アクション文法）。**省略可**——欄が無ければ []
/// （既存ゲートは無改変・版は上げない）。`kind` は閉じた列挙で、知らない値は unknown_enum（近いものへ
/// 寄せない・§00）。`params` は content の型を表す JSON Schema（不透明に保持・core が検証に使う）。
fn parse_actions(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<ActionDef>, WireErr> {
    let arr = match obj.get("actions") {
        None => return Ok(vec![]),
        Some(v) => v
            .as_array()
            .ok_or_else(|| WireErr::at("unknown_field", "hello.actions"))?,
    };
    let mut out = vec![];
    for a in arr {
        let o = a
            .as_object()
            .ok_or_else(|| WireErr::at("unknown_field", "hello.actions"))?;
        let name = o
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or_else(|| WireErr::at("missing_field", "hello.actions.name"))?
            .to_string();
        let description = o
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let params = o.get("params").cloned().unwrap_or(serde_json::json!({}));
        let kind_s = o
            .get("kind")
            .and_then(|x| x.as_str())
            .ok_or_else(|| WireErr::at("missing_field", "hello.actions.kind"))?;
        let kind = EffectKind::from_wire(kind_s)
            .ok_or_else(|| WireErr::at_detail("unknown_enum", "hello.actions.kind", kind_s))?;
        out.push(ActionDef {
            name,
            description,
            params,
            kind,
        });
    }
    Ok(out)
}

fn parse_membership_discovery(
    event: &serde_json::Map<String, serde_json::Value>,
) -> Result<MembershipDiscovery, WireErr> {
    let discovery = event
        .get("discovery")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| WireErr::at("missing_field", "event.discovery"))?;
    if let Some(bad) = unknown_field(discovery, &["address_kind", "guild_id", "label"]) {
        return Err(WireErr::at(
            "unknown_field",
            &format!("event.discovery.{bad}"),
        ));
    }
    let kind = match discovery
        .get("address_kind")
        .and_then(serde_json::Value::as_str)
    {
        Some("guild") => AddressKind::Guild,
        Some("dm") => AddressKind::Dm,
        Some("thread") => AddressKind::Thread,
        Some(other) => {
            return Err(WireErr::at_detail(
                "unknown_enum",
                "event.discovery.address_kind",
                other,
            ))
        }
        None => return Err(WireErr::at("missing_field", "event.discovery.address_kind")),
    };
    let guild_id = match discovery.get("guild_id") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| WireErr::at("invalid_field", "event.discovery.guild_id"))?
                .to_string(),
        ),
    };
    match kind {
        AddressKind::Guild if guild_id.is_none() => {
            return Err(WireErr::at("missing_field", "event.discovery.guild_id"))
        }
        AddressKind::Dm | AddressKind::Thread if guild_id.is_some() => {
            return Err(WireErr::at("unknown_field", "event.discovery.guild_id"))
        }
        _ => {}
    }
    let label = match discovery.get("label") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| WireErr::at("invalid_field", "event.discovery.label"))?
                .to_string(),
        ),
    };
    Ok(MembershipDiscovery {
        address_kind: kind,
        guild_id,
        label,
    })
}

/// 外から届く出来事の種別だけを受ける（プロトコル§03）。効果の確定でログに載る種別は届かない。
fn is_inbound_kind(k: EventKind) -> bool {
    matches!(
        k,
        EventKind::Said
            | EventKind::Edited
            | EventKind::Retracted
            | EventKind::Reacted
            | EventKind::UiAction
    )
}

fn atag_wire(k: ActivityKindTag) -> &'static str {
    match k {
        ActivityKindTag::Turn => "turn",
        ActivityKindTag::Background => "background",
    }
}
