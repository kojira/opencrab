use rusqlite::Connection;

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
        // **改名のみ。行の追加・削除・書き換えは一切しない。**
        //
        // `ALTER TABLE ... RENAME TO` と `ALTER TABLE ... RENAME COLUMN` は
        // テーブルの再構築を伴わない（SQLite が sqlite_schema の DDL 文字列を
        // 書き換えるだけ）ので、行はそのまま生き、**逆向きの RENAME で戻せる**
        // ＝可逆。一意制約 `(user_id, agent_id)` の作り直し（→ `(platform, user_id,
        // agent_id)`）は再構築が要る非可逆な変更なので、ここには**混ぜない**。
        //
        // 冪等性: 新規DB は SCHEMA_SQL 側で既に新しい名前なので、どの分岐も走らない。
        up: |conn| {
            if table_exists(conn, "trusted_discord_users")? && !table_exists(conn, "trusted_users")?
            {
                conn.execute_batch("ALTER TABLE trusted_discord_users RENAME TO trusted_users")?;
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
];

/// per-agent の Nostr sub-gateway 設定。秘密鍵はエージェント毎に隔離（鍵の共有防止）。
const AGENT_NOSTR_CONFIG_SQL: &str = "
CREATE TABLE IF NOT EXISTS agent_nostr_config (
    agent_id TEXT PRIMARY KEY,
    secret_key TEXT NOT NULL,
    relays_json TEXT NOT NULL DEFAULT '[]',
    filter_json TEXT NOT NULL DEFAULT '{}',
    enabled INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);
";

/// per-agent の Nostr 受信転記先設定（issue #252 段階 A）。
///
/// エージェントが Nostr で受け取った自分宛の受信を、エージェント単位で設定した 1 つの
/// Discord チャンネル（webhook）へ転記するための宛先。
///
/// - `enabled`: 既定 **0（無効）**。行を作っただけでは転記しない（fail-closed / #240 と同じ轍を
///   踏まない）。行が無いエージェントも無効として扱う（上位の解決が fail-closed）。
/// - `webhook_url`: 転記先の webhook URL。NULL / 空なら転記しない。URL の妥当性検証は
///   db 層では行わず、`opencrab_actions::webhook_target::resolve_nostr_relay_webhook` が担う
///   （db クレートは Discord/webhook の語彙に依存しない）。
const AGENT_NOSTR_RELAY_CONFIG_SQL: &str = "
CREATE TABLE IF NOT EXISTS agent_nostr_relay_config (
    agent_id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL DEFAULT 0,
    webhook_url TEXT,
    updated_at TEXT NOT NULL
);
";

/// per-agent のハートビート設定（#247）。**エージェント自身が触れる唯一の自律実行設定**。
///
/// - `enabled`: 既定 **0（無効）**。設定を作っただけで自律実行が始まらないようにする（#240）。
/// - `interval_secs`: NULL = 運用者の既定（設定ファイルの `[agent] heartbeat_interval_secs`）
///   に従う。値の下限は設定ファイル（`[agent] heartbeat_min_interval_secs`）で運用者が決め、
///   書き込み口（`set_my_heartbeat`）が下限より短い要求を**拒否**する。DB 側に CHECK は
///   置かない（下限は運用者が変えられる値なので、スキーマに焼き付けると変更のたびに
///   マイグレーションが要る）。
///
/// 行が無い / 壊れているときは**無効**として扱う（`queries::resolve_agent_heartbeat`）。
const AGENT_HEARTBEAT_CONFIG_SQL: &str = "
CREATE TABLE IF NOT EXISTS agent_heartbeat_config (
    agent_id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL DEFAULT 0,
    interval_secs INTEGER,
    updated_at TEXT NOT NULL
);
";

/// per-agent の MCP サーバ設定。1 エージェント × 複数サーバ（主キー (agent_id, name)）。
const AGENT_MCP_CONFIG_SQL: &str = "
CREATE TABLE IF NOT EXISTS agent_mcp_config (
    agent_id TEXT NOT NULL,
    name TEXT NOT NULL,
    command TEXT NOT NULL,
    args_json TEXT NOT NULL DEFAULT '[]',
    env_json TEXT NOT NULL DEFAULT '{}',
    trusted_only INTEGER NOT NULL DEFAULT 1,
    enabled INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (agent_id, name)
);
";

/// スキル利用のセッション単位記録（スリープ棚卸しの弱い利用ヒント用）。
/// 注入時ではなく「利用が検出された時」に記録する（名前一致ベース, ノイズあり）。
const SKILL_USAGE_LOG_SQL: &str = "
CREATE TABLE IF NOT EXISTS skill_usage_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_skill_usage_log_skill ON skill_usage_log(skill_id);
CREATE INDEX IF NOT EXISTS idx_skill_usage_log_session ON skill_usage_log(session_id);
";

/// ダッシュボードから編集する LLM/voice プロバイダー設定のオーバーライド。
/// TOML を土台に、行/フィールドが存在するものだけ上書きする。
const PROVIDER_SETTINGS_SQL: &str = "
CREATE TABLE IF NOT EXISTS llm_provider_overrides (
    provider TEXT PRIMARY KEY,
    enabled INTEGER,
    api_key TEXT,
    base_url TEXT,
    default_model TEXT,
    reasoning_effort TEXT,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS voice_config_override (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    config_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
";

/// version 2: タスク台帳。
///
/// `SCHEMA_SQL` 末尾の同名ブロックと**文面を完全一致**させること
/// （`task_ledger_schema_parity` テストが sqlite_master の SQL 文字列で比較する）。
const TASK_LEDGER_SQL: &str = r#"
-- ============================================
-- TASK LEDGER: 前向きワーキング状態（goal/契約/進捗/決定）
-- ============================================
CREATE TABLE IF NOT EXISTS task_ledger (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    goal TEXT NOT NULL,
    contract TEXT,
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'done', 'abandoned')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_ledger_session
    ON task_ledger(agent_id, session_id, status);
CREATE UNIQUE INDEX IF NOT EXISTS idx_task_ledger_one_active
    ON task_ledger(agent_id, session_id) WHERE status = 'active';

CREATE TABLE IF NOT EXISTS task_progress (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES task_ledger(id) ON DELETE CASCADE,
    kind TEXT NOT NULL DEFAULT 'progress'
        CHECK (kind IN ('progress', 'decision', 'blocker')),
    content TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_progress_task ON task_progress(task_id);
"#;

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

/// FROZEN — schema version 1 baseline。
///
/// この関数へは**今後追記しないこと**。スキーマ変更は version 2 以降の番号付き
/// [`MIGRATIONS`] エントリとして追加する。ここは version 1 として確定した履歴であり、
/// `backfill_short_ids` 呼び出しや `migrate_soul_identity_to_agents` 含めて凍結する。
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

    // 旧 soul テーブル向けマイグレーション（新規DBでは soul 不存在のためスキップ）
    if table_exists(conn, "soul")? {
        // soul.social_style_json カラムDROP（dead code削除）
        let has_social_style: bool = conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('soul') WHERE name='social_style_json'",
            )?
            .query_row([], |row| row.get::<_, i64>(0))
            .map(|c| c > 0)
            .unwrap_or(false);
        if has_social_style {
            conn.execute_batch("ALTER TABLE soul DROP COLUMN social_style_json")?;
        }

        // soul.thinking_style_json カラムDROP（dead code削除）
        let has_thinking_style: bool = conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('soul') WHERE name='thinking_style_json'",
            )?
            .query_row([], |row| row.get::<_, i64>(0))
            .map(|c| c > 0)
            .unwrap_or(false);
        if has_thinking_style {
            conn.execute_batch("ALTER TABLE soul DROP COLUMN thinking_style_json")?;
        }
    }

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
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_index_nodes_short_id ON memory_index_nodes(agent_id, short_id) WHERE short_id IS NOT NULL",
        )?;
        crate::queries::backfill_short_ids(conn)
            .map_err(|e| rusqlite::Error::InvalidParameterName(format!("{e}")))?;
    }

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

    conn.execute_batch("DROP TABLE IF EXISTS soul; DROP TABLE IF EXISTS identity;")?;
    Ok(())
}

/// 新規インストール用のスキーマ定義（全て `CREATE ... IF NOT EXISTS`）。
///
/// 注意: baseline 済みの既存DB（`user_version >= 1`）では、この `SCHEMA_SQL` は
/// **再実行されない**。したがってここにテーブル/列を追加しただけでは既存DBには反映されない。
/// 新しいテーブル/列は、必ず対応する番号付きマイグレーションを [`MIGRATIONS`] にも追加して
/// 既存DBへ届けること。
const SCHEMA_SQL: &str = r#"
-- ============================================
-- AGENTS: soul + identity 統合 + エージェント別モデル
-- ============================================
CREATE TABLE IF NOT EXISTS agents (
    agent_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    job_title TEXT,
    organization TEXT,
    image_url TEXT,
    persona_name TEXT NOT NULL,
    personality TEXT,
    instructions TEXT NOT NULL DEFAULT '',
    heartbeat_instructions TEXT NOT NULL DEFAULT '',
    model TEXT,
    reasoning_effort TEXT,
    web_search INTEGER,
    metadata_json TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- ============================================
-- MEMORY: キュレーション記憶
-- ============================================
CREATE TABLE IF NOT EXISTS memory_curated (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    category TEXT NOT NULL,
    content TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_memory_curated_agent ON memory_curated(agent_id);
CREATE INDEX IF NOT EXISTS idx_memory_curated_category ON memory_curated(agent_id, category);

-- ============================================
-- MEMORY: セッションログ
-- ============================================
CREATE TABLE IF NOT EXISTS memory_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    log_type TEXT NOT NULL,
    content TEXT NOT NULL,
    speaker_id TEXT,
    turn_number INTEGER,
    metadata_json TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_memory_sessions_agent ON memory_sessions(agent_id);
CREATE INDEX IF NOT EXISTS idx_memory_sessions_session ON memory_sessions(agent_id, session_id);

-- ============================================
-- MEMORY: FTS5全文検索
-- ============================================
CREATE VIRTUAL TABLE IF NOT EXISTS memory_sessions_fts USING fts5(
    content,
    agent_id UNINDEXED,
    session_id UNINDEXED,
    log_type UNINDEXED
);

-- ============================================
-- Skills: スキル管理
-- ============================================
CREATE TABLE IF NOT EXISTS skills (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    situation_pattern TEXT NOT NULL,
    guidance TEXT NOT NULL,
    source_type TEXT NOT NULL DEFAULT 'standard',
    source_context TEXT,
    file_path TEXT,
    effectiveness REAL,
    usage_count INTEGER NOT NULL DEFAULT 0,
    is_active INTEGER NOT NULL DEFAULT 1,
    permission TEXT NOT NULL DEFAULT '"agent"',
    archived INTEGER NOT NULL DEFAULT 0,
    last_used_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_skills_agent ON skills(agent_id);
CREATE INDEX IF NOT EXISTS idx_skills_active ON skills(agent_id, is_active);

-- ============================================
-- Impressions: 心象
-- ============================================
-- スコープは **agent × target**（#314）。同じ相手なら Discord でも Nostr でも
-- 同じ 1 行を更新・参照する（「同じ人は同じ人」）。`session_id` は
-- 「**最後に更新されたセッション**」の記録として残す（時系列の辿り先）。
CREATE TABLE IF NOT EXISTS impressions (
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

-- ============================================
-- LLM利用メトリクス
-- ============================================
CREATE TABLE IF NOT EXISTS llm_usage_metrics (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    session_id TEXT,
    timestamp TEXT NOT NULL,

    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    purpose TEXT NOT NULL,
    task_type TEXT,
    complexity TEXT,

    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    total_tokens INTEGER NOT NULL,
    estimated_cost_usd REAL NOT NULL,

    latency_ms INTEGER NOT NULL,
    time_to_first_token_ms INTEGER,

    quality_score REAL,
    self_evaluation TEXT,
    task_success INTEGER,
    would_use_again INTEGER,
    better_model_suggestion TEXT,

    tags TEXT,

    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_llm_metrics_agent ON llm_usage_metrics(agent_id);
CREATE INDEX IF NOT EXISTS idx_llm_metrics_model ON llm_usage_metrics(model);
CREATE INDEX IF NOT EXISTS idx_llm_metrics_timestamp ON llm_usage_metrics(timestamp);

-- ============================================
-- モデル経験ノート: エージェントが自由に書く定性的な知見
-- ============================================
CREATE TABLE IF NOT EXISTS model_experience_notes (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    provider TEXT,
    model TEXT,
    situation TEXT NOT NULL,
    observation TEXT NOT NULL,
    recommendation TEXT,
    tags TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_model_exp_agent ON model_experience_notes(agent_id);
CREATE INDEX IF NOT EXISTS idx_model_exp_model ON model_experience_notes(agent_id, provider, model);

-- ============================================
-- モデル価格情報
-- ============================================
CREATE TABLE IF NOT EXISTS model_pricing (
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    input_price_per_1m REAL NOT NULL,
    output_price_per_1m REAL NOT NULL,
    context_window INTEGER,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (provider, model)
);

-- ============================================
-- ハートビートログ
-- ============================================
CREATE TABLE IF NOT EXISTS heartbeat_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    decision TEXT NOT NULL,
    result_json TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_heartbeat_agent ON heartbeat_log(agent_id);

-- ============================================
-- ハートビート指示の監査ログ
-- ============================================
CREATE TABLE IF NOT EXISTS heartbeat_instructions_audit (
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
    ON heartbeat_instructions_audit(agent_id, created_at DESC);

-- ============================================
-- セッション状態
-- ============================================
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    mode TEXT NOT NULL DEFAULT 'facilitated',
    theme TEXT NOT NULL,
    phase TEXT NOT NULL DEFAULT 'divergent',
    turn_number INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'active',
    participant_ids_json TEXT NOT NULL DEFAULT '[]',
    facilitator_id TEXT,
    done_count INTEGER NOT NULL DEFAULT 0,
    max_turns INTEGER,
    metadata_json TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- ============================================
-- エージェントのセッション参加状態
-- ============================================
CREATE TABLE IF NOT EXISTS agent_sessions (
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    last_speech_at TEXT,
    done_declared INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (agent_id, session_id)
);

-- ============================================
-- Discordチャンネル設定
-- ============================================
CREATE TABLE IF NOT EXISTS discord_channel_config (
    channel_id TEXT NOT NULL,
    agent_id TEXT NOT NULL DEFAULT '',
    guild_id TEXT NOT NULL,
    channel_name TEXT NOT NULL DEFAULT '',
    readable INTEGER NOT NULL DEFAULT 1,
    writable INTEGER NOT NULL DEFAULT 1,
    whitelisted INTEGER NOT NULL DEFAULT 0,
    heartbeat_enabled INTEGER NOT NULL DEFAULT 1,
    heartbeat_interval_secs INTEGER,
    heartbeat_instructions TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL,
    PRIMARY KEY (channel_id, agent_id)
);
CREATE INDEX IF NOT EXISTS idx_discord_channel_guild ON discord_channel_config(guild_id);

-- ============================================
-- ペルソナプリセット
-- ============================================
CREATE TABLE IF NOT EXISTS soul_presets (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    preset_name TEXT NOT NULL,
    persona_name TEXT NOT NULL,
    custom_traits_json TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_soul_presets_agent ON soul_presets(agent_id);

-- ============================================
-- エージェント別Discord Bot設定
-- ============================================
CREATE TABLE IF NOT EXISTS agent_discord_config (
    agent_id TEXT PRIMARY KEY,
    bot_token TEXT NOT NULL,
    owner_discord_id TEXT NOT NULL DEFAULT '',
    enabled INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL
);

-- ============================================
-- エージェント別 Nostr sub-gateway 設定（秘密鍵は per-agent 隔離）
-- ============================================
CREATE TABLE IF NOT EXISTS agent_nostr_config (
    agent_id TEXT PRIMARY KEY,
    secret_key TEXT NOT NULL,
    relays_json TEXT NOT NULL DEFAULT '[]',
    filter_json TEXT NOT NULL DEFAULT '{}',
    enabled INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);

-- ============================================
-- エージェント別 Nostr 受信 → Discord 転記先（issue #252 段階 A）
-- 既定は無効（行があっても enabled=0 なら転記しない / fail-closed）
-- ============================================
CREATE TABLE IF NOT EXISTS agent_nostr_relay_config (
    agent_id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL DEFAULT 0,
    webhook_url TEXT,
    updated_at TEXT NOT NULL
);

-- ============================================
-- Agent Webhook Config (subtask/tool/lifecycle webhook defaults)
-- ============================================
CREATE TABLE IF NOT EXISTS agent_webhook_config (
    scope        TEXT NOT NULL DEFAULT 'agent',
    agent_id     TEXT NOT NULL,
    tool_name    TEXT NOT NULL DEFAULT '',
    kind         TEXT NOT NULL DEFAULT 'subtask',
    url          TEXT NOT NULL,
    events_json  TEXT,
    enabled      INTEGER NOT NULL DEFAULT 1,
    name         TEXT,
    created_by   TEXT,
    output_mode  TEXT NOT NULL DEFAULT 'summary',
    max_chars    INTEGER NOT NULL DEFAULT 1500,
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (scope, agent_id, tool_name, kind)
);
CREATE INDEX IF NOT EXISTS idx_agent_webhook_agent ON agent_webhook_config(agent_id);

-- ============================================
-- 記憶インデックス: 階層ツリーノード
-- ============================================
CREATE TABLE IF NOT EXISTS memory_index_nodes (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    parent_id TEXT REFERENCES memory_index_nodes(id) ON DELETE CASCADE,
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
CREATE INDEX IF NOT EXISTS idx_mem_idx_agent ON memory_index_nodes(agent_id);
CREATE INDEX IF NOT EXISTS idx_mem_idx_parent ON memory_index_nodes(agent_id, parent_id);
CREATE INDEX IF NOT EXISTS idx_mem_idx_type ON memory_index_nodes(agent_id, node_type);
CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_index_nodes_short_id ON memory_index_nodes(agent_id, short_id) WHERE short_id IS NOT NULL;

-- ============================================
-- 記憶インデックス: ウォーターマーク（進捗管理）
-- ============================================
CREATE TABLE IF NOT EXISTS memory_index_watermark (
    agent_id TEXT PRIMARY KEY,
    last_indexed_log_id INTEGER NOT NULL DEFAULT 0,
    last_indexed_at TEXT NOT NULL,
    total_nodes INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS daily_log_index_watermark (
    agent_id TEXT NOT NULL PRIMARY KEY,
    last_indexed_date TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- ============================================
-- Co-Agent信頼関係
-- ============================================
CREATE TABLE IF NOT EXISTS trusted_co_agents (
    id           TEXT PRIMARY KEY,
    agent_id     TEXT NOT NULL,
    co_agent_id  TEXT NOT NULL,
    allowed_actions TEXT,
    created_by   TEXT NOT NULL,
    created_at   DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (agent_id, co_agent_id)
);
CREATE INDEX IF NOT EXISTS idx_trusted_co_agents_agent ON trusted_co_agents(agent_id);

-- ============================================
-- 信頼済みユーザー（経路ごとの識別子空間）
-- ============================================
-- 旧名は `trusted_discord_users` / `discord_user_id`。Discord 以外の経路（web / rest）も
-- 同じ表を使うので #159 (v17) で改名した。旧DBは v17 の RENAME で追従する。
CREATE TABLE IF NOT EXISTS trusted_users (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  permission TEXT NOT NULL DEFAULT 'user',
  created_by TEXT NOT NULL DEFAULT 'owner',
  created_at TEXT NOT NULL,
  display_name TEXT NOT NULL DEFAULT '',
  -- その識別子が「どの経路のものか」（#214）。列追加前の行は全て Discord の
  -- 識別子空間なので DEFAULT 'discord'（`pending_interactions.platform` の前例に倣う）。
  -- 一意制約は (user_id, agent_id) のまま据え置く（作り直しは非可逆なので #159 の最終段）。
  platform TEXT NOT NULL DEFAULT 'discord',
  UNIQUE (user_id, agent_id)
);
CREATE INDEX IF NOT EXISTS idx_trusted_users_agent ON trusted_users(agent_id);

-- ============================================
-- エージェント別メモリインデックス設定
-- ============================================
CREATE TABLE IF NOT EXISTS agent_memory_index_config (
    agent_id TEXT PRIMARY KEY,
    batch_size INTEGER NOT NULL DEFAULT 50,
    threshold INTEGER NOT NULL DEFAULT 20,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_skill_consolidation_at TEXT
);

-- スキル利用のセッション単位記録（スリープ棚卸しの弱い利用ヒント用）
CREATE TABLE IF NOT EXISTS skill_usage_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_skill_usage_log_skill ON skill_usage_log(skill_id);
CREATE INDEX IF NOT EXISTS idx_skill_usage_log_session ON skill_usage_log(session_id);

-- ============================================
-- エージェント別許可コマンド（動的追加）
-- ============================================
CREATE TABLE IF NOT EXISTS agent_allowed_commands (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    command TEXT NOT NULL,
    added_by TEXT NOT NULL DEFAULT 'owner',
    added_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (agent_id, command)
);
CREATE INDEX IF NOT EXISTS idx_agent_allowed_commands_agent ON agent_allowed_commands(agent_id);

-- ============================================
-- LLM入出力ログ
-- ============================================
CREATE TABLE IF NOT EXISTS llm_logs (
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
    trigger_message_id TEXT,
    is_bot_iteration INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER,
    cache_creation_tokens INTEGER,
    created_at TEXT DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_llm_logs_agent ON llm_logs(agent_id);
CREATE INDEX IF NOT EXISTS idx_llm_logs_created ON llm_logs(agent_id, created_at DESC);

-- ============================================
-- インポート同期状態
-- ============================================
CREATE TABLE IF NOT EXISTS import_sync_state (
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
    ON import_sync_state(agent_id);

-- ============================================
-- エージェントログ
-- ============================================
CREATE TABLE IF NOT EXISTS agent_logs (
    id TEXT PRIMARY KEY,
    agent_id TEXT,
    level TEXT NOT NULL,
    context TEXT NOT NULL,
    message TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_agent_logs_agent ON agent_logs(agent_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_agent_logs_level ON agent_logs(level, created_at DESC);

-- ============================================
-- A2UI: 保留中のインタラクション
-- ============================================
CREATE TABLE IF NOT EXISTS pending_interactions (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    message_id TEXT,
    platform TEXT NOT NULL DEFAULT 'discord',
    surface_id TEXT NOT NULL,
    a2ui_components_json TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    response_json TEXT,
    responder_id TEXT,
    owner_only INTEGER NOT NULL DEFAULT 1,
    timeout_secs INTEGER NOT NULL DEFAULT 300,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    responded_at TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_pending_interactions_agent
    ON pending_interactions(agent_id, status);
CREATE INDEX IF NOT EXISTS idx_pending_interactions_session
    ON pending_interactions(session_id, status);
CREATE INDEX IF NOT EXISTS idx_pending_interactions_surface
    ON pending_interactions(surface_id);

-- ============================================
-- TASK LEDGER: 前向きワーキング状態（goal/契約/進捗/決定）
-- ============================================
CREATE TABLE IF NOT EXISTS task_ledger (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    goal TEXT NOT NULL,
    contract TEXT,
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'done', 'abandoned')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_ledger_session
    ON task_ledger(agent_id, session_id, status);
CREATE UNIQUE INDEX IF NOT EXISTS idx_task_ledger_one_active
    ON task_ledger(agent_id, session_id) WHERE status = 'active';

CREATE TABLE IF NOT EXISTS task_progress (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES task_ledger(id) ON DELETE CASCADE,
    kind TEXT NOT NULL DEFAULT 'progress'
        CHECK (kind IN ('progress', 'decision', 'blocker')),
    content TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_progress_task ON task_progress(task_id);
"#;

#[cfg(test)]
mod migration_tests {
    use super::*;
    use rusqlite::Connection;

    /// A. バージョン管理導入前の旧DBを模して、baseline が再適用され version 1 に
    /// スタンプされることを検証する。
    #[test]
    fn baseline_reconciles_pre_versioning_db() {
        let conn = crate::init_memory().expect("init");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // 旧DBを模す: version を 0 に戻し、baseline が再追加する列を落とす。
        conn.execute_batch("PRAGMA user_version = 0").unwrap();
        conn.execute_batch("ALTER TABLE skills DROP COLUMN archived")
            .unwrap();
        assert!(!column_exists(&conn, "skills", "archived").unwrap());

        // 再初期化で baseline + 番号付きマイグレーションが走り、列が復活し最新版にスタンプされる。
        initialize(&conn).expect("re-initialize");
        assert!(column_exists(&conn, "skills", "archived").unwrap());
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
    }

    /// B. 冪等性: baseline 済みDBで initialize を再実行しても破壊的再構築は走らず、
    /// 既存データが保持される。
    #[test]
    fn initialize_is_idempotent_and_non_destructive() {
        let conn = crate::init_memory().expect("init");
        conn.execute_batch(
            "INSERT INTO agents (agent_id, name, persona_name) VALUES ('sentinel', 'n', 'p')",
        )
        .unwrap();

        initialize(&conn).expect("second initialize");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agents WHERE agent_id = 'sentinel'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "sentinel row must survive (baseline not re-run)");
    }

    fn create_marker(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch("CREATE TABLE test_marker (id INTEGER)")
    }

    /// C. 番号付きマイグレーションのランナー: 未適用の version 2 を適用し、
    /// 再実行では no-op になる（version gate）。
    #[test]
    fn run_migrations_applies_and_then_skips() {
        let conn = crate::init_memory().expect("init");
        // 実 MIGRATIONS 適用済み（= 最新版）なので、fake v2 が未適用となる状態に戻す。
        conn.execute_batch("PRAGMA user_version = 1").unwrap();
        let fake = &[Migration {
            version: 2,
            description: "add test_marker",
            up: create_marker,
        }];

        run_migrations(&conn, fake).expect("apply v2");
        assert!(table_exists(&conn, "test_marker").unwrap());
        assert_eq!(schema_version(&conn).unwrap(), 2);

        // 再実行は no-op（既に version 2）。
        run_migrations(&conn, fake).expect("re-run no-op");
        assert_eq!(schema_version(&conn).unwrap(), 2);
    }

    fn fail_after_marker(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch("CREATE TABLE test_marker (id INTEGER)")?;
        Err(rusqlite::Error::InvalidQuery)
    }

    /// D. マイグレーション失敗時は、その up の変更と version スタンプが
    /// トランザクションごとロールバックされる。
    #[test]
    fn failed_migration_rolls_back_and_leaves_version() {
        let conn = crate::init_memory().expect("init");
        // 実 MIGRATIONS 適用済みなので、fake v2 が適用対象となる状態に戻す。
        conn.execute_batch("PRAGMA user_version = 1").unwrap();
        let fake = &[Migration {
            version: 2,
            description: "fails",
            up: fail_after_marker,
        }];

        let result = run_migrations(&conn, fake);
        assert!(result.is_err());
        assert!(!table_exists(&conn, "test_marker").unwrap());
        assert_eq!(schema_version(&conn).unwrap(), BASELINE_VERSION);
    }

    /// E. 実 MIGRATIONS の version は厳密増加・全て baseline より大きい・重複なし。
    #[test]
    fn agent_sessions_backfill_migration_v4() {
        let conn = crate::init_memory().expect("init");
        // v3 状態に戻し、v4 の backfill 対象となる sessions 行を用意する
        // （うち1件は壊れた JSON — skip され、他の行は影響を受けないこと）。
        conn.execute_batch("PRAGMA user_version = 3").unwrap();
        conn.execute_batch("DELETE FROM agent_sessions").unwrap();
        conn.execute_batch(
            "INSERT INTO sessions (id, mode, theme, phase, turn_number, status, participant_ids_json, done_count, created_at, updated_at)
             VALUES ('s1', 'discord', 't', 'active', 0, 'active', '[\"a1\",\"a2\"]', 0, '2026-01-01', '2026-01-01'),
                    ('s2', 'discord', 't', 'active', 0, 'active', 'not-json', 0, '2026-01-01', '2026-01-01'),
                    ('s3', 'discord', 't', 'active', 0, 'active', '[\"a1\"]', 0, '2026-01-01', '2026-01-01')",
        )
        .unwrap();

        run_migrations(&conn, MIGRATIONS).expect("v4 migration");

        let participants = crate::queries::list_session_participants(&conn, "s1").unwrap();
        assert_eq!(participants, vec!["a1".to_string(), "a2".to_string()]);
        assert!(crate::queries::list_session_participants(&conn, "s2")
            .unwrap()
            .is_empty());
        assert_eq!(
            crate::queries::count_sessions_for_agent(&conn, "a1").unwrap(),
            2
        );
        // 部分一致の誤マッチが無いこと（旧 LIKE 実装は "a" が "a1" にもマッチした）
        assert_eq!(
            crate::queries::count_sessions_for_agent(&conn, "a").unwrap(),
            0
        );
    }

    #[test]
    fn memory_index_fk_cascade_and_check_enforced() {
        let conn = crate::init_memory().expect("init");
        conn.execute_batch(
            "INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, title, summary, created_at, updated_at)
             VALUES ('r', 'a1', NULL, 'root', 't', 's', '2026-01-01', '2026-01-01'),
                    ('c', 'a1', 'r', 'topic', 't', 's', '2026-01-01', '2026-01-01')",
        )
        .unwrap();
        // CHECK: 不正 node_type は拒否
        assert!(conn
            .execute_batch(
                "INSERT INTO memory_index_nodes (id, agent_id, node_type, title, summary, created_at, updated_at)
                 VALUES ('x', 'a1', 'bogus', 't', 's', '2026-01-01', '2026-01-01')",
            )
            .is_err());
        // FK: 存在しない親は拒否
        assert!(conn
            .execute_batch(
                "INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, title, summary, created_at, updated_at)
                 VALUES ('y', 'a1', 'nope', 'topic', 't', 's', '2026-01-01', '2026-01-01')",
            )
            .is_err());
        // CASCADE: 親削除で子も消える
        conn.execute("DELETE FROM memory_index_nodes WHERE id = 'r'", [])
            .unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_index_nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn memory_index_rebuild_migration_v5_upgrades_legacy_table() {
        let conn = crate::init_memory().expect("init");
        // v4 時点の旧テーブル形（FK/CHECK なし）を再現する
        conn.execute_batch("PRAGMA user_version = 4").unwrap();
        conn.execute_batch(
            "DROP TABLE memory_index_nodes;
             CREATE TABLE memory_index_nodes (
                id TEXT PRIMARY KEY, agent_id TEXT NOT NULL, parent_id TEXT,
                node_type TEXT NOT NULL, source_type TEXT NOT NULL DEFAULT 'session_log',
                title TEXT NOT NULL, summary TEXT NOT NULL,
                start_log_id INTEGER, end_log_id INTEGER, source_session_id TEXT,
                date_from TEXT, date_to TEXT,
                depth INTEGER NOT NULL DEFAULT 0, child_count INTEGER NOT NULL DEFAULT 0,
                token_count INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, short_id TEXT
             );
             INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, title, summary, created_at, updated_at)
             VALUES ('r', 'a1', NULL, 'root', 't', 's', '2026-01-01', '2026-01-01'),
                    ('c', 'a1', 'r', 'topic', 't', 's', '2026-01-01', '2026-01-01'),
                    ('orphan', 'a1', 'ghost', 'topic', 't', 's', '2026-01-01', '2026-01-01'),
                    ('junk', 'a1', NULL, 'bogus_type', 't', 's', '2026-01-01', '2026-01-01')",
        )
        .unwrap();

        run_migrations(&conn, MIGRATIONS).expect("v5 migration");

        // FK が付与され、正常行は保存、孤児は parent NULL 化、不正 node_type は除外
        let fk_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_list('memory_index_nodes')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fk_count, 1);
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM memory_index_nodes ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            ids,
            vec!["c".to_string(), "orphan".to_string(), "r".to_string()]
        );
        let orphan_parent: Option<String> = conn
            .query_row(
                "SELECT parent_id FROM memory_index_nodes WHERE id = 'orphan'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphan_parent, None);
    }

    #[test]
    fn task_ledger_restart_count_migration_v6() {
        // 新規DB: init 直後から列がある（v6 適用済み）。
        let conn = crate::init_memory().expect("init");
        assert!(column_exists(&conn, "task_ledger", "restart_count").unwrap());

        // 既存DB（v5 時点 = 列なし）からの upgrade。
        conn.execute_batch(
            "DROP TABLE task_progress; DROP TABLE task_ledger; PRAGMA user_version = 1",
        )
        .unwrap();
        conn.execute_batch(TASK_LEDGER_SQL).unwrap();
        conn.execute_batch("PRAGMA user_version = 5").unwrap();
        conn.execute_batch(
            "INSERT INTO task_ledger (agent_id, session_id, goal, status, created_at, updated_at)
             VALUES ('a1', 's1', 'g', 'active', '2026-01-01', '2026-01-01')",
        )
        .unwrap();
        assert!(!column_exists(&conn, "task_ledger", "restart_count").unwrap());

        run_migrations(&conn, MIGRATIONS).expect("v6 migration");
        assert!(column_exists(&conn, "task_ledger", "restart_count").unwrap());
        // 既存行は DEFAULT 0 で読める
        let count: i64 = conn
            .query_row(
                "SELECT restart_count FROM task_ledger WHERE agent_id = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
    }

    #[test]
    fn memory_index_keywords_migration_v7() {
        // 新規DB: init 直後から列と FTS 表がある。
        let conn = crate::init_memory().expect("init");
        assert!(column_exists(&conn, "memory_index_nodes", "keywords_json").unwrap());
        assert!(column_exists(&conn, "memory_index_nodes", "summary_refreshed_at").unwrap());
        assert!(table_exists(&conn, "memory_index_fts").unwrap());

        // 既存DB（v6 時点 = 列も FTS も無い）からの upgrade。
        conn.execute_batch(
            "DROP TABLE memory_index_fts;
             DROP TABLE memory_index_nodes;
             CREATE TABLE memory_index_nodes (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                parent_id TEXT REFERENCES memory_index_nodes(id) ON DELETE CASCADE,
                node_type TEXT NOT NULL CHECK (node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly')),
                source_type TEXT NOT NULL DEFAULT 'session_log',
                title TEXT NOT NULL, summary TEXT NOT NULL,
                start_log_id INTEGER, end_log_id INTEGER, source_session_id TEXT,
                date_from TEXT, date_to TEXT,
                depth INTEGER NOT NULL DEFAULT 0, child_count INTEGER NOT NULL DEFAULT 0,
                token_count INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, short_id TEXT
             );
             INSERT INTO memory_index_nodes (id, agent_id, node_type, title, summary, created_at, updated_at)
             VALUES ('t-legacy', 'a1', 'topic', '旧トピック', '旧要約テキスト', '2026-01-01', '2026-01-01');
             PRAGMA user_version = 6;",
        )
        .unwrap();
        assert!(!column_exists(&conn, "memory_index_nodes", "keywords_json").unwrap());

        run_migrations(&conn, MIGRATIONS).expect("v7 migration");
        assert!(column_exists(&conn, "memory_index_nodes", "keywords_json").unwrap());
        assert!(column_exists(&conn, "memory_index_nodes", "summary_refreshed_at").unwrap());
        // 既存行が FTS にバックフィルされ、trigram で部分一致検索できる
        let fts_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_index_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_rows, 1);
        let hits = crate::queries::search_index_nodes(&conn, "a1", "旧要約", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node_id, "t-legacy");
        // keywords_json は DEFAULT '[]' で読める
        assert_eq!(hits[0].keywords_json, "[]");
        // 再実行してもバックフィルは重複しない
        conn.execute_batch("PRAGMA user_version = 6").unwrap();
        run_migrations(&conn, MIGRATIONS).expect("v7 rerun");
        let fts_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_index_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_rows, 1);
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
    }

    #[test]
    fn migrations_versions_are_strictly_increasing() {
        let mut prev = BASELINE_VERSION;
        for m in MIGRATIONS {
            assert!(
                m.version > prev,
                "migration versions must be strictly increasing and > baseline"
            );
            prev = m.version;
        }
    }

    /// v19: Nostr 受信転記先の表が増えるだけで、既存の Nostr 設定
    /// （`agent_nostr_config`）の行は 1 つも動かない（#252 段階 A）。冪等でもある。
    #[test]
    fn agent_nostr_relay_config_migration_v19_adds_table_without_touching_existing_rows() {
        let conn = crate::init_memory().expect("init");
        // v18 相当の既存 DB を模す: 新表を落として version を 18 へ戻す。
        // 既存の Nostr 設定に行を 1 件入れておき、移行で動かないことを見る。
        conn.execute_batch(
            "DROP TABLE IF EXISTS agent_nostr_relay_config;
             INSERT INTO agent_nostr_config
               (agent_id, secret_key, relays_json, filter_json, enabled, updated_at)
               VALUES ('a1', 'nsec1keep', '[\"wss://yabu.me\"]', '{}', 1, '2026-01-01');
             PRAGMA user_version = 18",
        )
        .unwrap();
        assert!(!table_exists(&conn, "agent_nostr_relay_config").unwrap());

        initialize(&conn).expect("upgrade v18 -> v19");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        assert!(table_exists(&conn, "agent_nostr_relay_config").unwrap());

        // 新表は空で始まる（既定は「設定なし」＝無効 / fail-closed）。
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent_nostr_relay_config", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0);

        // 既存の Nostr 設定は 1 バイトも変わらない。
        let (secret, enabled): (String, i64) = conn
            .query_row(
                "SELECT secret_key, enabled FROM agent_nostr_config WHERE agent_id = 'a1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((secret.as_str(), enabled), ("nsec1keep", 1));

        // 行を入れてから再実行しても冪等（CREATE TABLE IF NOT EXISTS で消えない）。
        conn.execute_batch(
            "INSERT INTO agent_nostr_relay_config (agent_id, enabled, webhook_url, updated_at)
             VALUES ('a1', 1, 'https://discord.com/api/webhooks/1/tok', '2026-01-02')",
        )
        .unwrap();
        initialize(&conn).expect("idempotent");
        let kept: String = conn
            .query_row(
                "SELECT webhook_url FROM agent_nostr_relay_config WHERE agent_id = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, "https://discord.com/api/webhooks/1/tok");
    }

    /// G. v1 DB が v2 マイグレーションでタスク台帳テーブルを獲得する。
    #[test]
    fn task_ledger_migration_upgrades_v1_db() {
        let conn = crate::init_memory().expect("init");
        // v1 相当の既存DBを模す: タスク台帳を落として version 1 に戻す。
        conn.execute_batch(
            "DROP TABLE task_progress; DROP TABLE task_ledger; PRAGMA user_version = 1",
        )
        .unwrap();
        assert!(!table_exists(&conn, "task_ledger").unwrap());

        initialize(&conn).expect("upgrade v1 -> latest");
        assert!(table_exists(&conn, "task_ledger").unwrap());
        assert!(table_exists(&conn, "task_progress").unwrap());
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
    }

    /// v3: display_name 列の付与。v2 相当 DB（列なし）からのアップグレードと、
    /// 新規 DB（SCHEMA_SQL 由来で列あり）での冪等性の両方を確認する。
    #[test]
    fn display_name_migration_upgrades_v2_db() {
        let conn = crate::init_memory().expect("init");
        // 新規 DB には既に列がある（SCHEMA_SQL 由来。表名は v17 で改名済みの新しい方）
        assert!(column_exists(&conn, "trusted_users", "display_name").unwrap());

        // v2 相当の既存 DB を模す: 旧表名・列なしのテーブルに作り直して version 2 に戻す
        conn.execute_batch(
            "DROP TABLE trusted_users;
             CREATE TABLE trusted_discord_users (
               id TEXT PRIMARY KEY,
               discord_user_id TEXT NOT NULL,
               agent_id TEXT NOT NULL,
               permission TEXT NOT NULL DEFAULT 'user',
               created_by TEXT NOT NULL DEFAULT 'owner',
               created_at TEXT NOT NULL,
               UNIQUE (discord_user_id, agent_id)
             );
             PRAGMA user_version = 2",
        )
        .unwrap();
        assert!(!column_exists(&conn, "trusted_discord_users", "display_name").unwrap());

        initialize(&conn).expect("upgrade v2 -> latest");
        // v3 で列が付き、v17 で表ごと改名される
        assert!(column_exists(&conn, "trusted_users", "display_name").unwrap());
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // 再実行しても冪等
        initialize(&conn).expect("idempotent");
    }

    /// v16: `trusted_discord_users.platform` の付与（#214）。
    ///
    /// 列追加のみで、**既存行は従来の経路（`discord`）として生きる**こと
    /// （既存の信頼済みユーザーが移行で一斉に権限を失わない）。
    #[test]
    fn platform_migration_keeps_existing_rows_on_discord() {
        let conn = crate::init_memory().expect("init");
        // 新規 DB には既に列がある（SCHEMA_SQL 由来。表名は v17 で改名済みの新しい方）
        assert!(column_exists(&conn, "trusted_users", "platform").unwrap());

        // v15 相当の既存 DB を模す: 旧表名・platform 列なしのテーブルに作り直し、行を 1 件
        // 入れて version 15 へ戻す。
        conn.execute_batch(
            "DROP TABLE trusted_users;
             CREATE TABLE trusted_discord_users (
               id TEXT PRIMARY KEY,
               discord_user_id TEXT NOT NULL,
               agent_id TEXT NOT NULL,
               permission TEXT NOT NULL DEFAULT 'user',
               created_by TEXT NOT NULL DEFAULT 'owner',
               created_at TEXT NOT NULL,
               display_name TEXT NOT NULL DEFAULT '',
               UNIQUE (discord_user_id, agent_id)
             );
             INSERT INTO trusted_discord_users
               (id, discord_user_id, agent_id, permission, created_by, created_at, display_name)
               VALUES ('old-1', '42', 'a1', 'co_agent', 'owner', '2026-01-01', 'Crab B');
             PRAGMA user_version = 15",
        )
        .unwrap();
        assert!(!column_exists(&conn, "trusted_discord_users", "platform").unwrap());

        initialize(&conn).expect("upgrade v15 -> latest");
        // v16 で列が付き、v17 で表ごと改名される
        assert!(column_exists(&conn, "trusted_users", "platform").unwrap());
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // 既存行は残り、従来の経路として引ける（他の列も失われていない）。
        let (platform, permission): (String, String) = conn
            .query_row(
                "SELECT platform, permission FROM trusted_users WHERE id = 'old-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(platform, "discord");
        // 権限は失われない。表記だけ v18 (#234) がケバブケースへ移す。
        assert_eq!(permission, "co-agent");

        // 再実行しても冪等
        initialize(&conn).expect("idempotent");
    }

    /// v17: `trusted_discord_users` → `trusted_users` / `discord_user_id` → `user_id`（#159）。
    ///
    /// 改名のみで、**既存行は 1 件も失われず、値もそのまま**であること。
    /// 一意制約と索引も新しい名前で生き続けること。
    #[test]
    fn trusted_users_rename_migration_preserves_rows() {
        let conn = crate::init_memory().expect("init");
        // 新規 DB は SCHEMA_SQL 由来で既に新しい名前
        assert!(table_exists(&conn, "trusted_users").unwrap());
        assert!(!table_exists(&conn, "trusted_discord_users").unwrap());

        // v16 相当の既存 DB を模す: 旧表名・旧列名で作り直し、行を 2 件入れて version 16 へ戻す。
        conn.execute_batch(
            "DROP TABLE trusted_users;
             CREATE TABLE trusted_discord_users (
               id TEXT PRIMARY KEY,
               discord_user_id TEXT NOT NULL,
               agent_id TEXT NOT NULL,
               permission TEXT NOT NULL DEFAULT 'user',
               created_by TEXT NOT NULL DEFAULT 'owner',
               created_at TEXT NOT NULL,
               display_name TEXT NOT NULL DEFAULT '',
               platform TEXT NOT NULL DEFAULT 'discord',
               UNIQUE (discord_user_id, agent_id)
             );
             CREATE INDEX idx_trusted_discord_users_agent ON trusted_discord_users(agent_id);
             INSERT INTO trusted_discord_users
               (id, discord_user_id, agent_id, permission, created_by, created_at, display_name, platform)
               VALUES ('old-1', '42', 'a1', 'co_agent', 'owner', '2026-01-01', 'Crab B', 'discord'),
                      ('old-2', '43', 'a1', 'user', 'owner', '2026-01-02', '', 'discord');
             PRAGMA user_version = 16",
        )
        .unwrap();

        initialize(&conn).expect("upgrade v16 -> v17");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // 表と列が改名され、旧名は消えている
        assert!(table_exists(&conn, "trusted_users").unwrap());
        assert!(!table_exists(&conn, "trusted_discord_users").unwrap());
        assert!(column_exists(&conn, "trusted_users", "user_id").unwrap());
        assert!(!column_exists(&conn, "trusted_users", "discord_user_id").unwrap());

        // 行は 2 件とも残り、値もそのまま
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM trusted_users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
        let (user_id, permission, display_name, platform): (String, String, String, String) = conn
            .query_row(
                "SELECT user_id, permission, display_name, platform FROM trusted_users WHERE id = 'old-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(user_id, "42");
        // 改名では値は動かない。表記が変わるのは後続の v18 (#234)。
        assert_eq!(permission, "co-agent");
        assert_eq!(display_name, "Crab B");
        assert_eq!(platform, "discord");

        // 一意制約は改名後も効いている（列名だけが変わった）
        assert!(conn
            .execute(
                "INSERT INTO trusted_users (id, user_id, agent_id, permission, created_by, created_at, display_name, platform) \
                 VALUES ('dup', '42', 'a1', 'user', 'owner', '2026-01-03', '', 'discord')",
                [],
            )
            .is_err());

        // 索引も新しい名前で貼り直されている
        let idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_trusted_users_agent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1);
        let old_idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_trusted_discord_users_agent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_idx, 0);

        // 再実行しても冪等
        initialize(&conn).expect("idempotent");
    }

    /// v18: `trusted_users.permission` の表記統一（#234）。
    ///
    /// **移行前後で権限の判定結果が変わらない**こと（同じ人が同じ権限のまま、表記だけ
    /// ケバブケースになる）。行は 1 件も増減しない。旧い綴りのうち**判定が完全一致で
    /// 見ていたもの（`co_agent`）だけ**を移し、それ以外は触らない（権限を増やさない）。
    #[test]
    fn permission_spelling_migration_rewrites_rows_without_changing_who_is_a_co_agent() {
        use crate::queries::TrustedUserPermission;

        let conn = crate::init_memory().expect("init");
        // v17 相当の既存 DB を模す: 旧表記の行を含めて 4 件入れ、version 17 へ戻す。
        conn.execute_batch(
            "DELETE FROM trusted_users;
             INSERT INTO trusted_users
               (id, user_id, agent_id, permission, created_by, created_at, display_name, platform)
               VALUES ('r1', '42', 'a1', 'co_agent', 'owner', '2026-01-01', 'Crab B', 'discord'),
                      ('r2', '43', 'a1', 'user',     'owner', '2026-01-02', '',       'discord'),
                      ('r3', '44', 'a1', 'owner',    'owner', '2026-01-03', '',       'discord'),
                      ('r4', '45', 'a1', 'coagent',  'owner', '2026-01-04', 'Typo',   'discord');
             PRAGMA user_version = 17",
        )
        .unwrap();

        // 移行前の判定（旧い読み出し = permission == 'co_agent' の完全一致）。
        let judged_before: Vec<(String, bool)> = conn
            .prepare("SELECT id, permission FROM trusted_users ORDER BY id")
            .unwrap()
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)? == "co_agent"))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        initialize(&conn).expect("upgrade v17 -> v18");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // 行は増減しない。
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM trusted_users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 4);

        // 移行後の判定（新しい読み出し = 列挙型）。移行前と一致すること。
        let judged_after: Vec<(String, bool)> = conn
            .prepare("SELECT id, permission FROM trusted_users ORDER BY id")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    TrustedUserPermission::from_db_str(&r.get::<_, String>(1)?)
                        == TrustedUserPermission::CoAgent,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(judged_before, judged_after);
        assert_eq!(
            judged_after,
            vec![
                ("r1".to_string(), true),
                ("r2".to_string(), false),
                ("r3".to_string(), false),
                ("r4".to_string(), false),
            ]
        );

        // 表記は移り、触らない行はそのまま（`coagent` は判定が拾っていなかったので拾わない）。
        let spellings: Vec<String> = conn
            .prepare("SELECT permission FROM trusted_users ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(spellings, vec!["co-agent", "user", "owner", "coagent"]);

        // 再実行しても冪等（2 回目は 0 行更新）。
        initialize(&conn).expect("idempotent");
        let after: Vec<String> = conn
            .prepare("SELECT permission FROM trusted_users ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(after, spellings);
    }

    /// v20: エージェント単位ハートビート設定の表が増えるだけで、既存の
    /// チャンネル単位設定（`discord_channel_config`）の行は 1 つも動かない（#247）。
    #[test]
    fn agent_heartbeat_config_migration_v20_adds_table_without_touching_channel_config() {
        let conn = crate::init_memory().expect("init");
        // v19 相当の既存 DB を模す: 新表を落として version を 19 へ戻す。
        // チャンネル単位設定には行を 1 件入れておき、移行で動かないことを見る。
        conn.execute_batch(
            "DROP TABLE IF EXISTS agent_heartbeat_config;
             INSERT INTO discord_channel_config
               (channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted,
                heartbeat_enabled, heartbeat_interval_secs, updated_at)
               VALUES ('ch-1', 'a1', 'g1', 'general', 1, 1, 1, 1, 60, '2026-01-01');
             PRAGMA user_version = 19",
        )
        .unwrap();
        assert!(!table_exists(&conn, "agent_heartbeat_config").unwrap());

        initialize(&conn).expect("upgrade v19 -> v20");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        assert!(table_exists(&conn, "agent_heartbeat_config").unwrap());

        // 新表は空で始まる（既定は「設定なし」＝無効）。
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent_heartbeat_config", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0);

        // チャンネル単位設定は 1 バイトも変わらない（段階 3 まで残す）。
        let (hb_enabled, hb_interval): (i64, Option<i64>) = conn
            .query_row(
                "SELECT heartbeat_enabled, heartbeat_interval_secs FROM discord_channel_config
                 WHERE channel_id = 'ch-1' AND agent_id = 'a1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((hb_enabled, hb_interval), (1, Some(60)));

        // 行を入れてから再実行しても冪等（CREATE TABLE IF NOT EXISTS で消えない）。
        conn.execute_batch(
            "INSERT INTO agent_heartbeat_config (agent_id, enabled, interval_secs, updated_at)
             VALUES ('a1', 1, 900, '2026-01-02')",
        )
        .unwrap();
        initialize(&conn).expect("idempotent");
        let kept: i64 = conn
            .query_row(
                "SELECT interval_secs FROM agent_heartbeat_config WHERE agent_id = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 900);
    }

    /// v20 相当（旧一意制約）の `impressions` を作り直し、行を入れて version 20 へ戻す。
    fn seed_legacy_impressions(conn: &Connection, rows: &str) {
        conn.execute_batch(&format!(
            "DROP TABLE impressions;
             CREATE TABLE impressions (
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
                UNIQUE(agent_id, session_id, target_id)
             );
             CREATE INDEX idx_impressions_session ON impressions(agent_id, session_id);
             INSERT INTO impressions
               (id, agent_id, session_id, target_id, target_name, personality,
                communication_style, recent_behavior, agreement, notes,
                last_updated_turn, created_at, updated_at)
               VALUES {rows};
             PRAGMA user_version = 20"
        ))
        .unwrap();
    }

    /// v21: 人物像を agent スコープへ（#314）。
    ///
    /// **重複が無い実データでは 1 行も失われない**こと。一意制約が
    /// `(agent_id, target_id)` に付け替わり、同じ相手なら別セッションからでも
    /// 同じ行を更新するようになること。
    #[test]
    fn impressions_agent_scope_migration_v21_preserves_rows() {
        let conn = crate::init_memory().expect("init");
        seed_legacy_impressions(
            &conn,
            "('i1', 'a1', 's-discord', 'u1', 'Alice', 'p1', '', '', '中立', '', 0, '2026-01-01', '2026-01-01'),
             ('i2', 'a1', 's-discord', 'u2', 'Bob',   'p2', '', '', '中立', '', 0, '2026-01-02', '2026-01-02'),
             ('i3', 'a1', 's-nostr',   'u3', 'Carol', 'p3', '', '', '中立', '', 0, '2026-01-03', '2026-01-03'),
             ('i4', 'a2', 's-discord', 'u1', 'Alice', 'p4', '', '', '中立', '', 0, '2026-01-04', '2026-01-04')",
        );

        initialize(&conn).expect("upgrade v20 -> v21");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // 4 行すべて残る（重複が無いので統合は起きない）。
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM impressions ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(ids, vec!["i1", "i2", "i3", "i4"]);
        // 中身も session_id も動かない。
        let (session_id, personality): (String, String) = conn
            .query_row(
                "SELECT session_id, personality FROM impressions WHERE id = 'i3'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(session_id, "s-nostr");
        assert_eq!(personality, "p3");

        // 新しい一意制約: 同じ (agent_id, target_id) は別セッションでも 1 行。
        assert!(conn
            .execute(
                "INSERT INTO impressions (id, agent_id, session_id, target_id, target_name, created_at, updated_at) \
                 VALUES ('dup', 'a1', 's-other', 'u1', 'Alice', '2026-02-01', '2026-02-01')",
                [],
            )
            .is_err());
        // エージェントが違えば別の行（agent スコープは維持）。
        assert_eq!(
            crate::queries::get_impressions(&conn, "a1").unwrap().len(),
            3
        );
        assert_eq!(
            crate::queries::get_impressions(&conn, "a2").unwrap().len(),
            1
        );

        // 再実行しても冪等（再構築は走らない）。
        initialize(&conn).expect("idempotent");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM impressions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 4);
    }

    /// v21: 同じ相手が複数セッションに散っている（＝旧スキーマで分断していた）DB でも
    /// 壊れない。統合方針は「`updated_at` が最新の行を残す・`created_at` は最古を継ぐ」。
    #[test]
    fn impressions_agent_scope_migration_v21_merges_duplicates() {
        let conn = crate::init_memory().expect("init");
        seed_legacy_impressions(
            &conn,
            "('old', 'a1', 's-discord', 'u1', 'Alice', 'old-note', '', '', '中立', '', 1, '2026-01-01', '2026-01-01'),
             ('new', 'a1', 's-nostr',   'u1', 'Alice2','new-note', '', '', '中立', '', 2, '2026-03-01', '2026-03-01'),
             ('keep','a1', 's-discord', 'u2', 'Bob',   'bob-note', '', '', '中立', '', 0, '2026-02-01', '2026-02-01')",
        );

        initialize(&conn).expect("upgrade v20 -> v21");

        let rows = crate::queries::get_impressions(&conn, "a1").unwrap();
        assert_eq!(rows.len(), 2, "u1 の 2 行が 1 行に統合される");

        let merged = crate::queries::get_impression(&conn, "a1", "u1")
            .unwrap()
            .expect("merged row");
        assert_eq!(merged.id, "new", "updated_at が新しい行が残る");
        assert_eq!(merged.personality, "new-note");
        assert_eq!(merged.target_name, "Alice2");
        assert_eq!(merged.session_id, "s-nostr");
        assert_eq!(merged.last_updated_turn, 2);
        // 「いつからの知り合いか」は統合対象の最古を引き継ぐ。
        let created_at: String = conn
            .query_row(
                "SELECT created_at FROM impressions WHERE target_id = 'u1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(created_at, "2026-01-01");

        // 重複していない相手は影響を受けない。
        let bob = crate::queries::get_impression(&conn, "a1", "u2")
            .unwrap()
            .expect("bob");
        assert_eq!(bob.id, "keep");
        assert_eq!(bob.personality, "bob-note");
    }

    /// v21: 旧制約の判定は `sqlite_master.sql` の文字列ではなく実際の索引の列を見る。
    ///
    /// 同じ旧制約でも表記（空白・列の並べ方）は DB を作ったバイナリによって揺れる。
    /// 表記が違っても再構築が走り、`upsert_impression` の `ON CONFLICT(agent_id,
    /// target_id)` が通ること。
    #[test]
    fn impressions_agent_scope_migration_v21_detects_reformatted_legacy_schema() {
        let conn = crate::init_memory().expect("init");
        // 旧制約と等価だが `sql LIKE '%UNIQUE(agent_id, session_id, target_id)%'` には
        // 引っ掛からない表記（`UNIQUE (` と余分な空白）。
        conn.execute_batch(
            "DROP TABLE impressions;
             CREATE TABLE impressions (
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
                UNIQUE  (agent_id,session_id,target_id)
             );
             PRAGMA user_version = 20",
        )
        .unwrap();

        initialize(&conn).expect("upgrade v20 -> v21");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // 新制約になっているので upsert（ON CONFLICT(agent_id, target_id)）が通る。
        let row = crate::queries::ImpressionRow {
            id: "i1".to_string(),
            agent_id: "a1".to_string(),
            session_id: "s-discord".to_string(),
            target_id: "u1".to_string(),
            target_name: "Alice".to_string(),
            personality: "p1".to_string(),
            communication_style: String::new(),
            recent_behavior: String::new(),
            agreement: "中立".to_string(),
            notes: String::new(),
            last_updated_turn: 0,
        };
        crate::queries::upsert_impression(&conn, &row).expect("upsert on new constraint");
        let mut again = row.clone();
        again.session_id = "s-nostr".to_string();
        again.personality = "p2".to_string();
        crate::queries::upsert_impression(&conn, &again).expect("upsert across sessions");
        assert_eq!(
            crate::queries::get_impressions(&conn, "a1").unwrap().len(),
            1,
            "別セッションからでも同じ 1 行を更新する"
        );
    }

    /// H. SCHEMA_SQL 側と TASK_LEDGER_SQL 側で生成されるテーブル定義が一致する
    /// （両所への二重記載がドリフトしていないことの検証）。
    #[test]
    fn task_ledger_schema_parity() {
        let dump = |conn: &Connection| -> Vec<String> {
            conn.prepare(
                "SELECT sql FROM sqlite_master
                 WHERE name IN ('task_ledger','task_progress',
                                'idx_task_ledger_session','idx_task_ledger_one_active',
                                'idx_task_progress_task')
                 ORDER BY name",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
        };

        // 新規DB: SCHEMA_SQL 由来（baseline 時点でテーブルが出来ており、v2 は no-op）。
        let fresh = crate::init_memory().expect("fresh");
        // 既存DB: baseline 後に v2 マイグレーション由来で作成。
        let migrated = crate::init_memory().expect("migrated");
        migrated
            .execute_batch(
                "DROP TABLE task_progress; DROP TABLE task_ledger; PRAGMA user_version = 1",
            )
            .unwrap();
        initialize(&migrated).expect("re-migrate");

        assert_eq!(dump(&fresh), dump(&migrated));
        assert_eq!(dump(&fresh).len(), 5, "expected 2 tables + 3 indexes");
    }

    /// F. ダウングレードガード: DB が既知の最新版より新しい場合はエラーにする。
    #[test]
    fn downgrade_is_rejected() {
        let conn = crate::init_memory().expect("init");
        conn.execute_batch("PRAGMA user_version = 999").unwrap();
        let fake = &[Migration {
            version: 2,
            description: "v2",
            up: create_marker,
        }];
        let result = run_migrations(&conn, fake);
        assert!(result.is_err(), "newer-than-supported DB must be rejected");
        assert!(!table_exists(&conn, "test_marker").unwrap());
    }
}
