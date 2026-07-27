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

pub fn update_trusted_co_agent_actions(
    conn: &Connection,
    agent_id: &str,
    co_agent_id: &str,
    allowed_actions: Option<&str>,
) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE trusted_co_agents SET allowed_actions = ?3 WHERE agent_id = ?1 AND co_agent_id = ?2",
        params![agent_id, co_agent_id, allowed_actions],
    )?;
    Ok(updated > 0)
}

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
// web / REST は自分の側のユーザー識別子で同じ表を引いていたため、識別子が一致すると
// 信頼が経路をまたいで引き継がれていた。`platform` 列を足し、認可の読み出しは
// `(platform, user_id, agent_id)` で引く。
//
// 表名 `trusted_discord_users` / 列名 `discord_user_id` は #159 で `trusted_users` /
// `user_id` に改名した（マイグレーション v17。Discord は `platform` の値のひとつでしかない）。
//
// 互換読み（自経路の行が無ければ従来の `discord` 経路も見る暫定のフォールバック）は
// #159 で撤去した。登録 API が経路を受け取れるようになった（`POST /api/agents/{id}/
// trusted-users` の `platform`）ため、web / REST のユーザーは自経路の行として登録する。
// **経路ごとの行を持たない既存の web / REST ユーザーはこの時点で信頼を失う**（緩む方向
// ではなく機能が止まる方向）。移行手順は `docs/api.md` の Trusted Users を参照。
//
// **#159 に残っている作業**:
// - 一意制約 `(user_id, agent_id)` → `(platform, user_id, agent_id)`（表の再構築＝非可逆）

/// 列追加前から存在する行が属する経路。マイグレーションの `DEFAULT` と一致させる。
pub const TRUSTED_PLATFORM_DISCORD: &str = "discord";
/// ダッシュボード（web ゲートウェイ）が申告するユーザー識別子の経路。
pub const TRUSTED_PLATFORM_WEB: &str = "web";
/// REST `POST /api/agents/{id}/messages` が申告するユーザー識別子の経路。
pub const TRUSTED_PLATFORM_REST: &str = "rest";

/// 読み出し側が実際に引く経路の全体。登録 API の検証に使う。
///
/// 未知の経路の行は**どの読み出しとも一致しない**（登録しても誰も信頼されない）。
/// 綴り間違いが「登録できたのに効かない」行として黙って残るのを防ぐため、
/// 登録 API はこの集合で弾く（fail-closed 側の検証であって、認可の判定ではない）。
pub const TRUSTED_PLATFORMS: [&str; 3] = [
    TRUSTED_PLATFORM_DISCORD,
    TRUSTED_PLATFORM_WEB,
    TRUSTED_PLATFORM_REST,
];

/// 読み出し側が引く経路として定義済みか。
pub fn is_known_trusted_platform(platform: &str) -> bool {
    TRUSTED_PLATFORMS.contains(&platform)
}

#[derive(Debug, Clone)]
pub struct TrustedUserRow {
    pub id: String,
    pub user_id: String,
    pub agent_id: String,
    pub permission: String,
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
        permission: row.get(3)?,
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
    permission: &str,
    created_by: &str,
    created_at: &str,
    display_name: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO trusted_users (id, user_id, agent_id, permission, created_by, created_at, display_name, platform) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        [id, user_id, agent_id, permission, created_by, created_at, display_name, platform],
    )?;
    Ok(())
}

/// このエージェントのピアレビュアー（permission='co_agent' の trusted user）一覧。
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
         FROM trusted_users WHERE platform = ?1 AND agent_id = ?2 AND permission = 'co_agent' \
         ORDER BY created_at ASC"
    ))?;
    let rows = stmt.query_map([platform, agent_id], trusted_user_from_row)?;
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
    permission: &str,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE trusted_users SET permission = ?2 WHERE id = ?1",
        [id, permission],
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
