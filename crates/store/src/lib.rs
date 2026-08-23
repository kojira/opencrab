//! store — 永続化。SQLite ひとつ（詳細§01・§03）。実装は 1 つだけで、テストも同じ実装
//! （`:memory:`）を使う。時刻は core が nanos で渡す（store は時計を持たない）。
//!
//! 権威は全部ここにある（詳細§03）。プロセス内の状態は「実行中の future への取っ手」だけ。

use opencrab_port::*;
use rusqlite::{params, Connection, OptionalExtension};
use sha1::Sha1;
use sha2::{Digest, Sha256};
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateStatusRow {
    pub instance_id: GateInstanceId,
    pub kind_id: GateKindId,
    pub revision: u64,
    pub enabled: bool,
    pub lifecycle: String,
    pub connection_epoch: Option<u64>,
    pub connection_revision: Option<u64>,
    pub connection_state: Option<String>,
}

#[derive(Clone, Debug)]
pub struct GateLaunchSecret {
    pub name: String,
    pub value: Vec<u8>,
    pub at_rest_format: String,
}

#[derive(Clone, Debug)]
pub struct GateLaunchRow {
    pub instance_id: GateInstanceId,
    pub kind_id: GateKindId,
    pub revision: u64,
    pub config_schema_id: String,
    pub config_bytes: Vec<u8>,
    pub secrets: Vec<GateLaunchSecret>,
}

fn read_gate_status(conn: &Connection) -> Result<Vec<GateStatusRow>> {
    let mut stmt = conn.prepare(
        "SELECT gi.instance_id,gi.kind_id,gi.active_revision,r.enabled,gi.lifecycle,
                gc.connection_epoch,gc.revision,gc.state
         FROM gate_instances gi JOIN gate_instance_revisions r
           ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
         LEFT JOIN gate_connections gc ON gc.instance_id=gi.instance_id
           AND gc.connection_epoch=(SELECT MAX(x.connection_epoch) FROM gate_connections x WHERE x.instance_id=gi.instance_id)
         WHERE r.present=1 ORDER BY gi.instance_id",
    )?;
    let rows = stmt
        .query_map([], |row| {
            let instance: String = row.get(0)?;
            let kind: String = row.get(1)?;
            Ok(GateStatusRow {
                instance_id: GateInstanceId::parse(instance)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                kind_id: GateKindId::parse(kind).map_err(|_| rusqlite::Error::InvalidQuery)?,
                revision: row.get::<_, i64>(2)? as u64,
                enabled: row.get::<_, i64>(3)? != 0,
                lifecycle: row.get(4)?,
                connection_epoch: row.get::<_, Option<i64>>(5)?.map(|value| value as u64),
                connection_revision: row.get::<_, Option<i64>>(6)?.map(|value| value as u64),
                connection_state: row.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

/// Open an existing canonical database without schema creation or migration and read one
/// transactionally consistent gate-status snapshot.
pub fn gate_status_read_only(path: impl AsRef<std::path::Path>) -> Result<Vec<GateStatusRow>> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.execute_batch("BEGIN")?;
    let rows = read_gate_status(&conn);
    conn.execute_batch("ROLLBACK")?;
    rows
}

pub fn gate_launch_read_only(
    path: impl AsRef<std::path::Path>,
    instance: &GateInstanceId,
) -> Result<Option<GateLaunchRow>> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.execute_batch("BEGIN")?;
    let base = conn
        .query_row(
            "SELECT gi.kind_id,gi.active_revision,r.config_schema_id,r.config_bytes,r.secret_set_id
             FROM gate_instances gi JOIN gate_instance_revisions r
               ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
             WHERE gi.instance_id=?1 AND r.present=1 AND r.enabled=1",
            params![instance.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional();
    let result = match base {
        Ok(Some((kind, revision, schema, config, secret_set))) => {
            let mut secrets = Vec::new();
            if let Some(secret_set) = secret_set {
                let mut stmt = conn.prepare(
                    "SELECT name,value,at_rest_format FROM secret_values
                     WHERE secret_set_id=?1 ORDER BY name",
                )?;
                secrets = stmt
                    .query_map(params![secret_set], |row| {
                        Ok(GateLaunchSecret {
                            name: row.get(0)?,
                            value: row.get(1)?,
                            at_rest_format: row.get(2)?,
                        })
                    })?
                    .collect::<Result<Vec<_>>>()?;
            }
            Some(GateLaunchRow {
                instance_id: instance.clone(),
                kind_id: GateKindId::parse(kind).map_err(|_| rusqlite::Error::InvalidQuery)?,
                revision: revision as u64,
                config_schema_id: schema,
                config_bytes: config,
                secrets,
            })
        }
        Ok(None) => None,
        Err(error) => {
            conn.execute_batch("ROLLBACK")?;
            return Err(error);
        }
    };
    conn.execute_batch("ROLLBACK")?;
    Ok(result)
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

fn map_gate_route(row: &rusqlite::Row<'_>) -> Result<GateRoute> {
    let kind: String = row.get(2)?;
    let instance: String = row.get(3)?;
    let purpose: String = row.get(8)?;
    Ok(GateRoute {
        subject_id: row.get(0)?,
        place_id: row.get(1)?,
        kind_id: GateKindId::parse(kind).map_err(|_| rusqlite::Error::InvalidQuery)?,
        instance_id: GateInstanceId::parse(instance).map_err(|_| rusqlite::Error::InvalidQuery)?,
        binding_id: row.get(4)?,
        address: row.get(5)?,
        connection_epoch: row.get::<_, i64>(6)? as u64,
        revision: row.get::<_, i64>(7)? as u64,
        purpose: RoutePurpose::parse(purpose).map_err(|_| rusqlite::Error::InvalidQuery)?,
    })
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

            CREATE TABLE IF NOT EXISTS v1_channels(
              place_id INTEGER NOT NULL,
              gate_name TEXT NOT NULL,
              address TEXT NOT NULL,
              PRIMARY KEY(place_id, gate_name)
            );

            CREATE TABLE IF NOT EXISTS v1_external_refs(
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
            CREATE UNIQUE INDEX IF NOT EXISTS idx_v1_external_refs_unique
              ON v1_external_refs(place_id, gate_name, external_id);

            CREATE TABLE IF NOT EXISTS v1_deliveries(
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
            CREATE TABLE IF NOT EXISTS v1_dedup_hits(
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

            CREATE TABLE IF NOT EXISTS v1_subject_identities(
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
              detached_from INTEGER
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
            CREATE TABLE IF NOT EXISTS v1_expanded_tools(
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

            -- issue #742 canonical gate model. These tables are created even when the converter
            -- emits no rows; runtime read-set closure is a schema property, not converter output.
            CREATE TABLE IF NOT EXISTS gate_kinds(
              kind_id TEXT PRIMARY KEY,
              protocol_major INTEGER NOT NULL,
              origin_scope TEXT NOT NULL CHECK(origin_scope IN ('instance','kind_address')),
              ingress_discovery TEXT NOT NULL CHECK(ingress_discovery IN ('prebound','membership'))
            );
            CREATE TABLE IF NOT EXISTS gate_instances(
              instance_id TEXT PRIMARY KEY,
              kind_id TEXT NOT NULL,
              label TEXT NOT NULL,
              owner_subject_id INTEGER,
              active_revision INTEGER NOT NULL,
              lifecycle TEXT NOT NULL CHECK(lifecycle IN ('stopped','starting','running','stopping'))
            );
            CREATE TABLE IF NOT EXISTS gate_instance_revisions(
              instance_id TEXT NOT NULL,
              revision INTEGER NOT NULL,
              present INTEGER NOT NULL,
              enabled INTEGER NOT NULL,
              config_schema_id TEXT NOT NULL,
              config_bytes BLOB NOT NULL,
              config_digest BLOB NOT NULL,
              secret_set_id TEXT,
              created_at INTEGER NOT NULL,
              PRIMARY KEY(instance_id,revision)
            );
            CREATE TABLE IF NOT EXISTS gate_connections(
              instance_id TEXT NOT NULL,
              connection_epoch INTEGER NOT NULL,
              revision INTEGER NOT NULL,
              state TEXT NOT NULL CHECK(state IN ('connecting','active','closed','failed')),
              connected_at INTEGER,
              disconnected_at INTEGER,
              last_error TEXT,
              PRIMARY KEY(instance_id,connection_epoch)
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_gate_connections_one_active
              ON gate_connections(instance_id) WHERE state='active';
            CREATE TABLE IF NOT EXISTS external_origin_scopes(
              scope_id TEXT PRIMARY KEY,
              kind_id TEXT NOT NULL,
              address TEXT NOT NULL,
              mode TEXT NOT NULL CHECK(mode IN ('instance','kind_address')),
              instance_id TEXT,
              place_id INTEGER NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_origin_scope_instance
              ON external_origin_scopes(instance_id,address) WHERE mode='instance';
            CREATE UNIQUE INDEX IF NOT EXISTS idx_origin_scope_kind_address
              ON external_origin_scopes(kind_id,address) WHERE mode='kind_address';
            CREATE TABLE IF NOT EXISTS gate_bindings(
              binding_id TEXT PRIMARY KEY,
              place_id INTEGER NOT NULL,
              instance_id TEXT NOT NULL,
              address TEXT NOT NULL,
              label TEXT,
              origin_scope_id TEXT NOT NULL,
              binding_metadata_schema_id TEXT NOT NULL,
              binding_metadata_bytes BLOB NOT NULL,
              binding_metadata_digest BLOB NOT NULL,
              UNIQUE(instance_id,address)
            );
            CREATE TABLE IF NOT EXISTS subject_routes(
              subject_id INTEGER NOT NULL,
              place_id INTEGER NOT NULL,
              kind_id TEXT NOT NULL,
              purpose TEXT NOT NULL,
              binding_id TEXT NOT NULL,
              PRIMARY KEY(subject_id,place_id,kind_id,purpose)
            );
            CREATE TABLE IF NOT EXISTS gate_subject_identities(
              instance_id TEXT NOT NULL,
              external_id TEXT NOT NULL,
              subject_id INTEGER NOT NULL,
              display_name TEXT,
              PRIMARY KEY(instance_id,external_id)
            );
            CREATE TABLE IF NOT EXISTS external_refs(
              place_id INTEGER NOT NULL,
              seq INTEGER NOT NULL,
              instance_id TEXT NOT NULL,
              binding_id TEXT NOT NULL,
              external_id TEXT NOT NULL,
              direction TEXT NOT NULL CHECK(direction IN ('inbound','outbound')),
              UNIQUE(instance_id,binding_id,external_id),
              UNIQUE(place_id,seq,binding_id)
            );
            CREATE TABLE IF NOT EXISTS deliveries(
              delivery_id TEXT PRIMARY KEY,
              place_id INTEGER NOT NULL,
              seq INTEGER NOT NULL,
              binding_id TEXT NOT NULL,
              instance_id TEXT NOT NULL,
              revision INTEGER NOT NULL,
              connection_epoch INTEGER NOT NULL,
              state TEXT NOT NULL CHECK(state IN ('prepared','sending','delivered','failed','indeterminate')),
              attempt INTEGER NOT NULL,
              deadline INTEGER,
              error TEXT
            );
            CREATE TABLE IF NOT EXISTS delivery_observations(
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              delivery_id TEXT NOT NULL,
              connection_epoch INTEGER NOT NULL,
              kind TEXT NOT NULL,
              payload_digest BLOB NOT NULL,
              observed_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS gate_dedup_hits(
              hit_id TEXT PRIMARY KEY,
              kind_id TEXT NOT NULL,
              origin_scope_id TEXT NOT NULL,
              place_id INTEGER NOT NULL,
              external_id TEXT NOT NULL,
              existing_seq INTEGER NOT NULL,
              observed_at INTEGER NOT NULL,
              source_row_ordinal INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS expanded_gate_tools(
              place_id INTEGER NOT NULL,
              subject_id INTEGER NOT NULL,
              kind_id TEXT NOT NULL,
              PRIMARY KEY(place_id,subject_id,kind_id)
            );
            CREATE TABLE IF NOT EXISTS external_origins(
              origin_scope_id TEXT NOT NULL,
              external_id TEXT NOT NULL,
              place_id INTEGER NOT NULL,
              seq INTEGER NOT NULL,
              author_digest BLOB NOT NULL,
              content_digest BLOB NOT NULL,
              PRIMARY KEY(origin_scope_id,external_id)
            );
            CREATE TABLE IF NOT EXISTS agent_grants(
              subject_id INTEGER NOT NULL,
              grant_revision INTEGER NOT NULL,
              permission TEXT NOT NULL,
              PRIMARY KEY(subject_id,grant_revision)
            );
            CREATE TABLE IF NOT EXISTS grant_sets(
              subject_id INTEGER NOT NULL,
              revision INTEGER NOT NULL,
              created_at INTEGER NOT NULL,
              PRIMARY KEY(subject_id,revision)
            );
            CREATE TABLE IF NOT EXISTS grant_source_provenance(
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              principal_subject_id INTEGER,
              gate_kind TEXT,
              external_id TEXT,
              source_permission TEXT,
              source_allowed_actions TEXT,
              source_record_key TEXT,
              created_by TEXT,
              created_at INTEGER
            );
            CREATE TABLE IF NOT EXISTS place_source_refs(
              source_system TEXT NOT NULL,
              source_address TEXT NOT NULL,
              place_id INTEGER NOT NULL,
              source_id BLOB NOT NULL,
              classification TEXT NOT NULL,
              metadata TEXT,
              facilitator_subject_id INTEGER,
              participant_public_ids TEXT,
              theme TEXT,
              phase TEXT,
              mode TEXT,
              source_status TEXT,
              source_turn_number INTEGER,
              source_done_count INTEGER,
              source_max_turns INTEGER,
              source_record_digest BLOB,
              updated_at INTEGER NOT NULL,
              UNIQUE(source_system,source_address)
            );
            CREATE TABLE IF NOT EXISTS place_default_policies(
              default_id TEXT PRIMARY KEY,
              place_id INTEGER,
              kind_id TEXT NOT NULL,
              resolution TEXT NOT NULL CHECK(resolution IN ('active','ambiguous_place','invalid_runtime_fields','conflicting_default')),
              source_row BLOB,
              source_updated_at INTEGER
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_place_default_policy_active
              ON place_default_policies(place_id,kind_id) WHERE resolution='active';
            CREATE TABLE IF NOT EXISTS place_subject_policies(
              place_id INTEGER NOT NULL,
              kind_id TEXT NOT NULL,
              subject_id INTEGER NOT NULL,
              admission TEXT NOT NULL CHECK(admission IN ('closed','canary','open')),
              readable INTEGER NOT NULL,
              writable INTEGER NOT NULL,
              whitelisted INTEGER NOT NULL,
              heartbeat_enabled INTEGER NOT NULL,
              heartbeat_interval_secs INTEGER,
              heartbeat_instructions TEXT NOT NULL,
              instructions_revision INTEGER NOT NULL,
              source_row BLOB NOT NULL,
              source_updated_at INTEGER NOT NULL,
              PRIMARY KEY(place_id,kind_id,subject_id)
            );
            CREATE TABLE IF NOT EXISTS schedules(
              id INTEGER PRIMARY KEY,
              owner_subject_id INTEGER NOT NULL,
              place_id INTEGER NOT NULL,
              kind TEXT NOT NULL,
              expression TEXT NOT NULL,
              timezone TEXT NOT NULL,
              interval_secs INTEGER,
              anchor_at INTEGER,
              enabled INTEGER NOT NULL,
              instruction TEXT NOT NULL,
              instruction_revision INTEGER NOT NULL,
              next_fire INTEGER,
              last_fired_at INTEGER,
              source_record_key INTEGER,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tool_policy_sets(
              subject_id INTEGER NOT NULL,
              revision INTEGER NOT NULL,
              created_at INTEGER NOT NULL,
              PRIMARY KEY(subject_id,revision)
            );
            CREATE TABLE IF NOT EXISTS tool_policy_entries(
              subject_id INTEGER NOT NULL,
              policy_revision INTEGER NOT NULL,
              tool_id TEXT NOT NULL,
              visibility TEXT NOT NULL,
              allowed INTEGER NOT NULL,
              PRIMARY KEY(subject_id,policy_revision,tool_id)
            );
            CREATE TABLE IF NOT EXISTS gate_operations(
              operation_id TEXT PRIMARY KEY,
              subject_id INTEGER,
              kind TEXT NOT NULL,
              binding_id TEXT,
              instance_id TEXT NOT NULL,
              connection_epoch INTEGER,
              state TEXT NOT NULL,
              attempt INTEGER NOT NULL,
              deadline INTEGER,
              error TEXT,
              public_result TEXT
            );
            CREATE TABLE IF NOT EXISTS interactions(
              id INTEGER PRIMARY KEY,
              owner_subject_id INTEGER NOT NULL,
              place_id INTEGER NOT NULL,
              binding_id TEXT,
              source_address TEXT NOT NULL,
              source_message_id TEXT,
              surface TEXT NOT NULL,
              surface_id TEXT NOT NULL,
              surface_payload TEXT NOT NULL,
              payload TEXT NOT NULL,
              owner_only INTEGER NOT NULL,
              timeout_secs INTEGER NOT NULL,
              deadline INTEGER NOT NULL,
              state TEXT NOT NULL,
              source_record_key TEXT,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS interaction_responses(
              interaction_id INTEGER PRIMARY KEY,
              interaction_source_key TEXT,
              responder_kind TEXT NOT NULL,
              responder_subject_id INTEGER,
              responder_external_id TEXT NOT NULL,
              response TEXT NOT NULL,
              responded_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS secret_sets(
              secret_set_id TEXT PRIMARY KEY,
              revision INTEGER NOT NULL,
              scope TEXT NOT NULL,
              created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS secret_values(
              secret_set_id TEXT NOT NULL,
              name TEXT NOT NULL,
              value BLOB NOT NULL,
              at_rest_format TEXT NOT NULL CHECK(at_rest_format IN ('source-plaintext','enc:v1','opaque')),
              value_digest BLOB NOT NULL,
              PRIMARY KEY(secret_set_id,name)
            );
            CREATE TABLE IF NOT EXISTS nostr_generated_keys(
              owner_subject_id INTEGER NOT NULL,
              npub TEXT NOT NULL,
              secret_set_id TEXT NOT NULL,
              state TEXT NOT NULL CHECK(state IN ('generated','adopted')),
              source_record BLOB,
              created_at INTEGER NOT NULL,
              adopted_at INTEGER
            );
            CREATE TABLE IF NOT EXISTS webhook_endpoints(
              id INTEGER PRIMARY KEY,
              owner_subject_id INTEGER NOT NULL,
              kind TEXT NOT NULL,
              scope TEXT NOT NULL,
              tool_name TEXT NOT NULL,
              endpoint TEXT NOT NULL,
              name TEXT,
              event_filter BLOB,
              output_mode TEXT NOT NULL,
              maximum_output_chars INTEGER NOT NULL,
              enabled INTEGER NOT NULL,
              created_by TEXT,
              updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS legacy_unowned_source_rows(
              source_db TEXT NOT NULL,
              source_table TEXT NOT NULL,
              source_key BLOB NOT NULL,
              row_values BLOB NOT NULL,
              reason TEXT NOT NULL CHECK(reason <> ''),
              PRIMARY KEY(source_db,source_table,source_key)
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
    migrate_v1_gate_tables(conn)?;
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        params![table],
        |row| row.get(0),
    )
}

/// Move the six worktree-v1 tables out of the canonical names before creating the v15 schema.
fn prepare_v1_gate_tables(conn: &Connection) -> Result<()> {
    for (old, marker) in [
        ("channels", "gate_name"),
        ("external_refs", "gate_name"),
        ("deliveries", "gate_name"),
        ("subject_identities", "gate_name"),
        ("dedup_hits", "gate_name"),
        ("expanded_tools", "gate_name"),
    ] {
        let legacy = format!("v1_{old}");
        if table_exists(conn, old)?
            && column_exists(conn, old, marker)?
            && !table_exists(conn, &legacy)?
        {
            conn.execute(&format!("ALTER TABLE {old} RENAME TO {legacy}"), [])?;
        }
    }
    Ok(())
}

const COMPAT_NAMESPACE: [u8; 16] = [
    0x3d, 0xbf, 0x7a, 0x0d, 0xa8, 0xcf, 0x5e, 0x5c, 0xa2, 0x3d, 0x4e, 0x8d, 0x0c, 0x60, 0x74, 0x21,
];

fn uuid_v5(locator: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(COMPAT_NAMESPACE);
    hasher.update(locator.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

pub fn compatibility_instance_id(kind: &GateKindId) -> GateInstanceId {
    let locator = format!("compat:{}", kind.as_str());
    GateInstanceId::from_canonical(uuid_v5(&locator))
}

fn deterministic_id(locator: &str) -> String {
    uuid_v5(locator)
}

fn runtime_uuid_v7(now_nanos: i64, locator: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let millis = (now_nanos.max(0) as u64 / 1_000_000) & 0x0000_ffff_ffff_ffff;
    let digest = Sha256::digest(format!("{locator}\0{sequence}").as_bytes());
    let mut bytes = [0_u8; 16];
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6] = 0x70 | ((sequence >> 8) as u8 & 0x0f);
    bytes[7] = sequence as u8;
    bytes[8..].copy_from_slice(&digest[..8]);
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

fn seed_compatibility_instance_on(conn: &Connection, kind: &GateKindId) -> Result<GateInstanceId> {
    let id = compatibility_instance_id(kind);
    conn.execute(
        "INSERT INTO gate_kinds(kind_id,protocol_major,origin_scope,ingress_discovery)
         VALUES(?1,1,'instance','prebound')
         ON CONFLICT(kind_id) DO NOTHING",
        params![kind.as_str()],
    )?;
    conn.execute(
        "INSERT INTO gate_instance_revisions(instance_id,revision,present,enabled,config_schema_id,config_bytes,config_digest,secret_set_id,created_at)
         VALUES(?1,1,1,1,'compat/v1',?2,?3,NULL,0)
         ON CONFLICT(instance_id,revision) DO NOTHING",
        params![id.as_str(), Vec::<u8>::new(), sha256(&[])],
    )?;
    conn.execute(
        "INSERT INTO gate_instances(instance_id,kind_id,label,owner_subject_id,active_revision,lifecycle)
         VALUES(?1,?2,?2,NULL,1,'stopped')
         ON CONFLICT(instance_id) DO NOTHING",
        params![id.as_str(), kind.as_str()],
    )?;
    Ok(id)
}

fn ensure_compat_binding(
    conn: &Connection,
    place: PlaceId,
    kind: &GateKindId,
    address: &str,
) -> Result<(GateInstanceId, String)> {
    let instance = seed_compatibility_instance_on(conn, kind)?;
    let scope_id = deterministic_id(&format!("scope\0{}\0{address}", instance.as_str()));
    let binding_id = deterministic_id(&format!("binding\0{}\0{address}", instance.as_str()));
    conn.execute(
        "INSERT INTO external_origin_scopes(scope_id,kind_id,address,mode,instance_id,place_id)
         VALUES(?1,?2,?3,'instance',?4,?5)
         ON CONFLICT(instance_id,address) WHERE mode='instance' DO UPDATE SET place_id=?5",
        params![scope_id, kind.as_str(), address, instance.as_str(), place],
    )?;
    let metadata = Vec::<u8>::new();
    conn.execute(
        "INSERT INTO gate_bindings(binding_id,place_id,instance_id,address,label,origin_scope_id,binding_metadata_schema_id,binding_metadata_bytes,binding_metadata_digest)
         VALUES(?1,?2,?3,?4,NULL,?5,'compat/v1',?6,?7)
         ON CONFLICT(instance_id,address) DO UPDATE SET place_id=?2,origin_scope_id=?5",
        params![binding_id, place, instance.as_str(), address, scope_id, metadata, sha256(&[])],
    )?;
    conn.execute(
        "INSERT INTO subject_routes(subject_id,place_id,kind_id,purpose,binding_id)
         SELECT subject_id,?1,?2,purpose,?3 FROM memberships
         CROSS JOIN (SELECT 'inbound' AS purpose UNION ALL SELECT 'outbound')
         WHERE place_id=?1 AND role='participant' AND left_at IS NULL
         ON CONFLICT(subject_id,place_id,kind_id,purpose) DO UPDATE SET binding_id=?3",
        params![place, kind.as_str(), binding_id],
    )?;
    Ok((instance, binding_id))
}

fn encode_sqlite_values(values: &[rusqlite::types::Value]) -> Vec<u8> {
    use rusqlite::types::Value;
    let mut out = Vec::new();
    out.extend_from_slice(&(values.len() as u64).to_be_bytes());
    for value in values {
        match value {
            Value::Null => out.push(0),
            Value::Integer(value) => {
                out.push(1);
                out.extend_from_slice(&value.to_be_bytes());
            }
            Value::Real(value) => {
                out.push(2);
                out.extend_from_slice(&value.to_bits().to_be_bytes());
            }
            Value::Text(value) => {
                out.push(3);
                out.extend_from_slice(&(value.len() as u64).to_be_bytes());
                out.extend_from_slice(value.as_bytes());
            }
            Value::Blob(value) => {
                out.push(4);
                out.extend_from_slice(&(value.len() as u64).to_be_bytes());
                out.extend_from_slice(value);
            }
        }
    }
    out
}

fn raw_v1_row(
    conn: &Connection,
    table: &str,
    rowid: i64,
    values: &[rusqlite::types::Value],
    reason: &str,
) -> Result<()> {
    let key = encode_sqlite_values(&[rusqlite::types::Value::Integer(rowid)]);
    let row_values = encode_sqlite_values(values);
    conn.execute(
        "INSERT INTO legacy_unowned_source_rows(source_db,source_table,source_key,row_values,reason)
         VALUES('worktree-v1',?1,?2,?3,?4)
         ON CONFLICT(source_db,source_table,source_key) DO UPDATE SET row_values=?3,reason=?4",
        params![table,key,row_values,reason],
    )?;
    Ok(())
}

fn raw_all_v1_rows(conn: &Connection, table: &str, reason: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("SELECT rowid,* FROM {table} ORDER BY rowid"))?;
    let rows = stmt
        .query_map([], |row| {
            let rowid = row.get::<_, i64>(0)?;
            let mut values = Vec::with_capacity(row.as_ref().column_count() - 1);
            for index in 1..row.as_ref().column_count() {
                values.push(row.get::<_, rusqlite::types::Value>(index)?);
            }
            Ok((rowid, values))
        })?
        .collect::<Result<Vec<_>>>()?;
    for (rowid, values) in rows {
        raw_v1_row(conn, table, rowid, &values, reason)?;
    }
    Ok(())
}

fn migrate_v1_gate_tables(conn: &Connection) -> Result<()> {
    if !table_exists(conn, "v1_channels")? {
        return Ok(());
    }
    let mut names = std::collections::BTreeSet::new();
    for table in [
        "v1_channels",
        "v1_external_refs",
        "v1_deliveries",
        "v1_subject_identities",
        "v1_dedup_hits",
        "v1_expanded_tools",
    ] {
        if !table_exists(conn, table)? {
            continue;
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT gate_name FROM {table} WHERE gate_name <> ''"
        ))?;
        for name in stmt.query_map([], |row| row.get::<_, String>(0))? {
            names.insert(name?);
        }
    }
    for name in names {
        let kind = GateKindId::parse(name).map_err(|_| rusqlite::Error::InvalidQuery)?;
        seed_compatibility_instance_on(conn, &kind)?;
    }

    let mut stmt = conn.prepare(
        "SELECT place_id,gate_name,address FROM v1_channels ORDER BY place_id,gate_name",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>>>()?;
    for (place, name, address) in rows {
        let kind = GateKindId::parse(name).map_err(|_| rusqlite::Error::InvalidQuery)?;
        ensure_compat_binding(conn, place, &kind, &address)?;
    }

    conn.execute_batch(
        "INSERT OR IGNORE INTO gate_subject_identities(instance_id,external_id,subject_id,display_name)
         SELECT gi.instance_id,v.external_id,v.subject_id,NULL
         FROM v1_subject_identities v JOIN gate_instances gi ON gi.kind_id=v.gate_name;

         INSERT OR IGNORE INTO expanded_gate_tools(place_id,subject_id,kind_id)
         SELECT place_id,subject_id,gate_name FROM v1_expanded_tools;"
    )?;

    let mut stmt = conn.prepare(
        "SELECT r.place_id,r.seq,r.gate_name,r.external_id,r.direction,b.instance_id,b.binding_id
         FROM v1_external_refs r JOIN gate_bindings b ON b.place_id=r.place_id
         JOIN gate_instances gi ON gi.instance_id=b.instance_id AND gi.kind_id=r.gate_name",
    )?;
    let refs = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>>>()?;
    for (place, seq, _kind, external, direction, instance, binding) in refs {
        let direction = match direction.as_str() {
            "in" => "inbound",
            "out" => "outbound",
            _ => continue,
        };
        conn.execute(
            "INSERT OR IGNORE INTO external_refs(place_id,seq,instance_id,binding_id,external_id,direction) VALUES(?1,?2,?3,?4,?5,?6)",
            params![place,seq,instance,binding,external,direction],
        )?;
    }

    // v1 deliveries lack both revision and connection_epoch. They are preserved exactly in the
    // unified raw carrier and are never weakened into the strict runtime delivery state machine.
    raw_all_v1_rows(conn, "v1_deliveries", "unresolved_parent")?;

    // Dedup observations are immutable and preserve duplicates by source row ordinal.
    let mut stmt = conn.prepare(
        "SELECT d.rowid,d.gate_name,d.place_id,d.external_id,d.existing_seq,d.at,
                gb.origin_scope_id
         FROM v1_dedup_hits d
         LEFT JOIN gate_instances gi ON gi.kind_id=d.gate_name
         LEFT JOIN gate_bindings gb ON gb.instance_id=gi.instance_id AND gb.place_id=d.place_id
         ORDER BY d.rowid",
    )?;
    let hits = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>>>()?;
    for (rowid, kind, place, external, existing, at, scope) in hits {
        if let Some(scope) = scope {
            let hit_id = deterministic_id(&format!("v1-dedup\0{rowid}"));
            conn.execute(
                "INSERT OR IGNORE INTO gate_dedup_hits(hit_id,kind_id,origin_scope_id,place_id,external_id,existing_seq,observed_at,source_row_ordinal)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![hit_id,kind,scope,place,external,existing,at,rowid],
            )?;
        } else {
            raw_v1_row(
                conn,
                "v1_dedup_hits",
                rowid,
                &[
                    rusqlite::types::Value::Text(kind),
                    rusqlite::types::Value::Integer(place),
                    rusqlite::types::Value::Text(external),
                    rusqlite::types::Value::Integer(existing),
                    rusqlite::types::Value::Integer(at),
                ],
                "unresolved_parent",
            )?;
        }
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

impl Store {
    pub fn new_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        prepare_v1_gate_tables(&conn)?;
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
        prepare_v1_gate_tables(&conn)?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    // ---- canonical gate kind / instance / connection / route (#742) ----

    pub fn seed_compatibility_instance(&self, kind: &GateKindId) -> Result<GateInstanceId> {
        seed_compatibility_instance_on(&self.c(), kind)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn install_gate_instance_revision(
        &self,
        instance: &GateInstanceId,
        kind: &GateKindId,
        label: &str,
        owner_subject: Option<SubjectId>,
        revision: u64,
        enabled: bool,
        origin_scope: OriginScope,
        ingress_discovery: IngressDiscovery,
        config_schema_id: &str,
        config_bytes: &[u8],
        now: i64,
    ) -> Result<()> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO gate_kinds(kind_id,protocol_major,origin_scope,ingress_discovery)
             VALUES(?1,2,?2,?3)
             ON CONFLICT(kind_id) DO UPDATE SET protocol_major=2,origin_scope=?2,ingress_discovery=?3",
            params![kind.as_str(),origin_scope.as_wire(),ingress_discovery.as_wire()],
        )?;
        tx.execute(
            "INSERT INTO gate_instances(instance_id,kind_id,label,owner_subject_id,active_revision,lifecycle)
             VALUES(?1,?2,?3,?4,?5,'stopped')
             ON CONFLICT(instance_id) DO UPDATE SET kind_id=?2,label=?3,owner_subject_id=?4,active_revision=?5",
            params![instance.as_str(),kind.as_str(),label,owner_subject,revision as i64],
        )?;
        tx.execute(
            "INSERT INTO gate_instance_revisions(instance_id,revision,present,enabled,config_schema_id,config_bytes,config_digest,secret_set_id,created_at)
             VALUES(?1,?2,1,?3,?4,?5,?6,NULL,?7)
             ON CONFLICT(instance_id,revision) DO NOTHING",
            params![instance.as_str(),revision as i64,enabled as i64,config_schema_id,config_bytes,sha256(config_bytes),now],
        )?;
        let stored: (i64, i64, String, Vec<u8>) = tx.query_row(
            "SELECT present,enabled,config_schema_id,config_bytes FROM gate_instance_revisions
             WHERE instance_id=?1 AND revision=?2",
            params![instance.as_str(), revision as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        if stored
            != (
                1,
                enabled as i64,
                config_schema_id.to_string(),
                config_bytes.to_vec(),
            )
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
        tx.commit()
    }

    /// Exact protocol-1 compatibility lookup. This query never creates rows.
    pub fn compatibility_instance(&self, kind: &GateKindId) -> Result<Option<GateInstanceId>> {
        let derived = compatibility_instance_id(kind);
        let value: Option<String> = self.c().query_row(
            "SELECT instance_id FROM gate_instances WHERE instance_id=?1 AND kind_id=?2 AND active_revision=1",
            params![derived.as_str(), kind.as_str()],
            |row| row.get(0),
        ).optional()?;
        value
            .map(|value| GateInstanceId::parse(value).map_err(|_| rusqlite::Error::InvalidQuery))
            .transpose()
    }

    pub fn begin_gate_connection(
        &self,
        instance: &GateInstanceId,
        revision: u64,
        now: i64,
    ) -> Result<u64> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let active_revision: i64 = tx.query_row(
            "SELECT active_revision FROM gate_instances WHERE instance_id=?1",
            params![instance.as_str()],
            |row| row.get(0),
        )?;
        if active_revision != revision as i64 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let epoch: i64 = tx.query_row(
            "SELECT COALESCE(MAX(connection_epoch),0)+1 FROM gate_connections WHERE instance_id=?1",
            params![instance.as_str()],
            |row| row.get(0),
        )?;
        tx.execute(
            "UPDATE gate_connections SET state='closed',disconnected_at=?2 WHERE instance_id=?1 AND state='active'",
            params![instance.as_str(), now],
        )?;
        tx.execute(
            "INSERT INTO gate_connections(instance_id,connection_epoch,revision,state,connected_at,disconnected_at,last_error)
             VALUES(?1,?2,?3,'connecting',?4,NULL,NULL)",
            params![instance.as_str(), epoch, revision as i64, now],
        )?;
        tx.execute(
            "UPDATE gate_instances SET lifecycle='starting' WHERE instance_id=?1",
            params![instance.as_str()],
        )?;
        tx.commit()?;
        Ok(epoch as u64)
    }

    pub fn gate_instance_revision_matches(
        &self,
        instance: &GateInstanceId,
        revision: u64,
    ) -> Result<Option<bool>> {
        self.c()
            .query_row(
                "SELECT active_revision=?2 FROM gate_instances WHERE instance_id=?1",
                params![instance.as_str(), revision as i64],
                |row| row.get::<_, bool>(0),
            )
            .optional()
    }

    pub fn activate_gate_connection(
        &self,
        instance: &GateInstanceId,
        epoch: u64,
        now: i64,
    ) -> Result<()> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE gate_connections SET state='active',connected_at=?3,last_error=NULL
             WHERE instance_id=?1 AND connection_epoch=?2 AND state='connecting'",
            params![instance.as_str(), epoch as i64, now],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        tx.execute(
            "UPDATE gate_instances SET lifecycle='running' WHERE instance_id=?1",
            params![instance.as_str()],
        )?;
        tx.commit()
    }

    pub fn close_gate_connection(
        &self,
        instance: &GateInstanceId,
        epoch: u64,
        failed_code: Option<&str>,
        now: i64,
    ) -> Result<()> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state = if failed_code.is_some() {
            "failed"
        } else {
            "closed"
        };
        tx.execute(
            "UPDATE gate_connections SET state=?3,disconnected_at=?4,last_error=?5
             WHERE instance_id=?1 AND connection_epoch=?2 AND state IN ('connecting','active')",
            params![instance.as_str(), epoch as i64, state, now, failed_code],
        )?;
        tx.execute(
            "UPDATE gate_instances SET lifecycle='stopped' WHERE instance_id=?1",
            params![instance.as_str()],
        )?;
        tx.commit()
    }

    pub fn ensure_compatibility_binding(
        &self,
        place: PlaceId,
        kind: &GateKindId,
        address: &str,
    ) -> Result<(GateInstanceId, String)> {
        ensure_compat_binding(&self.c(), place, kind, address)
    }

    pub fn set_subject_route(
        &self,
        subject: SubjectId,
        place: PlaceId,
        kind: &GateKindId,
        purpose: &RoutePurpose,
        binding_id: &str,
    ) -> Result<()> {
        let conn = self.c();
        conn.execute(
            "INSERT INTO subject_routes(subject_id,place_id,kind_id,purpose,binding_id) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(subject_id,place_id,kind_id,purpose) DO UPDATE SET binding_id=?5",
            params![subject,place,kind.as_str(),purpose.as_str(),binding_id],
        )?;
        Ok(())
    }

    pub fn ensure_tool_routes_for_kind(
        &self,
        kind: &GateKindId,
        tool_names: &[String],
    ) -> Result<()> {
        let conn = self.c();
        for tool in tool_names {
            let purpose = RoutePurpose::tool(tool).map_err(|_| rusqlite::Error::InvalidQuery)?;
            conn.execute(
                "INSERT INTO subject_routes(subject_id,place_id,kind_id,purpose,binding_id)
                 SELECT m.subject_id,m.place_id,?1,?2,gb.binding_id
                 FROM memberships m JOIN gate_bindings gb ON gb.place_id=m.place_id
                 JOIN gate_instances gi ON gi.instance_id=gb.instance_id AND gi.kind_id=?1
                 WHERE m.role='participant' AND m.left_at IS NULL
                 ON CONFLICT(subject_id,place_id,kind_id,purpose) DO UPDATE SET binding_id=excluded.binding_id",
                params![kind.as_str(),purpose.as_str()],
            )?;
        }
        Ok(())
    }

    pub fn gate_route(
        &self,
        subject: SubjectId,
        place: PlaceId,
        kind: &GateKindId,
        purpose: &RoutePurpose,
    ) -> Result<Option<GateRoute>> {
        self.c().query_row(
            "SELECT sr.subject_id,sr.place_id,sr.kind_id,gi.instance_id,gb.binding_id,gb.address,
                    gc.connection_epoch,gi.active_revision,sr.purpose
             FROM subject_routes sr JOIN gate_bindings gb ON gb.binding_id=sr.binding_id
             JOIN gate_instances gi ON gi.instance_id=gb.instance_id
             JOIN gate_connections gc ON gc.instance_id=gi.instance_id AND gc.revision=gi.active_revision AND gc.state='active'
             WHERE sr.subject_id=?1 AND sr.place_id=?2 AND sr.kind_id=?3 AND sr.purpose=?4",
            params![subject,place,kind.as_str(),purpose.as_str()],
            map_gate_route,
        ).optional()
    }

    pub fn gate_routes_for_place(
        &self,
        subject: SubjectId,
        place: PlaceId,
        purpose: &RoutePurpose,
    ) -> Result<Vec<GateRoute>> {
        let conn = self.c();
        let mut stmt = conn.prepare(
            "SELECT sr.subject_id,sr.place_id,sr.kind_id,gi.instance_id,gb.binding_id,gb.address,
                    gc.connection_epoch,gi.active_revision,sr.purpose
             FROM subject_routes sr JOIN gate_bindings gb ON gb.binding_id=sr.binding_id
             JOIN gate_instances gi ON gi.instance_id=gb.instance_id
             JOIN gate_connections gc ON gc.instance_id=gi.instance_id AND gc.revision=gi.active_revision AND gc.state='active'
             WHERE sr.subject_id=?1 AND sr.place_id=?2 AND sr.purpose=?3 ORDER BY sr.kind_id,gi.instance_id"
        )?;
        let rows = stmt
            .query_map(params![subject, place, purpose.as_str()], map_gate_route)?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Resolve an inbound address inside one concrete instance. The result is exact because
    /// `(instance_id,address)` is unique; callers never fall back to another instance of the kind.
    pub fn inbound_binding(
        &self,
        instance: &GateInstanceId,
        address: &str,
    ) -> Result<Option<(PlaceId, String)>> {
        self.c()
            .query_row(
                "SELECT place_id,binding_id FROM gate_bindings WHERE instance_id=?1 AND address=?2",
                params![instance.as_str(), address],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
    }

    pub fn resolve_subject_on_instance(
        &self,
        instance: &GateInstanceId,
        external: &str,
    ) -> Result<Option<SubjectId>> {
        self.c()
            .query_row(
                "SELECT subject_id FROM gate_subject_identities WHERE instance_id=?1 AND external_id=?2",
                params![instance.as_str(), external],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn resolve_external_on_binding(
        &self,
        binding_id: &str,
        external: &str,
    ) -> Result<Option<Seq>> {
        self.c()
            .query_row(
                "SELECT seq FROM external_refs WHERE binding_id=?1 AND external_id=?2",
                params![binding_id, external],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn record_dedup_on_binding(
        &self,
        kind: &GateKindId,
        binding_id: &str,
        place: PlaceId,
        external: &str,
        existing_seq: Seq,
        at: i64,
    ) -> Result<()> {
        let conn = self.c();
        let scope: String = conn.query_row(
            "SELECT origin_scope_id FROM gate_bindings WHERE binding_id=?1",
            params![binding_id],
            |row| row.get(0),
        )?;
        let ordinal: i64 = conn.query_row(
            "SELECT COALESCE(MAX(source_row_ordinal),-1)+1 FROM gate_dedup_hits WHERE origin_scope_id=?1",
            params![scope],
            |row| row.get(0),
        )?;
        let hit_id = runtime_uuid_v7(at, &format!("dedup\0{binding_id}\0{external}\0{ordinal}"));
        conn.execute(
            "INSERT INTO gate_dedup_hits(hit_id,kind_id,origin_scope_id,place_id,external_id,existing_seq,observed_at,source_row_ordinal)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![hit_id,kind.as_str(),scope,place,external,existing_seq,at,ordinal],
        )?;
        // Keep the protocol-1 query facade coherent while it remains supported.
        conn.execute(
            "INSERT INTO v1_dedup_hits(gate_name,place_id,external_id,existing_seq,at)
             VALUES(?1,?2,?3,?4,?5)",
            params![kind.as_str(), place, external, existing_seq, at],
        )?;
        Ok(())
    }

    pub fn gate_status(&self) -> Result<Vec<GateStatusRow>> {
        let conn = self.c();
        read_gate_status(&conn)
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
        let conn = self.c();
        conn.execute(
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
                "SELECT seq FROM v1_external_refs WHERE place_id=?1 AND gate_name=?2 AND external_id=?3",
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
            "INSERT INTO v1_external_refs(place_id,seq,gate_name,external_id,direction) VALUES(?1,?2,?3,?4,'in')",
            params![place, seq, gate.as_str(), origin],
        )?;
        tx.commit()?;
        Ok(Ingest::Appended(seq))
    }

    /// Canonical instance/binding-scoped ingress. Deduplication, event append, and external-ref
    /// creation share one immediate transaction, so concurrent duplicate delivery returns the
    /// already committed sequence and never allocates a second event.
    pub fn append_incoming_on_binding(
        &self,
        place: PlaceId,
        ev: &NewEvent,
        instance: &GateInstanceId,
        binding_id: &str,
        origin: &str,
        now: i64,
    ) -> Result<Ingest> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<Seq> = tx
            .query_row(
                "SELECT seq FROM external_refs WHERE instance_id=?1 AND binding_id=?2 AND external_id=?3",
                params![instance.as_str(),binding_id,origin],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(seq) = existing {
            return Ok(Ingest::Duplicate(seq));
        }
        let seq: Seq = tx.query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM events WHERE place_id=?1",
            params![place],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO events(place_id,seq,kind,author_subject_id,author_external_id,content_json,mentions_json,reply_to_seq,target_seq,for_subject_id,created_at,attachments_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![place,seq,ev.kind.as_str(),ev.author_subject,ev.author_external,content_to_json(&ev.content),mentions_to_json(&ev.mentions),ev.reply_to,ev.target,ev.for_subject,now,attachments_to_json(&ev.attachments)],
        )?;
        tx.execute(
            "INSERT INTO external_refs(place_id,seq,instance_id,binding_id,external_id,direction)
             VALUES(?1,?2,?3,?4,?5,'inbound')",
            params![place, seq, instance.as_str(), binding_id, origin],
        )?;
        let kind: String = tx.query_row(
            "SELECT kind_id FROM gate_instances WHERE instance_id=?1",
            params![instance.as_str()],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO v1_external_refs(place_id,seq,gate_name,external_id,direction)
             VALUES(?1,?2,?3,?4,'in')
             ON CONFLICT(place_id,seq,gate_name) DO UPDATE SET external_id=?4,direction='in'",
            params![place, seq, kind, origin],
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
                "INSERT INTO v1_deliveries(place_id,seq,gate_name,state,error,attempted_at)
                 VALUES(?1,?2,?3,'pending',NULL,?4)",
                params![place, seq, gate.as_str(), now],
            )?;
        }
        tx.commit()?;
        Ok(seq)
    }

    pub fn append_with_delivery_routes(
        &self,
        place: PlaceId,
        ev: &NewEvent,
        routes: &[GateRoute],
        now: i64,
    ) -> Result<Seq> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let seq: Seq = tx.query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM events WHERE place_id=?1",
            params![place],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO events(place_id,seq,kind,author_subject_id,author_external_id,content_json,mentions_json,reply_to_seq,target_seq,for_subject_id,created_at,attachments_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![place,seq,ev.kind.as_str(),ev.author_subject,ev.author_external,content_to_json(&ev.content),mentions_to_json(&ev.mentions),ev.reply_to,ev.target,ev.for_subject,now,attachments_to_json(&ev.attachments)],
        )?;
        for route in routes {
            let delivery_id = runtime_uuid_v7(
                now,
                &format!("delivery\0{place}\0{seq}\0{}", route.binding_id),
            );
            tx.execute(
                "INSERT INTO deliveries(delivery_id,place_id,seq,binding_id,instance_id,revision,connection_epoch,state,attempt,deadline,error)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,'prepared',0,NULL,NULL)",
                params![delivery_id,place,seq,route.binding_id,route.instance_id.as_str(),route.revision as i64,route.connection_epoch as i64],
            )?;
            tx.execute(
                "INSERT INTO v1_deliveries(place_id,seq,gate_name,state,error,attempted_at)
                 VALUES(?1,?2,?3,'pending',NULL,?4)",
                params![place, seq, route.kind_id.as_str(), now],
            )?;
        }
        tx.commit()?;
        Ok(seq)
    }

    pub fn begin_delivery(&self, place: PlaceId, seq: Seq, binding_id: &str) -> Result<bool> {
        Ok(self.c().execute(
            "UPDATE deliveries SET state='sending',attempt=attempt+1
             WHERE place_id=?1 AND seq=?2 AND binding_id=?3 AND state='prepared'",
            params![place, seq, binding_id],
        )? == 1)
    }

    pub fn finish_delivery(
        &self,
        place: PlaceId,
        seq: Seq,
        binding_id: &str,
        state: &str,
        error: Option<&str>,
    ) -> Result<bool> {
        if !matches!(state, "delivered" | "failed" | "indeterminate") {
            return Err(rusqlite::Error::InvalidQuery);
        }
        Ok(self.c().execute(
            "UPDATE deliveries SET state=?4,error=?5
             WHERE place_id=?1 AND seq=?2 AND binding_id=?3 AND state='sending'",
            params![place, seq, binding_id, state, error],
        )? == 1)
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
        let conn = self.c();
        conn.execute(
            "INSERT INTO v1_subject_identities(subject_id,gate_name,external_id) VALUES(?1,?2,?3)",
            params![subject, gate.as_str(), external],
        )?;
        let instance = seed_compatibility_instance_on(&conn, gate)?;
        conn.execute(
            "INSERT INTO gate_subject_identities(instance_id,external_id,subject_id,display_name)
             VALUES(?1,?2,?3,NULL)
             ON CONFLICT(instance_id,external_id) DO UPDATE SET subject_id=?3",
            params![instance.as_str(), external, subject],
        )?;
        Ok(())
    }

    /// 名寄せ。見つからなければ None（=主体は付かない・権限ゼロ）。
    pub fn resolve_subject(&self, gate: &GateName, external: &str) -> Result<Option<SubjectId>> {
        self.c()
            .query_row(
                "SELECT subject_id FROM v1_subject_identities WHERE gate_name=?1 AND external_id=?2",
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
                "SELECT external_id FROM v1_subject_identities WHERE subject_id=?1 AND gate_name=?2",
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
            "SELECT si.external_id FROM v1_subject_identities si
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
        let conn = self.c();
        conn.execute(
            "INSERT INTO memberships(place_id,subject_id,role,read_seq,joined_at)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(place_id,subject_id) DO UPDATE SET role=?3, left_at=NULL",
            params![place, subject, role_str(role), read_seq, now],
        )?;
        if role == Role::Participant {
            conn.execute(
                "INSERT INTO subject_routes(subject_id,place_id,kind_id,purpose,binding_id)
                 SELECT ?2,?1,gi.kind_id,purpose,gb.binding_id
                 FROM gate_bindings gb JOIN gate_instances gi ON gi.instance_id=gb.instance_id
                 CROSS JOIN (SELECT 'inbound' AS purpose UNION ALL SELECT 'outbound')
                 WHERE gb.place_id=?1
                 ON CONFLICT(subject_id,place_id,kind_id,purpose) DO UPDATE SET binding_id=excluded.binding_id",
                params![place, subject],
            )?;
        }
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
        self.c().execute(
            "UPDATE memberships SET read_seq=?3 WHERE place_id=?1 AND subject_id=?2",
            params![place, subject, read_seq],
        )?;
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
        let c = self.c();
        c.execute(
            "INSERT INTO activities(place_id,subject_id,kind,label,deadline_at,started_at,detached_from)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![place, subject, atag_str(kind), label, deadline_at, started_at, detached_from],
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
                "SELECT id,place_id,subject_id,kind,label,deadline_at,started_at,ended_at,end_reason,detached_from
                 FROM activities WHERE id=?1",
                params![id],
                map_activity,
            )
            .optional()
    }

    pub fn all_activities(&self) -> Result<Vec<ActivityRow>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT id,place_id,subject_id,kind,label,deadline_at,started_at,ended_at,end_reason,detached_from
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
            "SELECT id,place_id,subject_id,kind,label,deadline_at,started_at,ended_at,end_reason,detached_from
             FROM activities WHERE ended_at IS NULL ORDER BY id",
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
        let conn = self.c();
        conn.execute(
            "INSERT INTO v1_channels(place_id,gate_name,address) VALUES(?1,?2,?3)
             ON CONFLICT(place_id,gate_name) DO UPDATE SET address=?3",
            params![place, gate.as_str(), address],
        )?;
        ensure_compat_binding(&conn, place, gate, address)?;
        Ok(())
    }

    pub fn remove_channel(&self, place: PlaceId, gate: &GateName) -> Result<()> {
        let conn = self.c();
        conn.execute(
            "DELETE FROM v1_channels WHERE place_id=?1 AND gate_name=?2",
            params![place, gate.as_str()],
        )?;
        conn.execute(
            "DELETE FROM subject_routes WHERE place_id=?1 AND kind_id=?2",
            params![place, gate.as_str()],
        )?;
        conn.execute(
            "DELETE FROM gate_bindings WHERE place_id=?1 AND instance_id IN (SELECT instance_id FROM gate_instances WHERE kind_id=?2)",
            params![place, gate.as_str()],
        )?;
        Ok(())
    }

    /// 場に結ばれた全チャネル（gate, address）。効果の配送先と可能な効果の和に使う（詳細§02/§08）。
    pub fn channels_for_place(&self, place: PlaceId) -> Result<Vec<(GateName, String)>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT gate_name,address FROM v1_channels WHERE place_id=?1 ORDER BY gate_name",
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
            "SELECT place_id,address FROM v1_channels WHERE gate_name=?1 ORDER BY place_id",
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
                "SELECT place_id FROM v1_channels WHERE gate_name=?1 AND address=?2",
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
        let conn = self.c();
        conn.execute(
            "INSERT INTO v1_external_refs(place_id,seq,gate_name,external_id,direction) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(place_id,seq,gate_name) DO UPDATE SET external_id=?4, direction=?5",
            params![place, seq, gate.as_str(), external_id, direction],
        )?;
        if let Some((instance, binding)) = conn.query_row(
            "SELECT gi.instance_id,gb.binding_id FROM gate_bindings gb JOIN gate_instances gi ON gi.instance_id=gb.instance_id
             WHERE gb.place_id=?1 AND gi.kind_id=?2 ORDER BY gi.instance_id LIMIT 1",
            params![place, gate.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ).optional()? {
            let direction = match direction { "in" => "inbound", "out" => "outbound", _ => return Err(rusqlite::Error::InvalidQuery) };
            conn.execute(
                "INSERT INTO external_refs(place_id,seq,instance_id,binding_id,external_id,direction) VALUES(?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(place_id,seq,binding_id) DO UPDATE SET external_id=?5,direction=?6",
                params![place,seq,instance,binding,external_id,direction],
            )?;
        }
        Ok(())
    }

    pub fn record_external_ref_on_route(
        &self,
        route: &GateRoute,
        seq: Seq,
        external_id: &str,
        direction: &str,
    ) -> Result<()> {
        let direction = match direction {
            "in" => "inbound",
            "out" => "outbound",
            _ => return Err(rusqlite::Error::InvalidQuery),
        };
        let conn = self.c();
        conn.execute(
            "INSERT INTO external_refs(place_id,seq,instance_id,binding_id,external_id,direction)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(place_id,seq,binding_id) DO UPDATE SET external_id=?5,direction=?6",
            params![
                route.place_id,
                seq,
                route.instance_id.as_str(),
                route.binding_id,
                external_id,
                direction
            ],
        )?;
        if route.instance_id == compatibility_instance_id(&route.kind_id) {
            let legacy_direction = if direction == "inbound" { "in" } else { "out" };
            conn.execute(
                "INSERT INTO v1_external_refs(place_id,seq,gate_name,external_id,direction)
                 VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(place_id,seq,gate_name) DO UPDATE SET external_id=?4,direction=?5",
                params![
                    route.place_id,
                    seq,
                    route.kind_id.as_str(),
                    external_id,
                    legacy_direction
                ],
            )?;
        }
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
                "SELECT seq FROM v1_external_refs WHERE place_id=?1 AND gate_name=?2 AND external_id=?3",
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
            "INSERT INTO v1_dedup_hits(gate_name,place_id,external_id,existing_seq,at) VALUES(?1,?2,?3,?4,?5)",
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
        let conn = self.c();
        conn.execute(
            "INSERT OR IGNORE INTO v1_expanded_tools(place_id,subject_id,gate_name) VALUES(?1,?2,?3)",
            params![place, subject, gate.as_str()],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO expanded_gate_tools(place_id,subject_id,kind_id) VALUES(?1,?2,?3)",
            params![place, subject, gate.as_str()],
        )?;
        Ok(())
    }

    /// この参加（場×主体）で展開済みのゲート名。広告（advertised_tools）が本体を出す判断に使う。
    pub fn expanded_gates(&self, place: PlaceId, subject: SubjectId) -> Result<Vec<GateName>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT gate_name FROM v1_expanded_tools WHERE place_id=?1 AND subject_id=?2 ORDER BY gate_name",
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
            "SELECT COUNT(*) FROM v1_dedup_hits WHERE gate_name=?1",
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
                "SELECT external_id FROM v1_external_refs WHERE place_id=?1 AND seq=?2 AND gate_name=?3",
                params![place, seq, gate.as_str()],
                |r| r.get(0),
            )
            .optional()
    }

    /// seq → その出来事が届いた／出たチャネルと外界識別子（効果の宛先の解決・詳細§08）。
    pub fn external_ref_of(&self, place: PlaceId, seq: Seq) -> Result<Option<(GateName, String)>> {
        self.c()
            .query_row(
                "SELECT gate_name,external_id FROM v1_external_refs WHERE place_id=?1 AND seq=?2",
                params![place, seq],
                |r| Ok((GateName::new(r.get::<_, String>(0)?), r.get(1)?)),
            )
            .optional()
    }

    /// その場に外界識別子つきの出来事が 1 つでもあるか（宛先にできる出来事の有無・詳細§08）。
    /// 「宛先にできる出来事だけを提示する」判定に使う。
    pub fn place_has_external_refs(&self, place: PlaceId) -> Result<bool> {
        let n: i64 = self.c().query_row(
            "SELECT COUNT(*) FROM v1_external_refs WHERE place_id=?1",
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
            "INSERT INTO v1_deliveries(place_id,seq,gate_name,state,error,attempted_at) VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(place_id,seq,gate_name) DO UPDATE SET state=?4, error=?5, attempted_at=?6",
            params![place, seq, gate.as_str(), state, error, now],
        )?;
        Ok(())
    }

    pub fn deliveries_for(&self, place: PlaceId, seq: Seq) -> Result<Vec<(GateName, String)>> {
        let c = self.c();
        let mut stmt = c.prepare(
            "SELECT gate_name,state FROM v1_deliveries WHERE place_id=?1 AND seq=?2 ORDER BY gate_name",
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
    // 1 つの背景活動は 1 度だけ決着する（supervise_background の遷移ガード）ので、退避も活動ごとに
    // 1 件（activity_id が主鍵）。読みは **主体で絞る**（記憶と同じ主体分離）——他人の退避を指す
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

    #[test]
    fn compatibility_instance_is_deterministic_and_lookup_never_creates() {
        let store = Store::new_in_memory().unwrap();
        let kind = GateKindId::parse("web".to_string()).unwrap();
        assert!(store.compatibility_instance(&kind).unwrap().is_none());
        let first = store.seed_compatibility_instance(&kind).unwrap();
        let second = store.seed_compatibility_instance(&kind).unwrap();
        assert_eq!(first, second);
        assert_eq!(store.compatibility_instance(&kind).unwrap(), Some(first));
    }

    #[test]
    fn canonical_runtime_read_set_tables_exist_without_converter_rows() {
        let store = Store::new_in_memory().unwrap();
        let conn = store.c();
        for table in [
            "agent_grants",
            "deliveries",
            "delivery_observations",
            "expanded_gate_tools",
            "external_origin_scopes",
            "external_origins",
            "external_refs",
            "gate_bindings",
            "gate_connections",
            "gate_dedup_hits",
            "gate_instance_revisions",
            "gate_instances",
            "gate_kinds",
            "gate_operations",
            "gate_subject_identities",
            "grant_sets",
            "grant_source_provenance",
            "interactions",
            "interaction_responses",
            "nostr_generated_keys",
            "place_default_policies",
            "place_source_refs",
            "place_subject_policies",
            "schedules",
            "secret_sets",
            "secret_values",
            "subject_routes",
            "tool_policy_entries",
            "tool_policy_sets",
            "webhook_endpoints",
        ] {
            assert!(table_exists(&conn, table).unwrap(), "missing table {table}");
        }
    }

    #[test]
    fn worktree_v1_six_tables_migrate_or_retain_raw() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE channels(place_id INTEGER NOT NULL,gate_name TEXT NOT NULL,address TEXT NOT NULL,PRIMARY KEY(place_id,gate_name));
             CREATE TABLE external_refs(place_id INTEGER NOT NULL,seq INTEGER NOT NULL,gate_name TEXT NOT NULL,external_id TEXT NOT NULL,direction TEXT NOT NULL,PRIMARY KEY(place_id,seq,gate_name));
             CREATE TABLE deliveries(place_id INTEGER NOT NULL,seq INTEGER NOT NULL,gate_name TEXT NOT NULL,state TEXT NOT NULL,error TEXT,attempted_at INTEGER NOT NULL,PRIMARY KEY(place_id,seq,gate_name));
             CREATE TABLE subject_identities(subject_id INTEGER NOT NULL,gate_name TEXT NOT NULL,external_id TEXT NOT NULL,PRIMARY KEY(subject_id,gate_name));
             CREATE TABLE dedup_hits(gate_name TEXT NOT NULL,place_id INTEGER NOT NULL,external_id TEXT NOT NULL,existing_seq INTEGER NOT NULL,at INTEGER NOT NULL);
             CREATE TABLE expanded_tools(place_id INTEGER NOT NULL,subject_id INTEGER NOT NULL,gate_name TEXT NOT NULL,PRIMARY KEY(place_id,subject_id,gate_name));
             INSERT INTO channels VALUES(1,'web','room:a');
             INSERT INTO external_refs VALUES(1,1,'web','message-a','in');
             INSERT INTO deliveries VALUES(1,1,'web','sent',NULL,10);
             INSERT INTO subject_identities VALUES(1,'web','principal-a');
             INSERT INTO dedup_hits VALUES('web',1,'message-a',1,11);
             INSERT INTO expanded_tools VALUES(1,1,'web');",
        )
        .unwrap();
        prepare_v1_gate_tables(&conn).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO places(id,address,policy_json,created_at) VALUES(1,'room:a','{}',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO subjects(id,kind,name,persona,turn_runner,standing,created_at)
             VALUES(1,'agent','A','A','engine','trusted',0)",
            [],
        )
        .unwrap();
        migrate(&conn).unwrap();

        for table in [
            "v1_channels",
            "v1_external_refs",
            "v1_deliveries",
            "v1_subject_identities",
            "v1_dedup_hits",
            "v1_expanded_tools",
        ] {
            assert!(table_exists(&conn, table).unwrap());
        }
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM gate_bindings", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM gate_dedup_hits", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM legacy_unowned_source_rows WHERE source_table='v1_deliveries'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM deliveries", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
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
             CREATE TABLE events(place_id INTEGER NOT NULL, seq INTEGER NOT NULL, PRIMARY KEY(place_id, seq));",
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
