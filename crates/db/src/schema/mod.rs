use chrono::Utc;
use rusqlite::{params, Connection};

mod sql;
use sql::{
    AGENT_HEARTBEAT_CONFIG_SQL, AGENT_MCP_CONFIG_SQL, AGENT_NOSTR_CONFIG_SQL,
    AGENT_NOSTR_RELAY_CONFIG_SQL, AGENT_SCHEDULES_SQL, MEMORY_CATEGORY_MEMBERS_MM_SQL,
    MEMORY_CATEGORY_MEMBERS_SQL, PROVIDER_SETTINGS_SQL, SCHEMA_SQL, SESSION_HEARTBEAT_CONFIG_SQL,
    SKILL_USAGE_LOG_SQL, TASK_LEDGER_SQL,
};

/// スキーマのバージョン管理は `PRAGMA user_version` で行う。
///
/// 既存の冪等 `migrate()`（version 1 baseline）を凍結し、以降のスキーマ変更は
/// [`MIGRATIONS`] に番号付きで追加する。既存DBは全て `user_version = 0` なので、
/// 初回起動では baseline（`SCHEMA_SQL` + `migrate()`）を従来どおり適用してから
/// version 1 をスタンプする。以降の起動では baseline をスキップし、番号付き
/// マイグレーションのうち未適用のものだけを実行する。
const BASELINE_VERSION: i64 = 1;

/// 番号付きマイグレーション1件。
struct Migration {
    version: i64,
    #[allow(dead_code)]
    description: &'static str,
    up: fn(&Connection) -> rusqlite::Result<()>,
}

/// version 2 以降のスキーマ変更をここに追記する（version は厳密増加）。
///
/// 重要な運用ルール:
/// - `migrate()`（version 1 baseline）へは今後**追記しない**。新しい変更はここへ。
/// - `SCHEMA_SQL`（新規インストール用）にテーブル/列を足したら、既存DB
///   （baseline済み＝`SCHEMA_SQL` を再実行しない）にも届くよう、**必ず対応する
///   番号付きマイグレーションもここへ追加する**こと。忘れると既存DBだけ列が欠ける。
/// - 各 `up` は自身のトランザクション内で実行される（`run_migrations` 参照）。
///   `journal_mode`/`VACUUM` 等の非トランザクショナルな操作は `up` 内で行わない。
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 2,
        description: "task ledger: goal/contract/progress (issue #50)",
        up: |conn| conn.execute_batch(TASK_LEDGER_SQL),
    },
    Migration {
        version: 3,
        description: "trusted_discord_users.display_name (peer reviewer roster, issue #57)",
        // 新規DB は SCHEMA_SQL 側で列を持つため、column_exists でガードして冪等にする。
        // #159 (v17) で表は `trusted_users` に改名した。新規DBには旧名の表が存在しない
        // ので table_exists で先にガードする（無ければ何もしない）。
        up: |conn| {
            if !table_exists(conn, "trusted_discord_users")? {
                return Ok(());
            }
            if !column_exists(conn, "trusted_discord_users", "display_name")? {
                conn.execute_batch(
                    "ALTER TABLE trusted_discord_users ADD COLUMN display_name TEXT NOT NULL DEFAULT ''",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 4,
        description: "agent_sessions backfill from sessions.participant_ids_json (issue #37)",
        // participant の関係を agent_sessions テーブルに昇格する（#37）。
        // 既存 sessions の JSON 配列から backfill。壊れた JSON / 非文字列要素は
        // 行単位で skip（sessions 側の表示は participant_ids_json を読み続けるため
        // 情報は失われない）。INSERT OR IGNORE で再実行にも冪等。
        up: |conn| {
            let mut stmt = conn.prepare("SELECT id, participant_ids_json FROM sessions")?;
            let rows: Vec<(String, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            for (session_id, participants_json) in rows {
                let Ok(serde_json::Value::Array(ids)) =
                    serde_json::from_str::<serde_json::Value>(&participants_json)
                else {
                    // 壊れた JSON は skip（マイグレーション全体は落とさない）
                    continue;
                };
                for id in ids {
                    if let Some(agent_id) = id.as_str() {
                        conn.execute(
                            "INSERT OR IGNORE INTO agent_sessions (agent_id, session_id) VALUES (?1, ?2)",
                            rusqlite::params![agent_id, session_id],
                        )?;
                    }
                }
            }
            Ok(())
        },
    },
    Migration {
        version: 5,
        description: "memory_index_nodes: FK(parent_id, CASCADE) + CHECK(node_type) (issue #41)",
        // テーブル再構築（SQLite は既存テーブルへの FK/CHECK 追加不可）。
        // メモリインデックスは session_logs から再構築可能な派生データなので、
        // 整合しない行（不正 node_type）はコピー対象から外し、孤児 parent_id は
        // NULL に落とす（次回 rebuild で正しいツリーに戻る）。
        // FK 検査はトランザクション内で切り替え可能な defer_foreign_keys で
        // commit 時まで遅延させる（コピー順序に依存しない）。
        up: |conn| {
            // 冪等ガード: 新規DBは SCHEMA_SQL 側で FK/CHECK を持つ。
            let has_fk: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_list('memory_index_nodes')",
                [],
                |r| r.get(0),
            )?;
            if has_fk > 0 {
                return Ok(());
            }
            conn.execute_batch("PRAGMA defer_foreign_keys = ON")?;
            conn.execute_batch(
                "CREATE TABLE memory_index_nodes_new (
                    id TEXT PRIMARY KEY,
                    agent_id TEXT NOT NULL,
                    parent_id TEXT REFERENCES memory_index_nodes_new(id) ON DELETE CASCADE,
                    node_type TEXT NOT NULL CHECK (node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly')),
                    source_type TEXT NOT NULL DEFAULT 'session_log',
                    title TEXT NOT NULL,
                    summary TEXT NOT NULL,
                    start_log_id INTEGER,
                    end_log_id INTEGER,
                    source_session_id TEXT,
                    date_from TEXT,
                    date_to TEXT,
                    depth INTEGER NOT NULL DEFAULT 0,
                    child_count INTEGER NOT NULL DEFAULT 0,
                    token_count INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    short_id TEXT
                );
                INSERT INTO memory_index_nodes_new
                    (id, agent_id, parent_id, node_type, source_type, title, summary,
                     start_log_id, end_log_id, source_session_id, date_from, date_to,
                     depth, child_count, token_count, created_at, updated_at, short_id)
                    SELECT id, agent_id, parent_id, node_type, source_type, title, summary,
                           start_log_id, end_log_id, source_session_id, date_from, date_to,
                           depth, child_count, token_count, created_at, updated_at, short_id
                    FROM memory_index_nodes
                    WHERE node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly');
                UPDATE memory_index_nodes_new SET parent_id = NULL
                    WHERE parent_id IS NOT NULL
                      AND parent_id NOT IN (SELECT id FROM memory_index_nodes_new);
                DROP TABLE memory_index_nodes;
                ALTER TABLE memory_index_nodes_new RENAME TO memory_index_nodes;
                CREATE INDEX IF NOT EXISTS idx_mem_idx_agent ON memory_index_nodes(agent_id);
                CREATE INDEX IF NOT EXISTS idx_mem_idx_parent ON memory_index_nodes(agent_id, parent_id);
                CREATE INDEX IF NOT EXISTS idx_mem_idx_type ON memory_index_nodes(agent_id, node_type);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_index_nodes_short_id ON memory_index_nodes(agent_id, short_id) WHERE short_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_memory_index_nodes_source_type ON memory_index_nodes(agent_id, source_type);",
            )?;
            Ok(())
        },
    },
    Migration {
        version: 6,
        description: "task_ledger.restart_count (loop restart v1, issue #52)",
        // 新規/既存DBとも ALTER で追加する（SCHEMA_SQL 側の task_ledger ブロックは
        // TASK_LEDGER_SQL との文面パリティ制約があるため変更しない）。
        up: |conn| {
            if !column_exists(conn, "task_ledger", "restart_count")? {
                conn.execute_batch(
                    "ALTER TABLE task_ledger ADD COLUMN restart_count INTEGER NOT NULL DEFAULT 0",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 7,
        description: "memory index: keywords + rollup timestamp + node FTS (reverse lookup)",
        // キーワード逆引きと月次ロールアップの土台。新規DBも SCHEMA_SQL は触らず
        // ここで ALTER する（v6 前例）。FTS バックフィルは影テーブルが空のときだけ
        // 実行するので冪等。
        up: |conn| {
            if !column_exists(conn, "memory_index_nodes", "keywords_json")? {
                conn.execute_batch(
                    "ALTER TABLE memory_index_nodes ADD COLUMN keywords_json TEXT NOT NULL DEFAULT '[]'",
                )?;
            }
            if !column_exists(conn, "memory_index_nodes", "summary_refreshed_at")? {
                conn.execute_batch(
                    "ALTER TABLE memory_index_nodes ADD COLUMN summary_refreshed_at TEXT",
                )?;
            }
            // tokenize=trigram: 日本語は空白で区切られないため、既定の unicode61 だと
            // 文全体が 1 トークンになり部分語で当たらない。trigram は 3 文字以上の
            // 部分文字列マッチを可能にする（2 文字以下はクエリ層の LIKE フォールバック）。
            conn.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS memory_index_fts USING fts5(
                    title, summary, keywords,
                    node_id UNINDEXED, agent_id UNINDEXED, node_type UNINDEXED, source_type UNINDEXED,
                    tokenize='trigram')",
            )?;
            let fts_rows: i64 =
                conn.query_row("SELECT COUNT(*) FROM memory_index_fts", [], |r| r.get(0))?;
            if fts_rows == 0 {
                conn.execute_batch(
                    "INSERT INTO memory_index_fts (title, summary, keywords, node_id, agent_id, node_type, source_type)
                     SELECT title, summary, '', id, agent_id, node_type, source_type FROM memory_index_nodes",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 8,
        description: "provider settings overrides (dashboard-managed LLM/voice providers)",
        up: |conn| conn.execute_batch(PROVIDER_SETTINGS_SQL),
    },
    Migration {
        version: 9,
        description: "llm_provider_overrides.reasoning_effort (dashboard-editable thinking level)",
        up: |conn| {
            if !column_exists(conn, "llm_provider_overrides", "reasoning_effort")? {
                conn.execute_batch(
                    "ALTER TABLE llm_provider_overrides ADD COLUMN reasoning_effort TEXT",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 10,
        description: "agents.reasoning_effort (per-agent thinking level)",
        up: |conn| {
            if !column_exists(conn, "agents", "reasoning_effort")? {
                conn.execute_batch("ALTER TABLE agents ADD COLUMN reasoning_effort TEXT")?;
            }
            Ok(())
        },
    },
    Migration {
        version: 11,
        description: "sleep skill consolidation: skill_usage_log + agent_memory_index_config.last_skill_consolidation_at",
        up: |conn| {
            conn.execute_batch(SKILL_USAGE_LOG_SQL)?;
            // 棚卸しの最終実行時刻。SQLite の ADD COLUMN DEFAULT は定数のみで
            // datetime('now') を使えないため NULL 許容で追加し、初回シード/実行後に
            // 明示 UPSERT で now を刻む（design-sleep-skill-consolidation.md §5/§8.3）。
            if !column_exists(conn, "agent_memory_index_config", "last_skill_consolidation_at")? {
                conn.execute_batch(
                    "ALTER TABLE agent_memory_index_config ADD COLUMN last_skill_consolidation_at TEXT",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 12,
        description: "agents.web_search (per-agent URL読取り: provider native web_search/url_context)",
        up: |conn| {
            if !column_exists(conn, "agents", "web_search")? {
                conn.execute_batch("ALTER TABLE agents ADD COLUMN web_search INTEGER")?;
            }
            Ok(())
        },
    },
    Migration {
        version: 13,
        description: "agent_nostr_config (per-agent Nostr sub-gateway: 隔離鍵 + relays + filter)",
        up: |conn| conn.execute_batch(AGENT_NOSTR_CONFIG_SQL),
    },
    Migration {
        version: 14,
        description: "agent_mcp_config (per-agent MCP サーバ: command/args/env, 1エージェント複数)",
        up: |conn| conn.execute_batch(AGENT_MCP_CONFIG_SQL),
    },
    Migration {
        version: 15,
        description: "llm_provider_overrides に起動系（binary_path/args_json/working_dir/timeout_secs）を追加",
        up: |conn| {
            for (col, ty) in [
                ("binary_path", "TEXT"),
                ("args_json", "TEXT"),
                ("working_dir", "TEXT"),
                ("timeout_secs", "INTEGER"),
            ] {
                if !column_exists(conn, "llm_provider_overrides", col)? {
                    conn.execute_batch(&format!(
                        "ALTER TABLE llm_provider_overrides ADD COLUMN {col} {ty}"
                    ))?;
                }
            }
            Ok(())
        },
    },
    Migration {
        version: 16,
        description: "trusted_discord_users.platform (信頼済みユーザーの識別子空間を経路で分ける, issue #214)",
        // 列追加のみ（ほぼ可逆）。既存行は全て Discord の識別子空間なので DEFAULT 'discord'
        // で生かす。一意制約 (discord_user_id, agent_id) はここでは触らない
        // （変更するとテーブル再構築＝非可逆になるため #159 に合流させる）。
        // 新規DB は SCHEMA_SQL 側で列を持つため column_exists でガードして冪等にする（v3 前例）。
        // #159 (v17) で表は `trusted_users` に改名したので、v3 と同様 table_exists で
        // 旧名の表の有無を先に見る（新規DBには無いので何もしない）。
        up: |conn| {
            if !table_exists(conn, "trusted_discord_users")? {
                return Ok(());
            }
            if !column_exists(conn, "trusted_discord_users", "platform")? {
                conn.execute_batch(
                    "ALTER TABLE trusted_discord_users ADD COLUMN platform TEXT NOT NULL DEFAULT 'discord'",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 17,
        description:
            "trusted_discord_users → trusted_users / discord_user_id → user_id (Discord 命名の解消, issue #159)",
        // **信頼済みユーザーの行は 1 件も失わない。** 改名で移送するのが基本で、唯一の
        // 例外は #479 の「空の新表を DROP してから改名」だが、DROP するのは行ゼロの表だけ。
        //
        // `ALTER TABLE ... RENAME TO` と `ALTER TABLE ... RENAME COLUMN` は
        // テーブルの再構築を伴わない（SQLite が sqlite_schema の DDL 文字列を
        // 書き換えるだけ）ので、行はそのまま生き、**逆向きの RENAME で戻せる**
        // ＝可逆。一意制約 `(user_id, agent_id)` の作り直し（→ `(platform, user_id,
        // agent_id)`）は再構築が要る非可逆な変更なので、ここには**混ぜない**。
        //
        // 冪等性: 新規DB は SCHEMA_SQL 側で既に新しい名前なので、どの分岐も走らない。
        // 版付き旧DB では run_migrations が version>17 で二度と呼ばず、本番（version=38）は
        // baseline も通らない（下記 #479 分岐が本番を触ることはない）。
        up: |conn| {
            if table_exists(conn, "trusted_discord_users")? {
                if !table_exists(conn, "trusted_users")? {
                    // 通常の昇格経路（版付き旧DB）: 新表がまだ無いので単純に改名する。
                    conn.execute_batch(
                        "ALTER TABLE trusted_discord_users RENAME TO trusted_users",
                    )?;
                } else if !table_has_rows(conn, "trusted_users")? {
                    // #479: 版管理導入前（user_version<1）の旧DBは baseline 経路を通り、
                    // 先に SCHEMA_SQL が **空の** trusted_users を作る。そのため上の
                    // `!table_exists` ガードが false になって改名が skip され、旧表に
                    // データが取り残されていた（クラッシュしないので気づけない）。
                    // 空の新表を DROP してから改名でデータを移す。**空表の DROP は
                    // 行を 1 件も消さない**ので、通常経路（新表にデータあり）は下の else で
                    // 一切触らず保護される（設計上の安全条件）。
                    conn.execute_batch(
                        "DROP TABLE trusted_users;
                         ALTER TABLE trusted_discord_users RENAME TO trusted_users",
                    )?;
                }
                // else: 新表に既にデータがある = 既に正しく昇格済み。この並存は通常経路では
                // 起きないが、起きても実データを持つ新表は壊さず、旧表にも触れない（冪等・保全優先）。
            }
            if column_exists(conn, "trusted_users", "discord_user_id")? {
                conn.execute_batch(
                    "ALTER TABLE trusted_users RENAME COLUMN discord_user_id TO user_id",
                )?;
            }
            // インデックスは表に追従して残る（名前は旧いまま）。索引は行を持たない
            // 派生物なので、旧名を落として新名で貼り直す。
            conn.execute_batch(
                "DROP INDEX IF EXISTS idx_trusted_discord_users_agent;
                 CREATE INDEX IF NOT EXISTS idx_trusted_users_agent ON trusted_users(agent_id);",
            )?;
            Ok(())
        },
    },
    Migration {
        version: 18,
        description: "trusted_users.permission: 'co_agent' → 'co-agent' (権限表記の統一, issue #234)",
        // **既存行の書き換えのみ。行の追加・削除はしない。**
        //
        // 権限は列挙型になり、DB へ入る表記はケバブケースに統一した
        // （`queries::TrustedUserPermission`）。旧い表記 `co_agent` の行は、そのままだと
        // 読み出しで「未知の値 → ただの信頼済みユーザー」へ落ちて協働エージェントの
        // 権限を失う。**判定結果を変えないため**にここで表記だけを移す。
        //
        // 触るのは `co_agent` に完全一致する行だけ。`coagent` のような別の綴りは
        // 従来も協働エージェントとして扱われていなかった（判定は完全一致）ので、
        // ここで拾うと**権限が増える**方向の変更になる。拾わない。
        //
        // 冪等性: 2 回目以降は WHERE に一致する行が無いので 0 行更新。
        //
        // 可逆性（データ）: 逆向きの UPDATE（`'co-agent'` → `'co_agent'`）で行の内容は
        // 完全に戻せる。落ちる情報は無い。
        //
        // 切り戻し（運用）: **データを戻すだけでは古いバイナリは起動しない。**
        // `PRAGMA user_version` が 18 のままだと、起動時の版チェック（`run_migrations`）が
        // 「DB の版がこのバイナリの対応版より新しい」と判断してハードエラーで止まる。
        // バイナリを戻すときは版番号も 1 つ前（17）へ戻すこと:
        //
        //   BEGIN;
        //   UPDATE trusted_users SET permission = 'co_agent' WHERE permission = 'co-agent';
        //   PRAGMA user_version = 17;
        //   COMMIT;
        //
        // サーバを停止した状態で実施する（起動中の接続と競合させない）。
        up: |conn| {
            if !table_exists(conn, "trusted_users")? {
                return Ok(());
            }
            conn.execute(
                "UPDATE trusted_users SET permission = 'co-agent' WHERE permission = 'co_agent'",
                [],
            )?;
            Ok(())
        },
    },
    Migration {
        version: 19,
        description: "agent_nostr_relay_config (Nostr 受信を Discord へ転記する宛先, issue #252)",
        // **表の新設のみ。既存の表・行には一切触れない。**
        //
        // 既定は**無効**（`enabled INTEGER NOT NULL DEFAULT 0`）。行を作っただけで転記が
        // 始まらないよう fail-closed に倒す（#240 の轍）。行が無いエージェントも無効として
        // 扱う（`opencrab_actions::webhook_target::resolve_nostr_relay_webhook` が fail-closed）。
        //
        // 冪等性: `CREATE TABLE IF NOT EXISTS`。2 回目以降は no-op。
        //
        // 切り戻し: 表を落とすだけで元に戻る（失われるのはこの表の行だけ）。古いバイナリへ
        // 戻すときは版番号も戻すこと:
        //
        //   BEGIN;
        //   DROP TABLE IF EXISTS agent_nostr_relay_config;
        //   PRAGMA user_version = 18;
        //   COMMIT;
        up: |conn| conn.execute_batch(AGENT_NOSTR_RELAY_CONFIG_SQL),
    },
    Migration {
        version: 20,
        description: "agent_heartbeat_config (エージェント単位のハートビート有効/間隔, issue #247)",
        // **表の新設のみ。既存の表・行には一切触れない。**
        //
        // チャンネル単位の設定（`discord_channel_config.heartbeat_enabled` /
        // `heartbeat_interval_secs`）は**そのまま残す**。発火の判定をどちらから引くかの
        // 切り替えは段階 3（別 issue）で、この版では「エージェントが自分の設定を持てる」
        // ところまでしか進めない。
        //
        // 既定は**無効**（`enabled INTEGER NOT NULL DEFAULT 0`）。チャンネル設定は
        // 既定が有効で「行を作っただけで自律実行が始まる」形になっていた（#240）ので、
        // 同じ轍を踏まないよう逆にする。行が無いエージェントも無効として扱う
        // （`queries::resolve_agent_heartbeat` が fail-closed）。
        //
        // 冪等性: `CREATE TABLE IF NOT EXISTS`。2 回目以降は no-op。
        //
        // 切り戻し: 表を落とすだけで元に戻る（失われるのはこの表の行だけ）。
        // v19 の doc と同じく、古いバイナリへ戻すときは版番号も戻すこと:
        //
        //   BEGIN;
        //   DROP TABLE IF EXISTS agent_heartbeat_config;
        //   PRAGMA user_version = 19;
        //   COMMIT;
        up: |conn| conn.execute_batch(AGENT_HEARTBEAT_CONFIG_SQL),
    },
    Migration {
        version: 21,
        description: "impressions: UNIQUE(agent_id, session_id, target_id) → UNIQUE(agent_id, target_id)（人物像を agent スコープへ, issue #314）",
        // **一意制約の付け替えのみ。列は 1 つも増減しない。**
        //
        // 人物像は「同じ人は同じ人」なので、Discord と Nostr で話しても同じ 1 行を
        // 見るべきだった。旧制約はセッション毎に別レコードを作るため、経路が増えると
        // 必ず分断する（#314）。
        //
        // `session_id` 列は**残す**。スコープからは外れるが「最後にどのセッションで
        // 更新されたか」は時系列を辿る手掛かりとして意味があり、落とすと復元できない。
        //
        // 統合方針（同一 (agent_id, target_id) が複数セッションにある場合）:
        // - **`updated_at` が最新の行を残す**（同着は rowid が大きい方＝後に入った方）。
        // - `created_at` だけは統合対象の**最小値**を引き継ぐ（「いつからの知り合いか」を
        //   落とさない）。それ以外の列は勝った行の値をそのまま使う。テキストの機械的な
        //   結合はしない（人物像の中身に手を入れないため）。
        // - 重複が無ければ全行がそのまま残る（本番データはこれに該当）。
        //
        // 一意制約の変更はテーブル再構築が要る（`ALTER TABLE` では付け替えられない）。
        // 索引 `idx_impressions_session` は再構築で消える。読み出しが agent スコープに
        // なり (agent_id, session_id) を引かなくなるので貼り直さない
        // （UNIQUE(agent_id, target_id) の索引が agent_id 前方一致を賄う）。
        //
        // 冪等性: **新しい制約が既にあるときだけ** no-op（肯定形の判定）。新規DB は
        // SCHEMA_SQL 側で既に `UNIQUE(agent_id, target_id)` を持つので何もしない。
        //
        // 判定は `pragma_index_list` / `pragma_index_info` で**実際の索引の列を見る**
        // （v5 の `pragma_foreign_key_list`・v3 の `column_exists` と同じ流儀）。
        // `sqlite_master.sql` の文字列一致に頼ると、空白・大文字小文字・列順など表記の
        // 揺れで判定が外れる。外れ方が「旧制約のまま `user_version = 21` がスタンプ
        // される」方向だと、`upsert_impression` の `ON CONFLICT(agent_id, target_id)` が
        // 以後**毎回**失敗し、版が進んでいるので再起動しても直らない。肯定形なら
        // 判定が外れても「再構築が走る」側に倒れる（再構築は冪等）。
        //
        // 切り戻し: 統合で落ちた重複行は戻らない（重複が無ければ完全に可逆）。古い
        // バイナリへ戻すときは旧制約で再構築し直し、版番号も戻すこと。
        up: |conn| {
            // `(agent_id, target_id)` ちょうど 2 列の UNIQUE 索引があれば移行済み。
            // `id TEXT PRIMARY KEY` の自動索引は 1 列なので列数の条件で外れる。
            let already_migrated: i64 = conn.query_row(
                r#"SELECT COUNT(*) FROM pragma_index_list('impressions') il
                    WHERE il."unique" = 1
                      AND (SELECT COUNT(*) FROM pragma_index_info(il.name)) = 2
                      AND (SELECT COUNT(*) FROM pragma_index_info(il.name) ii
                            WHERE ii.name IN ('agent_id', 'target_id')) = 2"#,
                [],
                |r| r.get(0),
            )?;
            if already_migrated > 0 {
                return Ok(());
            }
            conn.execute_batch(
                "CREATE TABLE impressions_new (
                    id TEXT PRIMARY KEY,
                    agent_id TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    target_id TEXT NOT NULL,
                    target_name TEXT NOT NULL,
                    personality TEXT DEFAULT '',
                    communication_style TEXT DEFAULT '',
                    recent_behavior TEXT DEFAULT '',
                    agreement TEXT DEFAULT '中立',
                    notes TEXT DEFAULT '',
                    last_updated_turn INTEGER DEFAULT 0,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    UNIQUE(agent_id, target_id)
                );
                INSERT INTO impressions_new
                    (id, agent_id, session_id, target_id, target_name, personality,
                     communication_style, recent_behavior, agreement, notes,
                     last_updated_turn, created_at, updated_at)
                    SELECT o.id, o.agent_id, o.session_id, o.target_id, o.target_name,
                           o.personality, o.communication_style, o.recent_behavior,
                           o.agreement, o.notes, o.last_updated_turn,
                           (SELECT MIN(m.created_at) FROM impressions m
                             WHERE m.agent_id = o.agent_id AND m.target_id = o.target_id),
                           o.updated_at
                      FROM impressions o
                     WHERE o.rowid = (
                           SELECT w.rowid FROM impressions w
                            WHERE w.agent_id = o.agent_id AND w.target_id = o.target_id
                            ORDER BY w.updated_at DESC, w.rowid DESC
                            LIMIT 1);
                DROP TABLE impressions;
                ALTER TABLE impressions_new RENAME TO impressions;",
            )?;
            Ok(())
        },
    },
    Migration {
        version: 22,
        description: "agent_nostr_config.owner_pubkey (Nostr のオーナー識別子, issue #319)",
        // **列追加のみ。既存の表・行の内容には一切触れない。**
        //
        // Discord は per-agent 設定に `agent_discord_config.owner_discord_id` を持ち、
        // 発言者がオーナーかをそこで判定している。Nostr には対応する置き場所が無く、
        // 受信ターンの呼び出し元が一律 `Agent` に固定されていた（#319）。同じ形にする
        // ための列で、**既定は空文字＝オーナー未設定**（誰もオーナーにならない /
        // `opencrab_core::owner::is_owner_id` の fail-closed）。列を足しただけでは
        // どのエージェントの挙動も変わらない。
        //
        // 表現は **64 桁小文字 hex に正規化して保存する**（Nostr 受信イベントの
        // `pubkey` が hex なので、比較の基準を受信側に合わせる）。入口
        // （`configure_nostr` / REST）が npub でも hex でも受け取って正規化するため、
        // この列に npub が入ることは無い。
        //
        // 冪等性: 新規DB は `SCHEMA_SQL` 側で列を持つので `column_exists` でガードする
        // （v12 / v16 の前例）。2 回目以降は no-op。
        //
        // 切り戻し: 列は読まれなくなるだけで既存の行は壊れない。古いバイナリへ戻すときは
        // 版番号を戻すこと（列はそのままで良い）:
        //
        //   BEGIN;
        //   PRAGMA user_version = 21;
        //   COMMIT;
        up: |conn| {
            if !column_exists(conn, "agent_nostr_config", "owner_pubkey")? {
                conn.execute_batch(
                    "ALTER TABLE agent_nostr_config ADD COLUMN owner_pubkey TEXT NOT NULL DEFAULT ''",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 23,
        description: "memory index: node_type に 'category'/'meta' を追加 + カテゴリ層メンバー表 (issue #313)",
        // **CHECK 制約の拡張（許可値を増やす）＋ 参照表の新設のみ。既存の行・列は保持する。**
        //
        // 背景: `memory_index_nodes.node_type` には CHECK 制約があり、`'category'` /
        // `'meta'` は許可集合に無かった（`crates/db/src/schema.rs` の SCHEMA_SQL / v5）。
        // 加えて `insert_index_node` は `INSERT OR IGNORE` なので、CHECK 違反は**エラーに
        // ならず黙って無視される**（＝カテゴリノードを作ったつもりで消える）。SQLite は
        // CHECK を `ALTER` で広げられないため、v5 / v21 と同じ**テーブル再構築**で許可値を
        // 2 つ足す。全行を無条件コピーするので時系列ツリー（period/session/topic/daily）は
        // 無傷。孤児 parent_id は NULL に落とす（v5 の流儀）。
        //
        // カテゴリと topic の紐付けは `memory_category_members`（参照表）で持つ。parent 軸を
        // 使わないので topic は session 親を保持し、日付から辿る道が切れない（#313 要件）。
        //
        // 冪等性（極性は v21 と同じ「肯定形」だが、機構は v21 とは逆になる点に注意）:
        // v21 は `sqlite_master.sql` の文字列一致を**避け**て `pragma_index_list` /
        // `pragma_index_info` で索引の構造を見た（空白・大小・列順の表記揺れで判定が
        // 外れないため）。一方ここで見たいのは索引ではなく **`node_type` の CHECK 制約の
        // 許可値**で、CHECK は `pragma_table_info` 等の構造 pragma では取り出せない。よって
        // 已むを得ず `sqlite_master.sql`（テーブル定義 SQL）の文字列判定を採る。v21 が退けた
        // 方式そのものだが、判定対象が「索引の列」ではなく「CHECK に現れるリテラル」なので
        // 表記揺れの当たり方が違う（下記の安全性を参照）。
        //
        // 安全性: 現行スキーマの CHECK に `'category'` と `'meta'` の**両方**が現れるときだけ
        // 再構築を skip する。危険な外れ方は「まだ狭いのに広いと誤判定して skip する」方向
        // だが、狭い CHECK のテーブル SQL に `'category'`/`'meta'` の文字列が現れる余地は無い
        // （列名・既定値・他のどの CHECK にも含まれない）ので、この誤判定は起こり得ない。
        // 逆に「広いのに狭いと誤判定して再構築する」方向へ外れても再構築は冪等なので無害
        // （肯定形の利点）。新規DB は SCHEMA_SQL 側で既に広い CHECK を持つので再構築されない。
        //
        // 切り戻し（データは可逆・古いバイナリは版番号も戻すこと）: category/meta ノードと
        // member 行は sleep 中に作られる派生データなので、削除すれば原状復帰する。
        //   BEGIN;
        //   DELETE FROM memory_index_nodes WHERE node_type IN ('category','meta');
        //   DROP TABLE IF EXISTS memory_category_members;
        //   -- （厳密に旧 CHECK へ戻すなら v5 と同型の再構築で狭める）
        //   PRAGMA user_version = 22;
        //   COMMIT;
        up: |conn| {
            let widened: bool = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name='memory_index_nodes'",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .map(|sql| sql.contains("'category'") && sql.contains("'meta'"))
                .unwrap_or(false);
            if !widened {
                conn.execute_batch("PRAGMA defer_foreign_keys = ON")?;
                conn.execute_batch(
                    "CREATE TABLE memory_index_nodes_new (
                        id TEXT PRIMARY KEY,
                        agent_id TEXT NOT NULL,
                        parent_id TEXT REFERENCES memory_index_nodes_new(id) ON DELETE CASCADE,
                        node_type TEXT NOT NULL CHECK (node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly','category','meta')),
                        source_type TEXT NOT NULL DEFAULT 'session_log',
                        title TEXT NOT NULL,
                        summary TEXT NOT NULL,
                        start_log_id INTEGER,
                        end_log_id INTEGER,
                        source_session_id TEXT,
                        date_from TEXT,
                        date_to TEXT,
                        depth INTEGER NOT NULL DEFAULT 0,
                        child_count INTEGER NOT NULL DEFAULT 0,
                        token_count INTEGER NOT NULL DEFAULT 0,
                        created_at TEXT NOT NULL,
                        updated_at TEXT NOT NULL,
                        short_id TEXT,
                        keywords_json TEXT NOT NULL DEFAULT '[]',
                        summary_refreshed_at TEXT
                    );
                    INSERT INTO memory_index_nodes_new
                        (id, agent_id, parent_id, node_type, source_type, title, summary,
                         start_log_id, end_log_id, source_session_id, date_from, date_to,
                         depth, child_count, token_count, created_at, updated_at, short_id,
                         keywords_json, summary_refreshed_at)
                        SELECT id, agent_id, parent_id, node_type, source_type, title, summary,
                               start_log_id, end_log_id, source_session_id, date_from, date_to,
                               depth, child_count, token_count, created_at, updated_at, short_id,
                               keywords_json, summary_refreshed_at
                        FROM memory_index_nodes;
                    UPDATE memory_index_nodes_new SET parent_id = NULL
                        WHERE parent_id IS NOT NULL
                          AND parent_id NOT IN (SELECT id FROM memory_index_nodes_new);
                    DROP TABLE memory_index_nodes;
                    ALTER TABLE memory_index_nodes_new RENAME TO memory_index_nodes;
                    CREATE INDEX IF NOT EXISTS idx_mem_idx_agent ON memory_index_nodes(agent_id);
                    CREATE INDEX IF NOT EXISTS idx_mem_idx_parent ON memory_index_nodes(agent_id, parent_id);
                    CREATE INDEX IF NOT EXISTS idx_mem_idx_type ON memory_index_nodes(agent_id, node_type);
                    CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_index_nodes_short_id ON memory_index_nodes(agent_id, short_id) WHERE short_id IS NOT NULL;
                    CREATE INDEX IF NOT EXISTS idx_memory_index_nodes_source_type ON memory_index_nodes(agent_id, source_type);",
                )?;
            }
            conn.execute_batch(MEMORY_CATEGORY_MEMBERS_SQL)?;
            Ok(())
        },
    },
    Migration {
        version: 24,
        description: "skills.created_caller: 作成時 caller の trust class を記録 (issue #335 / #347 / #349)",
        // **列追加のみ。既存の表・行の内容には一切触れない。**
        //
        // #335（confused deputy 塞ぎ）で skills に作成時 caller の trust class
        // （'owner' / 'trusted' / 'agent'）を持たせ、read_skill が「このターンの caller が
        // 作成 caller を超えるなら本文を渡さない」でゲートする。NULL 許容で追加するため
        // 既存行は NULL のまま = legacy grandfather（Owner 相当）として従来どおり読める。
        // バックフィルはしない（既存スキルの本来の作成 caller は復元できないが、NULL→Owner
        // 扱いで壊さない。新規作成分は実 caller を記録して穴を塞ぐ）。
        //
        // #349: 当初この列追加は凍結された `migrate()` の guarded ALTER として書かれたが、
        // `migrate()` は新規 DB（`user_version < BASELINE_VERSION`）でしか呼ばれず、本番の
        // 既存 DB（`user_version = 23`）には効かず全スキル SELECT が `no such column` で落ちた。
        // 既存 DB に届かせるため番号付きマイグレーションへ移す。
        //
        // 冪等性: 新規 DB は `SCHEMA_SQL` の `CREATE TABLE skills` 側で列を持つので
        // `column_exists` でガードする（v12 / v16 / v22 の前例）。2 回目以降は no-op。
        //
        // 切り戻し: 列は読まれなくなるだけで既存の行は壊れない。古いバイナリへ戻すときは
        // 版番号を戻すこと（列はそのままで良い）:
        //
        //   BEGIN;
        //   PRAGMA user_version = 23;
        //   COMMIT;
        up: |conn| {
            if !column_exists(conn, "skills", "created_caller")? {
                conn.execute_batch("ALTER TABLE skills ADD COLUMN created_caller TEXT")?;
            }
            Ok(())
        },
    },
    Migration {
        version: 25,
        description: "skills.agent_visible: caller=Agent のターンへ露出してよいかの許可列 (issue #352)",
        // **列追加のみ。既存の表・行の内容には一切触れない。**
        //
        // #352: caller=Agent のターン（素の Agent 権限で走る run。外部 Nostr の受信ターンが
        // 典型例だが、判定軸は transport ではなく caller=Agent）には、許可した skill 以外を
        // index にも出さず read_skill の本文も渡さない。その許可を持たせる列。
        //
        // **既定 0（fail-closed）** で追加する。NOT NULL DEFAULT 0 なので既存の全行は自動的に
        // 0 = 「Agent には見せない」になる（＝オーナーが REST で 1 を立てるまで 1 件も見えない）。
        // Owner / CoAgent / TrustedUser の見え方は不変（絞りは caller=Agent のみ）。
        //
        // #349 の罠を踏まないため **番号付き MIGRATIONS へ置く**（凍結された `migrate()` は
        // 新規 DB でしか走らず、本番の既存 DB には届かない）。
        //
        // 冪等性: 新規 DB は `SCHEMA_SQL` の `CREATE TABLE skills` 側で列を持つので
        // `column_exists` でガードする（v24 の前例）。2 回目以降は no-op。
        //
        // 切り戻し: 列は読まれなくなるだけで既存の行は壊れない。古いバイナリへ戻すときは
        // 版番号を戻すこと（列はそのままで良い）:
        //
        //   BEGIN;
        //   PRAGMA user_version = 24;
        //   COMMIT;
        up: |conn| {
            if !column_exists(conn, "skills", "agent_visible")? {
                conn.execute_batch(
                    "ALTER TABLE skills ADD COLUMN agent_visible INTEGER NOT NULL DEFAULT 0",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 26,
        description: "記憶の分類レイヤを白紙化 + memory_category_members を多対多 PK へ (issue #358)",
        // **分類レイヤ（#344 の単一ラベル sticky 割当）を破棄し、タグを多対多にする。**
        // 段階1（#313 の設計・2026-08-03 確定）。ここでは既存データの破棄と PK 変更のみで、
        // タグ道具も整理ランも足さない（段階2以降）。
        //
        // やること:
        //  (1) `memory_index_nodes` の `node_type IN ('category','meta')` を削除。
        //      #344 が 12 件ずつ LLM に単一ラベルを sticky に割り当てて作った派生ノード。
        //      仕組みごと作り直すので破棄する（#346 で既に生成は停止済み）。
        //  (2) `memory_category_members` を PK `(agent_id, topic_id)`（1 topic = 高々 1
        //      category）から **`(agent_id, topic_id, category_id)`** へ作り直す。1 topic は
        //      複数の関心にまたがるのでタグは複数付けられる必要がある。旧行（本番 1,350 件）は
        //      どうせ白紙化するので DROP+CREATE で作り直すのが素直。
        //
        // **絶対に触らないもの**（#358 受け入れ条件）:
        //  - `memory_curated` の全行（特に `long_term/*` の記憶本文）。この表は参照しない。
        //  - 時系列ツリー: `node_type` が root/period/session/topic/daily/hourly/weekly/
        //    monthly/yearly のノード。DELETE は category/meta にしか当たらない。
        //  - topic の `keywords_json` 等の付随データ。
        //  - `node_type` の CHECK。category/meta は許可集合に残す（段階2でタグとして使い直す）。
        //    ＝ 本移行はテーブル再構築で CHECK を狭めたりしない。
        //
        // FTS 整合: `insert_index_node` は全 node_type を `memory_index_fts` へ入れる。
        // 生 SQL の `DELETE FROM memory_index_nodes` は FTS 孤児を残す（同ファイルの
        // `delete_index_node` の警告参照）ので、**先に category/meta の FTS 行を消してから**
        // ノードを消す。category/meta は parent 軸を使わない葉ノードなので CASCADE の子は無い。
        //
        // 冪等性（肯定形。v23 と同じ流儀）: members の PK に `category_id` が含まれるかを
        // `pragma_table_info.pk` で見て、既に多対多なら作り直しを skip する。新規 DB は
        // SCHEMA_SQL 側で既に多対多 PK なので再構築されない。DELETE 2 本は 2 回目は 0 件。
        //
        // 切り戻し（削除した分類データは #344 の sleep が作る派生なので再生成可能。古い
        // バイナリへ戻すなら版番号も戻すこと）:
        //   BEGIN;
        //   -- （厳密に旧 PK へ戻すなら (agent_id, topic_id) で同型に作り直す）
        //   PRAGMA user_version = 25;
        //   COMMIT;
        up: |conn| {
            // (1) 分類ノードを FTS ごと削除（時系列ツリーには当たらない）。
            conn.execute_batch(
                "DELETE FROM memory_index_fts
                     WHERE node_id IN (
                         SELECT id FROM memory_index_nodes WHERE node_type IN ('category','meta')
                     );
                 DELETE FROM memory_index_nodes WHERE node_type IN ('category','meta');",
            )?;
            // (2) members を多対多 PK へ作り直す（旧行は白紙化されるので破棄）。
            let already_multi: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('memory_category_members')
                         WHERE name = 'category_id' AND pk > 0",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .map(|c| c == 1)
                .unwrap_or(false);
            if !already_multi {
                conn.execute_batch(MEMORY_CATEGORY_MEMBERS_MM_SQL)?;
            }
            Ok(())
        },
    },
    Migration {
        version: 27,
        description:
            "agent_memory_index_config.last_organize_at: スリープ整理ランのマーカー列 (issue #313 段階3 / #361)",
        // **列追加のみ。既存の表・行の内容には一切触れない。**
        //
        // #313 段階3（#361）の整理ランが「前回いつ走ったか」を刻むマーカー。
        // `last_skill_consolidation_at`（v22）と同型。用途は 2 つ:
        //  (1) 日次ゲート（`now - last_organize_at >= 間隔`）
        //  (2) bounded worklist の下端（このマーカー以降に作られた topic だけを整理対象に）
        //
        // **NULL 既定 = 未実行**。整理ラン側は NULL のとき「初回遭遇」として `now` を
        // シードするだけで走らない（既存の全 topic を一気に対象化しない）。config 既定オフ
        // なので有効化するまでこの列は書かれない。
        //
        // #349 の罠を踏まないため **番号付き MIGRATIONS へ置く**（凍結された `migrate()` は
        // 新規 DB でしか走らず、本番の既存 DB には届かない）。
        //
        // 冪等性: 新規 DB は `SCHEMA_SQL` の `CREATE TABLE agent_memory_index_config` 側で
        // 列を持つので `column_exists` でガードする（v24 / v25 の前例）。2 回目以降は no-op。
        //
        // 切り戻し: 列は読まれなくなるだけで既存の行は壊れない。古いバイナリへ戻すときは
        // 版番号を戻すこと（列はそのままで良い）:
        //
        //   BEGIN;
        //   PRAGMA user_version = 26;
        //   COMMIT;
        up: |conn| {
            if !column_exists(conn, "agent_memory_index_config", "last_organize_at")? {
                conn.execute_batch(
                    "ALTER TABLE agent_memory_index_config ADD COLUMN last_organize_at TEXT",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 28,
        description:
            "agent_memory_index_config.organize_backlog_cursor: 過去分の遡り消化マーカー (issue #313 段階3b / #365)",
        // **列追加のみ。既存の表・行の内容には一切触れない。**
        //
        // #313 段階3b（#365）: 段階3 の整理ランは有効化時にマーカー（`last_organize_at`）を
        // `now` へ置くため、**有効化以前の過去 topic には永久にタグが付かない**（本番 6,551 件）。
        // オーナー判断「古い分も少しずつ消化する」に応え、日次の枠に過去分を N 件混ぜる。
        //
        // 過去分の消化は**新規側とは独立した進捗マーカー**（軸）が要る。`last_organize_at` は
        // 新規側（前進 / 昇順）なので**混ぜない**。この列は**遡り側（後退 / 降順）**の位置を
        // 刻む複合カーソル `"{created_at}|{id}"` で、有効化時の境界（`now`）から古い方向へ、
        // 「どこまで遡ったか」を記録する。「タグが付いていない」を判定条件にすると意図的に
        // 付けなかった topic を毎回拾い直すため、**位置マーカー**で進める（一期一会の尊重）。
        //
        // **NULL 既定 = 未シード**。整理ラン側は初回遭遇（`last_organize_at` が NULL）の
        // タイミングで両マーカーを `now` にシードする。config 既定オフなので有効化するまで
        // この列は書かれない。
        //
        // #349 の罠を踏まないため **番号付き MIGRATIONS へ置く**（凍結された `migrate()` は
        // 新規 DB でしか走らず、本番の既存 DB（現在 v27）には届かない）。
        //
        // 冪等性: 新規 DB は `SCHEMA_SQL` の `CREATE TABLE` 側で列を持つので `column_exists`
        // でガードする（v24 / v25 / v27 の前例）。2 回目以降は no-op。
        //
        // 切り戻し: 列は読まれなくなるだけで既存の行は壊れない。古いバイナリへ戻すときは
        // 版番号を戻すこと（列はそのままで良い）:
        //
        //   BEGIN;
        //   PRAGMA user_version = 27;
        //   COMMIT;
        up: |conn| {
            if !column_exists(conn, "agent_memory_index_config", "organize_backlog_cursor")? {
                conn.execute_batch(
                    "ALTER TABLE agent_memory_index_config ADD COLUMN organize_backlog_cursor TEXT",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 29,
        description:
            "agent_memory_index_config.organize_last_run_at: 整理ランの日次 throttle 用刻時 (issue #313 段階3b / #365)",
        // **列追加のみ。既存の表・行の内容には一切触れない。**
        //
        // #365 レビュー修正: 段階3b 初版は「新規 0 件の日」に新規側マーカー（`last_organize_at`）を
        // 壁時計 `now` へ前進させて日次 throttle を保っていた。しかし増分ビルドは topic 挿入と
        // watermark 更新が別ロック・非トランザクション（`memory_maintenance.rs` はビルドの Err を
        // warn で握って同 tick の整理ランまで進む）。その隙に **commit 済みだが `end_log_id >
        // watermark`（snapshot 外）の topic** があると `count_organize_topics` から漏れて
        // 新規 0 と判定され、`now` へ飛ばした新規側カーソルがその topic の `created_at` を追い越し、
        // watermark 追従後も新規側から拾えず遡り境界より新しいので遡り側からも届かず**恒久ロス**
        // （#364 blocker と同型）。
        //
        // 対処: 新規側カーソルは**実際に提示した新規 topic の位置**にしか進めない（0 件なら据え置き）。
        // 壁時計へは飛ばさない。ただしそれだと「静かな日/エージェント」で throttle 基準が過去へ
        // 留まり tick 毎起動になるため、**日次 throttle 専用の壁時計刻時**をこの列に分離する
        // （clean 完了ごとに `now` を刻む）。位置（新規/遡りの 2 軸カーソル）と時刻（この列）を
        // 別に持つことで両立させる。列を増やさずには両立できない（安全な位置前進は静かな日に
        // 過去へ退き、壁時計前進は上記の恒久ロスを生む）と判断した。
        //
        // **NULL 既定 = 未刻**。整理ラン側は初回遭遇（`last_organize_at` が NULL）で 3 マーカーを
        // 同時に `now` へシードする。移行 DB（段階3/3b で先に有効化）で本列だけ NULL の場合は
        // 日次ゲートが `last_organize_at` の created_at 部へフォールバックする（旧挙動）。config
        // 既定オフなので有効化するまで書かれない。
        //
        // #349 の罠を踏まないため **番号付き MIGRATIONS へ置く**。冪等性は `column_exists` ガード。
        //
        // 切り戻し: 列は読まれなくなるだけ。版番号を戻すこと（列はそのままで良い）:
        //
        //   BEGIN;
        //   PRAGMA user_version = 28;
        //   COMMIT;
        up: |conn| {
            if !column_exists(conn, "agent_memory_index_config", "organize_last_run_at")? {
                conn.execute_batch(
                    "ALTER TABLE agent_memory_index_config ADD COLUMN organize_last_run_at TEXT",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 30,
        description: "memory index: node_type に 'unit' を追加（記憶の単位・宣言ノード用 / issue #379 #376）",
        // **CHECK 制約の拡張（許可値を 1 つ増やす）のみ。既存の行・列は 1 行も失わない。**
        //
        // 背景（#376 段階1）: エージェントが自分の生ログの範囲 `[from_id, to_id]` を「1 つの
        // 記憶」として宣言する道具（`record_memory_unit`）を足す。宣言ノードは既存の time-series
        // topic（`node_type='topic'`, `source_type='session_log'`）と**構造的に混ざらない**よう、
        // 別 `node_type='unit'` として載せる（`source_type='declared'` も併記して表示で区別する）。
        //
        // なぜ `node_type='unit'`（`source_type='declared'` 案ではなく）: 監査（#379）で、
        // time-series・タグ整理の worklist 系クエリは `source_type='session_log'` を pin して
        // いるが、rollup の EXISTS 副問い合わせ / `get_topic_nodes_for_session` 等は「親チェイン /
        // `source_session_id`」で topic を絞る**裸の `node_type='topic'`** だと判明した。宣言を
        // 別 `node_type` にすれば、これら全ての `node_type='topic'` 述語から**自動で外れる**
        // （不変条件に依存しない構造的分離）。将来 誰かが裸の topic クエリを足しても混ざらない。
        //
        // `insert_index_node` は `INSERT OR IGNORE` なので、CHECK 違反はエラーにならず黙って
        // 無視される（＝宣言ノードを作ったつもりで消える）。SQLite は CHECK を `ALTER` で
        // 広げられないため、**v5 / v21 / v23 と同じテーブル再構築**で許可値を 1 つ足す。全行を
        // 無条件コピーするので time-series ツリー（period/session/topic/daily）も category/meta も
        // 無傷。孤児 parent_id は NULL に落とす（v23 の流儀）。
        //
        // FTS 整合: `memory_index_fts` は `node_id`（TEXT 列）で手動同期する独立 FTS5 で、
        // `content=`（external-content by rowid）ではない。再構築は `INSERT ... SELECT` で全 `id`
        // を保存するので FTS 行は全て有効なまま＝**FTS 孤児は起きない**（v23 も FTS を触って
        // いない）。
        //
        // 冪等性（肯定形。v23 と同じ流儀）: 現行スキーマの `node_type` CHECK に `'unit'` が
        // 既に現れるときだけ再構築を skip する（`sqlite_master.sql` の文字列判定）。狭い CHECK の
        // テーブル SQL に `'unit'` の文字列が現れる余地は無い（列名・既定値・他の CHECK の
        // どれにも含まれない）ので「まだ狭いのに広いと誤判定して skip」は起こり得ない。逆に
        // 「広いのに狭いと誤判定して再構築」へ外れても再構築は冪等なので無害。新規 DB は
        // SCHEMA_SQL 側で既に 'unit' を持つので再構築されない。
        //
        // 切り戻し（宣言ノードは本人が作る派生データ。削除すれば原状復帰。古いバイナリへ
        // 戻すときは版番号も戻すこと）:
        //   BEGIN;
        //   DELETE FROM memory_index_fts WHERE node_id IN
        //       (SELECT id FROM memory_index_nodes WHERE node_type='unit');
        //   DELETE FROM memory_index_nodes WHERE node_type IN ('unit','root') AND source_type='declared';
        //   -- （厳密に旧 CHECK へ戻すなら v23 と同型の再構築で狭める）
        //   PRAGMA user_version = 29;
        //   COMMIT;
        up: |conn| {
            let widened: bool = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name='memory_index_nodes'",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .map(|sql| sql.contains("'unit'"))
                .unwrap_or(false);
            if !widened {
                conn.execute_batch("PRAGMA defer_foreign_keys = ON")?;
                conn.execute_batch(
                    "CREATE TABLE memory_index_nodes_new (
                        id TEXT PRIMARY KEY,
                        agent_id TEXT NOT NULL,
                        parent_id TEXT REFERENCES memory_index_nodes_new(id) ON DELETE CASCADE,
                        node_type TEXT NOT NULL CHECK (node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly','category','meta','unit')),
                        source_type TEXT NOT NULL DEFAULT 'session_log',
                        title TEXT NOT NULL,
                        summary TEXT NOT NULL,
                        start_log_id INTEGER,
                        end_log_id INTEGER,
                        source_session_id TEXT,
                        date_from TEXT,
                        date_to TEXT,
                        depth INTEGER NOT NULL DEFAULT 0,
                        child_count INTEGER NOT NULL DEFAULT 0,
                        token_count INTEGER NOT NULL DEFAULT 0,
                        created_at TEXT NOT NULL,
                        updated_at TEXT NOT NULL,
                        short_id TEXT,
                        keywords_json TEXT NOT NULL DEFAULT '[]',
                        summary_refreshed_at TEXT
                    );
                    INSERT INTO memory_index_nodes_new
                        (id, agent_id, parent_id, node_type, source_type, title, summary,
                         start_log_id, end_log_id, source_session_id, date_from, date_to,
                         depth, child_count, token_count, created_at, updated_at, short_id,
                         keywords_json, summary_refreshed_at)
                        SELECT id, agent_id, parent_id, node_type, source_type, title, summary,
                               start_log_id, end_log_id, source_session_id, date_from, date_to,
                               depth, child_count, token_count, created_at, updated_at, short_id,
                               keywords_json, summary_refreshed_at
                        FROM memory_index_nodes;
                    UPDATE memory_index_nodes_new SET parent_id = NULL
                        WHERE parent_id IS NOT NULL
                          AND parent_id NOT IN (SELECT id FROM memory_index_nodes_new);
                    DROP TABLE memory_index_nodes;
                    ALTER TABLE memory_index_nodes_new RENAME TO memory_index_nodes;
                    CREATE INDEX IF NOT EXISTS idx_mem_idx_agent ON memory_index_nodes(agent_id);
                    CREATE INDEX IF NOT EXISTS idx_mem_idx_parent ON memory_index_nodes(agent_id, parent_id);
                    CREATE INDEX IF NOT EXISTS idx_mem_idx_type ON memory_index_nodes(agent_id, node_type);
                    CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_index_nodes_short_id ON memory_index_nodes(agent_id, short_id) WHERE short_id IS NOT NULL;
                    CREATE INDEX IF NOT EXISTS idx_memory_index_nodes_source_type ON memory_index_nodes(agent_id, source_type);",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 31,
        description:
            "agent_memory_index_config.memory_declare_cursor: 宣言ラン（記憶の単位）の進捗マーカー列 (issue #384 / #376 段階2)",
        // **列追加のみ。既存の表・行の内容には一切触れない。**
        //
        // #376 段階2（#384）: エージェント自身が自分の生ログ（memory_sessions）を俯瞰し、
        // 「どこからどこまでが 1 つの記憶か」を宣言するスリープラン（宣言ラン）が、
        // **どこまで宣言し終えたか**を刻む単一マーカー。タグ整理ラン（v27〜v29 の 3 列）とは
        // 入力も進捗も別物なので**別ラン・別マーカー**にする（設計 #376: 別ラン / 足回りは共有）。
        //
        // 中身は複合カーソル **`"{last_run_at_rfc3339}|{cursor_log_id}"`**（1 列に 2 情報）:
        //  - `last_run_at`: 日次 throttle の壁時計（clean 完了ごとに `now`）。
        //  - `cursor_log_id`: 生ログ id 上の**昇順・前進のみ**の位置（提示し終えた末尾）。
        // タグ整理ランが位置と throttle を別列に分けたのは、非トランザクションな索引ビルドが
        // 残す snapshot 外 topic を壁時計カーソルが追い越して恒久ロスする罠（#365）を避けるため。
        // 宣言ランは**生ログ（不変・append-only・id 単調増加）**を直接読むので snapshot も
        // watermark も関与せず、位置を id で持てば追い越しは起きない。ゆえに 1 列で両立できる。
        //
        // **NULL 既定 = 未実行**。宣言ラン側は NULL を `(throttle 無し, cursor=0)` と解釈し、
        // 初回は生ログの先頭（最古）から枠 N 件ぶんを提示する（タグ整理ランの「初回シードして
        // 1 回スキップ」は既存 topic の一斉対象化を防ぐためで、宣言ランは枠が毎回 N 件に有界な
        // ので不要 / seed-skip は入れない）。config 既定オフなので有効化するまで書かれない。
        //
        // #349 の罠を踏まないため **番号付き MIGRATIONS へ置く**（凍結された `migrate()` は
        // 新規 DB でしか走らず、本番の既存 DB（現在 v30）には届かない）。
        //
        // 冪等性: 新規 DB は `SCHEMA_SQL` の `CREATE TABLE agent_memory_index_config` 側で
        // 列を持つので `column_exists` でガードする（v24 / v25 / v27〜v29 の前例）。
        //
        // 切り戻し: 列は読まれなくなるだけで既存の行は壊れない。古いバイナリへ戻すときは
        // 版番号を戻すこと（列はそのままで良い）:
        //
        //   BEGIN;
        //   PRAGMA user_version = 30;
        //   COMMIT;
        up: |conn| {
            if !column_exists(conn, "agent_memory_index_config", "memory_declare_cursor")? {
                conn.execute_batch(
                    "ALTER TABLE agent_memory_index_config ADD COLUMN memory_declare_cursor TEXT",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 32,
        description:
            "既存の受信行の agent_id を送信者→受信側エージェントへ付け替え、索引/FTS に載せる (issue #380 / #377)",
        // **値の付け替えのみ。スキーマも行数も生ログ本文も変えない**（`agent_id` 列の値だけ）。
        //
        // 背景（#377 / #382）: 受信発言の記録は以前 `agent_id` 列にも `speaker_id` 列にも
        // **送信者ID**を入れていた。索引ビルドも FTS 記憶検索も `WHERE agent_id = <当該
        // エージェント>` で走るため、送信者名義の受信行は受信側エージェントの索引にも検索にも
        // 一切載らなかった。#382 で**これから書く**受信行は受信側名義へ直したが、**既存の
        // 受信行はそのまま**残る（Nostr は最初からこの形なので相手の発言が一度も載っていない）。
        //
        // このマイグレーションは**既存の受信行だけ**を受信側エージェント名義へ付け替える。
        // `speaker_id`（送信者）は変えない。FTS は `WHERE agent_id` で絞るだけなので、
        // `agent_id` を直せば topic 再索引なしで `search_my_history` から即引ける（#380）。
        //
        // ## 対象の特定（安全性の核心）
        // 受信側エージェントは **`session_id` に埋め込まれた agent_id** から復元する。
        // 現行の session_id は受信側エージェント自身のループが `discord-{agent_id}-{guild}-{channel}`
        // / `nostr-{agent_id}`（`nostr-{agent_id}-{pubkey}` の旧形も）で組み立てる（
        // `crates/discord/src/message_loop.rs` / `crates/nostr/src/manager.rs`）。よって
        // session_id に現れる agent_id が受信側の権威的な印。**実在する `agents` 表と JOIN
        // して**該当する 1 エージェントが定まる行だけを対象にする（推測しない）。
        //
        // 対象行の述語（3 つ全て満たす。これが**旧形の受信行**を正確に選ぶ）:
        //   - `log_type = 'speech'`
        //   - `metadata_json` の `source` が `'discord'` / `'nostr'`（＝受信 / `record_inbound_message`。
        //     `*_response` は応答なので除外、metadata 無しの旧々形も除外）
        //   - `agent_id = speaker_id`（旧形は両列とも送信者。#382 以降の新形は
        //     `agent_id`≠`speaker_id` なので**自動で除外**される＝二重移行しない）
        //
        // ## 触らない行（重要）
        //   - **session_id から受信側が一意に定まらない行**（旧い `discord-{guild}-{channel}`
        //     形式で agent_id が埋まっておらず、複数エージェントが同居した共有チャンネル等）は
        //     **1 行も触らない**（`agents` と JOIN しても該当なし＝`new_agent IS NULL`）。
        //     誤って別人の記憶に混ぜるより、載らないまま残す方が安全（#377「エージェント間で
        //     記憶を混ぜない（絶対）」）。本番コピー実測で 390 行がこれに該当し保留した。
        //   - metadata 無しの旧々形（`agent_id` は既に受信側で正しく載っている）
        //   - 応答行（`*_response`）・NO_REPLY・その他
        //
        // ## 本番コピーでの実測（適用前 user_version=31 / memory_sessions 47,497 行）
        // 述語に掛かるのは 5,047 行（issue #380 の件数と一致）。うち **4,657 行が一意に復元でき、
        // 390 行が復元不能**。**複数エージェントに match して曖昧になる行は 0 行**（`LIMIT 1` が
        // 選択を隠していない）。復元不能 390 は全て 2 セグメントの `discord-{guild}-{channel}` で、
        // 同一 session の応答者から推定する案も検討したが最大の 1 session に応答者が 3 人おり
        // 一意に決まらないため、推定はせず保留した。適用後は行数（全 39 テーブル）・`content` /
        // `speaker_id` / `session_id` / `metadata_json` / `created_at` とも一切変化なし。
        //
        // ## FTS も同時に直す
        // `memory_sessions_fts` は本体と手動同期する fts5 で、`agent_id` を UNINDEXED 列として
        // 持つ。ここを直し忘れると検索に載らないので、本体と**同一の rowid 集合**へ同じ値を書く。
        // 一時表 `_v32_inbound_remap` に「(rowid, 新 agent_id)」を一度だけ確定させ、本体と FTS の
        // 双方へ適用することで両者の集合を一致させる。
        //
        // ## v32 が回復する範囲（重要・ここで閉じるのは半分だけ）
        // **v32 が回復するのは FTS 記憶検索（`search_my_history`）のみ。索引ビルドへの取り込みは
        // 別課題として #380 に残る。** `search_session_logs` は `WHERE fts.agent_id = ?` だけで
        // 絞るので付け替えれば即引ける。一方、索引ビルドは watermark（`memory_index_watermark`
        // の `last_indexed_log_id`）を `after_id` に渡して `id > after_id` の行だけを拾う
        // （`crates/core/src/memory_index/index_builder.rs` → `get_unindexed_session_logs`）。
        // 付け替え対象は過去行なので **watermark より下**にあり（本番コピー実測: 対象 4,657 行は
        // 全て受信側の watermark 以下）、`agent_id` を直しても索引ビルドは 1 行も拾わない。
        // 索引へ実際に載せるには watermark を巻き戻す等の再索引の仕掛けが要る（#380 の項目 2・3）。
        //
        // ## 冪等性
        // 番号付き MIGRATIONS は `user_version` で 1 回しか走らない（#349 の凍結 `migrate()` は
        // 既存 DB に届かないのでここへ置く）。加えて SQL 自体も自然冪等: 付け替え後は
        // `agent_id`≠`speaker_id` になり述語から外れる。`new_agent` が現在値と同じ行（自己宛の
        // 縮退ケース）は remap から除くので二重に動かない。2 回目は対象 0 行。
        //
        // ## 切り戻し
        // 付け替え後の受信行は「新形の受信行」と値の上で区別できない（どちらも
        // `agent_id`≠`speaker_id`）。よって**データの機械的な巻き戻しはしない**。古いバイナリへ
        // 戻す場合も版番号だけ戻せばよい（古いバイナリは `agent_id` を読むだけで、受信側名義で
        // 載っていても壊れない＝むしろ望ましい状態）。厳密な原状復帰が要るなら v32 前のバックアップ
        // から復元する:
        //   BEGIN; PRAGMA user_version = 31; COMMIT;
        up: |conn| {
            // 受信側が一意に定まる旧形の受信行だけを (rowid, 新 agent_id) へ確定させる。
            // agents との JOIN で該当なし（＝復元不能）や自己宛（付け替え不要）は除く。
            //
            // `IF NOT EXISTS` は**付けない**。`CREATE TABLE ... AS SELECT` に付けると同名の
            // TEMP 表が既にある場合に SELECT が実行されず、**古い中身がそのまま適用される**
            // （冪等のつもりが逆に働き、黙って別の集合を書き換える）。先頭に
            // `DROP TABLE IF EXISTS` を置く手もあるが、それも残骸を黙って捨てるだけで
            // 「なぜ残っていたか」を隠す。ここは残骸があれば即エラーで落ちる形にして、
            // 異常に気づけるようにする（正常系は末尾の `DROP` で必ず消える。途中失敗時も
            // temp DB は同一トランザクションに参加するので巻き戻る）。
            //
            // 相関サブクエリの `LIMIT 1` は ORDER BY 無しなので、2 件以上 match すると黙って
            // 片方を選ぶ。2 件 match し得るのは、ある agent_id が別の agent_id の接頭辞に
            // なっている場合だけ。**これを構造的に排除する仕組みは無い**（`agents.agent_id` は
            // UUID 形とは限らず、本番にも UUID 形でないものが実在する）。担保は構造ではなく
            // **実測**である: 本番実測で、接頭辞関係にある agent_id の組は 0 組、述語に掛かる
            // 行のうち 2 件以上の agent に match する行は 0 行、非 UUID 形の agent_id を含む
            // session_id の行も 0 行だった。**agent_id の形が今後増える場合はここを再確認する。**
            conn.execute_batch(
                "CREATE TEMP TABLE _v32_inbound_remap AS
                 SELECT ms.id AS row_id,
                        (SELECT a.agent_id FROM agents a
                          WHERE ms.session_id = 'nostr-' || a.agent_id
                             OR ms.session_id LIKE 'nostr-' || a.agent_id || '-%'
                             OR ms.session_id LIKE 'discord-' || a.agent_id || '-%'
                          LIMIT 1) AS new_agent
                 FROM memory_sessions ms
                 WHERE ms.log_type = 'speech'
                   AND ms.agent_id = ms.speaker_id
                   AND json_extract(ms.metadata_json, '$.source') IN ('discord', 'nostr');
                 DELETE FROM _v32_inbound_remap WHERE new_agent IS NULL;
                 DELETE FROM _v32_inbound_remap
                     WHERE new_agent = (SELECT agent_id FROM memory_sessions WHERE id = row_id);

                 UPDATE memory_sessions
                     SET agent_id = (SELECT new_agent FROM _v32_inbound_remap WHERE row_id = memory_sessions.id)
                     WHERE id IN (SELECT row_id FROM _v32_inbound_remap);
                 UPDATE memory_sessions_fts
                     SET agent_id = (SELECT new_agent FROM _v32_inbound_remap WHERE row_id = memory_sessions_fts.rowid)
                     WHERE rowid IN (SELECT row_id FROM _v32_inbound_remap);

                 DROP TABLE _v32_inbound_remap;",
            )?;
            Ok(())
        },
    },
    Migration {
        version: 33,
        description:
            "sleep のメンテナンスラン（宣言/整理）が生んだ生ログと索引ノードを削除する (issue #393)",
        // **削除のみ。スキーマは変えない。**
        //
        // 背景（#393）: sleep のメンテナンスラン（記憶の宣言 `memory_declare` / タグ整理
        // `memory_organize`）は `run_agent_response` を通るため、そのターン（speech /
        // tool_call / tool_result）が生ログ `memory_sessions` に記録されていた。生ログは
        // 次の宣言ランの材料そのものなので、**整備作業のログが「記憶」の材料になる**。実際に
        // 本番で「生ログを初めて俯瞰し、E2E 試験期間を記憶として束ねた内省」というユニットが
        // 宣言された。#375 でアイドルのハートビートが topic を量産したのと同じ構造。
        //
        // **これから書く分**は `RunRequest::persist_turn_logs = false`（#393）で止まる
        // （`crates/actions/src/run_request.rs` / `crates/server/src/process.rs`）。この
        // マイグレーションは**既に書かれてしまった分**を消す。
        //
        // ## 対象の特定
        // `session_id` の接頭辞で引く。メンテナンスランの session_id を組み立てるのは
        // **2 箇所だけ**で、`RunRequest::new` の全呼び出し元を走査して確認した:
        //   - `crates/server/src/memory_declare.rs` … `sleep-declare-{agent_id}-{unix_ts}`
        //   - `crates/server/src/memory_organize.rs` … `sleep-organize-{agent_id}-{unix_ts}`
        // sleep のもう 1 つのラン（`skill_consolidation`）は素の LLM 1 コールで session を
        // 持たない（`llm_logs.session_id` は `None`）ため生ログを書かず、対象外。
        // 対話・heartbeat・subtask・nostr・web・REST の session_id はいずれも別の接頭辞
        // （`discord-` / `heartbeat-` / `subtask-` / `nostr-` / `web-` / `agent-msg-`）で、
        // `sleep-` で始まるものは無い。
        //
        // ## FTS も同じ rowid 集合で消す
        // `memory_sessions_fts` は本体と手動同期する通常の fts5（外部コンテンツではない）。
        // 片方だけ消すと孤児（本体に対応行が無い FTS 行）が増え、`search_my_history` に
        // 実体の無い行が出る。一時表 `_v33_maintenance_rows` に rowid を一度確定させ、本体と
        // FTS の**同一集合**へ適用する。既存の孤児には触れない（増やしも減らしもしない）。
        //
        // ## 索引ノードも一緒に消す（生ログだけ消すと「中身が引けない記憶」が残る）
        // **索引ビルドは削除対象の id 帯を既に通過済み**である（本番実測: 稼働中 3 体とも
        // `memory_index_watermark.last_indexed_log_id` が、その体の `sleep-declare-%` 行の
        // MAX(id) と一致）。つまりメンテナンスランのログから作られた索引ノードが既に存在する。
        // 生ログだけ消すと、`retrieve_memory_nodes`（`crates/actions/src/memory_access.rs`）が
        // `start_log_id..end_log_id` で本文を引いたとき `messages: []` を返す一方、
        // `search_memory_index` には `memory_index_fts` 経由でヒットし続ける
        // = **タイトルと要約はあるが中身が空の記憶**が残る。#393 の目的（整備作業を記憶にしない）が
        // 索引層で未達になるので、索引側も同時に消す。
        //
        // 対象は 2 種類:
        //   1. `source_session_id` がメンテナンスランのセッションを指すノード。索引ビルダは
        //      session / topic ノードに必ず `source_session_id: Some(session_id)` を入れる
        //      （`crates/core/src/memory_index/index_builder.rs`）ので**機械的に判定できる**。
        //   2. 本人が宣言したユニット（`node_type='unit'`）のうち、**範囲内の生ログが 1 件以上
        //      あり、その全てがメンテナンスラン由来**のもの。ユニットは `source_session_id` を
        //      持たない（`record_memory_unit` は id 範囲だけを刻む）ので範囲の中身で判定する。
        //      「整備作業そのものを記憶にしてしまったユニット」がこれに当たる（本番実測 1 件:
        //      「生ログを初めて俯瞰し、E2E 試験期間を記憶として束ねた内省」）。判定は生ログを
        //      消す**前**に行う必要があるため、削除順は「索引 → 生ログ」にしてある。
        //      範囲に通常のログが 1 件でも混じるユニットは対象外（本人の記憶を巻き添えにしない）。
        //
        // 子孫も含めて消す（再帰 CTE）。本番実測では対象ノードの子は全て対象に含まれており
        // （topic の親は必ず対象 session）、対象外のノードが巻き添えになる関係は 0 件だった。
        // 再帰にしてあるのは将来 CASCADE で黙って消える子の FTS 行が残らないようにするため。
        //
        // ## 親の集計列（`child_count`）は直す
        // `memory_index_nodes.child_count` は「直下の子の数」で、**本番では全 6,997 ノードが
        // 実カウントと一致している**（`index_stats` の `child_count_mismatch` はこれを見る /
        // `crates/core/src/memory_index/graph_query.rs`）。子を消すとここがずれるので、
        // 削除**前**に「生き残る親」を控えておき、削除後に実カウントで書き直す。
        //
        // **索引ビルダの再計算には任せられない。** 再計算（`index_builder.rs`）は現存する子から
        // `HashMap<parent_id, count>` を組んで**そのキーだけ**を UPDATE するので、子が 0 になった
        // 親は 1 度も書かれず古い値が残り続ける。本番では 5 つの親がずれ、うち 2 つ
        // （`period-…-2026-08` 2 件）は子 0 になる = 永久に直らない側に当たる。
        //
        // `updated_at` は触らない。ここでの書き換えは「子が消えた」ことの反映で、ノード自身の
        // 内容は変わっていない（`updated_at` が child_count 更新で汚れる件は `IndexNodeRow` の
        // doc にあるとおり。マイグレーションで全ノードの時刻を動かす方が読み手を混乱させる）。
        //
        // ## 索引まわりで**触らない**もの
        // - 空になる親（`period` ノード）自体は残す。`period-{agent_id}-{YYYY-MM}` は索引ビルダが
        //   同じ id で再利用するキーで、消しても次のビルドで作り直される。本番では 2 件が
        //   子 0 になるが、後続のセッションがそこへ吊り下がるだけで害が無い（`child_count` は
        //   上記のとおり 0 へ直す）。
        // - `memory_index_watermark.last_indexed_log_id` は id の**値比較**にしか使われない
        //   （`get_unindexed_session_logs` / `get_unindexed_log_count` の `id > ?`）ので、
        //   その id の行が消えても索引ビルドの入力は 1 行も変わらない。
        // - `memory_index_watermark.total_nodes` は「累計で何ノード作ったか」の積み上げ値
        //   （`index_builder` が `existing + nodes_created` で書くだけ）で、実カウントとは
        //   元から一致していない（本番実測 3,033 vs 実カウント 3,210 等）。API が返すのは
        //   `tree.len()`（`crates/server/src/api/agents.rs`）なので、ここは触らない。
        // - 宣言カーソル（`agent_memory_index_config.memory_declare_cursor` の位置部）も値比較のみ。
        //
        // ## 索引ノードを指す他テーブル
        // ノード id を値で持つ列はスキーマ全走査で `memory_index_nodes.parent_id`（自己参照）、
        // `memory_index_fts.node_id`、`memory_category_members.topic_id` / `.category_id` の 4 つ。
        // `memory_category_members` も同じ集合で削除する（宙に浮く参照を残さない）。**`topic_id`
        // という列名だが topic ノードとは限らず、削除対象のユニットを指す行が本番に 3 件あった。**
        //
        // ## 運用記録（`llm_logs` / `agent_logs`）は消さない
        // 消すのは `memory_sessions` と `memory_sessions_fts` の 2 表だけ。**何を行ったかの
        // 記録は別途必要**（#393 の追加受け入れ条件）なので、`llm_logs`（`session_id` で
        // ランを特定でき、LLM コールごとの生プロンプト = 累積 messages・応答・`tool_calls`・
        // トークン数を持つ）と `agent_logs`（context="sleep" の 1 ラン 1 行の要約）は残す。
        // 生ログから外すのは「記憶の材料としての扱い」だけ。
        //
        // ## 本番コピーでの実測（適用前 user_version=32）
        // 生ログ: `memory_sessions` 49,233 → 47,587 / `memory_sessions_fts` 49,441 → 47,795
        // （ともに -1,646。対象は `sleep-declare-%` のみで `sleep-organize-%` は 0 行）。
        // FTS 孤児 208 行は前後で不変、本体だけで FTS が無い行は 0 のまま。
        //
        // 索引: `memory_index_nodes` 6,997 → 6,896 / `memory_index_fts` 6,997 → 6,896
        // （ともに -101 = session 36 + topic 64 + unit 1）。`memory_index_fts` の孤児 0、
        // 本体だけのノード 0、親が存在しないノード 0（いずれも前後で 0）。
        // `memory_category_members` 392 → 389（-3。全て削除したユニットを `topic_id` に持つ行で、
        // **ユニットにもカテゴリが付く**ため `source_session_id` 由来のノードだけを見ると 0 に見える）。
        //
        // `child_count` は前後とも実カウントと**全ノードで一致**（mismatch 0 → 0）。子を失った
        // 5 つの親は 3→2（`declroot-…`）/ 12→0 / 14→3 / 68→57 / 2→0（`period-…-2026-08`）へ
        // 正しく減った。**0 になる 2 件が索引ビルダでは直らない側**（再計算は子を持つ親しか書かない）。
        //
        // **適用後、id 範囲を持つノードで範囲が空になるものは 0 件**（session 275/275・
        // topic 5,990/5,990・unit 71/71 が全て 1 件以上の生ログを引ける）。両 FTS の
        // `integrity-check` も通過。
        //
        // `llm_logs` 8,094 行・`agent_logs` 44 行は前後とも同数。全 39 テーブルの行数 diff で
        // 変化したのは上記 3 表と 2 つの FTS のシャドウ表だけ。
        //
        // ## 冪等性
        // 番号付き MIGRATIONS は `user_version` で 1 回しか走らないが、SQL 自体も自然冪等。
        // 2 回目は (1) が 0 行（既に消えている）、(2) も 0 行になる: 生ログを消した後は
        // `_v33_maintenance_rows` が空なので「範囲内の全行がメンテナンスラン由来」は
        // 「範囲内に行が 1 件も無い」と同値になり、直前の「1 件以上ある」条件と両立しない。
        // 本番コピーで版を 32 へ戻して再実行し、全テーブル行数の差分がゼロであることを確認した。
        //
        // ## 切り戻し
        // 削除した生ログと索引ノードは復元できない。原状復帰が要るなら v33 前のバックアップから
        // 戻す。バイナリだけ戻す場合は版番号を戻せばよい:
        //   BEGIN; PRAGMA user_version = 32; COMMIT;
        up: |conn| {
            // `IF NOT EXISTS` は付けない（v32 と同じ理由: 残骸があれば黙って古い集合を
            // 適用するより、即エラーで落ちて気づけるようにする）。正常系は末尾の `DROP` で
            // 必ず消え、途中失敗時も temp DB は同一トランザクションに参加して巻き戻る。
            //
            // **順序が意味を持つ**: ユニットの判定（範囲内の生ログが全てメンテナンスラン由来か）は
            // 生ログが残っているうちにしかできない。索引側を先に確定・削除してから生ログを消す。
            conn.execute_batch(
                "CREATE TEMP TABLE _v33_maintenance_rows AS
                 SELECT id AS row_id FROM memory_sessions
                 WHERE session_id LIKE 'sleep-declare-%'
                    OR session_id LIKE 'sleep-organize-%';

                 -- 削除する索引ノード（種を確定 → 子孫へ再帰的に広げる）。
                 CREATE TEMP TABLE _v33_index_nodes AS
                 WITH RECURSIVE seed(id) AS (
                     -- (1) メンテナンスランのセッションから作られた session / topic ノード
                     SELECT id FROM memory_index_nodes
                     WHERE source_session_id LIKE 'sleep-declare-%'
                        OR source_session_id LIKE 'sleep-organize-%'
                     UNION
                     -- (2) 範囲が「メンテナンスランのログだけ」で構成される宣言ユニット
                     SELECT n.id FROM memory_index_nodes n
                     WHERE n.node_type = 'unit'
                       AND n.start_log_id IS NOT NULL AND n.end_log_id IS NOT NULL
                       AND EXISTS (SELECT 1 FROM memory_sessions m
                                    WHERE m.agent_id = n.agent_id
                                      AND m.id BETWEEN n.start_log_id AND n.end_log_id)
                       AND NOT EXISTS (SELECT 1 FROM memory_sessions m
                                        WHERE m.agent_id = n.agent_id
                                          AND m.id BETWEEN n.start_log_id AND n.end_log_id
                                          AND m.id NOT IN (SELECT row_id FROM _v33_maintenance_rows))
                 ), subtree(id) AS (
                     SELECT id FROM seed
                     UNION
                     SELECT n.id FROM memory_index_nodes n JOIN subtree s ON n.parent_id = s.id
                 )
                 SELECT id AS node_id FROM subtree;

                 -- 子を失う「生き残る親」を削除**前**に控える（削除後は parent_id を辿れない）。
                 CREATE TEMP TABLE _v33_affected_parents AS
                 SELECT DISTINCT n.parent_id AS node_id
                 FROM memory_index_nodes n
                 WHERE n.id IN (SELECT node_id FROM _v33_index_nodes)
                   AND n.parent_id IS NOT NULL
                   AND n.parent_id NOT IN (SELECT node_id FROM _v33_index_nodes);

                 -- 索引: FTS → カテゴリ所属 → 本体の順に、同一 node_id 集合で消す。
                 DELETE FROM memory_index_fts
                     WHERE node_id IN (SELECT node_id FROM _v33_index_nodes);
                 DELETE FROM memory_category_members
                     WHERE topic_id IN (SELECT node_id FROM _v33_index_nodes)
                        OR category_id IN (SELECT node_id FROM _v33_index_nodes);
                 DELETE FROM memory_index_nodes
                     WHERE id IN (SELECT node_id FROM _v33_index_nodes);

                 -- 生き残る親の child_count を実カウントへ直す。
                 UPDATE memory_index_nodes
                     SET child_count = (SELECT COUNT(*) FROM memory_index_nodes c
                                         WHERE c.parent_id = memory_index_nodes.id)
                     WHERE id IN (SELECT node_id FROM _v33_affected_parents);

                 -- 生ログ: 本体と FTS を同一 rowid 集合で消す。
                 DELETE FROM memory_sessions_fts
                     WHERE rowid IN (SELECT row_id FROM _v33_maintenance_rows);
                 DELETE FROM memory_sessions
                     WHERE id IN (SELECT row_id FROM _v33_maintenance_rows);

                 DROP TABLE _v33_affected_parents;
                 DROP TABLE _v33_index_nodes;
                 DROP TABLE _v33_maintenance_rows;",
            )?;
            Ok(())
        },
    },
    Migration {
        version: 34,
        description:
            "agent_memory_index_config.memory_declare_window: 宣言ランの窓の希望（本人が決める境界と広さ）(issue #394)",
        // **列追加のみ。既存の表・行の内容には一切触れない。**
        //
        // #394: 宣言ランは「どこからどこまでが 1 つの記憶かは本人が決める」設計なのに、
        // **窓の境界と広さだけは機械が固定で決めていた**（カーソルは宣言内容と無関係に窓の
        // 終端へ進む / `memory_declare.rs`）。この列は、本人が道具
        // （`plan_next_memory_window`）で表明した**次回の窓の希望**を持つ。
        //
        // 中身は JSON（[`crate::queries::DeclareWindowPref`]）:
        //  - `next_from_id`: 次回の窓をここから始めたい（＝この id 以降は次回へ回す）。
        //    **そのランの終わりに消費して消える**（持ち越さない）。
        //  - `window_size`: 次回以降の窓に入れる生ログ件数。**sticky**（本人が上書きするまで
        //    効き続ける）。
        //  - `note`: 理由（監査に残すだけ / 機械は解釈しない）。
        //
        // どちらも**希望**であり、ランの側が前進の下限・上限へ丸めてから使う（本人任せにすると
        // 同じ窓を永久に再取得するループへ入る / #374 で実際に踏んだ罠）。丸めの規則は
        // `crates/server/src/memory_declare.rs` にある。
        //
        // **NULL 既定 = 希望なし**（従来どおり窓の終端まで進み、広さは config の `max_logs`）。
        // 既存 DB は列が NULL のまま増えるだけで、宣言ランの挙動は本人が道具を使うまで変わらない。
        //
        // 版番号が 33 でなく **34** なのは、33 を #393（宣言/整理ランのターンログを
        // `memory_sessions` に残さない）が使うため。
        //
        // ## 番号を飛ばして採るときの前提（**適用順の保証が要る**）
        // `run_migrations` は `m.version > user_version` でしか判定せず、適用のたびに
        // `user_version` を**その番号で**刻む。「番号 N は未適用」という台帳はどこにも無い。
        // したがって **番号の大きいマイグレーションを先に適用して再起動すると、番号の小さい
        // 未適用のマイグレーションは永久に skip される**（エラーも警告も出ない）。ここで 34 を
        // 先に刻んだ DB は `33 > 34` が偽になり、v33 が一度も走らないまま「適用済み」に見える。
        //
        // つまり番号を飛ばすときは、**飛ばされた番号の側（ここでは v33 / #393）が先に適用される
        // ことを運用で保証する**必要がある。この PR（#394）は #393 をマージ・再起動したあとに
        // 入れる前提で 34 を採っている。番号を飛ばして採る後続も、同じ保証を持てるときだけに
        // すること（持てないなら、後からマージする側が次の空き番号を取る）。
        //
        // 冪等性: 新規 DB は `SCHEMA_SQL` の `CREATE TABLE agent_memory_index_config` 側で
        // 列を持つので `column_exists` でガードする（v24 / v25 / v27〜v29 / v31 の前例）。
        //
        // 切り戻し: 列は読まれなくなるだけで既存の行は壊れない。古いバイナリへ戻すときは
        // 版番号を戻すこと（列はそのままで良い）。戻す先は **33**（#393 の v33 が適用済みの
        // 状態）。32 まで戻すと次回起動で v33 が再走する（対象 0 行なので無害だが不正確）:
        //
        //   BEGIN;
        //   PRAGMA user_version = 33;
        //   COMMIT;
        up: |conn| {
            if !column_exists(conn, "agent_memory_index_config", "memory_declare_window")? {
                conn.execute_batch(
                    "ALTER TABLE agent_memory_index_config ADD COLUMN memory_declare_window TEXT",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 35,
        description:
            "agent_memory_index_config.memory_condense_cursor: 凝縮ラン（記憶の 3 段目）の進捗マーカー列 (issue #411)",
        // **列追加のみ。既存の表・行の内容には一切触れない。**
        //
        // #411: ユニット（記憶の 2 段目 / エピソード）を俯瞰して「大事なこと」を抽出し、
        // `node_type='meta'` として人格の核に刻む凝縮ランを足す。この列はその進捗マーカーで、
        // 形式は複合カーソル `"{last_run_at}|{unit_count}"`（宣言ランの `memory_declare_cursor`
        // と同型）。位置部は「前回凝縮した時点のユニット総数」で、発火ゲート（ユニットが下限以上
        // 増えたか）と日次 throttle をこの 1 列で判定する。
        //
        // **NULL 既定 = 未実行**（初回は throttle が掛からず、ユニットが下限以上あれば発火する）。
        // 既存 DB は列が NULL のまま増えるだけで、このマイグレーション自体は挙動を変えない
        // （NULL=未実行。凝縮ランの有効/無効は config の `enabled` 次第。既定 ON は #457）。
        //
        // 冪等性: 新規 DB は `SCHEMA_SQL` の `CREATE TABLE agent_memory_index_config` 側で
        // 列を持つので `column_exists` でガードする（v24〜v29 / v31 / v34 の前例）。
        //
        // 切り戻し: 列は読まれなくなるだけで既存の行は壊れない。古いバイナリへ戻すときは
        // 版番号を戻すこと（列はそのままで良い）:
        //
        //   BEGIN;
        //   PRAGMA user_version = 34;
        //   COMMIT;
        up: |conn| {
            if !column_exists(conn, "agent_memory_index_config", "memory_condense_cursor")? {
                conn.execute_batch(
                    "ALTER TABLE agent_memory_index_config ADD COLUMN memory_condense_cursor TEXT",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 36,
        description: "agent_inbox: 外部イベント受信箱（webhook intake, issue #454）",
        // **新規テーブルの追加のみ。既存の表・行の内容には一切触れない。**
        //
        // #454: 外部 source（第一号 omoikane）の webhook / catch-up ポーリングで受け取った
        // 出来事を積む受信箱。専用ループ（`intake_process`）が未処理行を消化して
        // `processed_at` を刻む。
        //
        // 冪等性: `CREATE TABLE IF NOT EXISTS` / `CREATE ... INDEX IF NOT EXISTS` は
        // 自然冪等（v18/v19 の前例）。新規 DB は `SCHEMA_SQL` 側で同じ DDL を持つ。
        //
        // ゲートの向き: このマイグレーションは**テーブルを作るだけ**で、受信・消化を有効化
        // しない。webhook 受信は `[intake.secrets]` に secret を設定した source だけ通り
        // （未設定は 404）、消化ループは常時起動だが未処理行が無ければ LLM を呼ばない。
        // つまり空のテーブルを足しても既存挙動は 1 バイトも変わらない（積むものが無い）。
        //
        // 切り戻し: 古いバイナリへ戻すときは版番号を戻すこと（テーブルはそのままで良い。
        // 読み手が居なくなるだけで既存データは壊れない）:
        //
        //   BEGIN;
        //   PRAGMA user_version = 35;
        //   COMMIT;
        up: |conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS agent_inbox (
                    id TEXT PRIMARY KEY,
                    agent_id TEXT NOT NULL,
                    source TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    dedup_key TEXT NOT NULL,
                    payload_json TEXT NOT NULL,
                    received_at TEXT NOT NULL DEFAULT (datetime('now')),
                    processed_at TEXT
                );
                CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_inbox_dedup
                    ON agent_inbox(source, dedup_key);
                CREATE INDEX IF NOT EXISTS idx_agent_inbox_unprocessed
                    ON agent_inbox(agent_id, processed_at);",
            )?;
            Ok(())
        },
    },
    Migration {
        version: 37,
        description:
            "session_heartbeat_config + agent_schedules（セッション一本化スキーマ + 移行 / #439 × #455 × #456）",
        // **統合スケジューラ PR1: セッション一本化スキーマ + 移行 backfill。**
        //
        // 2 つの表を新設し、既存の agent/channel 二本立てハートビート設定を
        // **セッション単位の `session_heartbeat_config` へ backfill** する。旧表
        // （`agent_heartbeat_config` / `discord_channel_config.heartbeat_*`）は**残置**
        // （読まない・撤去は後続 PR）。**発火経路はまだ切り替えない**（PR2）。
        //
        // ## 不変条件（最重要・設計 §4.2）
        // **現状の発火挙動を 1 ビットも変えない**。opt-in 済みエージェントの Discord channel
        // 発火は現状 precedence（AgentScoped）が能動的に抑止（沈黙）しているので、その抑止を
        // `enabled=0` として**保存**する（無条件 enabled 化＝新規発火は禁止）。global
        // `heartbeat_enabled`（G）は per-session の状態ではないのでデータへ焼かず、発火時の
        // ランタイムゲートとして残す（PR2・kill-switch のライブ性を壊さない）。
        //
        // ## 原子性
        // `run_migrations` の per-migration トランザクション内で走り、`up` が `Err` を返すと
        // **アトミックにロールバック**される（版トラップは起きない）。移行行の形式検証は
        // commit 前にこの関数内で行い、壊れた行があれば `Err`（設計 §4.2.4 の実装契約）。
        //
        // ## 冪等性
        // `CREATE TABLE IF NOT EXISTS`。backfill の INSERT は `ON CONFLICT DO NOTHING`。
        // 新規 DB は `SCHEMA_SQL` 側で両表を持ち、旧表は空なので backfill は no-op。
        //
        // ## 切り戻し（古いバイナリへ戻すとき・旧表は無傷なので新表 2 つの DROP と版番号のみ）
        //   BEGIN;
        //   DROP TABLE IF EXISTS session_heartbeat_config;
        //   DROP TABLE IF EXISTS agent_schedules;
        //   PRAGMA user_version = 36;
        //   COMMIT;
        up: migrate_v37_session_heartbeat,
    },
    Migration {
        version: 38,
        description:
            "agent_schedules の語彙を heartbeat に揃える（last_run_at→last_fired_at・next_run_at 撤去 / #455）",
        // **統合スケジューラ PR4: 定時実行(#455)を配線する前の語彙・持ち方の整合。**
        //
        // v37 が作った `agent_schedules` は heartbeat と語彙・持ち方が割れていた:
        //   - `next_run_at` / `last_run_at` ↔ heartbeat の `next_fire_at` / `last_fired_at`
        //     （同じ「次に scheduler が手を出す時刻」に 2 名。#456 で潰した二重語彙の再来）
        //   - `next_run_at` は**列に持っていた**が、heartbeat は stale を避けるため
        //     **照会時算出**（キャッシュ列を持たない）。cron 計算は wake 時のみ・件数も僅少で
        //     ホットパスに無く、キャッシュは stale リスク（cron 式/tz/enabled 変更時の無効化漏れ）
        //     だけを増やす。→ **列を撤去して算出に寄せる**（heartbeat と同じ持ち方）。
        //
        // ## 変更（**非破壊・データ保存**）
        //   1. `last_run_at` → `last_fired_at` に RENAME（列の値はそのまま保存される）
        //   2. `next_run_at` を DROP（表示キャッシュに過ぎず、真実は照会時算出）
        // **DROP TABLE ではなく ALTER**（本番 agent_schedules は 0 行だが、万一行があっても
        // RENAME はデータを保存する側＝安全側に倒す）。**向き**: 発火挙動は 1 ビットも変えない
        // （この表からの発火は本 PR の scheduler 配線で初めて起きる。移行時点では誰も読まない）。
        //
        // ## 冪等性（#349 の轍を踏まない）
        // 新規 DB は SCHEMA_SQL 側で既に `last_fired_at` を持ち `next_run_at` を持たないので、
        // `column_exists` でガードして各 ALTER を no-op にする。既存 v37 DB でのみ RENAME/DROP が走る。
        //
        // ## 切り戻し（古いバイナリへ戻すとき）
        //   BEGIN;
        //   ALTER TABLE agent_schedules RENAME COLUMN last_fired_at TO last_run_at;
        //   ALTER TABLE agent_schedules ADD COLUMN next_run_at TEXT;
        //   PRAGMA user_version = 37;
        //   COMMIT;
        up: migrate_v38_align_schedule_vocab,
    },
];

/// このバイナリが知る最新スキーマバージョン。
#[cfg(test)]
fn latest_version() -> i64 {
    MIGRATIONS
        .last()
        .map(|m| m.version)
        .unwrap_or(BASELINE_VERSION)
}

/// 現在のスキーマバージョン（`PRAGMA user_version`）を読み取る。
fn schema_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

/// スキーマ初期化。
///
/// - `user_version < BASELINE_VERSION`（新規DB / バージョン管理導入前の既存DB）の場合、
///   `SCHEMA_SQL` + 凍結された `migrate()` を**1トランザクション**で適用し、version 1 を
///   スタンプする。破壊的なテーブルリビルドや DROP を含むため一括ロールバック可能にする
///   （途中失敗すれば version は 0 のままで、次回起動でクリーンに再試行される）。
/// - その後、`MIGRATIONS` のうち未適用の番号付きマイグレーションを**各自のトランザクション**で
///   適用する（途中失敗時は直前まで確定・再開可能）。
pub fn initialize(conn: &Connection) -> rusqlite::Result<()> {
    let current = schema_version(conn)?;
    if current < BASELINE_VERSION {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(SCHEMA_SQL)?;
        migrate(&tx)?;
        tx.execute_batch(&format!("PRAGMA user_version = {BASELINE_VERSION}"))?;
        tx.commit()?;
    }
    run_migrations(conn, MIGRATIONS)?;
    Ok(())
}

/// 番号付きマイグレーションを順に適用する。
///
/// `current` より大きい version の各マイグレーションを、それぞれ独自の
/// トランザクション内で実行し、成功後に同一トランザクション内で `user_version` を
/// スタンプする。`current` が既知の最新版より新しい（＝より新しいバイナリで作られたDBを
/// 古いバイナリで開いた）場合は、破壊的な誤動作を避けるため明示的にエラーにする。
fn run_migrations(conn: &Connection, migrations: &[Migration]) -> rusqlite::Result<()> {
    let current = schema_version(conn)?;
    let latest = migrations
        .last()
        .map(|m| m.version)
        .unwrap_or(BASELINE_VERSION);
    if current > latest {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some(format!(
                "database schema version {current} is newer than this binary supports ({latest}); please upgrade the application"
            )),
        ));
    }
    for m in migrations {
        if m.version > current {
            let tx = conn.unchecked_transaction()?;
            (m.up)(&tx)?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", m.version))?;
            tx.commit()?;
        }
    }
    Ok(())
}

/// Discord の `guild_id` / `channel_id` を正規化する（設計 §4.2 B3）。
///
/// 本番データに引用符付きの値（例 `"1465697209541726362"`）が混ざるため、連結して
/// `session_id` を作る前に `"` と空白（半角空白・タブ・改行など）を除去する。数字だけが
/// 残る前提で、残らなければ後段の [`session_id_is_valid`] が弾く（fail-closed）。
fn norm_discord_id(raw: &str) -> String {
    raw.chars()
        .filter(|c| *c != '"' && !c.is_whitespace())
        .collect()
}

/// backfill が作った `session_id` が発火先を導ける形式かを検証する（設計 §3.6 / §4.2 B4）。
///
/// `agent_id` はハイフンを含む UUID なので naive な `split('-')` はしない。保存済みの
/// `agent_id` で接頭辞を剥がし、`nostr-{agent}` か `discord-{agent}-{digits}-{digits}` に
/// 合致するかだけを見る（guild/channel は数字のみ）。**未知/解釈不能は false = fail-closed**。
fn session_id_is_valid(session_id: &str, agent_id: &str) -> bool {
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
fn migrate_v38_align_schedule_vocab(conn: &Connection) -> rusqlite::Result<()> {
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

fn migrate_v37_session_heartbeat(conn: &Connection) -> rusqlite::Result<()> {
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
    // 起動し（nostr_runner_impl.rs:94）、発火は `is_running` ゲート（heartbeat_delivery.rs:225）→
    // text_delivery（nostr/actions.rs:352）を通る。EXISTS だけだと **nostr disabled のエージェント
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

/// FROZEN — schema version 1 baseline。
///
/// ⚠️ **警告: ここへ ALTER / 列追加を足しても既存 DB には一切効かない。**
/// この関数は [`initialize`] から `user_version < BASELINE_VERSION`（＝新規 DB / 版管理
/// 導入前の DB）のときしか呼ばれない。本番など既に版がスタンプ済みの DB は
/// `run_migrations` しか通らないため、ここへ書いた変更は永久に no-op になる
/// （#347 でこの罠を踏み、本番の全スキル SELECT が `no such column` で落ちた。#349）。
/// **既存 DB に効かせる変更は必ず version 2 以降の番号付き [`MIGRATIONS`] エントリへ**
/// 追加すること。ここは version 1 として確定した履歴であり、`backfill_short_ids` 呼び出しや
/// `migrate_soul_identity_to_agents` 含めて凍結する。
///
/// 既存テーブルへのマイグレーション（カラム追加など）。
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    // sessions.metadata_json カラム追加（既存DBへの対応）
    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name='metadata_json'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE sessions ADD COLUMN metadata_json TEXT")?;
    }

    // skills.permission カラム追加（既存DBへの対応）
    let has_permission_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('skills') WHERE name='permission'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_permission_col {
        conn.execute_batch(
            "ALTER TABLE skills ADD COLUMN permission TEXT NOT NULL DEFAULT '\"agent\"'",
        )?;
    }

    // skills.archived カラム追加（スキルアーカイブ機能）
    let has_archived_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('skills') WHERE name='archived'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_archived_col {
        conn.execute_batch("ALTER TABLE skills ADD COLUMN archived INTEGER NOT NULL DEFAULT 0")?;
    }

    // discord_channel_config.whitelisted カラム追加
    let has_whitelisted_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('discord_channel_config') WHERE name='whitelisted'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_whitelisted_col {
        conn.execute_batch(
            "ALTER TABLE discord_channel_config ADD COLUMN whitelisted INTEGER NOT NULL DEFAULT 0",
        )?;
    }

    // discord_channel_config.heartbeat_enabled カラム追加
    let has_heartbeat_enabled_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('discord_channel_config') WHERE name='heartbeat_enabled'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_heartbeat_enabled_col {
        conn.execute_batch("ALTER TABLE discord_channel_config ADD COLUMN heartbeat_enabled INTEGER NOT NULL DEFAULT 1")?;
    }

    // discord_channel_config.heartbeat_interval_secs カラム追加
    let has_hb_interval: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('discord_channel_config') WHERE name='heartbeat_interval_secs'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_hb_interval {
        conn.execute_batch(
            "ALTER TABLE discord_channel_config ADD COLUMN heartbeat_interval_secs INTEGER",
        )?;
    }

    // discord_channel_config: agent_idカラム追加 + PKを(channel_id, agent_id)に変更
    let has_agent_id_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('discord_channel_config') WHERE name='agent_id'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_agent_id_col {
        // テーブル再作成でPKを(channel_id, agent_id)に変更
        conn.execute_batch("
            CREATE TABLE IF NOT EXISTS discord_channel_config_new (
                channel_id TEXT NOT NULL,
                agent_id TEXT NOT NULL DEFAULT '',
                guild_id TEXT NOT NULL,
                channel_name TEXT NOT NULL DEFAULT '',
                readable INTEGER NOT NULL DEFAULT 1,
                writable INTEGER NOT NULL DEFAULT 1,
                whitelisted INTEGER NOT NULL DEFAULT 0,
                heartbeat_enabled INTEGER NOT NULL DEFAULT 1,
                heartbeat_interval_secs INTEGER,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (channel_id, agent_id)
            );
            INSERT INTO discord_channel_config_new
                (channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, updated_at)
            SELECT channel_id, '', guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, updated_at
            FROM discord_channel_config;
            DROP TABLE discord_channel_config;
            ALTER TABLE discord_channel_config_new RENAME TO discord_channel_config;
            CREATE INDEX IF NOT EXISTS idx_discord_channel_guild ON discord_channel_config(guild_id);
            CREATE INDEX IF NOT EXISTS idx_discord_channel_agent ON discord_channel_config(agent_id);
        ")?;
    }
    // agent_idカラムが存在する場合もインデックスを保証する（新規DB・マイグレーション済みDB共通）
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_discord_channel_agent ON discord_channel_config(agent_id)",
    )?;

    // agents.heartbeat_instructions カラム追加（ハートビート専用指示）
    let has_agent_hb_instr: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('agents') WHERE name='heartbeat_instructions'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_agent_hb_instr {
        conn.execute_batch(
            "ALTER TABLE agents ADD COLUMN heartbeat_instructions TEXT NOT NULL DEFAULT ''",
        )?;
    }

    // discord_channel_config.heartbeat_instructions カラム追加（チャンネル単位の上書き）
    let has_channel_hb_instr: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('discord_channel_config') WHERE name='heartbeat_instructions'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_channel_hb_instr {
        conn.execute_batch(
            "ALTER TABLE discord_channel_config ADD COLUMN heartbeat_instructions TEXT NOT NULL DEFAULT ''",
        )?;
    }

    // heartbeat_instructions_audit テーブル作成（指示改変の監査ログ）
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS heartbeat_instructions_audit (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id TEXT NOT NULL,
            scope TEXT NOT NULL,
            channel_id TEXT,
            caller_identity TEXT NOT NULL,
            caller_discord_id TEXT,
            old_value TEXT,
            new_value TEXT,
            reason TEXT,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_heartbeat_instr_audit_agent
            ON heartbeat_instructions_audit(agent_id, created_at DESC);",
    )?;

    // agent_memory_index_config テーブル作成（既存DBへの対応）
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_memory_index_config (
            agent_id TEXT PRIMARY KEY,
            batch_size INTEGER NOT NULL DEFAULT 50,
            threshold INTEGER NOT NULL DEFAULT 20,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )?;

    // 旧 soul の JSON 列（social_style_json / thinking_style_json）は、以前ここで
    // 「dead code 削除」として DROP していた。だが soul は後段の
    // `migrate_soul_identity_to_agents` で**テーブルごと** DROP されるため、この個別 DROP は
    // 常にその直後の全体 DROP に呑まれる冗長操作でしかない。しかも thinking_style_json は
    // 自由記述 `description` を含み、ここで先に落とすと集約時の退避（#480）が拾えなくなる。
    // よって個別 DROP は撤去し、全 JSON 列の退避は集約側に一本化する（列は soul ごと消える）。

    // llm_logs テーブル作成（既存DBへの対応）
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS llm_logs (
            id TEXT PRIMARY KEY,
            agent_id TEXT NOT NULL,
            session_id TEXT,
            model TEXT,
            prompt TEXT NOT NULL DEFAULT '',
            response TEXT NOT NULL DEFAULT '',
            tool_calls TEXT,
            latency_ms INTEGER,
            prompt_tokens INTEGER,
            completion_tokens INTEGER,
            total_tokens INTEGER,
            error_code TEXT,
            error_body TEXT,
            requested_at TEXT,
            created_at TEXT DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_llm_logs_agent ON llm_logs(agent_id);
        CREATE INDEX IF NOT EXISTS idx_llm_logs_created ON llm_logs(agent_id, created_at DESC);",
    )?;

    // llm_logs 新カラム追加（既存DBへの対応）
    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='latency_ms'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN latency_ms INTEGER")?;
    }

    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='prompt_tokens'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN prompt_tokens INTEGER")?;
    }

    let has_col: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='completion_tokens'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN completion_tokens INTEGER")?;
    }

    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='total_tokens'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN total_tokens INTEGER")?;
    }

    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='error_code'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN error_code TEXT")?;
    }

    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='error_body'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN error_body TEXT")?;
    }

    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='requested_at'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_col {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN requested_at TEXT")?;
    }
    // After the requested_at column is added (or confirmed to exist), create the index.
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_llm_logs_requested ON llm_logs(agent_id, requested_at DESC)")?;

    // llm_logs.trigger_message_id カラム追加
    let has_trigger: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='trigger_message_id'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_trigger {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN trigger_message_id TEXT")?;
    }

    // llm_logs.is_bot_iteration カラム追加
    let has_bot_iter: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='is_bot_iteration'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_bot_iter {
        conn.execute_batch(
            "ALTER TABLE llm_logs ADD COLUMN is_bot_iteration INTEGER NOT NULL DEFAULT 0",
        )?;
    }

    // llm_logs.cache_read_tokens カラム追加
    let has_cache_read: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='cache_read_tokens'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_cache_read {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN cache_read_tokens INTEGER")?;
    }

    // llm_logs.cache_creation_tokens カラム追加
    let has_cache_creation: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('llm_logs') WHERE name='cache_creation_tokens'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_cache_creation {
        conn.execute_batch("ALTER TABLE llm_logs ADD COLUMN cache_creation_tokens INTEGER")?;
    }

    let has_source_type_col: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('memory_index_nodes') WHERE name='source_type'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_source_type_col {
        conn.execute_batch(
            "ALTER TABLE memory_index_nodes ADD COLUMN source_type TEXT NOT NULL DEFAULT 'session_log'",
        )?;
    }

    let has_date_from_col: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('memory_index_nodes') WHERE name='date_from'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_date_from_col {
        conn.execute_batch("ALTER TABLE memory_index_nodes ADD COLUMN date_from TEXT")?;
    }

    let has_date_to_col: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('memory_index_nodes') WHERE name='date_to'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_date_to_col {
        conn.execute_batch("ALTER TABLE memory_index_nodes ADD COLUMN date_to TEXT")?;
    }

    let has_short_id_col: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('memory_index_nodes') WHERE name='short_id'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_short_id_col {
        conn.execute_batch("ALTER TABLE memory_index_nodes ADD COLUMN short_id TEXT")?;
        crate::queries::backfill_short_ids(conn)
            .map_err(|e| rusqlite::Error::InvalidParameterName(format!("{e}")))?;
    }
    // short_id の partial index は SCHEMA_SQL ではなく **ここ** で張る（列確定後・#475）。
    // fresh DB は SCHEMA_SQL 側で列を持つので上の分岐は skip されるが、この index は
    // 新規 DB でも旧 DB でも必ず必要なので分岐の外で冪等に張る（IF NOT EXISTS）。
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_index_nodes_short_id ON memory_index_nodes(agent_id, short_id) WHERE short_id IS NOT NULL",
    )?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS daily_log_index_watermark (
            agent_id TEXT NOT NULL PRIMARY KEY,
            last_indexed_date TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )",
    )?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_memory_index_nodes_source_type
         ON memory_index_nodes (agent_id, source_type)",
    )?;

    // skills.skill_type カラムDROP（v2: executableタイプ廃止）
    let has_skill_type_drop: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('skills') WHERE name='skill_type'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if has_skill_type_drop {
        conn.execute_batch("ALTER TABLE skills DROP COLUMN skill_type")?;
    }

    // skills.code カラムDROP（v2: executableタイプ廃止）
    let has_code_drop: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('skills') WHERE name='code'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if has_code_drop {
        conn.execute_batch("ALTER TABLE skills DROP COLUMN code")?;
    }

    // memory_curated.created_at カラム追加
    let has_curated_created_at: bool = conn
        .prepare(
            "SELECT COUNT(*) FROM pragma_table_info('memory_curated') WHERE name='created_at'",
        )?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_curated_created_at {
        conn.execute_batch(
            "ALTER TABLE memory_curated ADD COLUMN created_at TEXT NOT NULL DEFAULT ''",
        )?;
    }

    if table_exists(conn, "soul")? {
        // soul.instructions カラム追加（操作ルール・AGENTS.md相当）
        let has_instructions: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('soul') WHERE name='instructions'")?
            .query_row([], |row| row.get::<_, i64>(0))
            .map(|c| c > 0)
            .unwrap_or(false);
        if !has_instructions {
            conn.execute_batch(
                "ALTER TABLE soul ADD COLUMN instructions TEXT NOT NULL DEFAULT ''",
            )?;
        }

        // soul.personality カラム追加（#480）。soul が `personality_json` しか持たない最初期
        // （2026-02・b6a145e）世代の DB は `personality` 列を持たず、後段の
        // `migrate_soul_identity_to_agents` が `SELECT ... s.personality ... FROM soul` で
        // `no such column: s.personality` を投げて起動不能になる。集約前に列を用意して塞ぐ。
        // 旧 `personality_json`（構造化 JSON）は agents.personality（自由記述 TEXT・NULL 可）に
        // 意味的対応が無いため移送せず NULL のままにする（起動の担保が目的・#478 と同じ発想）。
        let has_personality: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('soul') WHERE name='personality'")?
            .query_row([], |row| row.get::<_, i64>(0))
            .map(|c| c > 0)
            .unwrap_or(false);
        if !has_personality {
            conn.execute_batch("ALTER TABLE soul ADD COLUMN personality TEXT")?;
        }
    }

    // import_sync_state テーブル作成
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS import_sync_state (
            id TEXT PRIMARY KEY,
            agent_id TEXT NOT NULL,
            source_dir TEXT NOT NULL,
            file_type TEXT NOT NULL,
            file_name TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            synced_at TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_import_sync_state_key
            ON import_sync_state(agent_id, source_dir, file_name);
        CREATE INDEX IF NOT EXISTS idx_import_sync_state_agent
            ON import_sync_state(agent_id);",
    )?;

    // memory_curated の (agent_id, category) UNIQUE INDEX 追加
    // 既存の重複レコードをself-joinで削除してからインデックスを作成
    // SQLite特有の制限: サブクエリ内でLIMITが使えないのでself-joinを使う
    conn.execute_batch(
        "DELETE FROM memory_curated
         WHERE id IN (
             SELECT mc1.id FROM memory_curated mc1
             INNER JOIN memory_curated mc2 ON mc1.agent_id = mc2.agent_id
                 AND mc1.category = mc2.category
                 AND mc1.updated_at < mc2.updated_at
         );
         CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_curated_agent_category
             ON memory_curated(agent_id, category);",
    )?;

    // agent_logs テーブル作成（既存DBへの対応）
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_logs (
            id TEXT PRIMARY KEY,
            agent_id TEXT,
            level TEXT NOT NULL,
            context TEXT NOT NULL,
            message TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_agent_logs_agent ON agent_logs(agent_id, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_agent_logs_level ON agent_logs(level, created_at DESC);",
    )?;

    // soul + identity → agents 集約（既存DBのみ。soul テーブルがあればデータ移行して DROP）
    migrate_soul_identity_to_agents(conn)?;

    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// テーブルに 1 行以上あるかを判定する（#479: v17 の RENAME 分岐で使う）。
///
/// 呼び出し側で `table_exists` を確認済みの前提。`EXISTS` で 1 行見つかり次第打ち切るので
/// 全件 COUNT より軽い。テーブル名は SQL に埋め込むため、呼び出し元は必ず定数を渡すこと。
fn table_has_rows(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM \"{table}\")"),
        [],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
}

/// テーブルに指定カラムが存在するか判定する（将来の番号付きマイグレーション用ヘルパー）。
///
/// version 1 baseline (`migrate`) 内の約30箇所のインライン `pragma_table_info` プローブは
/// 凍結のためリファクタしないが、version 2 以降の `Migration::up` ではこのヘルパーを使う。
#[allow(dead_code)]
fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
        [table, column],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 旧 soul / identity を agents に統合し、旧テーブルを削除する。
fn migrate_soul_identity_to_agents(conn: &Connection) -> rusqlite::Result<()> {
    if !table_exists(conn, "soul")? {
        return Ok(());
    }

    // agents は SCHEMA で CREATE IF NOT EXISTS 済み。空または未作成のどちらでもよい。
    conn.execute_batch(
        r#"
        INSERT INTO agents (agent_id, name, job_title, organization, image_url, persona_name, personality, instructions, model, metadata_json, created_at, updated_at)
        SELECT
            s.agent_id,
            i.name,
            i.job_title,
            i.organization,
            i.image_url,
            s.persona_name,
            s.personality,
            s.instructions,
            NULL,
            i.metadata_json,
            datetime('now'),
            CASE WHEN s.updated_at >= i.updated_at THEN s.updated_at ELSE i.updated_at END
        FROM soul s
        INNER JOIN identity i ON s.agent_id = i.agent_id
        WHERE NOT EXISTS (SELECT 1 FROM agents a WHERE a.agent_id = s.agent_id);

        INSERT INTO agents (agent_id, name, job_title, organization, image_url, persona_name, personality, instructions, model, metadata_json, created_at, updated_at)
        SELECT
            s.agent_id,
            s.persona_name,
            NULL,
            NULL,
            NULL,
            s.persona_name,
            s.personality,
            s.instructions,
            NULL,
            NULL,
            datetime('now'),
            s.updated_at
        FROM soul s
        WHERE NOT EXISTS (SELECT 1 FROM identity i WHERE i.agent_id = s.agent_id)
          AND NOT EXISTS (SELECT 1 FROM agents a WHERE a.agent_id = s.agent_id);

        INSERT INTO agents (agent_id, name, job_title, organization, image_url, persona_name, personality, instructions, model, metadata_json, created_at, updated_at)
        SELECT
            i.agent_id,
            i.name,
            i.job_title,
            i.organization,
            i.image_url,
            i.name,
            NULL,
            '',
            NULL,
            i.metadata_json,
            datetime('now'),
            i.updated_at
        FROM identity i
        WHERE NOT EXISTS (SELECT 1 FROM soul s WHERE s.agent_id = i.agent_id)
          AND NOT EXISTS (SELECT 1 FROM agents a WHERE a.agent_id = i.agent_id);
        "#,
    )?;

    // #480: 上の集約は soul から persona_name / personality / instructions しか agents に
    // 写さない。残る JSON 列（social_style_json / personality_json=Big Five /
    // thinking_style_json / custom_traits_json）は直後の DROP TABLE soul で失われる。
    // thinking_style_json は自由記述 `description` を、custom_traits_json は利用者任意の JSON を
    // 含み得るため、「意図して設定した値を勝手に破棄しない」原則（#456）に反する。
    // → DROP 前に、存在する JSON 列を **agents.metadata_json.legacy_soul** へ入れ子で退避する。
    //
    // 頑健性: 世代により存在する列が違う（8b2b2b8 以降の soul は JSON 列を一切持たない）ため
    // 実在する列だけを動的に組み立てる。列値が不正 JSON / NULL でも起動を止めないよう
    // `json_valid` で分岐し（不正 JSON はエラーになる `json()` を避けて生文字列で保持）、
    // 既存の metadata_json（identity 由来）が入っている経路も壊さない（valid ならそこへ挿す・
    // 不正でも `_original_metadata` に退避してから legacy_soul を足す）。
    let legacy_cols = [
        "social_style_json",
        "personality_json",
        "thinking_style_json",
        "custom_traits_json",
    ];
    let present: Vec<&str> = legacy_cols
        .into_iter()
        .filter(|c| column_exists(conn, "soul", c).unwrap_or(false))
        .collect();
    if !present.is_empty() {
        let obj_fields = present
            .iter()
            .map(|c| format!("'{c}', CASE WHEN json_valid(s.{c}) THEN json(s.{c}) ELSE s.{c} END"))
            .collect::<Vec<_>>()
            .join(", ");
        // 不正 JSON を含む行数を先に数える（起動は止めず、warn で可視化するため）。
        // 「壊れていた」= 実在列のいずれかが非 NULL かつ `json_valid` でない行。
        let broken_pred = present
            .iter()
            .map(|c| format!("(s.{c} IS NOT NULL AND NOT json_valid(s.{c}))"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let broken_json_rows: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM soul s WHERE {broken_pred}"),
            [],
            |r| r.get(0),
        )?;

        let sql = format!(
            "UPDATE agents
             SET metadata_json = json_set(
                 CASE
                     WHEN metadata_json IS NULL THEN '{{}}'
                     WHEN json_valid(metadata_json) THEN metadata_json
                     ELSE json_object('_original_metadata', metadata_json)
                 END,
                 '$.legacy_soul',
                 json((SELECT json_object({obj_fields}) FROM soul s WHERE s.agent_id = agents.agent_id))
             )
             WHERE agent_id IN (SELECT agent_id FROM soul)"
        );
        let salvaged_rows = conn.execute(&sql, [])?;

        // 移行の可視化（#480）: この世代の DB は今まで起動できず、利用者は何が起きるか分からない。
        // 黙って通すと「何か消えたかも」と疑うことすらできないため、退避したことと件数を残す。
        if salvaged_rows > 0 {
            tracing::info!(
                salvaged_rows,
                "旧 soul の付随データ（JSON 列）を agents.metadata_json の legacy_soul へ退避した"
            );
        }
        if broken_json_rows > 0 {
            tracing::warn!(
                broken_json_rows,
                "旧 soul の付随データに不正 JSON が含まれ、構造化せず生文字列として退避した（起動は継続）"
            );
        }
    }

    conn.execute_batch("DROP TABLE IF EXISTS soul; DROP TABLE IF EXISTS identity;")?;
    Ok(())
}

#[cfg(test)]
mod migration_tests;
