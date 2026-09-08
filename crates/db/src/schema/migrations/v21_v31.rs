use super::super::*;

pub(super) const MIGRATIONS: &[Migration] = &[
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
];
