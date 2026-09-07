use super::super::*;

pub(super) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 43,
        description:
            "sessions に policy_json を足し、session_watches / tool_logs を新設する（載せ替え工程 3）",
        // 会話の単位はセッション。表はそのまま・データ移動ゼロ。
        // 属性は `sessions.policy_json`（DEFAULT '{}' = 現行挙動維持）。
        // agent_sessions には列を足さない。新表は watch 定義と tool_logs だけ。
        //
        // ## やってよいことだけ
        //   1. `ALTER TABLE sessions ADD COLUMN policy_json …`（列が無ければ）
        //   2. `CREATE TABLE IF NOT EXISTS session_watches` / `tool_logs`
        //   3. 同じ TX 内で構造不変アサート（失敗なら ROLLBACK）
        //
        // ## やってはいけないこと
        //   INSERT INTO 既存表、既存行の UPDATE、DROP、列改名、VIEW。
        //   表集合は適用前 ∪ {session_watches, tool_logs} と一致。
        //
        // ## 冪等性（#349/#475 の轍を踏まない）
        // 新規 DB は SCHEMA_SQL 側で列と 2 表を持つので、`column_exists` / `IF NOT EXISTS`
        // で no-op。既存 DB（v42）でのみ ALTER が走る。DDL のみ・既存行は触らない。
        //
        // ## 切り戻し（古いバイナリへ戻すとき）
        // 列と表は残して版番号だけ戻せばよい（古いバイナリは読まない）:
        //   BEGIN; PRAGMA user_version = 42; COMMIT;
        up: migrate_v43_transplant_schema,
    },
    Migration {
        version: 44,
        description: "agents.subject_id と external gate 4 表（V3 §6.1）",
        up: migrate_v44_extgate,
    },
    Migration {
        version: 45,
        description: "nostr_bundle_state（Nostr Bundle coordinator。V3 4表に含めない）",
        // 新規 DB は SCHEMA_SQL 側で表を持つので IF NOT EXISTS で no-op。
        // 既存 DB（v44）でのみ CREATE が走る。DDL のみ・既存行は触らない。
        //
        // ## やってよいことだけ
        //   CREATE TABLE IF NOT EXISTS nostr_bundle_state
        //
        // ## やってはいけないこと
        //   V3 4表への列追加、wire/admin 契約の変更、既存行の UPDATE/DROP。
        //
        // ## 切り戻し
        //   BEGIN; PRAGMA user_version = 44; COMMIT;
        up: migrate_v45_nostr_bundle_state,
    },
    Migration {
        version: 46,
        description:
            "会話圧縮の派生スナップショット表を追加する（#826-B）。正本は変えず行追加のみ",
        // 派生表。正本 memory_sessions は触らない。既存 DB へ CREATE IF NOT EXISTS。
        // #826 では v43 だったが、transplant の v43-45（載せ替え工程）と番号衝突するため
        // 統合時に v46 へ採番し直した。既存 transplant DB（user_version=45）は次回起動で
        // これだけを適用する。切り戻し: BEGIN; PRAGMA user_version = 45; COMMIT;（表は残してよい）
        up: |conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS conversation_snapshots (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    compacted_conversation TEXT NOT NULL,
                    through_log_id INTEGER NOT NULL,
                    token_count INTEGER NOT NULL,
                    created_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_conversation_snapshots_session
                    ON conversation_snapshots(session_id, id);",
            )
        },
    },
    Migration {
        version: 47,
        description:
            "gateway 能力 DI: gateway_operation_calls 表 + gate_instances.operation_declaration_digest（DI 拡張 §10.5）",
        // DI-13: transplant 系譜 v46 を base に後続 migration。generic な DI 表のみで、
        // 個別 gateway 語彙の列・表・CHECK は足さない（§10.5）。callback の
        // gateway_continuations はフェーズ 2 のため作らない。
        //
        // ## やってよいことだけ
        //   CREATE TABLE IF NOT EXISTS gateway_operation_calls
        //   ALTER TABLE gate_instances ADD COLUMN operation_declaration_digest（column_exists ガード）
        //
        // ## やってはいけないこと
        //   V3 4表への破壊的変更、既存行の UPDATE/DROP、operation ごとの列・表。
        //
        // ## 切り戻し
        //   BEGIN; PRAGMA user_version = 46; COMMIT;（表と列は残してよい）
        up: migrate_v47_gateway_operations,
    },
];
