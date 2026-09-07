// ---------- Provider overrides (dashboard-managed) ----------

/// TOML の LlmConfig に DB のプロバイダーオーバーライドを適用した実効設定を返す。
///
/// マージ規則:
/// - `enabled == Some(false)`: プロバイダーを実効設定から**除外**する
///   （TOML にキーがあっても登録されない）。
/// - `enabled == Some(true)` で TOML に無いプロバイダー: 空の ProviderConfig を
///   作ってオーバーライドを適用する（ollama 等のローカル系を UI から有効化する経路）。
/// - `api_key` / `base_url` / `default_model` は Some のフィールドだけ上書き。
///   Some("") は「TOML 値の消去」として扱う。
pub fn apply_llm_overrides(
    base: &LlmConfig,
    overrides: &[opencrab_db::queries::LlmProviderOverrideRow],
) -> LlmConfig {
    let mut cfg = base.clone();
    for row in overrides {
        if row.enabled == Some(false) {
            cfg.providers.remove(&row.provider);
            continue;
        }
        let entry = cfg.providers.entry(row.provider.clone()).or_default();
        if let Some(key) = &row.api_key {
            entry.api_key = key.clone();
        }
        if let Some(url) = &row.base_url {
            entry.base_url = url.clone();
        }
        if let Some(model) = &row.default_model {
            entry.default_model = model.clone();
        }
        if let Some(effort) = &row.reasoning_effort {
            entry.reasoning_effort = effort.clone();
        }
        if let Some(bp) = &row.binary_path {
            entry.binary_path = bp.clone();
        }
        if let Some(args_json) = &row.args_json {
            // JSON 配列としてパース。壊れていれば既存を保つ。
            if let Ok(args) = serde_json::from_str::<Vec<String>>(args_json) {
                entry.args = args;
            }
        }
        if let Some(wd) = &row.working_dir {
            entry.working_dir = wd.clone();
        }
        if let Some(t) = row.timeout_secs {
            if t > 0 {
                entry.timeout_secs = t as u64;
            }
        }
    }
    cfg
}

