use super::refs::{reply_target_of, ConversationRefs};

/// 長文イベントの切り詰め閾値（§9A.5）。タイムライン束ね（watch 車線）は 200 字、
/// 自分宛て（mention/reply 車線）は 2,000 字。origin は汎用 external_id の車線標識で判定する。
/// self 発話（origin なし）は切り詰めない。
pub(super) fn render_limit(origin: Option<&str>) -> Option<usize> {
    match origin {
        Some(o) if o.contains(":watch:") => Some(200),
        Some(_) => Some(2000),
        None => None,
    }
}

/// `limit` 字を超える本文を切り詰め、末尾に「…(全N字)」を付す（§9A.5）。resolve 案内は
/// 能力実装スライスで追加する。N は元本文の全文字数。
pub(super) fn truncate_body(content: &str, limit: usize) -> String {
    let total = content.chars().count();
    if total <= limit {
        return content.to_string();
    }
    let head: String = content.chars().take(limit).collect();
    format!("{head}…(全{total}字)")
}

/// 表示時の legacy メタ剥がし（§9A・row294b / row339）。会話組み立て時のみ適用し、保存データは
/// 書き換えない。受信転記の本文へ焼き込まれた種別ラベル行（`[… kind:N …]` 形。新形も旧
/// `from=…/target=…` 付きも）を落とす。種別ラベルは即時判定（受信側の内部処理）に使うが会話
/// 表示には不要（row294b: メンションとリプライは別物・表示にラベル不要）。core は transport を
/// 名指ししないので、行判定は汎用マーカー ` kind:<数字>` で行う（外部 origin の車線標識と同じく
/// 特定 SDK に依存しない）。
///
/// **本文（利用者・全話者の自由記述）はそのまま**（row339 裁定）。以前は残行に
/// `elide_raw_identifiers` を掛け bech32/64hex を短縮していたが、本文の識別子改変は「相手の発言の
/// 書き換え」＝情報破壊なので撤去した。識別子隠蔽は自前生成の構造部分（話者ラベル・u/e/c/s 参照・
/// tool 表示・spawn ack 等）に限定する。落とすのは構造ラベル行だけで、本文中の UUID/npub/64hex は
/// 原文のまま LLM へ渡す。長文の切り詰め（`…(全N字)`）は本文改変ではなく省略なので呼び出し側で維持。
pub(crate) fn strip_inbound_meta_for_display(content: &str) -> String {
    strip_meta_lines(content, |line| line.to_string())
}

/// 凍結 snapshot blob 専用の掃除（row295d）。単一ログ経路（[`strip_inbound_meta_for_display`]）に
/// 加えて、旧レンダリング由来の legacy 識別子—UUID（subtask/session）・`call_…`（tool call id）・
/// `"digest":"…"`（モデル不要な内部整合値）—も除去/短縮する。新形式（既に §9A・c/s 番号・→log:N）
/// には該当パターンが無いので無影響。単一ログの note 本文へは適用しない（利用者本文の過剰除去を避ける）。
///
/// row339: **マーカー無しの legacy blob 専用**。載せ替え前の歴史データは構造部分に生識別子が混在し
/// flat text で本文と区別できないため full スクラブを維持する。新規に凍結する snapshot は生成元の
/// 描画が既にクリーン（構造=u/e/c/s 短縮参照・本文=原文）なので [`FROZEN_SNAPSHOT_V2_MARKER`] を
/// 付けてこのスクラブをスキップする（[`restore_frozen_snapshot`]）。世代ゲートで「本文原文」裁定が
/// compaction 後も破れないようにする。
pub(crate) fn strip_frozen_snapshot(content: &str) -> String {
    strip_meta_lines(content, scrub_identifiers_for_display)
}

/// row339: 新規に凍結する snapshot の先頭に付ける世代マーカー。制御文字始まりで実会話行
/// （`[話者]…`）や legacy blob と衝突しない。付いていれば生成元がクリーンな §9A 描画だと分かる。
pub(crate) const FROZEN_SNAPSHOT_V2_MARKER: &str = "\u{1}oc-snapshot-v2";

/// 凍結時に [`FROZEN_SNAPSHOT_V2_MARKER`] を先頭付与する（永続化直前・[`persist_snapshot`]）。
pub(crate) fn frozen_snapshot_with_marker(text: &str) -> String {
    format!("{FROZEN_SNAPSHOT_V2_MARKER}\n{text}")
}

/// read 時に凍結 snapshot blob を世代ゲートして復元する（row339）。
/// - v2（マーカー付き）: 生成元がクリーンなのでスクラブせず**本文原文のまま**復元する
///   （本文中の UUID/npub/64hex を再マスクしない）。
/// - legacy（マーカー無し・載せ替え前の歴史データ）: 従来どおり [`strip_frozen_snapshot`] でスクラブ。
pub(crate) fn restore_frozen_snapshot(blob: &str) -> String {
    match blob.strip_prefix(FROZEN_SNAPSHOT_V2_MARKER) {
        Some(rest) => rest.strip_prefix('\n').unwrap_or(rest).to_string(),
        None => strip_frozen_snapshot(blob),
    }
}

/// 生の長識別子（UUID / `call_…` / `"digest":"…"` / bech32 / 32hex 以上）を短縮形へ落とす共通変換。
/// **single source**: 凍結 snapshot の per-line 掃除・検知器・tool_result 本文表示（失敗本文の生 hex
/// を含む）が同じ規則を共有する（[[single-source-of-truth-no-parallel-paths]]）。char 単位なので
/// 単一行にも複数行本文にも同じく効く。会話に載る短縮参照（u/e/c/s 番号・`→log:N`）や普通の語は不変。
pub(crate) fn scrub_identifiers_for_display(text: &str) -> String {
    elide_raw_identifiers(&clean_legacy_ids(text))
}

/// **検知器**（row318）: 新規 delta 描画行に生の長識別子（UUID / `call_…` / bech32 / 32hex 以上 /
/// `"digest":"…"`）が残っていないかを見る。残っていれば「描画器が短縮形を出し損ねた＝バグ」なので
/// その行を返す（呼び出し側が WARN する・fail-loud）。スクラブ（[`strip_frozen_snapshot`]）は凍結
/// snapshot blob 専用で、delta 行はここで**置換せず検知だけ**する（本番に `<uuid…>` の無意味な
/// プレースホルダを出さない）。正しく描画できていれば常に `None`。
pub(crate) fn leaked_identifier_in_delta(rendered: &str) -> Option<String> {
    for line in rendered.split('\n') {
        if scrub_identifiers_for_display(line) != line {
            return Some(line.to_string());
        }
    }
    None
}

/// **単一ログ描画**に対する漏れ検知（row318・#847）。検知器の目的は**描画器のバグ**を鳴らすこと
/// ＝構造部分（話者行・tool_call・spawn ack など）に生の長識別子が残っていないかを見る。
///
/// speech の**本文**は利用者の自由記述で、表示時は**原文のまま**渡す（row339 裁定・本文の識別子
/// 改変は相手の発言の書き換え＝情報破壊。構造ラベル行のみ落とす・[`strip_inbound_meta_for_display`]）。
/// この本文を full スクラブ基準の [`leaked_identifier_in_delta`] で見ると、利用者が発話に UUID/npub/
/// 64hex 等を書いた瞬間に、描画が正しくても WARN が出る（#847 の偽陽性）。偽 WARN はアラート疲労で
/// 本物の描画器バグ WARN をマスクするので、speech は**構造ヘッダ行だけ**を検知対象にし、本文は見ない。描画形は
/// `[話者]…:\n本文…` で、ヘッダ（話者/時刻/e番号/関係注記）に改行は無いため 1 行目がヘッダ。
///
/// tool_call の**引数本文**（未決着 call / preserve_arg_call_ids の DI reply 本文）も同じ性質——
/// 表示時にスクラブせず verbatim 保持する（reply 本文が次ターンで消えない・§9A.1/row292）ので、
/// bech32/64hex を含む reply 引数で speech 本文と同種の偽陽性が出る（#847 follow-up）。call 行
/// `[cN]: name(args)` の args（最初の `(` 以降）を外し、構造（話者行・call_ref `[cN]`/`[id=call_…]`・
/// tool 名）だけを検知対象にする。tool_result/spawn ack は全行が構造（本文は既に短縮/s 番号）なので
/// 従来どおり全体を見る。
///
/// 「本物の描画器バグ（構造部分に生 ID が残る）は検知し続ける」を維持する: speech のヘッダ行も
/// tool_call の call_ref も full スクラブ基準で見るので、名前引き失敗の生 agent UUID・未採番の
/// `id=call_…` フォールバックは従来どおり鳴る。引数本文を外しても真陽性は落ちない——描画器が短縮
/// すべき生 ID は call_ref/subtask 参照側に出るのであって、元から verbatim 保持の引数本文内には無い。
pub(crate) fn leaked_identifier_in_render(
    log: &opencrab_db::queries::SessionLogRow,
    rendered: &str,
) -> Option<String> {
    match log.log_type.as_str() {
        "speech" => {
            let header = rendered.split('\n').next().unwrap_or(rendered);
            leaked_identifier_in_delta(header)
        }
        "tool_call" => {
            for line in rendered.split('\n') {
                // call 行（`]: ` を含む）だけ args を外す。fallback 本文行など call 行以外は全文を見る。
                let structural = match line.find("]: ").and(line.find('(')) {
                    Some(open) => &line[..open],
                    None => line,
                };
                if scrub_identifiers_for_display(structural) != structural {
                    return Some(line.to_string());
                }
            }
            None
        }
        _ => leaked_identifier_in_delta(rendered),
    }
}

/// メタ行を落とし、残る各行に `per_line` を適用し、末尾空行を畳む共通ルーチン。
fn strip_meta_lines(content: &str, per_line: impl Fn(&str) -> String) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in content.split('\n') {
        // メタ行（`[… kind:1 …]`）を落とす（新形 / 旧 from=/target= 付きの両方）。
        if is_inbound_meta_line(line.trim()) {
            continue;
        }
        lines.push(per_line(line));
    }
    // メタ行を落とした跡の末尾空行を畳む。
    while lines.last().is_some_and(|s| s.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// snapshot blob の legacy 識別子（UUID / `call_…` / `"digest":"…"`）を除去/短縮する（row295d）。
fn clean_legacy_ids(line: &str) -> String {
    let line = elide_uuids(line);
    let line = elide_call_ids(&line);
    elide_digest_values(&line)
}

/// UUID（`8-4-4-4-12` hex）を `<uuid…>` へ。64hex（ダッシュ無し）はここでは当たらず elide_raw が担う。
fn elide_uuids(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(len) = uuid_len_at(&bytes[i..]) {
            out.push_str("<uuid…>");
            i += len;
        } else {
            let ch = line[i..].chars().next().expect("char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// 位置 0 から UUID（`8-4-4-4-12` hex）なら消費バイト長（36）を返す。前後境界も見る。
fn uuid_len_at(b: &[u8]) -> Option<usize> {
    let groups = [8usize, 4, 4, 4, 12];
    let mut pos = 0;
    for (gi, &g) in groups.iter().enumerate() {
        if gi > 0 {
            if b.get(pos) != Some(&b'-') {
                return None;
            }
            pos += 1;
        }
        for _ in 0..g {
            match b.get(pos) {
                Some(c) if c.is_ascii_hexdigit() => pos += 1,
                _ => return None,
            }
        }
    }
    // 直後が英数なら、より長い hex トークンの一部＝UUID ではない。ダッシュは許す
    // （`nostr-<uuid>-<channel>` のような dashed session id 内に埋まった UUID も剥がす・row295d 変種）。
    if matches!(b.get(pos), Some(c) if c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(pos)
}

/// `call_<英数16+>`（tool call id）を `<call…>` へ短縮。
fn elide_call_ids(line: &str) -> String {
    const NEEDLE: &str = "call_";
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(pos) = rest.find(NEEDLE) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + NEEDLE.len()..];
        let n = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .count();
        if n >= 16 {
            out.push_str("<call…>");
            rest = &after[n..];
        } else {
            out.push_str(NEEDLE);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// `"digest":"<hex>"` の値を `…` へ（モデル不要な内部整合値・row295d）。
fn elide_digest_values(line: &str) -> String {
    const NEEDLE: &str = "\"digest\":\"";
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(pos) = rest.find(NEEDLE) {
        out.push_str(&rest[..pos + NEEDLE.len()]);
        let after = &rest[pos + NEEDLE.len()..];
        let end = after.find('"').unwrap_or(after.len());
        let val = &after[..end];
        if !val.is_empty() && val.bytes().all(|b| b.is_ascii_hexdigit()) {
            out.push('…');
        } else {
            out.push_str(val);
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// `[… kind:<数字> …]` 形の受信メタ行か（transport 非依存の汎用判定）。
fn is_inbound_meta_line(trimmed: &str) -> bool {
    const MARKER: &str = " kind:";
    if !(trimmed.starts_with('[') && trimmed.ends_with(']')) {
        return false;
    }
    match trimmed.find(MARKER) {
        Some(idx) => trimmed[idx + MARKER.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// 受信メタ行 `[… kind:N ラベル …]` から会話ヘッダの関係注記を作る（row295c）。
///
/// kind ラベル全廃で「そもそもリアクション/リプライか」が失われた欠陥への対処。リプライ/
/// リアクション/リポストだけ種別を残す（素の投稿・メンション・DM・長文は注記なし）。対象ノートは
/// 受信メタ `reply_target`（row295c 6b で記録）→ 会話内 e 番号へ解決。会話に無い（旧行の未記録・
/// 窓外）対象は `→外部`。
pub(super) fn inbound_relation_annotation(
    log: &opencrab_db::queries::SessionLogRow,
    refs: &ConversationRefs,
) -> Option<String> {
    let label = log
        .content
        .split('\n')
        .map(str::trim)
        .find(|l| is_inbound_meta_line(l))
        .and_then(meta_line_label)?;
    let relation = match label {
        "リプライ" => "reply",
        "リアクション" => "reaction",
        "リポスト" => "repost",
        _ => return None,
    };
    // 対象ノートは受信メタの reply_target（row295c 6b）→ 会話内 e 番号。会話に無い（旧行の
    // 未記録・窓外）対象は `→外部`。
    let target = reply_target_of(log)
        .and_then(|t| refs.event_num_by_id(&t))
        .map(|n| format!("e{n}"))
        .unwrap_or_else(|| "外部".to_string());
    Some(format!("({relation}→{target})"))
}

/// エージェント**自身の発話クラス** op（reply/reaction/repost）の speech ログに関係注記を付ける
/// （DESIGN-RESUME-SETTLE §3.3.1 C6・発話クラス化）。撃ちっぱなし配送で永続した発話は
/// tool_call/tool_result の機械行を持たず「本文＋関係注記」だけで残る。注記の種別は metadata の
/// `utterance_kind`、対象ノートは `reply_target`（64hex）→ 会話内 e 番号（無ければ `→外部`）。
/// 受信転記（`inbound_relation_annotation`）とは metadata 由来かで区別し、二重付与しない。
pub(super) fn outgoing_relation_annotation(
    log: &opencrab_db::queries::SessionLogRow,
    refs: &ConversationRefs,
) -> Option<String> {
    let meta: serde_json::Value = serde_json::from_str(log.metadata_json.as_deref()?).ok()?;
    let kind = meta.get("utterance_kind").and_then(|v| v.as_str())?;
    let relation = match kind {
        "reply" => "reply",
        "reaction" => "reaction",
        "repost" => "repost",
        _ => return None,
    };
    let target = reply_target_of(log)
        .and_then(|t| refs.event_num_by_id(&t))
        .map(|n| format!("e{n}"))
        .unwrap_or_else(|| "外部".to_string());
    Some(format!("({relation}→{target})"))
}

/// `[… kind:<数字> <ラベル> …]` からラベル語だけを取り出す（新形も旧 from=/target= 付きも）。
fn meta_line_label(trimmed: &str) -> Option<&str> {
    const MARKER: &str = " kind:";
    let idx = trimmed.find(MARKER)?;
    let after = trimmed[idx + MARKER.len()..].trim_start_matches(|c: char| c.is_ascii_digit());
    let after = after.strip_prefix(' ')?;
    let end = after.find([' ', ']']).unwrap_or(after.len());
    Some(&after[..end])
}

/// 生の長い識別子の bech32 HRP。長い順に試す（`nprofile1` が `npub1` より先）。
const BECH32_HRPS: &[&str] = &["nprofile1", "nevent1", "naddr1", "npub1", "note1", "nsec1"];

/// 行内の生の長い識別子（bech32・64hex）を短縮する。英数の連続を 1 トークンとして境界で切り、
/// 通常語や短い hash は温存する。
fn elide_raw_identifiers(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut token = String::new();
    for ch in line.chars() {
        if ch.is_ascii_alphanumeric() {
            token.push(ch);
        } else {
            if !token.is_empty() {
                out.push_str(&elide_identifier_token(&token));
                token.clear();
            }
            out.push(ch);
        }
    }
    if !token.is_empty() {
        out.push_str(&elide_identifier_token(&token));
    }
    out
}

fn elide_identifier_token(tok: &str) -> String {
    for hrp in BECH32_HRPS {
        if let Some(body) = tok.strip_prefix(hrp) {
            if body.len() >= 30
                && body
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            {
                return format!("<{}…>", hrp.trim_end_matches('1'));
            }
        }
    }
    // 生 hex 識別子（pubkey/event_id=64hex・ダッシュ無し UUID/subtask=32hex 等）。短い hash や
    // git short-sha を巻き込まないよう 32 桁以上に限る（row318: 32hex 変種も長物ゼロの対象）。
    if tok.len() >= 32
        && tok
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    {
        return "<id…>".to_string();
    }
    tok.to_string()
}
