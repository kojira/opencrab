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
//! - トークン上限とワークスペースへの退避（超大結果で context 予算を吹き飛ばさない）
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
//!
//! #294: 上限超過時に**冒頭プレビューを渡すのをやめた**。パスを案内しつつ生データも
//! 流していたため、トークンを食う割に全体像は分からず、LLM が「先頭だけ見えている」
//! 状態で判断していた（979 人のフォロー一覧なら先頭 20 人で結論を出す）。さらに、
//! 中身を見る必要がないケース（パスを次のコマンドへ渡すだけ）でも 9.4KB を消費して
//! いた。いまはメタ情報だけを返し、参照方法は LLM に委ねる。
//! 併せて上限の物差しをバイトからトークンへ揃えた（[`TOOL_RESULT_TOKEN_LIMIT`]）。

use std::path::Path;

/// このトークン数以上の tool_result は本文を**一切**流さず、ワークスペースへ
/// 退避したうえでメタ情報（パス／バイト数／行数／推定トークン数）だけの案内に置き換える。
///
/// **バイトではなくトークンで測る**理由（#294）: 会話履歴のコンパクション
/// （`build_conversation_string` の `DEFAULT_CONTEXT_BUDGET_TOKENS`）は元からトークン
/// 基準で、tool_result だけバイト基準だった。同じコンテキスト予算を食い合うのに
/// 物差しが違うと、同じ 10KB でも日本語・英数字・base64 で実効トークン量が数倍ぶれ、
/// 「予算内のはずが溢れる／まだ余裕があるのに切る」が起きる。両者とも
/// [`crate::tokens::estimate_tokens`]（tiktoken `o200k_base`）で測る。
///
/// 値の根拠:
/// - 実測（#284）で 76,661 バイトの 1 件が 100k トークン級の会話予算を単独で食い潰し、
///   ユーザー発言が 1 件も残らなかった。1 件あたり数 KB 台でなければ話にならない。
/// - 旧 10,000 バイト上限の実効トークン量: tool_result はほぼ ASCII の JSON なので
///   ≒ 2,500 トークン（o200k_base で ~4 バイト/トークン）、日本語混じりでも ~3,000
///   トークン。2,500 は**旧上限をどちらの言語でも上回らない**値で、バイト → トークンの
///   切り替えで実効的に緩くならない。
/// - 100k トークン予算に対して 1 件 2.5k なら、1 ターンに数件積んでも会話本文の枠が残る。
/// - LLM 経路と DB 経路で**同じ値**を使う。ここがズレると「同ターンで見えた本文」と
///   「次ターンに会話へ再注入される本文」が食い違い、エージェントが前ターンの内容を
///   見失う（#272 と同種の破綻）。
pub const TOOL_RESULT_TOKEN_LIMIT: usize = 2_500;

/// 退避ファイル名 1 コンポーネント（session_id / tool_call_id）の上限バイト数。
///
/// 2 つの理由で必要:
/// - 多くのファイルシステムはファイル名 255 バイト。長い ID をそのまま繋ぐと
///   `std::fs::write` が `ENAMETOOLONG` で落ち、退避できたはずの全文を捨ててしまう。
/// - 案内文にはこのパスが載る。長さを縛らないと案内文自体が
///   [`TOOL_RESULT_TOKEN_LIMIT`] を超え、永続化側の「上限未満なら素通り」を通過して
///   LLM と DB の本文が食い違う（#286）。
const OFFLOAD_COMPONENT_LIMIT: usize = 64;

/// 本文が上限を超えているか。**tokenizer を呼ぶ前にバイト数で足切りする**（#294）。
///
/// `o200k_base` の 1 トークンは必ず 1 バイト以上なので `tokens <= bytes`。
/// つまりバイト数が上限未満なら、トークン数を数えるまでもなく上限未満。ツール結果は
/// 大半が数百バイトなので、この早期 return で BPE encode（数十 KB で ~数百 µs）は
/// ほぼ走らない。上界の性質は `tokens::tests::tokens_never_exceed_bytes` で固定。
fn exceeds_limit(s: &str) -> bool {
    if s.len() < TOOL_RESULT_TOKEN_LIMIT {
        return false;
    }
    crate::tokens::estimate_tokens(s) >= TOOL_RESULT_TOKEN_LIMIT
}

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
    // 長さも縛る（[`OFFLOAD_COMPONENT_LIMIT`] の doc 参照）。
    let sanitize_component = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .take(OFFLOAD_COMPONENT_LIMIT)
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

/// 本文の行数。末尾の改行は「空の最終行」を作らない（`"a\nb"` も `"a\nb\n"` も 2 行、
/// 空文字列は 0 行）。`head -n` / エディタの行番号と一致する数え方。
fn count_lines(s: &str) -> usize {
    s.lines().count()
}

/// 形式の手がかりを**パースせずに**推定する（全文は最大数十 KB なので、O(1) の
/// 端点チェック以上のコストは払わない）。判別できなければ `None`（案内から省く）。
fn format_hint(s: &str) -> Option<&'static str> {
    let t = s.trim();
    match (t.as_bytes().first()?, t.as_bytes().last()?) {
        (b'{', b'}') => Some("looks like a JSON object"),
        (b'[', b']') => Some("looks like a JSON array"),
        _ => None,
    }
}

/// 上限超過時の案内文を組む。**生データは 1 バイトも含めない**（#294）。
///
/// 含めるのはメタ情報だけ:
/// - 保存先（ワークスペース相対パス）
/// - バイトサイズ
/// - 行数
/// - 推定トークン数（上限の物差しと同じ単位。LLM が「全部読んだら予算をどれだけ
///   食うか」を自分で見積もれる）
/// - 形式の手がかり（判別できたときのみ）
///
/// 「どう参照するか」は**指示しない**。先頭だけ読む / grep する / jq で加工する /
/// そもそも読まずにパスを次のコマンドへ渡す、のどれが最適かはタスク次第で、
/// 特定の手順を強制すると 979 件の一覧を「先頭 20 件だけ見て結論」のような誤りを
/// 誘発する。選択肢だけ示して判断は LLM に委ねる。
///
/// 「同じツールを再実行するな」は残す（#284 のループ防止に効いている）。
fn oversized_notice(result_json: &str, saved_to: Option<&str>) -> String {
    let bytes = result_json.len();
    let lines = count_lines(result_json);
    // ここへ来る時点で `exceeds_limit` が encode 済み。ツール結果 1 件あたり
    // 2 回目の encode になるが、上限超過は稀（大半は早期 return する）。
    let tokens = crate::tokens::estimate_tokens(result_json);
    let hint = match format_hint(result_json) {
        Some(h) => format!(", {h}"),
        None => String::new(),
    };
    match saved_to {
        Some(rel) => format!(
            "[Tool result withheld: {bytes} bytes, {lines} lines, ~{tokens} tokens{hint}. \
             It exceeded the {TOOL_RESULT_TOKEN_LIMIT}-token inline limit, so none of its \
             content is included here. The full output was saved to `{rel}` (path relative to \
             your workspace root). It is up to you how to use it: read part of it, search it, \
             transform it, or pass the path straight to the next command without reading it at \
             all. If you cannot read that file (some runs have no file-reading tool), instead \
             re-run with a narrower request (smaller id/time window, fewer rows, or estimate \
             the size first) so the result fits under the limit. Do NOT re-run the same tool \
             with the same arguments just to see the output again.]"
        ),
        None => format!(
            "[Tool result withheld: {bytes} bytes, {lines} lines, ~{tokens} tokens{hint}. \
             It exceeded the {TOOL_RESULT_TOKEN_LIMIT}-token inline limit and could not be saved \
             to your workspace, so it was discarded - none of its content is included here and \
             there is no file to read. If you still need the data, re-run with narrower \
             arguments (filter/limit) rather than repeating the same call.]"
        ),
    }
}

/// 全経路共通の無害化本体（redaction → 上限判定 → 退避 → メタ情報のみの案内）。
fn sanitize_tool_result(
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

    if !exceeds_limit(&result_json) {
        return result_json;
    }

    let saved_to = offload_to_workspace(&result_json, session_id, tool_call_id, workspace_root);
    oversized_notice(&result_json, saved_to.as_deref())
}

/// tool_result を永続化用の本文へ変換する（redaction → トークン上限/退避）。
///
/// - `workspace_root` が `Some` なら、上限超過分は `<root>/tmp/{session}_{tool_call_id}.json`
///   へ退避し、DB にはメタ情報（パス／バイト数／行数／推定トークン数）だけの案内を残す。
/// - `None`（退避先不明）や書き込み失敗時も**生データは残さない**。「保存できずに
///   捨てた」と分かるメタ情報だけを残す。session_logs の本文は次ターンで会話へ
///   再注入される＝ LLM が読むものなので、切り詰めた生データを置いても
///   [`sanitize_tool_result_for_llm`] と同じ害（先頭だけ見て判断する）になる。
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
    sanitize_tool_result(
        tool_name,
        result_json,
        session_id,
        tool_call_id,
        workspace_root,
    )
}

/// tool_result を **LLM へ返す本文**へ変換する（redaction → トークン上限/退避）。
///
/// 上限を超えたら**生データを 1 バイトも返さない**（#294）。返すのは
///
/// - 全文の保存先（ワークスペース相対パス）
/// - バイトサイズ・行数・推定トークン数
/// - 判別できたときだけ形式の手がかり
///
/// だけで、参照方法は LLM に委ねる（[`oversized_notice`] の doc 参照）。
/// 退避できなかった場合（`workspace_root` が `None` / 書き込み失敗）も同様で、
/// 「保存できず捨てた」と分かる案内だけを返す（黙って切らないし、生データも流さない）。
///
/// # 永続化側との関係
///
/// #294 以降、この関数と [`sanitize_tool_result_for_log`] は**同じ本文**を返す
/// （どちらも生データを持たないメタ情報のみ）。`SkillEngine` は capped 本文を
/// `Message::tool` と `on_tool_result` の両方へ渡すため、
/// 「同ターンで LLM が見た本文」＝「DB に残る本文」＝「次ターンに再注入される本文」
/// が常に一致する（#272 と同種の食い違いを構造的に防ぐ）。呼び分けは残しているが、
/// これは呼び出し側の意図を型名で示すためで、挙動差は無い。
pub fn sanitize_tool_result_for_llm(
    tool_name: &str,
    result_json: &str,
    session_id: &str,
    tool_call_id: &str,
    workspace_root: Option<&Path>,
) -> String {
    sanitize_tool_result(
        tool_name,
        result_json,
        session_id,
        tool_call_id,
        workspace_root,
    )
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

    /// 上限超過はワークスペースへ退避し、DB 本文はメタ情報だけになる。
    #[test]
    fn sanitize_offloads_large_result_to_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = format!(r#"{{"data":"{}"}}"#, "x ".repeat(TOOL_RESULT_TOKEN_LIMIT));
        let out = sanitize_tool_result_for_log("read_file", &big, "sess1", "tc9", Some(dir.path()));
        assert!(out.contains("tmp/sess1_tc9.json"), "{out}");
        assert!(
            !out.contains("x x x"),
            "生データが DB 本文に混ざっている: {out}"
        );
        let saved = std::fs::read_to_string(dir.path().join("tmp/sess1_tc9.json")).unwrap();
        assert_eq!(saved.len(), big.len());
    }

    /// 退避先が無くても生データは残さない（#294）。切り詰めた本文も session_logs へ
    /// 入れない — 次ターンで会話へ再注入され、結局 LLM が「先頭だけ」を読む。
    #[test]
    fn sanitize_keeps_no_raw_data_when_offload_is_impossible() {
        let big = format!(r#"{{"data":"{}"}}"#, "あ".repeat(TOOL_RESULT_TOKEN_LIMIT));
        let out = sanitize_tool_result_for_log("read_file", &big, "sess", "tc-1", None);
        assert!(!out.contains("あああ"), "生データが流れている: {out}");
        assert!(out.contains("could not be saved"), "{out}");
        assert!(out.contains("discarded"), "{out}");
    }

    /// tool_call_id にパス区切りが混ざってもワークスペースの外へ書かない（#284）。
    #[test]
    fn offload_sanitizes_path_components() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = format!(r#"{{"data":"{}"}}"#, "x ".repeat(TOOL_RESULT_TOKEN_LIMIT));
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

    /// #294 中核: 上限超過時、LLM へ渡る本文に**生データが 1 バイトも含まれない**。
    #[test]
    fn llm_result_contains_no_raw_data() {
        let dir = tempfile::TempDir::new().unwrap();
        // 実事故（#284）と同じ形の、979 人のフォロー一覧を模した結果。
        let entries: Vec<String> = (0..979)
            .map(|i| format!(r#"{{"npub":"npub1follower{i:04}","name":"user{i:04}"}}"#))
            .collect();
        let big = format!(r#"{{"success":true,"data":[{}]}}"#, entries.join(","));
        assert!(big.len() > 40_000, "前提が崩れている: {}", big.len());

        let out = sanitize_tool_result_for_llm(
            "nostr_get_following",
            &big,
            "sessA",
            "tc1",
            Some(dir.path()),
        );

        // 元データの特徴的な文字列は 1 つも出てこない（先頭の 1 件すら渡さない）。
        assert!(
            !out.contains("npub1follower0000"),
            "生データが流れている: {out}"
        );
        assert!(
            !out.contains("npub1follower"),
            "生データが流れている: {out}"
        );
        assert!(!out.contains("user0000"), "生データが流れている: {out}");
        // 案内はメタ情報＋狭めて取り直す導線だけで、1KB 未満に収まる（76KB → 数百バイト）。
        assert!(out.len() < 800, "案内が肥大している: {} bytes", out.len());

        // 全文は退避され、そこを指している。
        assert!(out.contains("tmp/sessA_tc1.json"), "{out}");
        let saved = std::fs::read_to_string(dir.path().join("tmp/sessA_tc1.json")).unwrap();
        assert_eq!(saved.len(), big.len());
    }

    /// 案内にはパス・バイトサイズ・行数・推定トークン数が載る（#294 のオーナー要求）。
    #[test]
    fn llm_notice_reports_path_bytes_lines_and_tokens() {
        let dir = tempfile::TempDir::new().unwrap();
        // 3 行（末尾改行なし）。
        let big = format!(
            "{}\n{}\n{}",
            "a ".repeat(2_000),
            "b ".repeat(2_000),
            "c ".repeat(2_000)
        );
        let out =
            sanitize_tool_result_for_llm("execute_shell", &big, "sessB", "tc2", Some(dir.path()));

        assert!(out.contains("tmp/sessB_tc2.json"), "パスが無い: {out}");
        assert!(
            out.contains(&format!("{} bytes", big.len())),
            "バイトサイズが無い: {out}"
        );
        assert!(out.contains("3 lines"), "行数が無い: {out}");
        assert!(
            out.contains(&format!("~{} tokens", crate::tokens::estimate_tokens(&big))),
            "推定トークン数が無い: {out}"
        );
        // 参照方法は選択肢として示すだけで強制しない。
        assert!(out.contains("up to you how to use it"), "{out}");
        // ループ防止の趣旨は残す（#284）。
        assert!(out.contains("Do NOT re-run the same tool"), "{out}");
    }

    /// 形式の手がかりは判別できたときだけ載せる（無理なら省く）。
    #[test]
    fn format_hint_is_best_effort() {
        assert_eq!(format_hint(r#"{"a":1}"#), Some("looks like a JSON object"));
        assert_eq!(format_hint("  [1,2,3]\n"), Some("looks like a JSON array"));
        assert_eq!(format_hint("plain text output"), None);
        assert_eq!(format_hint(""), None);
    }

    /// 行数の数え方: 末尾改行は空行を増やさない。空文字列は 0 行。
    #[test]
    fn line_counting_matches_head_and_editors() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("\n"), 1);
        assert_eq!(count_lines("a"), 1);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb"), 2);
        assert_eq!(count_lines("a\nb\n"), 2);
        assert_eq!(count_lines("a\n\nb\n"), 3);
    }

    /// 上限未満の結果は LLM 経路でも素通り（回帰防止）。
    #[test]
    fn llm_result_under_limit_is_untouched() {
        let json = r#"{"success":true,"data":{"ok":true},"error":null}"#;
        let out = sanitize_tool_result_for_llm("read_file", json, "sess", "tc-1", None);
        assert_eq!(out, json);
    }

    /// バイト数では上限を超えるが**トークン数では超えない**本文は素通りする（#294）。
    ///
    /// 旧実装（10,000 バイト上限）ならここで切られていた。日本語 1 文字 3 バイトの
    /// 本文は、バイトで測ると実効トークン量よりずっと大きく見える。
    #[test]
    fn japanese_text_is_measured_in_tokens_not_bytes() {
        // 4,500 バイト超（旧バイト上限の半分弱）だが 2,500 トークン未満。
        let json = format!(r#"{{"data":"{}"}}"#, "こんにちは世界".repeat(220));
        assert!(json.len() > 4_500, "前提が崩れている: {}", json.len());
        assert!(crate::tokens::estimate_tokens(&json) < TOOL_RESULT_TOKEN_LIMIT);
        let out = sanitize_tool_result_for_llm("read_file", &json, "sess", "tc-1", None);
        assert_eq!(out, json);
    }

    /// 退避できないときも生データを流さず、消えたことを LLM に伝える。
    #[test]
    fn llm_result_without_workspace_explains_the_data_is_gone() {
        let big = format!(r#"{{"data":"{}"}}"#, "あ".repeat(20_000));
        let out = sanitize_tool_result_for_llm("execute_shell", &big, "sess", "tc-1", None);
        assert!(!out.contains("あああ"), "生データが流れている: {out}");
        assert!(out.contains("could not be saved"), "{out}");
        assert!(out.contains("there is no file to read"), "{out}");
        assert!(out.contains("narrower arguments"), "{out}");
    }

    /// #286: 案内文が長くなっても（session_id / tool_call_id が長い）上限を超えない。
    ///
    /// 案内文が上限を食い破ると、永続化側の「上限未満なら素通り」を通過して
    /// LLM が見た本文と DB に残る本文が食い違う。
    #[test]
    fn llm_notice_with_long_ids_still_fits_the_limit() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = "q ".repeat(50_000);
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
            !exceeds_limit(&out),
            "案内文が枠を食い破っている: {} bytes",
            out.len()
        );
        // ID を切り詰めるのでファイル名長エラーにならず、ちゃんと退避できている。
        assert!(out.contains("tmp/ssss"), "{out}");
        assert_eq!(dir.path().join("tmp").read_dir().unwrap().count(), 1);
        // 永続化側を通しても no-op（＝ DB と LLM の本文が一致する）。
        let logged =
            sanitize_tool_result_for_log("read_file", &out, &long_session, &long_call_id, None);
        assert_eq!(logged, out);
    }

    /// LLM 経路と DB 経路は同じ本文を返す（#294 の invariant）。
    #[test]
    fn llm_and_log_bodies_agree() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = format!(r#"{{"data":"{}"}}"#, "w ".repeat(TOOL_RESULT_TOKEN_LIMIT));
        let llm = sanitize_tool_result_for_llm("read_file", &big, "sessC", "tc3", Some(dir.path()));
        let log = sanitize_tool_result_for_log("read_file", &big, "sessC", "tc3", Some(dir.path()));
        assert_eq!(llm, log);
    }

    /// LLM 経路でも秘密フィールドはマスクされる。
    #[test]
    fn llm_result_redacts_secret_tool() {
        let json = r#"{"success":true,"data":{"nsec":"nsec1secret"},"error":null}"#;
        let out = sanitize_tool_result_for_llm("nostr_generate_key", json, "sess", "tc-1", None);
        assert!(!out.contains("nsec1secret"));
    }
}
