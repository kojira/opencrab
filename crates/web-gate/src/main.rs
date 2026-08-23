//! web-gate — web ゲートのプラグイン（プロトコル 版1）。**別プロセス・別クレート**。
//!
//! 片側は人（HTTP）、片側は core（Unix ソケットの 1 行 1 メッセージ JSON）。
//! core の型は 1 つも使わない。線に載る JSON を仕様書だけを見て自分で組む（タスクの規律）。
//!
//! web は「入場を絞れるゲート」（基本§03）。**誰を通すかはゲート側の判断**で、core は知らない：
//! POST に正しいトークンが無ければ 403 を返し、**core へは出来事を送らない**。通した相手だけが
//! `event` になり、ターンが起きる。
//!
//! **ゲートは記録を持たない**（プロトコル§10）。会話の確かな記録は core のログだけ。web は自分の
//! 写しを持たない——持てば第 2 の真実源になり、プロセスが死ねば消え、連番は分かれ、他のチャネルから
//! 入った発話は欠ける。だから履歴の `GET` は、その都度 core の `read`（§02）で読む。core を落として
//! 開き直しても・web を落として上げ直しても、前の会話は core から見える（web には残骸が無い）。
//!
//! 使い方:
//!   web-gate <core_socket> <http_port> [token]
//!
//! HTTP:
//!   POST /rooms/<room>/messages   Authorization: Bearer <token>   body {"author","text"}
//!   GET  /rooms/<room>/messages?since=<seq>
//!
//! room は address_form `room:[a-z0-9-]+` に対応（住所 = `room:<room>`）。

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

const PROTOCOL: u64 = 1;
const ADDRESS_FORM: &str = "room:[a-z0-9-]+";
const MAX_LINE: usize = 1024 * 1024;
/// core への要求（event・read）の受理・応答を待つ期限（プロトコル§00 の event は 10 秒。
/// read も外界に触らないので同じでよい）。過ぎたら失敗として扱い、接続は切らない。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// core からの応答（`ok`/`err` の中身）。id で対応づいた待ち手へ渡す。
enum Resp {
    Ok(Value),
    Err(Value),
}

/// 発話に外界識別子（origin）を振る（プロトコル§03/§04）。**番号を振ることと、記録を持つことは
/// 別**（§10）——web は撤去した自前の記録を戻さず、連番の写しも持たない。プロセス起動時刻を種にした
/// 一意なトークンを都度作るだけで、seq との対応づけ（external_refs）は core が持つ。web はこのトークンを
/// 二度と再生成しない（履歴は core の `read` で読み戻す）。起動時刻を種にするので、再起動を跨いでも
/// 同じ場の中でトークンが衝突しない（別プロセス＝別の種）。
struct OriginMint {
    base: u128,
    ctr: AtomicU64,
}

impl OriginMint {
    fn new() -> OriginMint {
        let base = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        OriginMint {
            base,
            ctr: AtomicU64::new(1),
        }
    }
    fn next(&self) -> String {
        let n = self.ctr.fetch_add(1, Ordering::SeqCst);
        format!("web-{}-{}", self.base, n)
    }
}

/// core への 1 往復の結末（HTTP の状態コードへ写す）。
enum Outcome {
    Ok(Value),
    Err(Value),
    Timeout,
    LinkDown,
}

/// その場の**活動の揮発表示**（プロトコル§05）。core の activity 通知（started/progress/ended）を
/// そのまま写した、GET が読む一時的な状態。**記録ではない**（§10）——プロセスが死ねば消えてよい・
/// 切断で捨てる。会話の記録は core のログだけ（GET 本体は read で読む）。
///
/// `active` は「エージェントが応答を作っている（ターンが走っている）」——**turn 種別の活動だけ**を数える
/// （背景ツールの活動は数えない）。started(turn) で入り ended で出る id を持ち、空でなければ active。
/// `label` は PROGRESS（進捗の揮発表示）で来た文言。PROGRESS はターン末尾（応答全文のパース時）に出る
/// ため、ターンの終わりでクリアすると生存窓がミリ秒になって誰も見えない。文言が指すのは多くの場合
/// そのターンが切り離した背景ツールの仕事なので、**何かの活動（turn＋background）が走っている間**は
/// 保ち、場が静かになったら（running が空になったら）クリアする。
#[derive(Default)]
struct RoomActivity {
    /// 走っている turn 活動の id（線に載る文字列のまま）。started/ended で出入りする。
    turns: HashSet<String>,
    /// 走っているすべての活動の id（turn＋background）。label の寿命はこちらで決める。
    running: HashSet<String>,
    /// 直近の進捗文言（PROGRESS）。running が空になったら None に戻す。
    label: Option<String>,
}

struct Shared {
    /// いまの接続がこの住所を購読しているか（プロトコル§02 の bind）。POST を送る前の起動順ゲートに使う。
    /// **永続しない**——接続ごとの状態。履歴（GET/read）はこれに依らず、core の channels 表で解決される。
    bound: Mutex<HashMap<String, bool>>,
    /// 場ごとの活動の揮発表示（§05）。GET が `active`/`label` を読む。切断で捨てる（記録ではない・§10）。
    activity: Mutex<HashMap<String, RoomActivity>>,
    /// いまの接続への書き出し口。切れていれば None（POST/GET は 503 を返す）。
    outbound: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// 送った要求（event・read）の応答待ち（id → 待ち手）。core が ok/err を返したら渡す。
    pending: Mutex<HashMap<String, oneshot::Sender<Resp>>>,
    reqid: AtomicU64,
    token: String,
    /// launcher が起動ごとに渡す readiness token。通常の手動起動では endpoint 自体を出さない。
    launcher_ready_token: Option<String>,
    /// 発話・効果に振る外界識別子（§03/§04）。記録は持たない（§10）——番号を振るだけ。
    origins: OriginMint,
}

impl Shared {
    /// 出来事・効果に外界識別子を 1 つ振る（プロトコル§03/§04）。
    fn mint_origin(&self) -> String {
        self.origins.next()
    }
    fn set_bound(&self, room: &str, bound: bool) {
        self.bound.lock().unwrap().insert(room.to_string(), bound);
    }
    fn is_bound(&self, room: &str) -> bool {
        self.bound
            .lock()
            .unwrap()
            .get(room)
            .copied()
            .unwrap_or(false)
    }
    /// core の activity 通知（§05）を場の揮発表示へ写す。応答はしない（描く手段が無ければ無視してよい）。
    /// `active` は turn 種別の活動だけで数える（背景ツールの活動は「応答中」ではない）。`kind` は started に
    /// だけ載るので、started(turn) で id を控え、ended はその id を外すだけ（ended に kind は無い）。
    fn apply_activity(
        &self,
        room: &str,
        state: &str,
        aid: &str,
        kind: Option<&str>,
        label: Option<&str>,
    ) {
        let mut map = self.activity.lock().unwrap();
        let e = map.entry(room.to_string()).or_default();
        match state {
            // active に数えるのはターンだけ。label の寿命には背景ツールの活動も数える。
            "started" => {
                e.running.insert(aid.to_string());
                if kind == Some("turn") {
                    e.turns.insert(aid.to_string());
                }
            }
            // 進捗の揮発表示（PROGRESS）。走っている間の label を差し替える。
            "progress" => {
                if let Some(l) = label {
                    e.label = Some(l.to_string());
                }
            }
            // 終了で id を外す。label のクリアは**ターンの終了**でだけ判定する——背景ツールの決着で
            // 消すと「決着 → 次ターン開始」の空白に落ちて、次ターンが答えを作っている数分間
            // （文言が指す仕事の続き）に表示が消える。ターンが終わって場が静かなら答えは出た——消す。
            "ended" => {
                let was_turn = e.turns.remove(aid);
                e.running.remove(aid);
                if was_turn && e.running.is_empty() {
                    e.label = None;
                }
            }
            _ => {}
        }
    }
    /// GET が読む場の状態: `(active, label)`。未知の場は「静か（active=false・label なし）」。
    fn room_status(&self, room: &str) -> (bool, Option<String>) {
        match self.activity.lock().unwrap().get(room) {
            Some(e) => (!e.turns.is_empty(), e.label.clone()),
            None => (false, None),
        }
    }
    /// いまの接続がすべての結びと応答待ちを失ったことにする（切断・繋ぎ直しの前・§08）。
    fn drop_connection(&self) {
        *self.outbound.lock().unwrap() = None;
        self.pending.lock().unwrap().clear(); // 待ち手を落とす → POST/GET は失敗を受ける
        self.bound.lock().unwrap().clear();
        // 活動の揮発表示も捨てる（記録ではない・§10）。繋ぎ直したら新しい通知から組み直す。
        self.activity.lock().unwrap().clear();
    }
    fn send_line(&self, line: String) -> bool {
        match self.outbound.lock().unwrap().as_ref() {
            Some(tx) => tx.send(line).is_ok(),
            None => false,
        }
    }
    fn next_reqid(&self) -> String {
        format!("req-{}", self.reqid.fetch_add(1, Ordering::SeqCst))
    }

    /// core へ要求を 1 つ送り、応答（か期限切れ・切断）を待つ。
    async fn request(&self, id: String, req: Value) -> Outcome {
        let (tx, rx) = oneshot::channel::<Resp>();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        if !self.send_line(req.to_string()) {
            self.pending.lock().unwrap().remove(&id);
            return Outcome::LinkDown;
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(Resp::Ok(v))) => Outcome::Ok(v),
            Ok(Ok(Resp::Err(e))) => Outcome::Err(e),
            Ok(Err(_)) => Outcome::LinkDown, // 待ち手が落ちた（接続が切れた）
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Outcome::Timeout
            }
        }
    }
}

fn room_of(address: &str) -> Option<String> {
    address.strip_prefix("room:").map(|s| s.to_string())
}

/// 効果の合図（core → plugin・プロトコル§04）への応答本体（`ok`/`err`・id は呼び手が足す）。
///
/// web が運べると名乗ったのは say・react（§01）。core はそれ以外を送らないが、来たら unknown_enum（§00）。
/// **say は ack で origin を返す**（§04・自分の発話を後から指せるように）。react は origin を返さない（§04）。
/// 結んでいない住所への効果は not_bound（§03/§04）。
fn effect_response(kind: &str, addr: &str, mint: impl FnOnce() -> String) -> Value {
    match kind {
        "say" => {
            if room_of(addr).is_some() {
                json!({"ok": {"delivered": true, "origin": mint()}})
            } else {
                json!({"err": {"code":"not_bound","at":"effect.address"}})
            }
        }
        "react" => {
            if room_of(addr).is_some() {
                json!({"ok": {"delivered": true}})
            } else {
                json!({"err": {"code":"not_bound","at":"effect.address"}})
            }
        }
        // 名乗っていない種別は寄せない（§00）。
        other => json!({"err": {"code":"unknown_enum","at":"effect.kind","detail": other}}),
    }
}

/// 人の投稿を core への `event`（プロトコル§03）へ組む。**origin を必ず添える**——これが無いと
/// この発話には反応も返信もできない（§03）。番号（origin）は都度振るだけで、記録は持たない（§10）。
///
/// 本文中の URL は attachments として**事実のまま**添える（DESIGN-images §1・ゲートは判断しない）。
/// 由来作者は投稿者そのもの（web チャットにリポストは無い）。中身の取得はここでは一切しない——
/// 読むかはエージェントが core-look で選び、由来作者の信頼判定も core が行う。
fn build_event(id: &str, room: &str, author: &str, text: &str, origin: &str) -> Value {
    let attachments: Vec<Value> = urls_in(text)
        .into_iter()
        .map(|url| json!({"kind": "image", "url": url, "origin_author": author}))
        .collect();
    let mut ev = json!({
        "id": id, "m": "event", "kind": "said",
        "address": format!("room:{room}"),
        "author": {"id": author, "display": author},
        "content": {"text": text},
        "origin": origin,
    });
    if !attachments.is_empty() {
        ev["attachments"] = Value::Array(attachments);
    }
    ev
}

/// 本文から http(s) URL を出現順に拾う（重複もそのまま——選別・正規化はしない・DESIGN-images §1）。
/// 区切りは空白と `<>"` 引用符。末尾の句読点（。、,）だけ落とす（貼り付け文の実情に合わせた最小限）。
fn urls_in(text: &str) -> Vec<String> {
    let mut out = vec![];
    for token in text.split(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"') {
        if token.starts_with("http://") || token.starts_with("https://") {
            let trimmed = token.trim_end_matches(['。', '、', ',']);
            if trimmed.len() > "https://".len() {
                out.push(trimmed.to_string());
            }
        }
    }
    out
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let socket_path = args
        .next()
        .expect("usage: web-gate <core_socket> <http_port> [token]");
    let http_port: u16 = args
        .next()
        .expect("usage: web-gate <core_socket> <http_port> [token]")
        .parse()
        .expect("http_port");
    let token = args
        .next()
        .or_else(|| std::env::var("WEB_TOKEN").ok())
        .unwrap_or_else(|| "secret-token".to_string());
    let launcher_ready_token = std::env::var("OPENCRAB_WEB_READY_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());

    let shared = Arc::new(Shared {
        bound: Mutex::new(HashMap::new()),
        activity: Mutex::new(HashMap::new()),
        outbound: Mutex::new(None),
        pending: Mutex::new(HashMap::new()),
        reqid: AtomicU64::new(1),
        token,
        launcher_ready_token,
        origins: OriginMint::new(),
    });

    let link = tokio::spawn(run_core_link(socket_path, shared.clone()));
    let result = run_http(http_port, shared).await;
    link.abort();
    result
}

// ---- core への線（プロトコル§00-§06）----

async fn run_core_link(socket_path: String, shared: Arc<Shared>) {
    loop {
        // core が落ちている間は繋がらない。繋がるまで待つ（誰が起こすかはプロトコルの外・§08）。
        let stream = match UnixStream::connect(&socket_path).await {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        serve_one_connection(stream, &shared).await;
        // 切れた。名乗りからやり直す（§08）。結びも応答待ちも解けている。
        shared.drop_connection();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn serve_one_connection(stream: UnixStream, shared: &Arc<Shared>) {
    let (mut read_half, mut write_half) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

    let writer = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if write_half.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = write_half.flush().await;
        }
    });

    // 名乗り（接続して最初に 1 回・§01）。tools は空、運べる効果は say だけ、capability 無し。
    // **hello を先に列へ入れてから** outbound を HTTP 側へ見せる——さもないと GET/POST が read/event を
    // hello より先に送り、core が「名乗る前に喋った」として切る（§02）。順序は 1 本の列で守る。
    // 運べる効果は say と react（§04）。宛先つきの say＝返信、react＝反応。宛先の解決は core が持つ
    // external_refs で行う——web が out/in で origin を返す/送ることで初めて、宛先つきの効果が使える。
    // アクションも宣言する（平文アクション文法）: reply→say・react→react。**アクション語彙は宣言駆動**で、
    // core は語を持たない（前文は形だけを語る）。宣言しないと前文が教えない語を使えず、モデルが未宣言の
    // `reply:…` を出して逐語配送される漏れになる（E2E 回帰の是正）。<対象> は返信/反応する発言の番号。
    let hello = json!({
        "id": "hello-1", "m": "hello", "protocol": PROTOCOL,
        "name": "web", "address_form": ADDRESS_FORM,
        "tools": [], "effects": ["say", "react"], "capabilities": [],
        "actions": [
            {"name": "reply", "kind": "say",
             "description": "誰かの発言に返信する（対象＝その発言の番号）", "params": {}},
            {"name": "react", "kind": "react",
             "description": "誰かの発言にリアクションする（対象＝その発言の番号）", "params": {}}
        ]
    });
    let _ = out_tx.send(hello.to_string());
    *shared.outbound.lock().unwrap() = Some(out_tx.clone());

    // 読み取りループ。**枠組みが壊れたら切る**（UTF-8 でない・JSON でない・物体でない・1 MiB 超）——
    // core 側が守る対称の規律（プロトコル§00「知らない／壊れは黙って通さない」）。
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    'read: loop {
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            if pos > MAX_LINE {
                break 'read; // 1 MiB 超 → 切る（§00）
            }
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = &line[..line.len() - 1];
            let s = match std::str::from_utf8(line) {
                Ok(s) => s,
                Err(_) => break 'read, // UTF-8 でない → 枠組み違反、切る（§00）
            };
            let v: Value = match serde_json::from_str(s) {
                Ok(v) => v,
                Err(_) => break 'read, // JSON でない → 切る（§00）
            };
            if !v.is_object() {
                break 'read; // 物体でない → 切る（§00）
            }
            handle_core_message(&v, shared, &out_tx);
        }
        if buf.len() > MAX_LINE {
            break;
        }
        match read_half.read(&mut chunk).await {
            Ok(0) => break, // EOF = core が切れた／落ちた
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    drop(out_tx);
    let _ = writer.await;
}

/// core からの 1 メッセージを処理する。`m` があれば要求、無ければ（自分の要求/hello への）応答。
fn handle_core_message(v: &Value, shared: &Arc<Shared>, out: &mpsc::UnboundedSender<String>) {
    let id = v.get("id").and_then(|x| x.as_str()).map(|s| s.to_string());
    let m = match v.get("m").and_then(|x| x.as_str()) {
        Some(m) => m,
        None => {
            // core → plugin 要求ではない = 自分が送った要求（event・read・hello）への応答。
            // 応答待ちがあれば結果を渡す。無ければ捨てる（hello の ok など・§00）。
            if let Some(id) = id {
                if let Some(waiter) = shared.pending.lock().unwrap().remove(&id) {
                    let resp = if let Some(ok) = v.get("ok") {
                        Resp::Ok(ok.clone())
                    } else if let Some(err) = v.get("err") {
                        Resp::Err(err.clone())
                    } else {
                        return; // ok も err も無い応答は捨てる（対応づけられない・§00）
                    };
                    let _ = waiter.send(resp);
                }
            }
            return;
        }
    };
    match m {
        // 活動の通知（応答しない・§05）。場の揮発表示（active/label）へ写す——GET が読む。
        // 描く手段はゲート側の判断: web は `active`（ターンが走っているか）と `label`（進捗の揮発表示）
        // だけを保つ。記録ではないので切断で捨てる（§10）。
        "activity" => {
            let addr = v.get("address").and_then(|x| x.as_str()).unwrap_or("");
            if let Some(room) = room_of(addr) {
                let state = v.get("state").and_then(|x| x.as_str()).unwrap_or("");
                let aid = v.get("activity_id").and_then(|x| x.as_str()).unwrap_or("");
                let kind = v.get("kind").and_then(|x| x.as_str());
                let label = v.get("label").and_then(|x| x.as_str());
                shared.apply_activity(&room, state, aid, kind, label);
            }
        }
        "bind" | "unbind" => {
            let addr = v.get("address").and_then(|x| x.as_str()).unwrap_or("");
            if let Some(room) = room_of(addr) {
                shared.set_bound(&room, m == "bind");
            }
            if let Some(id) = id {
                let _ = out.send(json!({"id": id, "ok": {}}).to_string());
            }
        }
        "effect" => {
            let id = id.unwrap_or_default();
            let addr = v.get("address").and_then(|x| x.as_str()).unwrap_or("");
            let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("");
            // 効果は core 側で既に確定し、core のログに載っている（web は GET で `read` して読む・§02/§04）。
            // web は記録を持たないので、ここでは受け取った合図を返すだけ。運ぶ先＝読める像なので delivered。
            // say は ack で origin を返す（§04）——**自分の発話を後から指せるように**（返信・反応の宛先に
            // なる）。react は §04 で origin を返さない。番号を振るだけで、seq との対応づけは core が持つ。
            let mut resp = effect_response(kind, addr, || shared.mint_origin());
            if let Some(obj) = resp.as_object_mut() {
                obj.insert("id".into(), id.into());
            }
            let _ = out.send(resp.to_string());
        }
        // web は tool も open も名乗っていないので来ないはず。来たら err（近いものに寄せない・§00）。
        other => {
            if let Some(id) = id {
                let _ = out.send(
                    json!({"id": id, "err": {"code":"unknown_message","detail": other}})
                        .to_string(),
                );
            }
        }
    }
}

// ---- HTTP（人の側）----

async fn run_http(port: u16, shared: Arc<Shared>) -> std::io::Result<()> {
    let bind_addr = std::env::var("WEB_GATE_BIND").unwrap_or_else(|_| "127.0.0.1".to_string());
    let listener = TcpListener::bind((bind_addr.as_str(), port)).await?;
    eprintln!("web-gate: http on {bind_addr}:{port}");
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => continue,
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let _ = handle_http(stream, shared).await;
        });
    }
}

struct Req {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<Req> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, val)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), val.trim().to_string());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Some(Req {
        method,
        path,
        query,
        headers,
        body,
    })
}

async fn handle_http(
    mut stream: tokio::net::TcpStream,
    shared: Arc<Shared>,
) -> std::io::Result<()> {
    let req = match read_request(&mut stream).await {
        Some(r) => r,
        None => return write_response(&mut stream, 400, &json!({"error":"bad request"})).await,
    };

    // 起動ごとの token を知る、この web-gate だけが成功する launcher 専用 probe。
    // 別サービスや以前の web-gate が同じ port で応答しても ready にはならない。
    if let Some(readiness) = launcher_readiness(
        &req.method,
        &req.path,
        shared.launcher_ready_token.as_deref(),
    ) {
        return match readiness {
            LauncherReadiness::Ready(token) => write_text(&mut stream, 200, token).await,
            LauncherReadiness::NotFound => {
                write_response(&mut stream, 404, &json!({"error":"not found"})).await
            }
        };
    }

    // 体験用の 1 枚チャット画面（依存なし・localhost 前提）。`GET /` か `GET /chat` で HTML を返す。
    // これはゲートの記録ではない——画面の JS が下の `GET/POST /rooms/<room>/messages` を叩き、履歴は
    // その都度 core の `read`（§02）で読む。web は写しを持たない（§10）。
    if req.method == "GET" && (req.path == "/" || req.path == "/chat") {
        return write_html(&mut stream, CHAT_HTML).await;
    }

    let room = req
        .path
        .strip_prefix("/rooms/")
        .and_then(|rest| rest.strip_suffix("/messages"))
        .map(|s| s.to_string());
    let room = match room {
        Some(r)
            if !r.is_empty()
                && r.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') =>
        {
            r
        }
        _ => return write_response(&mut stream, 404, &json!({"error":"not found"})).await,
    };

    match req.method.as_str() {
        "GET" => handle_get(&mut stream, &req, &room, &shared).await,
        "POST" => handle_post(&mut stream, &req, &room, &shared).await,
        _ => write_response(&mut stream, 405, &json!({"error":"method not allowed"})).await,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum LauncherReadiness<'a> {
    Ready(&'a str),
    NotFound,
}

/// Recognize the launcher-only route and preserve the token from this exact
/// web-gate process.  `None` means this is an ordinary HTTP route; `NotFound`
/// hides the launcher endpoint when the process was not started with a token.
fn launcher_readiness<'a>(
    method: &str,
    path: &str,
    token: Option<&'a str>,
) -> Option<LauncherReadiness<'a>> {
    if method != "GET" || path != "/__opencrab_launcher_ready" {
        return None;
    }
    Some(match token {
        Some(token) => LauncherReadiness::Ready(token),
        None => LauncherReadiness::NotFound,
    })
}

/// 履歴を返す。web は自分の記録を持たない——その都度 core の `read`（§02）で読む。
async fn handle_get(
    stream: &mut tokio::net::TcpStream,
    req: &Req,
    room: &str,
    shared: &Arc<Shared>,
) -> std::io::Result<()> {
    let since: u64 = req
        .query
        .split('&')
        .find_map(|kv| kv.strip_prefix("since="))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // `read` は from（含む）。since より後を読むので from = since + 1。limit は省略（core が上限で丸める）。
    let id = shared.next_reqid();
    let read = json!({
        "id": id, "m": "read",
        "address": format!("room:{room}"),
        "from": since + 1,
    });
    match shared.request(id, read).await {
        Outcome::Ok(ok) => {
            let messages = match read_to_messages(&ok) {
                Ok(messages) => messages,
                Err(()) => {
                    return write_response(
                        stream,
                        502,
                        &json!({"error":"invalid core read projection"}),
                    )
                    .await;
                }
            };
            // 場の揮発表示（§05）を添える: `active`（ターンが走っているか）と `label`（進捗・無ければ null）。
            // 履歴（messages）は core の read、活動（active/label）は core の activity 通知の写し——別の源。
            let (active, label) = shared.room_status(room);
            let mut body = json!({
                "room": room,
                "messages": messages,
                "active": active,
                "label": label,
            });
            // 続きがあれば next を渡す（次は ?since=next-1 で取り直す・§02）。
            if let Some(n) = ok.get("next").and_then(|x| x.as_i64()) {
                body["next"] = n.into();
            }
            write_response(stream, 200, &body).await
        }
        Outcome::Err(e) => {
            // まだ結ばれていない（起動順・再接続直後）→ 503。他は 502。
            let code = e.get("code").and_then(|x| x.as_str()).unwrap_or("");
            if code == "not_bound" {
                write_response(stream, 503, &json!({"error":"room not ready"})).await
            } else {
                write_response(stream, 502, &json!({"error":"core error"})).await
            }
        }
        Outcome::Timeout => write_response(stream, 504, &json!({"error":"core timeout"})).await,
        Outcome::LinkDown => write_response(stream, 503, &json!({"error":"core link down"})).await,
    }
}

/// `read` の 1 ページ（events）を、web の HTTP の形（messages）へ写す。
/// core が決めた `internal` の印はそのまま運び、チャット画面が既定表示から分離できるようにする。
/// 発話（said/spoke）を人／エージェントとして見せ、他の種別も生ログとしては落とさない。
fn read_to_messages(ok: &Value) -> Result<Vec<Value>, ()> {
    let mut out = vec![];
    if let Some(arr) = ok.get("events").and_then(|x| x.as_array()) {
        for ev in arr {
            let seq = ev.get("seq").and_then(|x| x.as_i64()).unwrap_or(0);
            let kind_wire = ev.get("kind").and_then(|x| x.as_str()).unwrap_or("");
            let internal = ev.get("internal").and_then(|x| x.as_bool()).ok_or(())?;
            let kind = match kind_wire {
                "said" => "user",
                "spoke" => "agent",
                other => other,
            };
            let who = ev
                .pointer("/author/display")
                .and_then(|x| x.as_str())
                .or_else(|| ev.pointer("/author/id").and_then(|x| x.as_str()))
                .unwrap_or("系");
            let text = ev
                .pointer("/content/text")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            // react（ReactEffect）の絵文字は content.symbol に載る（text ではない）。画面が
            // 「エージェントBが 🎉 をつけた」と描けるよう素通しする（黙って落とさない・§02）。
            let symbol = ev
                .pointer("/content/symbol")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            out.push(json!({
                "seq": seq,
                "who": who,
                "text": text,
                "kind": kind,
                "symbol": symbol,
                "internal": internal,
            }));
        }
    }
    Ok(out)
}

async fn handle_post(
    stream: &mut tokio::net::TcpStream,
    req: &Req,
    room: &str,
    shared: &Arc<Shared>,
) -> std::io::Result<()> {
    // 入場を絞る（§03）。トークンが合わなければ 403。**core へは何も送らない** — 通さない相手は系に存在しない。
    let authed = req
        .headers
        .get("authorization")
        .map(|h| h == &format!("Bearer {}", shared.token))
        .unwrap_or(false);
    if !authed {
        return write_response(stream, 403, &json!({"error":"forbidden"})).await;
    }

    let body: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(_) => return write_response(stream, 400, &json!({"error":"invalid json"})).await,
    };
    // author も text も欠けていたら 400（片方だけ緩くしない）。既定で埋めない。
    let author = match body.get("author").and_then(|x| x.as_str()) {
        Some(a) if !a.is_empty() => a.to_string(),
        _ => return write_response(stream, 400, &json!({"error":"author required"})).await,
    };
    let text = match body.get("text").and_then(|x| x.as_str()) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => return write_response(stream, 400, &json!({"error":"text required"})).await,
    };

    // core がまだこの場を購読していない（bind 前・再接続直後）→ 503。運べないものを送らない（起動順）。
    if !shared.is_bound(room) {
        return write_response(stream, 503, &json!({"error":"room not ready"})).await;
    }

    // event を送って**受理を待つ**。連番も記録も core が持つ（§02）。origin は都度振る——**記録は持たず
    // 番号だけ**（§10）。これで人の発話にも外界識別子が付き、エージェントが特定の発話へ返信・反応できる（§03）。
    let id = shared.next_reqid();
    let origin = shared.mint_origin();
    let ev = build_event(&id, room, &author, &text, &origin);
    match shared.request(id, ev).await {
        Outcome::Ok(ok) => {
            let seq = ok.get("seq").and_then(|x| x.as_i64()).unwrap_or(0);
            write_response(stream, 202, &json!({"accepted": true, "seq": seq})).await
        }
        Outcome::Err(_) => write_response(stream, 502, &json!({"error":"core rejected"})).await,
        Outcome::Timeout => write_response(stream, 504, &json!({"error":"core timeout"})).await,
        Outcome::LinkDown => write_response(stream, 503, &json!({"error":"core link down"})).await,
    }
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &Value,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "OK",
    };
    let body = serde_json::to_vec(body).unwrap();
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await
}

async fn write_text(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &str,
) -> std::io::Result<()> {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

/// 体験用のチャット画面を返す（`text/html`）。JSON の `write_response` と別経路——本文は HTML。
async fn write_html(stream: &mut tokio::net::TcpStream, html: &str) -> std::io::Result<()> {
    let body = html.as_bytes();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

/// 体験用の 1 枚チャット画面（インライン・依存なし）。作り込まない——体験が目的（統括タスク）。
/// 画面は `GET/POST /rooms/<room>/messages` を叩くだけ。履歴は core の `read`（§02）が権威で、web は
/// 写しを持たない（§10）。room とトークンは画面上で入れ替えられる（既定 room=main）。
const CHAT_HTML: &str = include_str!("chat.html");

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// web が「発話に外界識別子を振る」規律（プロトコル§03/§04・§10）を、線に載る JSON の形で確かめる。
// core の型は使わない——ゲート側の規律なので、JSON を手で組んで検査する（別クレートの依存にも opencrab-* は無い）。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_readiness_returns_only_its_own_start_token() {
        let readiness = launcher_readiness(
            "GET",
            "/__opencrab_launcher_ready",
            Some("this-launch-token"),
        );
        assert_eq!(
            readiness,
            Some(LauncherReadiness::Ready("this-launch-token"))
        );
        assert_ne!(
            readiness,
            Some(LauncherReadiness::Ready("another-launch-token"))
        );
    }

    #[test]
    fn launcher_readiness_is_hidden_without_a_start_token() {
        assert_eq!(
            launcher_readiness("GET", "/__opencrab_launcher_ready", None),
            Some(LauncherReadiness::NotFound)
        );
        assert_eq!(
            launcher_readiness("POST", "/__opencrab_launcher_ready", Some("token")),
            None
        );
    }

    // origin は毎回別の値（同じ場の中で衝突しない）。記録を持たずに一意な番号を振る（§10）。
    #[test]
    fn mint_gives_distinct_origins() {
        let m = OriginMint::new();
        let a = m.next();
        let b = m.next();
        let c = m.next();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        assert!(a.starts_with("web-"), "origin の書式: {a}");
    }

    // 別プロセス（別の OriginMint）は別の種を持つので、再起動を跨いでもトークンが衝突しない（§10）。
    #[test]
    fn distinct_processes_get_distinct_prefixes() {
        // base（起動時刻）は単調に進むので、2 つ作れば必ず桁が違う。時刻が同一でも counter は 1 から
        // 始まるが、prefix（base）が同じになるのは同一プロセスだけ——ここでは base の分離を確かめる。
        let m1 = OriginMint::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let m2 = OriginMint::new();
        assert_ne!(m1.base, m2.base, "別プロセスは別の種を持つべき");
    }

    // 人の投稿を event へ組むとき、**origin を必ず添える**（§03「無ければ反応も返信もできない」）。
    #[test]
    fn built_event_carries_origin() {
        let ev = build_event("req-1", "main", "test-owner", "これ見た？", "web-42-7");
        assert_eq!(ev.get("m").and_then(|x| x.as_str()), Some("event"));
        assert_eq!(ev.get("kind").and_then(|x| x.as_str()), Some("said"));
        assert_eq!(
            ev.get("address").and_then(|x| x.as_str()),
            Some("room:main")
        );
        assert_eq!(
            ev.pointer("/author/id").and_then(|x| x.as_str()),
            Some("test-owner")
        );
        assert_eq!(
            ev.pointer("/content/text").and_then(|x| x.as_str()),
            Some("これ見た？")
        );
        // ここが肝：origin を送るからこそ、この発話が宛先つき効果の対象になれる（§03）。
        assert_eq!(ev.get("origin").and_then(|x| x.as_str()), Some("web-42-7"));
    }

    // #751: HTTP 投影は core の internal marker を失わない。画面はこの印を見て既定非表示にする。
    #[test]
    fn read_projection_preserves_core_internal_marker() {
        let messages = read_to_messages(&json!({
            "events": [
                {
                    "seq": 1,
                    "kind": "said",
                    "internal": false,
                    "author": {"display": "Synthetic Human"},
                    "content": {"text": "hello"}
                },
                {
                    "seq": 2,
                    "kind": "settled",
                    "internal": true,
                    "author": {},
                    "content": {"text": "synthetic result"}
                }
            ]
        }))
        .expect("valid core projection");
        assert_eq!(messages.len(), 2, "raw HTTP projection keeps both rows");
        assert_eq!(messages[0].get("internal"), Some(&json!(false)));
        assert_eq!(messages[1].get("internal"), Some(&json!(true)));
        assert!(
            CHAT_HTML.contains("if (m.internal === true) return;"),
            "the chat surface hides core-marked internal rows by default"
        );
    }

    #[test]
    fn read_projection_rejects_a_missing_internal_marker() {
        let result = read_to_messages(&json!({
            "events": [{
                "seq": 1,
                "kind": "said",
                "author": {"display": "Synthetic Human"},
                "content": {"text": "hello"}
            }]
        }));
        assert!(result.is_err(), "missing core judgment must not fall back");
    }

    // say の ack は origin を返す（§04・自分の発話を後から指せるように）。
    #[test]
    fn say_ack_returns_origin() {
        let r = effect_response("say", "room:main", || "web-1-1".to_string());
        assert_eq!(
            r.pointer("/ok/delivered").and_then(|x| x.as_bool()),
            Some(true)
        );
        assert_eq!(
            r.pointer("/ok/origin").and_then(|x| x.as_str()),
            Some("web-1-1"),
            "say は ack で origin を返す（§04）"
        );
    }

    // react の ack は origin を返さない（§04）。運べる効果としては受理する（delivered）。
    #[test]
    fn react_ack_omits_origin() {
        let r = effect_response("react", "room:main", || "web-1-2".to_string());
        assert_eq!(
            r.pointer("/ok/delivered").and_then(|x| x.as_bool()),
            Some(true)
        );
        assert!(
            r.pointer("/ok/origin").is_none(),
            "react は origin を返さない（§04）: {r}"
        );
    }

    // 名乗っていない種別は近いものへ寄せず unknown_enum（§00）。
    #[test]
    fn unknown_effect_kind_errs() {
        let r = effect_response("boost", "room:main", || "x".to_string());
        assert_eq!(
            r.pointer("/err/code").and_then(|x| x.as_str()),
            Some("unknown_enum")
        );
    }

    // 結んでいない住所への効果は not_bound（§03/§04）。
    #[test]
    fn effect_to_bad_address_is_not_bound() {
        let r = effect_response("say", "not-a-room", || "x".to_string());
        assert_eq!(
            r.pointer("/err/code").and_then(|x| x.as_str()),
            Some("not_bound")
        );
    }

    // 活動の揮発表示（§05）の保持/クリアを検査する。core の型は使わず、線に載る JSON の値だけで駆動する。
    fn test_shared() -> Shared {
        Shared {
            bound: Mutex::new(HashMap::new()),
            activity: Mutex::new(HashMap::new()),
            outbound: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            reqid: AtomicU64::new(1),
            token: "t".into(),
            launcher_ready_token: None,
            origins: OriginMint::new(),
        }
    }

    // 未知の場は静か（active=false・label なし）。
    #[test]
    fn unknown_room_is_quiet() {
        let s = test_shared();
        assert_eq!(s.room_status("main"), (false, None));
    }

    // ターンの開始→進捗→終了で active/label が動く。started(turn) で active、progress で label、
    // ended で active も label も戻る（設計「ended で label クリア」）。
    #[test]
    fn turn_lifecycle_drives_active_and_label() {
        let s = test_shared();
        s.apply_activity("main", "started", "a1", Some("turn"), None);
        assert_eq!(
            s.room_status("main"),
            (true, None),
            "ターンが走っている＝active"
        );

        s.apply_activity("main", "progress", "a1", None, Some("3 件目を読んでいます"));
        assert_eq!(
            s.room_status("main"),
            (true, Some("3 件目を読んでいます".into())),
            "PROGRESS で label が載る"
        );

        s.apply_activity("main", "ended", "a1", None, None);
        assert_eq!(
            s.room_status("main"),
            (false, None),
            "ターン終了で active も label も戻る"
        );
    }

    // 背景ツールの活動は active にしない（turn 種別だけが「応答中」）。
    #[test]
    fn background_activity_does_not_make_room_active() {
        let s = test_shared();
        s.apply_activity("main", "started", "bg1", Some("background"), None);
        assert_eq!(
            s.room_status("main"),
            (false, None),
            "背景ツールの活動は active ではない"
        );
    }

    // 進捗の label はターンが走っている間だけ。ターンが無いのに progress が来ても（順序の乱れ）、
    // label は載るが active にはならない——そして ended（turns 空）で確実にクリアされる。
    #[test]
    fn label_is_cleared_when_turn_ends() {
        let s = test_shared();
        s.apply_activity("main", "started", "a1", Some("turn"), None);
        s.apply_activity("main", "progress", "a1", None, Some("下ごしらえ中"));
        assert_eq!(s.room_status("main").1.as_deref(), Some("下ごしらえ中"));
        s.apply_activity("main", "ended", "a1", None, None);
        assert!(s.room_status("main").1.is_none(), "ended で label クリア");
    }

    #[test]
    fn build_event_attaches_urls_with_poster_as_origin_author() {
        let ev = build_event(
            "req-1",
            "main",
            "test-owner",
            "これ見て https://x.test/a.png と https://x.test/b。",
            "o-1",
        );
        let atts = ev.get("attachments").and_then(|x| x.as_array()).unwrap();
        assert_eq!(atts.len(), 2);
        assert_eq!(
            atts[0].get("url").and_then(|x| x.as_str()),
            Some("https://x.test/a.png")
        );
        // 末尾の句点だけ落ちる。
        assert_eq!(
            atts[1].get("url").and_then(|x| x.as_str()),
            Some("https://x.test/b")
        );
        for a in atts {
            assert_eq!(a.get("kind").and_then(|x| x.as_str()), Some("image"));
            assert_eq!(
                a.get("origin_author").and_then(|x| x.as_str()),
                Some("test-owner")
            );
        }
        // URL の無い本文は attachments 自体を載せない（後方互換）。
        let ev2 = build_event("req-2", "main", "test-owner", "こんにちは", "o-2");
        assert!(ev2.get("attachments").is_none());
    }
}
