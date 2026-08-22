//! 大きな決着結果の退避（常時切り離し・案 A）。
//!
//! 常時切り離しでは、ツールの結果は**後のターン**の決着イベント（`Settled`）として会話へ戻る
//! （§07/§15）。結果が大きいまま決着本文に載ると、次ターンの文脈予算を単独で食い潰す（本体
//! opencrab #284 と同型）。本体はこれをワークスペースのファイルへ退避し `ws_read`/`execute_shell`
//! で部分読みするが、この social runtime は権威を全部 DB に置き（詳細§03）ファイルシステムを持たない。
//! そこで本体 offload の**仕様と思想**——閾値 2,500 トークン・大きい結果は退避して必要な分だけ
//! 読む・読み方のレシピを添える——を、この実装の基盤（store 背番号＋行範囲読み core ツール
//! `core-bg-read`）へ翻訳する。
//!
//! ここが持つのは「判定・整形・案内」だけ。退避先（store）とツールの配線は `lib.rs` が持つ。

use opencrab_port::TokenCounter;

/// inline（決着本文）に載せてよいトークン数の上限。**本体 `TOOL_RESULT_TOKEN_LIMIT` と同値**。
///
/// 本体（`crates/core/src/tool_result_log.rs`）の根拠をそのまま引く: 実測で数万バイトの 1 件が
/// 100k トークン級の会話予算を単独で食い潰した。1 件が数 KB 台（≒ 2,500 トークン）でなければ
/// 会話本文の枠が残らない。単位はトークン（バイトではない）——同じ長さでも日本語/英数字/base64 で
/// 実効トークン量が数倍ぶれる。判定は注入された `TokenCounter`（本番 o200k・テスト CharCounter）で行う。
pub const TOOL_RESULT_TOKEN_LIMIT: usize = 2_500;

/// 退避本文 1 件のバイト上限。**本体 `OFFLOAD_FILE_BYTE_LIMIT` と同値**（10MB）。
///
/// 退避先（store）へ落とす量にも歯止めを置く。本体は病的に膨らんだ尾（実測 486MB の 1 件）だけを
/// 頭打ちにし、正当な小物（1MB 未満）は 1 件も削らない値として 10MB を採った。超過時は先頭だけ
/// 保存し、案内で「不完全（truncated）」と明記する。
pub const OFFLOAD_BYTE_LIMIT: usize = 10 * 1024 * 1024;

/// `core-bg-read` が 1 回で返すトークンの天井。inline 上限から余白を引く（本体 `ws_read` の
/// `RANGE_CONTENT_TOKEN_CEILING` と同じ思想）——**読んだ結果自体が inline 上限を溢れさせない**
/// ことを構造的に守る。だから決着の案内は「返り値は必ず inline 上限未満に収まる」と約束できる。
/// 余白 400 は決着/読みの案内文（散文）が乗るぶん。
pub const RANGE_READ_TOKEN_CEILING: usize = TOOL_RESULT_TOKEN_LIMIT - 400;

/// 本文が inline 上限を超えているか。**単位はトークンのまま**（producer 契約と揃える）。
///
/// バイトの近道で両端を挟み、中間だけ有界にトークンを数える（**全量をトークナイズしない**・本体
/// #576 と同型）。生 body を全量 o200k で exact encode すると 486MB 級で GB 確保→OOM し、常時切り離しの
/// 主旨（待たせない・詰まらせない）を裏切る:
/// 1. `len < 上限` → o200k でも CharCounter でも `tokens <= bytes` なので、数えるまでもなく上限未満
///    （大半の結果は数百バイトでここで返る）。
/// 2. `len > 退避先の上限（10MB）` → この量は必ず上限トークンを超える（`bytes` に対し `tokens` が
///    極端に少なくなるのは 1 トークンが長大な単一ランのときだけで、それでも 10MB は上限をはるかに
///    超える）。退避は確定なので数えない——巨大入力を触らない近道。
/// 3. 中間（上限バイト〜10MB）だけ [`TokenCounter::count_reaches`] で判定する。本番実装（o200k）は
///    窓ごとに数えて上限到達で即 return するので、コストは「上限トークンぶんの入力＋端数 1 窓」で
///    頭打ちになる（全体サイズにも単一 pre-token の長さにも依存しない）。
pub fn exceeds_limit(counter: &dyn TokenCounter, s: &str) -> bool {
    if s.len() < TOOL_RESULT_TOKEN_LIMIT {
        return false;
    }
    if s.len() > OFFLOAD_BYTE_LIMIT {
        return true;
    }
    counter.count_reaches(s, TOOL_RESULT_TOKEN_LIMIT)
}

/// 本文の行数。末尾の改行は「空の最終行」を作らない（`"a\nb"` も `"a\nb\n"` も 2 行、空は 0 行）。
/// エディタ／行番号指定（`core-bg-read` の `start_line`）と一致する数え方。
pub fn count_lines(s: &str) -> usize {
    s.lines().count()
}

/// 退避先へ書く本文を上限バイトで丸める（本体と同じ流儀）。返すのは (保存する本文, 切り詰めたか)。
///
/// 上限超過時は**文字境界**で先頭だけ残し（バイト境界で切ると壊れた UTF-8 になる）、末尾が改行で
/// 終わらなければ改行を 1 つ足す（ファイルを完結した行で終える）。上限以下は 1 バイトも変えない。
pub fn clamp_body(body: &str) -> (String, bool) {
    if body.len() <= OFFLOAD_BYTE_LIMIT {
        return (body.to_string(), false);
    }
    let mut end = OFFLOAD_BYTE_LIMIT;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut s = body[..end].to_string();
    if !s.ends_with('\n') {
        s.push('\n');
    }
    (s, true)
}

/// 形式の手がかりを**パースせずに**端点だけで推定する（本体 `format_hint` と同じ）。判別できなければ
/// None（案内から省く）。生テキスト（shell 出力等）を "JSON" と偽らないための best-effort。
fn format_hint(s: &str) -> Option<&'static str> {
    let t = s.trim();
    match (t.as_bytes().first()?, t.as_bytes().last()?) {
        (b'{', b'}') => Some("JSON オブジェクトらしい"),
        (b'[', b']') => Some("JSON 配列らしい"),
        _ => None,
    }
}

/// 退避結果の**具体的な読み方**（本体 `read_recipe` に対応する実装）。本体はファイルパスに対する
/// `grep -n`→`ws_read`／`head -c` を示すが、この runtime に grep も execute_shell も無い——読み口は
/// `core-bg-read` ただ 1 つ。行番号で範囲を指定し、返り値は天井で必ず inline 上限未満に収まる。
fn read_recipe(activity_id: i64) -> String {
    format!(
        "読むには core-bg-read（activity={activity_id}・start_line・line_count）を呼ぶ。\
         start_line=1 で先頭から読める。返り値は必ず inline 上限未満に収まる（天井を超える指定は\
         自動で少なく返す）。全体像だけ要るなら、退避のもとになったツールを引数を絞って呼び直す。"
    )
}

/// 決着イベントに載せる案内（大きい結果を退避したとき）。**生データは 1 バイトも含めない**——
/// パス相当（活動 id）・バイト数・行数・約トークン数・形式の手がかり・読み方レシピだけ。
/// 数はすべて**実際に保存した本文**（`saved_body`）から数える（本体 C2: 実ファイルと嘘をつかない）。
pub fn settle_notice(
    activity_id: i64,
    ok: bool,
    saved_body: &str,
    truncated: bool,
    counter: &dyn TokenCounter,
) -> String {
    let bytes = saved_body.len();
    let lines = count_lines(saved_body);
    let tokens = counter.count(saved_body);
    let hint = match format_hint(saved_body) {
        Some(h) => format!("・{h}"),
        None => String::new(),
    };
    let head = if ok {
        format!("活動 #{activity_id} が完了した（成功）")
    } else {
        format!("活動 #{activity_id} が失敗した")
    };
    let recipe = read_recipe(activity_id);
    if truncated {
        format!(
            "{head}。結果が大きく退避先の上限も超えたので、先頭 {bytes} バイト（{lines} 行・約\
             {tokens} トークン{hint}）だけを退避した——残りは捨てた＝**不完全（truncated）**。{recipe} \
             これは先頭だけ。全体が要るなら、もとのツールを引数を絞って呼び直す。同じ引数で同じ\
             ツールを結果を見るためだけに呼び直さない。"
        )
    } else {
        format!(
            "{head}。結果が大きい（{bytes} バイト・{lines} 行・約{tokens} トークン{hint}）ので退避した——\
             本文はここに載せない。{recipe} 使い方は任せる（部分を読む・検索する・要点だけ取る）。\
             同じ引数で同じツールを結果を見るためだけに呼び直さない。"
        )
    }
}

/// `core-bg-read` が返す行範囲。返す本文は**必ず inline 上限未満**（天井 [`RANGE_READ_TOKEN_CEILING`]）。
pub struct Slice {
    pub text: String,
    /// 実際の開始行（1-based・clamp 後）。
    pub start_line: usize,
    /// 実際に返した行数（天井や範囲端で `line_count` より少ないことがある）。
    pub returned_lines: usize,
    /// 退避本文の総行数（案内で「全 N 行」を正しく出すため）。
    pub total_lines: usize,
    /// 天井で `line_count` より少なく返したか（案内で「指定より少ない」を伝える）。
    pub capped_by_ceiling: bool,
}

/// 退避本文から行範囲を取り出す。**返り値は必ず inline 上限未満**——天井を超えない範囲まで行を
/// 足し、超えたらそこで止める。1 行だけで天井を超える病的な本文でも、**必ず前進する**ように
/// その 1 行をバイトで頭打ち（トークン ≤ バイト）にして返す（読み口が行き止まりにならない）。
pub fn read_lines(
    counter: &dyn TokenCounter,
    body: &str,
    start_line: usize,
    line_count: usize,
) -> Slice {
    let lines: Vec<&str> = body.lines().collect();
    let total = lines.len();
    let start = start_line.max(1);
    if start > total {
        return Slice {
            text: String::new(),
            start_line: start,
            returned_lines: 0,
            total_lines: total,
            capped_by_ceiling: false,
        };
    }
    let want = line_count.max(1);
    let end = (start - 1 + want).min(total); // lines への排他上限
    let mut out = String::new();
    let mut returned = 0usize;
    let mut capped = false;
    for line in lines.iter().take(end).skip(start - 1) {
        let mark = out.len();
        out.push_str(line);
        out.push('\n');
        if counter.count(&out) > RANGE_READ_TOKEN_CEILING {
            out.truncate(mark);
            if returned == 0 {
                // 1 行目すら天井を超える: バイトで頭打ちにして 1 行分だけ返す（必ず前進する）。
                // 末尾に足す改行 1 文字ぶんの余白を残す（トークン ≤ バイトなので天井を割らない）。
                let mut cut = line.len().min(RANGE_READ_TOKEN_CEILING - 1);
                while cut > 0 && !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                out.push_str(&line[..cut]);
                out.push('\n');
                returned = 1;
            }
            capped = true;
            break;
        }
        returned += 1;
    }
    Slice {
        text: out,
        start_line: start,
        returned_lines: returned,
        total_lines: total,
        capped_by_ceiling: capped,
    }
}

/// `core-bg-read` の結果本文。取り出した行と、どこを返したか（全 N 行・天井で切ったか）を添える。
pub fn render_slice(activity_id: i64, slice: &Slice) -> String {
    if slice.returned_lines == 0 {
        return format!(
            "活動 #{activity_id} の退避は全 {} 行。指定 start_line={} は範囲外（1..={} で指定する）。",
            slice.total_lines, slice.start_line, slice.total_lines
        );
    }
    let end = slice.start_line + slice.returned_lines - 1;
    let mut head = format!(
        "活動 #{activity_id} の退避 {}〜{} 行目（全 {} 行",
        slice.start_line, end, slice.total_lines
    );
    if slice.capped_by_ceiling {
        head.push_str("・inline 上限に収めるため指定より少なく返した");
    }
    head.push_str("）:\n");
    head.push_str(&slice.text);
    head
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 文字 = 1 トークンの物差し（テスト用・本番 o200k とは切り離す）。
    struct CharCounter;
    impl TokenCounter for CharCounter {
        fn count(&self, s: &str) -> usize {
            s.chars().count()
        }
    }

    #[test]
    fn exceeds_limit_uses_byte_shortcut_then_tokens() {
        let c = CharCounter;
        assert!(!exceeds_limit(&c, "short"));
        // 上限ちょうど未満は数えず under。
        assert!(!exceeds_limit(&c, &"x".repeat(TOOL_RESULT_TOKEN_LIMIT - 1)));
        // 上限以上は over。
        assert!(exceeds_limit(&c, &"x".repeat(TOOL_RESULT_TOKEN_LIMIT)));
    }

    /// 有界性（レビュー修正1）: 巨大・病的な body でも exceeds_limit が**短時間で**判定を返す。
    /// 実時間は assert しない——完走すれば「全量トークナイズしていない」の証明（旧 exact encode は
    /// 486MB 級で GB 確保→OOM／単一ランで O(n²)。本番実装 O200kCounter で確かめる）。
    #[test]
    fn exceeds_limit_is_bounded_on_pathological_bodies() {
        let counter = crate::O200kCounter::new();
        // (1) 区切りの無い 5MB 単一ラン（10MB 未満なので count_reaches 経路）。窓打ち切りで即 true。
        //     旧 exact encode ならこの 1 pre-token が O(n²) で刺さる。
        let run = "a".repeat(5_000_000);
        assert!(exceeds_limit(&counter, &run));
        // (2) 10MB 超はバイト近道で即 true（数えない）。生成コストだけで完走する。
        let over = "z".repeat(OFFLOAD_BYTE_LIMIT + 1);
        assert!(exceeds_limit(&counter, &over));
    }

    #[test]
    fn read_lines_stays_under_ceiling_and_reports_range() {
        let c = CharCounter;
        // 各行 10 文字。天井（RANGE_READ_TOKEN_CEILING）ぶんの行数までしか返らない。
        let body = (0..1000)
            .map(|i| format!("line{i:05}")) // 9 文字 + 改行
            .collect::<Vec<_>>()
            .join("\n");
        let s = read_lines(&c, &body, 1, 1000);
        assert_eq!(s.start_line, 1);
        assert_eq!(s.total_lines, 1000);
        assert!(s.capped_by_ceiling, "1000 行は天井で切られる");
        assert!(
            c.count(&s.text) <= RANGE_READ_TOKEN_CEILING,
            "返り値は必ず天井以内: {} tokens",
            c.count(&s.text)
        );
        assert!(s.returned_lines > 0 && s.returned_lines < 1000);
    }

    #[test]
    fn read_lines_makes_progress_on_a_single_huge_line() {
        let c = CharCounter;
        // 1 行だけで天井を大きく超える。バイトで頭打ちにして必ず 1 行返す（行き止まりにしない）。
        let body = "z".repeat(RANGE_READ_TOKEN_CEILING * 3);
        let s = read_lines(&c, &body, 1, 1);
        assert_eq!(s.returned_lines, 1);
        assert!(s.capped_by_ceiling);
        assert!(c.count(&s.text) <= RANGE_READ_TOKEN_CEILING);
    }

    #[test]
    fn read_lines_out_of_range_returns_empty_with_total() {
        let c = CharCounter;
        let body = "a\nb\nc";
        let s = read_lines(&c, body, 99, 10);
        assert_eq!(s.returned_lines, 0);
        assert_eq!(s.total_lines, 3);
        assert!(render_slice(5, &s).contains("範囲外"));
    }

    #[test]
    fn settle_notice_withholds_body_and_names_the_read_tool() {
        let c = CharCounter;
        let body = "secret-line-A\nsecret-line-B\n";
        let n = settle_notice(42, true, body, false, &c);
        // 生データは載らない。
        assert!(!n.contains("secret-line-A"), "生データが載っている: {n}");
        // メタ情報＋読み方（core-bg-read・activity=42）が載る。
        assert!(n.contains("成功"));
        assert!(n.contains("2 行"));
        assert!(n.contains("core-bg-read（activity=42"), "{n}");
        // 切り詰めでない案内に truncated 表現は出ない。
        assert!(!n.contains("不完全"));
    }

    #[test]
    fn settle_notice_marks_truncation() {
        let c = CharCounter;
        let n = settle_notice(7, false, "head\n", true, &c);
        assert!(n.contains("失敗"));
        assert!(n.contains("不完全"), "{n}");
        assert!(n.contains("core-bg-read（activity=7"));
    }

    #[test]
    fn clamp_body_is_noop_under_limit() {
        let (out, truncated) = clamp_body("small body\n");
        assert_eq!(out, "small body\n");
        assert!(!truncated);
    }
}
