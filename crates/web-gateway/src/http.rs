//! web gateway の HTTP 境界（#154 第1スライス / 移設は #190 S4）。
//!
//! - `POST /api/agents/{id}/web/send` — ダッシュボードからの inbound。
//! - `GET  /api/agents/{id}/web/stream?conversation={cid}` — SSE 購読。
//!
//! ルータは [`routes`] が返し、上位（`crates/server` の `create_router`）は
//! `.merge()` で取り付けるだけ。ハンドラは状態型を [`WebAgentRunner`] で受けるので、
//! `crates/server` の `AppState` に型として依存しない。
//!
//! ここが持つのは HTTP の抽出とレスポンス整形だけで、session_id 規約・直列化・
//! SSE 配送・非ブロック dispatch の実体は [`crate::gateway`] / [`crate::respond`] にある。
//! 認可判定・セッション用意・DB 記録は [`WebAgentRunner`] 越しに呼ぶ。
//!
//! **#177 の保証**: 応答生成の本体（`crate::respond` の `run_and_deliver`）は兄弟
//! モジュールの private 項目なのでここからは到達できない。ハンドラが呼べるのは
//! 直列化込みの [`run_and_deliver_serialized`] だけである。

use std::convert::Infallible;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
    Json, Router,
};
use futures::Stream;
use serde::Deserialize;

use crate::gateway::{caller_type_label, web_session_id};
use crate::respond::{run_and_deliver_serialized, WebTurnOutcome};
use crate::runner::WebAgentRunner;

/// Production router と read-only inventory が共有するルート宣言。
///
/// method と handler を同じ行に置くことで、外形採取器が別の一覧を再構築できない形にする。
macro_rules! web_routes {
    ($apply:ident, $target:ident, $runner:ty) => {
        $apply!(
            $target,
            "/api/agents/{id}/web/send",
            post => send_web_message::<$runner>
        );
        $apply!(
            $target,
            "/api/agents/{id}/web/stream",
            get => web_stream::<$runner>
        );
    };
}

macro_rules! mount_route {
    ($router:ident, $path:literal, $first_method:ident => $first_handler:expr $(, $method:ident => $handler:expr)*) => {
        $router = $router.route(
            $path,
            axum::routing::$first_method($first_handler)$(.$method($handler))*,
        );
    };
}

macro_rules! describe_route {
    ($routes:ident, $path:literal, $($method:ident => $handler:expr),+ $(,)?) => {
        $routes.extend(std::iter::once(($path, vec![$(stringify!($method).to_ascii_uppercase()),+])));
    };
}

/// web gateway の HTTP ルート（`POST .../web/send` と `GET .../web/stream`）。
///
/// 状態型 `R` は上位のアプリ状態そのもの（`Router<R>` として返す）。上位は
/// `with_state` の前に `.merge(routes())` するだけでよい。パスは移設前と同一。
pub fn routes<R: WebAgentRunner>() -> Router<R> {
    let mut router = Router::new();
    web_routes!(mount_route, router, R);
    router
}

/// [`routes`] が実際に mount する path/method の read-only inventory。
pub fn route_inventory<R: WebAgentRunner>() -> Vec<(&'static str, Vec<String>)> {
    let mut routes = Vec::new();
    web_routes!(describe_route, routes, R);
    routes
}

#[derive(Debug, Deserialize)]
pub struct SendWebMessageRequest {
    pub conversation_id: String,
    pub content: String,
    /// 認可判定用のユーザ ID（省略時は匿名 web ユーザ）。
    #[serde(default)]
    pub user_id: Option<String>,
}

/// 認可判定に使う既定のユーザ ID（`user_id` 省略時・空文字時）。
///
/// **公開契約ではない**: `pub` なのは `crates/server` 側のテスト（正規化と owner 判定の
/// 噛み合わせ）から参照するためだけで、クレート外の利用者向けの API ではない。
/// 値そのものは HTTP レスポンス／`session_logs` の `speaker_id` に現れるので、
/// 変更するときは server 側の owner 判定テストも合わせて見ること。
#[doc(hidden)]
pub const DEFAULT_WEB_USER_ID: &str = "web-user";

/// リクエストの `user_id` を認可判定に使える形へ正規化する。
///
/// `Option<String>` なので `None` は既定値に落ちるが、JSON で `""` を明示すると
/// `Some("")` となり既定値が適用されない。空文字は認可上の主体として意味を持たない
/// （session_logs の speaker_id にもそのまま入る）ため、空白のみの場合も含めて
/// 既定の web ユーザとして扱う。
///
/// owner 判定そのものは [`WebAgentRunner::resolve_caller`] の実装側（`crates/server`
/// の `is_owner_id` / #164）に委ねる。前後の空白はここで落としておき、認可・
/// セッションキー・speaker_id すべてで同じ値を使う。正規化とその owner 判定の
/// 噛み合わせは server 側にテストがある。
///
/// **公開契約ではない**: [`DEFAULT_WEB_USER_ID`] と同じく、`pub` なのは server 側の
/// 境界テストから呼ぶためだけである（正規化はゲートウェイ、owner 判定は server に
/// あるため、両方を import できないと噛み合わせを検査できない）。ハンドラ経路で
/// 実際に正規化が効いていることは下の `handler_normalizes_the_user_id_*` が見る。
#[doc(hidden)]
pub fn normalize_user_id(user_id: Option<&str>) -> String {
    match user_id.map(str::trim) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => DEFAULT_WEB_USER_ID.to_string(),
    }
}

/// POST /api/agents/{id}/web/send — inbound メッセージを受けてエージェントを実行する。
///
/// [`run_and_deliver_serialized`]（直列化込みの唯一の公開入口）を実行し、直接応答を body で
/// 返しつつ SSE へも push する。subtask 完了 resume の応答は SSE のみで配送される。
pub async fn send_web_message<R: WebAgentRunner>(
    State(state): State<R>,
    Path(id): Path<String>,
    Json(req): Json<SendWebMessageRequest>,
) -> Response {
    let user_id = normalize_user_id(req.user_id.as_deref());
    let session_id = web_session_id(&id, &req.conversation_id);

    // 存在しないエージェントの弾き出し（#632）は `run_and_deliver_serialized`（web の
    // 唯一の公開ターン入口）が担う。ここでは `AgentNotFound` を 404 に写像する（下の match）。

    // 1. 認可: trusted_users から caller を導出する。
    let caller = state.resolve_caller(&id, &user_id);
    let caller_type = caller_type_label(&caller);

    // 2. セッションを用意する（無ければ作成）。
    if let Err(e) = state.ensure_web_session(&session_id, &id) {
        return Json(serde_json::json!({"error": format!("Failed to create session: {e}")}))
            .into_response();
    }

    // 3. ユーザ発話を DB へ記録する（応答生成は DB から会話を再構築する）。
    if let Err(e) = state.record_user_message(&id, &session_id, &user_id, &req.content) {
        return Json(serde_json::json!({"error": format!("Failed to log message: {e}")}))
            .into_response();
    }

    // 4. LLM プロバイダの可用性チェック。
    if !state.has_llm_providers() {
        return Json(serde_json::json!({
            "session_id": session_id,
            "caller_type": caller_type,
            "response": null,
            "error": "No LLM providers available",
        }))
        .into_response();
    }

    // 5. 実行して直接応答を返す（SSE へも push 済み）。per-session 直列化は
    //    `run_and_deliver_serialized` の内側に閉じている（呼び忘れが起こらない）。
    //    ランタイム（SSE チャンネル + ロック）は runner から引かれるため、inbound と
    //    完了 sink が別のランタイムを掴む余地は無い。存在確認もこの入口で 1 度だけ行われる。
    match run_and_deliver_serialized(&state, &id, &session_id, caller, None, "direct").await {
        WebTurnOutcome::AgentNotFound => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("agent not found: {id}")})),
        )
            .into_response(),
        // 存在確認の DB エラーは 404 ではなく 500（実在するエージェントを 404 に化けさせない）。
        WebTurnOutcome::Error(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
        WebTurnOutcome::Ran(response) => Json(serde_json::json!({
            "session_id": session_id,
            "caller_type": caller_type,
            "response": response,
        }))
        .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    pub conversation: String,
}

/// GET /api/agents/{id}/web/stream?conversation={cid} — SSE でエージェント発話を購読する。
///
/// inbound への直接応答と subtask 完了 resume の応答の双方が push される。
/// 未接続時に発話が失われないよう、応答は DB（session_logs）にも保存されている。
///
/// ## 取りこぼし時の契約（テストで固定済み）
///
/// broadcast のバックログ（`SSE_CHANNEL_CAPACITY`）を超えて遅れた購読者は
/// `Lagged` を受ける。このとき**ストリームは切らずに継続する**（落ちた発話は
/// `session_logs` から辿れるので、接続だけは維持するほうがダッシュボードの
/// 挙動として望ましい）。送信側が全て drop された `Closed` のときだけ終了する。
pub async fn web_stream<R: WebAgentRunner>(
    State(state): State<R>,
    Path(id): Path<String>,
    Query(q): Query<StreamQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let session_id = web_session_id(&id, &q.conversation);
    let rx = state.web_gateway().subscribe(&session_id);

    // broadcast::Receiver を SSE ストリームへ変換する（tokio-stream 依存を避け futures で）。
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(payload) => {
                    return Some((Ok(Event::default().data(payload)), rx));
                }
                // 遅い購読者でバックログが溢れた: 落ちた分は DB 側で辿れる。継続する。
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                // 送信側が全て drop された: ストリーム終了。
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use opencrab_actions::CallerIdentity;
    use tower::ServiceExt; // oneshot

    use crate::gateway::{web_session_id, WebEvent, SSE_CHANNEL_CAPACITY};
    use crate::testing::FakeRunner;

    /// [`routes`] に実リクエストを 1 本流して `(status, body)` を返す。
    ///
    /// body を読み切るので**終端するレスポンス専用**（SSE には `call_status` か
    /// `oneshot` を直接使う）。
    async fn call(runner: &FakeRunner, req: Request<Body>) -> (StatusCode, String) {
        let app = routes::<FakeRunner>().with_state(runner.clone());
        let res = app
            .oneshot(req)
            .await
            .expect("router がリクエストを落とした");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .expect("body を読めない");
        (
            status,
            String::from_utf8(bytes.to_vec()).expect("body が UTF-8 でない"),
        )
    }

    /// ステータス行だけを見る（body を読まないので SSE でも安全）。
    async fn call_status(runner: &FakeRunner, req: Request<Body>) -> StatusCode {
        let app = routes::<FakeRunner>().with_state(runner.clone());
        app.oneshot(req)
            .await
            .expect("router がリクエストを落とした")
            .status()
    }

    fn send_request(agent_id: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!("/api/agents/{agent_id}/web/send"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("リクエストを組めない")
    }

    fn get_request(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("リクエストを組めない")
    }

    fn inbound(content: &str) -> serde_json::Value {
        serde_json::json!({"conversation_id": "c1", "content": content})
    }

    #[test]
    fn empty_user_id_falls_back_to_default() {
        assert_eq!(normalize_user_id(None), DEFAULT_WEB_USER_ID);
        assert_eq!(normalize_user_id(Some("")), DEFAULT_WEB_USER_ID);
        assert_eq!(normalize_user_id(Some("   ")), DEFAULT_WEB_USER_ID);
    }

    #[test]
    fn non_empty_user_id_is_preserved() {
        assert_eq!(normalize_user_id(Some("alice")), "alice");
        assert_eq!(normalize_user_id(Some("  alice  ")), "alice");
        assert_eq!(
            normalize_user_id(Some("123456789012345678")),
            "123456789012345678"
        );
    }

    /// **ルータのパスが移設前と同一であること**。
    ///
    /// 以前は `routes::<FakeRunner>()` を構築するだけで、パス文字列を変えても落ちない
    /// テストだった（レビューの変異実験で「購読側のパスだけ変更」がすり抜けた）。
    /// 実リクエストを流して「404 にならない」ことを見るので、どちらのパスを変えても落ちる。
    #[tokio::test]
    async fn routes_expose_the_documented_paths() {
        let runner = FakeRunner::new("ok");

        // 1. 送信: POST /api/agents/{id}/web/send
        let status = call_status(&runner, send_request("a", inbound("hi"))).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "POST /api/agents/{{id}}/web/send が期待どおりに載っていない"
        );

        // 2. 購読: GET /api/agents/{id}/web/stream?conversation={cid}
        //    body（SSE）は終端しないので読まない。
        let status = call_status(
            &runner,
            get_request("/api/agents/a/web/stream?conversation=c1"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "GET /api/agents/{{id}}/web/stream が期待どおりに載っていない"
        );

        // 3. クエリ名 `conversation` も契約（ダッシュボードが組み立てる URL）。
        //    別名にすると extractor が 400 を返す = 404 とは区別できる。
        let status = call_status(&runner, get_request("/api/agents/a/web/stream")).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "`conversation` は必須クエリ（名前が変わると 400 になる）"
        );
    }

    /// 上の「404 でないこと」に意味を持たせるため、載っていないパスが 404 になることを見る。
    #[tokio::test]
    async fn unrouted_paths_are_not_captured() {
        let runner = FakeRunner::new("ok");
        for uri in [
            "/api/agents/a/web/sends",
            "/api/agents/a/web/streams",
            "/api/agents/a/web",
        ] {
            assert_eq!(
                call_status(&runner, get_request(uri)).await,
                StatusCode::NOT_FOUND,
                "{uri} は routes() の担当外"
            );
        }
    }

    /// レスポンスの `caller_type` は [`WebAgentRunner::resolve_caller`] の返り値由来であること。
    ///
    /// 呼び出し元判定を捨てて固定値（例: 常に owner）を返す変異はここで落ちる。
    #[tokio::test]
    async fn send_reports_the_caller_type_resolved_by_the_runner() {
        let cases = [
            (CallerIdentity::Owner, "owner"),
            (CallerIdentity::TrustedUser, "trusted_user"),
            (CallerIdentity::Agent, "agent"),
            (
                CallerIdentity::CoAgent {
                    agent_id: "peer".to_string(),
                },
                "co_agent",
            ),
        ];
        for (caller, expected) in cases {
            let runner = FakeRunner::new("やあ").with_caller(caller.clone());
            let (status, body) = call(&runner, send_request("a", inbound("hi"))).await;
            assert_eq!(status, StatusCode::OK);
            let v: serde_json::Value = serde_json::from_str(&body).expect("JSON でない");
            assert_eq!(
                v["caller_type"], expected,
                "caller_type が resolve_caller({caller:?}) の結果に由来していない"
            );
            assert_eq!(v["session_id"], web_session_id("a", "c1"));
            assert_eq!(v["response"], "やあ");
        }
    }

    /// **ユーザ ID の正規化がハンドラ経路で効いていること**。
    ///
    /// 正規化関数の単体テストだけでは、ハンドラが正規化を通さなくなっても検出できない。
    /// 認可判定（`resolve_caller`）と DB 記録（`record_user_message`）の両方へ
    /// 同じ正規化済みの値が渡ることを観測する。
    #[tokio::test]
    async fn handler_normalizes_the_user_id_before_authorization_and_recording() {
        let cases = [
            (serde_json::json!("  alice  "), "alice"),
            (serde_json::json!("alice"), "alice"),
            (serde_json::json!(""), DEFAULT_WEB_USER_ID),
            (serde_json::json!("   "), DEFAULT_WEB_USER_ID),
            (serde_json::Value::Null, DEFAULT_WEB_USER_ID),
        ];
        for (sent, expected) in cases {
            let runner = FakeRunner::new("ok");
            let (status, _) = call(
                &runner,
                send_request(
                    "a",
                    serde_json::json!({
                        "conversation_id": "c1",
                        "content": "hi",
                        "user_id": sent,
                    }),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK);

            let lookups = runner.caller_lookups();
            assert_eq!(lookups.len(), 1, "認可判定が 1 回呼ばれる");
            assert_eq!(
                lookups[0].user_id, expected,
                "認可判定へ正規化されていない user_id ({sent}) が渡っている"
            );
            assert_eq!(lookups[0].agent_id, "a");

            let messages = runner.user_messages();
            assert_eq!(messages.len(), 1, "ユーザ発話が 1 件記録される");
            assert_eq!(
                messages[0].user_id, expected,
                "session_logs へ正規化されていない user_id ({sent}) が渡っている"
            );
            assert_eq!(messages[0].agent_id, "a");
            assert_eq!(messages[0].session_id, web_session_id("a", "c1"));
            assert_eq!(messages[0].content, "hi");
        }
    }

    /// **存在しないエージェントはターンを起こさず 404 で弾く**（#632）。
    ///
    /// 存在確認は web の唯一の公開ターン入口 `run_and_deliver_serialized` が担い、ハンドラは
    /// その `AgentNotFound` を 404 に写像するだけ。**ターン（`run_agent_response`）は走らない**
    /// ことを固定する（応答の転記も配送もされない）。
    #[tokio::test]
    async fn unknown_agent_is_rejected_with_404_without_running() {
        let runner = FakeRunner::new("走ってはいけない").without_agent();
        let (status, body) = call(&runner, send_request("ghost", inbound("hi"))).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "存在しないエージェントは 404 で弾く"
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("JSON でない");
        assert_eq!(v["error"], "agent not found: ghost");

        // ターンが走らない（チョークポイントが run の手前で弾く）。
        assert!(
            runner.runs().is_empty(),
            "存在しないエージェントで実行された"
        );
        // 応答の転記もされない。
        assert!(
            runner.replies().is_empty(),
            "存在しないエージェントで応答が転記された"
        );
    }

    /// **存在するエージェントは従来どおり 200 でターンが走る**（#632 の回帰防止）。
    ///
    /// 上の 404 ガードが正常系まで巻き込んでいないことを、同じ形で対にして固定する。
    #[tokio::test]
    async fn existing_agent_runs_and_returns_200() {
        let runner = FakeRunner::new("やあ");
        let (status, body) = call(&runner, send_request("real", inbound("hi"))).await;
        assert_eq!(status, StatusCode::OK, "存在するエージェントは 200 で走る");
        let v: serde_json::Value = serde_json::from_str(&body).expect("JSON でない");
        assert_eq!(v["response"], "やあ");
        assert_eq!(v["session_id"], web_session_id("real", "c1"));
        assert_eq!(runner.runs().len(), 1, "ターンが 1 回走る");
        assert_eq!(runner.user_messages().len(), 1, "ユーザ発話が記録される");
    }

    /// **存在確認の DB エラーは 404 ではなく 500**（#632 レビュー指摘）。
    ///
    /// 一過性の DB エラーで実在するエージェントを 404 に化けさせない。ターンも走らない。
    #[tokio::test]
    async fn db_error_during_existence_check_is_500_not_404() {
        let runner = FakeRunner::new("走ってはいけない").failing_agent_exists("db is down");
        let (status, body) = call(&runner, send_request("real", inbound("hi"))).await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "DB エラーは 500（404 ではない）"
        );
        assert_ne!(status, StatusCode::NOT_FOUND);
        let v: serde_json::Value = serde_json::from_str(&body).expect("JSON でない");
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("db is down"),
            "エラー本文に原因が出る: {body}"
        );
        assert!(
            runner.runs().is_empty(),
            "DB エラー時にターンが走ってはいけない"
        );
    }

    /// LLM プロバイダ未設定のときのレスポンス形（docs/api.md の契約）。
    #[tokio::test]
    async fn send_without_llm_provider_reports_the_error_without_running() {
        let runner = FakeRunner::new("ok")
            .with_caller(CallerIdentity::TrustedUser)
            .without_llm_provider();
        let (status, body) = call(&runner, send_request("a", inbound("hi"))).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).expect("JSON でない");
        assert_eq!(v["error"], "No LLM providers available");
        assert!(v["response"].is_null());
        assert_eq!(v["caller_type"], "trusted_user");
        assert_eq!(v["session_id"], web_session_id("a", "c1"));
        assert!(runner.runs().is_empty(), "実行してはいけない");
        // ユーザ発話は記録済み（プロバイダ設定後に履歴として残る）。
        assert_eq!(runner.user_messages().len(), 1);
    }

    /// セッション用意・発話記録の失敗は実行せずにエラー本文を返す（現状の契約）。
    ///
    /// この 2 分岐だけ `session_id` / `caller_type` を返さない点も含めて固定する
    /// （ダッシュボードはエラー時にこれらを読めない前提で書く必要がある）。
    #[tokio::test]
    async fn send_reports_persistence_failures_without_running() {
        let cases = [
            (
                FakeRunner::new("ok").failing_ensure_session("disk full"),
                "Failed to create session: disk full",
            ),
            (
                FakeRunner::new("ok").failing_record_user_message("locked"),
                "Failed to log message: locked",
            ),
        ];
        for (runner, expected) in cases {
            let (status, body) = call(&runner, send_request("a", inbound("hi"))).await;
            assert_eq!(status, StatusCode::OK);
            let v: serde_json::Value = serde_json::from_str(&body).expect("JSON でない");
            assert_eq!(v["error"], expected);
            assert!(
                v["session_id"].is_null() && v["caller_type"].is_null(),
                "この分岐は session_id / caller_type を返さない: {body}"
            );
            assert!(runner.runs().is_empty(), "実行してはいけない");
        }
    }

    /// **取りこぼし（`Lagged`）ではストリームを切らない**。
    ///
    /// バックログ超過で落ちた発話は `session_logs` から辿れる。接続を切ると
    /// ダッシュボードが再購読するまで live push が止まるので、継続が契約。
    #[tokio::test]
    async fn sse_stream_continues_after_the_subscriber_lags() {
        let runner = FakeRunner::new("ok");
        let sid = web_session_id("a", "lag");

        let app = routes::<FakeRunner>().with_state(runner.clone());
        let res = app
            .oneshot(get_request("/api/agents/a/web/stream?conversation=lag"))
            .await
            .expect("router がリクエストを落とした");
        assert_eq!(res.status(), StatusCode::OK);
        let mut body = res.into_body();

        // 購読者は 1 件も読んでいない状態で capacity 超過まで publish する
        // （= 次の recv が `Lagged` になる）。
        for i in 0..(SSE_CHANNEL_CAPACITY + 50) {
            runner.web_gateway().publish(
                &sid,
                &WebEvent {
                    kind: "direct".to_string(),
                    agent_id: "a".to_string(),
                    content: format!("m{i}"),
                },
            );
        }

        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .expect("取りこぼし後に何も届かない（ストリームが止まった）")
            .expect("取りこぼしでストリームが終了した（`Lagged` で切ってはいけない）")
            .expect("body フレームの読み出しに失敗");
        let data = frame.into_data().expect("data フレームでない");
        let text = String::from_utf8(data.to_vec()).expect("UTF-8 でない");
        assert!(
            text.starts_with("data: "),
            "SSE の data フレームでない: {text:?}"
        );
        // 溢れた分は落ちるが、生き残った発話は届く。
        assert!(
            text.contains("\"kind\":\"direct\""),
            "中身が壊れている: {text:?}"
        );
    }

    /// 送信側が全て drop された（`Closed`）ときはストリームを終了する。
    #[tokio::test]
    async fn sse_stream_ends_when_the_channel_closes() {
        let runner = FakeRunner::new("ok");
        let app = routes::<FakeRunner>().with_state(runner.clone());
        let res = app
            .oneshot(get_request("/api/agents/a/web/stream?conversation=closing"))
            .await
            .expect("router がリクエストを落とした");
        assert_eq!(res.status(), StatusCode::OK);
        let mut body = res.into_body();

        // 最後の所有者を落とす → WebGateway ごと broadcast::Sender が drop される。
        drop(runner);

        let next = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .expect("Closed でもストリームが終わらない");
        assert!(
            next.is_none(),
            "送信側が全滅したらストリームは終了する（keep-alive で居座らない）"
        );
    }
}
