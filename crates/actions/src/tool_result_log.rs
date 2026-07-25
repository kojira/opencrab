//! tool_result を **永続化する前**に通す共通の無害化（redaction ＋ サイズ上限）。
//!
//! inline 実行（`crates/server/src/process.rs` の `on_tool_result`）と
//! background dispatch（`subtask::SubtaskToolDispatcher` → `settle_completed`）は
//! どちらも同じ本文を `session_logs` へ書き、後続ターンの
//! `build_conversation_string` が会話へ再注入する。したがって
//!
//! - 秘密フィールド（`nsec`）のマスク
//! - サイズ上限とワークスペースへの退避（超大結果で context 予算を吹き飛ばさない）
//!
//! は**両経路で同一**でなければならない。片方（dispatch）だけ素通りだと、
//! 5MB のファイル読取り結果がそのまま DB に入り resume 時の会話再構築を破壊する。
//! そこでロジックをこのモジュールへ 1 つだけ置き、両経路から呼ぶ。

use std::path::Path;

/// この長さ以上の tool_result は本文を DB に入れず、ワークスペースへ退避する。
pub const TOOL_RESULT_SIZE_LIMIT: usize = 10_000;

/// tool_result JSON から秘密鍵フィールド（`nsec`）をマスクする。永続化前に呼ぶ。
///
/// ここに渡るのは `ActionResult` ラッパ全体の serialize
/// （`{"success":..,"data":{..},"error":..}`）で、`nsec` は `data` の**中**にある。
/// トップレベルだけ見ると素通りするため、object を再帰的に辿って `nsec` を潰す。
/// JSON として解釈できない場合は生の中身に秘密鍵が残りうるため、固定の placeholder に
/// 置き換える（生保存で漏らさない）。
pub fn redact_secret_fields_json(result_json: &str) -> String {
    fn redact(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(obj) => {
                if obj.contains_key("nsec") {
                    obj.insert(
                        "nsec".to_string(),
                        serde_json::Value::String("[redacted]".to_string()),
                    );
                }
                for (_, child) in obj.iter_mut() {
                    redact(child);
                }
            }
            serde_json::Value::Array(arr) => {
                for child in arr.iter_mut() {
                    redact(child);
                }
            }
            _ => {}
        }
    }
    match serde_json::from_str::<serde_json::Value>(result_json) {
        Ok(mut v) => {
            redact(&mut v);
            v.to_string()
        }
        Err(_) => "{\"note\":\"[redacted secret result]\"}".to_string(),
    }
}

/// 秘密を持ち出しうるツール名（結果を必ず redaction してから永続化する）。
fn needs_redaction(tool_name: &str) -> bool {
    tool_name == "nostr_generate_key"
}

/// tool_result を永続化用の本文へ変換する（redaction → サイズ上限/退避）。
///
/// - `workspace_root` が `Some` なら、上限超過分は `<root>/tmp/{session}_{tool_call_id}.json`
///   へ退避し、DB にはポインタ（`[Tool Result: file://tmp/...]`）だけを残す。
/// - `None`（退避先不明）や書き込み失敗時は、文字境界を尊重して切り詰める
///   （バイト境界で切ると UTF-8 で panic する）。
pub fn sanitize_tool_result_for_log(
    tool_name: &str,
    result_json: &str,
    session_id: &str,
    tool_call_id: &str,
    workspace_root: Option<&Path>,
) -> String {
    // 防御的マスク（defense-in-depth）。nostr_generate_key は既に nsec を返さない
    // 設計だが、tool_result は後続ターンで会話へ再注入されるため、万一 nsec が
    // 混ざっても永続化前にここで潰す（DB 保存時漏洩＋持ち出しの防止）。
    let result_json = if needs_redaction(tool_name) {
        redact_secret_fields_json(result_json)
    } else {
        result_json.to_string()
    };

    if result_json.len() < TOOL_RESULT_SIZE_LIMIT {
        return result_json;
    }

    if let Some(root) = workspace_root {
        let tmp_dir = root.join("tmp");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let filename = format!("{session_id}_{tool_call_id}.json");
        let file_path = tmp_dir.join(&filename);
        if std::fs::write(&file_path, &result_json).is_ok() {
            return format!("[Tool Result: file://tmp/{filename}]");
        }
    }

    // 文字境界を尊重して切り詰める。
    let mut end = TOOL_RESULT_SIZE_LIMIT.min(result_json.len());
    while !result_json.is_char_boundary(end) {
        end -= 1;
    }
    result_json[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_nsec_nested_in_data() {
        // set_on_tool_result に渡る実際の形は ActionResult ラッパ全体で、
        // nsec は data の中に入る（トップレベル走査だけでは漏れる）。
        let wrapper =
            r#"{"success":true,"data":{"npub":"npub1ok","nsec":"nsec1xxx"},"error":null}"#;
        let out = redact_secret_fields_json(wrapper);
        assert!(!out.contains("nsec1xxx"));
        assert!(out.contains("[redacted]"));
        assert!(out.contains("npub1ok"));
    }

    #[test]
    fn non_json_input_is_replaced_wholesale() {
        let out = redact_secret_fields_json("nsec1plaintextleak");
        assert!(!out.contains("nsec1plaintextleak"));
    }

    /// 秘密を持たないツールの結果はマスクされない（redaction は対象ツールのみ）。
    #[test]
    fn sanitize_leaves_small_results_untouched() {
        let json = r#"{"success":true,"data":{"ok":true},"error":null}"#;
        let out = sanitize_tool_result_for_log("read_file", json, "sess", "tc-1", None);
        assert_eq!(out, json);
    }

    /// dispatch 経路でも `nostr_generate_key` の秘密鍵はマスクされる。
    #[test]
    fn sanitize_redacts_secret_tool() {
        let json = r#"{"success":true,"data":{"nsec":"nsec1secret"},"error":null}"#;
        let out = sanitize_tool_result_for_log("nostr_generate_key", json, "sess", "tc-1", None);
        assert!(!out.contains("nsec1secret"));
    }

    /// 上限超過はワークスペースへ退避し、DB 本文はポインタだけになる。
    #[test]
    fn sanitize_offloads_large_result_to_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = format!(r#"{{"data":"{}"}}"#, "x".repeat(TOOL_RESULT_SIZE_LIMIT));
        let out =
            sanitize_tool_result_for_log("read_file", &big, "sess-1", "tc-9", Some(dir.path()));
        assert_eq!(out, "[Tool Result: file://tmp/sess-1_tc-9.json]");
        let saved = std::fs::read_to_string(dir.path().join("tmp/sess-1_tc-9.json")).unwrap();
        assert_eq!(saved.len(), big.len());
    }

    /// 退避先が無ければ切り詰める（無制限に DB へ入れない）。マルチバイト境界も守る。
    #[test]
    fn sanitize_truncates_when_no_workspace() {
        let big = format!(r#"{{"data":"{}"}}"#, "あ".repeat(TOOL_RESULT_SIZE_LIMIT));
        let out = sanitize_tool_result_for_log("read_file", &big, "sess", "tc-1", None);
        assert!(out.len() <= TOOL_RESULT_SIZE_LIMIT);
        assert!(big.starts_with(&out));
    }
}
