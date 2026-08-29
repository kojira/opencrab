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
fn read_recipe(rel: &str) -> String {
    format!(
        "To read it, run `grep -n <pattern> {rel}` to get matching line numbers, then call \
         `ws_read` on that path with `start_line`/`line_count` (pass a grep line number as \
         `start_line`, or `start_line=1` to read from the top); `ws_read` always keeps its \
         output under the inline limit. If you have no `ws_read` tool, run `head -c 2000 {rel}` \
         via execute_shell to read a bounded prefix (the byte cap keeps it under the limit). \
         The saved body is line-oriented text whose line numbers line up with `ws_read`."
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
mod tests {
    use super::*;

    /// 秘密を含まない結果は**改変されない**（byte 一致）。前フィルタで parse すらしない。
    #[test]
    fn read_predicate_is_the_single_source_for_both_decisions() {
        // 「読み」の定義は 1 つ。上限（退避するか）と参照化（持ち越すか）が同じ集合を指す。
        for t in ["ws_read", "ws_list"] {
            assert!(is_read_tool(t));
            assert_eq!(inline_limit_for_tool(t), READ_TOOL_RESULT_TOKEN_LIMIT);
        }
        for t in ["execute_shell", "search_my_history", "ws_write"] {
            assert!(!is_read_tool(t));
            assert_eq!(inline_limit_for_tool(t), TOOL_RESULT_TOKEN_LIMIT);
        }
        assert_eq!(
            append_limit_for_tool("ws_read", Some(1_000)),
            1_000,
            "残り枠がツール上限より狭いときは残り枠"
        );
        assert_eq!(
            append_limit_for_tool("ws_read", Some(80_000)),
            READ_TOOL_RESULT_TOKEN_LIMIT,
            "残り枠が広いときはツール上限"
        );
        assert_eq!(append_limit_for_tool("ws_read", Some(0)), 0);
        assert_eq!(
            append_limit_for_tool("ws_read", None),
            READ_TOOL_RESULT_TOKEN_LIMIT
        );
    }

    #[test]
    fn remaining_budget_below_result_spools_stub() {
        let json = format!(r#"{{"data":"{}"}}"#, "word ".repeat(800));
        assert!(
            crate::tokens::estimate_tokens(&json) > 200,
            "前提: 本文は残り枠より大きい"
        );
        let dir = tempfile::TempDir::new().unwrap();
        let out = sanitize_tool_result_for_append(
            "ws_read",
            &json,
            "sess",
            "tc-rem",
            Some(dir.path()),
            Some(200),
        );
        assert_ne!(out, json);
        assert!(
            out.contains("Tool result withheld"),
            "残り枠不足はスタブ: {out}"
        );
        assert!(
            out.contains("start_line") && out.contains("line_count"),
            "スタブは狭めて読み直せる導線を残す: {out}"
        );
    }

    #[test]
    fn sanitize_leaves_secretless_result_byte_identical() {
        let json = r#"{"success":true,"data":{"npub":"npub1ok","note":"hello"},"error":null}"#;
        let out = sanitize_tool_result_for_log("any_tool", json, "sess", "tc-1", None);
        assert_eq!(out, json);
    }

    /// #620: `nsec` を値/キーに含む結果も**マスクされず原文のまま**流れる（キー名マスクは
    /// 撤去した）。上限未満なので byte 一致で素通りすることを固定する（オフロード判定は不変）。
    #[test]
    fn sanitize_leaves_nsec_bearing_result_unmasked_now() {
        let json = r#"{"data":{"text":"the nsec format starts with nsec1"},"error":null}"#;
        let out = sanitize_tool_result_for_log("any_tool", json, "sess", "tc-1", None);
        assert_eq!(out, json, "サイズ上限未満は原文のまま流れる");
    }

    /// 秘密を持たないツールの結果は改変されない。
    #[test]
    fn sanitize_leaves_small_results_untouched() {
        let json = r#"{"success":true,"data":{"ok":true},"error":null}"#;
        let out = sanitize_tool_result_for_log("read_file", json, "sess", "tc-1", None);
        assert_eq!(out, json);
    }

    /// 上限超過はワークスペースへ退避し、DB 本文はメタ情報だけになる。
    /// #616: 退避本文は書き込み前に整形される（stdout の無い JSON は pretty）。生データは
    /// ファイルには入るが DB 本文（notice）には 1 バイトも混ざらない。
    #[test]
    fn sanitize_offloads_large_result_to_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = format!(r#"{{"data":"{}"}}"#, "x ".repeat(TOOL_RESULT_TOKEN_LIMIT));
        let out = sanitize_tool_result_for_log("read_file", &big, "sess1", "tc9", Some(dir.path()));
        assert!(out.contains("tmp/sess1-tc9.json"), "{out}");
        assert!(
            !out.contains("x x x"),
            "生データが DB 本文に混ざっている: {out}"
        );
        let saved = std::fs::read_to_string(dir.path().join("tmp/sess1-tc9.json")).unwrap();
        // stdout の無い JSON は pretty 化される（複数行になり head/grep が効く）。中身は等価。
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&saved).unwrap(),
            serde_json::from_str::<serde_json::Value>(&big).unwrap()
        );
        assert!(saved.contains('\n'), "pretty 化されていない: {saved:.80}");
    }

    /// #568/#616: 退避ファイルが [`OFFLOAD_FILE_BYTE_LIMIT`] を超えたら**先頭だけ**保存し、
    /// 切り詰めは**文字境界**で行う（バイト境界で切ると壊れた UTF-8 になる）。末尾が改行で
    /// 終わらなければ改行を 1 つ足す（ファイルを完結した行で終える。#619 レビュー）。
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

        let saved = offload_to_workspace(&big, "txt", "sessT", "tcT", Some(dir.path())).unwrap();
        assert_eq!(saved.rel_path, "tmp/sessT-tcT.txt");
        // 'あ' の手前（文字境界）＝ LIMIT-1 バイトまで保存（足した改行は数に含めない）。
        assert_eq!(saved.saved_prefix_bytes, Some(OFFLOAD_FILE_BYTE_LIMIT - 1));

        let on_disk = std::fs::read(dir.path().join("tmp/sessT-tcT.txt")).unwrap();
        // 元本文 LIMIT-1 バイト + 完結用の改行 1 バイト。
        assert_eq!(on_disk.len(), OFFLOAD_FILE_BYTE_LIMIT);
        assert!(on_disk.len() < big.len(), "切り詰められていない");
        assert_eq!(on_disk.last(), Some(&b'\n'), "改行で終わっていない");
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
        let saved =
            offload_to_workspace(&content, "txt", "sessU", "tcU", Some(dir.path())).unwrap();
        assert_eq!(
            saved.saved_prefix_bytes, None,
            "上限以下は切り詰めない（None）"
        );
        let on_disk = std::fs::read_to_string(dir.path().join("tmp/sessU-tcU.txt")).unwrap();
        assert_eq!(on_disk, content, "上限以下は 1 バイトも変わらない");
    }

    /// #635: UUID を含む id はハイフンをそのまま残す（`_` に潰さない）。潰すと「壊れた UUID」に
    /// 見え、モデルがパスを『直そう』として実在しないパスを渡し、退避ファイルを開けなくなる。
    /// 区切りも `-` に揃えるので、ファイル名に `_` は 1 つも現れず、通知が案内するパスと実ファイル
    /// のパスは完全一致する（通知をそのままコピーすれば開ける）。
    #[test]
    fn offload_keeps_uuid_hyphens_and_notice_path_matches_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let session_id = "web-e2e-test-bot-repro631c";
        let tool_call_id = "6f3fd055-711e-48da-8573-3bfedc778dd9";
        let big = format!(r#"{{"data":"{}"}}"#, "x ".repeat(TOOL_RESULT_TOKEN_LIMIT));
        let out = sanitize_tool_result_for_log(
            "read_file",
            &big,
            session_id,
            tool_call_id,
            Some(dir.path()),
        );

        // 検査対象は**実装が返す通知本文** out（テストが組んだ文字列ではない）。out から退避パス
        // 部分（`tmp/…json`）を取り出して調べる。通知の散文には `ws_read` / `start_line` など
        // `_` を含む語があるので、全文ではなくパス部分に絞る。
        let start = out.find("tmp/").expect("通知に退避パスが無い");
        let end =
            start + out[start..].find(".json").expect("退避パスに .json が無い") + ".json".len();
        let path_in_notice = &out[start..end];

        // (1) 実装が組んだパスに `_` が 1 つも現れない（区切りもハイフンに揃っている）。
        assert!(
            !path_in_notice.contains('_'),
            "退避パスに `_` が残っている: {path_in_notice}"
        );
        // (2) UUID が原形のまま**実装の出力に**現れる（潰れて `6f3fd055_711e_…` になっていない）。
        assert!(
            path_in_notice.contains(tool_call_id),
            "UUID が原形で残っていない: {path_in_notice}"
        );
        assert!(
            !out.contains("6f3fd055_711e"),
            "UUID をアンダースコアへ潰した形が通知に混じっている: {out}"
        );
        // 期待値は直書き。検査対象（実装の出力）と完全一致することを見る。
        let expected = format!("tmp/{session_id}-{tool_call_id}.json");
        assert_eq!(path_in_notice, expected, "通知パスが期待と違う");
        // (6) 通知が案内したパスをそのまま開ける（実ファイルが存在する）。
        assert!(
            dir.path().join(path_in_notice).exists(),
            "通知が案内したパスにファイルが無い: {path_in_notice}"
        );
    }

    /// #635: `/` や `..` を含む id でも、ワークスペース（`tmp/` 直下）の外へ出ない。`/` も `.` も
    /// 英数字でないので `-` に潰れ、パス区切りにならない＝親ディレクトリへ抜けられない。
    #[test]
    fn offload_never_escapes_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let tmp = root.join("tmp");
        for (sid, tid) in [
            ("../../etc", "6f3fd055-711e-48da-8573-3bfedc778dd9"), // `..` で親へ抜けようとする
            ("a/b/c", "tc/../../x"),                               // `/` と `..` の混在
        ] {
            let saved = offload_to_workspace("hello", "txt", sid, tid, Some(root)).unwrap();
            // rel_path は "tmp/<name>" の 2 コンポーネントのみ＝階層が増えていない。
            assert_eq!(
                std::path::Path::new(&saved.rel_path).components().count(),
                2,
                "階層が増えた（脱出の兆候）: {}",
                saved.rel_path
            );
            let full = root.join(&saved.rel_path);
            assert!(full.starts_with(&tmp), "tmp の外へ出た: {}", saved.rel_path);
            assert!(full.exists(), "実ファイルが無い: {}", saved.rel_path);
        }
    }

    /// #568/#616: notice は「全文保存」と「切り詰め保存」を区別し、どちらも元サイズ
    /// （エンベロープ由来）＋保存量（保存本文由来）の二段構え。切り詰め時は truncated を明記。
    #[test]
    fn oversized_notice_marks_truncation_vs_full() {
        // 全文保存（saved_prefix_bytes = None）: 全文を書いた旨。切り詰め表現は出ない。
        let full = OffloadResult {
            rel_path: "tmp/a.json".to_string(),
            saved_prefix_bytes: None,
        };
        // orig_bytes=42（エンベロープ）, body="original content"（16 バイト・保存本文）。
        let n_full = oversized_notice(42, "original content", Some(&full));
        assert!(
            n_full.contains("written in full to `tmp/a.json`"),
            "{n_full}"
        );
        // 規模のシグナル（42）と保存本文サイズ（16）の両方が出る。
        assert!(
            n_full.contains("was 42 bytes"),
            "規模のシグナルが無い: {n_full}"
        );
        assert!(
            n_full.contains("16 bytes, 1 lines"),
            "保存本文の数が無い: {n_full}"
        );
        // #619 レビュー: エンベロープ長は "serialized result" と名乗る（"original tool
        // output" と言うとエスケープ水増し値を「元の出力」として再報告してしまう）。
        assert!(
            n_full.contains("the serialized result was"),
            "規模の文言が serialized result でない: {n_full}"
        );
        assert!(
            !n_full.contains("original tool output"),
            "誤解を招く original tool output が残っている: {n_full}"
        );
        assert!(
            !n_full.contains("Only the first"),
            "全文保存で切り詰め表現が出ている: {n_full}"
        );
        // #624: 全文保存でも読み方のレシピ（grep -n → ws_read / head -c）が入り、パスを指す。
        assert!(
            n_full.contains("grep -n <pattern> tmp/a.json"),
            "全文保存にレシピが無い: {n_full}"
        );
        assert!(n_full.contains("ws_read"), "ws_read 導線が無い: {n_full}");
        assert!(
            n_full.contains("head -c 2000 tmp/a.json"),
            "head -c 導線が無い: {n_full}"
        );
        // #624 レビュー: 上限を守らない sed -n は誘導しない（自己ループ防止）。
        assert!(
            !n_full.contains("sed "),
            "上限を守らない sed が残っている: {n_full}"
        );

        // 切り詰め保存（saved_prefix_bytes = Some）: 元サイズ・保存量・truncated を明記。
        let trunc = OffloadResult {
            rel_path: "tmp/b.json".to_string(),
            saved_prefix_bytes: Some(123),
        };
        // orig_bytes=9999（エンベロープ）だが保存したのは body の先頭 123 バイト。
        let body = "x".repeat(9999);
        let n_trunc = oversized_notice(9999, &body, Some(&trunc));
        assert!(
            n_trunc.contains("Only the first 123 bytes"),
            "保存量が無い: {n_trunc}"
        );
        assert!(
            n_trunc.contains("was 9999 bytes"),
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
        // #624: 打ち切りケースにも同じ読み方レシピが入り、正しいパス（tmp/b.json）を指す。
        assert!(
            n_trunc.contains("grep -n <pattern> tmp/b.json"),
            "打ち切りにレシピが無い: {n_trunc}"
        );
        assert!(n_trunc.contains("ws_read"), "ws_read 導線が無い: {n_trunc}");
        assert!(
            n_trunc.contains("head -c 2000 tmp/b.json"),
            "head -c 導線が無い: {n_trunc}"
        );
        assert!(
            !n_trunc.contains("sed "),
            "上限を守らない sed が残っている: {n_trunc}"
        );
        // 打ち切りは「先頭だけ」であることを明示（全体像の誤読を避ける）。
        assert!(
            n_trunc.contains("only the saved prefix"),
            "先頭のみの明示が無い: {n_trunc}"
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
        // 案内はメタ情報＋読み方レシピ＋狭めて取り直す導線だけで、なお小さい（76KB → 1KB 台）。
        // #624: 読み方レシピ（grep -n → ws_read / head -c）を足したぶん増えたが、生データを
        // 載せないので依然として桁違いに小さい。上限（2,500 トークン ≒ 数 KB）を食い破らない。
        assert!(out.len() < 1_400, "案内が肥大している: {} bytes", out.len());

        // 全文は退避され、そこを指している（#616: stdout の無い JSON は pretty 化）。
        assert!(out.contains("tmp/sessA-tc1.json"), "{out}");
        let saved = std::fs::read_to_string(dir.path().join("tmp/sessA-tc1.json")).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&saved).unwrap(),
            serde_json::from_str::<serde_json::Value>(&big).unwrap()
        );
        // 通知の bytes は「保存本文（pretty）」の実サイズと一致する（C2）。元サイズ
        // （エンベロープ）も規模のシグナルとして併記される。
        assert!(
            out.contains(&format!("Its saved form ({} bytes", saved.len())),
            "保存本文サイズが notice と一致しない: {out}"
        );
        assert!(
            out.contains(&format!("was {} bytes", big.len())),
            "元サイズ（エンベロープ）が notice に無い: {out}"
        );
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

        // #624: 生テキスト（parse 失敗の verbatim）は .txt。
        assert!(out.contains("tmp/sessB-tc2.txt"), "パスが無い: {out}");
        assert!(
            out.contains(&format!("{} bytes", big.len())),
            "バイトサイズが無い: {out}"
        );
        assert!(out.contains("3 lines"), "行数が無い: {out}");
        // 案内のトークン数は概算（`~` 付きの目安）。判定と同じ有界推定を使う（#576）。
        assert!(
            out.contains(&format!(
                "~{} tokens",
                crate::tokens::estimate_tokens_bounded(&big)
            )),
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

    /// 判定は**トークン基準**なので、日本語が「バイト量が多い」だけで不当に早く退避される
    /// ことはない（#294 の趣旨。#576 で全体トークナイズはやめたが単位はトークンのまま）。
    ///
    /// 日本語 1 文字 3 バイトの本文は、バイトで測ると実効トークン量よりずっと大きく見える。
    /// トークン数が上限未満なら、バイト数が上限相当を超えていても素通りする。
    #[test]
    fn japanese_text_is_measured_in_tokens_not_bytes() {
        // トークン上限に迫る量の日本語（バイトでは上限バイト換算 ~10KB を意識した長さ）だが、
        // トークン数では上限未満。ここで退避されないことを担保する。
        let json = format!(r#"{{"data":"{}"}}"#, "こんにちは世界".repeat(220));
        // バイトでは「上限トークン数」という数値（2,500）をゆうに超える一方…
        assert!(
            json.len() > TOOL_RESULT_TOKEN_LIMIT,
            "前提: バイトは 2,500 超"
        );
        // …トークンでは上限未満。だから退避されない。
        assert!(crate::tokens::estimate_tokens(&json) < TOOL_RESULT_TOKEN_LIMIT);
        let out = sanitize_tool_result_for_llm("read_file", &json, "sess", "tc-1", None);
        assert_eq!(out, json);
    }

    /// 退避判定は上限（2,500 トークン）の直下・直上・マルチバイト境界で、**正確な**
    /// トークン数と同じ側に落ちる（#576 の有界判定が境界をズラさない）。これらの本文は複数窓を
    /// 跨ぐが、CJK・空白区切りのトークンは窓境界（[`crate::tokens::BOUNDED_TOKENIZE_WINDOW`]）を
    /// 跨がないのでチャンク境界の上振れは出ない（上振れは base64/単一文字の長大ランのみ）。
    #[test]
    fn exceeds_limit_agrees_with_exact_token_count_across_boundary() {
        let samples: Vec<String> = vec![
            "あ".repeat(2_400),
            "あ".repeat(2_450),
            "あ".repeat(2_550),
            "あ".repeat(2_600),
            "word ".repeat(1_800),
            "word ".repeat(2_600),
            // マルチバイト＋ASCII 混在。複数窓を跨ぐが CJK/空白でトークンは境界を跨がない。
            format!("{}{}", "あ".repeat(1_250), "word ".repeat(1_250)),
        ];
        for s in &samples {
            let exact = crate::tokens::estimate_tokens(s);
            assert_eq!(
                exceeds_limit(s, TOOL_RESULT_TOKEN_LIMIT),
                exact >= TOOL_RESULT_TOKEN_LIMIT,
                "len={}, exact={exact}",
                s.len(),
            );
        }
    }

    /// 長い単一文字ラン（区切りの無い 1 pre-token）でも判定は返り、退避される。
    /// 全体を一括トークナイズしていたら 486MB 級で固まる経路を、有界判定が塞ぐ（#576）。
    /// 時間アサートは不安定なので、ここでは**判定が返って退避されること**だけを見る。
    #[test]
    fn huge_single_run_is_offloaded_without_hanging() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = "あ".repeat(100_000); // 300KB・単一 pre-token・確実に上限超
        let out =
            sanitize_tool_result_for_llm("execute_shell", &big, "sessR", "tcR", Some(dir.path()));
        assert!(out.contains("withheld"), "退避されていない: {out}");
        assert!(!out.contains("ああああ"), "生データが流れている");
        // 退避ファイルに全文が入っている（#624: 非 JSON は .txt）。
        let saved = std::fs::read_to_string(dir.path().join("tmp/sessR-tcR.txt")).unwrap();
        assert_eq!(saved.len(), big.len());
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
            !exceeds_limit(&out, TOOL_RESULT_TOKEN_LIMIT),
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

    /// #620: LLM 経路でも nsec キー名マスクは**しない**（撤去）。上限未満なので原文のまま。
    #[test]
    fn llm_result_no_longer_key_masks() {
        let json = r#"{"success":true,"data":{"nsec":"nsec1synthetic"},"error":null}"#;
        let out = sanitize_tool_result_for_llm("nostr_generate_key", json, "sess", "tc-1", None);
        assert_eq!(out, json, "上限未満は原文のまま（マスクしない）");
    }

    // ---- #616: 退避本文の整形（render_offload_body）と行境界打ち切り ----

    /// C3: shell 成功系（exit_code==0 かつ stderr 空）は**ヘッダ無し**で stdout を verbatim。
    /// 実改行が保たれ、`\n` の 2 文字化が起きない。
    #[test]
    fn render_shell_success_is_headerless_verbatim() {
        let stdout = "first line\nsecond line\n{\"json\":\"payload\"}\n";
        let env = serde_json::json!({
            "success": true,
            "data": {"stdout": stdout, "stderr": "", "exit_code": 0, "truncated": false},
            "error": null
        })
        .to_string();
        let (body, fmt) = render_offload_body(&env);
        assert_eq!(body.as_ref(), stdout, "ヘッダ無しで stdout そのまま");
        assert!(!body.contains("\\n"), "\\n が 2 文字化している: {body}");
        assert!(!body.contains("exit_code="), "成功系にヘッダが付いた");
        // #624: shell 生テキストは .txt。
        assert_eq!(fmt, OffloadFormat::Text);
        assert_eq!(fmt.extension(), "txt");
    }

    /// C3: 非ゼロ終了 or stderr 非空はヘッダが付く。stdout/stderr は生テキスト。
    #[test]
    fn render_shell_failure_gets_header() {
        // 非ゼロ終了。
        let env = serde_json::json!({
            "success": true,
            "data": {"stdout": "partial\noutput", "stderr": "boom\n", "exit_code": 2, "truncated": false},
            "error": null
        })
        .to_string();
        let (body, fmt) = render_offload_body(&env);
        assert!(body.starts_with("exit_code=2\n"), "{body}");
        assert!(body.contains("--- stderr ---\nboom\n"), "{body}");
        assert!(body.contains("--- stdout ---\npartial\noutput"), "{body}");
        // #624: ヘッダ付きでも shell 由来なので生テキスト＝ .txt。
        assert_eq!(fmt, OffloadFormat::Text);

        // exit_code==0 でも stderr 非空ならヘッダ。
        let env2 = serde_json::json!({
            "success": true,
            "data": {"stdout": "ok", "stderr": "warning", "exit_code": 0, "truncated": false},
            "error": null
        })
        .to_string();
        let (body2, fmt2) = render_offload_body(&env2);
        assert!(body2.starts_with("exit_code=0\n"), "{body2}");
        assert!(body2.contains("--- stderr ---\nwarning"), "{body2}");
        assert_eq!(fmt2, OffloadFormat::Text);
    }

    /// (b): stdout の無い JSON は pretty 化され、format_hint が "JSON object" を出す。
    #[test]
    fn render_structured_json_is_pretty() {
        let env = r#"{"success":true,"data":{"items":[1,2,3]},"error":null}"#;
        let (body, fmt) = render_offload_body(env);
        assert!(body.contains('\n'), "pretty 化されていない: {body}");
        assert!(body.contains("  "), "インデントが無い: {body}");
        assert_eq!(format_hint(&body), Some("looks like a JSON object"));
        // #624: pretty JSON は .json（jq が通る）。
        assert_eq!(fmt, OffloadFormat::Json);
        assert_eq!(fmt.extension(), "json");
        // 中身は等価。
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::from_str::<serde_json::Value>(env).unwrap()
        );
    }

    /// (c): parse 失敗は生バイト verbatim（借用のまま＝再割り当てしない）。
    /// #624: JSON ではないので .txt（`jq` を誘導しない）。
    #[test]
    fn render_non_json_is_borrowed_verbatim() {
        let raw = "not json at all\nline2\n";
        let (body, fmt) = render_offload_body(raw);
        assert_eq!(body.as_ref(), raw);
        assert!(matches!(body, std::borrow::Cow::Borrowed(_)));
        assert_eq!(fmt, OffloadFormat::Text);
        assert_eq!(fmt.extension(), "txt");
    }

    /// #619 レビュー: 打ち切りは**文字境界**でほぼ全量を保存し、末尾を改行で終える。行境界で
    /// 切る旧実装だと本文全体を捨てかねない（次テスト参照）ので採らない。改行を含む本文でも
    /// 「行の途中で切れる」ことは許容し（生テキストは読めて grep も効く）、ファイル完結は末尾の
    /// 改行 1 つで担保する。
    #[test]
    fn offload_truncates_at_char_boundary_and_ends_with_newline() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut body = String::with_capacity(OFFLOAD_FILE_BYTE_LIMIT + 8_000);
        while body.len() <= OFFLOAD_FILE_BYTE_LIMIT + 4_000 {
            body.push_str(&"x".repeat(1_000));
            body.push('\n');
        }
        assert!(body.len() > OFFLOAD_FILE_BYTE_LIMIT);

        let saved = offload_to_workspace(&body, "txt", "sL", "tL", Some(dir.path())).unwrap();
        let n = saved.saved_prefix_bytes.expect("切り詰められている");
        // 文字境界＝全部 ASCII なので上限ちょうど。ほぼ全量（上限分）を保存する。
        assert_eq!(n, OFFLOAD_FILE_BYTE_LIMIT);

        let on_disk = std::fs::read_to_string(dir.path().join("tmp/sL-tL.txt")).unwrap();
        assert!(on_disk.ends_with('\n'), "改行で終わっていない");
        // 上限ぶん + 完結用の改行（本文末尾がちょうど改行なら足さないが、この本文は途中で切れる）。
        assert!(
            on_disk.len() >= OFFLOAD_FILE_BYTE_LIMIT,
            "ほぼ全量が保存されていない"
        );
    }

    /// #619 レビューの回帰: 「早い位置に改行が 1 つ + 改行なしの巨大本文」で、**ほぼ全量**が
    /// 保存されること。窓内の最後の改行で切る旧実装だと end=7 になり、保存できたはずの ~10MB を
    /// 7 バイトへ激減させていた。文字境界で切る新実装はこれを起こさない。
    #[test]
    fn offload_early_single_newline_still_saves_near_full() {
        let dir = tempfile::TempDir::new().unwrap();
        // 7 バイト目に改行が 1 つ、以降は改行ゼロで上限超。
        let body = format!("header\n{}", "a".repeat(OFFLOAD_FILE_BYTE_LIMIT + 500));
        let saved = offload_to_workspace(&body, "txt", "sE", "tE", Some(dir.path())).unwrap();
        let n = saved.saved_prefix_bytes.expect("切り詰められている");
        // 旧実装なら 7。新実装は上限ちょうど（全部 ASCII）。
        assert_eq!(
            n, OFFLOAD_FILE_BYTE_LIMIT,
            "早い改行でデータが激減した（退行）"
        );
        let on_disk = std::fs::read_to_string(dir.path().join("tmp/sE-tE.txt")).unwrap();
        assert!(on_disk.ends_with('\n'), "改行で終わっていない");
        assert!(on_disk.len() > body.len() / 2, "ほぼ全量が保存されていない");
    }

    /// 改行が 1 つも無い 10MiB 超の本文でも、空ファイルにせずほぼ全量を保存し、末尾に改行を
    /// 足してファイルを完結させる（文字境界で切る＝壊れた UTF-8 にしない）。
    #[test]
    fn offload_no_newline_saves_near_full_and_appends_newline() {
        let dir = tempfile::TempDir::new().unwrap();
        let body = "a".repeat(OFFLOAD_FILE_BYTE_LIMIT + 500); // 改行ゼロ
        let saved = offload_to_workspace(&body, "txt", "sN", "tN", Some(dir.path())).unwrap();
        let n = saved.saved_prefix_bytes.expect("切り詰められている");
        // 全部 ASCII なので文字境界＝上限ちょうど。
        assert_eq!(n, OFFLOAD_FILE_BYTE_LIMIT);
        let on_disk = std::fs::read(dir.path().join("tmp/sN-tN.txt")).unwrap();
        assert!(!on_disk.is_empty(), "空ファイル");
        // 上限ぶん + 足した改行 1 バイト。
        assert_eq!(on_disk.len(), OFFLOAD_FILE_BYTE_LIMIT + 1);
        assert_eq!(on_disk.last(), Some(&b'\n'), "改行で終わっていない");
        assert!(std::str::from_utf8(&on_disk).is_ok(), "壊れた UTF-8");
    }

    /// C2 統合: shell の巨大 stdout を退避すると、ファイルは**行が保たれ**部分読み・検索が効き、
    /// notice の bytes/lines/tokens が**実ファイル**と一致する（「1 lines」にならない）。
    #[test]
    fn shell_offload_preserves_lines_and_notice_counts_match_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let stdout_text = (0..4_000)
            .map(|i| format!("row {i:05} value"))
            .collect::<Vec<_>>()
            .join("\n"); // 4000 行・末尾改行なし
        let env = serde_json::json!({
            "success": true,
            "data": {"stdout": stdout_text, "stderr": "", "exit_code": 0, "truncated": false},
            "error": null
        })
        .to_string();
        // エンベロープは上限を超える。
        assert!(exceeds_limit(&env, TOOL_RESULT_TOKEN_LIMIT), "前提: 上限超");

        let out = sanitize_tool_result_for_llm("execute_shell", &env, "sX", "tX", Some(dir.path()));

        // ファイルは stdout そのもの（実改行・ヘッダ無し）。#624: shell 生テキストは .txt。
        let saved = std::fs::read_to_string(dir.path().join("tmp/sX-tX.txt")).unwrap();
        assert_eq!(saved, stdout_text);
        assert!(!saved.contains("\\n"), "\\n が 2 文字化している");
        assert_eq!(count_lines(&saved), 4_000);

        // notice は保存本文の実数と一致する（C2: 「1 lines」にならない）。
        assert!(
            out.contains("4000 lines"),
            "行数が保存本文と一致しない: {out}"
        );
        assert!(
            out.contains(&format!("{} bytes", saved.len())),
            "保存本文サイズが notice に無い: {out}"
        );
        assert!(
            out.contains(&format!(
                "~{} tokens",
                crate::tokens::estimate_tokens_bounded(&saved)
            )),
            "トークン数が保存本文基準でない: {out}"
        );
        // shell の生テキストは "JSON object" と偽らない（C4: format_hint は保存本文基準）。
        assert!(
            !out.contains("looks like a JSON"),
            "生テキストを JSON と偽った: {out}"
        );
    }

    /// #620: nsec キー名マスクは撤去したので、退避（オフロード）でも notice には生データが
    /// 1 バイトも入らない（#294 の性質は不変）が、退避ファイル本文はマスクされずそのまま
    /// 書かれる（キー名マスクの復活が無いこと＝撤去の固定）。
    #[test]
    fn offload_does_not_key_mask_saved_body() {
        let dir = tempfile::TempDir::new().unwrap();
        let filler = "z".repeat(TOOL_RESULT_TOKEN_LIMIT * 4);
        let env = format!(
            r#"{{"success":true,"data":{{"nsec":"nsec1synthetic","note":"{filler}"}},"error":null}}"#
        );
        let out = sanitize_tool_result_for_llm("any_tool", &env, "sS", "tS", Some(dir.path()));
        // notice（inline）には生データを載せない（#294 は不変）。
        assert!(!out.contains("nsec1synthetic"), "notice に生データ: {out}");
        assert!(
            out.contains("Tool result withheld"),
            "退避 notice でない: {out}"
        );
        // 退避ファイル本文はキー名マスクされない（撤去の固定）。#624: 構造化 JSON は .json。
        let saved = std::fs::read_to_string(dir.path().join("tmp/sS-tS.json")).unwrap();
        assert!(
            !saved.contains("[redacted]"),
            "撤去したはずのキー名マスクが復活している: {saved:.120}"
        );
    }

    // ---- #624: 拡張子を中身に合わせる / 通知に読み方レシピを入れる ----

    /// #624: 退避ファイルの拡張子は**中身**に合わせる。shell 生テキストと parse 失敗の
    /// verbatim は `.txt`（JSON ではないので `jq` を誘導しない）、pretty JSON は `.json`。
    /// sanitize の全経路で実ファイルが正しい拡張子で作られることを 1 か所で固定する。
    #[test]
    fn offload_extension_matches_content() {
        let filler = "word ".repeat(TOOL_RESULT_TOKEN_LIMIT); // 確実に上限超

        // (a) shell 生テキスト（data.stdout が string・成功系）→ .txt。
        let dir_a = tempfile::TempDir::new().unwrap();
        let shell_env = serde_json::json!({
            "success": true,
            "data": {"stdout": filler.clone(), "stderr": "", "exit_code": 0, "truncated": false},
            "error": null
        })
        .to_string();
        let out_a = sanitize_tool_result_for_llm(
            "execute_shell",
            &shell_env,
            "sA",
            "tA",
            Some(dir_a.path()),
        );
        assert!(
            dir_a.path().join("tmp/sA-tA.txt").exists(),
            "shell が .txt でない"
        );
        assert!(
            !dir_a.path().join("tmp/sA-tA.json").exists(),
            ".json も作られた"
        );
        assert!(
            out_a.contains("tmp/sA-tA.txt"),
            "notice のパスが .txt でない: {out_a}"
        );

        // (b) 構造化 JSON（stdout string 無し）→ .json。
        let dir_b = tempfile::TempDir::new().unwrap();
        let json_env = format!(r#"{{"success":true,"data":{{"note":"{filler}"}},"error":null}}"#);
        let out_b =
            sanitize_tool_result_for_llm("read_file", &json_env, "sB", "tB", Some(dir_b.path()));
        assert!(
            dir_b.path().join("tmp/sB-tB.json").exists(),
            "JSON が .json でない"
        );
        assert!(
            out_b.contains("tmp/sB-tB.json"),
            "notice のパスが .json でない: {out_b}"
        );

        // (c) parse 失敗の verbatim（非 JSON）→ .txt。
        let dir_c = tempfile::TempDir::new().unwrap();
        let raw = "line one\n".repeat(TOOL_RESULT_TOKEN_LIMIT); // 非 JSON・上限超
        let out_c =
            sanitize_tool_result_for_llm("execute_shell", &raw, "sC", "tC", Some(dir_c.path()));
        assert!(
            dir_c.path().join("tmp/sC-tC.txt").exists(),
            "verbatim が .txt でない"
        );
        assert!(
            out_c.contains("tmp/sC-tC.txt"),
            "notice のパスが .txt でない: {out_c}"
        );
    }

    /// #624: 上限超過の通知（全文保存）に**具体的な読み方レシピ**が入る。`grep -n` で行番号 →
    /// `ws_read`、または `ws_read` を持たない caller 向けに `head -c`。読む手段が無い caller 向け
    /// の再実行導線も残る。#624 レビュー: 上限を守らない `sed -n` は誘導しない（自己ループ防止）。
    #[test]
    fn oversized_notice_carries_read_recipe() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = "row value\n".repeat(TOOL_RESULT_TOKEN_LIMIT); // 非 JSON・上限超 → .txt 全文保存
        let out = sanitize_tool_result_for_llm("execute_shell", &big, "sR", "tR", Some(dir.path()));

        // レシピの具体操作がパス入りで出る。
        assert!(
            out.contains("grep -n <pattern> tmp/sR-tR.txt"),
            "grep 導線が無い: {out}"
        );
        assert!(out.contains("ws_read"), "ws_read 導線が無い: {out}");
        assert!(out.contains("start_line"), "start_line が無い: {out}");
        // #624 レビュー: バイト頭打ちの head -c だけ（sed -n は落とした）。
        assert!(
            out.contains("head -c 2000 tmp/sR-tR.txt"),
            "head -c 導線が無い: {out}"
        );
        assert!(
            !out.contains("sed "),
            "上限を守らない sed が残っている: {out}"
        );
        // 読む手段が無い caller 向けの再実行導線は残す。
        assert!(
            out.contains("If you cannot read that file"),
            "非リーダー向け導線が無い: {out}"
        );
        assert!(
            out.contains("Do NOT re-run the same tool"),
            "ループ防止が無い: {out}"
        );
    }
}
