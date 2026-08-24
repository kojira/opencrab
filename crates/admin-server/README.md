# opencrab-admin-server

ダッシュボード（管理面）の API + React SPA 配信。

会話ゲート（`crates/web-gate`）とは**別プロセス・別クレート**。会話ゲートに管理 API を混ぜない。
store は owner ID 日次書き込みのため **RW・no-recover** で開く（`Store::open` は使わない）。
旧テーブル観測は読み取り専用。

## データの向き先（AGREED §2.11）

- oc2 が概念を置き換えたもの（agent→subject、session→place、会話ログ→events/turn_records、
  memory→memories）は **oc2 store の新テーブル**を読み、旧ダッシュボードの JSON 形へ写す（フロント無改変）。
- それ以外（schedules / trusted-users / allowed-commands / model-pricing 等）は**本体 DB スキーマ
  （正本）の旧テーブル**を `opencrab-db` の queries で読む。旧 `crates/server` のハンドラを移植したもの。
- `llm_logs` / `tool_logs`（#772 A）は store の読み取り（`list_llm_logs` / `llm_logs_stats` /
  `list_tool_logs`）。writer は `agent_id` に **subject の十進文字列**を書くので、path `{id}` を
  そのままキーにする（旧 `agents` 表への name 結合はしない）。レスポンスは本体封筒
  （list/stats 配列。`prompt_tokens` / `completion_tokens` / `created_at`）。表が無いときだけ 501。
  `GET /api/agents/{id}/tool-logs` は JSON のみ（フロントページは未着手）。
- 正本スキーマへ**未移行**のテーブル・列は、偽の空配列を返さず **501** で明示する（migration 側の責務）。
  未復元 subroute（skills unused / memory index GET status・tree / analytics 等）も 501。
- Discord / Nostr owner ID（`GET/PUT/PATCH/DELETE /api/agents/{id}/discord`、`GET/PUT/DELETE /api/agents/{id}/nostr`）は本体 wire を復元する（DESIGN-OWNER-IDENTITY）。
- Agents CRUD（`POST /api/agents`、`PUT/PATCH/DELETE /api/agents/{id}`）は slice-1 Subject* コマンド。PATCH の JSON null は未提供。未復元欄は値つき明示だけ 501。
- `POST /api/agents/{id}/messages` は `AgentDirectMessage`（place 保証 + said）のあと Spoke を待って本体封筒を返す。不在 agent は 404。
- `soul_presets`（B 表）の list/create/delete/apply。Apply は SubjectPatch で persona 合成を書く。
- Skills（台帳 `skills`）の GET/POST/PUT/toggle/archive/restore/seed-standard。unused は 501。
- curated DELETE は `forget`。memory index / daily-log-index の WRITE は B 表（#770 と同じ subject→旧 agent_id UUID。未解決・表不在は 501）。GET status/tree は 501。
- `POST /api/sessions` は `PlaceCreateLegacy`（`{theme,mode?,participant_ids,max_turns?}` → `{id}`。未解決 participant は 400）。`POST /api/sessions/{id}/mentor` は `PrivateJournalAppendMentor`（`{content}` → `{id}`。events に載せない）。`POST /api/sessions/{id}/messages` は 501。値つき未復元キーは 501。

## ビルドと起動

SPA を先にビルドしてから起動する（`web/dist` を配信する）。

```sh
# 1) SPA をビルド（web/dist を生成。dist はコミットしない）
cd web
npm ci && npm run build     # あるいは pnpm install && pnpm build
cd ..

# 2) admin-server を起動
#    引数:   admin-server <db_path> [http_port] [web_dist_dir] [compaction_ratio]
#    既定:   db=data/opencrab.db  port=8787  web_dist=web/dist  compaction_ratio=0.5
#    環境変数でも指定可: OPENCRAB_ADMIN_DB / _PORT / _WEB_DIST / _COMPACTION_RATIO
cargo run -p opencrab-admin-server -- data/opencrab.db 8787 web/dist
```

**稼働中 core と同じ DB を開くときも安全**: `Store::open` は使わず、
`Store::open_read_write_no_recover` で schema 初期化も runtime 回収もしない
（稼働中 core の epoch を閉じる副作用を避ける）。旧テーブルは `SQLITE_OPEN_READ_ONLY`。
