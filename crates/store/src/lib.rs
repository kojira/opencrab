//! store — 永続化。SQLite ひとつ（詳細§01・§03）。実装は 1 つだけで、テストも同じ実装
//! （`:memory:`）を使う。時刻は core が nanos で渡す（store は時計を持たない）。
//!
//! 権威は全部ここにある（詳細§03）。プロセス内の状態は「実行中の future への取っ手」だけ。

use opencrab_port::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::{Arc, Mutex};

pub type Result<T> = std::result::Result<T, rusqlite::Error>;

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Clone, Debug)]
pub struct PlaceRow {
    pub id: PlaceId,
    pub address: Option<String>,
    pub parent_id: Option<PlaceId>,
    pub policy_json: String,
    pub inherit_from_place: Option<PlaceId>,
    pub inherit_up_to_seq: Option<Seq>,
    pub closed_at: Option<i64>,
    pub close_reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct NewEvent {
    pub kind: EventKind,
    pub author_subject: Option<SubjectId>,
    pub author_external: Option<String>,
    pub content: Content,
    pub mentions: Vec<SubjectId>,
    pub reply_to: Option<Seq>,
    pub target: Option<Seq>,
    pub for_subject: Option<SubjectId>,
    /// この出来事に付いた添付（DESIGN-images §1）。記録するのは URL（参照）だけ——中身は保存しない。
    /// 既定 `[]`（添付なしは従来どおり・後方互換）。
    pub attachments: Vec<Attachment>,
}

/// `append_incoming` の結果（詳細§04）。畳んだか、新しく積んだか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ingest {
    Appended(Seq),
    Duplicate(Seq),
}

/// 再起動時の activity 回収結果。`Appended` のときだけ新しい event が生まれ、呼び手は
/// 発火判定を行う。`AlreadyRecorded` / `AlreadyEnded` は再実行時の no-op。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityInterruption {
    Appended { place: PlaceId, seq: Seq },
    AlreadyRecorded { place: PlaceId, seq: Seq },
    AlreadyEnded,
}

/// 通常の Background 決着結果。activity 終端・結果 event・provenance（必要なら offload）を
/// 1 transaction で確定し、再実行は保存済みの同じ結果へ収束する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundSettlement {
    Appended { place: PlaceId, seq: Seq },
    AlreadyRecorded { place: PlaceId, seq: Seq },
}

/// Background の大きな結果を、決着と同じ transaction で保存するための入力。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewBackgroundOffload {
    pub body: String,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct EventRow {
    pub place: PlaceId,
    pub seq: Seq,
    pub kind: EventKind,
    pub author_subject: Option<SubjectId>,
    pub author_external: Option<String>,
    pub content: Content,
    pub mentions: Vec<SubjectId>,
    pub reply_to: Option<Seq>,
    pub target: Option<Seq>,
    pub for_subject: Option<SubjectId>,
    pub created_at: i64,
    /// この出来事に付いた添付（DESIGN-images §1）。描画で「存在と番地」を出し、core-look が番地から引く。
    pub attachments: Vec<Attachment>,
}

#[derive(Clone, Debug)]
pub struct SubjectRow {
    pub id: SubjectId,
    pub kind: SubjectKind,
    /// 主体の同一性データ（表示名）。ログの著者表示・read の author_display はここを読む。
    /// 人格本文（`persona`）とは別列——名前は人格の構造ではなく主体の同一性（統括裁定）。
    pub name: String,
    /// 人格本文（逐語）。Agent のターンで system の先頭に逐語で載る（core は枠を被せない）。
    pub persona: String,
    pub turn_runner: String,
    pub standing: Standing,
}

#[derive(Clone, Debug)]
pub struct MembershipRow {
    pub place: PlaceId,
    pub subject: SubjectId,
    pub role: Role,
    pub read_seq: Seq,
}

#[derive(Clone, Debug)]
pub struct ActivityRow {
    pub id: ActivityId,
    pub place: PlaceId,
    pub subject: SubjectId,
    pub kind: ActivityKindTag,
    pub label: Option<String>,
    pub deadline_at: i64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub end_reason: Option<String>,
    pub detached_from: Option<ActivityId>,
    /// background activity が生まれた turn の入力範囲・権限・受理 tool call。
    /// turn activity と provenance 導入前の既存行では None。
    pub provenance: Option<BackgroundProvenance>,
}

/// background activity の発端と、core が権限確認後に受理した tool call。
/// range は store の通常の読み方と同じ `(from_exclusive, to_inclusive]`。
#[derive(Clone, Debug, PartialEq)]
pub struct BackgroundProvenance {
    pub origin_from_exclusive: Seq,
    pub origin_to_inclusive: Seq,
    pub origin_standing: Standing,
    pub tool_name: String,
    pub tool_args: serde_json::Value,
}

/// Background の結果 event（Settled / Interrupted）と、それを生んだ activity / 発端 / tool call の対応。
#[derive(Clone, Debug, PartialEq)]
pub struct SettledProvenance {
    pub activity: ActivityId,
    pub origin_from_exclusive: Seq,
    pub origin_to_inclusive: Seq,
    pub origin_standing: Standing,
    pub tool_name: String,
    pub tool_args: serde_json::Value,
}

/// ターンの事実だけを持つ（詳細§05）。文脈の事実（範囲・切り詰め・トークン数）は
/// 反復ごとに変わるので `context_records` にだけ置く。ここには重複させない（§10）。
/// 引き継ぎはターンの間ずっと一定なのでターンの事実として持つ。
#[derive(Clone, Debug)]
pub struct NewTurnRecord {
    pub place: PlaceId,
    pub subject: SubjectId,
    pub activity: ActivityId,
    pub inherit_from_seq: Option<Seq>,
    pub inherit_to_seq: Option<Seq>,
    pub iterations: i64,
    pub started_at: i64,
    pub ended_at: i64,
    pub end_reason: String,
    /// terminal な Engine failure の本文。Engine 起因でない結末と成功では None。
    pub failure_detail: Option<String>,
    /// NO_REPLY で配送を保留した地の文（平文アクション文法）。保留が無ければ None。
    pub withheld_text: Option<String>,
    /// このターンで受理した平文ツール行を逐語で残したもの（平文ツール行の設計）。ツール行は say として
    /// 配送されず（決着イベントが結果を運ぶ）、受理イベントも積まない——それでも「本文に書かれたが
    /// 配送しなかったもの」を黙って消さないための記録（withheld_text と同じ発想）。無ければ None。
    pub tool_lines: Option<String>,
    /// 返答の絞りの会計キー（DESIGN-attention §2）。このターンを起こした着火作者の正規化キー。
    /// オーナー・系・着火作者の無い発火では None（会計に載せない）。
    pub fired_by: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TurnRecordRow {
    pub id: i64,
    pub place: PlaceId,
    pub subject: SubjectId,
    pub activity: ActivityId,
    pub inherit_from_seq: Option<Seq>,
    pub inherit_to_seq: Option<Seq>,
    pub iterations: i64,
    pub end_reason: String,
    /// terminal な Engine failure の本文。Engine 起因でない結末と成功では None。
    pub failure_detail: Option<String>,
    /// NO_REPLY で配送を保留した地の文（平文アクション文法）。保留が無ければ None。
    pub withheld_text: Option<String>,
    /// 受理した平文ツール行を逐語で残したもの（平文ツール行の設計）。無ければ None。
    pub tool_lines: Option<String>,
    /// 返答の絞りの会計キー（DESIGN-attention §2）。オーナー・系・着火作者の無い発火では None。
    pub fired_by: Option<String>,
}

/// 反復ごとの文脈の観測（詳細§10）。多く回るターンほど後の反復で切り詰まるので、
/// トークン数・切り詰めの有無と範囲を反復ごとに残す（ターン記録の 1 行では足りない）。
#[derive(Clone, Debug)]
pub struct NewContextRecord {
    pub turn_record_id: i64,
    pub place: PlaceId,
    pub iteration: i64,
    pub ctx_from_seq: Option<Seq>,
    pub ctx_to_seq: Option<Seq>,
    pub skipped_from_seq: Option<Seq>,
    pub skipped_to_seq: Option<Seq>,
    pub prompt_tokens: i64,
}

#[derive(Clone, Debug)]
pub struct ContextRecordRow {
    pub id: i64,
    pub turn_record_id: i64,
    pub place: PlaceId,
    pub iteration: i64,
    pub ctx_from_seq: Option<Seq>,
    pub ctx_to_seq: Option<Seq>,
    pub skipped_from_seq: Option<Seq>,
    pub skipped_to_seq: Option<Seq>,
    pub prompt_tokens: i64,
}

/// 記憶の 1 件（記憶とワーカー §01）。主体・本文・由来（場＋連番範囲）・書かれた時刻・
/// 最後に読まれた時刻。まだ読まれていなければ `last_read_at` は None。
#[derive(Clone, Debug)]
pub struct MemoryRow {
    pub id: i64,
    pub subject: SubjectId,
    pub body: String,
    pub origin_place: PlaceId,
    pub origin_from_seq: Seq,
    pub origin_to_seq: Seq,
    pub written_at: i64,
    pub last_read_at: Option<i64>,
}

/// 退避の 1 件（常時切り離し・案 A）。所有主体・由来の場・退避本文（生テキスト・行指向）・
/// 退避先の上限超過で先頭だけ保存したか・作られた時刻。行数／バイト数は本文から数える（重複して
/// 持たない）——読みの案内（core-bg-read）も決着の案内も、この本文をそのまま数える。
#[derive(Clone, Debug)]
pub struct OffloadRow {
    pub activity_id: ActivityId,
    pub subject: SubjectId,
    pub place: PlaceId,
    pub body: String,
    pub truncated: bool,
    pub created_at: i64,
}

fn standing_str(s: Standing) -> &'static str {
    match s {
        Standing::Owner => "owner",
        Standing::Trusted => "trusted",
        Standing::Unknown => "unknown",
    }
}
/// 自分が書いた値の破損は「変換の失敗」で返す（倒す向きの安全とは無関係・詳細§15）。
fn decode_err(what: &str, got: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("unknown {what}: {got}").into(),
    )
}
fn standing_from(s: &str) -> rusqlite::Result<Standing> {
    Ok(match s {
        "owner" => Standing::Owner,
        "trusted" => Standing::Trusted,
        "unknown" => Standing::Unknown,
        other => return Err(decode_err("standing", other)),
    })
}
fn kind_str(k: SubjectKind) -> &'static str {
    match k {
        SubjectKind::Human => "human",
        SubjectKind::Agent => "agent",
    }
}
/// 未知値を Human（＝非エージェント）へ倒さない。倒れると、そのエージェントが二度と発火しない（§15）。
fn kind_from(s: &str) -> rusqlite::Result<SubjectKind> {
    Ok(match s {
        "human" => SubjectKind::Human,
        "agent" => SubjectKind::Agent,
        other => return Err(decode_err("subject kind", other)),
    })
}
fn role_str(r: Role) -> &'static str {
    match r {
        Role::Participant => "participant",
        Role::Observer => "observer",
    }
}
/// 未知値を、より強い Participant へ倒さない。ただし出来事の種別（`from_wire`）と落とし方を揃え、
/// inline panic ではなく「変換の失敗」で返す（自分が書いた値の破損 → 呼び手の `?`/`unwrap` で表に出る・詳細§15）。
fn role_from(s: &str) -> rusqlite::Result<Role> {
    Ok(match s {
        "participant" => Role::Participant,
        "observer" => Role::Observer,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown role: {other}").into(),
            ))
        }
    })
}
fn atag_str(k: ActivityKindTag) -> &'static str {
    match k {
        ActivityKindTag::Turn => "turn",
        ActivityKindTag::Background => "background",
    }
}
/// 未知値を Turn へ倒さない。倒れると背景の本数の数え上げから外れ、上限を静かにすり抜ける（§15）。
fn atag_from(s: &str) -> rusqlite::Result<ActivityKindTag> {
    Ok(match s {
        "turn" => ActivityKindTag::Turn,
        "background" => ActivityKindTag::Background,
        other => return Err(decode_err("activity kind", other)),
    })
}

fn content_to_json(c: &Content) -> String {
    serde_json::json!({ "text": c.text, "symbol": c.symbol }).to_string()
}
/// 壊れた JSON を空の Content へ倒さない。読めなければ変換の失敗（§15）。
fn content_from_json(s: &str) -> rusqlite::Result<Content> {
    let v: serde_json::Value =
        serde_json::from_str(s).map_err(|_| decode_err("content json", s))?;
    Ok(Content {
        text: v
            .get("text")
            .and_then(|x| x.as_str())
            .map(|x| x.to_string()),
        symbol: v
            .get("symbol")
            .and_then(|x| x.as_str())
            .map(|x| x.to_string()),
    })
}
/// 添付を JSON 配列へ（DESIGN-images §1）。`kind` は wire 名、`origin_author` は由来作者（§5・任意）。
fn attachments_to_json(a: &[Attachment]) -> String {
    let arr: Vec<serde_json::Value> = a
        .iter()
        .map(|att| {
            serde_json::json!({
                "kind": att.kind.as_wire(),
                "url": att.url,
                "origin_author": att.origin_author,
            })
        })
        .collect();
    serde_json::Value::Array(arr).to_string()
}
/// 壊れた JSON・未知の kind を空へ倒さない。読めなければ変換の失敗（§15）——近いものへ寄せない。
fn attachments_from_json(s: &str) -> rusqlite::Result<Vec<Attachment>> {
    let v: serde_json::Value =
        serde_json::from_str(s).map_err(|_| decode_err("attachments json", s))?;
    let arr = v
        .as_array()
        .ok_or_else(|| decode_err("attachments json (not array)", s))?;
    let mut out = Vec::with_capacity(arr.len());
    for a in arr {
        let kind_s = a
            .get("kind")
            .and_then(|x| x.as_str())
            .ok_or_else(|| decode_err("attachment.kind", s))?;
        let kind = AttachmentKind::from_wire(kind_s)
            .ok_or_else(|| decode_err("attachment.kind (unknown)", kind_s))?;
        let url = a
            .get("url")
            .and_then(|x| x.as_str())
            .ok_or_else(|| decode_err("attachment.url", s))?
            .to_string();
        let origin_author = a
            .get("origin_author")
            .and_then(|x| x.as_str())
            .map(|x| x.to_string());
        out.push(Attachment {
            kind,
            url,
            origin_author,
        });
    }
    Ok(out)
}
fn mentions_to_json(m: &[SubjectId]) -> String {
    serde_json::to_string(m).unwrap_or_else(|_| "[]".into())
}
/// 壊れた JSON を空の一覧へ倒さない。読めなければ変換の失敗（§15）。
fn mentions_from_json(s: &str) -> rusqlite::Result<Vec<SubjectId>> {
    serde_json::from_str(s).map_err(|_| decode_err("mentions json", s))
}

/// スキーマ（詳細§03）。**`IF NOT EXISTS`** にしてあるので、ファイルの DB を再起動で開き直しても
/// 二重作成で落ちない — 権威は全部ここに残り、場もログも生き残る（プロトコル§08・詳細§11）。
const SCHEMA: &str = r#"
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS places(
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              address TEXT,
              parent_id INTEGER,
              policy_json TEXT NOT NULL,
              inherit_from_place INTEGER,
              inherit_up_to_seq INTEGER,
              created_at INTEGER NOT NULL,
              closed_at INTEGER,
              close_reason TEXT
            );

            CREATE TABLE IF NOT EXISTS channels(
              place_id INTEGER NOT NULL,
              gate_name TEXT NOT NULL,
              address TEXT NOT NULL,
              PRIMARY KEY(place_id, gate_name)
            );

            CREATE TABLE IF NOT EXISTS external_refs(
              place_id INTEGER NOT NULL,
              seq INTEGER NOT NULL,
              gate_name TEXT NOT NULL,
              external_id TEXT NOT NULL,
              direction TEXT NOT NULL,
              PRIMARY KEY(place_id, seq, gate_name)
            );
            -- 外界識別子は (場, ゲート) の中で一意（詳細§04「(場, ゲート, 識別子) で一度きり」）。
            -- 冪等を**表の制約**で守る——「引いてから書く」の隙で二重に採番しても、この UNIQUE が
            -- 二本目の ref 挿入を弾く。冪等が呼ばれ方（直列かどうか）に依存しなくなる（駆動経路非依存）。
            CREATE UNIQUE INDEX IF NOT EXISTS idx_external_refs_unique
              ON external_refs(place_id, gate_name, external_id);

            CREATE TABLE IF NOT EXISTS deliveries(
              place_id INTEGER NOT NULL,
              seq INTEGER NOT NULL,
              gate_name TEXT NOT NULL,
              state TEXT NOT NULL,
              error TEXT,
              attempted_at INTEGER NOT NULL,
              PRIMARY KEY(place_id, seq, gate_name)
            );

            -- 重複の観測（詳細§04/§10）。同じ外界識別子が二度届いたら、既にある連番を返す
            -- ——そのとき 1 行ここに残す。ゲートごとの件数は、その繋ぎ方の問題として見える。
            CREATE TABLE IF NOT EXISTS dedup_hits(
              gate_name TEXT NOT NULL,
              place_id INTEGER NOT NULL,
              external_id TEXT NOT NULL,
              existing_seq INTEGER NOT NULL,
              at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS events(
              place_id INTEGER NOT NULL,
              seq INTEGER NOT NULL,
              kind TEXT NOT NULL,
              author_subject_id INTEGER,
              author_external_id TEXT,
              content_json TEXT NOT NULL,
              mentions_json TEXT NOT NULL,
              reply_to_seq INTEGER,
              target_seq INTEGER,
              for_subject_id INTEGER,
              created_at INTEGER NOT NULL,
              -- 添付（DESIGN-images §1）。JSON 配列で URL（参照）と由来作者だけを持つ——中身は保存しない。
              -- 既定 '[]'（添付なしの既存イベントは従来どおり・後方互換）。新規 DB はここで持ち、既存 DB は
              -- 下の冪等な移行で足す。
              attachments_json TEXT NOT NULL DEFAULT '[]',
              PRIMARY KEY(place_id, seq)
            );

            CREATE TABLE IF NOT EXISTS subjects(
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              kind TEXT NOT NULL,
              -- name = 表示名（同一性）、persona = 人格本文（統括裁定で 2 列に分離）。新規 DB はここで
              -- 両列を持つ。既存 DB（name 列が無い）は下の冪等な移行で name を足し persona からコピーする。
              name TEXT NOT NULL DEFAULT '',
              persona TEXT NOT NULL,
              turn_runner TEXT NOT NULL,
              standing TEXT NOT NULL,
              created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS subject_identities(
              subject_id INTEGER NOT NULL,
              gate_name TEXT NOT NULL,
              external_id TEXT NOT NULL,
              PRIMARY KEY(gate_name, external_id)
            );

            CREATE TABLE IF NOT EXISTS memberships(
              place_id INTEGER NOT NULL,
              subject_id INTEGER NOT NULL,
              role TEXT NOT NULL,
              read_seq INTEGER NOT NULL,
              joined_at INTEGER NOT NULL,
              left_at INTEGER,
              PRIMARY KEY(place_id, subject_id)
            );

            -- read_seq より後の standing 区間を即応で先に読んだ事実。先頭の通常区間を捨てずに
            -- 後続の即応区間だけを claim するための疎な既読で、read_seq が追いついたら削除する。
            CREATE TABLE IF NOT EXISTS out_of_order_reads(
              place_id INTEGER NOT NULL,
              subject_id INTEGER NOT NULL,
              seq INTEGER NOT NULL,
              PRIMARY KEY(place_id, subject_id, seq)
            );

            CREATE TABLE IF NOT EXISTS activities(
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              place_id INTEGER NOT NULL,
              subject_id INTEGER NOT NULL,
              kind TEXT NOT NULL,
              label TEXT,
              deadline_at INTEGER NOT NULL,
              started_at INTEGER NOT NULL,
              ended_at INTEGER,
              end_reason TEXT,
              detached_from INTEGER,
              origin_from_exclusive INTEGER,
              origin_to_inclusive INTEGER,
              origin_standing TEXT,
              accepted_tool_name TEXT,
              accepted_tool_args_json TEXT
            );

            -- Background の結果（Settled と再起動時の Interrupted）は、結果本文だけでなく、発端
            -- range・standing・受理 tool call・activity id を同じ event seq に結びつける。
            -- events への追記とこの行は 1 transaction で行う。
            CREATE TABLE IF NOT EXISTS settled_provenance(
              place_id INTEGER NOT NULL,
              seq INTEGER NOT NULL,
              activity_id INTEGER NOT NULL,
              origin_from_exclusive INTEGER NOT NULL,
              origin_to_inclusive INTEGER NOT NULL,
              origin_standing TEXT NOT NULL,
              accepted_tool_name TEXT NOT NULL,
              accepted_tool_args_json TEXT NOT NULL,
              PRIMARY KEY(place_id, seq)
            );
            -- 1 activity の結果は 1 event だけ。再起動回収を繰り返しても Background の
            -- Interrupted を二重に作れないことを表でも守る。
            CREATE UNIQUE INDEX IF NOT EXISTS idx_settled_provenance_activity
              ON settled_provenance(activity_id);

            -- provenance 導入前の schema から移行した Background の明示的な形式タグ。
            -- NULL provenance だけでは「旧形式」と「新形式の破損」を区別できないため、移行時にだけ記録する。
            -- 旧形式は汎用 Interrupted へ収束し、新形式の Background は引き続き完全な provenance を要求する。
            CREATE TABLE IF NOT EXISTS legacy_background_activities(
              activity_id INTEGER PRIMARY KEY
            );

            CREATE TABLE IF NOT EXISTS turn_records(
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              place_id INTEGER NOT NULL,
              subject_id INTEGER NOT NULL,
              activity_id INTEGER NOT NULL,
              inherit_from_seq INTEGER,
              inherit_to_seq INTEGER,
              iterations INTEGER NOT NULL,
              started_at INTEGER NOT NULL,
              ended_at INTEGER NOT NULL,
              end_reason TEXT NOT NULL,
              -- terminal な Engine failure の本文。分類は core が済ませ、store は逐語で写す。
              failure_detail TEXT,
              -- NO_REPLY（平文アクション文法）で配送を保留した地の文。配送しない代わりにここへ残す
              -- （外界にも場の共有ログにも出さない）。新規 DB はここで作られ、既存 DB は下の冪等な移行で足す。
              withheld_text TEXT,
              -- 受理した平文ツール行を逐語で残したもの（平文ツール行の設計）。ツール行は say として配送
              -- されず受理イベントも積まないので、黙って消さないためにここへ残す。移行も下で冪等に足す。
              tool_lines TEXT,
              -- 返答の絞りの会計キー（DESIGN-attention §2）。このターンを起こした**着火作者**の正規化キー。
              -- 高消費作者ごとに直近窓の消費を積算するために使う。オーナー・系の出来事・着火作者の無い
              -- 発火（batch/unconditional）では NULL——会計に載らない（オーナーは常に無制限）。移行も下で足す。
              fired_by TEXT
            );

            CREATE TABLE IF NOT EXISTS context_records(
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              turn_record_id INTEGER NOT NULL,
              place_id INTEGER NOT NULL,
              iteration INTEGER NOT NULL,
              ctx_from_seq INTEGER,
              ctx_to_seq INTEGER,
              skipped_from_seq INTEGER,
              skipped_to_seq INTEGER,
              prompt_tokens INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS schedule(
              place_id INTEGER NOT NULL,
              reason TEXT NOT NULL,
              next_fire_at INTEGER NOT NULL,
              PRIMARY KEY(place_id, reason)
            );

            -- 展開したゲート（ツール索引の「展開」・システム設計§10）。参加（場×主体）ごとに、
            -- どのゲートのツールを索引から取り出したか。権威は DB（詳細§03）——次のターンから見える。
            -- 展開に権限上の意味は無い（権限は参加者の権限で掛かる）ので、ここは可視面の状態だけを持つ。
            CREATE TABLE IF NOT EXISTS expanded_tools(
              place_id INTEGER NOT NULL,
              subject_id INTEGER NOT NULL,
              gate_name TEXT NOT NULL,
              PRIMARY KEY(place_id, subject_id, gate_name)
            );

            -- 記憶（記憶とワーカー §01）。主体が自分で書いた短い文。**主体ごと**で跨がない
            -- （§00「共有した分だけ人格が薄まる」）。構造を持たせない（種別・タグ・重要度を切らない・§01）。
            -- 由来 = どの場の、どの連番範囲から書かれたか——ログは書き換わらないので（§03）、この 2 値
            -- （場＋範囲）からその記憶が生まれた会話をいつでも完全に再現できる（引き継ぎと同じ仕掛け・
            -- 中身を複製しない）。書かれた時刻・最後に読まれた時刻は壁時計（プロセスを跨いで意味を持つ）。
            CREATE TABLE IF NOT EXISTS memories(
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              subject_id INTEGER NOT NULL,
              body TEXT NOT NULL,
              origin_place INTEGER NOT NULL,
              origin_from_seq INTEGER NOT NULL,
              origin_to_seq INTEGER NOT NULL,
              written_at INTEGER NOT NULL,
              last_read_at INTEGER
            );

            -- 背景活動の大きな結果の退避（常時切り離し・案 A）。本体 opencrab の offload は
            -- ワークスペースのファイルへ落とすが、social runtime は権威を全部 DB に置く（詳細§03）——
            -- ファイルシステムを持ち込まず、退避先もここ（store 背番号）にする。読みは行範囲の
            -- core ツール（core-bg-read）が担う。**主体ごと**で跨がない（記憶と同じ主体分離・§00/§06）:
            -- 読みは subject_id で絞り、他人の退避を指す SQL が 0 行に落ちる。1 つの背景活動は 1 度だけ
            -- 決着する（supervise_background の遷移ガード）ので activity_id を主鍵にする（活動ごと 1 件）。
            -- truncated = 退避先の上限（OFFLOAD_BYTE_LIMIT）を超えて先頭だけ保存したか。掃除は今は
            -- しない（残り続ける問題は別課題・ISSUES）。
            CREATE TABLE IF NOT EXISTS offloads(
              activity_id INTEGER PRIMARY KEY,
              subject_id INTEGER NOT NULL,
              place_id INTEGER NOT NULL,
              body TEXT NOT NULL,
              truncated INTEGER NOT NULL,
              created_at INTEGER NOT NULL
            );

            -- モデルごとの context_window（会話予算の物差し・§06）。本体 opencrab の model_pricing の
            -- この実装への転記だが、予算に要るのは context_window だけなので単価は持たない（最小登録）。会話予算 =
            -- この context_window × compaction_ratio。未登録モデルを実効にすると起動時に fail loud
            -- （既定値へ落とさない・近いものへ寄せない・§15）。app が既知モデルを起動時に seed する。
            CREATE TABLE IF NOT EXISTS model_context_windows(
              model TEXT PRIMARY KEY,
              context_window INTEGER NOT NULL
            );

            -- shell（core builtin）の権限（DESIGN-shell.md「権限（subject 単位・core が判定）」）。
            -- 主体ごとの 2 つの許可集合で、どちらも**既定は空**（既定で閉じている）:
            --   subject_allowed_tools    = この主体が使える off-by-default な core builtin（shell は
            --                              既定で入っていない。provision / owner の設定が足す）。
            --   subject_allowed_commands = shell の中で実行できるコマンド（argv[0] の完全一致・列挙 deny
            --                              はしない）。owner の語彙 core-allow-command（OwnerFollowUp）が
            --                              **未読に owner 発話があるターンでだけ**足せる（自己拡張の禁止）。
            -- 主体ごとで跨がない（記憶・退避と同じ主体分離）——読みは subject_id で絞る。
            CREATE TABLE IF NOT EXISTS subject_allowed_tools(
              subject_id INTEGER NOT NULL,
              tool_name TEXT NOT NULL,
              PRIMARY KEY(subject_id, tool_name)
            );
            CREATE TABLE IF NOT EXISTS subject_allowed_commands(
              subject_id INTEGER NOT NULL,
              command TEXT NOT NULL,
              PRIMARY KEY(subject_id, command)
            );

            -- URL の中身取得を許す**由来作者**の信頼リスト（DESIGN-images §5）。主体ごとに、その主体の
            -- ターンで core-look / core-read が取得してよい相手（由来作者の外界識別子）。**既定は空**
            -- （owner のみ取得可——owner は standing で常に通る）。owner の語彙 core-trust / core-untrust
            -- （OwnerFollowUp）が**未読に owner 発話があるターンでだけ**足し引きする（自己拡張の禁止・
            -- core-allow-command と同型）。主体ごとで跨がない（記憶・退避・shell 許可と同じ主体分離）。
            CREATE TABLE IF NOT EXISTS subject_trusted_authors(
              subject_id INTEGER NOT NULL,
              author_external TEXT NOT NULL,
              PRIMARY KEY(subject_id, author_external)
            );
            "#;

/// 冪等な移行（既存の流儀 `IF NOT EXISTS` と揃える）。`CREATE TABLE IF NOT EXISTS` は既存テーブルへ
/// 列を足さないので、古い DB には `ALTER TABLE ... ADD COLUMN` で足す。列が既にあれば何もしない
/// （PRAGMA で確認するので、エラー文字列に依存しない）。新規 DB は SCHEMA 側で既に列を持つ。
fn migrate(conn: &Connection) -> Result<()> {
    if !column_exists(conn, "turn_records", "failure_detail")? {
        conn.execute(
            "ALTER TABLE turn_records ADD COLUMN failure_detail TEXT",
            [],
        )?;
    }
    if !column_exists(conn, "turn_records", "withheld_text")? {
        conn.execute("ALTER TABLE turn_records ADD COLUMN withheld_text TEXT", [])?;
    }
    if !column_exists(conn, "turn_records", "tool_lines")? {
        conn.execute("ALTER TABLE turn_records ADD COLUMN tool_lines TEXT", [])?;
    }
    if !column_exists(conn, "turn_records", "fired_by")? {
        conn.execute("ALTER TABLE turn_records ADD COLUMN fired_by TEXT", [])?;
    }
    conn.execute(
        "CREATE TABLE IF NOT EXISTS legacy_background_activities(
           activity_id INTEGER PRIMARY KEY
         )",
        [],
    )?;
    let provenance_columns = [
        ("origin_from_exclusive", "INTEGER"),
        ("origin_to_inclusive", "INTEGER"),
        ("origin_standing", "TEXT"),
        ("accepted_tool_name", "TEXT"),
        ("accepted_tool_args_json", "TEXT"),
    ];
    let present_provenance_columns = provenance_columns
        .iter()
        .map(|(column, _)| column_exists(conn, "activities", column))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|present| *present)
        .count();
    match present_provenance_columns {
        0 => {
            // main の旧形式だった事実を、列追加で失う前に行単位で保存する。以後に作られる
            // provenance 無し Background はここへ入らないため、旧形式だけを汎用中断として扱える。
            conn.execute(
                "INSERT OR IGNORE INTO legacy_background_activities(activity_id)
                 SELECT id FROM activities WHERE kind='background'",
                [],
            )?;
            for (column, sql_type) in provenance_columns {
                conn.execute(
                    &format!("ALTER TABLE activities ADD COLUMN {column} {sql_type}"),
                    [],
                )?;
            }
        }
        5 => {}
        count => {
            return Err(rusqlite::Error::ToSqlConversionFailure(
                format!("unknown activities provenance schema: found {count} of 5 columns").into(),
            ));
        }
    }
    // 添付列（DESIGN-images §1）。既定 '[]' で足す——添付なしの既存イベントは従来どおり読める（後方互換）。
    if !column_exists(conn, "events", "attachments_json")? {
        conn.execute(
            "ALTER TABLE events ADD COLUMN attachments_json TEXT NOT NULL DEFAULT '[]'",
            [],
        )?;
    }
    // name 列（表示名）を足し、既存の persona 値（従来は短い表示ラベルだった）を name へコピーする。
    // 冪等: 二度目は列が既にあるので何もしない。コピーは name が空の行だけ（初回は全行が空）——
    // 一度コピーした後の再実行では name が非空なので上書きしない。
    if !column_exists(conn, "subjects", "name")? {
        conn.execute(
            "ALTER TABLE subjects ADD COLUMN name TEXT NOT NULL DEFAULT ''",
            [],
        )?;
        conn.execute("UPDATE subjects SET name = persona WHERE name = ''", [])?;
    }
    Ok(())
}

/// テーブルにその列があるか（`PRAGMA table_info`）。table は系内の定数だけを渡す（PRAGMA は
/// 束縛パラメータを取らないので format! で組む——外から来る値は渡さない）。
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        if r.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn origin_reply_to(from_exclusive: Seq, to_inclusive: Seq) -> Result<Option<Seq>> {
    if to_inclusive < from_exclusive {
        return Err(rusqlite::Error::ToSqlConversionFailure(
            "activity origin range is reversed".into(),
        ));
    }
    if to_inclusive == from_exclusive {
        return Ok(None);
    }
    from_exclusive.checked_add(1).map(Some).ok_or_else(|| {
        rusqlite::Error::ToSqlConversionFailure("origin range start overflow".into())
    })
}

impl Store {
    pub fn new_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// ファイルの DB を開く（無ければ作る）。**権威を跨いで残す**ための道 — core を落として再起動しても
    /// 場もログも生き残る（詳細§03/§11）。テストは `:memory:` を使うが、実プロセスの再起動を跨ぐ検証と
    /// 本番の app はこちらを使う。スキーマは `IF NOT EXISTS` なので、開き直しても既存の権威を壊さない。
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    // ---- places ----

    pub fn create_place(
        &self,
        address: Option<&str>,
        parent: Option<PlaceId>,
        policy_json: &str,
        inherit: Option<(PlaceId, Seq)>,
        now: i64,
    ) -> Result<PlaceId> {
        let c = self.c();
        c.execute(
            "INSERT INTO places(address,parent_id,policy_json,inherit_from_place,inherit_up_to_seq,created_at)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                address,
                parent,
                policy_json,
                inherit.map(|x| x.0),
                inherit.map(|x| x.1),
                now
            ],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn get_place(&self, place: PlaceId) -> Result<Option<PlaceRow>> {
        self.c()
            .query_row(
                "SELECT id,address,parent_id,policy_json,inherit_from_place,inherit_up_to_seq,closed_at,close_reason
                 FROM places WHERE id=?1",
                params![place],
                |r| {
                    Ok(PlaceRow {
                        id: r.get(0)?,
                        address: r.get(1)?,
                        parent_id: r.get(2)?,
                        policy_json: r.get(3)?,
                        inherit_from_place: r.get(4)?,
                        inherit_up_to_seq: r.get(5)?,
                        closed_at: r.get(6)?,
                        close_reason: r.get(7)?,
                    })
                },
            )
            .optional()
    }

    pub fn set_policy(&self, place: PlaceId, policy_json: &str) -> Result<()> {
        self.c().execute(
            "UPDATE places SET policy_json=?2 WHERE id=?1",
            params![place, policy_json],
        )?;
        Ok(())
    }

    pub fn close_place(&self, place: PlaceId, reason: &str, now: i64) -> Result<()> {
        self.c().execute(
            "UPDATE places SET closed_at=?2, close_reason=?3 WHERE id=?1 AND closed_at IS NULL",
            params![place, now, reason],
        )?;
        Ok(())
    }

    pub fn child_places(&self, parent: PlaceId) -> Result<Vec<PlaceRow>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT id,address,parent_id,policy_json,inherit_from_place,inherit_up_to_seq,closed_at,close_reason
             FROM places WHERE parent_id=?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![parent], |r| {
                Ok(PlaceRow {
                    id: r.get(0)?,
                    address: r.get(1)?,
                    parent_id: r.get(2)?,
                    policy_json: r.get(3)?,
                    inherit_from_place: r.get(4)?,
                    inherit_up_to_seq: r.get(5)?,
                    closed_at: r.get(6)?,
                    close_reason: r.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn all_open_places(&self) -> Result<Vec<PlaceId>> {
        let c = self.c();
        let mut stmt = c.prepare("SELECT id FROM places WHERE closed_at IS NULL ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, PlaceId>(0))?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- events / log ----

    /// 追記の中で採番する（BEGIN IMMEDIATE 相当をトランザクションで）。seq は場ごとに 1 から単調増加。
    pub fn append(&self, place: PlaceId, ev: &NewEvent, now: i64) -> Result<Seq> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // 採番の失敗を 1 に倒さない（既存行を上書きしうる）。DB エラーはそのまま返す（詳細§15）。
        let seq: Seq = tx.query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM events WHERE place_id=?1",
            params![place],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO events(place_id,seq,kind,author_subject_id,author_external_id,content_json,mentions_json,reply_to_seq,target_seq,for_subject_id,created_at,attachments_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                place,
                seq,
                ev.kind.as_str(),
                ev.author_subject,
                ev.author_external,
                content_to_json(&ev.content),
                mentions_to_json(&ev.mentions),
                ev.reply_to,
                ev.target,
                ev.for_subject,
                now,
                attachments_to_json(&ev.attachments)
            ],
        )?;
        tx.commit()?;
        Ok(seq)
    }

    /// running Background を通常決着させ、結果 event・provenance（必要なら offload）を同じ
    /// transaction で確定する。stop / deadline / success / failure の競合は、最初に commit した結果へ
    /// 収束する。commit 後の再実行も `AlreadyRecorded` となり、結果を二重追記しない。
    pub fn settle_background_activity(
        &self,
        id: ActivityId,
        end_reason: &str,
        content: &str,
        offload: Option<&NewBackgroundOffload>,
        ended_at: i64,
        event_created_at: i64,
    ) -> Result<BackgroundSettlement> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let activity = tx.query_row(
            "SELECT id,place_id,subject_id,kind,label,deadline_at,started_at,ended_at,end_reason,detached_from,origin_from_exclusive,origin_to_inclusive,origin_standing,accepted_tool_name,accepted_tool_args_json
             FROM activities WHERE id=?1",
            params![id],
            map_activity,
        )?;
        let existing_result: Option<(PlaceId, Seq)> = tx
            .query_row(
                "SELECT place_id,seq FROM settled_provenance WHERE activity_id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((place, seq)) = existing_result {
            if activity.kind != ActivityKindTag::Background
                || activity.ended_at.is_none()
                || activity.place != place
            {
                return Err(rusqlite::Error::ToSqlConversionFailure(
                    "settled provenance does not match its background activity".into(),
                ));
            }
            tx.commit()?;
            return Ok(BackgroundSettlement::AlreadyRecorded { place, seq });
        }
        if activity.ended_at.is_some() {
            return Err(rusqlite::Error::ToSqlConversionFailure(
                "background activity ended without settled provenance".into(),
            ));
        }
        if activity.kind != ActivityKindTag::Background {
            return Err(rusqlite::Error::ToSqlConversionFailure(
                "only background activity can be settled".into(),
            ));
        }
        let provenance = activity.provenance.ok_or_else(|| {
            rusqlite::Error::ToSqlConversionFailure(
                "background activity has no accepted tool provenance".into(),
            )
        })?;
        if provenance.origin_to_inclusive < provenance.origin_from_exclusive {
            return Err(rusqlite::Error::ToSqlConversionFailure(
                "settled origin range is reversed".into(),
            ));
        }

        let changed = tx.execute(
            "UPDATE activities SET ended_at=?2,end_reason=?3 WHERE id=?1 AND ended_at IS NULL",
            params![id, ended_at, end_reason],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::ToSqlConversionFailure(
                "background settlement transition was lost".into(),
            ));
        }
        if let Some(offload) = offload {
            tx.execute(
                "INSERT INTO offloads(activity_id,subject_id,place_id,body,truncated,created_at)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    activity.id,
                    activity.subject,
                    activity.place,
                    offload.body,
                    offload.truncated as i64,
                    event_created_at,
                ],
            )?;
        }
        let reply_to = origin_reply_to(
            provenance.origin_from_exclusive,
            provenance.origin_to_inclusive,
        )?;
        let seq: Seq = tx.query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM events WHERE place_id=?1",
            params![activity.place],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO events(place_id,seq,kind,author_subject_id,author_external_id,content_json,mentions_json,reply_to_seq,target_seq,for_subject_id,created_at,attachments_json)
             VALUES(?1,?2,'settled',NULL,NULL,?3,'[]',?4,NULL,?5,?6,'[]')",
            params![
                activity.place,
                seq,
                content_to_json(&Content::text(content)),
                reply_to,
                activity.subject,
                event_created_at,
            ],
        )?;
        let args_json = serde_json::to_string(&provenance.tool_args)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        tx.execute(
            "INSERT INTO settled_provenance(place_id,seq,activity_id,origin_from_exclusive,origin_to_inclusive,origin_standing,accepted_tool_name,accepted_tool_args_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                activity.place,
                seq,
                activity.id,
                provenance.origin_from_exclusive,
                provenance.origin_to_inclusive,
                standing_str(provenance.origin_standing),
                provenance.tool_name,
                args_json,
            ],
        )?;
        tx.commit()?;
        Ok(BackgroundSettlement::Appended {
            place: activity.place,
            seq,
        })
    }

    /// 再起動で残った activity を閉じ、対応する Interrupted event を同じ transaction で追記する。
    /// 新形式 Background は保存済み provenance も同じ transaction に入れる。移行時に明示分類した
    /// 旧形式 Background だけは、provenance の無い汎用 Interrupted として同じ transaction で閉じる。
    ///
    /// provenance 導入後の旧処理が残し得た「activity は interrupted 済みだが結果 event は無い」
    /// Background も、結果が未記録ならここで回収する。既に結果がある Background と、既に終わった
    /// Turn は no-op なので、起動を繰り返しても重複しない。
    pub fn interrupt_activity(
        &self,
        id: ActivityId,
        content: &str,
        ended_at: i64,
        event_created_at: i64,
    ) -> Result<ActivityInterruption> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let activity = tx.query_row(
            "SELECT id,place_id,subject_id,kind,label,deadline_at,started_at,ended_at,end_reason,detached_from,origin_from_exclusive,origin_to_inclusive,origin_standing,accepted_tool_name,accepted_tool_args_json
             FROM activities WHERE id=?1",
            params![id],
            map_activity,
        )?;
        let legacy_background = tx.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM legacy_background_activities WHERE activity_id=?1
             )",
            params![id],
            |r| Ok(r.get::<_, i64>(0)? != 0),
        )?;
        let background = match (
            activity.kind,
            legacy_background,
            activity.provenance.clone(),
        ) {
            (ActivityKindTag::Turn, false, None) => None,
            (ActivityKindTag::Background, false, Some(provenance)) => Some(provenance),
            // main 由来の旧形式だけは、provenance 導入前と同じ汎用 Interrupted にする。
            (ActivityKindTag::Background, true, None) => None,
            (ActivityKindTag::Background, false, None) => {
                return Err(rusqlite::Error::ToSqlConversionFailure(
                    "background activity has no accepted tool provenance".into(),
                ));
            }
            _ => {
                return Err(rusqlite::Error::ToSqlConversionFailure(
                    "activity provenance does not match its stored format".into(),
                ));
            }
        };
        let existing_result: Option<(PlaceId, Seq)> = tx
            .query_row(
                "SELECT place_id,seq FROM settled_provenance WHERE activity_id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        if activity.ended_at.is_some() {
            if activity.kind == ActivityKindTag::Background
                && activity.end_reason.as_deref() == Some("interrupted")
            {
                if let Some((place, seq)) = existing_result {
                    tx.commit()?;
                    return Ok(ActivityInterruption::AlreadyRecorded { place, seq });
                }
                // 旧 startup が activity だけを先に閉じて停止した中間状態。下で結果を補う。
            } else {
                tx.commit()?;
                return Ok(ActivityInterruption::AlreadyEnded);
            }
        } else {
            if existing_result.is_some() {
                return Err(rusqlite::Error::ToSqlConversionFailure(
                    "running activity already has settled provenance".into(),
                ));
            }
            let changed = tx.execute(
                "UPDATE activities SET ended_at=?2,end_reason='interrupted'
                 WHERE id=?1 AND ended_at IS NULL",
                params![id, ended_at],
            )?;
            if changed != 1 {
                return Err(rusqlite::Error::ToSqlConversionFailure(
                    "activity interruption transition was lost".into(),
                ));
            }
        }

        let reply_to = background
            .as_ref()
            .map(|provenance| {
                origin_reply_to(
                    provenance.origin_from_exclusive,
                    provenance.origin_to_inclusive,
                )
            })
            .transpose()?
            .flatten();
        let seq: Seq = tx.query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM events WHERE place_id=?1",
            params![activity.place],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO events(place_id,seq,kind,author_subject_id,author_external_id,content_json,mentions_json,reply_to_seq,target_seq,for_subject_id,created_at,attachments_json)
             VALUES(?1,?2,'interrupted',NULL,NULL,?3,'[]',?4,NULL,?5,?6,'[]')",
            params![
                activity.place,
                seq,
                content_to_json(&Content::text(content)),
                reply_to,
                activity.subject,
                event_created_at,
            ],
        )?;
        if let Some(provenance) = background {
            let args_json = serde_json::to_string(&provenance.tool_args)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            tx.execute(
                "INSERT INTO settled_provenance(place_id,seq,activity_id,origin_from_exclusive,origin_to_inclusive,origin_standing,accepted_tool_name,accepted_tool_args_json)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    activity.place,
                    seq,
                    activity.id,
                    provenance.origin_from_exclusive,
                    provenance.origin_to_inclusive,
                    standing_str(provenance.origin_standing),
                    provenance.tool_name,
                    args_json,
                ],
            )?;
        }
        tx.commit()?;
        Ok(ActivityInterruption::Appended {
            place: activity.place,
            seq,
        })
    }

    pub fn settled_provenance(
        &self,
        place: PlaceId,
        seq: Seq,
    ) -> Result<Option<SettledProvenance>> {
        self.c()
            .query_row(
                "SELECT activity_id,origin_from_exclusive,origin_to_inclusive,origin_standing,accepted_tool_name,accepted_tool_args_json
                 FROM settled_provenance WHERE place_id=?1 AND seq=?2",
                params![place, seq],
                map_settled_provenance,
            )
            .optional()
    }

    /// 外界識別子つきの出来事を、**1 トランザクションで**畳み・採番・追記・ref(in) 記録まで行う（詳細§04）。
    ///
    /// これで冪等が呼ばれ方に依存しなくなる: dedup 検査（SELECT）と採番・イベント挿入・ref 挿入が
    /// 1 つの `BEGIN IMMEDIATE` の中に入るので、途中で別の追記が割り込めない。加えて
    /// external_refs の UNIQUE(place, gate, external_id) が**表として**二重採番を弾く——検査を
    /// すり抜けても二本目の ref 挿入が失敗し、tx ごと巻き戻る。既にあれば `Duplicate(seq)`、
    /// 無ければ `Appended(seq)` を返す。「引けなかった」（DB エラー）は握り潰さず上げる（§15）。
    pub fn append_incoming(
        &self,
        place: PlaceId,
        ev: &NewEvent,
        gate: &GateName,
        origin: &str,
        now: i64,
    ) -> Result<Ingest> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // 畳み検査（tx の中で。UNIQUE と同じ鍵で引く）。
        let existing: Option<Seq> = tx
            .query_row(
                "SELECT seq FROM external_refs WHERE place_id=?1 AND gate_name=?2 AND external_id=?3",
                params![place, gate.as_str(), origin],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(seq) = existing {
            // 同じものなので同じ答えを返す。二重に書かない（tx はそのまま drop でロールバック）。
            return Ok(Ingest::Duplicate(seq));
        }
        let seq: Seq = tx.query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM events WHERE place_id=?1",
            params![place],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO events(place_id,seq,kind,author_subject_id,author_external_id,content_json,mentions_json,reply_to_seq,target_seq,for_subject_id,created_at,attachments_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                place,
                seq,
                ev.kind.as_str(),
                ev.author_subject,
                ev.author_external,
                content_to_json(&ev.content),
                mentions_to_json(&ev.mentions),
                ev.reply_to,
                ev.target,
                ev.for_subject,
                now,
                attachments_to_json(&ev.attachments)
            ],
        )?;
        // ref(in) を同じ tx で。UNIQUE が belt-and-suspenders（検査をすり抜けた並行挿入は
        // ここで弾かれ、tx ごと巻き戻る）。
        tx.execute(
            "INSERT INTO external_refs(place_id,seq,gate_name,external_id,direction) VALUES(?1,?2,?3,?4,'in')",
            params![place, seq, gate.as_str(), origin],
        )?;
        tx.commit()?;
        Ok(Ingest::Appended(seq))
    }

    /// 効果の確定（詳細§08 の擬似コード）: イベントの追記と、**運ぶチャネルの数だけ**の
    /// `deliveries(state=pending)` を **1 つの `BEGIN IMMEDIATE`** で作る。
    ///
    /// 配送の行が確定と同時に生まれるので、後で配送側が DB を引けなくても行は残り、`pending` の
    /// まま見える——「配送できなかった確定効果が黙って消える」が構造的に起きない。これを可観測にする
    /// ための新しい機構は要らない（`deliveries` 表がそのまま担う）。運ぶチャネルは呼び手（core）が
    /// 効果の宛先から決めて渡す（§08 の「宛先あり＝1 本 / 宛先なし＝全チャネル」）。
    pub fn append_with_deliveries(
        &self,
        place: PlaceId,
        ev: &NewEvent,
        channels: &[(GateName, String)],
        now: i64,
    ) -> Result<Seq> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let seq: Seq = tx.query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM events WHERE place_id=?1",
            params![place],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO events(place_id,seq,kind,author_subject_id,author_external_id,content_json,mentions_json,reply_to_seq,target_seq,for_subject_id,created_at,attachments_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                place,
                seq,
                ev.kind.as_str(),
                ev.author_subject,
                ev.author_external,
                content_to_json(&ev.content),
                mentions_to_json(&ev.mentions),
                ev.reply_to,
                ev.target,
                ev.for_subject,
                now,
                attachments_to_json(&ev.attachments)
            ],
        )?;
        for (gate, _addr) in channels {
            tx.execute(
                "INSERT INTO deliveries(place_id,seq,gate_name,state,error,attempted_at)
                 VALUES(?1,?2,?3,'pending',NULL,?4)",
                params![place, seq, gate.as_str(), now],
            )?;
        }
        tx.commit()?;
        Ok(seq)
    }

    pub fn latest_seq(&self, place: PlaceId) -> Result<Seq> {
        self.c().query_row(
            "SELECT COALESCE(MAX(seq),0) FROM events WHERE place_id=?1",
            params![place],
            |r| r.get(0),
        )
    }

    pub fn get_event(&self, place: PlaceId, seq: Seq) -> Result<Option<EventRow>> {
        self.c()
            .query_row(
                "SELECT place_id,seq,kind,author_subject_id,author_external_id,content_json,mentions_json,reply_to_seq,target_seq,for_subject_id,created_at,attachments_json
                 FROM events WHERE place_id=?1 AND seq=?2",
                params![place, seq],
                map_event,
            )
            .optional()
    }

    /// (from, to] の範囲を昇順で読む。
    pub fn read_range(
        &self,
        place: PlaceId,
        from_excl: Seq,
        to_incl: Seq,
    ) -> Result<Vec<EventRow>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT place_id,seq,kind,author_subject_id,author_external_id,content_json,mentions_json,reply_to_seq,target_seq,for_subject_id,created_at,attachments_json
             FROM events WHERE place_id=?1 AND seq>?2 AND seq<=?3 ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(params![place, from_excl, to_incl], map_event)?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn event_count(&self, place: PlaceId) -> Result<i64> {
        self.c().query_row(
            "SELECT COUNT(*) FROM events WHERE place_id=?1",
            params![place],
            |r| r.get(0),
        )
    }

    // ---- subjects / identities / memberships ----

    pub fn create_subject(
        &self,
        kind: SubjectKind,
        name: &str,
        persona: &str,
        turn_runner: &str,
        standing: Standing,
        now: i64,
    ) -> Result<SubjectId> {
        let c = self.c();
        c.execute(
            "INSERT INTO subjects(kind,name,persona,turn_runner,standing,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
            params![kind_str(kind), name, persona, turn_runner, standing_str(standing), now],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn get_subject(&self, id: SubjectId) -> Result<Option<SubjectRow>> {
        self.c()
            .query_row(
                "SELECT id,kind,name,persona,turn_runner,standing FROM subjects WHERE id=?1",
                params![id],
                |r| {
                    Ok(SubjectRow {
                        id: r.get(0)?,
                        kind: kind_from(&r.get::<_, String>(1)?)?,
                        name: r.get(2)?,
                        persona: r.get(3)?,
                        turn_runner: r.get(4)?,
                        standing: standing_from(&r.get::<_, String>(5)?)?,
                    })
                },
            )
            .optional()
    }

    pub fn add_identity(&self, subject: SubjectId, gate: &GateName, external: &str) -> Result<()> {
        self.c().execute(
            "INSERT INTO subject_identities(subject_id,gate_name,external_id) VALUES(?1,?2,?3)",
            params![subject, gate.as_str(), external],
        )?;
        Ok(())
    }

    /// 名寄せ。見つからなければ None（=主体は付かない・権限ゼロ）。
    pub fn resolve_subject(&self, gate: &GateName, external: &str) -> Result<Option<SubjectId>> {
        self.c()
            .query_row(
                "SELECT subject_id FROM subject_identities WHERE gate_name=?1 AND external_id=?2",
                params![gate.as_str(), external],
                |r| r.get(0),
            )
            .optional()
    }

    /// 主体のそのゲートでの外界識別子（subject → external）。read の `author.id` に使う（プロトコル§02）。
    /// そのゲートに素性が無ければ None（その主体はそのゲートから見えない名前）。
    pub fn identity_on_gate(&self, subject: SubjectId, gate: &GateName) -> Result<Option<String>> {
        self.c()
            .query_row(
                "SELECT external_id FROM subject_identities WHERE subject_id=?1 AND gate_name=?2",
                params![subject, gate.as_str()],
                |r| r.get(0),
            )
            .optional()
    }

    /// ある立場（standing）の主体の外界識別子を**全ゲートにわたって**集める（DESIGN-attention §1）。
    /// 元栓の許可集合の owner 源を組むのに使う——owner はどのゲートから来ても常に着火を許すので、
    /// owner のすべての素性（web / nostr / …）を集めて許可集合へ入れる。**更新経路でだけ**呼ぶ
    /// （ホットパスの照合はメモリ集合で行い DB を触らない・耐フラッド）。
    pub fn identities_with_standing(&self, standing: Standing) -> Result<Vec<String>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT si.external_id FROM subject_identities si
             JOIN subjects s ON s.id = si.subject_id
             WHERE s.standing = ?1",
        )?;
        let rows = stmt
            .query_map(params![standing_str(standing)], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn join(
        &self,
        place: PlaceId,
        subject: SubjectId,
        role: Role,
        read_seq: Seq,
        now: i64,
    ) -> Result<()> {
        self.c().execute(
            "INSERT INTO memberships(place_id,subject_id,role,read_seq,joined_at)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(place_id,subject_id) DO UPDATE SET role=?3, left_at=NULL",
            params![place, subject, role_str(role), read_seq, now],
        )?;
        Ok(())
    }

    pub fn get_membership(
        &self,
        place: PlaceId,
        subject: SubjectId,
    ) -> Result<Option<MembershipRow>> {
        self.c()
            .query_row(
                "SELECT place_id,subject_id,role,read_seq FROM memberships WHERE place_id=?1 AND subject_id=?2 AND left_at IS NULL",
                params![place, subject],
                |r| {
                    Ok(MembershipRow {
                        place: r.get(0)?,
                        subject: r.get(1)?,
                        role: role_from(&r.get::<_, String>(2)?)?,
                        read_seq: r.get(3)?,
                    })
                },
            )
            .optional()
    }

    pub fn members(&self, place: PlaceId) -> Result<Vec<MembershipRow>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT place_id,subject_id,role,read_seq FROM memberships WHERE place_id=?1 AND left_at IS NULL ORDER BY subject_id",
        )?;
        let rows = stmt
            .query_map(params![place], |r| {
                Ok(MembershipRow {
                    place: r.get(0)?,
                    subject: r.get(1)?,
                    role: role_from(&r.get::<_, String>(2)?)?,
                    read_seq: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn set_read_seq(&self, place: PlaceId, subject: SubjectId, read_seq: Seq) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE memberships SET read_seq=?3 WHERE place_id=?1 AND subject_id=?2",
            params![place, subject, read_seq],
        )?;
        tx.execute(
            "DELETE FROM out_of_order_reads
             WHERE place_id=?1 AND subject_id=?2 AND seq<=?3",
            params![place, subject, read_seq],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `read_seq` より後で既に読んだ event の seq。後続 standing の即応を先に claim しても、
    /// 先頭 standing を未読のまま保持するための疎な既読である。
    pub fn out_of_order_read_seqs(
        &self,
        place: PlaceId,
        subject: SubjectId,
        from_excl: Seq,
        to_incl: Seq,
    ) -> Result<Vec<Seq>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT seq FROM out_of_order_reads
             WHERE place_id=?1 AND subject_id=?2 AND seq>?3 AND seq<=?4 ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(params![place, subject, from_excl, to_incl], |r| r.get(0))?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 1 standing 区間を既読にする。先頭区間なら cursor を進め、直後に連続する疎な既読も
    /// 同じ transaction で畳む。後続区間なら cursor を動かさず、各 seq を疎な既読として残す。
    pub fn mark_read_group(
        &self,
        place: PlaceId,
        subject: SubjectId,
        seqs: &[Seq],
        is_prefix: bool,
    ) -> Result<()> {
        let Some(last) = seqs.last().copied() else {
            return Ok(());
        };
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if is_prefix {
            let mut cursor = last;
            let later = {
                let mut stmt = tx.prepare(
                    "SELECT seq FROM out_of_order_reads
                     WHERE place_id=?1 AND subject_id=?2 AND seq>?3 ORDER BY seq",
                )?;
                let rows = stmt
                    .query_map(params![place, subject, cursor], |r| r.get::<_, Seq>(0))?
                    .collect::<Result<Vec<_>>>()?;
                rows
            };
            for seq in later {
                if seq != cursor.saturating_add(1) {
                    break;
                }
                cursor = seq;
            }
            tx.execute(
                "UPDATE memberships SET read_seq=?3
                 WHERE place_id=?1 AND subject_id=?2 AND read_seq<?3",
                params![place, subject, cursor],
            )?;
            tx.execute(
                "DELETE FROM out_of_order_reads
                 WHERE place_id=?1 AND subject_id=?2 AND seq<=?3",
                params![place, subject, cursor],
            )?;
        } else {
            {
                let mut stmt = tx.prepare(
                    "INSERT OR IGNORE INTO out_of_order_reads(place_id,subject_id,seq)
                     VALUES(?1,?2,?3)",
                )?;
                for seq in seqs {
                    stmt.execute(params![place, subject, seq])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- activities ----

    #[allow(clippy::too_many_arguments)]
    pub fn start_activity(
        &self,
        place: PlaceId,
        subject: SubjectId,
        kind: ActivityKindTag,
        label: Option<&str>,
        deadline_at: i64,
        started_at: i64,
        detached_from: Option<ActivityId>,
    ) -> Result<ActivityId> {
        self.start_activity_with_provenance(
            place,
            subject,
            kind,
            label,
            deadline_at,
            started_at,
            detached_from,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_activity_with_provenance(
        &self,
        place: PlaceId,
        subject: SubjectId,
        kind: ActivityKindTag,
        label: Option<&str>,
        deadline_at: i64,
        started_at: i64,
        detached_from: Option<ActivityId>,
        provenance: Option<&BackgroundProvenance>,
    ) -> Result<ActivityId> {
        if provenance.is_some_and(|p| p.origin_to_inclusive < p.origin_from_exclusive) {
            return Err(rusqlite::Error::ToSqlConversionFailure(
                "activity origin range is reversed".into(),
            ));
        }
        let c = self.c();
        let args_json = provenance
            .map(|p| serde_json::to_string(&p.tool_args))
            .transpose()
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        c.execute(
            "INSERT INTO activities(place_id,subject_id,kind,label,deadline_at,started_at,detached_from,origin_from_exclusive,origin_to_inclusive,origin_standing,accepted_tool_name,accepted_tool_args_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                place,
                subject,
                atag_str(kind),
                label,
                deadline_at,
                started_at,
                detached_from,
                provenance.map(|p| p.origin_from_exclusive),
                provenance.map(|p| p.origin_to_inclusive),
                provenance.map(|p| standing_str(p.origin_standing)),
                provenance.map(|p| p.tool_name.as_str()),
                args_json,
            ],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// 実際に「走っている→終わり」の遷移を起こしたときだけ true。
    /// 既に終わっているものへの二度目の終了は 0 行（false）— 呼び手はそれを不正として扱える（詳細§02）。
    pub fn end_activity(&self, id: ActivityId, reason: &str, ended_at: i64) -> Result<bool> {
        let n = self.c().execute(
            "UPDATE activities SET ended_at=?2, end_reason=?3 WHERE id=?1 AND ended_at IS NULL",
            params![id, ended_at, reason],
        )?;
        Ok(n > 0)
    }

    /// 走っている活動の label を差し替える（PROGRESS・進捗の揮発表示）。**終わった活動には効かない**
    /// （ended_at IS NULL のときだけ）——決着後に表示だけが動くのを塞ぐ。遷移を起こしたら true。
    pub fn set_activity_label(&self, id: ActivityId, label: &str) -> Result<bool> {
        let n = self.c().execute(
            "UPDATE activities SET label=?2 WHERE id=?1 AND ended_at IS NULL",
            params![id, label],
        )?;
        Ok(n > 0)
    }

    pub fn get_activity(&self, id: ActivityId) -> Result<Option<ActivityRow>> {
        self.c()
            .query_row(
                "SELECT id,place_id,subject_id,kind,label,deadline_at,started_at,ended_at,end_reason,detached_from,origin_from_exclusive,origin_to_inclusive,origin_standing,accepted_tool_name,accepted_tool_args_json
                 FROM activities WHERE id=?1",
                params![id],
                map_activity,
            )
            .optional()
    }

    pub fn all_activities(&self) -> Result<Vec<ActivityRow>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT id,place_id,subject_id,kind,label,deadline_at,started_at,ended_at,end_reason,detached_from,origin_from_exclusive,origin_to_inclusive,origin_standing,accepted_tool_name,accepted_tool_args_json
             FROM activities ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], map_activity)?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn running_activities(&self) -> Result<Vec<ActivityRow>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT id,place_id,subject_id,kind,label,deadline_at,started_at,ended_at,end_reason,detached_from,origin_from_exclusive,origin_to_inclusive,origin_standing,accepted_tool_name,accepted_tool_args_json
             FROM activities WHERE ended_at IS NULL ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], map_activity)?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 再起動で原子的に中断へ収束させる対象。通常の running 行に加え、provenance 導入後の旧処理が
    /// activity の終端だけを確定して結果追記前に止まった Background を拾う。provenance 列の無い
    /// main 由来の中断済み Background は、旧 startup が既に汎用 Interrupted を記録した正当な履歴なので
    /// 再回収しない。main 由来の running Background は通常の running 行として拾い、移行時の形式タグを
    /// `interrupt_activity` が判定する。通常実行中に中断された Turn も回収対象へ混ぜない。
    pub fn activities_needing_interruption(&self) -> Result<Vec<ActivityRow>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT id,place_id,subject_id,kind,label,deadline_at,started_at,ended_at,end_reason,detached_from,origin_from_exclusive,origin_to_inclusive,origin_standing,accepted_tool_name,accepted_tool_args_json
             FROM activities
             WHERE ended_at IS NULL
                OR (kind='background' AND end_reason='interrupted'
                    AND origin_from_exclusive IS NOT NULL
                    AND origin_to_inclusive IS NOT NULL
                    AND origin_standing IS NOT NULL
                    AND accepted_tool_name IS NOT NULL
                    AND accepted_tool_args_json IS NOT NULL
                    AND NOT EXISTS (
                        SELECT 1 FROM settled_provenance WHERE activity_id=activities.id
                    ))
             ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], map_activity)?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- turn records ----

    pub fn write_turn_record(&self, r: &NewTurnRecord) -> Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO turn_records(place_id,subject_id,activity_id,inherit_from_seq,inherit_to_seq,iterations,started_at,ended_at,end_reason,failure_detail,withheld_text,tool_lines,fired_by)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                r.place, r.subject, r.activity, r.inherit_from_seq, r.inherit_to_seq,
                r.iterations, r.started_at, r.ended_at, r.end_reason, r.failure_detail,
                r.withheld_text, r.tool_lines, r.fired_by
            ],
        )?;
        Ok(c.last_insert_rowid())
    }

    // ---- context records（反復ごと・§10）----

    pub fn write_context_record(&self, r: &NewContextRecord) -> Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO context_records(turn_record_id,place_id,iteration,ctx_from_seq,ctx_to_seq,skipped_from_seq,skipped_to_seq,prompt_tokens)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                r.turn_record_id, r.place, r.iteration,
                r.ctx_from_seq, r.ctx_to_seq, r.skipped_from_seq, r.skipped_to_seq, r.prompt_tokens
            ],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn context_records(&self, turn_record_id: i64) -> Result<Vec<ContextRecordRow>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT id,turn_record_id,place_id,iteration,ctx_from_seq,ctx_to_seq,skipped_from_seq,skipped_to_seq,prompt_tokens
             FROM context_records WHERE turn_record_id=?1 ORDER BY iteration",
        )?;
        let rows = stmt
            .query_map(params![turn_record_id], |r| {
                Ok(ContextRecordRow {
                    id: r.get(0)?,
                    turn_record_id: r.get(1)?,
                    place: r.get(2)?,
                    iteration: r.get(3)?,
                    ctx_from_seq: r.get(4)?,
                    ctx_to_seq: r.get(5)?,
                    skipped_from_seq: r.get(6)?,
                    skipped_to_seq: r.get(7)?,
                    prompt_tokens: r.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn turn_records(&self, place: PlaceId) -> Result<Vec<TurnRecordRow>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT id,place_id,subject_id,activity_id,inherit_from_seq,inherit_to_seq,iterations,end_reason,failure_detail,withheld_text,tool_lines,fired_by
             FROM turn_records WHERE place_id=?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![place], map_turn_record)?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn all_turn_records(&self) -> Result<Vec<TurnRecordRow>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT id,place_id,subject_id,activity_id,inherit_from_seq,inherit_to_seq,iterations,end_reason,failure_detail,withheld_text,tool_lines,fired_by
             FROM turn_records ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], map_turn_record)?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 返答の絞りの会計（DESIGN-attention §2）。着火作者キー `fired_by` の、`since`（含む・
    /// `started_at`）以降に始まったターンの消費トークンを合算する。消費の実測は反復ごとの
    /// `context_records.prompt_tokens`——ターンの各反復ぶんを足し込む（多く回ったターンほど大きい）。
    /// この関数は**現ターンを記録する前**に呼ぶので、合算は過去のターンだけを見る（現ターンは未記録）。
    pub fn consumption_since(&self, fired_by: &str, since: i64) -> Result<i64> {
        self.c().query_row(
            "SELECT COALESCE(SUM(cr.prompt_tokens), 0)
             FROM turn_records tr
             JOIN context_records cr ON cr.turn_record_id = tr.id
             WHERE tr.fired_by = ?1 AND tr.started_at >= ?2",
            params![fired_by, since],
            |r| r.get(0),
        )
    }

    // ---- schedule ----

    pub fn schedule_set(&self, place: PlaceId, reason: &str, next_fire_at: i64) -> Result<()> {
        self.c().execute(
            "INSERT INTO schedule(place_id,reason,next_fire_at) VALUES(?1,?2,?3)
             ON CONFLICT(place_id,reason) DO UPDATE SET next_fire_at=?3",
            params![place, reason, next_fire_at],
        )?;
        Ok(())
    }

    pub fn schedule_get(&self, place: PlaceId, reason: &str) -> Result<Option<i64>> {
        self.c()
            .query_row(
                "SELECT next_fire_at FROM schedule WHERE place_id=?1 AND reason=?2",
                params![place, reason],
                |r| r.get(0),
            )
            .optional()
    }

    pub fn schedule_get_batch(&self, place: PlaceId) -> Option<i64> {
        self.schedule_get(place, "batch").ok().flatten()
    }

    pub fn schedule_clear(&self, place: PlaceId, reason: &str) -> Result<()> {
        self.c().execute(
            "DELETE FROM schedule WHERE place_id=?1 AND reason=?2",
            params![place, reason],
        )?;
        Ok(())
    }

    pub fn schedule_all(&self) -> Result<Vec<(PlaceId, String, i64)>> {
        let c = self.c();
        let mut stmt =
            c.prepare("SELECT place_id,reason,next_fire_at FROM schedule ORDER BY next_fire_at")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- モデルの context_window（会話予算の物差し・§06）----

    /// モデルの context_window を登録する（冪等・同じ model は値を更新する）。app が既知モデルを
    /// 起動時に seed する経路。値の物差しは会話予算（`context_window × compaction_ratio`）。
    pub fn register_model_context_window(&self, model: &str, context_window: i64) -> Result<()> {
        self.c().execute(
            "INSERT INTO model_context_windows(model,context_window) VALUES(?1,?2)
             ON CONFLICT(model) DO UPDATE SET context_window=?2",
            params![model, context_window],
        )?;
        Ok(())
    }

    /// モデルの context_window を引く。**未登録は `None`**（既定値へ倒さない——呼び手が fail loud
    /// する・§15）。一時的な DB 失敗は `Err`（居ないと混ぜない・詳細§15）。
    pub fn model_context_window(&self, model: &str) -> Result<Option<i64>> {
        self.c()
            .query_row(
                "SELECT context_window FROM model_context_windows WHERE model=?1",
                params![model],
                |r| r.get(0),
            )
            .optional()
    }

    // ---- channels（場を外界へ結ぶ経路・詳細§03）----

    /// 場をゲートの住所へ結ぶ（冪等・プロトコル§02）。同じ (place, gate) は住所を更新する。
    pub fn add_channel(&self, place: PlaceId, gate: &GateName, address: &str) -> Result<()> {
        self.c().execute(
            "INSERT INTO channels(place_id,gate_name,address) VALUES(?1,?2,?3)
             ON CONFLICT(place_id,gate_name) DO UPDATE SET address=?3",
            params![place, gate.as_str(), address],
        )?;
        Ok(())
    }

    pub fn remove_channel(&self, place: PlaceId, gate: &GateName) -> Result<()> {
        self.c().execute(
            "DELETE FROM channels WHERE place_id=?1 AND gate_name=?2",
            params![place, gate.as_str()],
        )?;
        Ok(())
    }

    /// 場に結ばれた全チャネル（gate, address）。効果の配送先と可能な効果の和に使う（詳細§02/§08）。
    pub fn channels_for_place(&self, place: PlaceId) -> Result<Vec<(GateName, String)>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT gate_name,address FROM channels WHERE place_id=?1 ORDER BY gate_name",
        )?;
        let rows = stmt
            .query_map(params![place], |r| {
                Ok((GateName::new(r.get::<_, String>(0)?), r.get(1)?))
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// あるゲートに結ばれた全チャネル（place, address）。**（再）接続で結び直す**のに使う（プロトコル§08）。
    /// 権威は DB にあるので、接続イベントごとにここを読めば、取りこぼしなく同じ住所へ結び直せる。
    pub fn channels_for_gate(&self, gate: &GateName) -> Result<Vec<(PlaceId, String)>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT place_id,address FROM channels WHERE gate_name=?1 ORDER BY place_id",
        )?;
        let rows = stmt
            .query_map(params![gate.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 住所（gate + address）から場を引く。届いた出来事の宛先の解決に使う（プロトコル§03）。
    /// 結んでいなければ None（呼び手は `not_bound` を返す）。
    pub fn place_for_channel(&self, gate: &GateName, address: &str) -> Result<Option<PlaceId>> {
        self.c()
            .query_row(
                "SELECT place_id FROM channels WHERE gate_name=?1 AND address=?2",
                params![gate.as_str(), address],
                |r| r.get(0),
            )
            .optional()
    }

    // ---- external_refs（外界の識別子との対応・詳細§03/§08）----
    //
    // 入っていくものも出ていくものも同じ表。返信先の解決も、効果の宛先の解決も、ここ 1 本。

    /// 出来事に外界識別子を対応づける。direction は "in"（届いた）/ "out"（出した）。
    pub fn record_external_ref(
        &self,
        place: PlaceId,
        seq: Seq,
        gate: &GateName,
        external_id: &str,
        direction: &str,
    ) -> Result<()> {
        self.c().execute(
            "INSERT INTO external_refs(place_id,seq,gate_name,external_id,direction) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(place_id,seq,gate_name) DO UPDATE SET external_id=?4, direction=?5",
            params![place, seq, gate.as_str(), external_id, direction],
        )?;
        Ok(())
    }

    /// 外界識別子 → その場での seq（返信先・対象の解決・プロトコル§03）。方向は問わない。
    /// 重複の畳み込み（詳細§04）もこの表を引く——同じ (場, ゲート, 識別子) は一度きり。
    pub fn resolve_external(
        &self,
        place: PlaceId,
        gate: &GateName,
        external_id: &str,
    ) -> Result<Option<Seq>> {
        self.c()
            .query_row(
                "SELECT seq FROM external_refs WHERE place_id=?1 AND gate_name=?2 AND external_id=?3",
                params![place, gate.as_str(), external_id],
                |r| r.get(0),
            )
            .optional()
    }

    /// 重複が来たことを 1 行残す（詳細§04/§10）。畳んだ答え（existing_seq）は本物のまま。
    pub fn record_dedup(
        &self,
        gate: &GateName,
        place: PlaceId,
        external_id: &str,
        existing_seq: Seq,
        at: i64,
    ) -> Result<()> {
        self.c().execute(
            "INSERT INTO dedup_hits(gate_name,place_id,external_id,existing_seq,at) VALUES(?1,?2,?3,?4,?5)",
            params![gate.as_str(), place, external_id, existing_seq, at],
        )?;
        Ok(())
    }

    // ---- ツール索引の展開（システム設計§10）----

    /// あるゲートのツールをこの参加（場×主体）で展開したことを記録する。冪等（既にあれば無害）。
    /// 権威は DB——次のターンから索引ではなく本体として見える（詳細§03）。
    pub fn expand_gate_tools(
        &self,
        place: PlaceId,
        subject: SubjectId,
        gate: &GateName,
    ) -> Result<()> {
        self.c().execute(
            "INSERT OR IGNORE INTO expanded_tools(place_id,subject_id,gate_name) VALUES(?1,?2,?3)",
            params![place, subject, gate.as_str()],
        )?;
        Ok(())
    }

    /// この参加（場×主体）で展開済みのゲート名。広告（advertised_tools）が本体を出す判断に使う。
    pub fn expanded_gates(&self, place: PlaceId, subject: SubjectId) -> Result<Vec<GateName>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT gate_name FROM expanded_tools WHERE place_id=?1 AND subject_id=?2 ORDER BY gate_name",
        )?;
        let rows = stmt
            .query_map(params![place, subject], |r| {
                Ok(GateName::new(r.get::<_, String>(0)?))
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// そのゲートから畳んだ重複の件数（§10 の観測。調整の材料）。
    pub fn dedup_count(&self, gate: &GateName) -> Result<i64> {
        self.c().query_row(
            "SELECT COUNT(*) FROM dedup_hits WHERE gate_name=?1",
            params![gate.as_str()],
            |r| r.get(0),
        )
    }

    /// seq → そのゲートでの外界識別子（read の `origin`・プロトコル§02）。gate で絞る
    /// （同じ seq に複数ゲートの ref があり得る）。そのゲートに ref が無ければ None
    /// （他のチャネルから入った発話・まだそのゲートへ運ばれていないもの）。
    pub fn external_id_on_gate(
        &self,
        place: PlaceId,
        seq: Seq,
        gate: &GateName,
    ) -> Result<Option<String>> {
        self.c()
            .query_row(
                "SELECT external_id FROM external_refs WHERE place_id=?1 AND seq=?2 AND gate_name=?3",
                params![place, seq, gate.as_str()],
                |r| r.get(0),
            )
            .optional()
    }

    /// seq → その出来事が届いた／出たチャネルと外界識別子（効果の宛先の解決・詳細§08）。
    pub fn external_ref_of(&self, place: PlaceId, seq: Seq) -> Result<Option<(GateName, String)>> {
        self.c()
            .query_row(
                "SELECT gate_name,external_id FROM external_refs WHERE place_id=?1 AND seq=?2",
                params![place, seq],
                |r| Ok((GateName::new(r.get::<_, String>(0)?), r.get(1)?)),
            )
            .optional()
    }

    /// The recorded direction for one gate-specific external reference.
    ///
    /// This is intentionally a separate query so existing callers of `external_ref_of` keep their
    /// established tuple shape while process E2E can distinguish ingress from confirmed egress.
    pub fn external_ref_direction(
        &self,
        place: PlaceId,
        seq: Seq,
        gate: &GateName,
    ) -> Result<Option<String>> {
        self.c()
            .query_row(
                "SELECT direction FROM external_refs WHERE place_id=?1 AND seq=?2 AND gate_name=?3",
                params![place, seq, gate.as_str()],
                |r| r.get(0),
            )
            .optional()
    }

    /// その場に外界識別子つきの出来事が 1 つでもあるか（宛先にできる出来事の有無・詳細§08）。
    /// 「宛先にできる出来事だけを提示する」判定に使う。
    pub fn place_has_external_refs(&self, place: PlaceId) -> Result<bool> {
        let n: i64 = self.c().query_row(
            "SELECT COUNT(*) FROM external_refs WHERE place_id=?1",
            params![place],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    // ---- deliveries（配送の記録・詳細§08）----
    //
    // pending → sent / failed。どちらも終点。再送しないので failed から戻る辺が無い。

    pub fn record_delivery(
        &self,
        place: PlaceId,
        seq: Seq,
        gate: &GateName,
        state: &str,
        error: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.c().execute(
            "INSERT INTO deliveries(place_id,seq,gate_name,state,error,attempted_at) VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(place_id,seq,gate_name) DO UPDATE SET state=?4, error=?5, attempted_at=?6",
            params![place, seq, gate.as_str(), state, error, now],
        )?;
        Ok(())
    }

    pub fn deliveries_for(&self, place: PlaceId, seq: Seq) -> Result<Vec<(GateName, String)>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT gate_name,state FROM deliveries WHERE place_id=?1 AND seq=?2 ORDER BY gate_name",
        )?;
        let rows = stmt
            .query_map(params![place, seq], |r| {
                Ok((GateName::new(r.get::<_, String>(0)?), r.get(1)?))
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- 記憶（記憶とワーカー §01/§03）----
    //
    // どの口も **主体を引数に取る**。主体は core がターンから決めて渡す（記憶とワーカー §03「主体を
    // 引数で受け取らない・型で守る」の store 側）。読み書き・削除・書き直しはすべて `subject_id` で
    // 絞る——他人の記憶を指す SQL が、書いても 0 行に落ちる（呼び手はそれを失敗として扱える）。
    //
    // 探し方（`recall`）は語の一致で足りるか埋め込みが要るか、まだ決めていない（記憶とワーカー §07）。
    // ここが唯一の探索の seam——後でワーカーの口に差し替えても、呼び手（core・道具）は変わらない。

    /// 1 件書く（覚える）。由来 = (origin_place, from, to)。id を返す。
    pub fn remember(
        &self,
        subject: SubjectId,
        body: &str,
        origin_place: PlaceId,
        origin_from_seq: Seq,
        origin_to_seq: Seq,
        now: i64,
    ) -> Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO memories(subject_id,body,origin_place,origin_from_seq,origin_to_seq,written_at,last_read_at)
             VALUES(?1,?2,?3,?4,?5,?6,NULL)",
            params![subject, body, origin_place, origin_from_seq, origin_to_seq, now],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// 語で引く（探す）。**新しい順・上限つき**（記憶とワーカー §03）。当たった記憶の
    /// 「最後に読まれた時刻」を now に進める（使われているかを測るため・§01）——探索と印つけを
    /// 1 トランザクションで。語の一致は `instr`（部分一致・ワイルドカード解釈をしない）。
    pub fn recall(
        &self,
        subject: SubjectId,
        word: &str,
        limit: i64,
        now: i64,
    ) -> Result<Vec<MemoryRow>> {
        // 公開関数が自分の事前条件を持つ（表側で守る）。空語は instr が全 TRUE になり自主体の全件を
        // 返してしまう——「語で引く」の語が無いなら該当なし（近いものへ寄せない・全件へ倒さない）。
        // 負の limit は SQLite で「無制限」になる——非負へ丸める。core の clamp とは別の層の歯止め。
        if word.is_empty() {
            return Ok(vec![]);
        }
        let limit = limit.max(0);
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut rows: Vec<MemoryRow> = {
            let mut stmt = tx.prepare(
                "SELECT id,subject_id,body,origin_place,origin_from_seq,origin_to_seq,written_at,last_read_at
                 FROM memories WHERE subject_id=?1 AND instr(body, ?2)>0 ORDER BY id DESC LIMIT ?3",
            )?;
            let out = stmt
                .query_map(params![subject, word, limit], map_memory)?
                .collect::<Result<Vec<_>>>()?;
            out
        };
        for m in &mut rows {
            tx.execute(
                "UPDATE memories SET last_read_at=?2 WHERE id=?1",
                params![m.id, now],
            )?;
            // 返す行にも今の印を反映する（DB の値と食い違わせない——たった今 now に進めた）。
            m.last_read_at = Some(now);
        }
        tx.commit()?;
        Ok(rows)
    }

    /// 指して消す（忘れる）。**自分の記憶だけ**（subject_id で絞る）。実際に消えたら true、
    /// 無い／自分のでなければ 0 行で false——呼び手が失敗として返せる。
    pub fn forget(&self, subject: SubjectId, id: i64) -> Result<bool> {
        let n = self.c().execute(
            "DELETE FROM memories WHERE id=?1 AND subject_id=?2",
            params![id, subject],
        )?;
        Ok(n > 0)
    }

    /// 指して本文を差し替える（書き直す）。**由来は残す**（記憶とワーカー §03）——origin と
    /// written_at は触らず body だけ。自分の記憶だけ（subject_id で絞る）。書き換えたら true。
    pub fn rewrite(&self, subject: SubjectId, id: i64, body: &str) -> Result<bool> {
        let n = self.c().execute(
            "UPDATE memories SET body=?3 WHERE id=?1 AND subject_id=?2",
            params![id, subject, body],
        )?;
        Ok(n > 0)
    }

    /// その主体の記憶を新しい順に全部。予算での切り詰めは呼び手（core）が行う（§06 と対）。
    /// 索引を組む毎ターンの経路は `memories_newest_first_limited` を使う（全件フェッチを避ける）。
    /// これは件数の少ない検証・調査用（テスト等）に残す。
    pub fn memories_newest_first(&self, subject: SubjectId) -> Result<Vec<MemoryRow>> {
        self.memories_newest_first_limited(subject, i64::MAX)
    }

    /// その主体の記憶を新しい順に、最大 `limit` 件（記憶とワーカー §03）。文脈の索引は毎ターン組むので、
    /// **索引に載り得る件数だけ**を引いて全件フェッチを避ける（呼び手が LIMIT を渡す）。総件数は
    /// `memory_count` で別に取る——「省略 N 件」を正しく数えるため（切ったスライスの長さでは数えない）。
    pub fn memories_newest_first_limited(
        &self,
        subject: SubjectId,
        limit: i64,
    ) -> Result<Vec<MemoryRow>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT id,subject_id,body,origin_place,origin_from_seq,origin_to_seq,written_at,last_read_at
             FROM memories WHERE subject_id=?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![subject, limit], map_memory)?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// その主体の記憶の総件数（索引の「省略 N 件」を正しく数えるため・§06）。
    pub fn memory_count(&self, subject: SubjectId) -> Result<i64> {
        self.c().query_row(
            "SELECT COUNT(*) FROM memories WHERE subject_id=?1",
            params![subject],
            |r| r.get(0),
        )
    }

    // ---- 退避（常時切り離しの大きな結果・案 A）----
    //
    // 1 つの背景活動は store の原子的な決着で 1 度だけ確定するので、退避も活動ごとに 1 件
    //（activity_id が主鍵）。読みは **主体で絞る**（記憶と同じ主体分離）——他人の退避を指す
    // SQL が 0 行に落ち、呼び手はそれを「無い」として扱える。

    /// 決着の大きな結果を退避する（活動ごと 1 件）。二度目の挿入は主鍵衝突で失敗する——決着は
    /// 1 度きり（遷移ガード）なので通常は起きないが、起きたら握り潰さず上げる（§15）。
    pub fn create_offload(
        &self,
        activity_id: ActivityId,
        subject: SubjectId,
        place: PlaceId,
        body: &str,
        truncated: bool,
        now: i64,
    ) -> Result<()> {
        self.c().execute(
            "INSERT INTO offloads(activity_id,subject_id,place_id,body,truncated,created_at)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![activity_id, subject, place, body, truncated as i64, now],
        )?;
        Ok(())
    }

    /// 退避を引く。**自分の退避だけ**（subject_id で絞る）。無い／自分のでなければ None——
    /// 呼び手は「読むものが無い」として失敗を返せる（近いものへ寄せない・§15）。
    pub fn read_offload(
        &self,
        subject: SubjectId,
        activity_id: ActivityId,
    ) -> Result<Option<OffloadRow>> {
        self.c()
            .query_row(
                "SELECT activity_id,subject_id,place_id,body,truncated,created_at
                 FROM offloads WHERE activity_id=?1 AND subject_id=?2",
                params![activity_id, subject],
                map_offload,
            )
            .optional()
    }

    // ---- shell の権限（subject 単位・DESIGN-shell.md）----
    //
    // どちらの許可も「引いてから判定」ではなく **SQL の存在で判定**する（EXISTS）。読みが `Err` に
    // なれば呼び手（core）はそれを握り潰さず失敗として扱える（fail-closed・§15）——「引けなかった」を
    // 「許可されていない」と混同しない（`Ok(false)` は明確に「無い＝不許可」）。

    /// この主体に off-by-default な core builtin を許可する（subject_allowed_tools）。冪等
    /// （既にあれば何もしない）。provision / owner の設定が使う。
    pub fn allow_tool(&self, subject: SubjectId, tool: &str) -> Result<()> {
        self.c().execute(
            "INSERT OR IGNORE INTO subject_allowed_tools(subject_id,tool_name) VALUES(?1,?2)",
            params![subject, tool],
        )?;
        Ok(())
    }

    /// この主体がその core builtin を使えるか（subject_allowed_tools に載っているか）。
    pub fn subject_allows_tool(&self, subject: SubjectId, tool: &str) -> Result<bool> {
        self.c().query_row(
            "SELECT EXISTS(SELECT 1 FROM subject_allowed_tools WHERE subject_id=?1 AND tool_name=?2)",
            params![subject, tool],
            |r| r.get::<_, i64>(0).map(|n| n != 0),
        )
    }

    /// この主体に shell コマンド（argv[0]）を許可する（subject_allowed_commands）。冪等。
    /// core-allow-command（owner-only・OwnerFollowUp）の実体。
    pub fn allow_command(&self, subject: SubjectId, command: &str) -> Result<()> {
        self.c().execute(
            "INSERT OR IGNORE INTO subject_allowed_commands(subject_id,command) VALUES(?1,?2)",
            params![subject, command],
        )?;
        Ok(())
    }

    /// この主体がその argv[0] を shell で実行してよいか（完全一致・空 allowlist は全拒否）。
    pub fn subject_allows_command(&self, subject: SubjectId, command: &str) -> Result<bool> {
        self.c().query_row(
            "SELECT EXISTS(SELECT 1 FROM subject_allowed_commands WHERE subject_id=?1 AND command=?2)",
            params![subject, command],
            |r| r.get::<_, i64>(0).map(|n| n != 0),
        )
    }

    /// この主体の信頼リストに由来作者を加える（DESIGN-images §5）。冪等（既にあれば何もしない）。
    /// core-trust（owner-only・OwnerFollowUp）の実体。
    pub fn trust_author(&self, subject: SubjectId, author_external: &str) -> Result<()> {
        self.c().execute(
            "INSERT OR IGNORE INTO subject_trusted_authors(subject_id,author_external) VALUES(?1,?2)",
            params![subject, author_external],
        )?;
        Ok(())
    }

    /// この主体の信頼リストから由来作者を外す（DESIGN-images §5「追加・削除」）。core-untrust の実体。
    /// 消えた行数を返す（0 = そもそも信頼していなかった——呼び手が fail loud に伝える・§15）。
    pub fn untrust_author(&self, subject: SubjectId, author_external: &str) -> Result<usize> {
        self.c().execute(
            "DELETE FROM subject_trusted_authors WHERE subject_id=?1 AND author_external=?2",
            params![subject, author_external],
        )
    }

    /// この主体がその由来作者を信頼しているか（完全一致・空リストは全拒否）。DESIGN-images §5。
    /// **owner 判定はここに含めない**——owner は standing で常に通るので、呼び手（core）が別に見る。
    pub fn subject_trusts_author(&self, subject: SubjectId, author_external: &str) -> Result<bool> {
        self.c().query_row(
            "SELECT EXISTS(SELECT 1 FROM subject_trusted_authors WHERE subject_id=?1 AND author_external=?2)",
            params![subject, author_external],
            |r| r.get::<_, i64>(0).map(|n| n != 0),
        )
    }
}

fn map_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    // 未知の種別を、発火する Said に化かさない。読めない行は失敗として返す（詳細§15）。
    let kind_s: String = r.get(2)?;
    let kind = EventKind::from_wire(&kind_s).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            format!("unknown event kind: {kind_s}").into(),
        )
    })?;
    Ok(EventRow {
        place: r.get(0)?,
        seq: r.get(1)?,
        kind,
        author_subject: r.get(3)?,
        author_external: r.get(4)?,
        content: content_from_json(&r.get::<_, String>(5)?)?,
        mentions: mentions_from_json(&r.get::<_, String>(6)?)?,
        reply_to: r.get(7)?,
        target: r.get(8)?,
        for_subject: r.get(9)?,
        created_at: r.get(10)?,
        attachments: attachments_from_json(&r.get::<_, String>(11)?)?,
    })
}

fn map_memory(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRow> {
    Ok(MemoryRow {
        id: r.get(0)?,
        subject: r.get(1)?,
        body: r.get(2)?,
        origin_place: r.get(3)?,
        origin_from_seq: r.get(4)?,
        origin_to_seq: r.get(5)?,
        written_at: r.get(6)?,
        last_read_at: r.get(7)?,
    })
}

fn map_turn_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<TurnRecordRow> {
    Ok(TurnRecordRow {
        id: r.get(0)?,
        place: r.get(1)?,
        subject: r.get(2)?,
        activity: r.get(3)?,
        inherit_from_seq: r.get(4)?,
        inherit_to_seq: r.get(5)?,
        iterations: r.get(6)?,
        end_reason: r.get(7)?,
        failure_detail: r.get(8)?,
        withheld_text: r.get(9)?,
        tool_lines: r.get(10)?,
        fired_by: r.get(11)?,
    })
}

fn map_offload(r: &rusqlite::Row<'_>) -> rusqlite::Result<OffloadRow> {
    Ok(OffloadRow {
        activity_id: r.get(0)?,
        subject: r.get(1)?,
        place: r.get(2)?,
        body: r.get(3)?,
        truncated: r.get::<_, i64>(4)? != 0,
        created_at: r.get(5)?,
    })
}

fn map_activity(r: &rusqlite::Row<'_>) -> rusqlite::Result<ActivityRow> {
    let origin_from_exclusive: Option<Seq> = r.get(10)?;
    let origin_to_inclusive: Option<Seq> = r.get(11)?;
    let origin_standing: Option<String> = r.get(12)?;
    let tool_name: Option<String> = r.get(13)?;
    let tool_args_json: Option<String> = r.get(14)?;
    let provenance = match (
        origin_from_exclusive,
        origin_to_inclusive,
        origin_standing,
        tool_name,
        tool_args_json,
    ) {
        (None, None, None, None, None) => None,
        (Some(from), Some(to), Some(standing), Some(name), Some(args)) => {
            Some(BackgroundProvenance {
                origin_from_exclusive: from,
                origin_to_inclusive: to,
                origin_standing: standing_from(&standing)?,
                tool_name: name,
                tool_args: json_value_from(&args, "accepted tool args")?,
            })
        }
        _ => return Err(decode_err("activity provenance", "partial row")),
    };
    Ok(ActivityRow {
        id: r.get(0)?,
        place: r.get(1)?,
        subject: r.get(2)?,
        kind: atag_from(&r.get::<_, String>(3)?)?,
        label: r.get(4)?,
        deadline_at: r.get(5)?,
        started_at: r.get(6)?,
        ended_at: r.get(7)?,
        end_reason: r.get(8)?,
        detached_from: r.get(9)?,
        provenance,
    })
}

fn map_settled_provenance(r: &rusqlite::Row<'_>) -> rusqlite::Result<SettledProvenance> {
    let standing: String = r.get(3)?;
    let args: String = r.get(5)?;
    Ok(SettledProvenance {
        activity: r.get(0)?,
        origin_from_exclusive: r.get(1)?,
        origin_to_inclusive: r.get(2)?,
        origin_standing: standing_from(&standing)?,
        tool_name: r.get(4)?,
        tool_args: json_value_from(&args, "settled tool args")?,
    })
}

fn json_value_from(json: &str, what: &str) -> rusqlite::Result<serde_json::Value> {
    serde_json::from_str(json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("invalid {what}: {e}").into(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(text: &str) -> NewEvent {
        NewEvent {
            kind: EventKind::Said,
            author_subject: None,
            author_external: Some("u".into()),
            content: Content::text(text),
            mentions: vec![],
            reply_to: None,
            target: None,
            for_subject: None,
            attachments: vec![],
        }
    }

    // 冪等は**表の制約**で守る（詳細§04）: append_incoming を同じ origin で 2 度呼んでも、二重に
    // 採番せず、既にある連番へ畳む。呼ばれ方（直列かどうか）に依存しない。
    #[test]
    fn append_incoming_folds_duplicate_origin() {
        let s = Store::new_in_memory().unwrap();
        let p = s.create_place(Some("p"), None, "{}", None, 0).unwrap();
        let g = GateName::new("nostr");

        let first = s.append_incoming(p, &ev("A"), &g, "note1X", 1).unwrap();
        assert_eq!(first, Ingest::Appended(1));
        // 同じ origin をもう一度 → 畳む（同じ連番・二重に書かない）。
        let second = s
            .append_incoming(p, &ev("A again"), &g, "note1X", 2)
            .unwrap();
        assert_eq!(second, Ingest::Duplicate(1));
        assert_eq!(s.latest_seq(p).unwrap(), 1, "二重に書かない");

        // 別の origin は普通に積む。別の場は独立。
        assert_eq!(
            s.append_incoming(p, &ev("B"), &g, "note1Y", 3).unwrap(),
            Ingest::Appended(2)
        );
        let p2 = s.create_place(Some("p2"), None, "{}", None, 0).unwrap();
        assert_eq!(
            s.append_incoming(p2, &ev("A"), &g, "note1X", 4).unwrap(),
            Ingest::Appended(1),
            "場が違えば同じ origin でも別"
        );
    }

    // 表の UNIQUE(place, gate, external_id) が、同じ鍵で別の seq を指す ref を弾く（駆動経路非依存の土台）。
    #[test]
    fn external_ref_unique_key_is_enforced_by_the_table() {
        let s = Store::new_in_memory().unwrap();
        let p = s.create_place(Some("p"), None, "{}", None, 0).unwrap();
        let g = GateName::new("nostr");
        // seq=1 に note1X を張る。
        s.append(p, &ev("A"), 1).unwrap();
        s.record_external_ref(p, 1, &g, "note1X", "in").unwrap();
        assert_eq!(
            s.external_ref_direction(p, 1, &g).unwrap().as_deref(),
            Some("in"),
            "direction is observable without changing external_ref_of's tuple"
        );
        // 別の seq=2 に同じ note1X を張ろうとすると、表が弾く（黙って二重にならない）。
        s.append(p, &ev("B"), 2).unwrap();
        assert!(
            s.record_external_ref(p, 2, &g, "note1X", "out").is_err(),
            "同じ (場, ゲート, 識別子) を別の seq に張れない"
        );
    }

    // failure_detail / withheld_text / tool_lines 列の往復と、移行の冪等性。
    // 移行は列の有無を PRAGMA で見るので、二度呼んでも列は増えず落ちない（既存の流儀と揃える）。
    #[test]
    fn turn_record_details_roundtrip_and_migrate_is_idempotent() {
        let s = Store::new_in_memory().unwrap();
        {
            // new_in_memory で 1 度呼ばれている。もう一度呼んでも列は既にあり、no-op で落ちない。
            let c = s.c();
            assert!(column_exists(&c, "turn_records", "failure_detail").unwrap());
            assert!(column_exists(&c, "turn_records", "withheld_text").unwrap());
            assert!(column_exists(&c, "turn_records", "tool_lines").unwrap());
            migrate(&c).unwrap();
            assert!(column_exists(&c, "turn_records", "failure_detail").unwrap());
            assert!(column_exists(&c, "turn_records", "withheld_text").unwrap());
            assert!(column_exists(&c, "turn_records", "tool_lines").unwrap());
        }
        let p = s.create_place(Some("p"), None, "{}", None, 0).unwrap();
        let subj = s
            .create_subject(SubjectKind::Agent, "A", "A", "e", Standing::Trusted, 0)
            .unwrap();
        let act = s
            .start_activity(p, subj, ActivityKindTag::Turn, None, 0, 0, None)
            .unwrap();
        // withheld_text = Some（NO_REPLY で保留した地の文）。
        s.write_turn_record(&NewTurnRecord {
            place: p,
            subject: subj,
            activity: act,
            inherit_from_seq: None,
            inherit_to_seq: None,
            iterations: 1,
            started_at: 0,
            ended_at: 0,
            end_reason: "no_reply".into(),
            failure_detail: None,
            withheld_text: Some("保留した地の文".into()),
            tool_lines: Some("core-recall::猫".into()),
            fired_by: None,
        })
        .unwrap();
        // withheld_text = None（保留なし）・tool_lines = None（ツール行なし）。
        s.write_turn_record(&NewTurnRecord {
            place: p,
            subject: subj,
            activity: act,
            inherit_from_seq: None,
            inherit_to_seq: None,
            iterations: 1,
            started_at: 0,
            ended_at: 0,
            end_reason: "failed".into(),
            failure_detail: Some("sentinel failure".into()),
            withheld_text: None,
            tool_lines: None,
            fired_by: None,
        })
        .unwrap();

        let rows = s.turn_records(p).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].end_reason, "no_reply");
        assert_eq!(rows[0].withheld_text.as_deref(), Some("保留した地の文"));
        assert_eq!(rows[0].tool_lines.as_deref(), Some("core-recall::猫"));
        assert_eq!(rows[0].failure_detail, None);
        assert_eq!(rows[1].end_reason, "failed");
        assert_eq!(rows[1].failure_detail.as_deref(), Some("sentinel failure"));
        assert_eq!(rows[1].withheld_text, None);
        assert_eq!(rows[1].tool_lines, None);
    }

    // 退避は主体で絞る（記憶と同じ主体分離）: 他人の退避を指す read_offload は None に落ちる。
    // 自分のは本文・切り詰めフラグごと引ける。活動ごと 1 件（activity_id 主鍵）。
    #[test]
    fn offload_is_readable_only_by_owner() {
        let s = Store::new_in_memory().unwrap();
        let p = s.create_place(Some("p"), None, "{}", None, 0).unwrap();
        let a = s
            .create_subject(SubjectKind::Agent, "A", "A", "e", Standing::Trusted, 0)
            .unwrap();
        let b = s
            .create_subject(SubjectKind::Agent, "B", "B", "e", Standing::Trusted, 0)
            .unwrap();
        // 活動 7 の退避は a のもの。
        s.create_offload(7, a, p, "line1\nline2\nline3", false, 1)
            .unwrap();
        // 所有者は引ける（本文・切り詰めフラグごと）。
        let row = s.read_offload(a, 7).unwrap().expect("所有者は引ける");
        assert_eq!(row.body, "line1\nline2\nline3");
        assert!(!row.truncated);
        assert_eq!(row.place, p);
        // 他人（b）は同じ活動 id でも引けない（None）——主体で絞るので他人の退避は見えない。
        assert!(
            s.read_offload(b, 7).unwrap().is_none(),
            "他人の退避は見えない"
        );
        // 無い活動も None。
        assert!(s.read_offload(a, 999).unwrap().is_none());
        // 切り詰めフラグは往復する。
        s.create_offload(8, a, p, "head only\n", true, 2).unwrap();
        assert!(s.read_offload(a, 8).unwrap().unwrap().truncated);
    }

    // 確定（追記）と配送の pending 行を 1 tx で作る（詳細§08）。行が確定と同時に生まれるので、
    // 後で配送側が引けなくても「配送できなかった確定効果」が pending として見える。
    #[test]
    fn append_with_deliveries_creates_pending_rows_atomically() {
        let s = Store::new_in_memory().unwrap();
        let p = s.create_place(Some("p"), None, "{}", None, 0).unwrap();
        let channels = vec![
            (GateName::new("nostr"), "filter:x".to_string()),
            (GateName::new("web"), "web:room1".to_string()),
        ];
        let seq = s
            .append_with_deliveries(p, &ev("hi"), &channels, 1)
            .unwrap();
        assert_eq!(seq, 1);
        // イベントが載っている。
        assert_eq!(s.latest_seq(p).unwrap(), 1);
        // 運ぶチャネルの数だけ pending 行がある（確定と同じ tx で生まれた）。
        let mut rows = s.deliveries_for(p, seq).unwrap();
        rows.sort();
        assert_eq!(
            rows,
            vec![
                (GateName::new("nostr"), "pending".to_string()),
                (GateName::new("web"), "pending".to_string()),
            ]
        );
        // チャネルレスな場（発話はログへ書くだけ・§06）: 行は 0。
        let seq2 = s.append_with_deliveries(p, &ev("alone"), &[], 2).unwrap();
        assert_eq!(seq2, 2);
        assert!(s.deliveries_for(p, seq2).unwrap().is_empty());
    }

    #[test]
    fn background_settlement_roundtrips_typed_args_and_empty_range() {
        let s = Store::new_in_memory().unwrap();
        let place = s.create_place(Some("p"), None, "{}", None, 0).unwrap();
        let subject = s
            .create_subject(SubjectKind::Agent, "A", "A", "engine", Standing::Trusted, 0)
            .unwrap();
        let activity_provenance = BackgroundProvenance {
            origin_from_exclusive: 0,
            origin_to_inclusive: 0,
            origin_standing: Standing::Owner,
            tool_name: "synthetic-tool".into(),
            tool_args: serde_json::json!({
                "flag": true,
                "count": 2,
                "items": ["a", "b"]
            }),
        };
        let activity = s
            .start_activity_with_provenance(
                place,
                subject,
                ActivityKindTag::Background,
                Some("synthetic-tool"),
                10,
                1,
                None,
                Some(&activity_provenance),
            )
            .unwrap();
        let first = s
            .settle_background_activity(activity, "done", "synthetic result", None, 2, 3)
            .unwrap();
        let seq = match first {
            BackgroundSettlement::Appended { place: p, seq } => {
                assert_eq!(p, place);
                seq
            }
            other => panic!("first settlement must append: {other:?}"),
        };
        let settled_provenance = SettledProvenance {
            activity,
            origin_from_exclusive: activity_provenance.origin_from_exclusive,
            origin_to_inclusive: activity_provenance.origin_to_inclusive,
            origin_standing: activity_provenance.origin_standing,
            tool_name: activity_provenance.tool_name.clone(),
            tool_args: activity_provenance.tool_args.clone(),
        };
        assert_eq!(
            s.get_activity(activity).unwrap().unwrap().provenance,
            Some(activity_provenance)
        );
        assert_eq!(
            s.settled_provenance(place, seq).unwrap(),
            Some(settled_provenance)
        );
        assert_eq!(
            s.get_event(place, seq).unwrap().unwrap().reply_to,
            None,
            "empty origin range must not invent a reply target"
        );
    }

    #[test]
    fn interrupt_activity_atomically_closes_and_appends_once() {
        let s = Store::new_in_memory().unwrap();
        let place = s.create_place(Some("p"), None, "{}", None, 0).unwrap();
        let subject = s
            .create_subject(SubjectKind::Agent, "A", "A", "engine", Standing::Trusted, 0)
            .unwrap();
        let activity_provenance = BackgroundProvenance {
            origin_from_exclusive: 0,
            origin_to_inclusive: 0,
            origin_standing: Standing::Owner,
            tool_name: "synthetic-tool".into(),
            tool_args: serde_json::json!({"mode": "bounded"}),
        };
        let activity = s
            .start_activity_with_provenance(
                place,
                subject,
                ActivityKindTag::Background,
                None,
                10,
                1,
                None,
                Some(&activity_provenance),
            )
            .unwrap();

        let first = s
            .interrupt_activity(activity, "synthetic interruption", 2, 3)
            .unwrap();
        let seq = match first {
            ActivityInterruption::Appended { place: p, seq } => {
                assert_eq!(p, place);
                seq
            }
            other => panic!("first interruption must append: {other:?}"),
        };
        assert_eq!(
            s.get_activity(activity)
                .unwrap()
                .unwrap()
                .end_reason
                .as_deref(),
            Some("interrupted")
        );
        assert_eq!(
            s.settled_provenance(place, seq).unwrap().unwrap().activity,
            activity
        );

        assert_eq!(
            s.interrupt_activity(activity, "must not be appended", 4, 5)
                .unwrap(),
            ActivityInterruption::AlreadyRecorded { place, seq }
        );
        assert_eq!(s.latest_seq(place).unwrap(), seq);
    }

    #[test]
    fn new_background_without_provenance_is_not_treated_as_legacy() {
        let s = Store::new_in_memory().unwrap();
        let place = s.create_place(Some("p"), None, "{}", None, 0).unwrap();
        let subject = s
            .create_subject(SubjectKind::Agent, "A", "A", "engine", Standing::Trusted, 0)
            .unwrap();
        let activity = s
            .start_activity(
                place,
                subject,
                ActivityKindTag::Background,
                None,
                10,
                1,
                None,
            )
            .unwrap();

        let error = s
            .interrupt_activity(activity, "must fail loudly", 2, 3)
            .expect_err("unmarked new-format Background must require provenance");
        assert!(
            error
                .to_string()
                .contains("background activity has no accepted tool provenance"),
            "unexpected error: {error}"
        );
        assert_eq!(s.get_activity(activity).unwrap().unwrap().ended_at, None);
        assert_eq!(s.latest_seq(place).unwrap(), 0);
    }

    // name/persona 分離の移行（統括裁定）。旧 DB（name 列なし）に name を足し、既存 persona からコピーする。
    // 冪等: 2 回目は列が既にあり、既に非空の name は上書きしない。
    #[test]
    fn migration_adds_name_copies_persona_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        // 旧スキーマ（name / failure_detail / fired_by 列が無い）を模す。
        conn.execute_batch(
            "CREATE TABLE subjects(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL,
                persona TEXT NOT NULL,
                turn_runner TEXT NOT NULL,
                standing TEXT NOT NULL,
                created_at INTEGER NOT NULL
             );
             CREATE TABLE turn_records(id INTEGER PRIMARY KEY, withheld_text TEXT, tool_lines TEXT);
             -- 旧 events（attachments_json が無い）。migrate が列を足す（本番は SCHEMA が先に作る）。
             CREATE TABLE events(place_id INTEGER NOT NULL, seq INTEGER NOT NULL, PRIMARY KEY(place_id, seq));
             -- 旧 activities（provenance 列が無い）。
             CREATE TABLE activities(id INTEGER PRIMARY KEY, kind TEXT NOT NULL);
             INSERT INTO activities(id,kind) VALUES(1,'background');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO subjects(kind,persona,turn_runner,standing,created_at)
             VALUES('agent','エージェントA','engine','trusted',0)",
            [],
        )
        .unwrap();

        assert!(
            !column_exists(&conn, "subjects", "name").unwrap(),
            "移行前は name 列が無い"
        );

        // 1 回目: name 列を足し、既存 persona を name へコピーする。
        migrate(&conn).unwrap();
        assert!(
            column_exists(&conn, "turn_records", "failure_detail").unwrap(),
            "failure_detail 列が足される"
        );
        assert!(
            column_exists(&conn, "subjects", "name").unwrap(),
            "name 列が足される"
        );
        assert!(
            column_exists(&conn, "activities", "accepted_tool_args_json").unwrap(),
            "activity provenance 列が足される"
        );
        let legacy_backgrounds: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM legacy_background_activities WHERE activity_id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy_backgrounds, 1, "旧 Background の形式を保存する");
        let name: String = conn
            .query_row("SELECT name FROM subjects WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "エージェントA", "既存 persona を name へコピー");

        // name を書き換えてから 2 回目: 冪等——エラーも出ず、既に非空の name は上書きしない。
        conn.execute("UPDATE subjects SET name='別の表示名' WHERE id=1", [])
            .unwrap();
        migrate(&conn).unwrap();
        let name2: String = conn
            .query_row("SELECT name FROM subjects WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            name2, "別の表示名",
            "冪等: 2 回目は既存 name を上書きしない"
        );
    }

    #[test]
    fn migration_rejects_unknown_partial_provenance_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE turn_records(
               id INTEGER PRIMARY KEY,
               failure_detail TEXT,
               withheld_text TEXT,
               tool_lines TEXT,
               fired_by TEXT
             );
             CREATE TABLE activities(
               id INTEGER PRIMARY KEY,
               kind TEXT NOT NULL,
               origin_from_exclusive INTEGER
             );",
        )
        .unwrap();

        let error = migrate(&conn).expect_err("partial provenance schema is not a known format");
        assert!(
            error
                .to_string()
                .contains("unknown activities provenance schema: found 1 of 5 columns"),
            "unexpected error: {error}"
        );
        assert!(
            !column_exists(&conn, "activities", "accepted_tool_args_json").unwrap(),
            "unknown format must not be completed as though it were legacy"
        );
    }

    // 表示名（name）は人格本文（persona）と別列として往復する（同一性と人格の分離・統括裁定）。
    #[test]
    fn get_subject_returns_name_distinct_from_persona() {
        let s = Store::new_in_memory().unwrap();
        let id = s
            .create_subject(
                SubjectKind::Agent,
                "表示名",
                "人格本文はこちら（逐語で system に載る）",
                "engine",
                Standing::Trusted,
                0,
            )
            .unwrap();
        let row = s.get_subject(id).unwrap().unwrap();
        assert_eq!(row.name, "表示名", "name は表示名");
        assert_eq!(
            row.persona, "人格本文はこちら（逐語で system に載る）",
            "persona は人格本文"
        );
    }
}
