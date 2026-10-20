//! D-1091: `hello` の `run_env`（この instance の binding 用の環境変数）の解析。
//!
//! gateway だけが「その binding が誰のアカウントか」を知っているので、gateway が宣言する。
//! core は名前も値も解釈せず、`execute_shell` の子へそのまま渡す。ここは形だけを検証する。

use serde_json::Value;

use crate::error::{ErrorCode, GateError};

/// 1 つの binding 宣言に載せられる環境変数の上限。`hello` は接続ごとに来るので、無制限に
/// 受け取ると 1 接続で巨大な map を抱えられる。実用（数個の鍵）には十分な余裕を残す。
const MAX_RUN_ENV_ENTRIES: usize = 64;
const MAX_RUN_ENV_NAME_LEN: usize = 128;
const MAX_RUN_ENV_VALUE_LEN: usize = 8192;

/// D-1091: hello の `run_env`（省略可）。名前→値の string map だけを受け付ける。
///
/// 値の意味は core では解釈しない（gateway の領分）。形だけ検証し、空名・非文字列・上限超過は
/// `bad_request` で切る（黙って捨てると、gateway は宣言したつもりで子に渡らない）。
pub(crate) fn parse_run_env(
    obj: &Value,
) -> Result<std::collections::BTreeMap<String, String>, GateError> {
    let mut out = std::collections::BTreeMap::new();
    let Some(value) = obj.get("run_env") else {
        return Ok(out);
    };
    if value.is_null() {
        return Ok(out);
    }
    let Value::Object(map) = value else {
        return Err(GateError::new(ErrorCode::BadRequest));
    };
    if map.len() > MAX_RUN_ENV_ENTRIES {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    for (name, value) in map {
        let Some(value) = value.as_str() else {
            return Err(GateError::new(ErrorCode::BadRequest));
        };
        if name.trim().is_empty()
            || name.len() > MAX_RUN_ENV_NAME_LEN
            || value.len() > MAX_RUN_ENV_VALUE_LEN
        {
            return Err(GateError::new(ErrorCode::BadRequest));
        }
        out.insert(name.clone(), value.to_string());
    }
    Ok(out)
}
