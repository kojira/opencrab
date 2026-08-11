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

/// 秘密として扱う JSON フィールド名の集合（**唯一の定義**）。
///
/// マスク（[`redact_secrets_in_place`]）・検出（[`contains_secret`]）・各 sink 前のゲートは
/// すべてここを見る。秘密鍵の種類が増えたら**ここへ 1 行足すだけ**で、外部 sink・永続化・
/// LLM 経路のすべてに同時に反映される。個別経路が自前でキー名を書く（＝分岐が増えるたびに
/// 片方だけ弱くなる #519 の再発）余地を残さないための 1 源。
///
/// 過剰一般化はしない: 実在する秘密キーだけを列挙する（現状 `nsec` のみ）。
pub const SECRET_KEYS: &[&str] = &["nsec"];

fn is_secret_key(key: &str) -> bool {
    SECRET_KEYS.contains(&key)
}

/// JSON Value を**再帰的に**辿り、秘密フィールド（[`SECRET_KEYS`]）の値を `[redacted]` に
/// 潰す（in-place）。1 つでも潰したら `true`。
///
/// object・array の任意の深さを辿る。トップレベルだけを見る実装は、実際の tool 結果
/// （`{"success":..,"data":{"nsec":..},..}` のように秘密が **`data` の中**へネストする、
/// あるいは配列要素に入る）で素通りする（#519）。秘密のマスクはこの 1 実装に集約し、
/// 外部 sink（bridge）も永続化（本モジュール）も同じここを通す。
pub fn redact_secrets_in_place(value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(obj) => {
            let mut redacted = false;
            for (key, child) in obj.iter_mut() {
                if is_secret_key(key) {
                    *child = serde_json::Value::String("[redacted]".to_string());
                    redacted = true;
                } else {
                    redacted |= redact_secrets_in_place(child);
                }
            }
            redacted
        }
        serde_json::Value::Array(arr) => {
            let mut redacted = false;
            for child in arr.iter_mut() {
                redacted |= redact_secrets_in_place(child);
            }
            redacted
        }
        _ => false,
    }
}

/// Value のどこか（ネスト・配列含む）に秘密フィールド（[`SECRET_KEYS`]）があるか。
///
/// 外部 sink へ渡す前のゲートに使う。無ければ clone せず借用のまま流せる（大半の結果）。
/// 検出も [`redact_secrets_in_place`] と同じ [`SECRET_KEYS`] を見るので、判定基準が
/// マスク基準とズレない。
pub fn contains_secret(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(obj) => obj
            .iter()
            .any(|(key, child)| is_secret_key(key) || contains_secret(child)),
        serde_json::Value::Array(arr) => arr.iter().any(contains_secret),
        _ => false,
    }
}

/// sanitize パイプライン用の秘密マスク（**内容ベース／ツール名に依存しない**）。
///
/// 従来は `nostr_generate_key` 決め打ちだったが、秘密を返す 3 つ目の経路が「また自前で
/// ツール名を登録する」余地を残していた（#519 の構造問題 C）。いまは結果 JSON に秘密
/// フィールドがあれば**ツール名に関わらず**潰す。マスク本体は共通の
/// [`redact_secrets_in_place`]（1 源）。
///
/// 安価な前フィルタ: 秘密キー名の文字列すら含まなければ、秘密フィールドは存在し得ない
/// ので parse を省き、借用のまま返す（大半の結果はここで素通り＝無駄な clone/再直列化を
/// しない）。substring は一致したがキーとしては存在しない（値の中に `nsec` の語がある等）
/// 場合も、何も潰さないなら元の文字列をそのまま返す。非 JSON はここでは触らない
/// （オフロード案内文の再無害化などを壊さない）。
fn redact_secrets_in_result(result_json: &str) -> std::borrow::Cow<'_, str> {
    if !SECRET_KEYS.iter().any(|k| result_json.contains(k)) {
        return std::borrow::Cow::Borrowed(result_json);
    }
    // parse できたら再帰マスク。何か潰したときだけ Owned を返す。substring は一致したが
    // キーとしては無い（値の中に語がある等）／非 JSON の場合は原文を借用のまま返す
    // （再直列化しない）。
    if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(result_json) {
        if redact_secrets_in_place(&mut v) {
            return std::borrow::Cow::Owned(v.to_string());
        }
    }
    std::borrow::Cow::Borrowed(result_json)
}

/// 退避ファイル 1 件の最大バイト数（#568）。
///
/// # なぜ要るか
/// [`TOOL_RESULT_TOKEN_LIMIT`] は「inline に載せる量」を縛るだけで、「ディスクへ落とす量」は
/// 無制限だった。本番で `execute_shell` の再帰 grep が過去の退避ファイル（`tmp/` 配下）を
/// 巻き込んで読み、その結果がさらに退避される自己増幅で、単一 509,447,453 バイト（約 486MB）の
/// ファイルまで育っていた（1,598 ファイル計 ~1GB、上位 2 件で 69%）。退避先はバックアップも
/// 重くし、読み返そうとすると再び退避されて増える。
///
/// # 10MB の由来
/// 本番の退避ファイル実測で 10MB を超えるのは 1,598 件中 **7 件のみ**。正当な結果（curl の
/// HTML・検索一覧など）はほぼ 1MB 未満で、10MB は「病的に膨らんだ尾」だけを頭打ちにして正当な
/// 小物を 1 件も削らない値。inline 上限（2,500 トークン ≒ 数 KB）より桁で大きいので、「全文は
/// ファイルで読む」用途は保たれる。
///
/// # 何が失われるか
/// 上限超過時は**先頭 [`OFFLOAD_FILE_BYTE_LIMIT`] バイト（文字境界で丸め）だけ保存**し、末尾は
/// 捨てる。再帰 grep なら後半のヒットが消える。ただし inline には元から全文を出しておらず
/// （#294）、notice に元サイズと「切り詰めた」ことを明記し、全文が要るなら引数を絞って再実行
/// する導線も残すので前進はできる。**上限以下は全文保存で従来と 1 バイトも変わらない。**
const OFFLOAD_FILE_BYTE_LIMIT: usize = 10 * 1024 * 1024;

/// 退避の結果（#568）。保存できたときの相対パスと、切り詰めたかどうか。
struct OffloadResult {
    /// ワークスペース相対の保存先パス。
    rel_path: String,
    /// [`OFFLOAD_FILE_BYTE_LIMIT`] 超過で**先頭だけ**保存したときの、保存したバイト数。
    /// 全文を保存したなら `None`（このとき notice は従来と同一）。
    saved_prefix_bytes: Option<usize>,
}

/// 上限超過分をワークスペースへ退避する。成功したら保存先（切り詰め有無つき）を返す。
///
/// [`OFFLOAD_FILE_BYTE_LIMIT`] を超える結果は**先頭バイト（文字境界で丸め）だけ**保存し、
/// `saved_prefix_bytes` にその長さを載せる（#568）。上限以下は全文保存で従来と 1 バイトも
/// 変わらない（`saved_prefix_bytes = None`）。
fn offload_to_workspace(
    result_json: &str,
    session_id: &str,
    tool_call_id: &str,
    workspace_root: Option<&Path>,
) -> Option<OffloadResult> {
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
    // #568: ディスクへ落とす量にも上限を設ける。超過時は**文字境界で**先頭だけ残す
    // （バイト境界で切ると壊れた UTF-8 になる）。上限以下は全文をそのまま書く（no-op）。
    let (to_write, saved_prefix_bytes) = if result_json.len() > OFFLOAD_FILE_BYTE_LIMIT {
        let mut end = OFFLOAD_FILE_BYTE_LIMIT;
        while end > 0 && !result_json.is_char_boundary(end) {
            end -= 1;
        }
        (&result_json[..end], Some(end))
    } else {
        (result_json, None)
    };
    if std::fs::write(tmp_dir.join(&filename), to_write).is_ok() {
        Some(OffloadResult {
            rel_path: format!("tmp/{filename}"),
            saved_prefix_bytes,
        })
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
fn oversized_notice(result_json: &str, saved: Option<&OffloadResult>) -> String {
    let bytes = result_json.len();
    let lines = count_lines(result_json);
    // ここへ来る時点で `exceeds_limit` が encode 済み。ツール結果 1 件あたり
    // 2 回目の encode になるが、上限超過は稀（大半は早期 return する）。
    let tokens = crate::tokens::estimate_tokens(result_json);
    let hint = match format_hint(result_json) {
        Some(h) => format!(", {h}"),
        None => String::new(),
    };
    match saved {
        // 全文を保存できた（[`OFFLOAD_FILE_BYTE_LIMIT`] 以下）。従来と同一文面（no-op）。
        Some(OffloadResult {
            rel_path: rel,
            saved_prefix_bytes: None,
        }) => format!(
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
        // #568: 上限超過で**先頭だけ**保存した。元サイズと保存量を明記し、「不完全」であること・
        // 全文が要るなら引数を絞って再実行する導線を残す（discarded とは別の状態）。
        Some(OffloadResult {
            rel_path: rel,
            saved_prefix_bytes: Some(saved_bytes),
        }) => format!(
            "[Tool result withheld: {bytes} bytes, {lines} lines, ~{tokens} tokens{hint}. \
             It exceeded the {TOOL_RESULT_TOKEN_LIMIT}-token inline limit, so none of its \
             content is included here. The full result was {bytes} bytes; only the first \
             {saved_bytes} bytes were saved to `{rel}` (path relative to your workspace root) \
             to cap the offload file size - the rest was discarded, so the saved file is \
             incomplete (truncated). You can read, search, or transform that prefix, but it is \
             not the whole output. If you need the full result, re-run with a narrower request \
             (smaller id/time window, fewer rows, or estimate the size first) so it fits. Do \
             NOT re-run the same tool with the same arguments just to see the output again.]"
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
///
/// `tool_name` は呼び出し元の意図表示・将来の per-tool 方針のために残すが、redaction は
/// **内容ベース**（[`redact_secrets_in_result`]）でツール名を見ない。秘密を返す経路が
/// 増えても、ここでツール名を登録し直す必要はない（#519）。
fn sanitize_tool_result(
    tool_name: &str,
    result_json: &str,
    session_id: &str,
    tool_call_id: &str,
    workspace_root: Option<&Path>,
) -> String {
    let _ = tool_name;
    // 防御的マスク（defense-in-depth）。tool_result は後続ターンで会話へ再注入され、
    // 永続化もされるため、万一 nsec が混ざっていれば（どのツールでも）ここで潰す。
    let result_json = redact_secrets_in_result(result_json);

    if !exceeds_limit(&result_json) {
        return result_json.into_owned();
    }

    let saved = offload_to_workspace(&result_json, session_id, tool_call_id, workspace_root);
    oversized_notice(&result_json, saved.as_ref())
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

    /// #519: `data` の中へネストした `nsec` が、**ツール名に依存せず**潰れる。
    /// 従来の永続化ゲートは `nostr_generate_key` 決め打ちで、別ツールが同じ形の秘密を
    /// 返すと素通りしていた。内容ベースに揃えた回帰テスト。
    #[test]
    fn sanitize_redacts_nested_secret_for_any_tool() {
        let wrapper =
            r#"{"success":true,"data":{"npub":"npub1ok","nsec":"nsec1nestedleak"},"error":null}"#;
        // わざと「秘密ツール」ではないツール名で呼ぶ。
        let out = sanitize_tool_result_for_log("some_other_tool", wrapper, "sess", "tc-1", None);
        assert!(
            !out.contains("nsec1nestedleak"),
            "ネスト nsec が漏れた: {out}"
        );
        assert!(out.contains("[redacted]"));
        assert!(out.contains("npub1ok"), "非秘密は保持する");
    }

    /// #519: 配列の中にネストした `nsec` も潰れる。
    #[test]
    fn sanitize_redacts_secret_inside_array() {
        let wrapper =
            r#"{"success":true,"data":{"keys":[{"name":"a","nsec":"nsec1inarray"}]},"error":null}"#;
        let out = sanitize_tool_result_for_llm("bulk_tool", wrapper, "sess", "tc-1", None);
        assert!(!out.contains("nsec1inarray"), "配列内 nsec が漏れた: {out}");
        assert!(out.contains("[redacted]"));
    }

    /// #519: トップレベルの `nsec` は従来どおり潰れる。
    #[test]
    fn sanitize_redacts_top_level_secret() {
        let wrapper = r#"{"nsec":"nsec1toplevel","npub":"npub1y"}"#;
        let out = sanitize_tool_result_for_log("any_tool", wrapper, "sess", "tc-1", None);
        assert!(!out.contains("nsec1toplevel"), "{out}");
        assert!(out.contains("[redacted]"));
    }

    /// 秘密を含まない結果は**改変されない**（byte 一致）。前フィルタで parse すらしない。
    #[test]
    fn sanitize_leaves_secretless_result_byte_identical() {
        let json = r#"{"success":true,"data":{"npub":"npub1ok","note":"hello"},"error":null}"#;
        let out = sanitize_tool_result_for_log("any_tool", json, "sess", "tc-1", None);
        assert_eq!(out, json);
    }

    /// `nsec` が**値**として現れる（キーではない）だけの結果は、再直列化せず原文のまま。
    /// 無駄な clone/正規化（キー順の入れ替え等）をしない性質を固定する。
    #[test]
    fn sanitize_does_not_reserialize_when_nsec_only_appears_as_value() {
        let json = r#"{"data":{"text":"the nsec format starts with nsec1"},"error":null}"#;
        let out = sanitize_tool_result_for_log("any_tool", json, "sess", "tc-1", None);
        assert_eq!(out, json, "秘密キーが無いのに再直列化された: {out}");
        // Cow が借用のままであることも直接確認（clone していない）。
        assert!(matches!(
            redact_secrets_in_result(json),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    /// マスク本体: object/array を再帰的に辿り、潰したかどうかを返す。
    #[test]
    fn redact_secrets_in_place_is_recursive() {
        let mut v = serde_json::json!({
            "a": {"nsec": "nsec1x"},
            "b": [{"nsec": "nsec1y"}, {"ok": 1}],
            "c": "keep",
        });
        assert!(redact_secrets_in_place(&mut v));
        assert_eq!(v["a"]["nsec"], "[redacted]");
        assert_eq!(v["b"][0]["nsec"], "[redacted]");
        assert_eq!(v["c"], "keep");
        assert!(!v.to_string().contains("nsec1"));
        // 秘密が無ければ false（改変なし）。
        let mut clean = serde_json::json!({"npub": "npub1ok"});
        assert!(!redact_secrets_in_place(&mut clean));
    }

    /// 検出: 検出基準がマスク基準（[`SECRET_KEYS`]）とズレない。
    #[test]
    fn contains_secret_matches_redaction_scope() {
        assert!(contains_secret(&serde_json::json!({"data": {"nsec": "x"}})));
        assert!(contains_secret(&serde_json::json!([{"nsec": "x"}])));
        assert!(!contains_secret(
            &serde_json::json!({"data": {"npub": "x"}})
        ));
        // 値に nsec の語があるだけでは検出しない（キー名で判定）。
        assert!(!contains_secret(
            &serde_json::json!({"text": "nsec1 is a prefix"})
        ));
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

    /// #568: 退避ファイルが [`OFFLOAD_FILE_BYTE_LIMIT`] を超えたら**先頭だけ**保存し、
    /// 切り詰めは**文字境界**で行う（バイト境界で切ると壊れた UTF-8 になる）。
    #[test]
    fn offload_truncates_over_limit_at_char_boundary() {
        let dir = tempfile::TempDir::new().unwrap();
        // 上限の 1 バイト手前に 3 バイト文字 'あ' を跨がせる。バイト境界で切ると
        // 'あ' の途中で割れて壊れた UTF-8 になるが、文字境界で切れば 'あ' の手前で止まる。
        let big = format!(
            "{}あ{}",
            "a".repeat(OFFLOAD_FILE_BYTE_LIMIT - 1),
            "b".repeat(200)
        );
        assert!(big.len() > OFFLOAD_FILE_BYTE_LIMIT);

        let saved = offload_to_workspace(&big, "sessT", "tcT", Some(dir.path())).unwrap();
        assert_eq!(saved.rel_path, "tmp/sessT_tcT.json");
        // 'あ' の手前（文字境界）＝ LIMIT-1 バイトで丸められる。
        assert_eq!(saved.saved_prefix_bytes, Some(OFFLOAD_FILE_BYTE_LIMIT - 1));

        let on_disk = std::fs::read(dir.path().join("tmp/sessT_tcT.json")).unwrap();
        assert_eq!(on_disk.len(), OFFLOAD_FILE_BYTE_LIMIT - 1);
        assert!(on_disk.len() < big.len(), "切り詰められていない");
        // 壊れた UTF-8 になっていない（境界で切った）＝末尾の 'あ'/'b' は残らない。
        let as_str = std::str::from_utf8(&on_disk).expect("切り詰め後も妥当な UTF-8");
        assert!(
            !as_str.contains('あ') && !as_str.contains('b'),
            "上限超過分（末尾）が残っている"
        );
    }

    /// #568: 上限以下は全文保存で**1 バイトも変わらない**（no-op）。
    #[test]
    fn offload_under_limit_saves_full_unchanged() {
        let dir = tempfile::TempDir::new().unwrap();
        let content = "hello ".repeat(1000); // ~6KB、上限以下
        let saved = offload_to_workspace(&content, "sessU", "tcU", Some(dir.path())).unwrap();
        assert_eq!(
            saved.saved_prefix_bytes, None,
            "上限以下は切り詰めない（None）"
        );
        let on_disk = std::fs::read_to_string(dir.path().join("tmp/sessU_tcU.json")).unwrap();
        assert_eq!(on_disk, content, "上限以下は 1 バイトも変わらない");
    }

    /// #568: notice は「全文保存」と「切り詰め保存」を区別し、切り詰め時は元サイズ・保存量・
    /// truncated を明記する。「保存できなかった（discarded）」とは別の状態。
    #[test]
    fn oversized_notice_marks_truncation_vs_full() {
        // 全文保存（saved_prefix_bytes = None）: 従来文面。切り詰め表現は出ない。
        let full = OffloadResult {
            rel_path: "tmp/a.json".to_string(),
            saved_prefix_bytes: None,
        };
        let n_full = oversized_notice("original content", Some(&full));
        assert!(
            n_full.contains("The full output was saved to `tmp/a.json`"),
            "{n_full}"
        );
        assert!(
            !n_full.contains("only the first"),
            "全文保存で切り詰め表現が出ている: {n_full}"
        );

        // 切り詰め保存（saved_prefix_bytes = Some）: 元サイズ・保存量・truncated を明記。
        let trunc = OffloadResult {
            rel_path: "tmp/b.json".to_string(),
            saved_prefix_bytes: Some(123),
        };
        let original = "x".repeat(9999);
        let n_trunc = oversized_notice(&original, Some(&trunc));
        assert!(
            n_trunc.contains("only the first 123 bytes"),
            "保存量が無い: {n_trunc}"
        );
        assert!(
            n_trunc.contains(&format!("{} bytes", original.len())),
            "元サイズが無い: {n_trunc}"
        );
        assert!(
            n_trunc.contains("truncated"),
            "切り詰めの明記が無い: {n_trunc}"
        );
        assert!(
            n_trunc.contains("Do NOT re-run the same tool"),
            "ループ防止が無い: {n_trunc}"
        );
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
