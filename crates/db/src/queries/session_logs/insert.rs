// ============================================
// MEMORY: Sessions
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLogRow {
    pub id: Option<i64>,
    pub agent_id: String,
    pub session_id: String,
    pub log_type: String,
    pub content: String,
    pub speaker_id: Option<String>,
    pub turn_number: Option<i32>,
    pub metadata_json: Option<String>,
    pub created_at: Option<String>,
}

/// 毎 tick 注入されるハートビートのプロンプト scaffolding 行が `speaker_id` に持つ目印（#518）。
///
/// `log_type='system'` かつ `speaker_id` がこの値の行は「何もしていない tick の指示文」で、
/// 記憶の材料（topic 要約）や会話再構成からは除外される。読み手（`index_builder` /
/// `process` の `is_heartbeat_noise` 系述語）はこの定数で判定すること。
///
/// **`RunRequest.gateway` の "heartbeat"（ハートビート発火の gateway ラベル・`scheduler.rs` の
/// `run_one_heartbeat` が渡す）とは別概念**。値がたまたま同じでも用途が違うので混同しないこと。
pub const HEARTBEAT_SPEAKER_ID: &str = "heartbeat";

/// ハートビート発話を本人の実会話（`discord-…`）セッションへ二重記録した行の
/// `metadata_json` に入る `source` 値（#425）。
pub const HEARTBEAT_CHANNEL_ECHO_SOURCE: &str = "heartbeat_channel_echo";

/// 上記エコー行の `metadata_json` そのもの。読み手は [`is_heartbeat_channel_echo`] で判定
/// すること（キー順・空白の違いに依存しないため）。
///
/// **#573 Stage B 以降、この印を新規に付ける書き手はいない**（HB 発話の記録を実会話
/// セッションへ一元化し、#425 エコーを撤去した）。定数と読み手フィルタ（[`is_heartbeat_channel_echo`]
/// / [`EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL`]）を残しているのは、**Stage B 以前に書かれた既存の
/// エコー行のためだけ**。撤去できないのは今この時点でのデータ事情によるもので、恒久的な仕様
/// ではない（撤去条件と追跡は #582）。
///
/// - Stage C 着手時点で、印つきの過去行は **19 行**・**単一の discord セッション**・
///   **2026-08-07〜2026-08-11** に存在する。
/// - これらのエコー本文（マーカーを剥いだクリーン本文）と**同じ発話**が、HB 専用セッション側
///   にも第一級 speech（決定マーカー前置の生応答）として残って索引され続ける。両者は
///   **包含関係で一致**する（byte 単位の完全一致ではない: 19/19 が包含一致・完全一致は 0）。
/// - したがってフィルタを外すと、この過去行が FTS・記憶索引へ入り、HB 専用セッション側の
///   同じ発話と**二重計上**される。
/// - **撤去の条件と追跡は #582**（材料として不要になった / 過去行を削除した / 二重計上が
///   実害でないと確認できた、のいずれか）。
pub const HEARTBEAT_CHANNEL_ECHO_METADATA: &str = r#"{"source":"heartbeat_channel_echo"}"#;

/// 行が HB 発話のエコー（**表示専用の二重記録**）かどうか（#425）。
///
/// エコー行は「生きた会話文脈を本人に見せる」ためだけの二重記録で、**記憶系
/// （FTS 検索・記憶索引・宣言材料）には一切載せない**。#425 当時、記憶材料としての HB 発話は
/// heartbeat 専用セッション側が担っており、症状修正のついでに記憶の挙動を変えないための
/// フィルタだった。**#573 Stage B 以降、新規の HB 発話は実会話セッションへ第一級で記録され、
/// エコーは書かれない**（[`HEARTBEAT_CHANNEL_ECHO_METADATA`] 参照）。よって現在この判定が効くのは
/// **Stage B 以前に書かれた既存のエコー行だけ**で、それらの本文と同じ発話は HB 専用セッション側
/// にも残って索引されるため、ここで落として二重計上を防ぐ。判定は次の 2 箇所で使う:
/// - [`insert_session_log_at`]: FTS 影テーブルへの投入をスキップ（検索に出さない）。
/// - `index_builder`: topic 要約の材料から除外（索引・宣言材料に入れない）。
///
/// （#588: HB 文脈の専用会話組み立て `build_channel_conversation_section` は撤去したので、そこでの
/// 使用は無くなった。この印は #425 のエコー行を索引・検索から落とすためだけに残る。）
///
/// `source` フィールドの**値**で判定するのでキー順・空白の違いに強い。値が現れない
/// 大きな `metadata_json`（`tool_call` 等）は substring で早期に弾き、無駄な JSON parse を
/// 避ける（`insert_session_log_at` は全 insert のホットパス）。
pub fn is_heartbeat_channel_echo(metadata_json: Option<&str>) -> bool {
    let Some(m) = metadata_json else {
        return false;
    };
    if !m.contains(HEARTBEAT_CHANNEL_ECHO_SOURCE) {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(m)
        .ok()
        .as_ref()
        .and_then(|v| v.get("source"))
        .and_then(|s| s.as_str())
        == Some(HEARTBEAT_CHANNEL_ECHO_SOURCE)
}

/// エコー行（表示専用）を記憶系クエリから除外する SQL 述語（#425）。**評価が真の行を保持**
/// する（＝ WHERE へ `AND {…}` の形でそのまま連結する）。
///
/// `metadata_json` の `source` フィールドで判定し、Rust の [`is_heartbeat_channel_echo`] と
/// **同じ行**を除外する（規則「エコーは生きた会話表示にだけ見え、記憶系のあらゆる経路から
/// 不可視」を SQL 経路でも一貫させる）。
/// - `metadata_json IS NULL`（大多数の行）は保持。
/// - malformed JSON は `NOT json_valid(...)` で保持する（`json_extract` がエラーにならない）。
/// - `source` を持たない/別値の JSON は null 安全 `IS NOT` で保持。
/// - source が [`HEARTBEAT_CHANNEL_ECHO_SOURCE`] の行だけを落とす。
///
/// バインドパラメータを持たず source リテラルは固定（ユーザ入力を含まない）なので、
/// 各クエリの WHERE 文字列へそのまま連結してよい。リテラルが上記 source 定数と一致することは
/// 単体テストで固定する（drift ガード）。
pub const EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL: &str = "(metadata_json IS NULL OR NOT json_valid(metadata_json) OR json_extract(metadata_json, '$.source') IS NOT 'heartbeat_channel_echo')";

/// `insert_session_log` の best-effort 版: 失敗を握り潰さず warn を残す（#47）。
///
/// 会話履歴のクリティカル経路では挿入失敗が「無言の履歴欠落」になる。伝播すると
/// 応答フロー自体を止めてしまう場所（ログは副作用）で使う想定なので、エラーは
/// 返さずログのみ。戻り値が要る/失敗を伝播すべき場所では `insert_session_log` を使うこと。
pub fn insert_session_log_best_effort(conn: &Connection, log: &SessionLogRow) {
    if let Err(e) = insert_session_log(conn, log) {
        tracing::warn!(
            session_id = %log.session_id,
            log_type = %log.log_type,
            "session log insert failed (best-effort path): {e}"
        );
    }
}

pub fn insert_session_log(conn: &Connection, log: &SessionLogRow) -> Result<i64> {
    insert_session_log_at(conn, log, &Utc::now().to_rfc3339())
}

/// `created_at` を**呼び出し側が決める** [`insert_session_log`]（#413）。
///
/// 通常の記録経路は「いま起きたこと」を書くので `Utc::now()` で正しいが、過去ログの
/// **取り込み**では元の発生時刻でなければ意味が無い（宣言ランの窓も記憶索引の期間も
/// `created_at` で切る）。`SessionLogRow::created_at` を黙って使う形にしなかったのは、
/// 既存の全呼び出しが `None` を渡しており、意味を後付けで変えると「渡し忘れたら現在時刻」
/// という静かな分岐が生まれるため。時刻を持ち込む経路だけがこちらを名指しで呼ぶ。
///
/// `created_at` は**他の行と同じ表記**（`DateTime::to_rfc3339()`）で渡すこと。比較も
/// バケットも文字列で走るので、表記が混ざると順序が壊れる。
pub fn insert_session_log_at(
    conn: &Connection,
    log: &SessionLogRow,
    created_at: &str,
) -> Result<i64> {
    // 本体テーブルとFTS影テーブルへの2書き込みをトランザクションで原子化する。
    // 途中失敗で FTS と memory_sessions が恒久的に不整合になるのを防ぐ。
    //
    // **既に外側のトランザクション中なら、そちらの原子性に乗る**（#413）。SQLite は
    // `BEGIN` の入れ子を許さないので、まとめて入れたい呼び出し側（取り込みは全行を
    // 1 トランザクションにする — 途中で落ちた半端な範囲を残すと宣言ランのカーソルが
    // その途中を跨ぐ）から呼ぶと、ここで無条件に `BEGIN` すると失敗する。
    let tx = if conn.is_autocommit() {
        Some(conn.unchecked_transaction()?)
    } else {
        None
    };

    conn.execute(
        "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            log.agent_id,
            log.session_id,
            log.log_type,
            log.content,
            log.speaker_id,
            log.turn_number,
            log.metadata_json,
            created_at,
        ],
    )?;

    let row_id = conn.last_insert_rowid();

    // FTSにも追加。ただし #425 のエコー行（表示専用の二重記録）は FTS に載せない
    // （記憶検索に二重ヒット・過大計上を出さない。記憶材料は heartbeat セッション側が担う）。
    // 本体テーブルには入れる（会話文脈の表示に使う）ので、ここで memory_sessions と
    // memory_sessions_fts が意図的に 1 行ずれる。手動同期の fts5 なので他経路が勝手に
    // 埋め戻すことはない（唯一の投入口はこの関数）。
    //
    // ⚠️ 将来 `memory_sessions_fts` を本体から全行再構築するマイグレーションを書く場合も、
    // ここと同じフィルタ（`is_heartbeat_channel_echo` / `EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL`）を
    // 必ず掛けること。掛け忘れるとエコー行が FTS に埋め戻り、記憶検索の二重ヒットが復活する。
    if !is_heartbeat_channel_echo(log.metadata_json.as_deref()) {
        conn.execute(
            "INSERT INTO memory_sessions_fts (rowid, content, agent_id, session_id, log_type)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                row_id,
                log.content,
                log.agent_id,
                log.session_id,
                log.log_type
            ],
        )?;
    }

    if let Some(tx) = tx {
        tx.commit()?;
    }

    Ok(row_id)
}

