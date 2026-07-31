//! tool_result を **LLM へ返す前**と **永続化する前**に通す共通の無害化
//! （redaction ＋ サイズ上限 ＋ ワークスペース退避）。
//!
//! tool_result は 3 つの経路で使われる:
//!
//! 1. **同ターンで LLM へ返す**（`SkillEngine` のツール往復。`Message::tool`）
//! 2. inline 実行の永続化（`crates/server/src/process.rs` の `on_tool_result`）
//! 3. background dispatch の永続化（`SubtaskToolDispatcher` → `settle_completed`）
//!
//! 2/3 は `session_logs` へ書き、後続ターンの `build_conversation_string` が会話へ
//! 再注入する。したがって
//!
//! - 秘密フィールド（`nsec`）のマスク
//! - サイズ上限とワークスペースへの退避（超大結果で context 予算を吹き飛ばさない）
//!
//! は**全経路で同一**でなければならない。
//!
//! #284: 従来は 2/3（永続化）だけに上限が効いており、1（LLM へ返す経路）は素通り
//! だった。その結果 76KB の tool_result がそのままプロンプトへ積まれ、**同じターンの
//! ユーザー発言が 1 件もプロンプトに載らない**という事故が起きた。ロジックをこの
//! モジュールへ 1 つだけ置き、3 経路すべてから呼ぶ。
//!
//! `crates/actions` ではなく core に置くのは、`SkillEngine`（core）が actions に
//! 依存できないため（依存方向は actions → core）。

use std::path::Path;

/// この長さ（バイト）以上の tool_result は本文をそのまま流さず、ワークスペースへ
/// 退避してポインタ（＋ LLM 経路では冒頭プレビュー）に置き換える。
///
/// 値の根拠:
/// - 実測（#284）で 76,661 バイトの 1 件が 100k トークン級の会話予算を単独で食い潰し、
///   ユーザー発言が 1 件も残らなかった。1 件あたり数 KB 台でなければ話にならない。
/// - LLM 経路と DB 経路で**同じ値**を使う。ここがズレると「同ターンで見えた本文」と
///   「次ターンに会話へ再注入される本文」が食い違い、エージェントが前ターンの内容を
///   見失う（#272 と同種の破綻）。
/// - 10KB ≒ 日本語で 7k トークン弱、英字なら 2.5k トークン程度。1 ターンに数件積んでも
///   会話本文の枠を残せる上限。
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

/// 秘密フィールドのマスク（対象ツールのみ）。
fn redact_if_needed(tool_name: &str, result_json: &str) -> String {
    if needs_redaction(tool_name) {
        redact_secret_fields_json(result_json)
    } else {
        result_json.to_string()
    }
}

/// 文字境界を尊重して `limit` バイト以内へ切り詰める（バイト境界で切ると UTF-8 で panic）。
fn truncate_on_char_boundary(s: &str, limit: usize) -> &str {
    let mut end = limit.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 上限超過分をワークスペースへ退避する。成功したらワークスペース相対パスを返す。
fn offload_to_workspace(
    result_json: &str,
    session_id: &str,
    tool_call_id: &str,
    workspace_root: Option<&Path>,
) -> Option<String> {
    let root = workspace_root?;
    let tmp_dir = root.join("tmp");
    let _ = std::fs::create_dir_all(&tmp_dir);
    // session_id / tool_call_id は外部（gateway・LLM プロバイダ）由来の文字列。
    // パス区切りが混ざるとワークスペースの外へ書きうるので、英数字以外を潰す。
    let sanitize_component = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    };
    let filename = format!(
        "{}_{}.json",
        sanitize_component(session_id),
        sanitize_component(tool_call_id)
    );
    if std::fs::write(tmp_dir.join(&filename), result_json).is_ok() {
        Some(format!("tmp/{filename}"))
    } else {
        None
    }
}

/// tool_result を永続化用の本文へ変換する（redaction → サイズ上限/退避）。
///
/// - `workspace_root` が `Some` なら、上限超過分は `<root>/tmp/{session}_{tool_call_id}.json`
///   へ退避し、DB にはポインタ（`[Tool Result: file://tmp/...]`）だけを残す。
/// - `None`（退避先不明）や書き込み失敗時は、文字境界を尊重して切り詰める。
///
/// 通常運転では `SkillEngine` が先に [`sanitize_tool_result_for_llm`] を通すため、
/// ここへ来る本文は既に上限内（＝ no-op）。dispatch 経路と、engine を経由しない
/// 呼び出しのための安全網として残す。
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
    let result_json = redact_if_needed(tool_name, result_json);

    if result_json.len() < TOOL_RESULT_SIZE_LIMIT {
        return result_json;
    }

    if let Some(rel) = offload_to_workspace(&result_json, session_id, tool_call_id, workspace_root)
    {
        return format!("[Tool Result: file://{rel}]");
    }

    truncate_on_char_boundary(&result_json, TOOL_RESULT_SIZE_LIMIT).to_string()
}

/// tool_result を **LLM へ返す本文**へ変換する（redaction → サイズ上限/退避）。
///
/// [`sanitize_tool_result_for_log`] と同じ上限・同じ退避先を使うが、返す本文には
///
/// - 何バイトが切られたか
/// - 全文がどこにあるか（ワークスペース相対パス）と**読み方**
/// - 冒頭プレビュー
///
/// を残す。エージェントは `read_file` / `execute_shell` で続きを読める。
/// 退避できなかった場合（`workspace_root` が `None` / 書き込み失敗）は、
/// 「全文は残っていない」と分かる案内付きで切り詰める（黙って切らない）。
pub fn sanitize_tool_result_for_llm(
    tool_name: &str,
    result_json: &str,
    session_id: &str,
    tool_call_id: &str,
    workspace_root: Option<&Path>,
) -> String {
    let result_json = redact_if_needed(tool_name, result_json);

    if result_json.len() < TOOL_RESULT_SIZE_LIMIT {
        return result_json;
    }

    let original_len = result_json.len();
    let notice = match offload_to_workspace(&result_json, session_id, tool_call_id, workspace_root)
    {
        Some(rel) => format!(
            "[Tool result truncated: {original_len} bytes exceeded the \
             {TOOL_RESULT_SIZE_LIMIT}-byte inline limit. Full output was saved to `{rel}` in your \
             workspace - read it with read_file (or execute_shell) if you need the rest. \
             Do NOT re-run the same tool just to see it again.]"
        ),
        None => format!(
            "[Tool result truncated: {original_len} bytes exceeded the \
             {TOOL_RESULT_SIZE_LIMIT}-byte inline limit. The full output could not be saved, so \
             only the beginning is shown. Narrow the tool arguments (filter/limit) instead of \
             re-running the same call.]"
        ),
    };

    // 案内文の実長から逆算する（#286）。固定枠を引くやり方だと、案内文に埋め込む
    // `session_id` / `tool_call_id` が長い場合に枠を食い破って全体が上限を超え、
    // 永続化側（`sanitize_tool_result_for_log`）の「上限未満なら素通り」を通過して
    // **LLM が見た本文と DB に残る本文が食い違う**。`+ 1` は連結する改行の分。
    let preview_budget = TOOL_RESULT_SIZE_LIMIT.saturating_sub(notice.len() + 1);
    let preview = truncate_on_char_boundary(&result_json, preview_budget);
    format!("{notice}\n{preview}")
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
        let out = sanitize_tool_result_for_log("read_file", &big, "sess1", "tc9", Some(dir.path()));
        assert_eq!(out, "[Tool Result: file://tmp/sess1_tc9.json]");
        let saved = std::fs::read_to_string(dir.path().join("tmp/sess1_tc9.json")).unwrap();
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

    /// tool_call_id にパス区切りが混ざってもワークスペースの外へ書かない（#284）。
    #[test]
    fn offload_sanitizes_path_components() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = format!(r#"{{"data":"{}"}}"#, "x".repeat(TOOL_RESULT_SIZE_LIMIT));
        let out = sanitize_tool_result_for_log(
            "read_file",
            &big,
            "sess",
            "../../etc/passwd",
            Some(dir.path()),
        );
        assert!(!out.contains(".."));
        assert_eq!(dir.path().join("tmp").read_dir().unwrap().count(), 1);
    }

    /// #284 中核: LLM へ返す本文も上限内に収まり、退避先と読み方の案内が残る。
    #[test]
    fn llm_result_is_capped_and_points_at_full_output() {
        let dir = tempfile::TempDir::new().unwrap();
        // 実事故と同規模（76KB 超）の結果。
        let big = format!(r#"{{"data":"{}"}}"#, "u".repeat(80_000));
        let out = sanitize_tool_result_for_llm(
            "nostr_get_following",
            &big,
            "sessA",
            "tc1",
            Some(dir.path()),
        );
        assert!(
            out.len() <= TOOL_RESULT_SIZE_LIMIT,
            "LLM へ渡す本文が上限超過: {}",
            out.len()
        );
        assert!(out.contains("truncated"));
        assert!(out.contains("tmp/sessA_tc1.json"));
        assert!(out.contains("read_file"));
        // 冒頭プレビューは残る（何のツール結果か分かる）。
        assert!(out.contains(r#"{"data":"uuu"#));
        // 全文は退避されている。
        let saved = std::fs::read_to_string(dir.path().join("tmp/sessA_tc1.json")).unwrap();
        assert_eq!(saved.len(), big.len());
    }

    /// 上限未満の結果は LLM 経路でも素通り（回帰防止）。
    #[test]
    fn llm_result_under_limit_is_untouched() {
        let json = r#"{"success":true,"data":{"ok":true},"error":null}"#;
        let out = sanitize_tool_result_for_llm("read_file", json, "sess", "tc-1", None);
        assert_eq!(out, json);
    }

    /// 退避できないときも黙って切らず、切られたことを LLM に伝える。
    #[test]
    fn llm_result_without_workspace_still_explains_truncation() {
        let big = format!(r#"{{"data":"{}"}}"#, "あ".repeat(20_000));
        let out = sanitize_tool_result_for_llm("execute_shell", &big, "sess", "tc-1", None);
        assert!(out.len() <= TOOL_RESULT_SIZE_LIMIT);
        assert!(out.contains("truncated"));
        assert!(out.contains("could not be saved"));
    }

    /// #286: 案内文が長くなっても（session_id / tool_call_id が長い）上限を超えない。
    ///
    /// 固定枠を引く実装だと枠を食い破り、永続化側の「上限未満なら素通り」を通過して
    /// LLM が見た本文と DB に残る本文が食い違う。
    #[test]
    fn llm_notice_with_long_ids_still_fits_the_limit() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = "q".repeat(50_000);
        let long_session = "s".repeat(2_000);
        let long_call_id = "c".repeat(2_000);
        let out = sanitize_tool_result_for_llm(
            "read_file",
            &big,
            &long_session,
            &long_call_id,
            Some(dir.path()),
        );
        assert!(
            out.len() <= TOOL_RESULT_SIZE_LIMIT,
            "案内文が枠を食い破っている: {}",
            out.len()
        );
        // 永続化側を通しても no-op（＝ DB と LLM の本文が一致する）。
        let logged =
            sanitize_tool_result_for_log("read_file", &out, &long_session, &long_call_id, None);
        assert_eq!(logged, out);
    }

    /// LLM 経路でも秘密フィールドはマスクされる。
    #[test]
    fn llm_result_redacts_secret_tool() {
        let json = r#"{"success":true,"data":{"nsec":"nsec1secret"},"error":null}"#;
        let out = sanitize_tool_result_for_llm("nostr_generate_key", json, "sess", "tc-1", None);
        assert!(!out.contains("nsec1secret"));
    }
}
