/// heartbeat セッションの speech 行が「エージェント自身の idle（静観）応答」かどうかを判定する。
///
/// 判定は**過去データ**（旧 `SPEAK:` / `LEARN` / `IDLE` の語彙で残った heartbeat speech 行）を
/// メモリ索引から除外するためのもの: 応答本文に非空の `SPEAK:` があれば発話、`LEARN` を含めば
/// 学習として残す。**#588 Stage 3 でハートビートの専用語彙（旧 `HeartbeatDecision`）は撤去され、
/// 新しい行はこの形式で書かれない**（通常の配送記録になる）ので、この判定は既存の履歴のための
/// もの。マーカー集合を列挙せず「全大文字トークン + 中身の有無」で見るため、語彙撤去後の新しい
/// 記録に対しても安全側（＝実ありとして残す）に働く。
///
/// #517: 残り（SPEAK/LEARN でない）は従来「一律 idle」としていたが、#515 で当時の HB は IDLE の
/// 記録が「`IDLE: <なぜ見送ったか>`」という**本人の言葉の理由**を持つようになっていた（過去データ）。
/// 理由つきの記録は材料として意味があるので落とさない。**中身があるか**で判定する:
/// 先頭の全大文字マーカー（`IDLE` / `NO_REPLY` 等 = `[A-Z_]+` と続く任意の `:`）を剥いで
/// なお非空の本文が残れば「実あり」として残し、剥いだ後が空（無内容の `IDLE` 等・
/// pre-#515 の 2,991 件）だけを idle ノイズとして除外する。
///
/// この変更は**除外を狭める方向のみ**（新 idle 集合 ⊆ 旧 idle 集合）。SPEAK/LEARN の
/// 扱いは不変。マーカー集合を列挙せず「全大文字トークン + 中身の有無」で見るので、
/// 生成タイトルの揺れや将来のマーカー追加に強い。
///
/// **話者ガード**: idle として捨ててよいのは `speaker_id == agent_id`、つまり
/// エージェント自身の応答行に限る。本番のハートビートセッションは実測で
/// 単一話者（全 speech が自分）だが、万一「他者の発言」が混ざっても、それは
/// 相手の言葉という実質なので idle 扱いにせず材料に残す（本文が SPEAK:/LEARN を
/// 含まなくても落とさない）。
///
/// 対象は `speech` 行のみ。それ以外の log_type は常に「実あり」として残す
/// （呼び出し側で heartbeat セッションに限定する）。
pub(super) fn is_idle_heartbeat_speech(
    log: &opencrab_db::queries::SessionLogRow,
    agent_id: &str,
) -> bool {
    if log.log_type != "speech" {
        return false;
    }
    // 自分以外の発話（他者の言葉）は idle 扱いにしない = 材料に残す。
    if log.speaker_id.as_deref() != Some(agent_id) {
        return false;
    }
    let text = log.content.trim();
    // SPEAK: の後に非空の内容があれば発話 = 実あり。
    if let Some(rest) = text
        .lines()
        .find(|l| l.contains("SPEAK:"))
        .and_then(|l| l.split_once("SPEAK:").map(|x| x.1))
    {
        if !rest.trim().is_empty() {
            return false;
        }
    }
    // LEARN を含めば学習 = 実あり。
    if text.to_uppercase().contains("LEARN") {
        return false;
    }
    // #517: 先頭の全大文字マーカーを剥いで中身が残れば実あり（`IDLE: <理由>` を残す）。
    // 無内容の裸マーカー（`IDLE` 等）だけが idle ノイズ。
    idle_decision_has_no_reason(text)
}

/// 先頭の全大文字決定マーカー（`IDLE` / `NO_REPLY` 等 = `[A-Z_]+`）と続く任意の `:` を
/// 剥いで、残りが空白のみか（＝理由本文が無い裸マーカー）を返す（#517）。
///
/// マーカーが無い本文（CJK 等で始まる散文）は剥がすものが無く、非空なら `false`。
/// これにより「中身があるか」で idle を判定し、`IDLE: <理由>`・改行後に本文が続く
/// `IDLE\n\n…`・マーカー無しの散文はすべて残し、無内容の `IDLE` / `NO_REPLY` /
/// 空文字だけを true（＝ idle ノイズ）とする。
pub(super) fn idle_decision_has_no_reason(text: &str) -> bool {
    let t = text.trim();
    // 先頭の連続する [A-Z_] をマーカーとして数える（ASCII 大文字とアンダースコアのみ）。
    let marker_len = t
        .bytes()
        .take_while(|b| b.is_ascii_uppercase() || *b == b'_')
        .count();
    // マーカーが実在するときだけ剥ぐ（無ければ本文全体をそのまま見る）。
    let rest = &t[marker_len..];
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    rest.trim().is_empty()
}

/// 「何もしていない tick」を構成するノイズ行かどうか（HB 由来ノイズの判定）。
///
/// #573 Stage A で呼び出し側の `heartbeat-` 接頭辞ゲートを外し、全セッションへ無条件適用
/// するようになった。述語は元から接頭辞非依存で、下記 2 種は HB 経路しか生まない目印
/// （`speaker_id='heartbeat'`）と中身の有無で判定するため、実会話セッションの中身のある
/// 行を落とすことはない（[`is_idle_heartbeat_speech`] 参照）。
///
/// 除外対象は 2 種類:
/// 1. 毎 tick 注入されるハートビートのプロンプト scaffolding
///    （`log_type='system'` かつ `speaker_id='heartbeat'`, `main.rs` の 357-388 行）。
///    これは記憶ではなく毎回同一の指示文で、除かないと全 heartbeat バッチに必ず
///    残ってしまい「実質ログが残らない」バッチが存在しなくなる。
/// 2. idle（静観）の speech 行（[`is_idle_heartbeat_speech`]）。
///
/// 逆に `tool_call` / `tool_result` / `inner_voice` / 実のある speech（SPEAK/LEARN）/
/// 他者の発言は実際の活動・実質なので材料に残す。これらが 1 件も残らないバッチだけを
/// 「純idle」として topic 化しない（#374）。
pub(super) fn is_heartbeat_noise(
    log: &opencrab_db::queries::SessionLogRow,
    agent_id: &str,
) -> bool {
    if log.log_type == "system"
        && log.speaker_id.as_deref() == Some(opencrab_db::queries::HEARTBEAT_SPEAKER_ID)
    {
        return true;
    }
    is_idle_heartbeat_speech(log, agent_id)
}
