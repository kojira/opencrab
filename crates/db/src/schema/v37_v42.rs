use chrono::Utc;
use rusqlite::{params, Connection};

use super::helpers::column_exists;
use super::sql::{AGENT_SCHEDULES_SQL, SESSION_HEARTBEAT_CONFIG_SQL};

/// Discord の `guild_id` / `channel_id` を正規化する（設計 §4.2 B3）。
///
/// 本番データに引用符付きの値（例 `"222233334444555566"`）が混ざるため、連結して
/// `session_id` を作る前に `"` と空白（半角空白・タブ・改行など）を除去する。数字だけが
/// 残る前提で、残らなければ後段の [`session_id_is_valid`] が弾く（fail-closed）。
pub(super) fn norm_discord_id(raw: &str) -> String {
    raw.chars()
        .filter(|c| *c != '"' && !c.is_whitespace())
        .collect()
}

/// backfill が作った `session_id` が発火先を導ける形式かを検証する（設計 §3.6 / §4.2 B4）。
///
/// `agent_id` はハイフンを含む UUID なので naive な `split('-')` はしない。保存済みの
/// `agent_id` で接頭辞を剥がし、`nostr-{agent}` か `discord-{agent}-{digits}-{digits}` に
/// 合致するかだけを見る（guild/channel は数字のみ）。**未知/解釈不能は false = fail-closed**。
pub(super) fn session_id_is_valid(session_id: &str, agent_id: &str) -> bool {
    if session_id == format!("nostr-{agent_id}") {
        return true;
    }
    if let Some(rest) = session_id.strip_prefix(&format!("discord-{agent_id}-")) {
        // rest = "{guild}-{channel}"。guild/channel は数値（ハイフン無し）なので rsplit_once 安全。
        if let Some((guild, channel)) = rest.rsplit_once('-') {
            return !guild.is_empty()
                && !channel.is_empty()
                && guild.chars().all(|c| c.is_ascii_digit())
                && channel.chars().all(|c| c.is_ascii_digit());
        }
    }
    false
}

/// v37 マイグレーション本体（セッション一本化スキーマ + backfill / #439 × #455 × #456・PR1）。
///
/// **現状の発火挙動を 1 ビットも変えない**のが不変条件（設計 §4.2）。`run_migrations` の
/// per-migration トランザクション内で走り、**この関数が `Err` を返すと全体がロールバック**
/// される。移行行の形式検証は commit 前にこの関数内で行う（別コネクション・commit 後検証は
/// 原子性が崩れるので使わない）。
/// v38: `agent_schedules` の語彙を heartbeat に揃える（#455・設計 §7）。
///
/// heartbeat の `last_fired_at` / 照会時算出（キャッシュ列なし）に合わせて、
/// `last_run_at` を RENAME し `next_run_at` 列を撤去する。**非破壊**（RENAME は値を保存）。
/// `column_exists` ガードで新規 DB（SCHEMA_SQL 側で既に最終形）では no-op（#349 の轍回避）。
pub(super) fn migrate_v38_align_schedule_vocab(conn: &Connection) -> rusqlite::Result<()> {
    // 1. last_run_at → last_fired_at（heartbeat の語彙へ統一）。値はそのまま保存される。
    if column_exists(conn, "agent_schedules", "last_run_at")?
        && !column_exists(conn, "agent_schedules", "last_fired_at")?
    {
        conn.execute_batch(
            "ALTER TABLE agent_schedules RENAME COLUMN last_run_at TO last_fired_at;",
        )?;
    }
    // 2. next_run_at を撤去（照会時算出に寄せる＝stale フリー。列はキャッシュに過ぎない）。
    if column_exists(conn, "agent_schedules", "next_run_at")? {
        conn.execute_batch("ALTER TABLE agent_schedules DROP COLUMN next_run_at;")?;
    }
    Ok(())
}

pub(super) fn migrate_v37_session_heartbeat(conn: &Connection) -> rusqlite::Result<()> {
    // 1. 新テーブル（冪等）。新規 DB は SCHEMA_SQL 側で既に作成済みなので no-op。
    conn.execute_batch(SESSION_HEARTBEAT_CONFIG_SQL)?;
    conn.execute_batch(AGENT_SCHEDULES_SQL)?;

    // 移行時刻（壁時計・rfc3339）。enabled 行の anchor に打ち、移行直後の一斉発火を避けて
    // next_fire を「移行時刻 + interval（未来）」へ置く（＝密にしない・設計 §4.4 の「後ろ」）。
    let now = Utc::now().to_rfc3339();

    // opt-in 集合。opt-in 済みは現状 Discord channel 発火が precedence（AgentScoped）で抑止
    // （沈黙）されているので、その抑止を enabled=0 として保存する（step2）。**向き**: enabled を
    // 0 へ倒す＝発火を「増やさない」方向（沈黙の保存）。
    //
    // **判定は `resolve_agent_heartbeat`（heartbeat.rs:193）の意味論に一致させる（F2 修正）**:
    // raw `enabled=1` ではなく、`interval_secs <= 0`（壊れた値）は resolve が `enabled:false` へ
    // 倒すため opt-in から除外する。除外すると当該 agent は AgentScoped に入らず、未 opt-in として
    // ChannelScoped（G 有効時）で Discord 発火する現状に一致する。**この不一致を捕まえるため、
    // 不変条件テストの旧側は raw ではなく resolve_agent_heartbeat を使う**（テストが移行と同じ
    // 近似を共有しないようにする）。
    let opted_in: std::collections::HashSet<String> = {
        let mut stmt = conn.prepare(
            "SELECT agent_id FROM agent_heartbeat_config
             WHERE enabled = 1 AND (interval_secs IS NULL OR interval_secs > 0)",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };

    // ── step1: Nostr セッション ─────────────────────────────────────────────
    // opt-in 済み（resolve 意味論・上記）かつ **Nostr gateway が実際に稼働する条件を満たす**
    // → nostr-{agent} を enabled=1 で作る。Nostr の agent スコープ発火は global（G）に依らず
    // 発火していたので enabled=1（G ゲート対象外）。
    //
    // **Nostr 判定は runtime の実発火条件に一致させる（F1 修正）**: 単なる EXISTS ではなく
    // **`agent_nostr_config.enabled = 1`** を要求する。runtime は enabled=1 の gateway だけを
    // 起動し（nostr_runner_impl.rs:94）、時刻発火は**稼働中の gateway だけが受け口を登録する**
    // `TimedFireRouter`（#588・nostr/manager.rs の `run_nostr_loop` で register）経由で届く。
    // EXISTS だけだと **nostr disabled のエージェント
    // を enabled=1 の nostr セッションにして PR2 で新規発火させてしまう**（runtime では鳴らない）。
    // opt-in だが Nostr 稼働条件を満たさない（Discord 専用の旧 agent スコープ）は現状も出口なしで
    // 沈黙 → セッション行を作らない（#456 決定3）。interval は agent_heartbeat_config の保持値。
    {
        let mut stmt = conn.prepare(
            "SELECT ahc.agent_id, ahc.interval_secs
             FROM agent_heartbeat_config ahc
             WHERE ahc.enabled = 1 AND (ahc.interval_secs IS NULL OR ahc.interval_secs > 0)
               AND EXISTS (SELECT 1 FROM agent_nostr_config anc
                           WHERE anc.agent_id = ahc.agent_id AND anc.enabled = 1)",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (agent_id, interval_secs) in rows {
            let session_id = format!("nostr-{agent_id}");
            conn.execute(
                "INSERT INTO session_heartbeat_config
                    (agent_id, session_id, enabled, interval_secs, anchor_at, last_fired_at, updated_at)
                 VALUES (?1, ?2, 1, ?3, ?4, NULL, ?4)
                 ON CONFLICT(agent_id, session_id) DO NOTHING",
                params![agent_id, session_id, interval_secs, now],
            )?;
        }
    }

    // ── step2: Discord channel セッション（explicit per-agent 行）──────────────
    // discord_channel_config.heartbeat_enabled=1 AND agent_id!='' を移す。
    //   enabled = opt-in 済みなら 0（抑止を保存）、未 opt-in なら 1。
    //   ※ 未 opt-in を無条件 1 にしてよいのは、G=false 時に発火を止めるのはランタイムの G
    //     ゲート（PR2）が担うため（enabled は「このセッションの HB 設定は on」の意味で、
    //     実発火は `enabled AND (nostr- OR G)`）。ここで G を焼き込まない（A2）。
    // session_id = discord-{agent}-{norm(guild)}-{norm(channel)}（B3 正規化）。
    // anchor は enabled=1 のみ now（enabled=0 は有効化時に打つ）。
    {
        let mut stmt = conn.prepare(
            "SELECT agent_id, guild_id, channel_id, heartbeat_interval_secs
             FROM discord_channel_config
             WHERE heartbeat_enabled = 1 AND agent_id != ''",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (agent_id, guild_id, channel_id, interval_secs) in rows {
            let guild = norm_discord_id(&guild_id);
            let channel = norm_discord_id(&channel_id);
            let session_id = format!("discord-{agent_id}-{guild}-{channel}");
            let enabled: i64 = if opted_in.contains(&agent_id) { 0 } else { 1 };
            let anchor: Option<&str> = if enabled == 1 {
                Some(now.as_str())
            } else {
                None
            };
            conn.execute(
                "INSERT INTO session_heartbeat_config
                    (agent_id, session_id, enabled, interval_secs, anchor_at, last_fired_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)
                 ON CONFLICT(agent_id, session_id) DO NOTHING",
                params![agent_id, session_id, enabled, interval_secs, anchor, now],
            )?;
        }
    }

    // ── step3: Discord global 行（agent_id=''）の展開（enabled=0・統括裁定確定）────
    // global 行（heartbeat_enabled=1）が現に効かせていた「その channel の既定」を、対象
    // エージェントごとに **enabled=0** の行として記録する（発火はさせない）。
    //
    // **enabled=0 の理由と向き（過去に向き違いの事故があるため明記）**: この移行は「HB
    // ループが立つエージェント集合（config の discord `agent_ids` ∪ opt-in）」を参照できない
    // （G と同じ TOML/runtime 概念）。したがって global fallback 経由で現に発火している
    // エージェントを enabled=1 で正しく再現できない。**発火を増やさない側（enabled=0）へ倒す。**
    // 行自体は残すので「かつて global 既定で拾われていた」事実は #460 の議論材料として保存される。
    //
    // **限界（PR/設計に明記）**: global fallback 経由の発火はこの移行では保存されない。**本番では
    // その集合が空**（その channel に明示行を持たないエージェントは HB ループに含まれない）で
    // あることを本番コピーで実測確認済み。他環境ではこの経路の発火は沈黙側へ倒れる。
    //
    // 対象 = `agents` のうち、その channel に明示行を持たず（明示行持ちは step2 で移行済み）、
    // かつ whitelisted（明示行が無いので global 行の whitelisted へ fallback）なエージェント。
    // 名前で分岐しない（データ駆動）。interval は global 行の値を保持（enabled=0 なので発火は
    // しないが値は残す）。step2 先行 + 明示チェック + ON CONFLICT DO NOTHING で二重の保険。
    {
        let globals: Vec<(String, String, Option<i64>)> = {
            let mut stmt = conn.prepare(
                "SELECT guild_id, channel_id, heartbeat_interval_secs
                 FROM discord_channel_config
                 WHERE agent_id = '' AND heartbeat_enabled = 1",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let agents: Vec<String> = {
            let mut stmt = conn.prepare("SELECT agent_id FROM agents")?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for (guild_id, channel_id, interval_secs) in &globals {
            let guild = norm_discord_id(guild_id);
            let channel = norm_discord_id(channel_id);
            for agent_id in &agents {
                // その channel に明示行を持つエージェントは step2 で移行済み → skip。
                let explicit: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM discord_channel_config WHERE channel_id = ?1 AND agent_id = ?2",
                    params![channel_id, agent_id],
                    |r| r.get(0),
                )?;
                if explicit > 0 {
                    continue;
                }
                // whitelisted（明示行なし → global 行の whitelisted へ fallback）でなければ skip。
                if !crate::queries::is_channel_whitelisted_for_agent(conn, channel_id, agent_id) {
                    continue;
                }
                let session_id = format!("discord-{agent_id}-{guild}-{channel}");
                conn.execute(
                    "INSERT INTO session_heartbeat_config
                        (agent_id, session_id, enabled, interval_secs, anchor_at, last_fired_at, updated_at)
                     VALUES (?1, ?2, 0, ?3, NULL, NULL, ?4)
                     ON CONFLICT(agent_id, session_id) DO NOTHING",
                    params![agent_id, session_id, interval_secs, now],
                )?;
            }
        }
    }

    // ── 検証（設計 §4.2.4・全移行行）───────────────────────────────────────────
    // 全 session_id が nostr-{agent} / discord-{agent}-{digits}-{digits} に合致するか。
    // 合致しない行があれば Err → per-migration tx でアトミックにロールバック（版トラップ無し）。
    {
        let mut stmt = conn.prepare("SELECT agent_id, session_id FROM session_heartbeat_config")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (agent_id, session_id) in rows {
            if !session_id_is_valid(&session_id, &agent_id) {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                    Some(format!(
                        "v37 backfill produced malformed session_id '{session_id}' (fail-closed; migration rolled back)"
                    )),
                ));
            }
        }
    }

    Ok(())
}
