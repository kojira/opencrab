use super::super::*;

pub(super) const MIGRATIONS: &[Migration] = &[
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
    Migration {
        version: 39,
        description:
            "memory_sessions(session_id, log_type, id) 複合インデックス（session_id 先頭クエリの全表 SCAN 解消 / #546）",
        // **#546: session_id を先頭に引くクエリが全表 SCAN だった。**
        //
        // 既存の `idx_memory_sessions_session` は `(agent_id, session_id)` で**先頭が
        // agent_id**。だが #404/#508 の共有チャンネルセッション（`discord-{guild}-{channel}`・
        // session_id に agent を含まず複数 agent が同居）を **agent_id 無し**で引く
        // `list_recent_session_logs_of_type`（`WHERE session_id=? AND log_type=? ORDER BY id
        // DESC LIMIT`）等はこのインデックスを使えず、`EXPLAIN` が `SCAN memory_sessions`
        // （本番 55,580 行）だった。ここに agent_id を足すと呼び手 1 体の行だけに絞られ、他
        // 参加者の発言（共有チャンネル会話の大半）が消えて #404/#508 が壊れる（実測: ある
        // 共有セッションで呼び手 153 件 / 他 7 名 308 件）。＝**クエリは変えずインデックスで解く**。
        //
        // ## 効き（本番コピーでの実測 EXPLAIN QUERY PLAN）
        //   - list_recent_session_logs_of_type / list_recent_user_speech_logs（session_id +
        //     log_type）: `SEARCH … USING INDEX (session_id=? AND log_type=?)`。
        //   - list_recent_session_logs / list_session_logs_by_session
        //     （session_id のみ）: `SEARCH … USING COVERING INDEX (session_id=?)` + 小さな
        //     TEMP B-TREE（全表 SCAN は解消）。**1 本で全部に効く。**
        //
        // ## 本番コピーでの実測（適用前 user_version=38 / memory_sessions 55,580 行 / 2.77GB）
        //   - CREATE INDEX: 約 0.10 秒。
        //   - DB サイズ増: +約 4.85 MB（+0.18%）。
        //   - 冪等: 2 回目は `IF NOT EXISTS` で no-op（約 0.01 秒）。
        //
        // ## 冪等性
        // 新規 DB は SCHEMA_SQL 側で同インデックスを持つ。`CREATE INDEX IF NOT EXISTS` で
        // 2 度流しても no-op。DDL のみでデータは触らない。
        //
        // ## 切り戻し（古いバイナリへ戻すとき・インデックス削除と版番号のみ）
        //   BEGIN;
        //   DROP INDEX IF EXISTS idx_memory_sessions_session_type;
        //   PRAGMA user_version = 38;
        //   COMMIT;
        up: |conn| {
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_memory_sessions_session_type \
                 ON memory_sessions(session_id, log_type, id);",
            )
        },
    },
    Migration {
        version: 40,
        description:
            "co_agent 逆引き表: agent_discord_config.bot_user_id + agent_nostr_config.self_pubkey（発言者識別子→agent UUID / #489）",
        // **#489: co_agent が識別子空間の食い違いで発火しない問題の逆引き表。**
        //
        // `trusted_co_agents` は agent UUID 対（agent_id ↔ co_agent_id）で登録されるのに、
        // caller 解決は経路の生の発言者識別子（Discord user_id / Nostr pubkey）を突き合わせて
        // いたため、UUID 登録の行が一度も一致しなかった。**発言者識別子 → agent UUID の逆引き**を
        // 各設定表に持たせて解消する（案B）。
        //   - `agent_discord_config.bot_user_id`: この bot 自身の Discord user id。
        //   - `agent_nostr_config.self_pubkey`: この agent 自身の Nostr pubkey（64 桁小文字 hex）。
        //
        // ## 汚染防止（最重要）
        // どちらの列も**書くのは各 agent 自身の接続だけ**（Discord: `get_current_user` /
        // Nostr: 自 secret_key 由来 pubkey・identity 切替の新 pubkey）。config 構造体・upsert・
        // REST 設定 API のどれにも載せないので、外部が「識別子 ↔ UUID」を仕込む経路が無い。
        // 既定は空文字で「未接続 = 逆引き不可 = fail-closed」。
        //
        // ## 冪等性（#349/#475 の轍を踏まない）
        // 新規 DB は SCHEMA_SQL 側で両列を持つので、`column_exists` でガードして各 ALTER を
        // no-op にする。既存 DB（v39）でのみ 2 本の ADD COLUMN が走る。DDL のみ・データは触らない
        // （既存 4 行の `trusted_co_agents` も書き換えない）。
        //
        // ## 切り戻し（古いバイナリへ戻すとき）
        // 列を消す必要は無い（古いバイナリは両列を読まない）。版番号だけ戻せばよい:
        //   BEGIN; PRAGMA user_version = 39; COMMIT;
        // 列も落としたい場合は SQLite の DROP COLUMN（3.35+）で:
        //   ALTER TABLE agent_discord_config DROP COLUMN bot_user_id;
        //   ALTER TABLE agent_nostr_config DROP COLUMN self_pubkey;
        up: |conn| {
            if !column_exists(conn, "agent_discord_config", "bot_user_id")? {
                conn.execute_batch(
                    "ALTER TABLE agent_discord_config ADD COLUMN bot_user_id TEXT NOT NULL DEFAULT ''",
                )?;
            }
            if !column_exists(conn, "agent_nostr_config", "self_pubkey")? {
                conn.execute_batch(
                    "ALTER TABLE agent_nostr_config ADD COLUMN self_pubkey TEXT NOT NULL DEFAULT ''",
                )?;
            }
            Ok(())
        },
    },
    Migration {
        version: 41,
        description:
            "provider rename: openai → hermit（agents.model 前置換 + model_pricing / model_experience_notes / llm_provider_overrides の provider 追従 / #660）",
        // **#660: プロバイダの「名乗り名」と「API 形式」を分離した。**
        //
        // `[llm.providers.openai]` は本物の OpenAI ではなくローカルの OpenAI 互換プロキシ
        // （hermit-shell）を指していた。名乗り名を実体に合わせて `hermit` に改め、`type="openai"`
        // で「形式は OpenAI 互換」を表すようにした（config 側）。これに伴い、保存済みの
        // `provider:model` 値（互換エイリアス・フォールバックは入れない方針）を DB でも追従させる。
        //
        // ## なぜ agents.model と model_pricing を同一マイグレーション（＝同一トランザクション）で
        // `run_migrations` は各 Migration を 1 トランザクションで実行するため、ここの 4 本の UPDATE
        // は不可分に適用される。`agents.model` を `hermit:` にしたのに `model_pricing.provider` が
        // `openai` のままだと、context_window ゲート（`context_budget`）が (provider, model) 照合を
        // 外して「未登録」エラーを出し、エージェントが起動時に止まる。**片方だけ動かさない**ことが
        // この 1 トランザクションの主眼。
        //
        // ## 置換規則
        //   - `agents.model`: **先頭アンカー** `LIKE 'openai:%'` のものだけ、`openai:` を `hermit:`
        //     へ。`substr(model, 8)` は "openai:"（7 文字）の次＝コロン後の model 部分（SQLite の
        //     substr は 1-indexed）。部分一致（`openrouter:...` 等）や別名前空間を巻き込まない。
        //   - `model_pricing` / `model_experience_notes` / `llm_provider_overrides`: provider 列が
        //     ちょうど `openai` の行だけ `hermit` へ。`hermit` は本改修で初めて導入する名前なので
        //     既存行と衝突しない（PK 競合なし）。
        //
        // ## 触らないもの
        //   - `llm_logs` / `llm_usage_metrics` の過去行は履歴。遡って書き換えると「当時 openai
        //     だった」記録を失うため保存。新規行は resolve 後の実効名（hermit）が自然に入る。
        //   - voice など別名前空間の "openai"（`[voice] provider="openai"` 等）は LLM の provider
        //     とは無関係。ここは llm 系テーブルの provider 列と agents.model だけを対象にする。
        //
        // ## 前提テーブルの存在
        //   - `llm_provider_overrides` は番号付き migration（PROVIDER_SETTINGS_SQL）で既存 DB にも
        //     作成済み。
        //   - `model_pricing` / `model_experience_notes` は SCHEMA_SQL 由来。現行バイナリが起動時に
        //     `upsert_model_pricing`（hot_reload / context_budget）等でこれらを触るため、本番 DB に
        //     存在することは経験的に保証される（無ければ現行バイナリが既に起動不能）。**デプロイ前に
        //     本番コピーで migrate を実測すること**（CI は毎回新規 DB で既存データ経路を踏まない）。
        //
        // ## 冪等性
        // 番号付き migration は `user_version` で 1 回だけ走る。SQL 自体も、2 度目は WHERE が
        // `openai` 行を 1 つも拾わないため自然に no-op。
        //
        // ## 切り戻し（古いバイナリへ戻すとき）
        //   BEGIN;
        //   UPDATE agents SET model = 'openai:' || substr(model, 8) WHERE model LIKE 'hermit:%';
        //   UPDATE model_pricing SET provider = 'openai' WHERE provider = 'hermit';
        //   UPDATE model_experience_notes SET provider = 'openai' WHERE provider = 'hermit';
        //   UPDATE llm_provider_overrides SET provider = 'openai' WHERE provider = 'hermit';
        //   PRAGMA user_version = 40;
        //   COMMIT;
        // （config も openai 名へ戻すこと。戻さないと次回起動で hermit 参照が未定義になる。）
        up: |conn| {
            conn.execute_batch(
                "UPDATE agents SET model = 'hermit:' || substr(model, 8) \
                     WHERE model LIKE 'openai:%'; \
                 UPDATE model_pricing SET provider = 'hermit' WHERE provider = 'openai'; \
                 UPDATE model_experience_notes SET provider = 'hermit' WHERE provider = 'openai'; \
                 UPDATE llm_provider_overrides SET provider = 'hermit' WHERE provider = 'openai';",
            )
        },
    },
    Migration {
        version: 42,
        description:
            "model_pricing に max_output_tokens 列を足し、在庫の in-use モデルへ実能力値をバックフィルする（#676）",
        // **#676: 出力トークン上限をモデル毎に持たせる。**
        //
        // エンジンは使用モデルの `max_output_tokens` を各リクエストの max_tokens に使い、
        // 未登録（NULL / 0 以下）なら使用時に fail loud で止める（グローバルな任意定数を
        // 既定に置かない方針）。deploy 直後に在庫の in-use モデルが全ターン fail loud に
        // なる（＝ハード切替）のを避けるため、公式値を確認できたモデルを同じ migration で
        // バックフィルする。
        //
        // ## 冪等性
        //   - 列追加は `column_exists` でガード（新規 DB は SCHEMA_SQL 側で既に持つので no-op、
        //     既存 DB でのみ ALTER が走る）。
        //   - バックフィルは `WHERE model = ? AND max_output_tokens IS NULL` で、既に値が
        //     入っている行や対象モデルが無い DB では自然に no-op。値の上書きはしない。
        //
        // ## バックフィル値と出典（公式で確認できたモデルのみ）
        //   - claude-opus-5 = 128000 … Anthropic 公式 models overview の Max output（128k）。
        //     provider は問わず model 名で一致させる（在庫は hermit:claude-opus-5、rename 済み）。
        //   ※ gpt-5.6 系は OpenAI 一次ドキュメントが per-tier の max output を公表しておらず、
        //     推測で backfill しない方針のため本 migration では触らない（NULL のまま。使用時に
        //     fail loud。値が確定したら別 migration か登録フォームから入れる）。
        //
        // ## 切り戻し（古いバイナリへ戻すとき）
        //   列を消す必要は無い（古いバイナリは列を読まない）。版番号だけ戻せばよい:
        //     BEGIN; PRAGMA user_version = 41; COMMIT;
        up: |conn| {
            if !column_exists(conn, "model_pricing", "max_output_tokens")? {
                conn.execute_batch(
                    "ALTER TABLE model_pricing ADD COLUMN max_output_tokens INTEGER",
                )?;
            }
            // 公式で確認できたモデルのみバックフィル。model 名で一致（在庫は 1 行 1 モデル名）。
            conn.execute(
                "UPDATE model_pricing SET max_output_tokens = 128000 \
                 WHERE model = 'claude-opus-5' AND max_output_tokens IS NULL",
                [],
            )?;
            Ok(())
        },
    },
];
