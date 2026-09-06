/// **nsec（Nostr 秘密鍵）は設定取得 API の応答に平文で現れない**（#203 の一括点検）。
///
/// `GET /api/agents/{id}/nostr` は `secret_key_masked` にマスク済みの値を載せる契約だが、
/// マスク関数を素通しに書き換えても落ちるテストが 1 件も無かった。nsec は Nostr の
/// アイデンティティそのもので、漏れれば第三者がそのエージェントとして投稿できる。
/// マスクの**戻り値**ではなく **API の応答ボディ全体**に平文が含まれないことを見る
/// （経路が違えば別のフィールドから漏れうるため）。
// #654: `/api/agents/{id}/nostr` ルートは nostr feature 時のみマウントされる（#651）。off では
// ルート不在で保存/取得の契約が成立しないので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn test_get_nostr_config_never_returns_raw_secret_key() {
    let app = create_test_app();
    // 本物と同じ形の、しかしテスト専用のダミー nsec。
    let nsec = "nsec1testonlyfakesecretkeyvalue000000000000000000000000000000";

    // 保存（enabled=false なのでゲートウェイは起動しない = ネットワークに出ない）。
    let (status, json) = send_request(
        app.clone(),
        "PUT",
        "/api/agents/a1/nostr",
        Some(serde_json::json!({
            "secret_key": nsec,
            "relays": ["wss://relay.example"],
            "keywords": ["crab"],
            "enabled": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(
        !json.to_string().contains(nsec),
        "保存の応答に平文 nsec が含まれている: {json}"
    );

    // 取得。
    let (status, json) = send_request(app.clone(), "GET", "/api/agents/a1/nostr", None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["configured"], true);
    assert_eq!(json["has_secret_key"], true, "鍵の有無は伝える: {json}");
    assert!(
        !json.to_string().contains(nsec),
        "取得の応答に平文 nsec が含まれている: {json}"
    );
    // 平文の断片も出さない（末尾数文字を見せる形のマスクへ緩めたら落とす）。
    assert!(
        !json.to_string().contains("testonlyfake"),
        "nsec の一部が応答に含まれている: {json}"
    );
    assert_eq!(
        json["secret_key_masked"], "••••••••",
        "マスク済みの固定文字列を返す: {json}"
    );
}

/// 平文（非 JSON）のエラーボディを読む。
///
/// `send_request` は JSON として解釈できないボディをバイト列の配列にして返すため、
/// エラー文言をそのまま `contains` できない。
// #654: この helper を使うのは nostr feature 依存の鍵払い出し e2e（#651）だけなので同じ cfg で囲む。
#[cfg(feature = "nostr")]
fn plain_body(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Array(bytes) => {
            let raw: Vec<u8> = bytes
                .iter()
                .filter_map(|b| b.as_u64().map(|n| n as u8))
                .collect();
            String::from_utf8_lossy(&raw).into_owned()
        }
        other => other.to_string(),
    }
}

/// **鍵の払い出しの受け口が無い構成では 503 で失敗する**（#191 段階2 PR4）。
///
/// 鍵生成は transport 固有の操作で、`AppState` の名指しフィールドから
/// capability の受け口（登録簿 → `key_provisioning`）へ移した。ハーネスの
/// `AppState` は登録簿が空なので受け口が引けない。ここが「無ければ黙って既定の
/// 外部コマンドを叩く」側へ倒れると、REST から想定外のバイナリで鍵を生成し
/// うる（= 外部プロセスの spawn が無言で起きる）ため、**明示的に失敗する**こと
/// と**文言が変わっていない**ことを固定する。
///
/// 判定の位置も仕様: prefix の書式検証（400）より後、鍵の生成より手前。
// #654: `/api/agents/{id}/nostr/generate` ルートは nostr feature 時のみマウントされる（#651）。
// off ではルート不在で 503 契約が成立しないので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn test_generate_nostr_key_fails_without_key_provisioning() {
    let app = create_test_app();

    let (status, body) = send_request(
        app.clone(),
        "POST",
        "/api/agents/a1/nostr/generate",
        Some(serde_json::json!({"prefix": "cr"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "受け口が無ければ 503（黙って既定の外部コマンドへ倒さない）: {body}"
    );
    assert!(
        plain_body(&body).contains("Nostr マネージャが無効です"),
        "エラー文言は据え置き: {body}"
    );

    // 書式が不正な prefix は受け口の有無より**手前**で 400（無効な prefix で
    // 外部プロセスを起こさない、という既存の順序）。
    let (status, body) = send_request(
        app.clone(),
        "POST",
        "/api/agents/a1/nostr/generate",
        Some(serde_json::json!({"prefix": "bbb"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "prefix の検証が先（bech32 に無い文字）: {body}"
    );
}

