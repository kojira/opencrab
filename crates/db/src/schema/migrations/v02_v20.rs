use super::super::*;

pub(super) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 2,
        description: "task ledger: goal/contract/progress (issue #50)",
        up: |conn| conn.execute_batch(TASK_LEDGER_SQL),
    },
    Migration {
        version: 3,
        description: "retired legacy external-user display migration",
        up: |_| Ok(()),
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
        description: "retired gateway-owned configuration migration",
        up: |_| Ok(()),
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
        description: "retired legacy identity-source migration",
        up: |_| Ok(()),
    },
    Migration {
        version: 17,
        description: "retired legacy trusted-user naming migration",
        up: |_| Ok(()),
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
        description: "retired gateway-owned relay configuration migration",
        up: |_| Ok(()),
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
];
