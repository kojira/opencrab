//! 外部イベント受信（webhook intake / issue #454）。
//!
//! 3 つの関心をまとめる:
//!
//! 1. **署名検証** [`verify_signature`] — source ごとの共有 secret で HMAC-SHA256 を定数時間照合。
//! 2. **ルーティング + 投入** [`route_and_enqueue`] — (source, event_type) を config のルートで
//!    agent_id へ写し、dedup 付きで `agent_inbox` に積む。
//! 3. **catch-up ポーリング** [`spawn_intake_catchup_loop`] — webhook は at-most-once なので、
//!    起動時 + 定期に source 側の一覧 API から未処理分を補充する（[`SourceAdapter`]）。
//!
//! 受信箱の**消化**（LLM ターン）はここには無い。heartbeat ループの有無に依存しないよう、
//! バイナリ側の専用ループ（`intake_process`）が担う。
//!
//! # 秘密の扱い
//! source secret と Bearer トークンはここと `AppState.intake` に閉じ込め、ログ・エラー・
//! API 応答へ出さない。[`verify_signature`] は真偽だけを返す（詳細を返すと oracle になる）。

pub mod rest_list;

use anyhow::Result;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use opencrab_db::queries::{enqueue_inbox_event, InboxInsert};

use crate::AppState;

type HmacSha256 = Hmac<Sha256>;

/// 受信箱へ積む 1 イベント（source は投入時に付ける）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeEvent {
    pub event_type: String,
    /// source 内で一意な重複排除キー。webhook と catch-up で同じ規則を使うこと。
    pub dedup_key: String,
    pub payload_json: String,
}

/// HMAC-SHA256 署名を**定数時間**で検証する。
///
/// `provided` は `sha256=<hex>` 形式（プレフィックス無しの生 hex も許容）。secret はこの関数に
/// 閉じ込め、戻り値は真偽のみ。照合は [`Mac::verify_slice`]（内部で定数時間比較）に委ね、
/// 自前のバイト比較（早期 return でタイミングが漏れる）を書かない。
pub fn verify_signature(secret: &str, raw_body: &[u8], provided: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    let hex_part = provided.strip_prefix("sha256=").unwrap_or(provided).trim();
    let Some(sig) = hex_decode(hex_part) else {
        return false;
    };
    // secret 長は任意。HMAC の鍵長制限は無い（new_from_slice は Infallible 相当）。
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(raw_body);
    mac.verify_slice(&sig).is_ok()
}

/// 16 進文字列をバイト列へ。奇数長・非 hex は `None`。
///
/// ここは秘密に依存しない（提示された署名値のデコード）ので定数時間である必要はない。
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    // chunks_exact(2) は末尾の余りを黙って落とすので、奇数長・空は先に弾く。
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return None;
    }
    bytes
        .chunks_exact(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some((hi * 16 + lo) as u8)
        })
        .collect()
}

/// JSON のスカラ id（文字列 or 整数）を文字列にする。オブジェクト/配列/null/bool は `None`。
pub fn json_scalar_id(v: Option<&serde_json::Value>) -> Option<String> {
    match v {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// webhook body の `data` から重複排除キーを導く。
///
/// `data.id`（文字列/数値）があれば `"{event_type}:{id}"`。無ければ raw body の SHA-256 を
/// `"{event_type}:sha256:{hex}"` として使う（**同一配送の二重**だけ防ぐ。id が無い source は
/// catch-up との相互 dedup ができないので、その場合は webhook 単独の再送のみ弾ける）。
///
/// catch-up 側（[`SourceAdapter`] 実装）も id ありのイベントは同じ `"{event_type}:{id}"` を
/// 作るので、同一イベントは webhook↔catch-up で一致する。
pub fn webhook_dedup_key(event_type: &str, data: &serde_json::Value, raw_body: &[u8]) -> String {
    if let Some(id) = json_scalar_id(data.get("id")) {
        return format!("{event_type}:{id}");
    }
    let mut hasher = Sha256::new();
    hasher.update(raw_body);
    let hex = hex_encode(&hasher.finalize());
    format!("{event_type}:sha256:{hex}")
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 投入結果。webhook / catch-up の両方が使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// 新規に積んだ。
    Enqueued,
    /// dedup で既存（積まなかった）。
    Duplicate,
    /// (source, event_type) に対応するルートが無く、積まなかった（受理はしてよい）。
    NoRoute,
}

/// (source, event_type) をルーティングして受信箱へ積む。
///
/// ルートが無ければ [`EnqueueOutcome::NoRoute`]（webhook は 202 を返してよいが、消化されない）。
/// agent_id は config のルート値（名前 or UUID）をそのまま保存する。名前→UUID の解決は
/// 消化ループ側で tick ごとに行う（heartbeat と同じ「発火時に解決」の流儀）。
pub fn route_and_enqueue(
    state: &AppState,
    source: &str,
    event: &IntakeEvent,
) -> Result<EnqueueOutcome> {
    let Some(agent_id) = state.intake.route_agent(source, &event.event_type) else {
        return Ok(EnqueueOutcome::NoRoute);
    };
    let insert = InboxInsert {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        source: source.to_string(),
        event_type: event.event_type.clone(),
        dedup_key: event.dedup_key.clone(),
        payload_json: event.payload_json.clone(),
    };
    let conn = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("db lock poisoned: {e}"))?;
    let inserted = enqueue_inbox_event(&conn, &insert)?;
    Ok(if inserted {
        EnqueueOutcome::Enqueued
    } else {
        EnqueueOutcome::Duplicate
    })
}

/// source 側の一覧 API から未処理分を取り出す抽象。汎用実装は [`rest_list::RestListAdapter`]
/// （`kind = "rest_list"`）。第一号 omoikane も config の値としてこの型で構成する。
#[async_trait::async_trait]
pub trait SourceAdapter: Send + Sync {
    /// source 名（`/api/hooks/{source}` / config のキーと一致）。
    fn source(&self) -> &str;
    /// 直近のイベントを取得する。dedup_key は webhook と同じ規則で払い出すこと。
    async fn poll_recent(&self) -> Result<Vec<IntakeEvent>>;
}

/// 設定の `[[intake.sources]]` から有効な source アダプタを組み立てる。**source 名は型に
/// 焼き付かない**——`kind` で種別を選ぶだけなので、REST 一覧の source は設定を足すだけで増える
/// （コード変更不要 / issue #470）。無効（`enabled=false` / base_url 空等）な source は畳まれて
/// 出てこない。
pub fn build_adapters(cfg: &crate::config::IntakeConfig) -> Vec<Box<dyn SourceAdapter>> {
    use crate::config::IntakeSourceKind;
    let mut adapters: Vec<Box<dyn SourceAdapter>> = Vec::new();
    for src in &cfg.sources {
        match src.kind {
            IntakeSourceKind::RestList => {
                if let Some(a) = rest_list::RestListAdapter::from_config(src) {
                    adapters.push(Box::new(a));
                }
            }
        }
    }
    adapters
}

/// catch-up する source のうち、対応する route（配送先）が config に 1 件も無いものを返す
/// （#470-N1・起動時警告用）。これらは取得しても NoRoute で捨てられる（silent 障害の芽）。
/// `active_sources` は実際に catch-up する（＝アダプタ化された）source 名なので、`enabled=false`
/// で畳まれた source や無効な source は対象外（そもそもポーリングせず捨ても起きない）。
fn sources_missing_routes<'a>(
    routes: &[crate::config::IntakeRoute],
    active_sources: &'a [String],
) -> Vec<&'a str> {
    active_sources
        .iter()
        .filter(|src| !routes.iter().any(|r| &r.source == *src))
        .map(|s| s.as_str())
        .collect()
}

/// catch-up ポーリングループを起動する（起動時 + 定期）。
///
/// webhook は at-most-once なので、停止中に落ちたイベントはここで補充する（受け入れ基準:
/// 「停止中に発生したイベントが再起動後の catch-up で処理される」）。**LLM は呼ばない**
/// （受信箱に積むだけ）。アダプタが 1 つも無ければ即 return する。
pub fn spawn_intake_catchup_loop(state: AppState) {
    let adapters = build_adapters(&state.intake);
    if adapters.is_empty() {
        tracing::info!("intake catch-up: source アダプタ未設定のため起動しない");
        return;
    }
    // 最低 60 秒に丸める（設定値はそのまま保持し、ここで床を効かせる / 既存ループと同流儀）。
    let interval_secs = state.intake.catch_up_interval_secs.max(60);
    let sources: Vec<String> = adapters.iter().map(|a| a.source().to_string()).collect();
    // #470-N1: catch-up する source に対応する route（配送先）が 1 件も無いと、取得には成功する
    // がイベントは NoRoute で捨てられ debug ログにしか出ない。config だけで source を足せるように
    // した結果 `name` のタイポ（routes.source と不一致）が最も起きやすい運用ミスになり、しかも
    // silent（外部 API を叩いて取得までは成功するので「設定したのに何も起きない」）。**起動時に
    // 気づける手段**として warn を出す（error にしない・起動を止めない。route を後から足す運用は
    // 正当なので、これは制約ではなく情報）。
    for missing in sources_missing_routes(&state.intake.routes, &sources) {
        tracing::warn!(
            source = %missing,
            "intake catch-up: この source に対応する [[intake.routes]] が無い。取得したイベントは配送先が無く捨てられる（routes.source を name と一致させること）"
        );
    }
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(interval_secs);
        tracing::info!(
            interval_secs,
            sources = ?sources,
            "intake catch-up loop started"
        );
        loop {
            for adapter in &adapters {
                run_catchup_once(&state, adapter.as_ref()).await;
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// 1 アダプタを 1 回ポーリングして受信箱へ補充する。失敗は握って次周期へ（ループを殺さない）。
async fn run_catchup_once(state: &AppState, adapter: &dyn SourceAdapter) {
    let source = adapter.source();
    match adapter.poll_recent().await {
        Ok(events) => {
            let mut enqueued = 0usize;
            let mut duplicate = 0usize;
            let mut no_route = 0usize;
            for ev in &events {
                match route_and_enqueue(state, source, ev) {
                    Ok(EnqueueOutcome::Enqueued) => enqueued += 1,
                    Ok(EnqueueOutcome::Duplicate) => duplicate += 1,
                    Ok(EnqueueOutcome::NoRoute) => no_route += 1,
                    Err(e) => {
                        tracing::warn!(source, error = %e, "intake catch-up: enqueue 失敗");
                    }
                }
            }
            tracing::debug!(
                source,
                polled = events.len(),
                enqueued,
                duplicate,
                no_route,
                "intake catch-up: ポーリング完了"
            );
        }
        Err(e) => {
            // 秘密を出さないため error の Display のみ（アダプタ側で本文/トークンは含めない）。
            tracing::warn!(source, error = %e, "intake catch-up: ポーリング失敗");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 既知ベクタ: HMAC-SHA256(key="key", msg="The quick brown fox jumps over the lazy dog")
    /// = f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8
    const KEY: &str = "key";
    const MSG: &[u8] = b"The quick brown fox jumps over the lazy dog";
    const SIG_HEX: &str = "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8";

    #[test]
    fn verify_accepts_correct_signature() {
        assert!(verify_signature(KEY, MSG, SIG_HEX));
        assert!(verify_signature(KEY, MSG, &format!("sha256={SIG_HEX}")));
        assert!(verify_signature(
            KEY,
            MSG,
            &format!("sha256={}", SIG_HEX.to_uppercase())
        ));
    }

    #[test]
    fn verify_rejects_tampered_body_key_or_sig() {
        assert!(
            !verify_signature(KEY, b"tampered", SIG_HEX),
            "body 改変を通した"
        );
        assert!(
            !verify_signature("wrong", MSG, SIG_HEX),
            "別 secret を通した"
        );
        assert!(!verify_signature(KEY, MSG, "deadbeef"), "別署名を通した");
        assert!(!verify_signature("", MSG, SIG_HEX), "空 secret を通した");
        assert!(!verify_signature(KEY, MSG, "sha256="), "空 hex を通した");
        assert!(!verify_signature(KEY, MSG, "sha256=zz"), "非 hex を通した");
        assert!(!verify_signature(KEY, MSG, "sha256=abc"), "奇数長を通した");
    }

    #[test]
    fn dedup_key_prefers_id_then_hash() {
        let with_id = serde_json::json!({"id": 7});
        assert_eq!(
            webhook_dedup_key("comment.created", &with_id, b"{}"),
            "comment.created:7"
        );
        let str_id = serde_json::json!({"id": "abc"});
        assert_eq!(
            webhook_dedup_key("chat.message", &str_id, b"{}"),
            "chat.message:abc"
        );
        // id 無し → 同じ body は同じキー、違う body は違うキー。
        let no_id = serde_json::json!({"x": 1});
        let k1 = webhook_dedup_key("e", &no_id, b"body-A");
        let k2 = webhook_dedup_key("e", &no_id, b"body-A");
        let k3 = webhook_dedup_key("e", &no_id, b"body-B");
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
        assert!(k1.starts_with("e:sha256:"));
    }

    /// #470 の受け入れ条件: **コードを触らず、設定だけで 2 つ目の source を足せる**こと。
    /// 2 つの `[[intake.sources]]`（別サービス）を TOML から parse し、`build_adapters` が
    /// 両方をアダプタ化することを示す。source 名は型に焼き付いていない。
    #[test]
    fn two_sources_configured_by_config_only() {
        let toml = r#"
process_interval_secs = 60
catch_up_interval_secs = 600

[[sources]]
name = "omoikane"
kind = "rest_list"
base_url = "https://kb.example"
auth = { kind = "bearer", token = "tok-1" }
list_path = "/v1/comments/recent"
query = { entry_created_by = "uid-1", limit = "50" }
event_type = "comment.created"

[[sources]]
name = "acme"
kind = "rest_list"
base_url = "https://acme.example/api/"
list_path = "notes"
query = { since = "0" }
id_field = "note_id"
event_type = "note.created"
array_path = "results"
"#;
        let cfg: crate::config::IntakeConfig =
            toml::from_str(toml).expect("generic sources parse without code changes");
        assert_eq!(cfg.sources.len(), 2);
        let adapters = build_adapters(&cfg);
        let names: Vec<&str> = adapters.iter().map(|a| a.source()).collect();
        assert_eq!(
            names,
            vec!["omoikane", "acme"],
            "2 つ目の source が設定だけでアダプタ化される（コード変更ゼロ）"
        );
    }

    /// `enabled = false` の source は畳まれて出てこない（設定を残したまま一時停止できる）。
    #[test]
    fn disabled_source_is_dropped_but_others_remain() {
        let toml = r#"
[[sources]]
name = "paused"
kind = "rest_list"
enabled = false
base_url = "https://x.example"
event_type = "e.created"

[[sources]]
name = "active"
kind = "rest_list"
base_url = "https://y.example"
event_type = "e.created"
"#;
        let cfg: crate::config::IntakeConfig = toml::from_str(toml).unwrap();
        let adapters = build_adapters(&cfg);
        let names: Vec<&str> = adapters.iter().map(|a| a.source()).collect();
        assert_eq!(names, vec!["active"]);
    }

    /// #470-N1: `[[intake.sources]].name` が `[[intake.routes]].source` と不一致だと、取得しても
    /// NoRoute で捨てられる。`sources_missing_routes` がその source を検出する（起動時 warn の土台）。
    #[test]
    fn sources_missing_routes_flags_name_route_mismatch() {
        let toml = r#"
[[routes]]
source = "omoikane"
event_type = "comment.created"
agent_id = "scout"

# name が routes.source と不一致（タイポ相当）→ 取得しても捨てられる
[[sources]]
name = "omoiakne"
kind = "rest_list"
base_url = "https://kb.example"
event_type = "comment.created"

# name が route と一致 → 配送先あり
[[sources]]
name = "omoikane"
kind = "rest_list"
base_url = "https://kb2.example"
event_type = "comment.created"
"#;
        let cfg: crate::config::IntakeConfig = toml::from_str(toml).unwrap();
        let active: Vec<String> = build_adapters(&cfg)
            .iter()
            .map(|a| a.source().to_string())
            .collect();
        let missing = sources_missing_routes(&cfg.routes, &active);
        // route と一致する "omoikane" は含まれず、不一致の "omoiakne" だけが検出される。
        assert_eq!(missing, vec!["omoiakne"]);
    }

    /// route が全 source に揃っていれば空（誤検出しない）。disabled で畳まれた source も対象外。
    #[test]
    fn sources_missing_routes_empty_when_all_matched() {
        let routes = vec![crate::config::IntakeRoute {
            source: "active".into(),
            event_type: "e.created".into(),
            agent_id: "scout".into(),
        }];
        // active_sources には有効化された source のみ入る前提（build_adapters が畳む）。
        let active = vec!["active".to_string()];
        assert!(sources_missing_routes(&routes, &active).is_empty());
    }
}
