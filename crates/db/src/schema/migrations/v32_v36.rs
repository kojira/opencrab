use super::super::*;

pub(super) const MIGRATIONS: &[Migration] = &[
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
        // #454: 外部 source（第一号 sample-source）の webhook / catch-up ポーリングで受け取った
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
];
