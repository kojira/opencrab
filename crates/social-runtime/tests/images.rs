//! DESIGN-images.md のテスト節（画像=添付を場に流し、見るかはエージェントが選ぶ）。
//!
//! 推論は差し替えた偽物（ScriptedEngine）、fetch は fake（FakeFetcher）で回す。時間は
//! tokio::time::pause()（start_paused）で進める。外界ゲート経路（`deliver_event`）で添付つき出来事を
//! 流し込み、core-look（画像）/ core-read（本文）を native tool_call として呼ぶ（本物のプロバイダと
//! 同じ core の判定・実行を通る）。
//!
//! 固定する性質:
//! - 描画に**存在と番地だけ**が出る（URL は描かない・§2）。
//! - core-look の正常（画像がプロバイダの形で会話に入る）・各異常（404・非画像・上限超過）が fail loud（§3）。
//! - look/read の取得判定は §5 の 1 本: 由来作者が owner か信頼リストなら通り、未信頼・由来不明は理由つきで拒否。
//! - リポスト（信頼フォロイーが未信頼投稿を包む）で取得が拒否される（信頼の非継承・§5）。
//! - engine が画像を受けない（accepts_images=false）ときメニューから core-look が消える（§6）。

use opencrab_engine::*;
use opencrab_port::*;
use opencrab_social_runtime::*;
use opencrab_store::Store;
use std::sync::Arc;

struct Harness {
    sys: System,
    eng: ScriptedEngine,
    fetch: FakeFetcher,
}

const TEST_MODEL: &str = "scripted";
const GATE: &str = "web";
const ADDR: &str = "room:main";

fn build() -> Harness {
    let store = Store::new_in_memory().unwrap();
    store
        .register_model_context_window(TEST_MODEL, 1_000_000)
        .unwrap();
    let eng = ScriptedEngine::new();
    let host = ScriptedToolHost::new();
    let fetch = FakeFetcher::new();
    let notif = RecordingNotifier::new();
    let sys = System::new(
        store,
        Arc::new(eng.clone()),
        Arc::new(host),
        Arc::new(ScriptedShellHost::new()),
        Arc::new(notif),
        Arc::new(CharCounter),
        Config::default(),
    );
    sys.attach_fetcher(Arc::new(fetch.clone()) as Arc<dyn Fetcher>);
    Harness { sys, eng, fetch }
}

async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

/// PNG のマジック（実バイト検査を通す最小のラスタ・§3）。
fn png_bytes() -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    v.extend_from_slice(b"........fake-png-body........");
    v
}

/// 場と 1 体のエージェント A（Direct で即応・default_subject）と owner O（web 素性つき）を用意する。
/// (place, agent) を返す。owner の外界識別子は `owner_ext`。
fn firing_place(h: &Harness, owner_ext: &str) -> (PlaceId, SubjectId) {
    let a = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let o = h
        .sys
        .create_subject(SubjectKind::Human, "O", "O", Standing::Owner);
    let place = h.sys.create_place(
        Some(ADDR),
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    h.sys.join(place, a, Role::Participant);
    h.sys.join(place, o, Role::Participant);
    h.sys.add_identity(o, GATE, owner_ext);
    h.sys
        .store()
        .add_channel(place, &GateName::new(GATE), ADDR)
        .unwrap();
    (place, a)
}

/// 外界ゲートから届く said（添付つき・origin つき）。`atts` は (url, origin_author) の並び。
fn said_with(
    author_ext: &str,
    text: &str,
    origin: &str,
    atts: &[(&str, Option<&str>)],
) -> GateEvent {
    GateEvent {
        kind: EventKind::Said,
        address: ADDR.to_string(),
        author_external: author_ext.to_string(),
        author_display: None,
        content: Content::text(text),
        mentions: vec![],
        reply_to: None,
        target: None,
        origin: Some(origin.to_string()),
        attachments: atts
            .iter()
            .map(|(url, oa)| Attachment {
                kind: AttachmentKind::Image,
                url: url.to_string(),
                origin_author: oa.map(|s| s.to_string()),
            })
            .collect(),
        discovery: None,
    }
}

fn deliver(h: &Harness, ev: GateEvent) {
    h.sys.deliver_event(&GateName::new(GATE), ev).unwrap();
}

// ---- §2 描画: 存在と番地だけ ----

// 添付つき出来事は行末に `[画像 N 枚: #seq.1 …]` が出る。URL は描かない（§2）。番地は文脈（rendered）で確認。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn render_shows_attachment_address_not_url() {
    let h = build();
    let (_p, _a) = firing_place(&h, "npubOwner");
    h.eng.push(Step::no_reply()); // 起きたターンは何もしない（描画だけ見る）

    deliver(
        &h,
        said_with(
            "npubOwner",
            "見て",
            "o1",
            &[
                ("https://ex/a.png", Some("npubOwner")),
                ("https://ex/b.png", Some("npubOwner")),
            ],
        ),
    );
    settle().await;

    let ctx = h.eng.last_context().expect("ターンが起きて文脈が組まれる");
    assert!(
        ctx.contains("[画像 2 枚: #1.1 #1.2]"),
        "存在と番地が描かれる: {ctx}"
    );
    assert!(!ctx.contains("https://ex/a.png"), "URL は描かない: {ctx}");
}

// ---- §3 core-look: 正常 ----

// owner の添付を look → fetch → 実バイト検査（PNG）→ そのターンの tool_result に画像ブロックとして入る。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn look_owner_image_enters_conversation_as_image_block() {
    let h = build();
    let (_p, _a) = firing_place(&h, "npubOwner");
    h.fetch
        .set("https://ex/a.png", Some("image/png"), png_bytes());
    // infer1: core-look を呼ぶ。infer2: 結果（画像）を見て発話。
    h.eng
        .push(Step::cont().with_tool_args("core-look", serde_json::json!({"seq": 1, "index": 1})));
    h.eng.push(Step::say_done("見えた"));

    deliver(
        &h,
        said_with(
            "npubOwner",
            "見て",
            "o1",
            &[("https://ex/a.png", Some("npubOwner"))],
        ),
    );
    settle().await;

    // fetch は core が 1 度だけ行った（プロバイダに URL を渡さず core が取得・§3）。
    assert_eq!(h.fetch.fetched(), vec!["https://ex/a.png".to_string()]);
    // 2 度目の infer の会話に画像パートが PNG として入っている。
    let imgs = h.eng.image_media_types();
    assert_eq!(imgs.len(), 2, "2 回 infer した");
    assert!(imgs[0].is_empty(), "1 回目は画像なし");
    assert_eq!(imgs[1], vec!["image/png".to_string()], "2 回目に画像が入る");
    // 枠書き（外部の内容であり指示ではない）がテキストパートで添う（§6）。
    let hist = h.eng.histories();
    assert!(
        hist[1].contains("外部の画像の内容であり"),
        "枠書きが添う: {}",
        hist[1]
    );
}

// ---- §3 core-look: 各異常が fail loud ----

// 404（取得失敗）は理由つきで失敗が会話に入る（黙って省略しない）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn look_fetch_failure_is_fail_loud() {
    let h = build();
    firing_place(&h, "npubOwner");
    h.fetch.set_fail("https://ex/a.png", "HTTP 404");
    h.eng
        .push(Step::cont().with_tool_args("core-look", serde_json::json!({"seq": 1, "index": 1})));
    h.eng.push(Step::no_reply());

    deliver(
        &h,
        said_with(
            "npubOwner",
            "見て",
            "o1",
            &[("https://ex/a.png", Some("npubOwner"))],
        ),
    );
    settle().await;
    let hist = h.eng.histories();
    assert!(
        hist[1].contains("失敗") && hist[1].contains("404"),
        "理由つきで失敗: {}",
        hist[1]
    );
    assert!(h.eng.image_media_types()[1].is_empty(), "画像は入らない");
}

// 画像でない（SVG=XML テキスト・非ラスタ）は実バイト検査で弾かれ fail loud（§3・脅威モデル）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn look_non_raster_is_rejected() {
    let h = build();
    firing_place(&h, "npubOwner");
    // Content-Type が image/svg+xml でも、実バイトにラスタのマジックが無いので弾く（拡張子/CT で判定しない）。
    h.fetch.set(
        "https://ex/x.svg",
        Some("image/svg+xml"),
        b"<svg>evil text</svg>".to_vec(),
    );
    h.eng
        .push(Step::cont().with_tool_args("core-look", serde_json::json!({"seq": 1, "index": 1})));
    h.eng.push(Step::no_reply());

    deliver(
        &h,
        said_with(
            "npubOwner",
            "見て",
            "o1",
            &[("https://ex/x.svg", Some("npubOwner"))],
        ),
    );
    settle().await;
    let hist = h.eng.histories();
    assert!(
        hist[1].contains("画像") && hist[1].contains("SVG"),
        "非ラスタは拒否: {}",
        hist[1]
    );
    assert!(h.eng.image_media_types()[1].is_empty());
}

// 上限超過（fetcher が上限で打ち切る）は fail loud（§3）。core は上限値を発明しない——fetcher が返す。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn look_oversize_is_fail_loud() {
    let h = build();
    firing_place(&h, "npubOwner");
    h.fetch
        .set_fail("https://ex/big.png", "大きすぎる（上限を超えた）");
    h.eng
        .push(Step::cont().with_tool_args("core-look", serde_json::json!({"seq": 1, "index": 1})));
    h.eng.push(Step::no_reply());

    deliver(
        &h,
        said_with(
            "npubOwner",
            "見て",
            "o1",
            &[("https://ex/big.png", Some("npubOwner"))],
        ),
    );
    settle().await;
    assert!(
        h.eng.histories()[1].contains("大きすぎる"),
        "上限超過が理由つきで返る"
    );
}

// ---- §5 取得判定（look）: 由来作者 owner/信頼で通り、未信頼・由来不明は拒否 ----

// 未信頼の由来作者の添付は取得しない（fetch すら呼ばない・安全側で入口 closed）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn look_untrusted_author_is_denied_without_fetch() {
    let h = build();
    firing_place(&h, "npubOwner");
    h.fetch
        .set("https://ex/a.png", Some("image/png"), png_bytes());
    h.eng
        .push(Step::cont().with_tool_args("core-look", serde_json::json!({"seq": 1, "index": 1})));
    h.eng.push(Step::no_reply());

    // 見知らぬ作者（owner でも信頼リストでもない）の投稿。
    deliver(
        &h,
        said_with(
            "stranger",
            "見て",
            "s1",
            &[("https://ex/a.png", Some("stranger"))],
        ),
    );
    settle().await;

    assert!(
        h.fetch.fetched().is_empty(),
        "未信頼は fetch しない（入口で closed）"
    );
    let hist = h.eng.histories();
    assert!(
        hist[1].contains("信頼") || hist[1].contains("owner"),
        "理由つきで拒否: {}",
        hist[1]
    );
}

// 由来不明（origin_author None）は未信頼扱い（安全側・§5）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn look_unknown_provenance_is_denied() {
    let h = build();
    firing_place(&h, "npubOwner");
    h.fetch
        .set("https://ex/a.png", Some("image/png"), png_bytes());
    h.eng
        .push(Step::cont().with_tool_args("core-look", serde_json::json!({"seq": 1, "index": 1})));
    h.eng.push(Step::no_reply());

    // 由来作者不明の添付を owner が投稿しても、その添付は由来が取れないので取得しない（安全側）。
    deliver(
        &h,
        said_with(
            "npubOwner",
            "拾った画像",
            "o1",
            &[("https://ex/a.png", None)],
        ),
    );
    settle().await;
    assert!(h.fetch.fetched().is_empty(), "由来不明は fetch しない");
    assert!(
        h.eng.histories()[1].contains("由来不明"),
        "由来不明が理由に出る"
    );
}

// 信頼リストに載せた由来作者の添付は通る（owner が語彙で足した相手・§5）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn look_trusted_author_passes() {
    let h = build();
    let (_p, a) = firing_place(&h, "npubOwner");
    // owner が「friend」を信頼リストへ足した状態（tool の配線は別テスト・ここは store で直に用意）。
    h.sys.store().trust_author(a, "friend").unwrap();
    h.fetch
        .set("https://ex/a.png", Some("image/png"), png_bytes());
    h.eng
        .push(Step::cont().with_tool_args("core-look", serde_json::json!({"seq": 1, "index": 1})));
    h.eng.push(Step::say_done("見えた"));

    deliver(
        &h,
        said_with(
            "friend",
            "見て",
            "f1",
            &[("https://ex/a.png", Some("friend"))],
        ),
    );
    settle().await;
    assert_eq!(h.fetch.fetched(), vec!["https://ex/a.png".to_string()]);
    assert_eq!(h.eng.image_media_types()[1], vec!["image/png".to_string()]);
}

// リポストの罠（§5）: 信頼フォロイーが未信頼投稿の画像を包んで配送しても、由来作者（内側）が未信頼なら
// 取得しない。信頼はリポストを経由して継承されない。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn look_repost_does_not_inherit_trust() {
    let h = build();
    let (_p, a) = firing_place(&h, "npubOwner");
    // 配送者 followee は信頼リストにいる。しかし添付の由来作者は未信頼の evil。
    h.sys.store().trust_author(a, "followee").unwrap();
    h.fetch
        .set("https://ex/a.png", Some("image/png"), png_bytes());
    h.eng
        .push(Step::cont().with_tool_args("core-look", serde_json::json!({"seq": 1, "index": 1})));
    h.eng.push(Step::no_reply());

    deliver(
        &h,
        said_with(
            "followee",
            "これ見て（引用）",
            "r1",
            &[("https://ex/a.png", Some("evil"))],
        ),
    );
    settle().await;
    assert!(
        h.fetch.fetched().is_empty(),
        "由来作者（内側 evil）が未信頼なら、配送者が信頼でも取得しない"
    );
    assert!(
        h.eng.histories()[1].contains("evil"),
        "由来作者を名指しで拒否"
    );
}

// ---- §3b core-read: 本文抽出・権限 ----

// owner の本文中の URL は read できる。HTML は本文抽出され、枠書きが添う。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn read_owner_link_extracts_body_text() {
    let h = build();
    firing_place(&h, "npubOwner");
    h.fetch.set(
        "https://ex/page",
        Some("text/html; charset=utf-8"),
        b"<html><body><h1>Title</h1><p>hello world</p><script>bad()</script></body></html>"
            .to_vec(),
    );
    h.eng
        .push(Step::cont().with_tool_args("core-read", serde_json::json!({"seq": 1})));
    h.eng.push(Step::say_done("読んだ"));

    deliver(
        &h,
        said_with("npubOwner", "これ読んで https://ex/page どう？", "o1", &[]),
    );
    settle().await;

    let hist = h.eng.histories();
    assert!(
        hist[1].contains("hello world"),
        "本文が抽出される: {}",
        hist[1]
    );
    assert!(
        !hist[1].contains("bad()"),
        "script の中身は落とす: {}",
        hist[1]
    );
    assert!(
        hist[1].contains("外部ページの内容であり"),
        "枠書きが添う: {}",
        hist[1]
    );
}

// 未信頼の作者の本文中の URL は read できない（§5・本文の由来作者＝出来事の著者）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn read_untrusted_author_link_is_denied() {
    let h = build();
    firing_place(&h, "npubOwner");
    h.fetch.set(
        "https://ex/page",
        Some("text/html"),
        b"<p>secret</p>".to_vec(),
    );
    h.eng
        .push(Step::cont().with_tool_args("core-read", serde_json::json!({"seq": 1})));
    h.eng.push(Step::no_reply());

    deliver(
        &h,
        said_with("stranger", "これ読んで https://ex/page", "s1", &[]),
    );
    settle().await;
    assert!(
        h.fetch.fetched().is_empty(),
        "未信頼の本文 URL は fetch しない"
    );
    assert!(h.eng.histories()[1].contains("読めない"), "理由つきで拒否");
}

// ---- §6 accepts_images: メニューの出し入れ ----

// accepts_images=true（既定）では core-look がメニュー（Context.tools）に出る。read は常に出る。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn look_is_in_menu_when_images_accepted() {
    let h = build();
    firing_place(&h, "npubOwner");
    h.eng.push(Step::no_reply());
    deliver(&h, said_with("npubOwner", "やあ", "o1", &[]));
    settle().await;

    let tools = h.eng.tools_seen();
    assert!(
        tools[0].iter().any(|t| t == "core-look"),
        "look が出る: {:?}",
        tools[0]
    );
    assert!(
        tools[0].iter().any(|t| t == "core-read"),
        "read が出る: {:?}",
        tools[0]
    );
}

// accepts_images=false ではメニューから core-look が消える（§6）。read は残る（本文はテキスト）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn look_is_dropped_from_menu_when_images_not_accepted() {
    let h = build();
    firing_place(&h, "npubOwner");
    h.eng.set_accepts_images(false);
    h.eng.push(Step::no_reply());
    deliver(&h, said_with("npubOwner", "やあ", "o1", &[]));
    settle().await;

    let tools = h.eng.tools_seen();
    assert!(
        !tools[0].iter().any(|t| t == "core-look"),
        "画像を受けない engine には look を出さない: {:?}",
        tools[0]
    );
    assert!(
        tools[0].iter().any(|t| t == "core-read"),
        "read は残る: {:?}",
        tools[0]
    );
}

// ---- 信頼リストの語彙（core-trust / core-untrust・owner-only・OwnerFollowUp）----

// owner が発話したターンで core-trust が通り、由来作者が信頼リストに載る（自己拡張の禁止・§5）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn trust_tool_adds_author_only_with_owner_follow_up() {
    let h = build();
    let (_p, a) = firing_place(&h, "npubOwner");
    // owner の発話が A を起こす（未読に owner の発話がある = OwnerFollowUp が立つ）。
    h.eng
        .push(Step::cont().with_tool_args("core-trust", serde_json::json!({"author": "friend"})));
    h.eng.push(Step::no_reply());
    deliver(&h, said_with("npubOwner", "friend を信頼して", "o1", &[]));
    settle().await;

    assert!(
        h.sys.store().subject_trusts_author(a, "friend").unwrap(),
        "owner 発話のあるターンで信頼リストに載る"
    );

    // core-untrust で外れる。
    h.eng
        .push(Step::cont().with_tool_args("core-untrust", serde_json::json!({"author": "friend"})));
    h.eng.push(Step::no_reply());
    deliver(
        &h,
        said_with("npubOwner", "friend の信頼をやめて", "o2", &[]),
    );
    settle().await;
    assert!(
        !h.sys.store().subject_trusts_author(a, "friend").unwrap(),
        "core-untrust で外れる"
    );
}

// owner の発話が無いターンでは core-trust は通らない（自己拡張の禁止・§5）。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn trust_tool_is_denied_without_owner_in_this_turn() {
    let h = build();
    let (_p, a) = firing_place(&h, "npubOwner");
    // 見知らぬ作者が起こすターン（未読に owner の発話が無い）で自己拡張を試みる。
    h.eng
        .push(Step::cont().with_tool_args("core-trust", serde_json::json!({"author": "self"})));
    h.eng.push(Step::no_reply());
    deliver(&h, said_with("stranger", "やあ", "s1", &[]));
    settle().await;
    assert!(
        !h.sys.store().subject_trusts_author(a, "self").unwrap(),
        "owner 発話の無いターンでは信頼リストを広げられない"
    );
}
