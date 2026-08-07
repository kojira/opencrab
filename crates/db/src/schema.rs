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
        // 既存 DB は列が NULL のまま増えるだけで、凝縮ランは既定オフ（config）なので挙動は
        // 変わらない。
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
];

/// カテゴリ層メンバー表 — **v23 当時の形**（topic ↔ category の参照, issue #313）。
///
/// PK は `(agent_id, topic_id)` = 1 topic 高々 1 category（sticky）。**これは v23 が
/// 作った履歴の形**であり、v26（#358）で多対多 PK（[`MEMORY_CATEGORY_MEMBERS_MM_SQL`]）
/// へ作り直す。最終形（新規 DB の SCHEMA_SQL / 既存 DB の v26 収束先）は多対多の方。
/// この const は v23 マイグレーション専用として残す（凍結された履歴の再現）。FK は
/// 張らない（追記的・可逆を優先: category/meta を切り戻しで消しても member 行が残る
/// だけで害が無い）。
const MEMORY_CATEGORY_MEMBERS_SQL: &str = "
CREATE TABLE IF NOT EXISTS memory_category_members (
    agent_id TEXT NOT NULL,
    topic_id TEXT NOT NULL,
    category_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (agent_id, topic_id)
);
CREATE INDEX IF NOT EXISTS idx_memory_category_members_cat ON memory_category_members(agent_id, category_id);
";

/// カテゴリ層メンバー表 — **多対多 PK の最終形**（issue #358 / v26）。
///
/// PK を `(agent_id, topic_id, category_id)` にして 1 topic に複数の category を付けられる
/// ようにする。SQLite は PK 変更＝テーブル再構築なので DROP+CREATE で作り直す（v26 の時点で
/// 旧行は白紙化対象なので保全しない）。**SCHEMA_SQL 内の同名ブロックと文面を揃えること**
/// （新規 DB は SCHEMA_SQL、既存 DB は v26 で同じ形に収束する）。FK は張らない（v23 と同方針）。
const MEMORY_CATEGORY_MEMBERS_MM_SQL: &str = "
DROP TABLE IF EXISTS memory_category_members;
CREATE TABLE memory_category_members (
    agent_id TEXT NOT NULL,
    topic_id TEXT NOT NULL,
    category_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (agent_id, topic_id, category_id)
);
CREATE INDEX IF NOT EXISTS idx_memory_category_members_cat ON memory_category_members(agent_id, category_id);
";

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
    -- 作成時の caller の trust class（'owner' / 'trusted' / 'agent'）。NULL = この列より
    -- 前に作られた既存スキル（legacy grandfather = Owner 相当扱い）。read_skill が
    -- 「強いターンが弱いスキルを借りる」confused deputy を塞ぐために参照する（#335）。
    created_caller TEXT,
    -- caller=Agent のターン（＝素の Agent 権限で走る run。外部 Nostr の受信ターンが
    -- 典型例だが、判定軸は transport ではなく **caller=Agent** である）に、この skill を
    -- index（system prompt）へ出し read_skill の本文を渡してよいか。既定 0 = 見せない
    -- （fail-closed）。オーナーがダッシュボード（REST）で少数だけ 1 に切り替える。
    -- Owner / CoAgent / TrustedUser の見え方には影響しない（従来どおり全部見える）。issue #352。
    agent_visible INTEGER NOT NULL DEFAULT 0,
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
    -- Nostr 経路のオーナー識別子（#319）。`agent_discord_config.owner_discord_id` の
    -- Nostr 版。**64 桁小文字 hex に正規化して保存**し、既定の空文字は「オーナー未設定
    -- ＝誰もオーナーにならない」を意味する（fail-closed）。
    owner_pubkey TEXT NOT NULL DEFAULT '',
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
    short_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_mem_idx_agent ON memory_index_nodes(agent_id);
CREATE INDEX IF NOT EXISTS idx_mem_idx_parent ON memory_index_nodes(agent_id, parent_id);
CREATE INDEX IF NOT EXISTS idx_mem_idx_type ON memory_index_nodes(agent_id, node_type);
CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_index_nodes_short_id ON memory_index_nodes(agent_id, short_id) WHERE short_id IS NOT NULL;

-- ============================================
-- 記憶インデックス: カテゴリ層メンバー（topic ↔ category の参照, issue #313）
-- ============================================
-- 時系列ツリー（root→period→session→topic）の parent_id を壊さないため、topic の
-- カテゴリ所属は parent 軸ではなく**参照**で持つ。PK は `(agent_id, topic_id, category_id)`
-- の多対多（issue #358）。1 topic は複数の関心にまたがるのでタグは複数付けられる。
-- node への FK は張らない（category/meta を切り戻しで消しても member 行が残るだけで
-- 害が無く、join 側で解決する。追記的・可逆を優先）。
-- ※ 既存 DB は v26 で同じ形に収束する（[`MEMORY_CATEGORY_MEMBERS_MM_SQL`] と文面一致）。
CREATE TABLE IF NOT EXISTS memory_category_members (
    agent_id TEXT NOT NULL,
    topic_id TEXT NOT NULL,
    category_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (agent_id, topic_id, category_id)
);
CREATE INDEX IF NOT EXISTS idx_memory_category_members_cat ON memory_category_members(agent_id, category_id);

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
    last_skill_consolidation_at TEXT,
    last_organize_at TEXT,
    organize_backlog_cursor TEXT,
    organize_last_run_at TEXT,
    memory_declare_cursor TEXT,
    memory_declare_window TEXT,
    memory_condense_cursor TEXT
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

    /// v22: `agent_nostr_config.owner_pubkey` の付与（#319）。
    ///
    /// **既存の行を失わない**こと（秘密鍵・リレー・有効フラグがそのまま残る）と、
    /// 既存行の新しい列が **空＝オーナー未設定**（誰もオーナーにならない）で始まる
    /// ことを見る。冪等性（新規 DB は SCHEMA_SQL 側で列を持つ）も確認する。
    #[test]
    fn nostr_owner_pubkey_migration_v22_preserves_existing_rows() {
        let conn = crate::init_memory().expect("init");
        // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
        assert!(column_exists(&conn, "agent_nostr_config", "owner_pubkey").unwrap());

        // v20 相当の既存 DB を模す: 列を持たない表を作り直して version を 20 へ戻す。
        // 行の内容は「移行後も 1 バイトも変わらない」ことを見るための実データ。
        conn.execute_batch(
            "DROP TABLE agent_nostr_config;
             CREATE TABLE agent_nostr_config (
                agent_id TEXT PRIMARY KEY,
                secret_key TEXT NOT NULL,
                relays_json TEXT NOT NULL DEFAULT '[]',
                filter_json TEXT NOT NULL DEFAULT '{}',
                enabled INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL
             );
             INSERT INTO agent_nostr_config
               (agent_id, secret_key, relays_json, filter_json, enabled, updated_at)
               VALUES ('a1', 'nsec1keep', '[\"wss://relay.example\"]', '{\"keywords\":[\"x\"]}', 1, '2026-01-01');
             PRAGMA user_version = 20",
        )
        .unwrap();
        assert!(!column_exists(&conn, "agent_nostr_config", "owner_pubkey").unwrap());

        initialize(&conn).expect("upgrade v20 -> v22");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        assert!(column_exists(&conn, "agent_nostr_config", "owner_pubkey").unwrap());

        // 既存行はそのまま残り、新しい列は空（＝オーナー未設定 / fail-closed）。
        let (secret, relays, filter, enabled, owner): (String, String, String, i64, String) = conn
            .query_row(
                "SELECT secret_key, relays_json, filter_json, enabled, owner_pubkey
                 FROM agent_nostr_config WHERE agent_id = 'a1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .expect("既存行が失われた");
        assert_eq!(secret, "nsec1keep");
        assert_eq!(relays, r#"["wss://relay.example"]"#);
        assert_eq!(filter, r#"{"keywords":["x"]}"#);
        assert_eq!(enabled, 1);
        assert_eq!(owner, "", "移行直後にオーナーが居てはいけない");

        // 再実行しても列は 1 つのまま（column_exists ガード）で、値も消えない。
        conn.execute_batch(
            "UPDATE agent_nostr_config SET owner_pubkey = 'ff' WHERE agent_id = 'a1'",
        )
        .unwrap();
        initialize(&conn).expect("idempotent");
        let kept: String = conn
            .query_row(
                "SELECT owner_pubkey FROM agent_nostr_config WHERE agent_id = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, "ff");
    }

    /// v24: 既存 DB（`user_version = 23`, `created_caller` 列なし）へ番号付き
    /// マイグレーションが届き、列が追加され、既存行は NULL のまま保たれることを検証する。
    ///
    /// これは #349 の本番事故の再現ガード。#347 は列追加を凍結 `migrate()` に置いたため、
    /// 既に版がスタンプ済みの本番 DB では走らず、全スキル SELECT が
    /// `no such column: created_caller` で落ちた。CI は毎回新規 DB（`migrate()` が走る）
    /// なので検出できなかった。ここでは **既存 DB を模して** `run_migrations` 経路のみで
    /// 列が届くことを固定する。
    #[test]
    fn skills_created_caller_migration_v24_reaches_existing_db() {
        let conn = crate::init_memory().expect("init");
        // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
        assert!(column_exists(&conn, "skills", "created_caller").unwrap());

        // v23 相当の既存 DB を模す: 列を落とし version を 23 へ戻す。
        // 列を持たない状態で入れた行は、移行後 NULL（legacy grandfather = Owner 相当）に
        // なること（＝既存行が壊れないこと）を見るための実データ。
        conn.execute_batch(
            "ALTER TABLE skills DROP COLUMN created_caller;
             INSERT INTO skills
               (id, agent_id, name, description, situation_pattern, guidance,
                source_type, usage_count, is_active, permission, archived,
                created_at, updated_at)
               VALUES ('legacy1', 'a1', 'n', 'd', 'sp', 'g',
                       'experience', 0, 1, '\"agent\"', 0,
                       '2026-01-01', '2026-01-01');
             PRAGMA user_version = 23",
        )
        .unwrap();
        assert!(!column_exists(&conn, "skills", "created_caller").unwrap());

        // 起動経路（initialize → run_migrations）で v24 が届く。migrate() は
        // user_version >= BASELINE_VERSION のため走らない（本番と同じ経路）。
        initialize(&conn).expect("upgrade v23 -> latest");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        assert!(column_exists(&conn, "skills", "created_caller").unwrap());

        // 既存行は残り、新しい列は NULL（legacy grandfather）。
        let (name, created_caller): (String, Option<String>) = conn
            .query_row(
                "SELECT name, created_caller FROM skills WHERE id = 'legacy1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("既存行が失われた");
        assert_eq!(name, "n");
        assert_eq!(created_caller, None, "既存行は NULL のまま = Owner 相当");

        // #349 の直接の症状: 列を含む SELECT が通ること。
        conn.query_row(
            "SELECT id, agent_id, name, created_caller FROM skills WHERE id = 'legacy1'",
            [],
            |r| r.get::<_, String>(0),
        )
        .expect("created_caller を含む SELECT が通らない");

        // 冪等性: 再実行しても落ちず、書いた値も消えない（column_exists ガード）。
        conn.execute_batch("UPDATE skills SET created_caller = 'agent' WHERE id = 'legacy1'")
            .unwrap();
        initialize(&conn).expect("idempotent");
        let kept: Option<String> = conn
            .query_row(
                "SELECT created_caller FROM skills WHERE id = 'legacy1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept.as_deref(), Some("agent"));
    }

    /// v25: 既存 DB（`user_version = 24`, `agent_visible` 列なし）へ番号付き
    /// マイグレーションが届き、列が **既定 0（fail-closed）** で追加され、既存行が
    /// すべて 0（＝Agent には見せない）になることを検証する（#352 / #349 の事故ガード）。
    #[test]
    fn skills_agent_visible_migration_v25_reaches_existing_db_default_zero() {
        let conn = crate::init_memory().expect("init");
        // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
        assert!(column_exists(&conn, "skills", "agent_visible").unwrap());

        // v24 相当の既存 DB を模す: agent_visible 列を落とし version を 24 へ戻す。
        // 列を持たない状態で複数行を入れ、移行後すべて 0 になること（既存が壊れず、
        // かつ既定で Agent 非露出になること）を見る。
        conn.execute_batch(
            "ALTER TABLE skills DROP COLUMN agent_visible;
             INSERT INTO skills
               (id, agent_id, name, description, situation_pattern, guidance,
                source_type, usage_count, is_active, permission, archived,
                created_at, updated_at)
               VALUES
               ('s1', 'a1', 'n1', 'd', 'sp', 'g', 'experience', 0, 1, '\"agent\"', 0,
                '2026-01-01', '2026-01-01'),
               ('s2', 'a1', 'n2', 'd', 'sp', 'g', 'experience', 0, 1, '\"agent\"', 0,
                '2026-01-01', '2026-01-01');
             PRAGMA user_version = 24",
        )
        .unwrap();
        assert!(!column_exists(&conn, "skills", "agent_visible").unwrap());
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM skills", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, 2, "移行前の行数");

        // 起動経路（initialize → run_migrations）で v25 が届く。
        initialize(&conn).expect("upgrade v24 -> latest");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        assert!(column_exists(&conn, "skills", "agent_visible").unwrap());

        // 既存の全行が残り、agent_visible は既定 0（fail-closed）。
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM skills", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after, 2, "行が失われた");
        let visible_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM skills WHERE agent_visible = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            visible_count, 0,
            "既存行はすべて非露出（既定 0）でなければならない"
        );

        // 列を含む SELECT が通ること（#349 の症状の非退行）。
        conn.query_row(
            "SELECT id, agent_visible FROM skills WHERE id = 's1'",
            [],
            |r| r.get::<_, i64>(1),
        )
        .expect("agent_visible を含む SELECT が通らない");

        // 冪等性: オーナーが 1 行を露出許可した後に再実行しても落ちず、値も消えない。
        conn.execute_batch("UPDATE skills SET agent_visible = 1 WHERE id = 's1'")
            .unwrap();
        initialize(&conn).expect("idempotent");
        let kept: i64 = conn
            .query_row(
                "SELECT agent_visible FROM skills WHERE id = 's1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1, "再実行で露出許可が消えてはならない");
    }

    /// v27（#361 / #313 段階3）: `agent_memory_index_config.last_organize_at` が
    /// **既存 DB に届く**こと、既定 NULL であること、既存行が壊れないこと、再実行で
    /// 落ちないこと（#349 の事故ガード）。本番は v26 なので v26 → latest の経路を模す。
    #[test]
    fn agent_memory_index_config_last_organize_at_migration_v27_reaches_existing_db() {
        let conn = crate::init_memory().expect("init");
        // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
        assert!(column_exists(&conn, "agent_memory_index_config", "last_organize_at").unwrap());

        // v26 相当の既存 DB を模す: last_organize_at 列を落とし version を 26 へ戻す。
        // 既存の設定行（last_skill_consolidation_at には値あり）を入れておき、移行で
        // その値が消えないこと・新列が NULL で足されることを見る。
        conn.execute_batch(
            "ALTER TABLE agent_memory_index_config DROP COLUMN last_organize_at;
             INSERT INTO agent_memory_index_config
               (agent_id, batch_size, threshold, updated_at, last_skill_consolidation_at)
               VALUES ('a1', 50, 20, '2026-01-01', '2026-07-01T00:00:00Z');
             PRAGMA user_version = 26",
        )
        .unwrap();
        assert!(!column_exists(&conn, "agent_memory_index_config", "last_organize_at").unwrap());

        // 起動経路（initialize → run_migrations）で v27 が届く。
        initialize(&conn).expect("upgrade v26 -> latest");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        assert!(column_exists(&conn, "agent_memory_index_config", "last_organize_at").unwrap());

        // 既存行が残り、新列は NULL（未実行）、隣の列の値は消えていない。
        let (organize, consolidation): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT last_organize_at, last_skill_consolidation_at
                 FROM agent_memory_index_config WHERE agent_id = 'a1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("last_organize_at を含む SELECT が通らない");
        assert_eq!(
            organize, None,
            "新列は既定 NULL（未実行）でなければならない"
        );
        assert_eq!(
            consolidation.as_deref(),
            Some("2026-07-01T00:00:00Z"),
            "隣の列の既存値が失われた"
        );

        // 冪等性: 値を書いた後に再実行しても落ちず、値も消えない。
        conn.execute_batch(
            "UPDATE agent_memory_index_config SET last_organize_at = '2026-08-03T00:00:00Z' WHERE agent_id = 'a1'",
        )
        .unwrap();
        initialize(&conn).expect("idempotent");
        let kept: Option<String> = conn
            .query_row(
                "SELECT last_organize_at FROM agent_memory_index_config WHERE agent_id = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            kept.as_deref(),
            Some("2026-08-03T00:00:00Z"),
            "再実行でマーカーが消えてはならない"
        );
    }

    /// v28（#365 / #313 段階3b）: `agent_memory_index_config.organize_backlog_cursor` が
    /// **既存 DB に届く**こと、既定 NULL であること、既存行・隣の列が壊れないこと、再実行で
    /// 落ちないこと（#349 の事故ガード）。本番は v27 なので v27 → latest の経路を模す。
    #[test]
    fn agent_memory_index_config_backlog_cursor_migration_v28_reaches_existing_db() {
        let conn = crate::init_memory().expect("init");
        // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
        assert!(column_exists(
            &conn,
            "agent_memory_index_config",
            "organize_backlog_cursor"
        )
        .unwrap());

        // v27 相当の既存 DB を模す: organize_backlog_cursor 列を落とし version を 27 へ戻す。
        // 既存の設定行（last_organize_at / last_skill_consolidation_at に値あり）を入れておき、
        // 移行でその値が消えないこと・新列が NULL で足されることを見る。
        conn.execute_batch(
            "ALTER TABLE agent_memory_index_config DROP COLUMN organize_backlog_cursor;
             INSERT INTO agent_memory_index_config
               (agent_id, batch_size, threshold, updated_at, last_skill_consolidation_at, last_organize_at)
               VALUES ('a1', 50, 20, '2026-01-01', '2026-07-01T00:00:00Z', '2026-08-03T00:00:00Z|n5');
             PRAGMA user_version = 27",
        )
        .unwrap();
        assert!(!column_exists(
            &conn,
            "agent_memory_index_config",
            "organize_backlog_cursor"
        )
        .unwrap());

        // 起動経路（initialize → run_migrations）で v28 が届く。
        initialize(&conn).expect("upgrade v27 -> latest");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        assert!(column_exists(
            &conn,
            "agent_memory_index_config",
            "organize_backlog_cursor"
        )
        .unwrap());

        // 既存行が残り、新列は NULL（未シード）、隣の 2 列の値は消えていない。
        let (backlog, organize, consolidation): (Option<String>, Option<String>, Option<String>) =
            conn.query_row(
                "SELECT organize_backlog_cursor, last_organize_at, last_skill_consolidation_at
                 FROM agent_memory_index_config WHERE agent_id = 'a1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("organize_backlog_cursor を含む SELECT が通らない");
        assert_eq!(
            backlog, None,
            "新列は既定 NULL（未シード）でなければならない"
        );
        assert_eq!(
            organize.as_deref(),
            Some("2026-08-03T00:00:00Z|n5"),
            "新規側マーカーの既存値が失われた"
        );
        assert_eq!(
            consolidation.as_deref(),
            Some("2026-07-01T00:00:00Z"),
            "隣の列の既存値が失われた"
        );

        // 冪等性: 値を書いた後に再実行しても落ちず、値も消えない。
        conn.execute_batch(
            "UPDATE agent_memory_index_config SET organize_backlog_cursor = '2026-06-01T00:00:00Z|old3' WHERE agent_id = 'a1'",
        )
        .unwrap();
        initialize(&conn).expect("idempotent");
        let kept: Option<String> = conn
            .query_row(
                "SELECT organize_backlog_cursor FROM agent_memory_index_config WHERE agent_id = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            kept.as_deref(),
            Some("2026-06-01T00:00:00Z|old3"),
            "再実行でマーカーが消えてはならない"
        );
    }

    /// v29（#365 レビュー / #313 段階3b）: `agent_memory_index_config.organize_last_run_at` が
    /// **既存 DB に届く**こと、既定 NULL であること、既存の 2 軸マーカー・隣の列が壊れないこと、
    /// 再実行で落ちないこと。本番は v27 なので v28 相当（列 3 本目だけ欠く）→ latest を模す。
    #[test]
    fn agent_memory_index_config_last_run_at_migration_v29_reaches_existing_db() {
        let conn = crate::init_memory().expect("init");
        assert!(column_exists(&conn, "agent_memory_index_config", "organize_last_run_at").unwrap());

        // v28 相当の既存 DB を模す: organize_last_run_at 列を落とし version を 28 へ戻す。
        // 2 軸マーカーに値を入れておき、移行で消えないこと・新列が NULL で足されることを見る。
        conn.execute_batch(
            "ALTER TABLE agent_memory_index_config DROP COLUMN organize_last_run_at;
             INSERT INTO agent_memory_index_config
               (agent_id, batch_size, threshold, updated_at, last_organize_at, organize_backlog_cursor)
               VALUES ('a1', 50, 20, '2026-01-01', '2026-08-04T00:00:00Z|n5', '2026-06-01T00:00:00Z|old3');
             PRAGMA user_version = 28",
        )
        .unwrap();
        assert!(
            !column_exists(&conn, "agent_memory_index_config", "organize_last_run_at").unwrap()
        );

        // 起動経路（initialize → run_migrations）で v29 が届く。
        initialize(&conn).expect("upgrade v28 -> latest");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        assert!(column_exists(&conn, "agent_memory_index_config", "organize_last_run_at").unwrap());

        // 既存の 2 軸マーカーは残り、新列は NULL（未刻）。
        let (last_run, organize, backlog): (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT organize_last_run_at, last_organize_at, organize_backlog_cursor
                 FROM agent_memory_index_config WHERE agent_id = 'a1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("organize_last_run_at を含む SELECT が通らない");
        assert_eq!(last_run, None, "新列は既定 NULL（未刻）でなければならない");
        assert_eq!(organize.as_deref(), Some("2026-08-04T00:00:00Z|n5"));
        assert_eq!(backlog.as_deref(), Some("2026-06-01T00:00:00Z|old3"));

        // 冪等性: 値を書いた後に再実行しても落ちず、値も消えない。
        conn.execute_batch(
            "UPDATE agent_memory_index_config SET organize_last_run_at = '2026-08-05T00:00:00Z' WHERE agent_id = 'a1'",
        )
        .unwrap();
        initialize(&conn).expect("idempotent");
        let kept: Option<String> = conn
            .query_row(
                "SELECT organize_last_run_at FROM agent_memory_index_config WHERE agent_id = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept.as_deref(), Some("2026-08-05T00:00:00Z"));
    }

    /// v31（#384 / #376 段階2）: `agent_memory_index_config.memory_declare_cursor` が
    /// **既存 DB に届く**こと、既定 NULL であること、既存行・隣の列（タグ整理ランの 3 列）が
    /// 壊れないこと、再実行で落ちないこと（#349 の事故ガード）。本番は v30 なので v30 →
    /// latest の経路を模す。
    #[test]
    fn agent_memory_index_config_declare_cursor_migration_v31_reaches_existing_db() {
        let conn = crate::init_memory().expect("init");
        // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
        assert!(
            column_exists(&conn, "agent_memory_index_config", "memory_declare_cursor").unwrap()
        );

        // v30 相当の既存 DB を模す: memory_declare_cursor 列を落とし version を 30 へ戻す。
        // タグ整理ランの 3 マーカーに値を入れておき、移行でそれらが消えないこと・新列が
        // NULL で足されることを見る。
        conn.execute_batch(
            "ALTER TABLE agent_memory_index_config DROP COLUMN memory_declare_cursor;
             INSERT INTO agent_memory_index_config
               (agent_id, batch_size, threshold, updated_at, last_organize_at,
                organize_backlog_cursor, organize_last_run_at)
               VALUES ('a1', 50, 20, '2026-01-01', '2026-08-04T00:00:00Z|n5',
                       '2026-06-01T00:00:00Z|old3', '2026-08-05T00:00:00Z');
             PRAGMA user_version = 30",
        )
        .unwrap();
        assert!(
            !column_exists(&conn, "agent_memory_index_config", "memory_declare_cursor").unwrap()
        );

        // 起動経路（initialize → run_migrations）で v31 が届く。
        initialize(&conn).expect("upgrade v30 -> latest");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        assert!(
            column_exists(&conn, "agent_memory_index_config", "memory_declare_cursor").unwrap()
        );

        // 新列は NULL（未実行）、タグ整理ランの 3 マーカーは残る。
        let (declare, organize, backlog, last_run): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT memory_declare_cursor, last_organize_at, organize_backlog_cursor,
                        organize_last_run_at
                 FROM agent_memory_index_config WHERE agent_id = 'a1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("memory_declare_cursor を含む SELECT が通らない");
        assert_eq!(declare, None, "新列は既定 NULL（未実行）でなければならない");
        assert_eq!(organize.as_deref(), Some("2026-08-04T00:00:00Z|n5"));
        assert_eq!(backlog.as_deref(), Some("2026-06-01T00:00:00Z|old3"));
        assert_eq!(last_run.as_deref(), Some("2026-08-05T00:00:00Z"));

        // 冪等性: 値を書いた後に再実行しても落ちず、値も消えない。
        conn.execute_batch(
            "UPDATE agent_memory_index_config SET memory_declare_cursor = '2026-08-06T00:00:00Z|4242' WHERE agent_id = 'a1'",
        )
        .unwrap();
        initialize(&conn).expect("idempotent");
        let kept: Option<String> = conn
            .query_row(
                "SELECT memory_declare_cursor FROM agent_memory_index_config WHERE agent_id = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            kept.as_deref(),
            Some("2026-08-06T00:00:00Z|4242"),
            "再実行でマーカーが消えてはならない"
        );
    }

    /// v34（#394）: `agent_memory_index_config.memory_declare_window`（本人が決める窓の希望）が
    /// **既存 DB に届く**こと、既定 NULL であること、隣の列（宣言ランのマーカー・タグ整理ランの
    /// 3 列）が壊れないこと、再実行で値が消えないこと。
    #[test]
    fn agent_memory_index_config_declare_window_migration_v34_reaches_existing_db() {
        let conn = crate::init_memory().expect("init");
        // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
        assert!(
            column_exists(&conn, "agent_memory_index_config", "memory_declare_window").unwrap()
        );

        // 列を落とし、宣言ランのマーカーに値を入れた状態で版を戻す（既存 DB を模す）。
        conn.execute_batch(
            "ALTER TABLE agent_memory_index_config DROP COLUMN memory_declare_window;
             INSERT INTO agent_memory_index_config
               (agent_id, batch_size, threshold, updated_at, memory_declare_cursor,
                organize_last_run_at)
               VALUES ('a1', 50, 20, '2026-01-01', '2026-08-05T00:00:00Z|23594',
                       '2026-08-05T00:00:00Z');
             PRAGMA user_version = 32",
        )
        .unwrap();
        assert!(
            !column_exists(&conn, "agent_memory_index_config", "memory_declare_window").unwrap()
        );

        initialize(&conn).expect("upgrade v32 -> latest");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        assert!(
            column_exists(&conn, "agent_memory_index_config", "memory_declare_window").unwrap()
        );

        let (window, cursor, organize): (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT memory_declare_window, memory_declare_cursor, organize_last_run_at
                 FROM agent_memory_index_config WHERE agent_id = 'a1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("memory_declare_window を含む SELECT が通らない");
        assert_eq!(
            window, None,
            "新列は既定 NULL（希望なし）でなければならない"
        );
        assert_eq!(cursor.as_deref(), Some("2026-08-05T00:00:00Z|23594"));
        assert_eq!(organize.as_deref(), Some("2026-08-05T00:00:00Z"));

        // 冪等性: 値を書いた後に再実行しても落ちず、値も消えない。
        conn.execute_batch(
            "UPDATE agent_memory_index_config SET memory_declare_window = '{\"window_size\":450}' WHERE agent_id = 'a1'",
        )
        .unwrap();
        initialize(&conn).expect("idempotent");
        let kept: Option<String> = conn
            .query_row(
                "SELECT memory_declare_window FROM agent_memory_index_config WHERE agent_id = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept.as_deref(), Some("{\"window_size\":450}"));
    }

    /// v35（#411）: `agent_memory_index_config.memory_condense_cursor`（凝縮ランのマーカー）が
    /// **既存 DB に届く**こと、既定 NULL であること、隣の列（宣言ランのマーカー/窓・タグ整理ランの
    /// マーカー）が壊れないこと、再実行で値が消えないこと。
    #[test]
    fn agent_memory_index_config_condense_cursor_migration_v35_reaches_existing_db() {
        let conn = crate::init_memory().expect("init");
        // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
        assert!(
            column_exists(&conn, "agent_memory_index_config", "memory_condense_cursor").unwrap()
        );

        // 列を落とし、隣の列に値を入れた状態で版を戻す（既存 DB を模す）。
        conn.execute_batch(
            "ALTER TABLE agent_memory_index_config DROP COLUMN memory_condense_cursor;
             INSERT INTO agent_memory_index_config
               (agent_id, batch_size, threshold, updated_at, memory_declare_cursor,
                memory_declare_window)
               VALUES ('a1', 50, 20, '2026-01-01', '2026-08-07T00:00:00Z|60000',
                       '{\"window_size\":300}');
             PRAGMA user_version = 34",
        )
        .unwrap();
        assert!(
            !column_exists(&conn, "agent_memory_index_config", "memory_condense_cursor").unwrap()
        );

        initialize(&conn).expect("upgrade v34 -> latest");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        assert!(
            column_exists(&conn, "agent_memory_index_config", "memory_condense_cursor").unwrap()
        );

        let (condense, declare, window): (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT memory_condense_cursor, memory_declare_cursor, memory_declare_window
                 FROM agent_memory_index_config WHERE agent_id = 'a1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("memory_condense_cursor を含む SELECT が通らない");
        assert_eq!(
            condense, None,
            "新列は既定 NULL（未実行）でなければならない"
        );
        assert_eq!(declare.as_deref(), Some("2026-08-07T00:00:00Z|60000"));
        assert_eq!(window.as_deref(), Some("{\"window_size\":300}"));

        // 冪等性: 値を書いた後に再実行しても落ちず、値も消えない。
        conn.execute_batch(
            "UPDATE agent_memory_index_config SET memory_condense_cursor = '2026-08-08T00:00:00Z|346' WHERE agent_id = 'a1'",
        )
        .unwrap();
        initialize(&conn).expect("idempotent");
        let kept: Option<String> = conn
            .query_row(
                "SELECT memory_condense_cursor FROM agent_memory_index_config WHERE agent_id = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept.as_deref(), Some("2026-08-08T00:00:00Z|346"));
    }

    /// v20 の DB が **v21（impressions の再構築）→ v22（owner_pubkey の追加）** を
    /// 順に通り、**どちらのデータも失われない**（#314 と #319 が同じ版列に並ぶ）。
    ///
    /// v21 は表の再構築（`DROP TABLE impressions`）を伴い、v22 は別の表への列追加。
    /// 別々のマイグレーションが独立に通ることは各テストで見ているが、**同じ 1 回の
    /// 起動で連続して流れる**のは実運用の経路（v20 で止まっていた DB が今回の
    /// バイナリで初めて起動する）なので、ここで両方を 1 本の流れとして固定する。
    #[test]
    fn v20_db_upgrades_through_v21_and_v22_without_losing_data() {
        let conn = crate::init_memory().expect("init");

        // (1) 旧一意制約の impressions に行を入れ、version 20 へ戻す。
        seed_legacy_impressions(
            &conn,
            "('i1', 'a1', 's-discord', 'u1', 'Alice', 'p1', '', '', '中立', '', 0, '2026-01-01', '2026-01-01'),
             ('i2', 'a1', 's-nostr',   'u2', 'Bob',   'p2', '', '', '中立', '', 0, '2026-01-02', '2026-01-02')",
        );
        // (2) 同じ DB の agent_nostr_config も v20 相当（owner_pubkey 無し）へ戻す。
        //     `seed_legacy_impressions` が既に version 20 を刻んでいるので、ここでも
        //     刻み直して「両方が v20 の状態」を作る。
        conn.execute_batch(
            "DROP TABLE agent_nostr_config;
             CREATE TABLE agent_nostr_config (
                agent_id TEXT PRIMARY KEY,
                secret_key TEXT NOT NULL,
                relays_json TEXT NOT NULL DEFAULT '[]',
                filter_json TEXT NOT NULL DEFAULT '{}',
                enabled INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL
             );
             INSERT INTO agent_nostr_config
               (agent_id, secret_key, relays_json, filter_json, enabled, updated_at)
               VALUES ('a1', 'nsec1keep', '[\"wss://relay.example\"]', '{}', 1, '2026-01-01');
             PRAGMA user_version = 20",
        )
        .unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 20);
        assert!(!column_exists(&conn, "agent_nostr_config", "owner_pubkey").unwrap());

        // (3) 1 回の起動で v21 → v22 が順に流れる。
        initialize(&conn).expect("upgrade v20 -> v21 -> v22");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // (4a) v21 の結果: 行は 2 件とも残り、一意制約が agent スコープになっている。
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM impressions ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(ids, vec!["i1", "i2"], "v21 で人物像の行が失われた");
        // 別セッションからでも同じ (agent_id, target_id) は 1 行に収束する
        // （旧制約のままなら 2 行目が入ってしまう）。
        conn.execute_batch(
            "INSERT INTO impressions
               (id, agent_id, session_id, target_id, target_name, personality,
                communication_style, recent_behavior, agreement, notes,
                last_updated_turn, created_at, updated_at)
               VALUES ('i3', 'a1', 's-other', 'u1', 'Alice', 'p9', '', '', '中立', '', 0, '2026-01-05', '2026-01-05')
             ON CONFLICT(agent_id, target_id) DO UPDATE SET personality = excluded.personality",
        )
        .expect("新しい一意制約で upsert できる");
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM impressions WHERE agent_id = 'a1' AND target_id = 'u1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);

        // (4b) v22 の結果: Nostr 設定の行はそのまま残り、オーナーは未設定で始まる。
        let (secret, relays, enabled, owner): (String, String, i64, String) = conn
            .query_row(
                "SELECT secret_key, relays_json, enabled, owner_pubkey
                 FROM agent_nostr_config WHERE agent_id = 'a1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("v22 で Nostr 設定の行が失われた");
        assert_eq!(secret, "nsec1keep");
        assert_eq!(relays, r#"["wss://relay.example"]"#);
        assert_eq!(enabled, 1);
        assert_eq!(owner, "", "移行直後にオーナーが居てはいけない");
    }

    /// 新規 DB は最初から最新版で、**v21 と v22 の両方の構造**を持つ
    /// （`SCHEMA_SQL` と `MIGRATIONS` が食い違っていない）。
    #[test]
    fn fresh_db_has_both_v21_and_v22_structures() {
        let conn = crate::init_memory().expect("init");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        // v22: 列がある。
        assert!(column_exists(&conn, "agent_nostr_config", "owner_pubkey").unwrap());
        // v21: `(agent_id, target_id)` ちょうど 2 列の UNIQUE 索引がある。
        let unique_pairs: i64 = conn
            .query_row(
                r#"SELECT COUNT(*) FROM pragma_index_list('impressions') il
                    WHERE il."unique" = 1
                      AND (SELECT COUNT(*) FROM pragma_index_info(il.name)) = 2
                      AND (SELECT COUNT(*) FROM pragma_index_info(il.name) ii
                            WHERE ii.name IN ('agent_id', 'target_id')) = 2"#,
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unique_pairs, 1, "新規 DB に v21 の一意制約が無い");
    }

    /// v23: node_type の CHECK を `'category'`/`'meta'` へ拡張し、参照表を新設する。
    /// v22 相当（狭い CHECK）の既存 DB から、既存の時系列ノードを1件も失わずに
    /// 移行できること、移行後は category/meta が実際に**保存できる**こと（`INSERT OR
    /// IGNORE` による沈黙が起きないこと）を固定する。
    #[test]
    fn memory_index_category_migration_v23_widens_check_and_preserves_rows() {
        let conn = crate::init_memory().expect("init");

        // v22 相当の既存 DB を模す: 狭い CHECK のテーブルへ作り直し、時系列ツリーを積む。
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        conn.execute_batch(
            "DROP TABLE memory_index_nodes;
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
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, short_id TEXT,
                keywords_json TEXT NOT NULL DEFAULT '[]', summary_refreshed_at TEXT
             );
             INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, title, summary, created_at, updated_at)
             VALUES ('r', 'a1', NULL, 'root', 'root', 's', '2026-01-01', '2026-01-01'),
                    ('p', 'a1', 'r', 'period', '2026-01', 's', '2026-01-01', '2026-01-01'),
                    ('sess', 'a1', 'p', 'session', 'S', 's', '2026-01-01', '2026-01-01'),
                    ('t', 'a1', 'sess', 'topic', 'Rust入門', 's', '2026-01-02', '2026-01-02');
             DROP TABLE IF EXISTS memory_category_members;
             PRAGMA user_version = 22;",
        )
        .unwrap();

        // 狭い CHECK では category ノードは拒否される（＝移行が必要な証拠）。
        assert!(
            conn.execute(
                "INSERT INTO memory_index_nodes (id, agent_id, node_type, title, summary, created_at, updated_at)
                 VALUES ('c0', 'a1', 'category', 'X', '', '2026-01-01', '2026-01-01')",
                [],
            )
            .is_err(),
            "移行前は category が CHECK で拒否されるはず"
        );

        run_migrations(&conn, MIGRATIONS).expect("v23 migration");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // 時系列ツリーは1件も失われていない。
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM memory_index_nodes ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(ids, vec!["p", "r", "sess", "t"]);

        // 移行後は category / meta が実際に保存できる（OR IGNORE の沈黙が起きない）。
        conn.execute(
            "INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, source_type, title, summary, created_at, updated_at)
             VALUES ('cat1', 'a1', NULL, 'category', 'category', 'kojiraさんの教え', '', '2026-02-01', '2026-02-01'),
                    ('meta1', 'a1', NULL, 'meta', 'category', 'ルール群', '', '2026-02-01', '2026-02-01')",
            [],
        )
        .expect("移行後は category/meta を登録できる");

        // 参照表が存在し、割当を保存できる。全チェーンは v26 まで走るので PK は多対多
        // （`(agent_id, topic_id, category_id)`）＝同じ topic に複数の category を付けられる。
        assert!(table_exists(&conn, "memory_category_members").unwrap());
        conn.execute(
            "INSERT INTO memory_category_members (agent_id, topic_id, category_id, created_at)
             VALUES ('a1', 't', 'cat1', '2026-02-01'),
                    ('a1', 't', 'meta1', '2026-02-02')",
            [],
        )
        .expect("v26 の多対多 PK では同一 topic に複数 category を付けられる");
    }

    /// v26: 分類レイヤの白紙化 + members の多対多 PK 化（issue #358）。本番相当の v25 DB
    /// （分類ノード + その FTS 行 + 旧 PK の members 行 + 記憶本文 memory_curated + 時系列
    /// ツリー）を模し、`run_migrations` 経路のみで:
    ///  - category/meta ノードと**その FTS 行**が消えること（FTS 孤児を残さない）
    ///  - 時系列ツリー（node_type 別）と memory_curated が 1 行も減らないこと
    ///  - members が空になり、PK が多対多になって同一 topic に複数 category を入れられること
    ///  - user_version が上がり、2 回実行しても落ちないこと
    /// を固定する。
    #[test]
    fn memory_index_reset_and_multi_tag_migration_v26() {
        let conn = crate::init_memory().expect("init");

        // --- v25 相当へ戻す: members を旧 PK (agent_id, topic_id) で作り直し、user_version=25 ---
        conn.execute_batch(
            "DROP TABLE IF EXISTS memory_category_members;
             CREATE TABLE memory_category_members (
                agent_id TEXT NOT NULL,
                topic_id TEXT NOT NULL,
                category_id TEXT NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY (agent_id, topic_id)
             );",
        )
        .unwrap();

        // 時系列ツリー（root/period/session/topic/daily）+ 分類ノード（category/meta）を積む。
        // 分類ノードは insert_index_node と同様に FTS へも入れて「孤児が残らない」ことを見る。
        conn.execute_batch(
            "INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, source_type, title, summary, created_at, updated_at)
             VALUES ('r', 'a1', NULL, 'root', 'session_log', 'root', 's', '2026-01-01', '2026-01-01'),
                    ('p', 'a1', 'r', 'period', 'session_log', '2026-01', 's', '2026-01-01', '2026-01-01'),
                    ('sess', 'a1', 'p', 'session', 'session_log', 'S', 's', '2026-01-01', '2026-01-01'),
                    ('t', 'a1', 'sess', 'topic', 'session_log', 'Rust入門', 's', '2026-01-02', '2026-01-02'),
                    ('d', 'a1', NULL, 'daily', 'daily_log', '2026-01-02', 's', '2026-01-02', '2026-01-02'),
                    ('cat1', 'a1', NULL, 'category', 'category', 'kojiraさんの教え', '', '2026-02-01', '2026-02-01'),
                    ('meta1', 'a1', NULL, 'meta', 'category', 'ルール群', '', '2026-02-01', '2026-02-01');
             -- 全ノードを FTS へ（分類ノードも。孤児掃除の対象になる）。
             INSERT INTO memory_index_fts (title, summary, keywords, node_id, agent_id, node_type, source_type)
             SELECT title, summary, '', id, agent_id, node_type, source_type FROM memory_index_nodes;
             -- 旧 PK members に割当を積む（白紙化される）。
             INSERT INTO memory_category_members (agent_id, topic_id, category_id, created_at)
             VALUES ('a1', 't', 'cat1', '2026-02-01');
             -- 記憶本文（絶対に消さない）。
             INSERT INTO memory_curated (id, agent_id, category, content, updated_at)
             VALUES ('m1', 'a1', 'long_term/rule', '送金は必ず二重確認', '2026-02-01'),
                    ('m2', 'a1', 'reflection', '振り返り', '2026-02-01');
             PRAGMA user_version = 25;",
        )
        .unwrap();

        // --- 適用前のスナップショット ---
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        let curated_before = count("SELECT COUNT(*) FROM memory_curated");
        let timeline_before = count(
            "SELECT COUNT(*) FROM memory_index_nodes
                 WHERE node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly')",
        );
        assert_eq!(curated_before, 2);
        assert_eq!(timeline_before, 5);
        assert_eq!(
            count("SELECT COUNT(*) FROM memory_index_nodes WHERE node_type IN ('category','meta')"),
            2
        );
        assert_eq!(count("SELECT COUNT(*) FROM memory_category_members"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM memory_index_fts"), 7);

        // --- v26 を適用（run_migrations 経路のみ。本番と同じ道）---
        run_migrations(&conn, MIGRATIONS).expect("v26 migration");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // 分類ノードは消え、時系列ツリーと memory_curated は 1 行も減らない。
        assert_eq!(
            count("SELECT COUNT(*) FROM memory_index_nodes WHERE node_type IN ('category','meta')"),
            0,
            "分類ノードが白紙化される"
        );
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM memory_index_nodes
                     WHERE node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly')",
            ),
            timeline_before,
            "時系列ツリーは 1 行も減らない"
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM memory_curated"),
            curated_before,
            "記憶本文は 1 行も減らない"
        );
        // FTS 孤児が残らない: 分類ノードの 2 行が消え、時系列 5 行だけが残る。
        assert_eq!(
            count("SELECT COUNT(*) FROM memory_index_fts"),
            5,
            "FTS 孤児を残さない"
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM memory_index_fts WHERE node_type IN ('category','meta')"),
            0
        );
        // node_type の CHECK からは category/meta を外さない（段階2で使い直す）: 再登録できる。
        conn.execute(
            "INSERT INTO memory_index_nodes (id, agent_id, node_type, source_type, title, summary, created_at, updated_at)
             VALUES ('cat2', 'a1', 'category', 'category', 'Y', '', '2026-03-01', '2026-03-01')",
            [],
        )
        .expect("category は CHECK に残っているので再登録できる");

        // members は空になり、PK が多対多。同一 topic に複数 category を入れられる。
        assert_eq!(
            count("SELECT COUNT(*) FROM memory_category_members"),
            0,
            "members は白紙化"
        );
        conn.execute(
            "INSERT INTO memory_category_members (agent_id, topic_id, category_id, created_at)
             VALUES ('a1', 't', 'cat2', '2026-03-01'),
                    ('a1', 't', 'meta9', '2026-03-02')",
            [],
        )
        .expect("多対多 PK では同一 topic に複数 category を付けられる");
        assert!(
            conn.execute(
                "INSERT INTO memory_category_members (agent_id, topic_id, category_id, created_at)
                 VALUES ('a1', 't', 'cat2', '2026-03-03')",
                [],
            )
            .is_err(),
            "同一 (agent_id, topic_id, category_id) の重複は PK で拒否される"
        );

        // --- 冪等性: 2 回目は落ちず、既に多対多なので members を作り直さない（行が残る）---
        run_migrations(&conn, MIGRATIONS).expect("v26 は冪等（2 回目も落ちない）");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        assert_eq!(
            count("SELECT COUNT(*) FROM memory_category_members"),
            2,
            "2 回目は members を作り直さない（多対多を検知して skip）"
        );
    }

    /// v32: 既存の受信行（送信者名義）を受信側エージェント名義へ付け替え、索引/FTS に
    /// 載せる（#380）。session_id に埋まった agent_id で受信側を復元できる行だけを対象にし、
    /// 復元できない行・新形・旧々形・応答は 1 行も触らないこと、FTS も同時に直ること、
    /// FTS 検索へ受信側名義で載ること、冪等であることを固定する。索引ビルドについては
    /// 「watermark を巻き戻せば載る形」かつ「watermark 先行下では載らない」の両側を固定する
    /// （v32 が回復するのは FTS 検索のみで、索引への取り込みは #380 に残る）。
    #[test]
    fn v32_remaps_inbound_agent_id_to_recipient_and_indexes() {
        use crate::queries::{
            get_unindexed_session_logs, insert_session_log, search_session_logs, SessionLogRow,
        };
        let conn = crate::init_memory().expect("init");

        // 受信側エージェント（session_id に UUID が埋まる）。migration は agents と JOIN する。
        let recipient = "aaaaaaaa-1111-2222-3333-444444444444";
        conn.execute(
            "INSERT INTO agents (agent_id, name, persona_name) VALUES (?1, 'r', 'p')",
            [recipient],
        )
        .unwrap();

        let mk = |agent: &str, session: &str, speaker: &str, content: &str, meta: Option<&str>| {
            SessionLogRow {
                id: None,
                agent_id: agent.to_string(),
                session_id: session.to_string(),
                log_type: "speech".to_string(),
                content: content.to_string(),
                speaker_id: Some(speaker.to_string()),
                turn_number: None,
                metadata_json: meta.map(|m| m.to_string()),
                created_at: None,
            }
        };

        // (1) 旧形の受信行・discord（復元可能）: agent=speaker=送信者、session に recipient が埋まる。
        let id_discord = insert_session_log(
            &conn,
            &mk(
                "sender-d",
                &format!("discord-{recipient}-100-200"),
                "sender-d",
                "discord inbound apple",
                Some(r#"{"source":"discord"}"#),
            ),
        )
        .unwrap();
        // (2) 旧形の受信行・nostr（復元可能, pubkey 付き session）。
        let id_nostr = insert_session_log(
            &conn,
            &mk(
                "sender-n",
                &format!("nostr-{recipient}-deadbeef"),
                "sender-n",
                "nostr inbound banana",
                Some(r#"{"source":"nostr"}"#),
            ),
        )
        .unwrap();
        // (3) 旧形の受信行・nostr（復元可能, recipient 単独 session）。
        let id_nostr2 = insert_session_log(
            &conn,
            &mk(
                "sender-n2",
                &format!("nostr-{recipient}"),
                "sender-n2",
                "nostr inbound cherry",
                Some(r#"{"source":"nostr"}"#),
            ),
        )
        .unwrap();
        // (4) 復元不能: discord-{guild}-{channel}（agent_id が埋まっていない）→ 触らない。
        let id_unresolved = insert_session_log(
            &conn,
            &mk(
                "sender-u",
                "discord-100-200",
                "sender-u",
                "unresolved inbound durian",
                Some(r#"{"source":"discord"}"#),
            ),
        )
        .unwrap();
        // (5) 新形の受信行（既に受信側名義, agent≠speaker）→ 触らない。
        let id_newform = insert_session_log(
            &conn,
            &mk(
                recipient,
                &format!("discord-{recipient}-100-200"),
                "sender-x",
                "newform inbound elder",
                Some(r#"{"source":"discord"}"#),
            ),
        )
        .unwrap();
        // (6) 旧々形（metadata 無し・既に受信側名義）→ 触らない。
        let id_oldold = insert_session_log(
            &conn,
            &mk(
                recipient,
                &format!("discord-{recipient}-100-200"),
                "sender-y",
                "oldold inbound fig",
                None,
            ),
        )
        .unwrap();
        // (7) 応答行（source discord_response, agent=speaker=recipient）→ 触らない。
        let id_reply = insert_session_log(
            &conn,
            &mk(
                recipient,
                &format!("discord-{recipient}-100-200"),
                recipient,
                "reply grape",
                Some(r#"{"source":"discord_response"}"#),
            ),
        )
        .unwrap();

        // v31 起点へ落として run_migrations で v32 を実際に走らせる。
        conn.execute_batch("PRAGMA user_version = 31").unwrap();
        run_migrations(&conn, MIGRATIONS).expect("v32");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        let agent_of = |id: i64| -> String {
            conn.query_row(
                "SELECT agent_id FROM memory_sessions WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        let fts_agent_of = |id: i64| -> String {
            conn.query_row(
                "SELECT agent_id FROM memory_sessions_fts WHERE rowid=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        let speaker_of = |id: i64| -> String {
            conn.query_row(
                "SELECT speaker_id FROM memory_sessions WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };

        // (1)(2)(3) 受信側名義へ付け替わり、FTS も追随、speaker_id は送信者のまま。
        for (id, sender) in [
            (id_discord, "sender-d"),
            (id_nostr, "sender-n"),
            (id_nostr2, "sender-n2"),
        ] {
            assert_eq!(agent_of(id), recipient, "本体 agent_id が受信側へ");
            assert_eq!(fts_agent_of(id), recipient, "FTS agent_id が受信側へ");
            assert_eq!(speaker_of(id), sender, "speaker_id は送信者のまま");
        }

        // (4)(5)(6)(7) 触っていない。
        assert_eq!(agent_of(id_unresolved), "sender-u", "復元不能行は不変");
        assert_eq!(
            fts_agent_of(id_unresolved),
            "sender-u",
            "復元不能行の FTS も不変"
        );
        assert_eq!(agent_of(id_newform), recipient, "新形は不変");
        assert_eq!(agent_of(id_oldold), recipient, "旧々形は不変");
        assert_eq!(agent_of(id_reply), recipient, "応答行は不変");

        // 索引ビルド入力に受信側名義で載る（送信者名義では載らない）。
        //
        // ここは `after_id = 0`（＝索引 watermark を巻き戻した状態）での確認であって、
        // 「watermark を巻き戻せば受信側名義で載る形になっている」ことだけを固定している。
        // 実際の索引ビルドは watermark（`last_indexed_log_id`）を `after_id` へ渡す
        // （`crates/core/src/memory_index/index_builder.rs`）ため、**本番のように watermark が
        // 対象行より先行している状況では、v32 だけでは索引へ入らない**（直下でその側も固定する）。
        // 索引ビルドへの実取り込みには別途 #380 の対応が要る。
        let indexed = get_unindexed_session_logs(&conn, recipient, 0, 100).unwrap();
        assert!(indexed.iter().any(|r| r.content == "discord inbound apple"));
        assert!(indexed.iter().any(|r| r.content == "nostr inbound banana"));
        assert!(
            get_unindexed_session_logs(&conn, "sender-d", 0, 100)
                .unwrap()
                .is_empty(),
            "受信行が送信者名義で索引入力に残っている"
        );

        // watermark が対象行より先行している状態（本番がこれ）では、付け替えても索引ビルド
        // 入力には載らない。v32 の効き目が FTS 検索に限られることを明示的に固定する。
        //
        // watermark は**付け替え対象の最大 id**（=(3)）にする。全体の MAX(id) にすると結果が
        // 必ず空になり、付け替えが 1 行も起きていなくても通る空回りのテストになる。(3) を境に
        // すれば recipient 名義の (5)(6)(7) は結果に残るので、「クエリが何も返していないだけ」
        // ではなく「付け替え行**だけ**が watermark に切られている」ことを固定できる。
        let above_watermark = get_unindexed_session_logs(&conn, recipient, id_nostr2, 100).unwrap();
        assert!(
            above_watermark
                .iter()
                .any(|r| r.content == "newform inbound elder"),
            "watermark より上の受信側名義行は載る（フィルタが空を返しているだけではない証拠）"
        );
        assert!(
            !above_watermark
                .iter()
                .any(|r| r.content == "discord inbound apple"
                    || r.content == "nostr inbound banana"
                    || r.content == "nostr inbound cherry"),
            "watermark 先行下では付け替え行は索引ビルド入力に載らない（#380 の残課題）"
        );

        // FTS 記憶検索で受信側が相手の発言を引ける。送信者名義では引けない。
        let hits = search_session_logs(&conn, recipient, "apple", 10).unwrap();
        assert!(hits.iter().any(|h| h.content == "discord inbound apple"));
        assert!(search_session_logs(&conn, "sender-d", "apple", 10)
            .unwrap()
            .is_empty());
        // 復元不能行は送信者名義のまま（付け替えていない証拠 = 誤って混ぜていない）。
        assert!(search_session_logs(&conn, "sender-u", "durian", 10)
            .unwrap()
            .iter()
            .any(|h| h.content == "unresolved inbound durian"));

        // 冪等: 版を 31 へ戻して up() を再実行しても、付け替えは二重に起きない。
        let same_before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_sessions WHERE agent_id = speaker_id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute_batch("PRAGMA user_version = 31").unwrap();
        run_migrations(&conn, MIGRATIONS).expect("v32 再実行");
        let same_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_sessions WHERE agent_id = speaker_id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(same_before, same_after, "2 回目で付け替えが二重に起きない");
        assert_eq!(
            agent_of(id_discord),
            recipient,
            "再実行後も (1) は受信側のまま"
        );
        assert_eq!(
            agent_of(id_unresolved),
            "sender-u",
            "再実行後も復元不能行は不変"
        );
    }

    /// v33: sleep のメンテナンスラン（宣言 `sleep-declare-*` / 整理 `sleep-organize-*`）が
    /// 生んだ生ログを消す（#393）。本体と FTS を**同一 rowid 集合**で消すこと（＝孤児を
    /// 増やさない）、他の接頭辞のログを 1 行も巻き込まないこと、既存の孤児に触らないこと、
    /// 運用記録（`llm_logs` / `agent_logs`）を消さないこと、冪等であることを固定する。
    #[test]
    fn v33_deletes_maintenance_run_logs_from_both_tables() {
        use crate::queries::{insert_session_log, SessionLogRow};
        let conn = crate::init_memory().expect("init");
        let agent = "a1";

        let mk = |session: &str, content: &str| SessionLogRow {
            id: None,
            agent_id: agent.to_string(),
            session_id: session.to_string(),
            log_type: "speech".to_string(),
            content: content.to_string(),
            speaker_id: Some(agent.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };

        // メンテナンスランの生ログ（消える）。
        let id_declare = insert_session_log(&conn, &mk("sleep-declare-a1-1700000000", "declare"))
            .expect("declare");
        let id_organize =
            insert_session_log(&conn, &mk("sleep-organize-a1-1700000001", "organize"))
                .expect("organize");
        // 通常の会話・ハートビート等（残る）。`sleep` を含むが接頭辞ではない session_id や、
        // 接頭辞に見えて別物の `sleep-` も混ぜて、LIKE が広く効きすぎないことを確かめる。
        let keep = [
            ("discord-a1-100-200", "discord"),
            ("heartbeat-a1-100", "heartbeat"),
            ("subtask-42", "subtask"),
            ("nostr-a1", "nostr"),
            ("web-a1-conv", "web"),
            ("agent-msg-a1-u1", "rest"),
            ("discord-a1-sleep-declare-x", "not a maintenance run"),
        ];
        let keep_ids: Vec<i64> = keep
            .iter()
            .map(|(s, c)| insert_session_log(&conn, &mk(s, c)).expect("keep"))
            .collect();

        // 既存の孤児（本体に対応行が無い FTS 行）を 1 件仕込む。本番にも 208 行あり、
        // v33 がそれを増やしも減らしもしないことを固定する。
        conn.execute(
            "INSERT INTO memory_sessions_fts (rowid, content, agent_id, session_id, log_type)
             VALUES (999999, 'orphan', ?1, 'discord-a1-orphan', 'speech')",
            [agent],
        )
        .expect("orphan");

        // 運用記録（#393 の追加受け入れ条件）。同じメンテナンスランの `llm_logs`
        // （session_id で引ける生プロンプト/生応答/tool_calls）と `agent_logs`
        // （context="sleep" の 1 ラン 1 行の要約）を仕込み、**v33 がこれらを消さない**ことを
        // 固定する。生ログから外すのは「記憶の材料としての扱い」だけで、何を行ったかの
        // 運用記録は残す。
        crate::queries::insert_llm_log(
            &conn,
            &crate::queries::LlmLogRow {
                id: "llm-1".to_string(),
                agent_id: agent.to_string(),
                session_id: Some("sleep-declare-a1-1700000000".to_string()),
                model: Some("m".to_string()),
                prompt: "p".to_string(),
                response: "r".to_string(),
                tool_calls: Some("[]".to_string()),
                latency_ms: Some(1),
                prompt_tokens: None,
                completion_tokens: None,
                total_tokens: None,
                error_code: None,
                error_body: None,
                requested_at: None,
                trigger_message_id: None,
                is_bot_iteration: false,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                created_at: "2026-08-05T00:00:00+00:00".to_string(),
            },
        )
        .expect("llm_log");
        crate::queries::insert_agent_log(
            &conn,
            &crate::queries::AgentLogRow {
                id: "audit-1".to_string(),
                agent_id: Some(agent.to_string()),
                level: "info".to_string(),
                context: "sleep".to_string(),
                message: r#"{"kind":"memory_declare"}"#.to_string(),
                created_at: None,
            },
        )
        .expect("agent_log");

        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        let orphans = |c: &Connection| -> i64 {
            c.query_row(
                "SELECT COUNT(*) FROM memory_sessions_fts f
                 LEFT JOIN memory_sessions m ON m.id = f.rowid WHERE m.id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        let body_before = count("SELECT COUNT(*) FROM memory_sessions");
        let fts_before = count("SELECT COUNT(*) FROM memory_sessions_fts");
        let orphans_before = orphans(&conn);
        assert_eq!(orphans_before, 1, "孤児の仕込みが効いている");

        conn.execute_batch("PRAGMA user_version = 32").unwrap();
        run_migrations(&conn, MIGRATIONS).expect("v33");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // 対象 2 行が本体からも FTS からも消えている。
        for id in [id_declare, id_organize] {
            assert_eq!(
                count(&format!(
                    "SELECT COUNT(*) FROM memory_sessions WHERE id = {id}"
                )),
                0,
                "本体から消える"
            );
            assert_eq!(
                count(&format!(
                    "SELECT COUNT(*) FROM memory_sessions_fts WHERE rowid = {id}"
                )),
                0,
                "FTS からも同じ rowid で消える"
            );
        }
        // 他は 1 行も巻き込んでいない（本体・FTS とも）。
        for id in &keep_ids {
            assert_eq!(
                count(&format!(
                    "SELECT COUNT(*) FROM memory_sessions WHERE id = {id}"
                )),
                1,
                "メンテナンスラン以外は残る"
            );
            assert_eq!(
                count(&format!(
                    "SELECT COUNT(*) FROM memory_sessions_fts WHERE rowid = {id}"
                )),
                1,
                "FTS 側も残る"
            );
        }
        assert_eq!(
            count("SELECT COUNT(*) FROM memory_sessions"),
            body_before - 2
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM memory_sessions_fts"),
            fts_before - 2
        );
        assert_eq!(orphans(&conn), orphans_before, "孤児は増えも減りもしない");

        // 運用記録は 1 行も消えていない。session_id が対象と同じ `sleep-declare-%` でも
        // `llm_logs` は対象外（消すのは memory_sessions と memory_sessions_fts だけ）。
        assert_eq!(
            count("SELECT COUNT(*) FROM llm_logs WHERE session_id LIKE 'sleep-declare-%'"),
            1,
            "llm_logs は消さない（何を行ったかの記録は残す）"
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM agent_logs WHERE context = 'sleep'"),
            1,
            "agent_logs（sleep 監査）は消さない"
        );

        // 冪等: 版を 32 へ戻して再実行しても何も変わらない（対象 0 行）。
        let body_after = count("SELECT COUNT(*) FROM memory_sessions");
        let fts_after = count("SELECT COUNT(*) FROM memory_sessions_fts");
        conn.execute_batch("PRAGMA user_version = 32").unwrap();
        run_migrations(&conn, MIGRATIONS).expect("v33 再実行");
        assert_eq!(count("SELECT COUNT(*) FROM memory_sessions"), body_after);
        assert_eq!(count("SELECT COUNT(*) FROM memory_sessions_fts"), fts_after);
        assert_eq!(orphans(&conn), orphans_before);
        assert_eq!(count("SELECT COUNT(*) FROM llm_logs"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM agent_logs"), 1);
    }

    /// v33（索引側 / #393 の 2 巡目レビュー指摘）: メンテナンスランのログから作られた
    /// 索引ノードも消す。生ログだけ消すと「タイトルと要約はあるが中身を引くと空が返る記憶」が
    /// 残るため（索引ビルドは本番で対象 id 帯を通過済み）。
    ///
    /// 固定するもの:
    /// - `source_session_id` がメンテナンスランを指す session / topic ノードが本体と
    ///   `memory_index_fts` の**同一 node_id 集合**で消えること
    /// - 対象ノードの子孫も消えること（FTS 孤児を残さない）
    /// - 空になる親（period）は**残す**こと
    /// - 通常の会話由来のノードを 1 件も巻き込まないこと
    /// - 宣言ユニットは「範囲が丸ごとメンテナンスラン由来」のものだけ消し、通常ログが
    ///   混じるユニット・範囲が空のユニットは残すこと
    /// - `memory_category_members` の宙に浮く参照が消えること（残るノードの行は残ること）
    /// - 冪等であること
    #[test]
    fn v33_deletes_maintenance_run_index_nodes_and_only_pure_maintenance_units() {
        use crate::queries::{insert_index_node, insert_session_log, IndexNodeRow, SessionLogRow};
        let conn = crate::init_memory().expect("init");
        let agent = "a1";

        let log = |session: &str, content: &str| SessionLogRow {
            id: None,
            agent_id: agent.to_string(),
            session_id: session.to_string(),
            log_type: "speech".to_string(),
            content: content.to_string(),
            speaker_id: Some(agent.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        // 生ログ: メンテナンスラン 2 行 → 通常 1 行 → メンテナンスラン 1 行 の順に入れて、
        // ユニットの範囲が「丸ごと」か「混在」かを id 範囲で作り分けられるようにする。
        let m1 = insert_session_log(&conn, &log("sleep-declare-a1-1", "m1")).unwrap();
        let m2 = insert_session_log(&conn, &log("sleep-declare-a1-1", "m2")).unwrap();
        let normal = insert_session_log(&conn, &log("discord-a1-1-2", "normal")).unwrap();
        let m3 = insert_session_log(&conn, &log("sleep-organize-a1-1", "m3")).unwrap();

        let node = |id: &str,
                    parent: Option<&str>,
                    node_type: &str,
                    src_session: Option<&str>,
                    range: Option<(i64, i64)>| IndexNodeRow {
            id: id.to_string(),
            agent_id: agent.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            node_type: node_type.to_string(),
            source_type: if node_type == "unit" {
                "declared".to_string()
            } else {
                "session_log".to_string()
            },
            title: format!("title-{id}"),
            summary: format!("summary-{id}"),
            start_log_id: range.map(|r| r.0),
            end_log_id: range.map(|r| r.1),
            source_session_id: src_session.map(|s| s.to_string()),
            date_from: None,
            date_to: None,
            depth: 2,
            child_count: 0,
            token_count: 0,
            created_at: "2026-08-05T00:00:00+00:00".to_string(),
            updated_at: "2026-08-05T00:00:00+00:00".to_string(),
            short_id: None,
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        };

        // 親（period）と、その下のメンテナンス由来 session + その topic 子。
        insert_index_node(&conn, &node("period-1", None, "period", None, None)).unwrap();
        insert_index_node(
            &conn,
            &node(
                "sess-m",
                Some("period-1"),
                "session",
                Some("sleep-declare-a1-1"),
                Some((m1, m2)),
            ),
        )
        .unwrap();
        insert_index_node(
            &conn,
            &node(
                "topic-m",
                Some("sess-m"),
                "topic",
                Some("sleep-declare-a1-1"),
                Some((m1, m2)),
            ),
        )
        .unwrap();
        // 対象 session の子だが `source_session_id` が無い（再帰で一緒に消える = FTS 孤児を残さない）。
        insert_index_node(
            &conn,
            &node(
                "topic-m-nosrc",
                Some("sess-m"),
                "topic",
                None,
                Some((m1, m2)),
            ),
        )
        .unwrap();
        // 通常の会話由来（残る）。period-1 の子でもあるので、親が空にならない側も確かめられる。
        insert_index_node(
            &conn,
            &node(
                "sess-ok",
                Some("period-1"),
                "session",
                Some("discord-a1-1-2"),
                Some((normal, normal)),
            ),
        )
        .unwrap();
        insert_index_node(
            &conn,
            &node(
                "topic-ok",
                Some("sess-ok"),
                "topic",
                Some("discord-a1-1-2"),
                Some((normal, normal)),
            ),
        )
        .unwrap();
        // 子が全て消えて空になる period（本番に 2 件あるケース）。索引ビルダの child_count
        // 再計算は「子を持つ親」しか書かないので、ここが 0 へ直らないと永久にずれたままになる。
        insert_index_node(&conn, &node("period-2", None, "period", None, None)).unwrap();
        insert_index_node(
            &conn,
            &node(
                "sess-m2",
                Some("period-2"),
                "session",
                Some("sleep-organize-a1-1"),
                Some((m3, m3)),
            ),
        )
        .unwrap();
        // ユニット 3 種。範囲が丸ごとメンテナンス由来 / 通常ログ混在 / 範囲が空。
        // 宣言ユニットの根（本番の `declroot-{agent_id}`）も置き、子を 1 つ失う側を作る。
        insert_index_node(&conn, &node("declroot-1", None, "root", None, None)).unwrap();
        insert_index_node(
            &conn,
            &node(
                "unit-pure",
                Some("declroot-1"),
                "unit",
                None,
                Some((m1, m2)),
            ),
        )
        .unwrap();
        insert_index_node(
            &conn,
            &node(
                "unit-mixed",
                Some("declroot-1"),
                "unit",
                None,
                Some((m1, m3)),
            ),
        )
        .unwrap();
        insert_index_node(
            &conn,
            &node(
                "unit-empty",
                Some("declroot-1"),
                "unit",
                None,
                Some((900_000, 900_001)),
            ),
        )
        .unwrap();

        // 健全な索引の状態を作る: child_count を実カウントに揃える（本番も全ノードで一致している）。
        conn.execute_batch(
            "UPDATE memory_index_nodes
                 SET child_count = (SELECT COUNT(*) FROM memory_index_nodes c
                                     WHERE c.parent_id = memory_index_nodes.id);",
        )
        .unwrap();

        // カテゴリ所属: 消える topic と残る topic の両方に付ける。
        conn.execute_batch(
            "INSERT INTO memory_category_members (agent_id, topic_id, category_id, created_at)
                 VALUES ('a1','topic-m','cat-1','2026-08-05T00:00:00+00:00'),
                        ('a1','topic-ok','cat-1','2026-08-05T00:00:00+00:00');",
        )
        .unwrap();

        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        let exists = |id: &str| -> i64 {
            count(&format!(
                "SELECT COUNT(*) FROM memory_index_nodes WHERE id = '{id}'"
            ))
        };
        let in_fts = |id: &str| -> i64 {
            count(&format!(
                "SELECT COUNT(*) FROM memory_index_fts WHERE node_id = '{id}'"
            ))
        };
        let fts_orphans = || -> i64 {
            count(
                "SELECT COUNT(*) FROM memory_index_fts f
                 LEFT JOIN memory_index_nodes n ON n.id = f.node_id WHERE n.id IS NULL",
            )
        };
        let nodes_without_fts = || -> i64 {
            count(
                "SELECT COUNT(*) FROM memory_index_nodes n
                 LEFT JOIN memory_index_fts f ON f.node_id = n.id WHERE f.node_id IS NULL",
            )
        };
        let child_count_of = |id: &str| -> i64 {
            count(&format!(
                "SELECT child_count FROM memory_index_nodes WHERE id = '{id}'"
            ))
        };
        let child_count_mismatch = || -> i64 {
            count(
                "SELECT COUNT(*) FROM memory_index_nodes n
                 WHERE n.child_count <> (SELECT COUNT(*) FROM memory_index_nodes c
                                          WHERE c.parent_id = n.id)",
            )
        };
        assert_eq!(count("SELECT COUNT(*) FROM memory_index_nodes"), 12);
        assert_eq!(fts_orphans(), 0);
        assert_eq!(nodes_without_fts(), 0);
        assert_eq!(child_count_mismatch(), 0, "仕込み時点では全ノード一致");
        assert_eq!(child_count_of("period-1"), 2);
        assert_eq!(child_count_of("period-2"), 1);
        assert_eq!(child_count_of("declroot-1"), 3);

        conn.execute_batch("PRAGMA user_version = 32").unwrap();
        run_migrations(&conn, MIGRATIONS).expect("v33");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // 消えたもの（本体と FTS の両方から）。
        for id in ["sess-m", "sess-m2", "topic-m", "topic-m-nosrc", "unit-pure"] {
            assert_eq!(exists(id), 0, "{id} は消える");
            assert_eq!(in_fts(id), 0, "{id} は FTS からも同じ集合で消える");
        }
        // 残るもの。空になった period も残す（索引ビルダが同じ id で再利用するキー）。
        for id in [
            "period-1",
            "period-2",
            "declroot-1",
            "sess-ok",
            "topic-ok",
            "unit-mixed",
            "unit-empty",
        ] {
            assert_eq!(exists(id), 1, "{id} は残る");
            assert_eq!(in_fts(id), 1, "{id} の FTS も残る");
        }
        assert_eq!(count("SELECT COUNT(*) FROM memory_index_nodes"), 7);
        assert_eq!(fts_orphans(), 0, "FTS 孤児を作らない");
        assert_eq!(nodes_without_fts(), 0, "本体だけのノードも作らない");

        // 生き残る親の child_count が実カウントへ直っている。**子が 0 になった period-2 まで
        // 直ること**が肝（索引ビルダの再計算は子を持つ親しか書かないので、ここで直さないと
        // 永久にずれたまま残る）。
        assert_eq!(child_count_mismatch(), 0, "削除後も全ノードで一致");
        assert_eq!(child_count_of("period-1"), 1, "sess-m を失って 2→1");
        assert_eq!(child_count_of("period-2"), 0, "子が全て消えて 1→0");
        assert_eq!(child_count_of("declroot-1"), 2, "unit-pure を失って 3→2");

        // カテゴリ所属: 消えた topic への参照だけが消え、残る topic の行は残る。
        assert_eq!(
            count("SELECT COUNT(*) FROM memory_category_members WHERE topic_id = 'topic-m'"),
            0,
            "宙に浮く参照を残さない"
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM memory_category_members WHERE topic_id = 'topic-ok'"),
            1,
            "残るノードの所属は残す"
        );

        // 生ログ側: メンテナンス 3 行が消え、通常 1 行は残る。
        assert_eq!(count("SELECT COUNT(*) FROM memory_sessions"), 1);
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE id = {normal}"
            )),
            1
        );

        // FTS integrity-check（削除で索引が壊れていないこと）。
        conn.execute_batch(
            "INSERT INTO memory_index_fts(memory_index_fts) VALUES('integrity-check');",
        )
        .expect("memory_index_fts integrity");

        // 冪等: 版を 32 へ戻して再実行しても何も変わらない。生ログが消えた後は
        // 「範囲に 1 件以上ある」が成り立たないので、unit-mixed / unit-empty も対象外のまま。
        conn.execute_batch("PRAGMA user_version = 32").unwrap();
        run_migrations(&conn, MIGRATIONS).expect("v33 再実行");
        assert_eq!(count("SELECT COUNT(*) FROM memory_index_nodes"), 7);
        assert_eq!(count("SELECT COUNT(*) FROM memory_sessions"), 1);
        assert_eq!(exists("unit-mixed"), 1, "再実行で本人の記憶を巻き込まない");
        assert_eq!(exists("unit-empty"), 1);
        assert_eq!(fts_orphans(), 0);
        assert_eq!(
            child_count_mismatch(),
            0,
            "再実行でも child_count は一致のまま"
        );
    }

    /// v20 起点の一気通貫（v20→v21→v22→v23）。稼働中の本番 DB は v22 なので実運用の
    /// 経路は v22→v23 だが、新規環境や古い DB からの復元では v20 から連鎖する。この道で
    /// (1) memory_index の時系列ツリーが 1 件も失われず CHECK が広がること、(2) 途中の
    /// v21（impressions を agent スコープへ）と v22（owner_pubkey 追加）も併せて適用され
    /// 最終版へ到達することを固定する（従来は user_version=22 を手で置いた単独移行のみ）。
    #[test]
    fn migration_chain_from_v20_reaches_latest_and_preserves_memory_index() {
        let conn = crate::init_memory().expect("init");

        // --- v22 相当の memory_index_nodes（狭い CHECK）へ戻し、時系列ツリーを積む ---
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        conn.execute_batch(
            "DROP TABLE memory_index_nodes;
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
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, short_id TEXT,
                keywords_json TEXT NOT NULL DEFAULT '[]', summary_refreshed_at TEXT
             );
             INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, title, summary, created_at, updated_at)
             VALUES ('r', 'a1', NULL, 'root', 'root', 's', '2026-01-01', '2026-01-01'),
                    ('p', 'a1', 'r', 'period', '2026-01', 's', '2026-01-01', '2026-01-01'),
                    ('sess', 'a1', 'p', 'session', 'S', 's', '2026-01-01', '2026-01-01'),
                    ('t', 'a1', 'sess', 'topic', 'Rust入門', 's', '2026-01-02', '2026-01-02');
             DROP TABLE IF EXISTS memory_category_members;",
        )
        .unwrap();

        // --- v21 相当の agent_nostr_config（owner_pubkey 無し）へ戻す（v22 を実際に走らせる）---
        conn.execute_batch("ALTER TABLE agent_nostr_config DROP COLUMN owner_pubkey")
            .unwrap();
        assert!(!column_exists(&conn, "agent_nostr_config", "owner_pubkey").unwrap());

        // --- v20 相当の impressions（旧一意制約）へ戻し、user_version を 20 に落とす ---
        // 同一 (agent_id, target_id) を別セッションで 2 行 + 別 target を 1 行（v21 の統合を確認）。
        seed_legacy_impressions(
            &conn,
            "('i1','a1','s1','u1','U','','','','中立','',0,'2026-01-01','2026-01-02'),\
             ('i2','a1','s2','u1','U','','','','中立','',0,'2026-01-03','2026-01-01'),\
             ('i3','a1','s1','u2','V','','','','中立','',0,'2026-01-01','2026-01-01')",
        );
        assert_eq!(schema_version(&conn).unwrap(), 20);

        // 一気通貫で v20 → 最新へ。
        run_migrations(&conn, MIGRATIONS).expect("v20 -> latest chain");
        assert_eq!(schema_version(&conn).unwrap(), latest_version());

        // (1) memory_index: 時系列ツリーは 1 件も失われない。
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM memory_index_nodes ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(ids, vec!["p", "r", "sess", "t"]);
        // CHECK は広がり、category が実際に保存できる（OR IGNORE の沈黙が起きない）。
        conn.execute(
            "INSERT INTO memory_index_nodes (id, agent_id, node_type, source_type, title, summary, created_at, updated_at)
             VALUES ('cat1', 'a1', 'category', 'category', 'X', '', '2026-02-01', '2026-02-01')",
            [],
        )
        .expect("移行後は category を登録できる");
        assert!(table_exists(&conn, "memory_category_members").unwrap());

        // (2) v21 が適用され、impressions が (agent_id, target_id) スコープへ畳まれている。
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM impressions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 2, "同一 (agent_id, target_id) は 1 行へ統合される");
        let u1_created: String = conn
            .query_row(
                "SELECT created_at FROM impressions WHERE agent_id='a1' AND target_id='u1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            u1_created, "2026-01-01",
            "created_at は統合対象の最小値を引き継ぐ"
        );

        // (3) v22 が適用され、owner_pubkey 列が復活している。
        assert!(column_exists(&conn, "agent_nostr_config", "owner_pubkey").unwrap());
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
