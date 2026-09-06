use crate::context_budget::{assemble_from_snapshot, TurnGovernor, CONTEXT_BUDGET_EXHAUSTED};
use crate::tokens::estimate_tokens;

/// コンパクション時に最低限保持する最近のログ件数。
/// 旧 `fit_logs_to_budget` 経路（対比テスト用）だけが参照する。
#[allow(dead_code)]
pub(super) const RECENT_MIN_LOGS: usize = 10;
/// コンパクション時に**必ず**保持する直近ユーザー発言の件数（#284）。
///
/// `RECENT_MIN_LOGS` は「直近 N 件のログ」しか保証しない。ツール往復が走ると
/// 直近 10 件が tool_call / tool_result だけで埋まり、ユーザー発言が 1 件も
/// プロンプトに載らないまま応答する（= #284 の事故）。ログ種別に関係なく
/// 「直近のユーザー発言 N 件」を別枠で確保し、予算配分でも最優先で取る。
///
/// 5 件の根拠: 実例では直近 10 件が tool_result 5 + evaluation 2 + 自分の発言 3 で
/// 埋まり、ユーザー発言が 0 件になっていた。ユーザーは指示を短文で連投する
/// （「全員フォローして」「無視？」「つらい」）ため、1〜2 件では直前の言い直しだけを
/// 拾って元の指示を落とす。5 件なら一連の連投をまたいで意図が読める。
pub const RECENT_MIN_USER_SPEECHES: usize = 5;

/// 会話履歴が空のときに `build_conversation_inner` が返すマーカー（#691 の判定にも使う）。
pub const NO_MESSAGES_MARKER: &str = "No messages yet.";

/// 応答直前に会話履歴の末尾へ置く出力指示（#691）。
///
/// opus-5 は 1:1 の長い会話で「次のユーザー発言」を続きとして**捏造**する傾向がある
/// （モデル固有の挙動・オーナー観測）。会話履歴は `[ID] [時刻]:` 形式で 1 行 1 発話に
/// 連結されるため、生成モデルが最も自然な予測として「次の話者行」を書き足してしまう。
/// 生成点に最も近い指示が最も効く（オーナー裁定・実測）ので、履歴の**直後**
/// （＝生成点の直前）にこの 1 行だけを置く。ロール分離・履歴形式の変更・出力の
/// フィルタはしない（オーナー裁定で対策から除外）。履歴が空（`NO_MESSAGES_MARKER`）の
/// ときは真似る対象が無いので付けない。
pub const RESPONSE_ONLY_DIRECTIVE: &str = "ここから先はあなた自身の本文のみを書く。`[ID] [時刻]:` 形式の行（他の話者の発言の再現・引用・続き）を出力してはならない。";

/// セッションログから会話文字列を構築する（トークン予算ベースのコンパクション対応）。
///
/// `context_budget_tokens` はこの会話セクションに使えるトークン予算（`conversation_high`）。
/// Memory Index の注入判定は [`crate::context_budget::apply_line_items`] に一本化する。
/// ここは判定結果（`include_memory_index`）だけを受け取り、部分切り詰めはしない。
pub fn build_conversation_string(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    context_budget_tokens: usize,
) -> Result<String, anyhow::Error> {
    build_conversation_string_with_memory_index(
        conn,
        session_id,
        agent_id,
        context_budget_tokens,
        true,
    )
}

/// [`build_conversation_string`] と同じだが、Memory Index を載せるかを呼び出し側が決める。
pub fn build_conversation_string_with_memory_index(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    context_budget_tokens: usize,
    include_memory_index: bool,
) -> Result<String, anyhow::Error> {
    build_conversation_string_with_waters(
        conn,
        session_id,
        agent_id,
        context_budget_tokens,
        context_budget_tokens / 2,
        include_memory_index,
    )
}

/// 二水位を明示して会話を組む（#826-B）。開始は組立と検査。高水位超過のときだけ刈る。
pub fn build_conversation_string_with_waters(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    conversation_high: usize,
    conversation_low: usize,
    include_memory_index: bool,
) -> Result<String, anyhow::Error> {
    let prefix = build_context_prefix_sections(conn, session_id, agent_id, include_memory_index);

    // conversation_high から引くのは会話車線（台帳・人物像）。MI は fixed 済み。
    let mut inner_budget = conversation_high;
    for section in prefix.billed() {
        inner_budget = inner_budget.saturating_sub(estimate_tokens(section));
    }
    // #536: 最後の `parts.join("\n\n")` の区切りも出力へ含まれるので計上する。
    // 会話車線の区切りだけを conversation_high から引く（MI は fixed 済み）。
    inner_budget = inner_budget.saturating_sub(prefix.billed().count() * estimate_tokens("\n\n"));
    // #691: 履歴の直後に置く出力指示のぶんを会話予算から先に引く。prepend 前の返り値が
    // `context_budget_tokens` を超えないという契約（下の budget テスト群）を保つため、
    // #536 の区切り計上と同じ流儀で組み込み前に確保する。履歴が空で指示を付けない場合は
    // 数十トークン過剰に確保するだけで実害はない（空履歴は "No messages yet." のみ）。
    let directive_cost = estimate_tokens(RESPONSE_ONLY_DIRECTIVE) + estimate_tokens("\n\n");
    inner_budget = inner_budget.saturating_sub(directive_cost);
    let prefix_cost = conversation_high.saturating_sub(inner_budget);
    let inner_low = conversation_low.saturating_sub(prefix_cost);
    let inner = build_conversation_inner(conn, session_id, agent_id, inner_budget, inner_low)?;

    // #691: 履歴が空（真似る対象が無い）ときは出力指示を付けない。
    let history_is_empty = inner == NO_MESSAGES_MARKER;

    let mut parts = prefix.ordered();
    parts.push(inner);
    let mut out = parts.join("\n\n");
    if !history_is_empty {
        // 応答直前の出力指示を履歴の**直後**（＝生成点の直前）へ 1 行だけ置く（#691）。
        out.push_str("\n\n");
        out.push_str(RESPONSE_ONLY_DIRECTIVE);
    }
    Ok(out)
}

/// 会話本文の前に置く固定セクション（台帳 / [Memory Index] / [Impressions]）。
struct ContextPrefixSections {
    ledger: Option<String>,
    memory_index: Option<String>,
    impressions: Option<String>,
}

impl ContextPrefixSections {
    fn billed(&self) -> impl Iterator<Item = &String> {
        self.ledger.iter().chain(self.impressions.iter())
    }

    fn ordered(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if let Some(s) = &self.ledger {
            parts.push(s.clone());
        }
        if let Some(s) = &self.memory_index {
            parts.push(s.clone());
        }
        if let Some(s) = &self.impressions {
            parts.push(s.clone());
        }
        parts
    }
}

/// すべて `session_id` を「いま走っているセッション」として解決する。best-effort で、
/// どれが欠けても会話構築は続行する。
///
/// Memory Index の注入判定は呼び出し側（`apply_line_items`）が行う。
fn build_context_prefix_sections(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    include_memory_index: bool,
) -> ContextPrefixSections {
    // タスク台帳（前向きワーキング状態）を会話の先頭に前置する。
    // system prompt 側は 1h キャッシュされるため、毎ターン変わる台帳状態はここに置く。
    // 台帳の読み出し失敗で返信自体を殺さない（warn して台帳なしで続行）。
    let ledger = match crate::task_ledger::build_ledger_section(conn, agent_id, session_id) {
        Ok(section) => section,
        Err(e) => {
            tracing::warn!("failed to build task ledger section for session {session_id}: {e}");
            None
        }
    };

    // [Memory Index]: 長期記憶のコンパクトな目次を常時前置する（月次要約 + 本人が
    // 宣言した記憶の単位 + 未宣言の現在月 topic、short_id 付き）。台帳と同じく
    // 「動的状態は会話側」（system は 1h キャッシュ）。best-effort — 失敗しても
    // 返信は殺さない。
    // コンパクション時の [Past context summary]（build_conversation_inner 内、
    // 現セッションの topic のみ）とは役割が異なり、こちらは現セッション由来の
    // topic を除外するため short_id が両方に出ることはない（invariant）。
    // 宣言ユニットはエージェント単位（生涯スコープ）でこちらにだけ出る
    // （get_topic_nodes_for_session は node_type='topic' しか拾わない / #403）。
    // 入れる/入れないは `apply_line_items` の判定をそのまま使う。部分切り詰めはしない。
    let memory_index = if include_memory_index {
        match crate::memory_index::build_memory_index_section(conn, agent_id, session_id) {
            Ok(section) => section,
            Err(e) => {
                tracing::warn!(
                    "failed to build memory index section for session {session_id}: {e}"
                );
                None
            }
        }
    } else {
        None
    };

    // [Impressions]: いま話している相手の人物像（#314）。人物像は agent スコープ
    // （経路をまたいで同じ相手なら同じ 1 行）だが、**載せるのは直近の発話者の分だけ**で、
    // 人数もフィールド長もビルダ側で上限が掛かっている。台帳・memory index と同じく
    // best-effort — 読み出しに失敗しても返信は殺さない。
    let impressions =
        match crate::impression_section::build_impression_section(conn, agent_id, session_id) {
            Ok(section) => section,
            Err(e) => {
                tracing::warn!("failed to build impression section for session {session_id}: {e}");
                None
            }
        };

    ContextPrefixSections {
        ledger,
        memory_index,
        impressions,
    }
}

/// 会話文字列本体の構築（タスク台帳の前置は `build_conversation_string` 側で行う）。
///
/// 開始時はスナップショット＋差分の組立と検査だけ。高水位超過のときだけ低水位まで刈る。
/// 現行の開始時 `fit_logs_to_budget` は走らせない。
fn build_conversation_inner(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    conversation_high: usize,
    conversation_low: usize,
) -> Result<String, anyhow::Error> {
    let assembled = assemble_from_snapshot(conn, session_id, agent_id)?;
    if assembled.text == NO_MESSAGES_MARKER {
        return Ok(assembled.text);
    }
    let mut gov = TurnGovernor::new(conversation_high, conversation_low);
    let Some(outcome) = gov.compact_start_if_over(assembled.tokens, &assembled.items) else {
        return Ok(assembled.text);
    };
    if outcome.exhausted {
        return Err(anyhow::anyhow!(
            "{CONTEXT_BUDGET_EXHAUSTED}: conversation tokens after inviolable lanes exceed input_high"
        ));
    }
    Ok(outcome.text)
}
