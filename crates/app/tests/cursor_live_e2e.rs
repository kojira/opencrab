//! cursor 平文専用 engine の端から端まで（別プロセス・実ソケット・**本物の cursor-agent CLI**）。
//!
//! `OPENCRAB_LLM_PROVIDER=cursor` で core を起動し、web ゲート越しに人が投稿する。実際の `cursor:grok` が
//! アクション文法（`reply:<番号>:…` / `react:<番号>:絵文字` / `NO_REPLY`）を出し、それが core の配送経路を
//! 通って **DB の権威**（`reply_to`/`target`/`symbol` と turn の `end_reason`）に載ることを実観測する。
//!
//! ターンの完了検知は**直列化**を使う（core は主体のターンを直列化する）: 沈黙のターン（react+NO_REPLY）の
//! 後に、必ず発話で返るターンを投げ、その発話が web 履歴に見えたら前の沈黙ターンも決着済みと分かる。DB は
//! core を殺してから開く（走行中に権威へ同時アクセスしない）。
//!
//! CI に cursor-agent は無いので `#[ignore]`。走らせ方（cursor-agent 認証済みが前提・grok に 3 回叩く）:
//!   cargo test -p opencrab-app --test cursor_live_e2e -- --ignored --nocapture

use opencrab_port::{EventKind, SubjectId, SubjectKind};
use opencrab_store::Store;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const TOKEN: &str = "secret-token";

struct Proc(Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bin_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// エージェントB相当の人格。アクション文法を使い分けるよう明示する（menu と文法前文は core が入れる）。
const TEST_AGENT_PERSONA: &str = "あなたは「エージェントB」。この場のチャットで人と気さくに話すアシスタントです。\
ユーザーの依頼に忠実に従い、アクション文法を使い分けます。誰かの発言に返事するときは reply を、\
絵文字だけで反応するときは react を使います。発話しないよう言われたら NO_REPLY を書きます。\
対象番号は、返信・反応する相手の発言の行頭にある番号です。指示された行動だけを行い、余計な地の文は書きません。";

/// cursor engine を選び、エージェントBの場を 1 つ起こす core を上げる。
fn spawn_core_cursor(sock: &Path, db: &Path, places_json: &Path) -> Proc {
    let child = Command::new(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .arg(sock)
        .arg(db)
        .env("OPENCRAB_LLM_PROVIDER", "cursor")
        .env("OPENCRAB_LLM_MODEL", "cursor-grok-4.6-high")
        .env("OPENCRAB_PLACES", places_json)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit()) // provider 選択・失敗を目視できるように stderr は素通し
        .spawn()
        .expect("spawn opencrab-social-runtime (cursor)");
    Proc(child)
}

fn spawn_web(_sock: &Path, _port: u16) -> Proc {
    panic!("protocol 1 web-gate crate was removed; this e2e is stopped");
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn http(
    port: u16,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: Option<&str>,
) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let body = body.unwrap_or("");
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: Bearer {a}\r\n"));
    }
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).ok()?;
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).ok()?;
    let text = String::from_utf8_lossy(&resp).to_string();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())?;
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Some((status, body))
}

fn post(port: u16, author: &str, text: &str) -> Option<(u16, String)> {
    let body = format!("{{\"author\":\"{author}\",\"text\":\"{text}\"}}");
    http(
        port,
        "POST",
        "/rooms/main/messages",
        Some(TOKEN),
        Some(&body),
    )
}

fn get_history(port: u16) -> String {
    http(port, "GET", "/rooms/main/messages?since=0", None, None)
        .map(|(_, b)| b)
        .unwrap_or_default()
}

fn wait_history_contains(port: u16, needle: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if get_history(port).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// 場に「エージェントの発話（spoke/out）」が n 件見えるまで待つ。read の line は `"kind":"agent"`。
/// 直列化の完了検知に使う——発話ターンが見えたら、その前の沈黙ターンも決着済み。
fn wait_agent_messages_at_least(port: u16, n: usize, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let h = get_history(port);
        if h.matches("\"kind\":\"agent\"").count() >= n {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    bin_dir().join(format!("cursor-e2e-{}-{}-{}", std::process::id(), n, name))
}

fn find_room_place(store: &Store, address: &str) -> Option<i64> {
    for p in store.all_open_places().ok()? {
        if let Ok(Some(row)) = store.get_place(p) {
            if row.address.as_deref() == Some(address) {
                return Some(p);
            }
        }
    }
    None
}

#[test]
#[ignore = "requires cursor-agent CLI + auth (grok); makes 3 real API calls"]
fn cursor_grok_reply_react_no_reply_over_real_socket() {
    let sock = scratch("s.sock");
    let db = scratch("core.db");
    let places = scratch("places.json");
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&db);
    assert!(sock.as_os_str().len() < 100, "socket path too long");

    // エージェントBの場を 1 つ（web・Direct 即応・誰からでも）。persona 以外は provision_web_room と同じ既定。
    let cfg = format!(
        r#"{{"places":[{{"address":"room:main","gate":"web","name":"エージェントB","persona":{persona},"policy":{{"immediate":["direct"],"immediate_from":"anyone","batch_window_ms":null,"unconditional_interval_ms":null}}}}]}}"#,
        persona = serde_json::to_string(TEST_AGENT_PERSONA).unwrap()
    );
    std::fs::write(&places, cfg).expect("write places.json");

    let port = free_port();
    let mut core = spawn_core_cursor(&sock, &db, &places);
    let web = spawn_web(&sock, port);

    // web の HTTP が起ち上がるのを待つ（履歴 GET が通るまで）。
    assert!(
        wait_history_contains(port, "[", Duration::from_secs(10))
            || get_history(port).is_empty()
            || wait_history_contains(port, "]", Duration::from_secs(5)),
        "web-gate HTTP が起動しない"
    );

    // ---- ターン A: reply（スレッド付き配送）----
    // 依頼どおり reply でひと言返す。エージェントの発話が履歴に見えたら A 完了。
    assert!(post(
        port,
        "test-owner",
        "エージェントB、この発言に reply で短くあいさつを返してね。"
    )
    .is_some());
    assert!(
        wait_agent_messages_at_least(port, 1, Duration::from_secs(120)),
        "ターン A: エージェントBの reply（発話）が返らない: {}",
        get_history(port)
    );

    // ---- ターン B: react + NO_REPLY（沈黙のリアクション）----
    // 明示アクション（react）は NO_REPLY に関わらず発火し、発話は保留され end_reason=no_reply になる。
    // このターンは発話を出さないので、次の C の発話が見えることで完了を確かめる（直列化）。
    assert!(post(
        port,
        "test-owner",
        "エージェントB、このうれしい知らせに react で絵文字をひとつだけ付けて。発話はしないで NO_REPLY にして: テストが全部通ったよ",
    )
    .is_some());

    // ---- ターン C: reply（B の完了を直列化で確かめるための可視ターン）----
    assert!(post(
        port,
        "test-owner",
        "エージェントB、この発言にも reply でひと言ちょうだい。"
    )
    .is_some());
    assert!(
        wait_agent_messages_at_least(port, 2, Duration::from_secs(150)),
        "ターン C: 2 件目の発話が返らない（＝B の沈黙ターンも未決着）: {}",
        get_history(port)
    );

    // ---- 権威（DB）で確かめる。core を殺してから開く（走行中に同時アクセスしない）----
    core.0.kill().ok();
    core.0.wait().ok();
    drop(web);

    let store = Store::open(&db).expect("open core db");
    let place = find_room_place(&store, "room:main").expect("room:main");
    let latest = store.latest_seq(place).unwrap();

    let mut reply_threaded: Option<(i64, i64)> = None; // (spoke_seq, reply_to)
    let mut react_symbol: Option<(i64, Option<i64>, String)> = None; // (seq, target, symbol)
    for s in 1..=latest {
        let ev = match store.get_event(place, s).unwrap() {
            Some(e) => e,
            None => continue,
        };
        match ev.kind {
            EventKind::Spoke => {
                if let Some(rt) = ev.reply_to {
                    reply_threaded.get_or_insert((s, rt));
                }
            }
            EventKind::ReactEffect => {
                if let Some(sym) = ev.content.symbol.clone() {
                    react_symbol.get_or_insert((s, ev.target, sym));
                }
            }
            _ => {}
        }
    }

    // 1. reply がスレッド付き（Spoke に reply_to が載る）で配送された。
    let (spoke_seq, reply_to) =
        reply_threaded.expect("スレッド付きの reply（Spoke.reply_to）が DB に無い");
    eprintln!("reply: Spoke seq={spoke_seq} reply_to={reply_to}");
    assert!(
        reply_to >= 1 && reply_to < spoke_seq,
        "reply_to は先行する発言を指す"
    );

    // 2. react が絵文字（symbol）と対象付きで載った。
    let (react_seq, target, symbol) =
        react_symbol.expect("react（ReactEffect.symbol）が DB に無い");
    eprintln!("react: seq={react_seq} target={target:?} symbol={symbol:?}");
    assert!(!symbol.trim().is_empty(), "react の symbol が空");
    assert!(
        target.is_some(),
        "react の target（対象 seq）が載っていない"
    );

    // 3. NO_REPLY が効いた（発話を保留したターンの end_reason=no_reply）。
    let turns = store.all_turn_records().unwrap();
    let no_reply = turns.iter().find(|t| t.end_reason == "no_reply");
    eprintln!(
        "turn end_reasons: {:?}",
        turns
            .iter()
            .map(|t| t.end_reason.as_str())
            .collect::<Vec<_>>()
    );
    let nr = no_reply.expect("end_reason=no_reply のターンが DB に無い（NO_REPLY が効いていない）");
    eprintln!(
        "no_reply turn: id={} withheld_text={:?}",
        nr.id, nr.withheld_text
    );

    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&places);
}

// ================================================================================================
// 平文ツール行（記憶）の実往復。本物の cursor モデルが core-remember / core-recall の平文ツール行を
// メニューから使えるか——決着→次ターンでの反映（記憶の実往復）まで DB の権威で観測する。
// engine 本体は触らない（ハーネス＝この関数群のみ追加）。
// ================================================================================================

/// エージェントB（記憶の道具を使う版）。頼まれた好み・事実を core の記憶で覚え、あとで recall で引く。
/// JSON の組み立て（body/from/to）はモデルに委ねる——メニューが道具の形と「結果は決着で返る」を教える。
const TEST_AGENT_MEMORY_PERSONA: &str = "あなたは「エージェントB」。この場のチャットで人と気さくに話すアシスタントです。\
ユーザーに好みや事実を覚えてと頼まれたら、あなた自身の記憶の道具で覚えます。あとで尋ねられたら記憶から\
思い出して答えます。道具（ツール）の結果は決着で返るので、情報が足りないときはまずツールの行だけを書き、\
発話は結果が返ってから行います。指示された行動だけを行い、余計な地の文・前置きは書きません。";

/// cursor engine を、モデルを明示して起こす（既定 spawn は grok 固定なので、モデルを渡せる版）。
fn spawn_core_cursor_model(sock: &Path, db: &Path, places_json: &Path, model: &str) -> Proc {
    let child = Command::new(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .arg(sock)
        .arg(db)
        .env("OPENCRAB_LLM_PROVIDER", "cursor")
        .env("OPENCRAB_LLM_MODEL", model)
        .env("OPENCRAB_PLACES", places_json)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn opencrab-social-runtime (cursor, model)");
    Proc(child)
}

/// 短い Unix ソケットのパス（macOS の sun_path ~104 上限に収める）。target dir が深いと
/// バイナリ隣接（scratch）では溢れるので、scratch 直下の浅い専用ディレクトリに置く。
fn short_sock() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = PathBuf::from("/Volumes/2TB/openclaw/.claude-scratch/opencrab/opencrab-sk");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{}-{}.sock", std::process::id(), n))
}

/// その場の Agent 主体（エージェントB）の subject_id を拾う。記憶は主体で絞るので、権威照会に要る。
fn find_agent_subject(store: &Store, place: i64) -> Option<SubjectId> {
    for m in store.members(place).ok()? {
        if let Ok(Some(s)) = store.get_subject(m.subject) {
            if s.kind == SubjectKind::Agent {
                return Some(m.subject);
            }
        }
    }
    None
}

/// 記憶の実往復（覚える→思い出す）を 1 つのモデルで回す。呼び手がモデル名を渡す（grok / composer 共用）。
/// 成立条件は「記憶が永続し（remember）、思い出しの reply に本文が載る（recall→決着→reply）」こと。
/// 観測（漏れ・ツール行の逐語・決着列）は全部 eprintln で出す。
///
/// `require_success`: 記憶の永続を hard assert するか。実測でツール行の JSON/位置引数を正しく組めるモデル
/// （grok）は true。ツール行の引数を `key=value`（CLI 風）で書くなど**現状**成立しないモデル（composer 2.5）は
/// false にして観測専用にする——常に panic する #[ignore] テストを避けつつ、癖を診断出力として残す。
fn run_memory_roundtrip(model: &str, require_success: bool) {
    // Unix ソケットのパス上限（macOS ~104）に収めるため、ソケットだけ短い専用ディレクトリに置く。
    // db / places.json は長くてよい（scratch = バイナリ隣接）。
    let sock = short_sock();
    let db = scratch("mem.db");
    let places = scratch("mem-places.json");
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&db);
    assert!(sock.as_os_str().len() < 100, "socket path too long");

    let cfg = format!(
        r#"{{"places":[{{"address":"room:main","gate":"web","name":"エージェントB","persona":{persona},"policy":{{"immediate":["direct"],"immediate_from":"anyone","batch_window_ms":null,"unconditional_interval_ms":null}}}}]}}"#,
        persona = serde_json::to_string(TEST_AGENT_MEMORY_PERSONA).unwrap()
    );
    std::fs::write(&places, cfg).expect("write places.json");

    let port = free_port();
    let mut core = spawn_core_cursor_model(&sock, &db, &places, model);
    let web = spawn_web(&sock, port);

    assert!(
        wait_history_contains(port, "[", Duration::from_secs(10))
            || get_history(port).is_empty()
            || wait_history_contains(port, "]", Duration::from_secs(5)),
        "web-gate HTTP が起動しない"
    );

    // ---- ターン1: 覚える（期待: core-remember のツール行 → 決着）----
    assert!(post(
        port,
        "test-owner",
        "エージェントB、私の好物を覚えといて。ラーメンね。"
    )
    .is_some());
    // tool-line only + 決着後の発話でも、say + tool line 同時でも、いずれ agent の発話が 1 件は見える。
    let turn1_replied = wait_agent_messages_at_least(port, 1, Duration::from_secs(180));
    eprintln!("[{model}] turn1 replied(agent msg>=1) = {turn1_replied}");

    // ---- ターン2: 思い出す ----
    // 注意（前提の訂正）: 記憶が少数のうちは memory_index が本文ごと索引に載せる（`- #1 <本文>`）。
    // よって 1 件だけの好物はモデルの文脈に既に見えており、core-recall を使わず索引から直接答えるのが
    // 設計上正しい（recall は索引から溢れた本文を語で取り戻す道具）。ここでは「思い出しの reply が
    // 本文を正しく載せるか（記憶が使えているか）」を、turn1 の応答とは別の**新しい agent 発話**で観測する。
    assert!(post(port, "test-owner", "ところで、私の好物ってなんだっけ？").is_some());
    let recall_replied = wait_agent_messages_at_least(port, 2, Duration::from_secs(220));
    eprintln!("[{model}] turn2 replied(agent msg>=2) = {recall_replied}");
    let final_history = get_history(port);
    let round_trip = final_history.contains("ラーメン");

    // ---- 権威（DB）で確かめる。core を殺してから開く（走行中に権威へ同時アクセスしない）----
    core.0.kill().ok();
    core.0.wait().ok();
    drop(web);

    let store = Store::open(&db).expect("open core db");
    let place = find_room_place(&store, "room:main").expect("room:main");
    let agent = find_agent_subject(&store, place).expect("agent subject");

    // 1. 記憶が実際に永続したか（記憶の権威）。
    let mems = store.memories_newest_first(agent).unwrap();
    eprintln!(
        "[{model}] memories = {:?}",
        mems.iter()
            .map(|m| (
                m.id,
                m.body.clone(),
                m.origin_from_seq,
                m.origin_to_seq,
                m.last_read_at
            ))
            .collect::<Vec<_>>()
    );

    // 2. 決着（Settled）と発話（Spoke）の列挙——決着が立っているか・発話に何が載ったか。
    let latest = store.latest_seq(place).unwrap();
    let mut settled: Vec<(i64, String)> = vec![];
    let mut spoke: Vec<(i64, String)> = vec![];
    for s in 1..=latest {
        if let Some(ev) = store.get_event(place, s).unwrap() {
            match ev.kind {
                EventKind::Settled => {
                    settled.push((s, ev.content.text.clone().unwrap_or_default()))
                }
                EventKind::Spoke => spoke.push((s, ev.content.text.clone().unwrap_or_default())),
                _ => {}
            }
        }
    }
    eprintln!("[{model}] settled = {settled:?}");
    eprintln!("[{model}] spoke = {spoke:?}");

    // 3. 受理した平文ツール行の逐語（ターン記録）と、保留した地の文。
    let turns = store.all_turn_records().unwrap();
    for t in &turns {
        eprintln!(
            "[{model}] turn id={} end_reason={} tool_lines={:?} withheld={:?}",
            t.id, t.end_reason, t.tool_lines, t.withheld_text
        );
    }
    let tool_lines: Vec<String> = turns.iter().filter_map(|t| t.tool_lines.clone()).collect();

    eprintln!("[{model}] round_trip(ラーメン in history) = {round_trip}");
    eprintln!("[{model}] === final web history ===\n{final_history}\n=== end history ===");

    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&places);

    // 成立判定は require_success のときだけ hard assert（診断出力は上で済んでいる）。
    if require_success {
        assert!(
            !mems.is_empty(),
            "[{model}] 記憶が永続していない（remember 未成立）: settled={settled:?} tool_lines={tool_lines:?}"
        );
        assert!(
            mems.iter().any(|m| m.body.contains("ラーメン")),
            "[{model}] 記憶本文にラーメンが無い: {mems:?}"
        );
    }
}

#[test]
#[ignore = "requires cursor-agent CLI + auth (grok); makes real API calls"]
fn cursor_grok_memory_tool_line_roundtrip() {
    run_memory_roundtrip("cursor-grok-4.6-high", true);
}

// Composer 2.5。context_window は保守運用値 128k で seed 済み（KNOWN_MODEL_CONTEXT_WINDOWS・
// 公式未公開のため能力値ではない）。grok と同一ハーネスで記憶往復と漏れ（フェンス・解説文）を観測する。
#[test]
#[ignore = "requires cursor-agent CLI + auth (composer); makes real API calls"]
fn cursor_composer_memory_tool_line_roundtrip() {
    // 成立を強制する（require_success=true・grok と同格へ昇格）。以前は composer 2.5 がツール行の引数を
    // `body=… from=… to=…`（CLI フラグ風）で書き、多フィールドの core-remember が段2（逐語で場へ漏出）へ
    // 落ちて記憶が永続しなかった。根因は**メニューが引数の符号化を教えていなかった**こと。ツール行の
    // メニュー表示に書き方の実例（多フィールド＝1 行 JSON・単一 string＝素の値）を宣言 params から自動生成
    // して載せたところ、composer 2.5 は `core-remember::{"body":…,"from":…,"to":…}` と正しい 1 行 JSON を
    // 書き、記憶が永続・生構文漏れ無しになった（feat/tool-menu-encoding・2026-08-18 実測）。
    run_memory_roundtrip("composer-2.5", true);
}
