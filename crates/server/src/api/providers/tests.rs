use super::*;
use serde_json::json;

fn body(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().unwrap().clone()
}

#[test]
fn valid_provider_name_rules() {
    assert!(valid_provider_name("acp"));
    assert!(valid_provider_name("openai-4o_mini"));
    assert!(!valid_provider_name(""));
    assert!(!valid_provider_name("bad name")); // space
    assert!(!valid_provider_name("bad/name")); // slash
    assert!(!valid_provider_name(&"x".repeat(65))); // too long
}

#[test]
fn build_override_row_sets_launch_fields() {
    let db = opencrab_db::Db::memory().unwrap();
    let conn = db.lock().unwrap();
    let b = body(json!({
        "binary_path": "/usr/bin/acp",
        "args": ["--foo", "bar"],
        "timeout_secs": 90,
        "enabled": true,
    }));
    let row = build_override_row(&conn, "acp", &b).unwrap().unwrap();
    assert_eq!(row.provider, "acp");
    assert_eq!(row.binary_path.as_deref(), Some("/usr/bin/acp"));
    assert_eq!(row.args_json.as_deref(), Some(r#"["--foo","bar"]"#));
    assert_eq!(row.timeout_secs, Some(90));
    assert_eq!(row.enabled, Some(true));
}

#[test]
fn build_override_row_all_none_means_delete() {
    let db = opencrab_db::Db::memory().unwrap();
    let conn = db.lock().unwrap();
    // 空文字/null は解除。全て解除なら None（＝行削除）。
    let b = body(json!({
        "binary_path": "",
        "working_dir": "",
        "args": null,
        "timeout_secs": null,
    }));
    assert!(build_override_row(&conn, "acp", &b).unwrap().is_none());
}

#[test]
fn build_override_row_merges_onto_existing() {
    let db = opencrab_db::Db::memory().unwrap();
    let conn = db.lock().unwrap();
    // まず binary_path を保存。
    let first = build_override_row(&conn, "acp", &body(json!({"binary_path": "/a"})))
        .unwrap()
        .unwrap();
    opencrab_db::queries::upsert_llm_provider_override(&conn, &first).unwrap();
    // 次に timeout だけ変更 → binary_path は保持される（部分更新）。
    let merged = build_override_row(&conn, "acp", &body(json!({"timeout_secs": 30})))
        .unwrap()
        .unwrap();
    assert_eq!(merged.binary_path.as_deref(), Some("/a"));
    assert_eq!(merged.timeout_secs, Some(30));
}

#[test]
fn build_override_row_rejects_bad_types() {
    let db = opencrab_db::Db::memory().unwrap();
    let conn = db.lock().unwrap();
    assert!(build_override_row(&conn, "acp", &body(json!({"timeout_secs": "nope"}))).is_err());
    assert!(build_override_row(&conn, "acp", &body(json!({"args": "nope"}))).is_err());
}

/// 自動 health_check/ロールバックの対象は codex/cursor のみ。acp は
/// ネットワーク依存の本物ハンドシェイクのため自動対象から外す（#127 レビュー指摘）。
#[test]
fn auto_healthtest_excludes_acp_and_api_providers() {
    assert!(is_auto_healthtest_provider("codex"));
    assert!(is_auto_healthtest_provider("cursor"));
    assert!(!is_auto_healthtest_provider("acp"));
    assert!(!is_auto_healthtest_provider("openai"));
}

/// レビュー Finding 1 の回帰ガード: subprocess プロバイダを enabled=false で
/// 無効化するとルーターから消える（get_provider が None）。
/// `apply_provider_override_with_rollback` はこの「意図した非登録」を
/// health_check 失敗と混同してロールバックしてはならない（＝適用成功で返す）。
#[test]
fn disabling_subprocess_provider_removes_it_from_router() {
    use crate::config::{apply_llm_overrides, build_llm_router, LlmConfig, ProviderConfig};
    let mut cfg = LlmConfig::default();
    cfg.providers.insert(
        "acp".to_string(),
        ProviderConfig {
            binary_path: "/bin/true".to_string(),
            ..Default::default()
        },
    );
    // default_provider は定義済みセクションを指す必要がある（#660）。
    // このテストの主題は override による登録/除外なので、既定を唯一の provider に合わせる。
    cfg.default_provider = "acp".to_string();
    // enabled 無指定 → acp は登録される（build は I/O せず成功）。
    let router = build_llm_router(&cfg).unwrap();
    assert!(router.get_provider("acp").is_some());
    // enabled=false override を適用 → acp はルーターから消える。
    let overrides = vec![opencrab_db::queries::LlmProviderOverrideRow {
        provider: "acp".to_string(),
        enabled: Some(false),
        ..Default::default()
    }];
    let merged = apply_llm_overrides(&cfg, &overrides);
    let router2 = build_llm_router(&merged).unwrap();
    assert!(
        router2.get_provider("acp").is_none(),
        "disabled subprocess provider must be absent from the router"
    );
}

/// D-1141: disabling a provider that `[llm.fallback] chain` still names is refused with the
/// startup error and nothing is saved, so the next startup still works.
#[test]
fn disabling_a_provider_the_fallback_chain_names_is_refused_and_not_saved() {
    use crate::config::ProviderConfig;
    let mut config = crate::test_config("chatgpt", "gpt-6-sol");
    for name in ["chatgpt", "ollama"] {
        config
            .llm
            .providers
            .insert(name.to_string(), ProviderConfig::default());
    }
    config.llm.fallback.chain = vec!["ollama".to_string()];
    let state = crate::AppState::new(
        opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap()),
        opencrab_llm::router::LlmRouter::new(),
        config,
    );
    let disable = {
        let conn = state.db.lock().unwrap();
        build_override_row(&conn, "ollama", &body(json!({"enabled": false})))
            .unwrap()
            .unwrap()
    };

    let (status, message) = store_override(&state, "ollama", Some(&disable)).unwrap_err();

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        message.contains("fallback.chain references undefined provider 'ollama'"),
        "{message}"
    );
    let conn = state.db.lock().unwrap();
    assert!(
        opencrab_db::queries::get_llm_provider_override(&conn, "ollama")
            .unwrap()
            .is_none()
    );
    drop(conn);
    crate::config::build_effective_llm_router(&state.db, &state.config().llm)
        .expect("startup still builds the router");
}
