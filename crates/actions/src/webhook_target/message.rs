/// spawn 時に渡される webhook 設定（最小形）。
#[derive(Clone, Debug, PartialEq)]
pub struct WebhookConfig {
    pub url: String,
    /// 送信対象イベント名。None の場合は全イベントを送る。
    pub events: Option<Vec<String>>,
}

impl WebhookConfig {
    /// spawn_subtask の引数から webhook 設定を取り出す。
    ///
    /// 期待する最小 JSON 形:
    /// ```json
    /// { "webhook": { "url": "https://...", "events": ["started", "completed"] } }
    /// ```
    /// `events` は省略可能。`url` が無い / 空 / 空白のみなら「明示指定なし」として
    /// None を返す（呼び出し側はデフォルトへフォールバックできる）。
    pub fn from_args(args: &serde_json::Value) -> Option<WebhookConfig> {
        let wh = args.get("webhook")?;
        let url = wh.get("url").and_then(|v| v.as_str())?.to_string();
        if url.trim().is_empty() {
            return None;
        }
        let events = wh.get("events").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        });
        Some(WebhookConfig { url, events })
    }

    pub fn from_parts(url: String, events: Option<Vec<String>>) -> Option<WebhookConfig> {
        if url.trim().is_empty() {
            return None;
        }
        Some(WebhookConfig { url, events })
    }

    /// 指定イベントを送るべきか。events 未指定なら常に true。
    pub fn wants(&self, event: &str) -> bool {
        match &self.events {
            Some(list) => {
                // 比較は canonical な status 名で行う。depth0 sink は
                // `tool_call_started`/`tool_call_completed`/... を、subtask path は
                // `subtask.started`/`started`/... を渡してくるため、両辺を正規化して
                // 同じ語彙（started/completed/failed/rejected/...）で突き合わせる。
                let want = normalize_event_name(event);
                if list.iter().any(|e| normalize_event_name(e) == want) {
                    return true;
                }
                // Backward compatibility for callers that created lifecycle streams before
                // progress existed: started/completed streams should include tool progress too.
                want == "progress" && list.iter().any(|e| normalize_event_name(e) == "started")
            }
            None => true,
        }
    }
}

/// イベント名を canonical な status 名へ正規化する。
/// `subtask.` 接頭辞（subtask lifecycle）と `tool_call_` 接頭辞（depth0 tool sink）を剥がし、
/// `started`/`completed`/`failed`/`rejected`/`timed_out`/`progress` 等の素の status に揃える。
fn normalize_event_name(event: &str) -> &str {
    event
        .strip_prefix("subtask.")
        .or_else(|| event.strip_prefix("tool_call_"))
        .unwrap_or(event)
}

/// raw text を char 単位で limit 以下の chunk に分割する（UTF-8 境界を壊さない）。
pub fn chunk_text(text: &str, limit: usize) -> Vec<String> {
    if text.is_empty() || limit == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(limit)
        .map(|c| c.iter().collect::<String>())
        .collect()
}

/// raw text を `part X/N` 付きメッセージ列に整形する。
///
/// この framing はピアレビュー等で「part X/N の生データを読め」という
/// プロンプト規約とセットのプロトコルなので、変更時は全利用箇所と
/// system prompt（server/process.rs）を同時に更新すること。
pub fn build_part_messages(content: &str, limit: usize) -> Vec<String> {
    let chunks = chunk_text(content, limit);
    let part_count = chunks.len();
    chunks
        .iter()
        .enumerate()
        .map(|(i, c)| format!("part {}/{}\n{}", i + 1, part_count, c))
        .collect()
}

// ---- 長文の「出だしプレビュー + 全文ファイル添付」（#293） ----
//
// ## なぜ分割連投をやめたか
//
// 従来は長文を [`chunk_text`] で 1900〜2000 文字ごとに割り、`part X/N` を付けて
// **複数メッセージとして連投**していた。これは
//
// - Discord のチャンネルが同一内容の断片で埋まって読みづらい
// - 1 論理メッセージあたり N 回 POST するため **Discord のレート制限（429）に当たり
//   やすい**（リトライで大量に飛んだ実例あり）
// - 断片が別メッセージなので**全文をコピーしづらい**
//
// という 3 つの問題を抱えていた。そこで **1 通の multipart 送信**（出だしのプレビュー
// テキスト + 全文を添付ファイル）へ切り替える。送信回数は長さに依らず常に 1 回。
//
// この module は **ポリシー（どこで切るか・何を添えるか）だけ**を持ち、HTTP は持たない
// （module doc の依存方針どおり）。実際の multipart POST は transport 側
// （`crates/discord/src/gateway_actions/webhook.rs` と Nostr 転記の送信箇所）にある。

/// これを **超えたら**添付方式へ切り替える文字数。
///
/// 値は Discord の 1 メッセージ上限そのもの（2000 文字）。つまり「1 通に収まるものは
/// 従来どおり素のテキスト 1 通、収まらないものだけ添付」という単純な境界で、**短い
/// メッセージの見え方・送られ方は一切変わらない**（JSON 1 本のまま。回帰しない）。
pub const ATTACHMENT_THRESHOLD_CHARS: usize = 2000;

/// 添付に切り替えたとき、本文に載せる出だしプレビューの文字数。
///
/// 「数百文字」＝Discord で 5〜10 行程度、スクロールせずに要旨が掴める量。プレビュー
/// + 案内文を足しても 2000 文字上限に対して十分な余裕がある（案内文は 150 文字程度）。
pub const ATTACHMENT_PREVIEW_CHARS: usize = 600;

/// 添付ファイルの最大バイト数（これを超える分は切り詰めて明示する）。
///
/// Discord の無課金サーバのアップロード上限は 25 MiB だが、その 1/3 弱の 8 MiB で頭打ち
/// にする。理由は 2 つ:
/// 1. boost 状況に依らず確実に通る安全側の値であること。
/// 2. **1 回の multipart POST が長時間化しないこと**。配送は spawn 済みタスク内とはいえ、
///    巨大ボディの送信が延々と続くとその run の後続配送が詰まる。切り詰めは**送信前**に
///    行う（ここ）。
pub const ATTACHMENT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// 添付ファイルの content_type。全文はプレーンテキストなので固定。
pub const ATTACHMENT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

/// 添付ファイル名の最大長（拡張子込み）。
const ATTACHMENT_SLUG_MAX: usize = 48;

/// webhook に添える 1 ファイル。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookAttachment {
    /// 送信時のファイル名。**秘密・個人情報を含めない**（[`attachment_filename`] 参照）。
    pub filename: String,
    pub content_type: String,
    /// ファイル本体。`content` と**同一の文字列**から作る（マスク済みの本文がそのまま
    /// 添付になり、添付側だけ生の秘密が漏れることが構造的に起きない）。
    pub data: Vec<u8>,
    /// [`ATTACHMENT_MAX_BYTES`] を超えたため末尾を落としたか。
    pub truncated: bool,
}

/// webhook へ送る 1 通（本文 + 任意の添付）。
///
/// 添付が `None` なら従来どおり JSON 1 本で送る。`Some` なら multipart 1 本で送る。
/// どちらも **1 通 = 1 POST**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookMessage {
    pub content: String,
    pub attachment: Option<WebhookAttachment>,
}

impl WebhookMessage {
    /// 添付なしの素のテキスト 1 通。
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            attachment: None,
        }
    }

    pub fn has_attachment(&self) -> bool {
        self.attachment.is_some()
    }

    /// この 1 通で受け手に届く**全文**。添付があれば添付本体（＝プレビュー元の全文）、
    /// 無ければ本文そのもの。「分割されていた頃の part を連結した文字列」に相当する
    /// ので、検証や表示でロスの有無を見るときはこちらを使う。
    pub fn delivered_text(&self) -> String {
        match &self.attachment {
            Some(att) => String::from_utf8_lossy(&att.data).into_owned(),
            None => self.content.clone(),
        }
    }
}

impl From<String> for WebhookMessage {
    fn from(s: String) -> Self {
        WebhookMessage::text(s)
    }
}

impl From<&str> for WebhookMessage {
    fn from(s: &str) -> Self {
        WebhookMessage::text(s)
    }
}

/// slug から安全な添付ファイル名（`<slug>.txt`）を作る。
///
/// **秘密・個人情報をファイル名に載せない**ため、呼び出し側には「イベント名 / ツール名 /
/// 用途」といった静的な語彙だけを渡させ、ここで更に ASCII 英数と `-` `_` `.` 以外を
/// `-` へ潰し、長さも切り詰める（ユーザ名・URL・トークンが混ざっても素通ししない）。
/// 空になった場合は `output.txt` にフォールバックする。
pub fn attachment_filename(slug: &str) -> String {
    let cleaned: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(['-', '.']).to_string();
    let stem: String = trimmed.chars().take(ATTACHMENT_SLUG_MAX).collect();
    if stem.is_empty() {
        "output.txt".to_string()
    } else {
        format!("{stem}.txt")
    }
}

/// `text` を [`ATTACHMENT_MAX_BYTES`] 以下のバイト列にする（UTF-8 境界を壊さない）。
/// 切り詰めた場合は末尾に省略マーカーを足し、`truncated = true` を返す。
fn attachment_bytes(text: &str) -> (Vec<u8>, bool) {
    if text.len() <= ATTACHMENT_MAX_BYTES {
        return (text.as_bytes().to_vec(), false);
    }
    const MARKER: &str =
        "\n\n--- [truncated] the rest was omitted because the attachment hit the size cap ---\n";
    let budget = ATTACHMENT_MAX_BYTES.saturating_sub(MARKER.len());
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + MARKER.len());
    out.push_str(&text[..end]);
    out.push_str(MARKER);
    (out.into_bytes(), true)
}

/// 長文を「出だしのプレビュー + 全文添付」の **1 通**に畳む。
///
/// - [`ATTACHMENT_THRESHOLD_CHARS`] 以下なら添付せず、`text` をそのまま本文にする
///   （＝従来どおり JSON 1 本で飛ぶ。回帰なし）。
/// - 超えたら先頭 [`ATTACHMENT_PREVIEW_CHARS`] 文字だけを本文に載せ、**全文をそのまま**
///   添付する。添付本体は本文と同じ `text` から作るので、呼び出し前に掛かっている
///   マスク（nostr の `nsec` マスク等）は添付側にもそのまま効く。
///
/// `slug` は追跡用のファイル名の素。静的な語彙のみを渡すこと
/// （[`attachment_filename`] が更にサニタイズする）。
pub fn build_message_with_optional_attachment(text: &str, slug: &str) -> WebhookMessage {
    build_message_with_attachment_preview(text, slug, ATTACHMENT_PREVIEW_CHARS)
}

/// [`build_message_with_optional_attachment`] のプレビュー長を指定できる版。
///
/// `preview_chars` は [`ATTACHMENT_PREVIEW_CHARS`] を上限とするクランプで、既定より
/// **短く**したいとき（webhook 設定の `max_chars` 等）に使う。0 は既定扱い。
pub fn build_message_with_attachment_preview(
    text: &str,
    slug: &str,
    preview_chars: usize,
) -> WebhookMessage {
    let total_chars = text.chars().count();
    if total_chars <= ATTACHMENT_THRESHOLD_CHARS {
        return WebhookMessage::text(text);
    }
    let preview_chars = if preview_chars == 0 {
        ATTACHMENT_PREVIEW_CHARS
    } else {
        preview_chars.min(ATTACHMENT_PREVIEW_CHARS)
    };
    let filename = attachment_filename(slug);
    let (data, truncated) = attachment_bytes(text);
    let preview: String = text.chars().take(preview_chars).collect();
    let note = if truncated {
        format!(
            "\n…\n\n📎 full text attached as `{filename}` ({total_chars} chars; the file was \
             truncated at {ATTACHMENT_MAX_BYTES} bytes and the tail is omitted)"
        )
    } else {
        format!("\n…\n\n📎 full text attached as `{filename}` ({total_chars} chars)")
    };
    WebhookMessage {
        content: format!("{preview}{note}"),
        attachment: Some(WebhookAttachment {
            filename,
            content_type: ATTACHMENT_CONTENT_TYPE.to_string(),
            data,
            truncated,
        }),
    }
}

/// Discord webhook の JSON body を組む。multipart のときは同じ JSON を `payload_json`
/// パートに載せる（Discord webhook の仕様）。
///
/// `suppress_mentions` は [`build_relay_webhook_body`] の doc を参照。
pub fn build_webhook_body(content: &str, suppress_mentions: bool) -> serde_json::Value {
    if suppress_mentions {
        json!({
            "content": content,
            "allowed_mentions": { "parse": [] },
        })
    } else {
        json!({ "content": content })
    }
}

/// Nostr 受信転記の Discord webhook POST body を組む（issue #252 段階 A）。
///
/// **必ず `allowed_mentions: { "parse": [] }` を乗せる**。Discord webhook は
/// `allowed_mentions` 省略時に content 内のメンション（`@everyone` / `@here` /
/// `<@userid>` / `<@&roleid>`）を全解決して通知を飛ばす。転記対象は第三者が送れる
/// 「エージェント宛の受信イベント」なので、抑止しないと第三者が `@everyone` を
/// 含めるだけで転記先サーバ全員へ通知が飛ぶ（mention 暴発）。空の parse 配列で
/// 全種別の解決を Discord 側で止める。
pub fn build_relay_webhook_body(chunk: &str) -> serde_json::Value {
    build_webhook_body(chunk, true)
}

/// webhook URL のトークン（末尾セグメント）をマスクして返す。ログ・応答用。
pub fn redact_webhook_url(url: &str) -> String {
    match url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/[redacted]"),
        None => "[redacted]".to_string(),
    }
}

// ---- Secret redaction (retained utility) ----
//
// 本設計（docs/design-webhook-output-lossless.md §2 P4）により、covered 経路
// （work-channel 出力: command/stdout/stderr/args/result）からは redaction を完全に外した。
// 以下の関数群はもはや配送経路では呼ばれないが、covered 経路外（別タスク・§8）で再利用しうる
// 汎用ユーティリティとして残す。

const REDACTED: &str = "[REDACTED]";
const SECRET_PREFIXES: [&str; 4] = ["sk-", "ghp_", "xoxb-", "AKIA"];
const KV_MARKERS: [&str; 5] = ["TOKEN", "SECRET", "PASSWORD", "KEY", "API"];

/// 既知のシークレットパターンを [REDACTED] に置換する汎用ユーティリティ。
/// 取りこぼし対策として保守的に倒す（長い base64/hex 連や Bearer トークンも redact）。
/// 冪等: 既に redact 済みの文字列を再度通しても安全。
/// 注: covered 経路（webhook 出力）では **呼ばない**（§2 P4）。
pub fn redact_secrets(input: &str) -> String {
    input
        .split('\n')
        .map(redact_secrets_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_secrets_line(line: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut redact_next = false;
    for tok in line.split_whitespace() {
        if redact_next {
            out.push(REDACTED.to_string());
            redact_next = false;
            continue;
        }
        if tok.eq_ignore_ascii_case("bearer") {
            out.push(tok.to_string());
            redact_next = true;
            continue;
        }
        let (rendered, want_next) = redact_secret_token(tok);
        out.push(rendered);
        redact_next = want_next;
    }
    out.join(" ")
}

/// 1 トークンを検査し、(置換後文字列, 次トークンも redact すべきか) を返す。
fn redact_secret_token(tok: &str) -> (String, bool) {
    let core = tok.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | ',' | ';' | '(' | ')' | '`' | '[' | ']' | '{' | '}'
        )
    });
    if core.is_empty() {
        return (tok.to_string(), false);
    }
    // Discord webhook URL（ホスト不問）
    if core.contains("/api/webhooks/") {
        return (REDACTED.to_string(), false);
    }
    // KEY=VALUE / KEY:VALUE （キーに TOKEN/SECRET/PASSWORD/KEY/API を含む）
    if let Some(idx) = core.find(['=', ':']) {
        let (k, rest) = core.split_at(idx);
        let delim = &core[idx..idx + 1];
        let value = &rest[1..];
        let key_up = k.trim_matches('"').to_ascii_uppercase();
        if KV_MARKERS.iter().any(|m| key_up.contains(m)) {
            if value.trim().is_empty() {
                // 値は次トークン側にある（例: `"token": "abc"`）
                return (tok.to_string(), true);
            }
            return (format!("{k}{delim}{REDACTED}"), false);
        }
    }
    // 既知プレフィックス
    for p in SECRET_PREFIXES {
        if core.starts_with(p) && core.len() > p.len() + 3 {
            return (REDACTED.to_string(), false);
        }
    }
    // 長い base64 / hex 連
    if core.len() >= 32
        && core
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-'))
    {
        return (REDACTED.to_string(), false);
    }
    (tok.to_string(), false)
}

/// Discord webhook URL を検証する。空・パース不可・Discord webhook でない場合は Err(理由)。
///
/// 理由文字列に raw URL は含めない。
pub fn validate_webhook_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("url is empty".to_string());
    }
    let rest = match url.strip_prefix("https://") {
        Some(r) => r,
        None => return Err("url must start with https://".to_string()),
    };
    // host = "https://" と最初の '/' の間の部分。
    let (host, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => return Err("url has no path".to_string()),
    };
    const ALLOWED_HOSTS: [&str; 4] = [
        "discord.com",
        "discordapp.com",
        "ptb.discord.com",
        "canary.discord.com",
    ];
    if !ALLOWED_HOSTS.contains(&host) {
        return Err("host is not a Discord webhook host".to_string());
    }
    let webhook_path = match path.strip_prefix("/api/webhooks/") {
        Some(p) => p,
        None => return Err("path must start with /api/webhooks/".to_string()),
    };
    // id / token の 2 つ以上の非空セグメントが必要。
    let segments: Vec<&str> = webhook_path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return Err("path is missing webhook id or token".to_string());
    }
    Ok(())
}

