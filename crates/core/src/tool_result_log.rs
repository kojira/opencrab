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
//! - トークン上限とワークスペースへの退避（超大結果で context 予算を吹き飛ばさない）
//!
//! は**全経路で同一**でなければならない。
//!
//! #620: かつてここにあった「秘密フィールド（`nsec` キー名）のマスク」は撤去した。キー名
//! 一致は実際の混入（別の文字列値の中に鍵が含まれる形）を検出できず、`nsec` を JSON キーに
//! 持つ結果を出す producer も皆無だった。鍵は at-rest 暗号化＋実行時 env 注入で扱う。
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
///
/// **これは複数 crate の共有契約（#576）。** ツール結果を作る側は「この上限**トークン**内なら
/// 退避されない」ことを頼りに自分の出力をトークンで頭打ちにしている:
/// `ws_read`（`RANGE_CONTENT_TOKEN_CEILING = ここ - 400`）、`memory_units`
/// （`HISTORY_RESULT_TOKEN_BUDGET = ここ * 8/10` / `INLINE_LIMIT_TOKENS = ここ`）、
/// `search`、Nostr 受信退避など。**だから単位はトークンから動かせない**。判定をバイトに
/// すると同じ値でも言語・エンコードでバイト量がぶれ、トークンで上限内に収めた本文が
/// バイトで退避され、これら producer の保証が破れる。#576 で消したのは「判定の単位」では
/// なく「全体を一括トークナイズすること」だけ（[`exceeds_limit`] を参照）。
///
/// **producer は「この上限そのもの」ではなく、余白を引いた予算に出力を収めること。**
/// 判定（[`exceeds_limit`] → [`crate::tokens::tokens_reach_limit`]）は入力を窓分割して数える
/// ため、窓境界で累計が数トークン**上振れ**する（最悪見積もりでも合計 100 トークン未満）。
/// 上限ちょうどを出力 cap に採るとこの上振れが刺さって退避されうる。実際に本文を切っている
/// のは余白付きの `HISTORY_RESULT_TOKEN_BUDGET`（-500）/ `RANGE_CONTENT_TOKEN_CEILING`（-400）
/// で、`INLINE_LIMIT_TOKENS`（余白ゼロ＝上限そのもの）は名前に反して cap ではなく LLM への
/// **表示値**（`fits` 判定と表示フィールドにしか使われない）。将来ここへ「上限そのもの」を
/// cap に採る producer を足さないこと。
pub const TOOL_RESULT_TOKEN_LIMIT: usize = 2_500;

/// **読み**（`ws_read` 等）の結果に使う inline 上限（#707）。
///
/// [`TOOL_RESULT_TOKEN_LIMIT`]（2,500）は「量が事前に分からない出力」——shell の stdout——が
/// 会話予算を食い潰す事故（#284）への cap で、旧 10,000 **バイト**上限のトークン換算（#294）。
/// 暴走への cap としては正しいが、**読みに当てると往復が増えるだけ**だった:
///
/// - 本番実測（2026-08-20）: 700 行の設計文書で 180 行を要求して **46 行**しか返らず、9 往復
///   しても読み終わらない。1 往復ごとにモデルの推論（実測 100〜130 秒）が挟まるので、読解
///   だけで 25〜30 分。サブタスクが 1,700 秒の制限に達して **commit ゼロ**で終わった
/// - 刻んでも**文脈は節約できない**。15 回に分けても 15 件すべてが履歴に積まれ合計は同じ
///
/// 値 30,000 の根拠: 2,000 行の典型的なソースが 1 回で入ること（実測 700 行 = 18,000 →
/// 2,000 行 ≒ 25,000）。会話予算は `context_window` の誤登録修正（80,000 → 1,000,000）で
/// 500,000 になったので、1 件 30,000 は **6%**。加えて読みの本文は会話へ持ち越さない
/// （`conversation.rs` の参照化）ので、積み上がらない。
pub const READ_TOOL_RESULT_TOKEN_LIMIT: usize = 30_000;

/// このツール結果に適用する inline 上限（#707）。
///
/// 分ける軸は「出力量を**誰が決めたか**」。エージェントが行範囲を指定した読みは自分で量を
/// 決めているので、上限は「会話に収まるか」だけ見ればよい。コマンドの stdout は量が事前に
/// 分からないので低い cap で退避へ倒す（#284 の防御）。
pub fn inline_limit_for_tool(tool_name: &str) -> usize {
    if is_read_tool(tool_name) {
        READ_TOOL_RESULT_TOKEN_LIMIT
    } else {
        TOOL_RESULT_TOKEN_LIMIT
    }
}

/// append 時の inline 上限。ツール別上限と、そのときの残り会話枠の小さい方。
///
/// `remaining` が `None` のときは水位が無い（テスト / 水位未設定）のでツール別上限だけ。
/// `Some(0)` は枠が無いので 0。sanitizer は 0 なら必ずスタブに倒す。
pub fn append_limit_for_tool(tool_name: &str, remaining: Option<usize>) -> usize {
    match remaining {
        Some(left) => inline_limit_for_tool(tool_name).min(left),
        None => inline_limit_for_tool(tool_name),
    }
}

/// **「読み」の唯一の定義**（#707）。もう一度呼べば同じものが得られ、副作用が無いツール。
///
/// この 1 つの述語を、性質の違う 2 つの判断が**両方**参照する:
/// - [`inline_limit_for_tool`]（この結果を退避するか＝1 回で運べる量）
///
/// **#709 以降、参照化はすべてのツール結果に掛かる**ので、この述語が効くのは
/// [`inline_limit_for_tool`]（1 回で運べる量）だけになった。「読み」を増やすときはここを直す。
pub fn is_read_tool(tool_name: &str) -> bool {
    matches!(tool_name, "ws_read" | "ws_list")
}

/// 退避ファイル名 1 コンポーネント（session_id / tool_call_id）の上限バイト数。
///
/// 2 つの理由で必要:
/// - 多くのファイルシステムはファイル名 255 バイト。長い ID をそのまま繋ぐと
///   `std::fs::write` が `ENAMETOOLONG` で落ち、退避できたはずの全文を捨ててしまう。
/// - 案内文にはこのパスが載る。長さを縛らないと案内文自体が
///   [`TOOL_RESULT_TOKEN_LIMIT`] を超え、永続化側の「上限未満なら素通り」を通過して
///   LLM と DB の本文が食い違う（#286）。
const OFFLOAD_COMPONENT_LIMIT: usize = 64;

/// 本文が上限を超えているか。**単位はトークンのまま**（[`TOOL_RESULT_TOKEN_LIMIT`] は
/// producer 側の契約と共有しているため／このモジュール冒頭の理由を参照）。ただし
/// **全体をトークナイズしない**（#576）。
///
/// 2 段構え:
/// 1. `o200k_base` の 1 トークンは必ず 1 バイト以上なので `tokens <= bytes`。バイト数が
///    上限未満なら、数えるまでもなく上限未満（大半のツール結果は数百バイトでここで返る）。
///    上界の性質は `tokens::tests::tokens_never_exceed_bytes` で固定。
/// 2. 超えていそうなものだけ [`crate::tokens::tokens_reach_limit`] で判定する。これは先頭から
///    窓ぶんずつ encode し、累計が上限に達した時点で打ち切る。コストは「上限トークンぶんの
///    入力」で頭打ちになり、巨大入力（486MB）や長い単一文字ランを一括トークナイズする
///    O(n²) の入口を塞ぐ（#576）。判定はトークン基準のまま＝ producer の「上限内なら
///    退避されない」保証は変わらない。
fn exceeds_limit(s: &str, limit: usize) -> bool {
    if s.len() < limit {
        return false;
    }
    crate::tokens::tokens_reach_limit(s, limit)
}

// #620: 「秘密として扱う JSON フィールド名の集合」（`SECRET_KEYS`）と、それを使う
// マスク（`redact_secrets_in_place`）・検出（`contains_secret`）・sanitize 前段の
// `redact_secrets_in_result` は**撤去した**。キー名一致は実際の混入（別の文字列値の中に
// 鍵が含まれる形）を検出できず、`nsec` を JSON キーに持つ結果を tool_result / sink へ出す
// producer も皆無だった（列挙で確認 / #620）。鍵は at-rest 暗号化＋実行時 env 注入で
// 「エージェントの読める範囲の外」に置く方式へ移し、事後のキー名マスクには依存しない。
// content ベースの `nsec` トークン伏せ（`crates/nostr/src/cli.rs` の `redact_nsec_tokens`）は
// passthrough stdout の自由文向けに別途残している（役割が違う）。

/// 退避ファイルへ書き込む**本文**を、後から部分読み・検索できる形に整える（#616）。
///
/// 退避先は従来「エンベロープ JSON をそのまま 1 行」で書いていた。`stdout` 全体が 1 本の
/// JSON 文字列に押し込まれて実改行が `\n` の 2 文字に化け、ファイルが改行を 1 つも持たない
/// 1 行になるため、`head -n` / `grep` / `sed` / `jq` のどれも役に立たず、`grep` が当たると
/// 数 MB が丸ごと返って一発で文脈が枯れる。ここで**書き込む直前に 1 回だけ**内容ベースで
/// 整形する（判定はツール名を見ない）。
///
/// - **(a) `data.stdout` が string（shell 形）** → 生テキスト。
///   `exit_code == 0` かつ `stderr` が空なら**ヘッダを付けず** `stdout` を verbatim で書く
///   （成功系の大多数。`gh`/`curl` の JSON 応答をそのまま `jq` / 次コマンドへ直渡しできる）。
///   それ以外（非ゼロ終了 or stderr 非空）は `exit_code` と `stderr`/`stdout` をヘッダ付きで
///   書く。`stdout`/`stderr` は**parse 済みの文字列値**として取り出すので serde が `\n` を
///   実改行へ戻し、エスケープはゼロになる。`truncated` フィールドはヘッダから落とす
///   （`crates/actions/src/tools/shell.rs:232` で**常に false**＝情報損失なし）。
/// - **(b) それ以外の JSON** → `to_string_pretty`（複数行になり `head`/`grep` が効く）。
/// - **(c) parse 失敗** → 生バイトを verbatim（借用のまま）。
///
/// #624: どの分岐だったかを [`OffloadFormat`] で一緒に返す。退避ファイルの拡張子を中身に
/// 合わせるため（生テキストは `.txt`、pretty JSON は `.json`）。以前は常に `.json` 固定で、
/// #616 で shell 本文を生テキストに変えたのに拡張子が `.json` のままだったので、`jq` を
/// 試して失敗する誤誘導になっていた。
///
/// #620: 以前はここより手前で nsec キー名 redaction を通していたが撤去した（守るものが無い）。
fn render_offload_body(result_json: &str) -> (std::borrow::Cow<'_, str>, OffloadFormat) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(result_json) else {
        // (c) 非 JSON はそのまま。JSON ではないので拡張子は .txt。
        return (std::borrow::Cow::Borrowed(result_json), OffloadFormat::Text);
    };
    // (a) shell 形: data.stdout が string のときだけ。
    // 注意: `data.stdout` が string というだけで shell 扱いにしている。現状のツール群では
    // `execute_shell` 以外にこの形は無いので衝突しないが、将来 `data.stdout: string` を返す
    // 別ツールが出たら誤分類し得る（そのときは shell だけを識別する判別子を足す）。
    if let Some(stdout) = value
        .get("data")
        .and_then(|d| d.get("stdout"))
        .and_then(|s| s.as_str())
    {
        let data = &value["data"];
        let stderr = data.get("stderr").and_then(|s| s.as_str()).unwrap_or("");
        let exit_code = data.get("exit_code").and_then(|c| c.as_i64());
        // C3: 成功系（exit_code==0 かつ stderr 空）はヘッダ無しで stdout を verbatim。
        if exit_code == Some(0) && stderr.is_empty() {
            return (
                std::borrow::Cow::Owned(stdout.to_string()),
                OffloadFormat::Text,
            );
        }
        let code = exit_code.unwrap_or(-1);
        return (
            std::borrow::Cow::Owned(format!(
                "exit_code={code}\n--- stderr ---\n{stderr}\n--- stdout ---\n{stdout}"
            )),
            OffloadFormat::Text,
        );
    }
    // (b) それ以外の JSON は pretty。失敗したら原文（起こり得ないが安全側）。
    match serde_json::to_string_pretty(&value) {
        Ok(pretty) => (std::borrow::Cow::Owned(pretty), OffloadFormat::Json),
        // pretty 化に失敗した原文は元々 valid JSON（from_str が通っている）なので .json。
        Err(_) => (std::borrow::Cow::Borrowed(result_json), OffloadFormat::Json),
    }
}

/// 退避本文の形式（#624）。退避ファイルの拡張子を中身に合わせるためだけに使う。
///
/// - [`OffloadFormat::Text`] → `.txt`: shell の生テキスト（分岐 a）と parse 失敗の verbatim
///   （分岐 c）。どちらも JSON ではないので `jq` は効かず、`grep`/`sed`/`head` が効く。
/// - [`OffloadFormat::Json`] → `.json`: pretty 化した構造化 JSON（分岐 b）。`jq` が通る。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OffloadFormat {
    Text,
    Json,
}

impl OffloadFormat {
    /// 退避ファイルの拡張子（先頭ドット無し）。
    fn extension(self) -> &'static str {
        match self {
            OffloadFormat::Text => "txt",
            OffloadFormat::Json => "json",
        }
    }
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
    /// [`OFFLOAD_FILE_BYTE_LIMIT`] 超過で**先頭だけ**保存したときの、保存した**本文**の
    /// 先頭バイト数（`render_offload_body` 後の本文に対する長さ）。ファイル完結のために末尾へ
    /// 改行を 1 つ足すことがあるが、その改行はこの数に**含めない**（元本文のどこまでを保存
    /// したか＝ notice が数え直す範囲を指す）。全文を保存したなら `None`。
    saved_prefix_bytes: Option<usize>,
}

/// 上限超過分をワークスペースへ退避する。成功したら保存先（切り詰め有無つき）を返す。
///
/// [`OFFLOAD_FILE_BYTE_LIMIT`] を超える結果は**先頭バイト（文字境界で丸め）だけ**保存し、
/// `saved_prefix_bytes` にその長さを載せる（#568）。上限以下は全文保存で従来と 1 バイトも
/// 変わらない（`saved_prefix_bytes = None`）。
fn offload_to_workspace(
    body: &str,
    ext: &str,
    session_id: &str,
    tool_call_id: &str,
    workspace_root: Option<&Path>,
) -> Option<OffloadResult> {
    let root = workspace_root?;
    let tmp_dir = root.join("tmp");
    let _ = std::fs::create_dir_all(&tmp_dir);
    // session_id / tool_call_id は外部（gateway・LLM プロバイダ）由来の文字列。
    // パス区切り（`/`）や `..` が混ざるとワークスペースの外へ書きうるので、英数字以外を潰す
    // （`/` も `.` も潰れるのでパス脱出は防げる）。長さも縛る（[`OFFLOAD_COMPONENT_LIMIT`]）。
    //
    // #635: 潰す文字も区切りも `-` に揃え、ファイル名に現れる区切りを含めて「全部ハイフン」に
    // する。`_` と `-` が混在すると、UUID を含む id（例: `6f3fd055-711e-48da-8573-3bfedc778dd9`）が
    // 「壊れた UUID」に見え、モデルがパスを『直そう』として実在しないパスを渡し、退避ファイルを
    // 開けなくなる。全部 `-` なら UUID は元の見た目のまま残り、直す動機が消える。
    let sanitize_component = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .take(OFFLOAD_COMPONENT_LIMIT)
            .collect()
    };
    // #624: 拡張子は中身に合わせる（生テキストは .txt、pretty JSON は .json）。ext は
    // [`OffloadFormat::extension`] 由来の固定文字列なので sanitize は不要。
    // #635: component 間の区切りも `-` に揃える（「全部ハイフン」）。
    let filename = format!(
        "{}-{}.{ext}",
        sanitize_component(session_id),
        sanitize_component(tool_call_id)
    );
    // #568/#616: ディスクへ落とす量にも上限を設ける。超過時は**文字境界**で先頭だけ残し
    // （バイト境界で切ると壊れた UTF-8 になる）、末尾が改行で終わらなければ改行を 1 つ足す。
    //
    // 行境界で切る案は採らない（PR #619 レビュー）: pretty JSON はどこで切っても valid には
    // ならず、生テキストは行の途中で切れても読めて grep も効くので、行境界が唯一買うのは
    // 「ファイルが完結した行で終わる」ことだけ。それは末尾に改行を足せば得られる。逆に窓内の
    // 最後の改行で切ると、`"header\n" + 改行なしの巨大本文` のような入力で end=7 になり、
    // 保存できたはずの ~10MB を 7 バイトへ激減させる（改行ゼロなら丸ごと残るのに逆転する）。
    // 上限以下は全文をそのまま書く（no-op）。
    let (to_write, saved_prefix_bytes) = if body.len() > OFFLOAD_FILE_BYTE_LIMIT {
        let mut end = OFFLOAD_FILE_BYTE_LIMIT;
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        let mut s = body[..end].to_string();
        if !s.ends_with('\n') {
            s.push('\n'); // 完結した行でファイルを終える（足した改行は保存量に数えない）
        }
        (std::borrow::Cow::Owned(s), Some(end))
    } else {
        (std::borrow::Cow::Borrowed(body), None)
    };
    if std::fs::write(tmp_dir.join(&filename), to_write.as_ref()).is_ok() {
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
/// - **具体的な読み方のレシピ**（[`read_recipe`]。#624）
///
/// #624: 従来は「どう参照するかは指示しない（読む / grep / jq / パスを次コマンドへ渡す、
/// のどれが最適かはタスク次第）」と選択肢だけ示して判断を委ねていた。だが実運用で、
/// `execute_shell` の巨大結果が退避されると**エージェントが中身を 1 バイトも見ないまま
/// 「結果を受け取り次第続行します」と言って沈黙する**（3 セッション連続）事例が出た。原因は
/// 文面が「どう読むか」の具体を示さず「待つ」を選ばせること。#616（生テキスト退避）と
/// #617（`ws_read` の行指定）で**実際に読めるようになった**ので、その手順を明示する
/// （`grep -n` で行番号 → `ws_read(start_line, line_count)`、または `head -c` でバイト頭打ち。
/// 詳細と「なぜ `sed -n` を並べないか」は [`read_recipe`] を参照）。
/// 「先頭 20 件だけ見て結論」を避けたい趣旨は残すため、レシピは**部分読み・検索**を勧める形で、
/// 全体像が要るなら引数を絞って取り直す導線も併記する。読む手段が無い caller（ファイル読み
/// ツールを持たない run）には、その再実行の導線だけが効く。
///
/// 「同じツールを再実行するな」は残す（#284 のループ防止に効いている）。
///
/// #616: バイト数・行数・トークン数・形式の手がかりは**実際に保存する本文**（`body`、必要なら
/// 先頭だけ）から数える。従来はエンベロープ（`result_json`）から数えていたため、本文を生
/// テキストに変えると実ファイルが 3,303 行でも「1 lines」と嘘を報告した（C2）。
///
/// `orig_bytes`（= redaction 後のエンベロープ長）は規模のシグナルとして別途残すが、文言は
/// **"the serialized result was N bytes"** とする（PR #619 レビュー）。これは JSON 直列化で
/// エスケープ水増しされた値で、実際のツール出力（stdout 実体）ではない。"original tool output"
/// と名乗ると、エージェントがこの水増し値を「元の出力サイズ」として外へ再報告してしまう
/// （#616 の実害そのもの: 実際に「全体 grep が約 761MB まで膨らんだ」と Discord に書かれた）。
/// 全文保存側にも #568 の「規模＋保存量」の二段構えを広げて整合させる。
/// 退避ファイルの**具体的な読み方**（#624）。ファイルを読める caller（`ws_read` /
/// `execute_shell` を持つ owner 等価）に効く手順を書く。読めない caller には、呼び出し側の
/// 案内に残す「引数を絞って再実行する」導線が効く（レシピは害にならない：実行できないだけ）。
///
/// #616 で退避本文が**行のある生テキスト**になり、#617 で `ws_read` が**行指定**（`start_line`
/// / `line_count`）になったので、`grep -n` の行番号をそのまま `ws_read` へ渡す導線が実際に
/// 機能する。以前は退避本文が JSON 1 行で `grep` が全部返していた。
///
/// #624 レビュー: **確実に inline 上限を守る導線だけ**を並べる。`sed -n '1,200p'` は落とした
/// （200 行が高密度だと 2,500 トークンを超え、`execute_shell` 経由で**同じ退避・同じ通知が
/// 跳ね返る**＝この PR が救おうとする場面そのもので自己ループする）。残す 2 つはどちらも
/// 上限を構造的に守る:
/// - `ws_read`（`start_line`/`line_count`）: `compute_ws_read` が返り値を必ず inline 上限未満に
///   抑える。`start_line=1` で「上から読む」も表現でき、`sed` を落としても失われる導線は無い。
/// - `head -c 2000 <path>`: **バイト**で頭打ちにするので、トークン数 ≤ バイト数より必ず上限内。
///   `head -n`（行数）は 1 行が長いと超えるので**採らない**。`ws_read` を持たない caller
///   （`ws_read` は `OWNER_ONLY_ACTIONS`）には `head -c` が唯一の安全な導線なので残す。
///
/// #856 発見3（統括判定 (a)）: **`ws_read` 直読みを先頭に置き、`grep -n` は「特定の一致へ
/// 飛ぶ」オプションとして後置する。** 旧文面は `grep -n <pattern>` 先行だったため、退避ファイルを
/// 読むだけの一般ケースでも先に `grep` が走る。広いパターンだと grep 出力自体が 2,500 上限を
/// 超えて `execute_shell` 経由で**もう一度退避される**（#856 発見3 の grep スパイラル）。無限
/// ループにはならない（再退避も次の `ws_read` で読める）が 1 段余計に回る。`ws_read`
/// （`start_line=1`）は行番号を必要とせず、`compute_ws_read` が返り値を構造的に inline 上限未満へ
/// 抑える（[`RANGE_CONTENT_TOKEN_CEILING`] / `crates/actions/src/workspace.rs`）ので、直読みを
/// 先頭にすればこの余計な 1 段が消える。`grep` は「行番号を得て特定箇所へ飛ぶ」用途に残すが、
/// **退避ファイルへの grep は広いパターンだと再退避され得る**ことを文面で明示し、その場合も
/// 生成された新しい退避ファイルを `ws_read` で読めば閉じることを示す（回復可能＝無限ループ
/// でない）。substrings `grep -n <pattern> {rel}` / `head -c 2000 {rel}` は据え置き（回収導線
/// の契約）。
fn read_recipe(rel: &str) -> String {
    format!(
        "To read it, call `ws_read` on that path with `start_line`/`line_count` (`start_line=1` \
         reads from the top; `ws_read` always keeps its output under the inline limit and pages \
         the rest via `has_more`/`next_line`, so reading it back never re-offloads). To jump to \
         a specific match, run `grep -n <pattern> {rel}` first and pass a returned line number as \
         `start_line` (a broad pattern may itself be offloaded; if so, just `ws_read` the new \
         file it names). If you have no `ws_read` tool, run `head -c 2000 {rel}` via \
         execute_shell for a bounded prefix. The saved body is line-oriented text whose line \
         numbers line up with `ws_read`."
    )
}

fn oversized_notice(orig_bytes: usize, body: &str, saved: Option<&OffloadResult>) -> String {
    // C2: 実際にファイルへ入る本文（全文 or 先頭だけ）を確定し、そこから数える。
    let saved_slice = match saved {
        Some(OffloadResult {
            saved_prefix_bytes: Some(n),
            ..
        }) => &body[..*n],
        _ => body,
    };
    let bytes = saved_slice.len();
    let lines = count_lines(saved_slice);
    // 「約 N トークン」は LLM が「全部読んだら予算をどれだけ食うか」を見積もるための**目安**。
    // 全体をトークナイズすると巨大退避（486MB 実績）で同期 CPU を食う（#576）ので、先頭窓の
    // 密度から概算する（`~` 付きで目安と分かる文言）。判定と違い数を返すのでこちらを使う。
    let tokens = crate::tokens::estimate_tokens_bounded(saved_slice);
    // C4: 形式の手がかりも保存本文から（shell 形の生テキストを "JSON object" と偽らない）。
    let hint = match format_hint(saved_slice) {
        Some(h) => format!(", {h}"),
        None => String::new(),
    };
    match saved {
        // 全文を保存できた（[`OFFLOAD_FILE_BYTE_LIMIT`] 以下）。元サイズと保存量の二段構え。
        Some(OffloadResult {
            rel_path: rel,
            saved_prefix_bytes: None,
        }) => {
            let recipe = read_recipe(rel);
            format!(
                "[Tool result withheld: the serialized result was {orig_bytes} bytes. Its saved \
                 form ({bytes} bytes, {lines} lines, ~{tokens} tokens{hint}) was written in full \
                 to `{rel}` (path relative to your workspace root). It exceeded the \
                 {TOOL_RESULT_TOKEN_LIMIT}-token inline limit, so none of its content is included \
                 here. {recipe} It is up to you how to use it: read part of it, search it, \
                 transform it, or pass the path straight to the next command without reading it \
                 at all. If you cannot read that file (some runs have no file-reading tool), \
                 instead re-run with a narrower request (smaller id/time window, fewer rows, or \
                 estimate the size first) so the result fits under the limit. Do NOT re-run the \
                 same tool with the same arguments just to see the output again.]"
            )
        }
        // #568: 上限超過で**先頭だけ**保存した。元サイズと保存量を明記し、「不完全」であること・
        // 全文が要るなら引数を絞って再実行する導線を残す（discarded とは別の状態）。
        Some(OffloadResult {
            rel_path: rel,
            saved_prefix_bytes: Some(_),
        }) => {
            let recipe = read_recipe(rel);
            format!(
                "[Tool result withheld: the serialized result was {orig_bytes} bytes. Only the \
                 first {bytes} bytes ({lines} lines, ~{tokens} tokens{hint}) were saved to \
                 `{rel}` (path relative to your workspace root) to cap the offload file size - \
                 the rest was discarded, so the saved file is incomplete (truncated); it ends on \
                 a complete line. {recipe} Remember this is only the saved prefix, not the whole \
                 output. If you need the full result, re-run with a narrower request (smaller \
                 id/time window, fewer rows, or estimate the size first) so it fits. Do NOT \
                 re-run the same tool with the same arguments just to see the output again.]"
            )
        }
        None => format!(
            "[Tool result withheld: the serialized result was {orig_bytes} bytes ({lines} \
             lines, ~{tokens} tokens{hint}). It exceeded the {TOOL_RESULT_TOKEN_LIMIT}-token \
             inline limit and could not be saved to your workspace, so it was discarded - none of \
             its content is included here and there is no file to read. If you still need the \
             data, re-run with narrower arguments (filter/limit) rather than repeating the same \
             call.]"
        ),
    }
}

/// 全経路共通の無害化本体（上限判定 → 退避 → メタ情報のみの案内）。
///
/// `tool_name` は呼び出し元の意図表示・将来の per-tool 方針のために残す。#620 で nsec キー名
/// マスクは撤去したので、ここが行うのはサイズ上限と退避だけ。
fn sanitize_tool_result(
    _tool_name: &str,
    result_json: &str,
    session_id: &str,
    tool_call_id: &str,
    workspace_root: Option<&Path>,
    limit: usize,
) -> String {
    // 上限は呼び出し側が [`append_limit_for_tool`] / [`inline_limit_for_tool`] で決めて渡す。
    // #620: 旧来の nsec キー名マスク（`redact_secrets_in_result`）は撤去した（守るものが
    // 無い / 鍵は at-rest 暗号化と env 注入で扱う）。ここはサイズ上限と退避だけを行う。
    if !exceeds_limit(result_json, limit) {
        return result_json.to_string();
    }

    // #616: 書き込む直前に 1 回だけ、部分読み・検索できる形へ整える。
    // 退避判定（[`exceeds_limit`]）はエンベロープ基準＝ producer の契約は不変。
    // #624: 中身に合わせて拡張子を決める（生テキスト → .txt、pretty JSON → .json）。
    let (body, fmt) = render_offload_body(result_json);
    let saved = offload_to_workspace(
        body.as_ref(),
        fmt.extension(),
        session_id,
        tool_call_id,
        workspace_root,
    );
    // 元サイズ（規模のシグナル）はエンベロープ由来、bytes/lines/tokens は保存本文由来（C2）。
    oversized_notice(result_json.len(), body.as_ref(), saved.as_ref())
}

/// tool_result を永続化用の本文へ変換する（redaction → トークン上限/退避）。
///
/// - `workspace_root` が `Some` なら、上限超過分は `<root>/tmp/{session}-{tool_call_id}.{ext}`
///   （`ext` は中身に合わせて `txt`/`json`。#624）へ退避し、DB にはメタ情報（パス／バイト数／
///   行数／推定トークン数／読み方レシピ）だけの案内を残す。
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
        inline_limit_for_tool(tool_name),
    )
}

/// append 時: ツール別上限と残り会話枠の小さい方へ切り詰める。超えたら既存スタブ。
pub fn sanitize_tool_result_for_append(
    tool_name: &str,
    result_json: &str,
    session_id: &str,
    tool_call_id: &str,
    workspace_root: Option<&Path>,
    remaining: Option<usize>,
) -> String {
    sanitize_tool_result(
        tool_name,
        result_json,
        session_id,
        tool_call_id,
        workspace_root,
        append_limit_for_tool(tool_name, remaining),
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
        inline_limit_for_tool(tool_name),
    )
}

#[cfg(test)]
#[path = "tool_result_log/tests/mod.rs"]
mod tests;
