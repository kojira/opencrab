use super::super::*;
use rusqlite::Connection;
// ── v37: セッション一本化スキーマ + 移行（#439 × #455 × #456・PR1）──────────

/// v37 適用前（user_version=36）の DB を模す: 新表 2 つを落として版を 36 へ戻す。
/// 旧表（agent_heartbeat_config / discord_channel_config / agent_nostr_config）は
/// baseline/番号付き migration で既に存在するので、そこへ fixture を積む。
fn setup_pre_v37(conn: &Connection) {
    conn.execute_batch(
        "DROP TABLE IF EXISTS session_heartbeat_config;
             DROP TABLE IF EXISTS agent_schedules;
             PRAGMA user_version = 36;",
    )
    .unwrap();
    assert_eq!(schema_version(conn).unwrap(), 36);
}

fn shc_row(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
) -> crate::queries::SessionHeartbeatConfigRow {
    crate::queries::get_session_heartbeat_config(conn, agent_id, session_id)
        .unwrap()
        .unwrap_or_else(|| panic!("expected session row {agent_id} / {session_id}"))
}

/// v37 backfill が **現状の発火挙動を保存**することを検証する（設計 §4.2 の step1/step2/
/// step3・正規化）。step3 の global 展開は **enabled=0**（発火を増やさない）で作る。
#[test]
fn v37_backfill_preserves_firing_and_normalizes() {
    let conn = crate::init_memory().expect("init");
    setup_pre_v37(&conn);

    // 旧設定の fixture。
    //  A: opt-in 済み(enabled=1) かつ Nostr 有り  → nostr-A enabled=1、Discord 抑止(0)
    //  B: opt-in 済み かつ Nostr 無し             → nostr 作らない（出口なし・沈黙）
    //  C: 未 opt-in（行はあるが disabled）        → Discord enabled=1
    //  D: agent_heartbeat_config に行なし=未 opt-in、guild/channel が引用符付き → 正規化
    //  E: heartbeat_enabled=0 の Discord 行       → 移行しない
    //  global('' 行, ch205, whitelisted=1)        → step3 で A〜E に enabled=0 展開
    conn.execute_batch(
            "INSERT INTO agents (agent_id, name, persona_name) VALUES
                ('A','A','A'),('B','B','B'),('C','C','C'),('D','D','D'),('E','E','E');
             INSERT INTO agent_heartbeat_config (agent_id, enabled, interval_secs, updated_at) VALUES
                ('A', 1, 18000, '2026-01-01'),
                ('B', 1, 1200,  '2026-01-01'),
                ('C', 0, 10800, '2026-01-01');
             INSERT INTO agent_nostr_config (agent_id, secret_key, relays_json, filter_json, enabled, updated_at) VALUES
                ('A', 'nsecA', '[]', '{}', 1, '2026-01-01');
             INSERT INTO discord_channel_config
                (channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, heartbeat_instructions, updated_at) VALUES
                ('201', 'A', '100', '', 1, 1, 1, 1, NULL,  '', '2026-01-01'),
                ('202', 'C', '100', '', 1, 1, 1, 1, 10800, '', '2026-01-01'),
                ('\"222\"', 'D', '\"111\"', '', 1, 1, 1, 1, NULL, '', '2026-01-01'),
                ('204', 'E', '100', '', 1, 1, 1, 0, NULL,  '', '2026-01-01'),
                ('205', '',  '100', '', 1, 1, 1, 1, NULL,  '', '2026-01-01');",
        )
        .unwrap();

    initialize(&conn).expect("apply v37");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    // v47（DI 拡張の gateway_operation_calls + gate_instances.operation_declaration_digest）が
    // 最新。v38..v47 とも session_heartbeat_config を触らないので、下の v37 backfill 検証（発火
    // 集合・正規化）はそのまま成立する。新しい migration が session_heartbeat_config を触ったら
    // この guard を更新し、下の期待値を見直すこと。
    assert_eq!(latest_version(), 47, "v47 が最新版であること");

    // 期待: 9 行 = step1(nostr-A) 1 + step2(A/201=0, C/202=1, D/222=1) 3 +
    //             step3(A,B,C,D,E の ch205 展開・全 enabled=0) 5。
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_heartbeat_config", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(total, 9, "backfill 行数");

    // step1: Nostr セッション（G 非依存で発火していたので enabled=1・anchor 打つ）。
    let a_nostr = shc_row(&conn, "A", "nostr-A");
    assert!(a_nostr.enabled);
    assert_eq!(
        a_nostr.interval_secs,
        Some(18000),
        "意図した interval を保持"
    );
    assert!(a_nostr.anchor_at.is_some(), "enabled 行は anchor を打つ");
    assert!(a_nostr.last_fired_at.is_none());

    // step2: A の Discord 行は opt-in 抑止を enabled=0 として保存（anchor は NULL）。
    let a_disc = shc_row(&conn, "A", "discord-A-100-201");
    assert!(
        !a_disc.enabled,
        "opt-in 済みの Discord 発火は現状沈黙＝enabled=0 で保存"
    );
    assert!(a_disc.anchor_at.is_none(), "enabled=0 は anchor を打たない");

    // step2: C（未 opt-in）は enabled=1・interval 保持・anchor 打つ。
    let c_disc = shc_row(&conn, "C", "discord-C-100-202");
    assert!(c_disc.enabled);
    assert_eq!(c_disc.interval_secs, Some(10800));
    assert!(c_disc.anchor_at.is_some());

    // step2 正規化(B3): guild/channel の引用符が除去され discord-D-111-222 になる。
    let d_disc = shc_row(&conn, "D", "discord-D-111-222");
    assert!(d_disc.enabled);

    // B: Nostr 無しの opt-in は出口なし＝セッションを作らない（#456 決定3）。
    assert!(
        crate::queries::get_session_heartbeat_config(&conn, "B", "nostr-B")
            .unwrap()
            .is_none(),
        "Discord 専用 opt-in は Nostr セッションを作らない"
    );
    // E: heartbeat_enabled=0 は移行しない。
    assert!(
        crate::queries::get_session_heartbeat_config(&conn, "E", "discord-E-100-204")
            .unwrap()
            .is_none()
    );
    // step3: global 行(ch205)を A〜E へ **enabled=0** で展開（発火は増やさない・統括裁定）。
    //  B は他に行が無いエージェントだが、global 既定の到達先として enabled=0 行が残る。
    let b_205 = shc_row(&conn, "B", "discord-B-100-205");
    assert!(!b_205.enabled, "global 展開は enabled=0（発火させない）");
    assert!(b_205.anchor_at.is_none());
    let a_205 = shc_row(&conn, "A", "discord-A-100-205");
    assert!(!a_205.enabled);
    // step3 が発火（enabled=1）を 1 件も増やさないこと。
    let expanded_enabled: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_heartbeat_config WHERE session_id LIKE '%-100-205' AND enabled = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(expanded_enabled, 0, "global 展開で発火を増やさない");
    // agent_id='' のセッションは決して作らない（global 行そのものは session にしない）。
    let global_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_heartbeat_config WHERE agent_id = ''",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(global_rows, 0, "agent_id='' のセッションは作らない");
}

/// v37 backfill が壊れた session_id（非数値 channel）を作ったら `up()` 内検証で `Err` を
/// 返し、per-migration トランザクションで**アトミックにロールバック**する（設計 §4.2.4）。
#[test]
fn v37_backfill_rejects_malformed_session_id_and_rolls_back() {
    let conn = crate::init_memory().expect("init");
    setup_pre_v37(&conn);

    // 未 opt-in agent X の Discord 行で channel_id が非数値 → discord-X-100-abc（不正形式）。
    conn.execute_batch(
            "INSERT INTO discord_channel_config
                (channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, heartbeat_instructions, updated_at) VALUES
                ('abc', 'X', '100', '', 1, 1, 1, 1, NULL, '', '2026-01-01');",
        )
        .unwrap();

    let result = initialize(&conn);
    assert!(result.is_err(), "壊れた session_id は fail-closed で Err");
    // ロールバック: 版は 36 のまま、新表も作られていない（CREATE ごと巻き戻る）。
    assert_eq!(schema_version(&conn).unwrap(), 36, "版トラップは起きない");
    assert!(
        !table_exists(&conn, "session_heartbeat_config").unwrap(),
        "Err なら CREATE TABLE ごとロールバックされる"
    );
}

/// SCHEMA_SQL（新規 DB）と v37 migration（既存 DB）が **同じ形**の新表を作ることを、
/// sqlite_master の SQL 文字列で比較して固定する（定数と SCHEMA_SQL の drift を検出）。
/// 新規 DB（SCHEMA_SQL 経由）と既存 DB（v37→v38 migration 経由）が **同じ最終形**の
/// スキーマへ収束することを固定する（定数と SCHEMA_SQL の drift を検出）。
///
/// v38 で `agent_schedules` は v37 の CREATE を **ALTER で書き換える**ため、
/// `sqlite_master.sql` の生テキストは両経路で一致しない（fresh=SCHEMA_SQL の手書き、
/// migrated=旧定数を ALTER が書き換えたもの）。→ **agent_schedules は列構造
/// （pragma_table_info）で比較**する（空白非依存・ALTER 安全・構造契約そのもの）。
/// v38 が触らない `session_heartbeat_config` と index は従来どおり生 SQL で比較する。
#[test]
fn schedule_schema_parity_fresh_vs_migrated() {
    // 生 SQL 比較対象（v38 が触らない = 両経路で CREATE テキストが同一）。
    let raw_sql = |conn: &Connection| -> Vec<String> {
        conn.prepare(
            "SELECT sql FROM sqlite_master
                 WHERE name IN ('session_heartbeat_config', 'idx_agent_schedules_agent')
                   AND sql IS NOT NULL
                 ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    // 列構造比較（name, type, notnull, dflt_value, pk）。ALTER 経由でも一致する契約。
    let cols = |conn: &Connection,
                table: &str|
     -> Vec<(String, String, i64, Option<String>, i64)> {
        conn.prepare("SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1) ORDER BY cid")
                .unwrap()
                .query_map([table], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
    };

    // 新規 DB: SCHEMA_SQL 由来。
    let fresh = crate::init_memory().expect("fresh");
    // 既存 DB: 新表を落として版を 36 へ戻し、v37→v38 migration で作り直す。
    let migrated = crate::init_memory().expect("migrated");
    setup_pre_v37(&migrated);
    initialize(&migrated).expect("re-migrate v37+v38");

    assert_eq!(raw_sql(&fresh), raw_sql(&migrated));
    assert_eq!(raw_sql(&fresh).len(), 2, "1 table(shc) + 1 index");
    assert_eq!(
        cols(&fresh, "agent_schedules"),
        cols(&migrated, "agent_schedules"),
        "agent_schedules の列構造は新規/既存で一致（v38 収束）"
    );
    // 語彙が heartbeat へ揃い、キャッシュ列が消えたことを両経路で固定する。
    let names: Vec<String> = cols(&fresh, "agent_schedules")
        .into_iter()
        .map(|c| c.0)
        .collect();
    assert!(
        names.contains(&"last_fired_at".to_string()),
        "last_fired_at に揃っている"
    );
    assert!(
        !names.contains(&"last_run_at".to_string()),
        "旧 last_run_at は消えている"
    );
    assert!(
        !names.contains(&"next_run_at".to_string()),
        "next_run_at キャッシュ列は撤去されている（照会時算出）"
    );
}

/// v38 が **非破壊**（RENAME は行データを保存し、DROP は next_run_at だけを落とす）で
/// あることを、v37 の旧列に値を入れた行が v38 後も `last_fired_at` に生き残ることで固定する。
/// DROP TABLE 方式へ退行するとこのテストが落ちる（0 行前提でも安全側＝保存を守る）。
#[test]
fn v38_is_non_destructive_and_preserves_last_fired() {
    let conn = crate::init_memory().expect("db");
    // v37 の旧スキーマ（last_run_at / next_run_at）まで巻き戻す。
    setup_pre_v37(&conn);
    // v37 だけ適用した状態を作るため、v38 を含まない一時 MIGRATIONS で v37 まで進める。
    let up_to_v37: Vec<Migration> = MIGRATIONS
        .iter()
        .filter(|m| m.version <= 37)
        .map(|m| Migration {
            version: m.version,
            description: m.description,
            up: m.up,
        })
        .collect();
    run_migrations(&conn, &up_to_v37).expect("apply through v37");
    assert!(column_exists(&conn, "agent_schedules", "last_run_at").unwrap());
    assert!(column_exists(&conn, "agent_schedules", "next_run_at").unwrap());

    // 旧列に値を持つ行を入れる（万一本番に行があっても保存されることの代理検証）。
    conn.execute(
            "INSERT INTO agent_schedules
                (agent_id, session_id, cron_expr, timezone, message, enabled, anchor_at, last_run_at, next_run_at, created_at, updated_at)
             VALUES ('a1','nostr-a1','0 7 * * *','Asia/Tokyo','morning',1,'2026-01-01T00:00:00Z','2026-08-09T07:00:00+09:00','2026-08-10T07:00:00+09:00','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

    // v38 を適用。
    run_migrations(&conn, MIGRATIONS).expect("apply v38");

    // RENAME で last_fired_at に値が保存され、next_run_at 列は消えている。
    assert!(!column_exists(&conn, "agent_schedules", "last_run_at").unwrap());
    assert!(column_exists(&conn, "agent_schedules", "last_fired_at").unwrap());
    assert!(!column_exists(&conn, "agent_schedules", "next_run_at").unwrap());
    let preserved: String = conn
        .query_row(
            "SELECT last_fired_at FROM agent_schedules WHERE agent_id='a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        preserved, "2026-08-09T07:00:00+09:00",
        "RENAME は last_run_at の値を last_fired_at に保存する（非破壊）"
    );
}

#[test]
fn norm_discord_id_strips_quotes_and_whitespace() {
    assert_eq!(
        norm_discord_id("\"222233334444555566\""),
        "222233334444555566"
    );
    assert_eq!(norm_discord_id("  123 456 "), "123456");
    assert_eq!(norm_discord_id("123\t\n"), "123");
    assert_eq!(norm_discord_id("123"), "123");
}

#[test]
fn session_id_is_valid_handles_uuid_agent_and_fail_closed() {
    let agent = "11111111-1111-4111-8111-111111111111"; // ハイフンを含む UUID
    assert!(session_id_is_valid(&format!("nostr-{agent}"), agent));
    assert!(session_id_is_valid(
        &format!("discord-{agent}-100-201"),
        agent
    ));
    // 非数値 guild/channel は fail-closed。
    assert!(!session_id_is_valid(
        &format!("discord-{agent}-100-abc"),
        agent
    ));
    assert!(!session_id_is_valid(
        &format!("discord-{agent}-abc-201"),
        agent
    ));
    // 発火経路を持たない種別・未知接頭辞は fail-closed。
    assert!(!session_id_is_valid(&format!("web-{agent}"), agent));
    assert!(!session_id_is_valid(&format!("heartbeat-{agent}"), agent));
    // 別 agent の id で剥がそうとしても合致しない。
    assert!(!session_id_is_valid(
        &format!("nostr-{agent}"),
        "other-agent"
    ));
}

// ── 不変条件テスト（設計 §4.2 A2 / 受け入れ基準 B1）─────────────────────────
//
// 「移行が発火集合を変えない」を、**期待集合を手書きせず**に検証する。旧側は実コード
// 経路どおりに計算し、新側は移行後の enabled セッションから計算して**一致**を見る。
// `G ∈ {true, false}` でパラメタライズする（移行は G を焼き込まないので DB は 1 つで両方
// 計算できる）。
//
// **旧側の実発火の定義（実コードを正・統括指示で訂正済み）**: 現行の ChannelScoped 発火
// 経路（main.rs:494-590）は **whitelist ゲートも writable ゲートも適用しない**（発火先は
// `list_heartbeat_channels`＝`heartbeat_enabled=1` のみ・channel_config.rs:198）。よって
// 旧側実発火に `is_channel_whitelisted_for_agent` を含めてはいけない。旧発火は **HB ループ
// が立つエージェント**（config discord `agent_ids` ∪ opt-in）にのみ起こるため、loop 集合を
// 入力に取る。

const INV_DEFAULT_INTERVAL: u64 = 1800;
const INV_MIN_INTERVAL: u64 = 300;

/// runtime で Nostr が実際に鳴る条件を判定する。**`enabled = 1` を要求する**（存在だけの
/// COUNT にしない・F1）。runtime は enabled=1 の gateway だけ起動する（nostr_runner_impl.rs:94）
/// ため、移行の EXISTS 近似と**同じ判定式をテストが共有しない**ようにここで実効条件を使う。
fn agent_nostr_fires(conn: &Connection, agent_id: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM agent_nostr_config WHERE agent_id = ?1 AND enabled = 1",
        [agent_id],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// ChannelScoped の発火先 channel_id 群（実コード main.rs:326-372 の dedup を再現）。
/// `list_heartbeat_channels`（heartbeat_enabled=1）を当該 agent 向け（agent 固有 or global）に
/// 絞り、同一 channel_id では agent 固有行を global 行より優先して dedup する。
/// **whitelist ゲートは適用しない**（実コードの ChannelScoped 経路に存在しない）。
fn channelscoped_targets(conn: &Connection, agent_id: &str) -> std::collections::BTreeSet<String> {
    let all = crate::queries::list_heartbeat_channels(conn).unwrap();
    let mut selected: std::collections::HashMap<String, crate::queries::ChannelConfigRow> =
        std::collections::HashMap::new();
    for c in all {
        if !c.agent_id.is_empty() && c.agent_id != agent_id {
            continue;
        }
        match selected.get(&c.channel_id) {
            Some(existing) if !existing.agent_id.is_empty() && c.agent_id.is_empty() => continue,
            _ => {
                selected.insert(c.channel_id.clone(), c);
            }
        }
    }
    selected.into_keys().collect()
}

/// 旧システムが実際に外部へ届ける発火集合を、実コード経路どおりに計算する。
fn old_real_firing(
    conn: &Connection,
    g: bool,
    loop_agents: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeSet<(String, String)> {
    let mut out = std::collections::BTreeSet::new();
    for agent in loop_agents {
        let resolved = crate::queries::resolve_agent_heartbeat(
            conn,
            agent,
            INV_DEFAULT_INTERVAL,
            INV_MIN_INTERVAL,
        );
        // firing_plan（実 main.rs:169-183 の分岐を再現）。
        if resolved.enabled {
            // AgentScoped: 1 回発火。外部到達は Nostr gateway が**実際に鳴る**ときだけ
            // （Discord 専用・Nostr disabled は空 channel or 未起動 → 外部発火ゼロ）。
            if agent_nostr_fires(conn, agent) {
                out.insert((agent.clone(), "nostr".to_string()));
            }
        } else if g {
            // ChannelScoped: heartbeat_enabled=1 チャンネル（dedup）。whitelist ゲート無し。
            for ch in channelscoped_targets(conn, agent) {
                out.insert((agent.clone(), ch));
            }
        }
        // None（未 opt-in かつ G=false）: 発火なし。
    }
    out
}

/// 移行後に実際に外部へ届ける発火集合を、enabled=1 セッションから計算する。
/// `discord-` は G ゲート、`nostr-` は G 非依存。whitelist ゲートは現状経路に無いので掛けない。
fn new_real_firing(conn: &Connection, g: bool) -> std::collections::BTreeSet<(String, String)> {
    let mut out = std::collections::BTreeSet::new();
    for row in crate::queries::list_enabled_session_heartbeat_configs(conn).unwrap() {
        let agent = &row.agent_id;
        let sid = &row.session_id;
        if *sid == format!("nostr-{agent}") {
            out.insert((agent.clone(), "nostr".to_string()));
        } else if let Some(rest) = sid.strip_prefix(&format!("discord-{agent}-")) {
            if let Some((_guild, channel)) = rest.rsplit_once('-') {
                if g {
                    out.insert((agent.clone(), channel.to_string()));
                }
            }
        }
    }
    out
}

/// **不変条件**: 移行直後に発火するセッション集合＝移行前に実発火していた集合。
/// 旧側は実 precedence（resolve_agent_heartbeat + firing_plan + Nostr 到達 + ChannelScoped
/// dedup、whitelist ゲート無し + loop membership）で計算、新側は enabled セッションから計算。
/// `G ∈ {true, false}` の両方で一致することを見る。**期待集合は手書きしない。**
#[test]
fn v37_invariant_old_firing_equals_new_firing() {
    let conn = crate::init_memory().expect("init");
    setup_pre_v37(&conn);

    // prod を模した fixture（channel/guild は clean numeric = 正規化と同値）。
    //  optn : opt-in + Nostr enabled=1（→ AgentScoped Nostr 発火）
    //  optnn: opt-in・Nostr 無し（→ AgentScoped だが外部到達なし）
    //  plain: 未 opt-in・loop に居る（→ ChannelScoped）。global ch304 にも**明示行**を持つ
    //         （＝global fallback 経由の発火を持ち込まない＝prod と同じ状況）
    //  noloop: 未 opt-in・loop に**居ない**（→ 発火しない。prod の e2e-test 相当）
    //  optn0: opt-in・Nostr 行はあるが **enabled=0**（→ runtime で鳴らない・F1 の probe）。
    //         移行が EXISTS 近似だと nostr セッションを enabled=1 で作り新側だけ発火＝不一致。
    //  optbad: hb enabled=1 だが **interval_secs=0**（resolve が enabled:false へ倒す・F2 の
    //         probe）。resolve 意味論では未 opt-in なので ChannelScoped で Discord 発火する。
    //         移行が raw enabled=1 だと opt-in 扱いして Discord を抑止・nostr を作り不一致。
    //         ※ optbad は global ch304 にも**明示行**を持たせる（plain と同様）。持たせないと
    //         loop エージェントが global fallback 経由で ch304 に発火する状況になり、それは
    //         移行が保存できない既知の限界（enabled=0 展開）＝設計どおりの不一致になるため。
    //         prod では loop エージェントは global fallback に依存していないので、それを模す。
    conn.execute_batch(
            "INSERT INTO agents (agent_id, name, persona_name) VALUES
                ('optn','optn','optn'),('optnn','optnn','optnn'),('plain','plain','plain'),
                ('noloop','noloop','noloop'),('optn0','optn0','optn0'),('optbad','optbad','optbad');
             INSERT INTO agent_heartbeat_config (agent_id, enabled, interval_secs, updated_at) VALUES
                ('optn', 1, 18000, '2026-01-01'),
                ('optnn',1, 1200,  '2026-01-01'),
                ('optn0',1, 5000,  '2026-01-01'),
                ('optbad',1, 0,    '2026-01-01');
             INSERT INTO agent_nostr_config (agent_id, secret_key, relays_json, filter_json, enabled, updated_at) VALUES
                ('optn',  'nsec', '[]', '{}', 1, '2026-01-01'),
                ('optn0', 'nsec', '[]', '{}', 0, '2026-01-01'),
                ('optbad','nsec', '[]', '{}', 1, '2026-01-01');
             INSERT INTO discord_channel_config
                (channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, heartbeat_instructions, updated_at) VALUES
                ('300', 'optn',  '900', '', 1, 1, 1, 1, NULL, '', '2026-01-01'),
                ('301', 'optnn', '900', '', 1, 1, 1, 1, NULL, '', '2026-01-01'),
                ('302', 'plain', '900', '', 1, 1, 1, 1, NULL, '', '2026-01-01'),
                ('304', 'plain', '900', '', 1, 1, 1, 1, NULL, '', '2026-01-01'),
                ('305', 'optbad','900', '', 1, 1, 1, 1, NULL, '', '2026-01-01'),
                ('304', 'optbad','900', '', 1, 1, 1, 1, NULL, '', '2026-01-01'),
                ('304', '',      '900', '', 1, 1, 1, 1, NULL, '', '2026-01-01');",
        )
        .unwrap();

    initialize(&conn).expect("apply v37");

    // loop membership の模型（config discord agent_ids ∪ opt-in）。noloop は含めない。
    let loop_agents: std::collections::BTreeSet<String> =
        ["optn", "optnn", "plain", "optn0", "optbad"]
            .iter()
            .map(|s| s.to_string())
            .collect();

    for g in [true, false] {
        let old = old_real_firing(&conn, g, &loop_agents);
        let new = new_real_firing(&conn, g);
        assert_eq!(old, new, "移行が発火集合を変えた (G={g})");
    }
    // 非空（vacuous でない）ことを確かめる。
    assert!(
        !old_real_firing(&conn, true, &loop_agents).is_empty(),
        "fixture が発火を含むこと"
    );
    // noloop（prod の e2e-test 相当）は新側で発火しない＝移行が発火を増やしていない。
    let fires_noloop = new_real_firing(&conn, true)
        .iter()
        .any(|(a, _)| a == "noloop");
    assert!(
        !fires_noloop,
        "loop に居ないエージェントを移行が発火させない"
    );
}
