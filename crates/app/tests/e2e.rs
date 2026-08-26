//! 別プロセス・実ソケットでの端から端まで（タスクの完了基準）。
//!
//! ここで守るのは「プロセス内で繋いだ往復」ではなく、**実際に別プロセスで、実際のソケット越しに**
//! 動くこと。起動の順序・繋ぎ直し・落ちても戻ることは、ここで初めて出る。
//!
//! 走らせるもの:
//!   - `opencrab-social-runtime`（core のプロセス。Unix ソケットでプラグインを受ける）
//!   - `web-gate`（web ゲートのプラグイン。別プロセス。線に載る JSON を自分で組む）
//!
//! テストは人の代わりに HTTP（curl 相当）で叩く。
//!
//! **どちらのバイナリも「いまのコード」であることを cargo が保証する**（タスク #1）:
//! 両方とも app 自身の bin 目標（`opencrab-social-runtime`／`web-gate-e2e`）なので、cargo は
//! `CARGO_BIN_EXE_*` をこのテストに渡す前に**必ず再ビルドする**。手順書に「先に build しろ」と
//! 書く必要が無い — 書き忘れても古いバイナリで緑にならない。`web-gate-e2e` は別クレート `web` の
//! ソースを app の bin として持つだけで、実体は同じ web-gate（Cargo.toml 参照）。

use opencrab_store::Store;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const TOKEN: &str = "secret-token";

/// 子プロセスの取っ手。テストが panic しても Drop で確実に殺す（ゾンビを残さない）。
struct Proc(Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bin_dir() -> PathBuf {
    // scratch のパス置き場（target 配下）。バイナリ本体は CARGO_BIN_EXE_* で引く。
    PathBuf::from(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn spawn_core(sock: &Path, db: &Path) -> Proc {
    // 同一パッケージの bin なので cargo が必ず作り直す（古いバイナリで通らない・#1）。
    let child = Command::new(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .arg(sock)
        .arg(db)
        .arg("room:main")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn opencrab-social-runtime");
    Proc(child)
}

fn spawn_web(_sock: &Path, _port: u16) -> Proc {
    panic!("protocol 1 web-gate crate was removed; this e2e is stopped");
}

/// 落として上げ直す。**先に殺してから**新しいのを上げる（旧プロセスが線を握ったまま
/// 新プロセスが同じソケット／ポートに来る取り違えを避ける — 落ちても戻ることを正しく試すため）。
fn restart(slot: &mut Proc, f: impl FnOnce() -> Proc) {
    slot.0.kill().ok();
    slot.0.wait().ok();
    *slot = f();
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// 最小の HTTP クライアント（curl 相当）。Connection: close なので EOF まで読む。
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

fn post(port: u16, author: &str, text: &str, token: &str) -> Option<(u16, String)> {
    let body = format!("{{\"author\":\"{author}\",\"text\":\"{text}\"}}");
    http(
        port,
        "POST",
        "/rooms/main/messages",
        Some(token),
        Some(&body),
    )
}

fn get_history(port: u16) -> String {
    http(port, "GET", "/rooms/main/messages?since=0", None, None)
        .map(|(_, b)| b)
        .unwrap_or_default()
}

/// 投稿する → ターンが起きる → 返ってくる、を **返るまで** 試す。
///
/// 落ちた直後は web がまだ core の切断に気づいておらず（あるいは繋ぎ直しの最中で）、投稿が
/// 取りこぼされ得る（拾わない・§05）。人のクライアントと同じく、返しが見えるまで投げ直す。
/// これが起動順・繋ぎ直し・落ちても戻ることの、外から見た収束点。返しが見えたら true。
fn roundtrip(port: u16, author: &str, text: &str, timeout: Duration) -> bool {
    let reply = format!("受け取りました:「{text}」");
    let start = Instant::now();
    let mut last_post = Instant::now() - Duration::from_secs(1);
    while start.elapsed() < timeout {
        if get_history(port).contains(&reply) {
            return true;
        }
        // 200ms ごとに投げ直す（間で GET を刻んで返しを待つ）。
        if last_post.elapsed() >= Duration::from_millis(200) {
            let _ = post(port, author, text, TOKEN);
            last_post = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// GET の履歴が needle を含むまで待つ（web の HTTP が起動しきるのも兼ねる）。
fn wait_history_contains(port: u16, needle: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if get_history(port).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn scratch(name: &str) -> PathBuf {
    // ソケットのパスは短く保つ（macOS の sun_path は 104 バイト上限）。target 配下（= /Volumes/2TB 側）に置く。
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = bin_dir();
    dir.join(format!("e2e-{}-{}-{}", std::process::id(), n, name))
}

#[test]
#[ignore = "protocol 1 web-gate crate was removed"]
fn end_to_end_over_real_socket_and_process() {
    let sock = scratch("s.sock");
    let db = scratch("core.db");
    // 初回は残骸を消してから（前回の DB が混じらないように・手順書とも対応）。
    // web は記録を持たない（§10）ので消すのは core の権威（DB とソケット）だけ。
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&db);
    assert!(
        sock.as_os_str().len() < 100,
        "Unix ソケットのパスが長すぎる（{} bytes）: {}",
        sock.as_os_str().len(),
        sock.display()
    );
    let port = free_port();

    // ---- 起動 ----
    let mut core = spawn_core(&sock, &db);
    let mut web = spawn_web(&sock, port);

    // ---- 目標3: curl で端から端まで（投稿 → ターン → 返る）----
    // 初回は起動・結びの収束も兼ねて長めに待つ（別プロセス・実ソケット）。
    assert!(
        roundtrip(
            port,
            "test-owner",
            "these-are-real-sockets",
            Duration::from_secs(30)
        ),
        "ターンが起きてエージェントの発話が返る: {}",
        get_history(port)
    );
    assert!(
        get_history(port).contains("\"kind\":\"agent\""),
        "返しはエージェントの発話として載る: {}",
        get_history(port)
    );

    // ---- 目標2/規律: web は入場を絞る。通さない相手は core に存在しない ----
    let (status, _) = post(port, "intruder", "let-me-in-please", "WRONG-TOKEN").unwrap();
    assert_eq!(status, 403, "トークンが合わなければ 403");
    // 少し待っても、通していない投稿は履歴（= web が core から受けた効果）に現れない。
    std::thread::sleep(Duration::from_millis(500));
    let h = get_history(port);
    assert!(
        !h.contains("let-me-in-please"),
        "通さなかった相手の発話は系に入らない: {h}"
    );

    // ---- 新テスト1: web を落として上げ直しても前の会話が見える（写しが消えて困らないこと・§10）----
    // web は記録を持たない。落として上げ直すと、履歴は core の `read`（§02）で読み直す——
    // だから写しが消えても会話は消えない。第 2 の真実源が無いので、そもそも消える写しが無い。
    restart(&mut web, || spawn_web(&sock, port));
    assert!(
        wait_history_contains(port, "these-are-real-sockets", Duration::from_secs(10)),
        "web 再起動後も前の会話が見える（core の read で読む・§10/§02）: {}",
        get_history(port)
    );
    assert!(
        roundtrip(
            port,
            "test-owner",
            "after-plugin-restart",
            Duration::from_secs(30)
        ),
        "プラグインを落として上げ直したら、繋ぎ直して返る: {}",
        get_history(port)
    );

    // ---- fix 3: core だけ落とす → 戻る。**core の DB に前の出来事が残っていることを直接確かめる** ----
    core.0.kill().ok();
    core.0.wait().ok();
    // core が握っていない今、DB を直接開いて、web の GET 越しではなく **権威（core のログ）** を測る。
    {
        let store = Store::open(&db).expect("open core db");
        let place = find_room_place(&store, "room:main").expect("room:main の場が DB にある");
        let latest = store.latest_seq(place).unwrap();
        let mut texts = String::new();
        for s in 1..=latest {
            if let Some(ev) = store.get_event(place, s).unwrap() {
                if let Some(t) = ev.content.text {
                    texts.push_str(&t);
                    texts.push('\n');
                }
            }
        }
        assert!(
            texts.contains("these-are-real-sockets") && texts.contains("after-plugin-restart"),
            "core の DB に前の会話が残っている（web のメモリではなく権威で確認・fix 3）: {texts}"
        );

        // ---- 目標1（web でも宛先つきの効果が使える下地）: web が発話に外界識別子（origin）を振る ----
        // 人の投稿(said・in)にもエージェントの発話(spoke・out)にも origin が external_refs に載る。
        // これが**別プロセスの本物の web-gate**を通って記録されていることを、権威（core の DB）で直接確かめる。
        // 番号を振るだけで web は記録を持たない（§10）——写しではなく core の表に seq と対応づいて残る（§03/§08）。
        // 最初の往復は said=seq1・spoke=seq2（発火方針は Direct 即応のみ、まとめ・無条件は無い）。
        assert!(
            store.place_has_external_refs(place).unwrap(),
            "web が origin を振るので、場に宛先にできる出来事がある（§08）"
        );
        assert!(
            store.external_ref_of(place, 1).unwrap().is_some(),
            "人の発話に外界識別子が付く（特定の発話へ返信・反応できる・§03）"
        );
        assert!(
            store.external_ref_of(place, 2).unwrap().is_some(),
            "エージェントの発話に外界識別子が付く（自分の発話を後から指せる・§04/§08）"
        );
    } // store を閉じてから core を上げ直す
    core = spawn_core(&sock, &db); // 同じ DB+socket で上げ直す
    assert!(
        roundtrip(
            port,
            "test-owner",
            "after-core-restart",
            Duration::from_secs(30)
        ),
        "core 再起動後もターンが起きて返る（場は同じ DB から復元）: {}",
        get_history(port)
    );

    // ---- 目標4: 両方落とす → 戻る ----
    web.0.kill().ok();
    web.0.wait().ok();
    core.0.kill().ok();
    core.0.wait().ok();
    core = spawn_core(&sock, &db);
    web = spawn_web(&sock, port);
    assert!(
        roundtrip(
            port,
            "test-owner",
            "after-both-restart",
            Duration::from_secs(30)
        ),
        "両方再起動後もターンが起きて返る: {}",
        get_history(port)
    );

    // 後始末（Drop でも殺すが明示的に）。
    drop(web);
    drop(core);
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&db);
}

/// core の DB を直接開いて、住所 `room:xxx` の場を引く（fix 3 の直接確認に使う）。
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
