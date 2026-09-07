//! Claude Code のセッションログ（`~/.claude/projects/<project>/<session>.jsonl`）を
//! `memory_sessions` へ取り込む経路（#413 段階1）。
//!
//! 既存の [`crate::import::import_service`] は **OpenClaw のワークスペース**
//! （`soul` / `identity` / `memory_curated` / `daily_logs` / `skills`）を取り込むもので、
//! `.jsonl` の生ログは対象外。ここは**別経路**として作り、既存の取り込みには触らない。
//!
//! このモジュールは**純粋な変換**だけを持つ（ファイルを読み、行を並べ、DB へ入れる形を
//! 組み立てる）。DB への書き込みと退避ファイルの生成は呼び出し側（`opencrab-import-claude-code`）
//! が行う。テストを合成データだけで書けるようにするため。
//!
//! # 何を取り込み、何を捨てるか（#413 で確定）
//!
//! `.jsonl` の 1 行は `type` を持ち、`assistant` / `user` だけが体験で、それ以外は運用の
//! メタ情報。さらに `assistant` / `user` の中身（`message.content`）はブロックの配列で、
//! ここにツール往復が混ざる。**行の種別だけでは足りず、ブロック単位で選別する。**
//!
//! ```text
//! 残す    assistant:text        → speech（発話者＝本人）
//!         user:text             → speech（発話者＝相手）
//!         `promptSource: "system"` の user 行のうち他セッションからの連絡 → speech（発話者＝system）
//! 印だけ  assistant:thinking    → inner_voice（本文は退避、生ログには印だけ）
//! 捨てる  assistant:tool_use / user:tool_result / user:image
//!         `promptSource: "system"` かつ本文が task-notification の user 行
//!         `isSidechain: true`（サブエージェント＝道具の動き）の行すべて
//!         非対話の type すべて（file-history-snapshot / queue-operation / pr-link /
//!         last-prompt / mode / permission-mode / ai-title / bridge-session / attachment /
//!         system / custom-title / file-history-delta / frame-link …）
//! ```
//!
//! 根拠（1 プロジェクト = 5 セッション / 22,821 行の実測）: ツール往復
//! （`tool_use` + `tool_result`）が本文（`text`）の 4.4 倍あり、落とすだけで容量の 39% が
//! 消えて材料の密度が上がる。非対話の type は**運用のメタ情報**であって体験ではなく、
//! 取り込むと #393 で消した「整備作業が記憶になる」のと同じ状態になる。
//!
//! `user` 行のうち `promptSource: "system"` で**本文が task-notification の行だけ**落とす。
//! task-notification はバックグラウンドタスクの完了通知＝サブタスクの成果物ダンプで、本人の
//! 体験ではなく道具が返した作業結果。#393 と同じ形なので落とす（#413）。同じ
//! `promptSource: "system"` でも他セッションからの連絡（agent-message 等）は「他者から連絡を
//! 受け判断を迫られた」対人的体験なので取り込み、`speaker_id` = `system` にする（オーナーの
//! UX 判断 2026-08-07）。`type: "system"` の非対話行とは別物で、こちらは `type: "user"`。
//!
//! `isSidechain: true` のレコード（サブエージェント）も **type を問わず落とす**。これは本人が
//! 呼んだ道具の内部の動きであって本人の体験ではなく、task-notification と同じ「道具の動きは
//! 記憶に入れない」判断軸（#413）。実データではサブエージェントのログは
//! `<project>/<uuid>/subagents/` 配下の別ファイルにあり [`scan_project_dir`] の非再帰
//! `read_dir` でも読まれないが、そこに依存せず**フラグを明示的に見て落とす**（偶然の
//! 非取り込みに頼ると、inline `isSidechain: true` が top-level ファイルに現れたとき無ガードで
//! 混ざる）。
//!
//! 捨てる側を**列挙しない**のが要点。`assistant:text` / `user:text` / `assistant:thinking`
//! 以外はすべて落ちるので、Claude Code 側に新しい `type` が増えても勝手に混ざらない。
//!
//! # `assistant:thinking` は参照だけ残す
//!
//! thinking は本文の 1.7 倍あり、生ログへ展開すると本文の密度が落ちる。かといって
//! 捨てると「なぜそう考えたか」が残らない（結論しか書かれていない本文からは復元できない）。
//! そこで**生ログには短い印だけを置き、全文はワークスペースへ退避**する。
//!
//! リポジトリ内の先例 2 つのうち、**先例 1（[`crate::tool_result_log`] のワークスペース
//! 退避）**に倣う。理由:
//!
//! - `memory_index_nodes`（先例 2）は**宣言済みの記憶の索引**で、`search_memory_index` の
//!   検索対象であり #403 が文脈へ注入する先でもある。生の thinking をそこへ入れると、
//!   索引が材料で埋まって「整備作業が記憶になる」のと同じ壊れ方をする。索引は宣言の
//!   出力を置く場所で、材料を置く場所ではない。
//! - 先例 1 は「本文が大きすぎるので参照へ置き換える」というこの問題そのものの形で、
//!   スキーマ変更が要らない。退避先はエージェントのワークスペース＝本人がファイル
//!   読み取りで辿れる場所なので、**印が退避先のパスを名乗るだけで参照が成立する**
//!   （先例 1 の案内文がパスを載せているのと同じ）。
//!
//! 識別子の付け方だけは先例 2（`memory_index_nodes.short_id` の `t1` / `u1` / `p1`）に
//! 揃え、`th1` / `th2` … の連番にする。ハッシュより短く、順序が読める。
//!
//! 印を**引くための道具と、その存在をプロンプトに書くこと**は段階2（宣言ラン）の仕事で、
//! ここでは扱わない。#403 は「作ったのに存在を知らされず誰も引かなかった」失敗をしている。
//!
//! ## ただし実データに thinking の本文はほぼ入っていない
//!
//! 実測（全プロジェクトを走査）: `thinking` ブロック 12,121 個のうち、`thinking` の
//! **文字列が空でないものは 32 個**（計 26,932 文字）しかない。残りは `thinking: ""` と
//! 不透明な `signature` だけで、**思考の本文はログに書かれていない**。#413 が
//! 「thinking は本文の 1.7 倍」と見積もったバイト数は、この `signature` のバイト数。
//!
//! したがって上の参照方式は**実装はされているが、いまの材料ではほぼ印が立たない**。
//! 本文が空のブロックは印を作らずに落とす（空ファイルを指す死んだ参照を残さない）。
//! 本文が入る形式に変わったとき / 32 個のような例外に当たったときだけ印が出る。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 生ログに残す thinking の印に付ける接頭辞（`th1` / `th2` …）。
///
/// `memory_index_nodes.short_id` の `t`（topic）と衝突させないため 2 文字にしてある。
/// 両者は別のテーブルで名前空間も別だが、**エージェントが読む文字列としては同じ場所に
/// 並ぶ**（宣言ランは索引の `[u28]` と生ログの `[think:th28]` を同じ画面で見る）ので、
/// 見た目で区別が付かないと参照先を取り違える。
const THINKING_ID_PREFIX: &str = "th";

/// thinking の全文を退避するワークスペース相対ディレクトリ。
///
/// `tmp/`（[`crate::tool_result_log`] の退避先）とは分ける。あちらは 1 ターン限りの
/// 使い捨てで消えて構わないが、こちらは生ログの印が指し続ける参照先で、消えると
/// 印だけが残って辿れなくなる。
pub const THINKING_DIR: &str = "claude_code/thinking";

/// `metadata_json` に入れる由来の印。この値で「この取り込みが入れた行」を識別する
/// （重複防止と、既存エージェントへの誤爆の検出に使う）。
pub const SOURCE_TAG: &str = "claude_code";

/// Claude Code の相手（＝人間のオーナー）を表す `speaker_id`。
///
/// #382 の規約は `agent_id` = 受信者 / `speaker_id` = 送信者。Claude Code のログには
/// 相手の識別子が入っていない（`userType: "external"` しか無い）ので、**取り込み側で
/// 1 つの定数を与える**。定数にしておくと `is_user_speech`（`speaker_id != agent_id`）が
/// 期待どおり働き、impressions も 1 人の相手として一貫する。
pub const USER_SPEAKER_ID: &str = "claude-code-user";

/// 機械が差し込んだ `user` 行（`promptSource: "system"`）の `speaker_id`。
///
/// 実データの内訳は task-notification（バックグラウンドタスクの完了通知）と
/// 他セッションからのエージェント間メッセージで、**人間の発言ではない**。#382 の規約が
/// `speaker_id` = 送信者である以上、人間と同じ名義にすると「誰が言ったか」が壊れる。
/// opencrab 側で既に使われている `"system"` に揃える。なお task-notification は
/// [`plan_record`] で落とす（#413）ので、この名義が実際に付くのは他セッションからの連絡。
pub const SYSTEM_SPEAKER_ID: &str = "system";

/// 取り込んだ 1 行が誰の発言かの区別。`speaker_id` 列へ落ちる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    /// 本人（`assistant`）。`speaker_id` = `agent_id`。
    Agent,
    /// 相手＝人間のオーナー（`user`）。
    User,
    /// 機械が差し込んだ `user` 行（他セッションからの連絡など）。`speaker_id` = `system`。
    /// ただし本文が task-notification（道具の成果物ダンプ）の行は [`plan_record`] で
    /// 落とす（#413）。取り込むのは他セッションからの連絡のような対人的体験だけ。
    System,
}

impl Speaker {
    /// `agent_id` を与えて実際の `speaker_id` を決める。
    pub fn speaker_id(self, agent_id: &str) -> String {
        match self {
            Speaker::Agent => agent_id.to_string(),
            Speaker::User => USER_SPEAKER_ID.to_string(),
            Speaker::System => SYSTEM_SPEAKER_ID.to_string(),
        }
    }
}

/// `memory_sessions` へ 1 行として入る単位。
#[derive(Debug, Clone)]
pub struct PlannedRow {
    /// `cc-<Claude Code セッション UUID>`。
    pub session_id: String,
    /// `speech` か `inner_voice`。
    pub log_type: String,
    pub speaker: Speaker,
    /// 本文。thinking のときは**印だけ**（全文は [`Self::thinking_body`]）。
    pub content: String,
    /// RFC3339（DB の他の行と同じ表記へ正規化済み）。
    pub created_at: String,
    /// 元レコードの `uuid`。重複防止の鍵の一部。
    pub source_uuid: String,
    /// 元レコードの `message.content` 配列内の位置。重複防止の鍵の一部。
    pub block_index: usize,
    pub git_branch: Option<String>,
    /// thinking の全文（退避先へ書く）。thinking 以外は `None`。
    pub thinking_body: Option<String>,
    /// thinking に割り当てた短 id（`th1` …）。thinking 以外は `None`。
    pub thinking_id: Option<String>,
}

impl PlannedRow {
    /// 重複防止の鍵。`(セッション, 元レコード uuid, ブロック位置)` で 1 行が一意に決まる。
    pub fn dedup_key(&self) -> (String, String, usize) {
        (
            self.session_id.clone(),
            self.source_uuid.clone(),
            self.block_index,
        )
    }

    /// `metadata_json` に載せる由来。重複防止はここを読んで復元する。
    pub fn metadata_json(&self) -> String {
        let mut meta = serde_json::json!({
            "source": SOURCE_TAG,
            "uuid": self.source_uuid,
            "block": self.block_index,
        });
        if let Some(branch) = &self.git_branch {
            meta["git_branch"] = serde_json::json!(branch);
        }
        if let Some(tid) = &self.thinking_id {
            meta["think_id"] = serde_json::json!(tid);
        }
        meta.to_string()
    }

    /// thinking の全文を書くワークスペース相対パス。thinking 以外は `None`。
    pub fn thinking_rel_path(&self) -> Option<String> {
        self.thinking_id
            .as_ref()
            .map(|tid| format!("{THINKING_DIR}/{tid}.txt"))
    }
}

/// 種別ごとの件数とバイト数（「何を取り込み、何を捨てたか」の実測）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KindStat {
    pub count: usize,
    pub bytes: usize,
}

/// 走査の集計。取り込み結果の報告に使う。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanStats {
    /// `assistant:text` / `user:tool_result` / `meta:queue-operation` のようなキーごとの実測。
    pub kept: BTreeMap<String, KindStat>,
    pub dropped: BTreeMap<String, KindStat>,
    pub files: usize,
    pub lines: usize,
    /// JSON として読めなかった行（走行中セッションの末尾など）。
    pub unparsable_lines: usize,
    /// `timestamp` を持たない / 解釈できなかったために落とした対話行。
    pub undatable_rows: usize,
    /// 実データに現れた `cwd` の集合（プロジェクトの同一性の確認用）。
    pub cwds: BTreeSet<String>,
}

impl ScanStats {
    fn add(map: &mut BTreeMap<String, KindStat>, key: &str, bytes: usize) {
        let e = map.entry(key.to_string()).or_default();
        e.count += 1;
        e.bytes += bytes;
    }

    pub fn kept_rows(&self) -> usize {
        self.kept.values().map(|s| s.count).sum()
    }

    pub fn kept_bytes(&self) -> usize {
        self.kept.values().map(|s| s.bytes).sum()
    }

    pub fn dropped_bytes(&self) -> usize {
        self.dropped.values().map(|s| s.bytes).sum()
    }
}

/// 1 プロジェクト分の走査結果。
#[derive(Debug, Clone)]
pub struct ProjectScan {
    /// **時刻昇順**に並べた取り込み候補。
    pub rows: Vec<PlannedRow>,
    pub stats: ScanStats,
}

/// `timestamp` を DB の他の行と同じ表記へ正規化する。
///
/// Claude Code は `2026-06-29T03:38:11.126Z`、opencrab は `Utc::now().to_rfc3339()` ＝
/// `2026-06-29T03:38:11.126456+00:00`。**表記を揃えないと文字列比較の順序が壊れる**
/// （`created_at` の比較・`substr` バケットは全て文字列で走る。`'Z'` は数字より大きいので
/// 同じ秒の中で Z 表記が常に後ろへ回る）。
fn normalize_timestamp(raw: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
}

/// `message.content` を「ブロックの配列」に正規化する。
///
/// `user` の素朴な発言は `content` が**文字列**で入る（`[{"type":"text",...}]` ではない）。
/// 配列だけを見ると人間の発言をまるごと落とす。
fn content_blocks(message: &Value) -> Vec<Value> {
    match message.get("content") {
        Some(Value::Array(arr)) => arr.clone(),
        Some(Value::String(s)) => vec![serde_json::json!({"type": "text", "text": s})],
        _ => Vec::new(),
    }
}

fn block_bytes(block: &Value) -> usize {
    serde_json::to_string(block).map(|s| s.len()).unwrap_or(0)
}

/// `promptSource: "system"` の行が task-notification（バックグラウンドタスクの完了通知）か。
///
/// 実データでは system 名義の行は本文が文字列 1 つで、task-notification は必ず
/// `<task-notification>` タグで始まる（他セッションからの連絡は
/// `Another Claude session sent a message:` で始まる）。先頭ブロックの本文の先頭で見分ける。
fn is_task_notification(blocks: &[Value]) -> bool {
    blocks
        .first()
        .and_then(|b| b.get("text").and_then(Value::as_str))
        .map(|t| t.trim_start().starts_with("<task-notification>"))
        .unwrap_or(false)
}

/// 1 行（1 レコード）を見て、取り込む行を組み立てつつ集計する。
///
/// `next_thinking_index` は thinking に振る連番のカーソル（呼び出し側が進める）。
fn plan_record(
    record: &Value,
    raw_len: usize,
    stats: &mut ScanStats,
    out: &mut Vec<PlannedRow>,
    next_thinking_index: &mut usize,
) {
    let rec_type = record.get("type").and_then(Value::as_str).unwrap_or("?");

    // サイドチェーン（サブエージェント）は道具の動きであって本人の体験ではないので、
    // type を問わず行ごと落とす（task-notification と同じ「道具の動きは記憶に入れない」
    // 判断軸）。実データではサブエージェントのログは `<project>/<uuid>/subagents/` 配下の
    // 別ファイルにあり、scan の非再帰 read_dir でも読まれない。が、inline で
    // `isSidechain: true` が top-level のファイルに現れても混ざらないよう、**フラグを
    // 明示的に見て落とす**（偶然の非取り込みに頼らない）。
    if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        ScanStats::add(
            &mut stats.dropped,
            &format!("{rec_type}:sidechain"),
            raw_len,
        );
        return;
    }

    // 非対話の type は行ごと落とす（列挙せず、対話 2 種以外を全部落とす）。
    if rec_type != "assistant" && rec_type != "user" {
        ScanStats::add(&mut stats.dropped, &format!("meta:{rec_type}"), raw_len);
        return;
    }

    if let Some(cwd) = record.get("cwd").and_then(Value::as_str) {
        stats.cwds.insert(cwd.to_string());
    }

    let Some(message) = record.get("message") else {
        ScanStats::add(
            &mut stats.dropped,
            &format!("{rec_type}:no-message"),
            raw_len,
        );
        return;
    };
    let blocks = content_blocks(message);

    // 送信者は行の属性（ブロックごとには変わらない）。
    let speaker = match rec_type {
        "assistant" => Speaker::Agent,
        _ => match record.get("promptSource").and_then(Value::as_str) {
            Some("system") => Speaker::System,
            _ => Speaker::User,
        },
    };

    // 機械が差し込んだ `user` 行（`promptSource: "system"`）のうち、**本文が
    // task-notification の行だけ**取り込まない。task-notification はバックグラウンド
    // タスクの完了通知＝サブタスクの成果物ダンプで、本人の体験ではなく道具が返した
    // 作業結果。#393 で declare_window から外した「整備作業が記憶になる」のと同じ形。
    // 同じ `promptSource: "system"` でも他セッションからの連絡（agent-message 等）は
    // 「他者から連絡を受け判断を迫られた」対人的体験なので取り込む（オーナーの UX 判断
    // 2026-08-07 / #413）。落とした量は集計に残す。
    if speaker == Speaker::System && is_task_notification(&blocks) {
        ScanStats::add(&mut stats.dropped, "user:task-notification", raw_len);
        return;
    }

    let source_uuid = record
        .get("uuid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let session_uuid = record
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let git_branch = record
        .get("gitBranch")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let created_at = record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(normalize_timestamp);

    for (block_index, block) in blocks.iter().enumerate() {
        let block_type = block.get("type").and_then(Value::as_str).unwrap_or("?");
        let key = format!("{rec_type}:{block_type}");
        let bytes = block_bytes(block);

        // 取り込むのは本文 2 種と thinking だけ。それ以外（tool_use / tool_result /
        // image / 将来増えるもの）は全部落とす。
        let text = match (rec_type, block_type) {
            ("assistant", "text") | ("user", "text") => block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            ("assistant", "thinking") => block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            _ => {
                ScanStats::add(&mut stats.dropped, &key, bytes);
                continue;
            }
        };

        // 空本文は行にしない（`memory_sessions.content` は NOT NULL だが、空行を積んでも
        // 材料にならないうえ宣言ランの窓を無駄に食う）。
        if text.trim().is_empty() {
            ScanStats::add(&mut stats.dropped, &format!("{key}:empty"), bytes);
            continue;
        }

        // 時刻が取れない行は取り込まない。id 昇順＝時刻昇順が宣言ランの前提で、
        // 時刻の無い行を混ぜるとその前提が静かに崩れる。
        let Some(created_at) = created_at.clone() else {
            stats.undatable_rows += 1;
            ScanStats::add(&mut stats.dropped, &format!("{key}:no-timestamp"), bytes);
            continue;
        };

        let session_id = format!("cc-{session_uuid}");
        let row = if block_type == "thinking" {
            let tid = format!("{THINKING_ID_PREFIX}{}", *next_thinking_index);
            *next_thinking_index += 1;
            PlannedRow {
                session_id,
                log_type: "inner_voice".to_string(),
                speaker,
                content: thinking_marker(&tid, text),
                created_at,
                source_uuid: source_uuid.clone(),
                block_index,
                git_branch: git_branch.clone(),
                thinking_body: Some(text.to_string()),
                thinking_id: Some(tid),
            }
        } else {
            PlannedRow {
                session_id,
                log_type: "speech".to_string(),
                speaker,
                content: text.to_string(),
                created_at,
                source_uuid: source_uuid.clone(),
                block_index,
                git_branch: git_branch.clone(),
                thinking_body: None,
                thinking_id: None,
            }
        };
        ScanStats::add(&mut stats.kept, &key, bytes);
        out.push(row);
    }
}

/// 生ログに残す thinking の印。**本文は 1 文字も含めない**。
///
/// 含めるのは短 id・文字数・退避先パスだけ。[`crate::tool_result_log`] の案内と同じ
/// 考え方で、冒頭プレビューは付けない（先頭だけ見て judge する誘導になる）。
fn thinking_marker(thinking_id: &str, body: &str) -> String {
    format!(
        "[think:{thinking_id}] assistant thinking, {} chars, full text at `{THINKING_DIR}/{thinking_id}.txt` (relative to your workspace root)",
        body.chars().count()
    )
}

/// 1 つの `.jsonl` を走査して取り込み候補を積む。
fn scan_session_file(
    path: &Path,
    stats: &mut ScanStats,
    out: &mut Vec<PlannedRow>,
    next_thinking_index: &mut usize,
) -> Result<()> {
    use std::io::BufRead;

    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    stats.files += 1;
    for line in reader.lines() {
        // 走行中のセッションは末尾が書きかけになりうる。読めない行で全体を落とさない。
        let Ok(line) = line else {
            stats.unparsable_lines += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        stats.lines += 1;
        match serde_json::from_str::<Value>(&line) {
            Ok(record) => plan_record(&record, line.len(), stats, out, next_thinking_index),
            Err(_) => stats.unparsable_lines += 1,
        }
    }
    Ok(())
}

/// プロジェクトディレクトリ（`~/.claude/projects/<project>/`）配下の `.jsonl` を全部走査する。
///
/// `first_thinking_index` は thinking 連番の開始値（再取り込みで既存の印と衝突させない
/// ために、呼び出し側が既存の最大値 + 1 を渡す）。
///
/// 戻り値の `rows` は**時刻昇順**。宣言ラン（#384）は `agent_id` 単位で id 昇順に窓を
/// 進めるので、**挿入順＝時刻順**でなければ窓が時間軸を行き来する。セッションは並行して
/// 走ることがあるため、ファイル単位ではなくプロジェクト全体で並べ直す。
pub fn scan_project_dir(dir: &Path, first_thinking_index: usize) -> Result<ProjectScan> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    // 走査順を決めておく（thinking の連番が実行ごとに変わらないようにする）。
    files.sort();

    let mut stats = ScanStats::default();
    let mut rows: Vec<PlannedRow> = Vec::new();
    let mut next_thinking_index = first_thinking_index;
    for path in &files {
        scan_session_file(path, &mut stats, &mut rows, &mut next_thinking_index)?;
    }

    // 時刻昇順。同時刻は元の走査順（ファイル名順 → 行順）を保つ安定ソート。
    rows.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(ProjectScan { rows, stats })
}

/// 既に取り込み済みの行を除く（同じファイルを 2 回取り込んでも増えない）。
///
/// 鍵は `(session_id, 元レコード uuid, ブロック位置)`。Claude Code の `.jsonl` は
/// **追記のみ**で書き換わらないので、この 3 つ組は 1 行を恒久的に一意に指す。既存行の
/// `metadata_json` から復元するため、**専用のテーブルもマイグレーションも要らない**。
pub fn filter_already_imported(
    rows: Vec<PlannedRow>,
    imported: &HashSet<(String, String, usize)>,
) -> Vec<PlannedRow> {
    rows.into_iter()
        .filter(|r| !imported.contains(&r.dedup_key()))
        .collect()
}

/// 既存行の `(session_id, metadata_json)` から重複防止の鍵集合を復元する。
///
/// この取り込みが入れた行（`source` が [`SOURCE_TAG`]）だけを拾う。
pub fn imported_keys_from_metadata<'a>(
    rows: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
) -> HashSet<(String, String, usize)> {
    let mut set = HashSet::new();
    for (session_id, metadata) in rows {
        let Some(meta) = metadata.and_then(|m| serde_json::from_str::<Value>(m).ok()) else {
            continue;
        };
        if meta.get("source").and_then(Value::as_str) != Some(SOURCE_TAG) {
            continue;
        }
        let (Some(uuid), Some(block)) = (
            meta.get("uuid").and_then(Value::as_str),
            meta.get("block").and_then(Value::as_u64),
        ) else {
            continue;
        };
        set.insert((session_id.to_string(), uuid.to_string(), block as usize));
    }
    set
}

/// 既存の `think_id` の最大連番 + 1（次に振る番号）。1 件も無ければ 1。
pub fn next_thinking_index<'a>(metadatas: impl IntoIterator<Item = &'a str>) -> usize {
    let mut max = 0usize;
    for meta in metadatas {
        let Ok(v) = serde_json::from_str::<Value>(meta) else {
            continue;
        };
        let Some(tid) = v.get("think_id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(n) = tid
            .strip_prefix(THINKING_ID_PREFIX)
            .and_then(|d| d.parse::<usize>().ok())
        {
            max = max.max(n);
        }
    }
    max + 1
}

#[cfg(test)]
mod tests;
