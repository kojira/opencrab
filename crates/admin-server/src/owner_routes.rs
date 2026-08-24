//! Discord / Nostr owner ID 経路の復元（DESIGN-OWNER-IDENTITY）。
//! handler は extract → store コマンド → 本体 JSON。SQL は書かない。

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use opencrab_port::{GateKindId, SubjectKind};
use opencrab_store::{OwnerExternalChange, OwnerIdentityError};

use crate::api::{AdminState, ApiResult};

fn kind(name: &str) -> GateKindId {
    GateKindId::parse(name.to_string()).expect("discord/nostr kind")
}

fn owner_err(error: OwnerIdentityError) -> (StatusCode, Json<Value>) {
    match error {
        OwnerIdentityError::InstanceUnknown => (
            StatusCode::OK,
            Json(json!({
                "ok": false,
                "error": "No Discord config found. Use PUT to create one.",
            })),
        ),
        OwnerIdentityError::IdentityConflict | OwnerIdentityError::AmbiguousIdentity => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "identity_conflict", "detail": error.to_string() })),
        ),
        OwnerIdentityError::Store(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "store_error", "detail": e.to_string() })),
        ),
    }
}

fn parse_agent(id: &str) -> ApiResult<i64> {
    id.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "bad_id",
                "detail": "id は整数（subject/place の内部 ID）である必要があります",
            })),
        )
    })
}

/// このルートがまだ適用しないフィールドが JSON に含まれていたら 501。部分適用して ok を返さない。
fn reject_unapplied_fields(body: &Value, fields: &[&str]) -> ApiResult<()> {
    let present: Vec<&str> = fields
        .iter()
        .copied()
        .filter(|name| body.get(*name).is_some())
        .collect();
    if present.is_empty() {
        return Ok(());
    }
    Err((
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "unimplemented",
            "detail": format!(
                "this route does not yet apply {}: see roadmap #772",
                present.join(", ")
            ),
            "fields": present,
        })),
    ))
}

fn decode_body<T: DeserializeOwned>(body: Value) -> ApiResult<T> {
    serde_json::from_value(body).map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "bad_request", "detail": e.to_string() })),
        )
    })
}

fn ensure_agent(st: &AdminState, agent: i64) -> ApiResult<()> {
    match st.store.get_subject(agent) {
        Ok(Some(row)) if row.kind == SubjectKind::Agent => Ok(()),
        Ok(Some(_)) | Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "not_found", "detail": "agent がありません" })),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "store_error", "detail": e.to_string() })),
        )),
    }
}

fn token_masked(_has_secret: bool) -> String {
    "***".to_string()
}

fn discord_configured_json(
    owner_discord_id: &str,
    enabled: bool,
    running: bool,
    has_secret: bool,
) -> Value {
    json!({
        "configured": true,
        "enabled": enabled,
        "token_masked": token_masked(has_secret),
        "owner_discord_id": owner_discord_id,
        "running": running,
    })
}

fn nostr_json(
    configured: bool,
    enabled: bool,
    running: bool,
    has_secret: bool,
    owner_pubkey: &str,
    config_bytes: &[u8],
) -> Value {
    let parsed: Value = serde_json::from_slice(config_bytes).unwrap_or_else(|_| json!({}));
    let relays = parsed
        .get("relays")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let filter = parsed
        .get("filter")
        .cloned()
        .unwrap_or_else(|| json!({ "authors": [], "keywords": [], "kinds": [] }));
    json!({
        "configured": configured,
        "enabled": enabled,
        "running": running,
        "has_secret_key": has_secret,
        "secret_key_masked": if has_secret { "••••••••" } else { "" },
        "owner_pubkey": owner_pubkey,
        "relays": relays,
        "filter": filter,
    })
}

fn discord_put_owner(req: &PutDiscordBody) -> OwnerExternalChange {
    OwnerExternalChange::Set(
        req.owner_discord_id
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string(),
    )
}

fn discord_patch_owner(req: &PatchDiscordBody) -> OwnerExternalChange {
    match &req.owner_discord_id {
        None => OwnerExternalChange::Keep,
        Some(value) => OwnerExternalChange::Set(value.trim().to_string()),
    }
}

/// None = 現状維持、Some("") = クリア、Some(hex) = 設定。不正は 400。
pub(crate) fn normalize_owner_pubkey_input(
    raw: Option<&str>,
) -> Result<Option<String>, (StatusCode, Json<Value>)> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Some(String::new()));
    }
    normalize_pubkey(trimmed).map(Some).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "bad_owner_pubkey",
                "detail": "owner_pubkey は npub1... か 64 桁の hex で指定してください",
            })),
        )
    })
}

const CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
const PUBKEY_HEX_LEN: usize = 64;
const NPUB_HRP: &str = "npub";

fn polymod(values: &[u8]) -> u32 {
    const GEN: [u32; 5] = [
        0x3b6a_57b2,
        0x2650_8e6d,
        0x1ea1_19fa,
        0x3d42_33dd,
        0x2a14_62b3,
    ];
    let mut chk: u32 = 1;
    for v in values {
        let top = chk >> 25;
        chk = ((chk & 0x01ff_ffff) << 5) ^ u32::from(*v);
        for (i, g) in GEN.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

fn hrp_expand(hrp: &str) -> Vec<u8> {
    let mut out: Vec<u8> = hrp.bytes().map(|c| c >> 5).collect();
    out.push(0);
    out.extend(hrp.bytes().map(|c| c & 31));
    out
}

fn convert_5_to_8(data: &[u8]) -> Option<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::with_capacity(data.len() * 5 / 8);
    for &v in data {
        if v >> 5 != 0 {
            return None;
        }
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        while bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    if bits >= 5 || (acc << (8 - bits)) & 0xff != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
fn convert_8_to_5(data: &[u8]) -> Vec<u8> {
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::with_capacity(data.len() * 8 / 5 + 1);
    for &v in data {
        acc = (acc << 8) | u32::from(v);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(((acc >> bits) & 31) as u8);
        }
    }
    if bits > 0 {
        out.push(((acc << (5 - bits)) & 31) as u8);
    }
    out
}

fn bech32_decode(s: &str) -> Option<(String, Vec<u8>)> {
    if !s.is_ascii() {
        return None;
    }
    if s.chars().any(|c| c.is_ascii_lowercase()) && s.chars().any(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let lower = s.to_ascii_lowercase();
    let sep = lower.rfind('1')?;
    if sep == 0 || sep + 7 > lower.len() {
        return None;
    }
    let hrp = &lower[..sep];
    if hrp.bytes().any(|c| !(33..=126).contains(&c)) {
        return None;
    }
    let mut data = Vec::with_capacity(lower.len() - sep - 1);
    for c in lower[sep + 1..].bytes() {
        data.push(CHARSET.iter().position(|&x| x == c)? as u8);
    }
    let mut checked = hrp_expand(hrp);
    checked.extend_from_slice(&data);
    if polymod(&checked) != 1 {
        return None;
    }
    data.truncate(data.len() - 6);
    Some((hrp.to_string(), data))
}

#[cfg(test)]
fn bech32_encode(hrp: &str, data: &[u8]) -> String {
    let mut values = hrp_expand(hrp);
    values.extend_from_slice(data);
    values.extend_from_slice(&[0u8; 6]);
    let checksum = polymod(&values) ^ 1;
    let mut out = String::with_capacity(hrp.len() + 1 + data.len() + 6);
    out.push_str(hrp);
    out.push('1');
    for &d in data {
        out.push(CHARSET[d as usize] as char);
    }
    for i in 0..6 {
        out.push(CHARSET[((checksum >> (5 * (5 - i))) & 31) as usize] as char);
    }
    out
}

fn normalize_pubkey(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.len() == PUBKEY_HEX_LEN && s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Some(s.to_ascii_lowercase());
    }
    let (hrp, data) = bech32_decode(s)?;
    if hrp != NPUB_HRP {
        return None;
    }
    let bytes = convert_5_to_8(&data)?;
    if bytes.len() != PUBKEY_HEX_LEN / 2 {
        return None;
    }
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
fn npub_from_hex(hex: &str) -> String {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
        .collect();
    bech32_encode(NPUB_HRP, &convert_8_to_5(&bytes))
}

#[derive(Debug, Deserialize)]
pub struct PutDiscordBody {
    pub owner_discord_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PatchDiscordBody {
    #[allow(dead_code)]
    pub bot_token: Option<String>,
    pub owner_discord_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PutNostrBody {
    #[allow(dead_code)]
    #[serde(default)]
    pub secret_key: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub relays: Vec<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub authors: Vec<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub keywords: Vec<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub kinds: Vec<u32>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub owner_pubkey: Option<String>,
}

async fn get_discord(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&id)?;
    ensure_agent(&st, agent)?;
    let Some(instance) = st
        .store
        .dedicated_gate_instance(&kind("discord"), agent)
        .map_err(owner_err)?
    else {
        return Ok(Json(json!({ "configured": false })));
    };
    let Some(proj) = st
        .store
        .gate_owner_projection(&instance)
        .map_err(|e| owner_err(OwnerIdentityError::Store(e)))?
    else {
        return Ok(Json(json!({ "configured": false })));
    };
    if !proj.present {
        return Ok(Json(json!({ "configured": false })));
    }
    Ok(Json(discord_configured_json(
        &proj.owner_external_id,
        proj.enabled,
        proj.running,
        proj.has_secret,
    )))
}

async fn put_discord(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    reject_unapplied_fields(&body, &["bot_token"])?;
    let req: PutDiscordBody = decode_body(body)?;
    let agent = parse_agent(&id)?;
    ensure_agent(&st, agent)?;
    let instance = st
        .store
        .ensure_dedicated_gate_instance(&kind("discord"), agent, 0)
        .map_err(owner_err)?;
    st.store
        .apply_owner_principal(&instance, discord_put_owner(&req), 0)
        .map_err(owner_err)?;
    Ok(Json(json!({
        "ok": true,
        "message": "Discord bot started.",
    })))
}

async fn patch_discord(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Json(req): Json<PatchDiscordBody>,
) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&id)?;
    ensure_agent(&st, agent)?;
    let Some(instance) = st
        .store
        .dedicated_gate_instance(&kind("discord"), agent)
        .map_err(owner_err)?
    else {
        return Ok(Json(json!({
            "ok": false,
            "error": "No Discord config found. Use PUT to create one.",
        })));
    };
    let out = match st
        .store
        .apply_owner_principal(&instance, discord_patch_owner(&req), 0)
    {
        Ok(out) => out,
        Err(OwnerIdentityError::InstanceUnknown) => {
            return Ok(Json(json!({
                "ok": false,
                "error": "No Discord config found. Use PUT to create one.",
            })));
        }
        Err(error) => return Err(owner_err(error)),
    };
    let proj = st
        .store
        .gate_owner_projection(&instance)
        .map_err(|e| owner_err(OwnerIdentityError::Store(e)))?
        .ok_or_else(|| owner_err(OwnerIdentityError::InstanceUnknown))?;
    Ok(Json(json!({
        "ok": true,
        "configured": proj.present,
        "enabled": proj.enabled,
        "token_masked": token_masked(proj.has_secret),
        "owner_discord_id": out.owner_external_id,
    })))
}

async fn delete_discord(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&id)?;
    ensure_agent(&st, agent)?;
    let Some(instance) = st
        .store
        .dedicated_gate_instance(&kind("discord"), agent)
        .map_err(owner_err)?
    else {
        return Ok(Json(json!({ "deleted": false })));
    };
    let deleted = st
        .store
        .tombstone_gate_instance(&instance, 0)
        .map_err(owner_err)?;
    Ok(Json(json!({ "deleted": deleted })))
}

async fn get_nostr(State(st): State<AdminState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&id)?;
    ensure_agent(&st, agent)?;
    let Some(instance) = st
        .store
        .dedicated_gate_instance(&kind("nostr"), agent)
        .map_err(owner_err)?
    else {
        return Ok(Json(nostr_json(false, false, false, false, "", b"{}")));
    };
    let Some(proj) = st
        .store
        .gate_owner_projection(&instance)
        .map_err(|e| owner_err(OwnerIdentityError::Store(e)))?
    else {
        return Ok(Json(nostr_json(false, false, false, false, "", b"{}")));
    };
    if !proj.present {
        return Ok(Json(nostr_json(false, false, false, false, "", b"{}")));
    }
    Ok(Json(nostr_json(
        true,
        proj.enabled,
        proj.running,
        proj.has_secret,
        &proj.owner_external_id,
        &proj.config_bytes,
    )))
}

async fn put_nostr(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    reject_unapplied_fields(&body, &["secret_key", "relays", "enabled"])?;
    let req: PutNostrBody = decode_body(body)?;
    let agent = parse_agent(&id)?;
    ensure_agent(&st, agent)?;
    let owner = normalize_owner_pubkey_input(req.owner_pubkey.as_deref())?;
    let instance = st
        .store
        .ensure_dedicated_gate_instance(&kind("nostr"), agent, 0)
        .map_err(owner_err)?;
    let change = match owner {
        None => OwnerExternalChange::Keep,
        Some(value) => OwnerExternalChange::Set(value),
    };
    st.store
        .apply_owner_principal(&instance, change, 0)
        .map_err(owner_err)?;
    Ok(Json(json!({ "updated": true, "enabled": req.enabled })))
}

async fn delete_nostr(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&id)?;
    ensure_agent(&st, agent)?;
    let Some(instance) = st
        .store
        .dedicated_gate_instance(&kind("nostr"), agent)
        .map_err(owner_err)?
    else {
        return Ok(Json(json!({ "deleted": false })));
    };
    let deleted = st
        .store
        .tombstone_gate_instance(&instance, 0)
        .map_err(owner_err)?;
    Ok(Json(json!({ "deleted": deleted })))
}

pub fn owner_id_routes() -> Router<AdminState> {
    Router::new()
        .route(
            "/api/agents/{id}/discord",
            get(get_discord)
                .put(put_discord)
                .patch(patch_discord)
                .delete(delete_discord),
        )
        .route(
            "/api/agents/{id}/nostr",
            get(get_nostr).put(put_nostr).delete(delete_nostr),
        )
}

#[cfg(test)]
mod contract {
    use super::*;
    use crate::api::{create_router, AdminState};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use opencrab_db::Db;
    use opencrab_port::Standing;
    use opencrab_store::Store;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn dummy_db() -> Arc<Db> {
        Arc::new(Db::from_connection(
            rusqlite::Connection::open_in_memory().expect("memory db"),
        ))
    }

    fn state_from_store(store: Store) -> AdminState {
        AdminState {
            store: Arc::new(store),
            db: dummy_db(),
            compaction_ratio: 0.5,
        }
    }

    fn seed_agent(store: &Store) -> i64 {
        store
            .create_subject(
                SubjectKind::Agent,
                "A",
                "persona",
                "engine",
                Standing::Trusted,
                0,
            )
            .expect("agent")
    }

    async fn call(
        state: AdminState,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(match body {
                Some(value) => Body::from(serde_json::to_vec(&value).expect("json")),
                None => Body::empty(),
            })
            .expect("request");
        let response = create_router(state)
            .oneshot(request)
            .await
            .expect("oneshot");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn discord_put_omits_and_empty_clear_owner() {
        let store = Store::new_in_memory().expect("store");
        let agent = seed_agent(&store);
        let state = state_from_store(store);
        let (status, body) = call(
            state.clone(),
            "PUT",
            &format!("/api/agents/{agent}/discord"),
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        let (status, body) = call(
            state.clone(),
            "GET",
            &format!("/api/agents/{agent}/discord"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["configured"], true);
        assert_eq!(body["owner_discord_id"], "");

        let (status, _) = call(
            state.clone(),
            "PUT",
            &format!("/api/agents/{agent}/discord"),
            Some(json!({
                "owner_discord_id": "  owner-1  "
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = call(
            state.clone(),
            "GET",
            &format!("/api/agents/{agent}/discord"),
            None,
        )
        .await;
        assert_eq!(body["owner_discord_id"], "owner-1");

        let (status, _) = call(
            state.clone(),
            "PUT",
            &format!("/api/agents/{agent}/discord"),
            Some(json!({
                "owner_discord_id": ""
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = call(state, "GET", &format!("/api/agents/{agent}/discord"), None).await;
        assert_eq!(body["owner_discord_id"], "");
    }

    #[tokio::test]
    async fn discord_patch_omit_keeps_empty_clears() {
        let store = Store::new_in_memory().expect("store");
        let agent = seed_agent(&store);
        let state = state_from_store(store);
        let (status, body) = call(
            state.clone(),
            "PATCH",
            &format!("/api/agents/{agent}/discord"),
            Some(json!({"owner_discord_id": "owner-2"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], false);

        call(
            state.clone(),
            "PUT",
            &format!("/api/agents/{agent}/discord"),
            Some(json!({
                "owner_discord_id": "owner-2"
            })),
        )
        .await;
        let (status, body) = call(
            state.clone(),
            "PATCH",
            &format!("/api/agents/{agent}/discord"),
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["owner_discord_id"], "owner-2");

        let (status, body) = call(
            state.clone(),
            "PATCH",
            &format!("/api/agents/{agent}/discord"),
            Some(json!({"owner_discord_id": "  owner-3\n"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["owner_discord_id"], "owner-3");

        let (status, body) = call(
            state,
            "PATCH",
            &format!("/api/agents/{agent}/discord"),
            Some(json!({"owner_discord_id": ""})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["owner_discord_id"], "");
    }

    #[tokio::test]
    async fn nostr_put_keep_clear_and_rejects_bad_npub() {
        let store = Store::new_in_memory().expect("store");
        let agent = seed_agent(&store);
        let state = state_from_store(store);
        let hex = "11".repeat(32);
        let npub = npub_from_hex(&hex);

        let (status, body) = call(
            state.clone(),
            "PUT",
            &format!("/api/agents/{agent}/nostr"),
            Some(json!({
                "owner_pubkey": "not-a-key"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let (status, body) = call(
            state.clone(),
            "PUT",
            &format!("/api/agents/{agent}/nostr"),
            Some(json!({
                "owner_pubkey": npub
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["updated"], true);
        let (_, got) = call(
            state.clone(),
            "GET",
            &format!("/api/agents/{agent}/nostr"),
            None,
        )
        .await;
        assert_eq!(got["owner_pubkey"], hex);

        let (status, _) = call(
            state.clone(),
            "PUT",
            &format!("/api/agents/{agent}/nostr"),
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, got) = call(
            state.clone(),
            "GET",
            &format!("/api/agents/{agent}/nostr"),
            None,
        )
        .await;
        assert_eq!(got["owner_pubkey"], hex);

        let (status, _) = call(
            state.clone(),
            "PUT",
            &format!("/api/agents/{agent}/nostr"),
            Some(json!({
                "owner_pubkey": ""
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, got) = call(state, "GET", &format!("/api/agents/{agent}/nostr"), None).await;
        assert_eq!(got["owner_pubkey"], "");
    }

    #[tokio::test]
    async fn put_rejects_unapplied_fields_without_partial_write() {
        let store = Store::new_in_memory().expect("store");
        let agent = seed_agent(&store);
        let state = state_from_store(store);

        let (status, body) = call(
            state.clone(),
            "PUT",
            &format!("/api/agents/{agent}/discord"),
            Some(json!({
                "bot_token": "synthetic-token",
                "owner_discord_id": "should-not-apply"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
        assert_eq!(body["error"], "unimplemented");
        assert!(body["detail"].as_str().unwrap_or("").contains("bot_token"));
        assert!(body["detail"].as_str().unwrap_or("").contains("#772"));
        let (_, got) = call(
            state.clone(),
            "GET",
            &format!("/api/agents/{agent}/discord"),
            None,
        )
        .await;
        assert_eq!(got["configured"], false, "{got}");

        let hex = "22".repeat(32);
        let (status, body) = call(
            state.clone(),
            "PUT",
            &format!("/api/agents/{agent}/nostr"),
            Some(json!({
                "secret_key": "nsec1notapplied",
                "relays": ["wss://relay.example"],
                "enabled": true,
                "owner_pubkey": hex
            })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
        assert_eq!(body["error"], "unimplemented");
        let detail = body["detail"].as_str().unwrap_or("");
        assert!(detail.contains("secret_key"), "{detail}");
        assert!(detail.contains("relays"), "{detail}");
        assert!(detail.contains("enabled"), "{detail}");
        assert!(detail.contains("#772"), "{detail}");
        let (_, got) = call(state, "GET", &format!("/api/agents/{agent}/nostr"), None).await;
        assert_eq!(got["configured"], false, "{got}");
        assert_eq!(got["owner_pubkey"], "");
    }
}
