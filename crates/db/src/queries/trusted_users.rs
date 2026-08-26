use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// TRUSTED CO-AGENTS
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedCoAgentRow {
    pub id: String,
    pub agent_id: String,
    pub co_agent_id: String,
    pub allowed_actions: Option<String>,
    pub created_by: String,
    pub created_at: String,
}

pub fn list_trusted_co_agents(conn: &Connection, agent_id: &str) -> Result<Vec<TrustedCoAgentRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, co_agent_id, allowed_actions, created_by, created_at
         FROM trusted_co_agents WHERE agent_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(params![agent_id], |row| {
        Ok(TrustedCoAgentRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            co_agent_id: row.get(2)?,
            allowed_actions: row.get(3)?,
            created_by: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// `agent_id` にとって `co_agent_id` が信頼済み co-agent として登録されているか。
///
/// 呼び出し元の権限解決（[`crate::queries`] を使う `caller_identity` / Discord の
/// `resolve_caller`）が、`trusted_users` の照合に加えてこの表も引くために使う（#485）。
/// この表は co-agents API（`POST /api/agents/{id}/co-agents`）が書き込むが、以前は
/// list API しか読まず**権限解決に配線されていなかった**（登録しても相手は Agent の
/// まま = owner 等価に届かない）バグの修正。`allowed_actions` はここでは見ない
/// （登録があれば co_agent = owner 等価。#485 は widening のみで別ゲートを足さない）。
pub fn is_trusted_co_agent(conn: &Connection, agent_id: &str, co_agent_id: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM trusted_co_agents WHERE agent_id = ?1 AND co_agent_id = ?2",
        params![agent_id, co_agent_id],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

pub fn insert_trusted_co_agent(conn: &Connection, row: &TrustedCoAgentRow) -> Result<()> {
    conn.execute(
        "INSERT INTO trusted_co_agents (id, agent_id, co_agent_id, allowed_actions, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(agent_id, co_agent_id) DO UPDATE SET
            allowed_actions = excluded.allowed_actions,
            created_by = excluded.created_by",
        params![
            row.id,
            row.agent_id,
            row.co_agent_id,
            row.allowed_actions,
            row.created_by,
            row.created_at,
        ],
    )?;
    Ok(())
}

// `update_trusted_co_agent_actions` は #490 で撤去した。唯一の呼び出し元だった
// co-agents API の PATCH（`allowed_actions` 更新）が無くなり、この列は権限判定に
// 使われないため書き換える経路も要らなくなった（列自体は互換のため残す）。

pub fn delete_trusted_co_agent(
    conn: &Connection,
    agent_id: &str,
    co_agent_id: &str,
) -> Result<bool> {
    let deleted = conn.execute(
        "DELETE FROM trusted_co_agents WHERE agent_id = ?1 AND co_agent_id = ?2",
        params![agent_id, co_agent_id],
    )?;
    Ok(deleted > 0)
}

// ============================================
// TrustedUser
// ============================================

// ---- 経路（identity platform, #214） ----
//
// 信頼済みユーザーの表は元々「Discord のユーザー識別子」ひとつの平坦な空間だった。
// web と当時の direct-message REST は自分の側のユーザー識別子で同じ表を引いていたため、
// 識別子が一致すると信頼が経路をまたいで引き継がれていた。`platform` 列を足し、認可の
// 読み出しは `(platform, user_id, agent_id)` で引く。
//
// 表名 `trusted_discord_users` / 列名 `discord_user_id` は #159 で `trusted_users` /
// `user_id` に改名した（マイグレーション v17。Discord は `platform` の値のひとつでしかない）。
//
// 互換読み（自経路の行が無ければ従来の `discord` 経路も見る暫定のフォールバック）は
// #159 で撤去した。登録 API が経路を受け取れるようになった（`POST /api/agents/{id}/
// trusted-users` の `platform`）ため、web と当時の direct-message REST のユーザーは自経路の
// 行として登録する。**経路ごとの行を持たない既存ユーザーはこの時点で信頼を失う**（緩む
// 方向ではなく機能が止まる方向）。移行手順は `docs/api.md` の Trusted Users を参照。
//
// **#159 に残っている作業**:
// - 一意制約 `(user_id, agent_id)` → `(platform, user_id, agent_id)`（表の再構築＝非可逆）

/// 列追加前から存在する行が属する経路。マイグレーションの `DEFAULT` と一致させる。
pub const TRUSTED_PLATFORM_DISCORD: &str = "discord";
/// ダッシュボード（web ゲートウェイ）が申告するユーザー識別子の経路。
pub const TRUSTED_PLATFORM_WEB: &str = "web";
/// 撤去済みの direct-message REST が使用していた識別子の経路（既存行の互換用）。
pub const TRUSTED_PLATFORM_REST: &str = "rest";
/// Nostr 受信イベントの著者 pubkey の経路（#319）。
///
/// 識別子は **64 桁小文字 hex**（Nostr 受信イベントの `pubkey` の表現）で登録する。
/// npub で登録された行も引けるよう、読み出し側（`crates/server` の Nostr の呼び出し元
/// 解決）は hex と npub の両方の表現で引く。
pub const TRUSTED_PLATFORM_NOSTR: &str = "nostr";
/// external gate 受信の author 識別子の経路（V3 §7.2）。
pub const TRUSTED_PLATFORM_EXTGATE: &str = "extgate";

/// 登録 API が受け付ける識別子経路の全体。撤去済み REST の互換値も含む。
///
/// `rest` 以外の未知の経路の行は**どの読み出しとも一致しない**（登録しても誰も
/// 信頼されない）。
/// 綴り間違いが「登録できたのに効かない」行として黙って残るのを防ぐため、
/// 登録 API はこの集合で弾く（fail-closed 側の検証であって、認可の判定ではない）。
pub const TRUSTED_PLATFORMS: [&str; 5] = [
    TRUSTED_PLATFORM_DISCORD,
    TRUSTED_PLATFORM_WEB,
    TRUSTED_PLATFORM_REST,
    TRUSTED_PLATFORM_NOSTR,
    TRUSTED_PLATFORM_EXTGATE,
];

/// 登録 API で受け付ける経路（legacy `rest` を含む）として定義済みか。
pub fn is_known_trusted_platform(platform: &str) -> bool {
    TRUSTED_PLATFORMS.contains(&platform)
}

// ---- 権限（列挙型, #234） ----
//
// 信頼済みユーザーの権限はかつて素の文字列で、登録 API は受け取った値を検証も
// 正規化もせずそのまま保存していた。判定側は `permission == "co_agent"` の完全一致、
// ダッシュボードの選択肢は `co-agent`。**表記が 1 文字違うだけで登録は黙って無効**に
// なり（行はあるので「信頼済みユーザー」までは落ちるが協働エージェントにはならない）、
// UI にもログにも何も出なかった（#234）。
//
// 表記は**ケバブケースに統一**する。設定ファイルのコマンド権限
// （`opencrab_actions::tools::CommandPermission`）は既に列挙型でケバブケースを
// 指定しており、不正値はパースエラーで弾かれる＝型で守られている。守られている側の
// 規約に寄せる。
//
// **`CommandPermission` を再利用しない理由**: 値の集合も意味も違う。
// `CommandPermission` は「そのコマンドを実行するのに必要な最低の呼び出し元権限」
// （`owner` / `agent` / `co-agent`）で、`agent`＝誰でも可を含む。こちらは
// 「登録した相手に与える権限」（`owner` / `user` / `co-agent`）で、`agent`＝
// 未登録の最小権限は**そもそも登録の対象にならない**（行が無い状態がそれ）。
// 型を共有すると、片方にしか意味の無い variant が他方の入口を通ってしまう。

/// 信頼済みユーザーに与える権限（`trusted_users.permission`）。
///
/// **表記ゆれは型で起こりえない**: DB へ書く文字列は [`Self::as_db_str`] だけが作り、
/// 入口の検証は [`Self::parse`] だけが通す。serde 表現も同じケバブケース。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TrustedUserPermission {
    /// オーナー相当。
    Owner,
    /// ただの信頼済みユーザー（登録の既定）。
    #[default]
    User,
    /// 協働エージェント（相互レビューの名簿に載る）。
    CoAgent,
}

/// 権限の全体。ダッシュボードの選択肢はここから導く（一致は
/// `dashboard_permission_options_match_the_enum` が検査する）。
pub const TRUSTED_USER_PERMISSIONS: [TrustedUserPermission; 3] = [
    TrustedUserPermission::Owner,
    TrustedUserPermission::User,
    TrustedUserPermission::CoAgent,
];

impl TrustedUserPermission {
    /// DB に入る表記（ケバブケース）。**書き込みはこの関数だけを通す。**
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::User => "user",
            Self::CoAgent => "co-agent",
        }
    }

    /// 既知の表記だけを受け付ける（入口の検証用）。**別表記の受け入れはしない** —
    /// 「どちらの表記も通す」が #234 そのものだったので、通す表記はひとつに保つ。
    pub fn parse(s: &str) -> Option<Self> {
        TRUSTED_USER_PERMISSIONS
            .into_iter()
            .find(|p| p.as_db_str() == s)
    }

    /// DB の既存値を読む。**未知の値は [`Self::User`] へ倒す**（fail-closed）。
    ///
    /// 行があるのに権限が読めない状態は、従来も「協働エージェントでもオーナーでも
    /// ない＝ただの信頼済みユーザー」に落ちていた。判定結果を変えないため、その
    /// 落とし先を保つ（緩む方向へは倒さない）。
    pub fn from_db_str(s: &str) -> Self {
        Self::parse(s).unwrap_or(Self::User)
    }
}

#[derive(Debug, Clone)]
pub struct TrustedUserRow {
    pub id: String,
    pub user_id: String,
    pub agent_id: String,
    /// 与えられている権限（#234 で素の文字列から列挙型へ）。
    pub permission: TrustedUserPermission,
    pub created_by: String,
    pub created_at: String,
    /// ロスター表示用の名前（ピアレビュアー一覧等）。空文字可。
    pub display_name: String,
    /// `user_id` がどの経路の識別子か（#214）。既存行は `discord`。
    pub platform: String,
}

fn trusted_user_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrustedUserRow> {
    Ok(TrustedUserRow {
        id: row.get(0)?,
        user_id: row.get(1)?,
        agent_id: row.get(2)?,
        permission: TrustedUserPermission::from_db_str(&row.get::<_, String>(3)?),
        created_by: row.get(4)?,
        created_at: row.get(5)?,
        display_name: row.get(6)?,
        platform: row.get(7)?,
    })
}

const TRUSTED_USER_COLUMNS: &str =
    "id, user_id, agent_id, permission, created_by, created_at, display_name, platform";

/// `(経路, 識別子, エージェント)` で 1 行引く。経路が違えば別人として扱う。
///
/// 引けない（未登録 / DB エラー）ときは `None` = 最小権限へ倒れる（fail-closed）。
pub fn get_trusted_user(
    conn: &Connection,
    platform: &str,
    user_id: &str,
    agent_id: &str,
) -> Option<TrustedUserRow> {
    conn.query_row(
        &format!(
            "SELECT {TRUSTED_USER_COLUMNS} \
             FROM trusted_users \
             WHERE platform = ?1 AND user_id = ?2 AND agent_id = ?3"
        ),
        [platform, user_id, agent_id],
        trusted_user_from_row,
    )
    .ok()
}

/// 管理 UI（`GET /api/agents/{id}/trusted-users`）向けの一覧。**経路で絞らない**
/// （運用者が全経路の登録を一覧できる必要があるため）。認可の判定には使わない。
pub fn list_trusted_users(conn: &Connection, agent_id: &str) -> Result<Vec<TrustedUserRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRUSTED_USER_COLUMNS} \
         FROM trusted_users WHERE agent_id = ?1 ORDER BY created_at ASC"
    ))?;
    let rows = stmt.query_map([agent_id], trusted_user_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// 信頼済みユーザーを 1 件登録する。
///
/// `platform` は「`user_id` がどの経路の識別子か」。読み出し側と同じ位置
/// （`conn` の次）に置いて、経路と permission の取り違えを起きにくくしている。
///
/// 一意制約は `(user_id, agent_id)` のままなので、**同じ識別子文字列を
/// 別経路で登録することはまだできない**（制約の作り直しは非可逆なので #159）。
/// 衝突すると `UNIQUE constraint failed` で Err になる（呼び出し元の REST は 409 に
/// 写す）。旧行を消してから登録し直す運用で回避できる — `docs/api.md` の移行手順。
#[allow(clippy::too_many_arguments)]
pub fn add_trusted_user(
    conn: &Connection,
    platform: &str,
    id: &str,
    agent_id: &str,
    user_id: &str,
    permission: TrustedUserPermission,
    created_by: &str,
    created_at: &str,
    display_name: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO trusted_users (id, user_id, agent_id, permission, created_by, created_at, display_name, platform) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        [id, user_id, agent_id, permission.as_db_str(), created_by, created_at, display_name, platform],
    )?;
    Ok(())
}

/// このエージェントのピアレビュアー（permission='co-agent' の trusted user）一覧。
/// プロンプトのロスター表示と reviewer 解決の両方がこれを使う（選定ロジックの一元化）。
///
/// **`platform` で絞る（#159）**。以前は絞っていなかったが、返信の受理ゲート
/// （`peer_review::record_peer_review_reply`）は送信者を**その受信経路の空間**で引くため、
/// 別経路の co_agent は「依頼は飛ぶが返信を受理されない」非対称になっていた。名簿を
/// 受理側と同じ経路に揃えることで、指名できる相手＝返信を受理できる相手にする。
///
/// **緩める方向へは動かせない**: ここは絞り込みだけで、受理ゲート側に新しい経路の
/// 判定を足してはいない（別経路に認可判定を新設すると権限が昇格しうる経路になる）。
pub fn list_co_agent_reviewers(
    conn: &Connection,
    platform: &str,
    agent_id: &str,
) -> Result<Vec<TrustedUserRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRUSTED_USER_COLUMNS} \
         FROM trusted_users WHERE platform = ?1 AND agent_id = ?2 AND permission = ?3 \
         ORDER BY created_at ASC"
    ))?;
    let rows = stmt.query_map(
        [
            platform,
            agent_id,
            TrustedUserPermission::CoAgent.as_db_str(),
        ],
        trusted_user_from_row,
    )?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn update_trusted_user_display_name(
    conn: &Connection,
    id: &str,
    display_name: &str,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE trusted_users SET display_name = ?2 WHERE id = ?1",
        [id, display_name],
    )?;
    Ok(n > 0)
}

pub fn update_trusted_user_permission(
    conn: &Connection,
    id: &str,
    permission: TrustedUserPermission,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE trusted_users SET permission = ?2 WHERE id = ?1",
        [id, permission.as_db_str()],
    )?;
    Ok(n > 0)
}

pub fn remove_trusted_user(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM trusted_users WHERE id = ?1", [id])?;
    Ok(n > 0)
}

/// `(経路, 識別子, エージェント)` が登録されているか。DB エラー時は false（fail-closed）。
pub fn is_trusted_user(conn: &Connection, platform: &str, user_id: &str, agent_id: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM trusted_users \
         WHERE platform = ?1 AND user_id = ?2 AND agent_id = ?3",
        [platform, user_id, agent_id],
        |row| row.get::<_, i64>(0),
    )
    .map(|c| c > 0)
    .unwrap_or(false)
}

/// そのエージェントに **その経路の** 信頼ユーザーが何件登録されているか（#214）。
///
/// **認可の判定には使わないこと。** かつて DM 許可が「登録が 0 件ならオーナーのみ、
/// オーナー未設定なら全許可」という二段構えで件数を見ていたが、この
/// fail-open は #174 で撤去した（許可は `is_trusted_user` とオーナー判定だけで
/// 決まる）。件数で分岐を切り替える形を復活させると、経路ごとに登録が分かれた
/// 途端（#159）に権限が緩む方向へ倒れる。
///
/// **現在の呼び出し元は無い**（この crate のテストのみ）。件数の表示・統計のような
/// 認可と無関係な用途のために残してある。
pub fn trusted_user_count(conn: &Connection, platform: &str, agent_id: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM trusted_users WHERE platform = ?1 AND agent_id = ?2",
        [platform, agent_id],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}
