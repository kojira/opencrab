# opencrab API Reference

Base URL: `http://localhost:3000`

## Quick Reference

| Method | Endpoint | Purpose |
|--------|----------|---------|
| GET | `/health` | Health check → `"ok"` (plain text) |
| GET | `/api/health` | Health check → `{"status":"ok"}` (JSON) |
| **External Intake** | | |
| POST | `/api/hooks/{source}` | Receive external event (HMAC-verified) → 202 |
| **Agents** | | |
| POST | `/api/agents` | Create agent |
| GET | `/api/agents` | List agents |
| GET | `/api/agents/{id}` | Get agent (identity + soul) |
| DELETE | `/api/agents/{id}` | Delete agent |
| **Soul & Identity** | | |
| GET | `/api/agents/{id}/soul` | Get soul |
| PUT | `/api/agents/{id}/soul` | Update soul |
| GET | `/api/agents/{id}/identity` | Get identity |
| PUT | `/api/agents/{id}/identity` | Update identity |
| **Soul Presets** | | |
| GET | `/api/agents/{id}/soul/presets` | List presets |
| POST | `/api/agents/{id}/soul/presets` | Save current soul as preset |
| DELETE | `/api/agents/{id}/soul/presets/{preset_id}` | Delete preset |
| POST | `/api/agents/{id}/soul/presets/{preset_id}/apply` | Apply preset to soul |
| **Skills** | | |
| GET | `/api/agents/{id}/skills` | List skills (active only) |
| POST | `/api/agents/{id}/skills` | Add skill |
| PUT | `/api/agents/{id}/skills/{skill_id}` | Update skill (partial) |
| POST | `/api/agents/{id}/skills/{skill_id}/toggle` | Toggle skill on/off |
| POST | `/api/agents/{id}/skills/{skill_id}/archive` | Archive skill |
| POST | `/api/agents/{id}/skills/{skill_id}/restore` | Restore archived skill |
| GET | `/api/agents/{id}/skills/unused` | List skills unused for 7+ days |
| **Memory** | | |
| GET | `/api/agents/{id}/memory/curated` | List curated memories |
| POST | `/api/agents/{id}/memory/search` | Search memory (FTS5) |
| GET | `/api/agents/{id}/memory/index` | Get index status |
| POST | `/api/agents/{id}/memory/index` | Trigger incremental index build |
| DELETE | `/api/agents/{id}/memory/index` | Delete entire memory index |
| GET | `/api/agents/{id}/memory/index/tree` | Get index tree structure |
| PUT | `/api/agents/{id}/memory/index/config` | Update index config |
| POST | `/api/agents/{id}/memory/index/rebuild` | Full index rebuild |
| POST | `/api/agents/{id}/memory/index/merge` | Merge topics |
| **Sessions** | | |
| POST | `/api/sessions` | Create session |
| GET | `/api/sessions` | List sessions |
| GET | `/api/sessions/{id}` | Get session detail |
| POST | `/api/sessions/{id}/messages` | Send message (triggers agent response) |
| GET | `/api/sessions/{id}/logs` | Get session logs |
| POST | `/api/sessions/{id}/mentor` | Insert mentor instruction |
| **Agent Schedules (#455)** | | |
| GET | `/api/agents/{id}/schedules` | List schedules (each with computed `next_fire_at`) |
| POST | `/api/agents/{id}/schedules` | Create schedule (cron/`@every`; validates cron/tz/session; 400 on invalid) |
| PATCH | `/api/schedules/{sid}` | Update schedule (cron/tz change or enable resets anchor; disable preserves phase) |
| DELETE | `/api/schedules/{sid}` | Delete schedule |
| **Web** | | |
| POST | `/api/agents/{id}/web/send` | Send message from web dashboard (inbound) |
| GET | `/api/agents/{id}/web/stream` | Subscribe to agent utterances (SSE) |
| **Analytics** | | |
| GET | `/api/agents/{id}/analytics` | Analytics summary |
| GET | `/api/agents/{id}/analytics/detail` | Analytics by model |
| **Workspace** | | |
| GET | `/api/agents/{id}/workspace` | List workspace files |
| GET | `/api/agents/{id}/workspace/{path}` | Read file |
| PUT | `/api/agents/{id}/workspace/{path}` | Write file |
| **Discord** | | |
| GET | `/api/agents/{id}/discord` | Get Discord config |
| PUT | `/api/agents/{id}/discord` | Save Discord config (full update) |
| PATCH | `/api/agents/{id}/discord` | Partial update Discord config |
| DELETE | `/api/agents/{id}/discord` | Remove Discord config |
| POST | `/api/agents/{id}/discord/start` | Start Discord gateway |
| POST | `/api/agents/{id}/discord/stop` | Stop Discord gateway |
| **Channel Configs** | | |
| GET | `/api/agents/{id}/channel-configs` | List channel configs by guild |
| PUT | `/api/agents/{id}/channel-configs` | Upsert channel config |
| DELETE | `/api/agents/{id}/channel-configs/{channel_id}` | Delete channel config |
| **Co-Agents** | | |
| GET | `/api/agents/{id}/co-agents` | List co-agents |
| POST | `/api/agents/{id}/co-agents` | Register co-agent |
| DELETE | `/api/agents/{id}/co-agents/{co_agent_id}` | Remove co-agent |
| **Trusted Users** | | |
| GET | `/api/agents/{id}/trusted-users` | List trusted users |
| POST | `/api/agents/{id}/trusted-users` | Add trusted user |
| PATCH | `/api/agents/{id}/trusted-users/{user_id}` | Update permission |
| DELETE | `/api/agents/{id}/trusted-users/{user_id}` | Remove trusted user |
| **Allowed Commands** | | |
| GET | `/api/agents/{id}/allowed-commands` | List allowed shell commands |
| POST | `/api/agents/{id}/allowed-commands` | Add allowed command |
| DELETE | `/api/agents/{id}/allowed-commands/{command}` | Remove allowed command |
| **LLM Logs** | | |
| GET | `/api/agents/{id}/llm-logs` | List LLM call logs |
| GET | `/api/agents/{id}/llm-logs/stats` | LLM log statistics (30d) |
| **Import** | | |
| POST | `/api/import/scan` | Scan workspace directory |
| POST | `/api/import/execute` | Execute workspace import |

---

## Health

### GET /health

**目的**: サーバー生存確認

**Response**: `"ok"` (plain text, Content-Type: text/plain)

---

### GET /api/health

**目的**: API サーバー生存確認（JSON 形式）

**Response**

```json
{"status": "ok"}
```

---

## External Event Intake

外部システム（第一号: ナレッジベース omoikane）の出来事を受け取り、エージェントの受信箱
（`agent_inbox`）に積む webhook（issue #454）。受理したイベントは **処理せず積むだけ**で、
専用の消化ループが heartbeat とは独立に処理する。設定は `config/default.toml` の `[intake]`。

### POST /api/hooks/{source}

**目的**: 外部イベントを受信して受信箱へ積む（例: `/api/hooks/omoikane`）。

**認証**: source ごとの共有 secret による HMAC-SHA256（**定数時間**照合）。

- 署名ヘッダ: `X-{Source}-Signature: sha256=<hex(hmac-sha256(secret, raw_body))>`
  （汎用 `X-Hook-Signature` も受理）。
- secret は `[intake.secrets]`（`${ENV}` で注入）から解決。未設定 / 空の source は **404**。

**Body**: `{"type": "<event type>", "data": {...}, "delivered_at": "..."}`

**ルーティング**: `[[intake.routes]]` の `(source, event_type) → agent_id`（完全一致）。該当が
無いイベントは受理（202）はするが受信箱に積まれない。

**dedup**: `data.id` から `"{event_type}:{id}"` を作り、`UNIQUE(source, dedup_key)` +
`INSERT OR IGNORE` で二重投入を防ぐ（webhook 再送 / catch-up との相互重複を弾く）。

**Status**

| Status | 条件 |
|--------|------|
| 202 Accepted | 署名 OK。積んだ / dedup で既存 / ルート無し（いずれも受理） |
| 400 Bad Request | body が JSON でない / `type` が空 |
| 401 Unauthorized | 署名ヘッダ欠落 or 不正（受信箱は汚染しない） |
| 404 Not Found | secret 未設定の source |

**信頼性（catch-up）**: webhook は at-most-once。停止中に落ちたイベントは source 側の一覧 API
を真実として起動時 + 定期（`catch_up_interval_secs`）にポーリングし、未処理分を補充する。

**消化**: 新規イベントを積んだ直後に消化ループを即起こし（`process_interval_secs` を待たない・
issue #499）、`process_interval_secs` ごとのポーリングは取りこぼし・再試行の安全網として残す。
いずれの起動でも**未処理が空なら LLM を呼ばない**。

---

## Agents

### POST /api/agents

**目的**: エージェントを新規作成する

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| name | string | ✅ | エージェントの識別名 |
| persona_name | string | ✅ | AI に見せるペルソナ名 |
| id | UUID | ❌ | 省略時は自動生成 |

**Example Request**

```json
{"name": "kairo", "persona_name": "かいろ"}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| id | UUID | 作成されたエージェントの ID |
| name | string | エージェントの識別名 |

**Example Response**

```json
{"id": "550e8400-e29b-41d4-a716-446655440000", "name": "kairo"}
```

---

### GET /api/agents

**目的**: 全エージェント一覧を取得する

**Response**: AgentSummary[]

| Field | Type | Description |
|-------|------|-------------|
| id | UUID | エージェント ID |
| name | string | 識別名 |
| persona_name | string | ペルソナ名 |
| image_url | string | アバター画像 URL |
| status | string | エージェントの状態 |
| skill_count | number | 登録スキル数 |
| session_count | number | セッション数 |

**Example Response**

```json
[{"id": "550e8400-e29b-41d4-a716-446655440000", "name": "kairo", "persona_name": "かいろ", "image_url": "https://example.com/kairo.png", "status": "active", "skill_count": 5, "session_count": 12}]
```

---

### GET /api/agents/{id}

**目的**: エージェントの identity と soul を取得する

**Response**

| Field | Type | Description |
|-------|------|-------------|
| identity | IdentityRow | 後述の Identity 構造 |
| soul | SoulRow | 後述の Soul 構造 |

**Example Response**

```json
{
  "identity": {
    "agent_id": "550e8400-e29b-41d4-a716-446655440000",
    "name": "kairo",
    "job_title": "AI Assistant",
    "organization": "opencrab",
    "image_url": "https://example.com/kairo.png",
    "metadata_json": "{}"
  },
  "soul": {
    "agent_id": "550e8400-e29b-41d4-a716-446655440000",
    "persona_name": "かいろ",
    "social_style_json": "{\"assertiveness\":0.3,\"responsiveness\":0.8,\"style_name\":\"Amiable\"}",
    "thinking_style_json": "{\"primary\":\"直感的\",\"secondary\":\"論理的\",\"description\":\"\"}",
    "personality": "friendly and curious",
    "instructions": "You are a helpful hermit crab AI agent."
  }
}
```

---

### DELETE /api/agents/{id}

**目的**: エージェントを削除する

**Response**

```json
{"deleted": true}
```

---

## Soul & Identity

### SoulRow 構造

| Field | Type | Description |
|-------|------|-------------|
| agent_id | UUID | エージェント ID |
| persona_name | string | ペルソナ名 |
| social_style_json | JSON string | ソーシャルスタイル（後述） |
| thinking_style_json | JSON string | 思考スタイル（後述） |
| personality | string \| null | 性格の自由記述 |
| instructions | string | システムプロンプトに含める指示 |

**social_style_json の内部構造**

| Field | Type | Description |
|-------|------|-------------|
| assertiveness | number (0.0–1.0) | 主張性 |
| responsiveness | number (0.0–1.0) | 応答性 |
| style_name | string | `"Analytical"` \| `"Driver"` \| `"Amiable"` \| `"Expressive"` |

**thinking_style_json の内部構造**

| Field | Type | Description |
|-------|------|-------------|
| primary | string | 主要思考スタイル（例: `"論理的"`, `"直感的"`） |
| secondary | string | 副次思考スタイル |
| description | string | 自由記述 |

### IdentityRow 構造

| Field | Type | Description |
|-------|------|-------------|
| agent_id | UUID | エージェント ID |
| name | string | 識別名 |
| job_title | string \| null | 役職 |
| organization | string \| null | 所属組織 |
| image_url | string \| null | アバター画像 URL |
| metadata_json | JSON string \| null | 任意のメタデータ |

---

### GET /api/agents/{id}/soul

**目的**: エージェントの Soul を取得する

**Response**: SoulRow

**Example Response**

```json
{
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "persona_name": "かいろ",
  "social_style_json": "{\"assertiveness\":0.3,\"responsiveness\":0.8,\"style_name\":\"Amiable\"}",
  "thinking_style_json": "{\"primary\":\"直感的\",\"secondary\":\"論理的\",\"description\":\"\"}",
  "personality": "friendly and curious",
  "instructions": "You are a helpful hermit crab AI agent."
}
```

---

### PUT /api/agents/{id}/soul

**目的**: エージェントの Soul を更新する

> `agent_id` は URL パスパラメータから自動設定される（ボディに含めても上書きされる）

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| persona_name | string | ✅ | ペルソナ名 |
| social_style_json | JSON string | ✅ | ソーシャルスタイル JSON 文字列 |
| thinking_style_json | JSON string | ✅ | 思考スタイル JSON 文字列 |
| personality | string | ❌ | 性格の自由記述 |
| instructions | string | ✅ | システムプロンプトに含める指示 |

**Example Request**

```json
{
  "persona_name": "かいろ",
  "social_style_json": "{\"assertiveness\":0.3,\"responsiveness\":0.8,\"style_name\":\"Amiable\"}",
  "thinking_style_json": "{\"primary\":\"直感的\",\"secondary\":\"論理的\",\"description\":\"\"}",
  "instructions": "You are a helpful hermit crab AI agent."
}
```

**Response**

```json
{"updated": true}
```

---

### GET /api/agents/{id}/identity

**目的**: エージェントの Identity を取得する

**Response**: IdentityRow

**Example Response**

```json
{"agent_id": "550e8400-e29b-41d4-a716-446655440000", "name": "kairo", "job_title": "AI Assistant", "organization": "opencrab", "image_url": "https://example.com/kairo.png", "metadata_json": "{}"}
```

---

### PUT /api/agents/{id}/identity

**目的**: エージェントの Identity を更新する

> `agent_id` は URL パスパラメータから自動設定される

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| name | string | ✅ | 識別名 |
| job_title | string | ❌ | 役職 |
| organization | string | ❌ | 所属組織 |
| image_url | string | ❌ | アバター画像 URL |
| metadata_json | JSON string | ❌ | 任意のメタデータ |

**Example Request**

```json
{"name": "kairo", "job_title": "AI Assistant"}
```

**Response**

```json
{"updated": true}
```

---

## Soul Presets

### GET /api/agents/{id}/soul/presets

**目的**: Soul プリセット一覧を取得する

**Response**: SoulPresetRow[]

| Field | Type | Description |
|-------|------|-------------|
| id | UUID | プリセット ID |
| agent_id | UUID | エージェント ID |
| preset_name | string | プリセット名 |
| persona_name | string | ペルソナ名 |
| custom_traits_json | JSON string \| null | カスタム特性 |

**Example Response**

```json
[{"id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890", "agent_id": "550e8400-e29b-41d4-a716-446655440000", "preset_name": "formal-mode", "persona_name": "かいろ", "custom_traits_json": null}]
```

---

### POST /api/agents/{id}/soul/presets

**目的**: 現在の Soul をプリセットとして保存する

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| preset_name | string | ✅ | プリセット名 |

**Example Request**

```json
{"preset_name": "formal-mode"}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| ok | bool | 成功なら `true` |
| id | UUID | 作成されたプリセット ID |

**Example Response**

```json
{"ok": true, "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"}
```

---

### DELETE /api/agents/{id}/soul/presets/{preset_id}

**目的**: Soul プリセットを削除する

**Response**

```json
{"deleted": true}
```

---

### POST /api/agents/{id}/soul/presets/{preset_id}/apply

**目的**: プリセットを現在の Soul に適用する

**Response**

```json
{"ok": true}
```

---

## Skills

### SkillRow 構造

| Field | Type | Description |
|-------|------|-------------|
| id | UUID | スキル ID |
| agent_id | UUID | エージェント ID |
| name | string | スキル名 |
| description | string | スキルの説明 |
| situation_pattern | string | このスキルを使う状況パターン |
| guidance | string | スキル実行時の指示内容 |
| source_type | string | スキルの出自（`"manual"` など） |
| source_context | string \| null | 出自コンテキスト |
| file_path | string \| null | スクリプトファイルパス |
| is_active | bool | 有効/無効 |
| permission | string | `"agent"` \| `"owner"` \| `"trusted"` |
| archived | bool | アーカイブ済みか |
| usage_count | number | 使用回数 |
| effectiveness | number \| null | 効果スコア |

---

### GET /api/agents/{id}/skills

**目的**: エージェントのスキル一覧を取得する（デフォルトはアクティブのみ）

**Query Parameters**

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| include_archived | bool | ❌ | `true` でアーカイブ済みも含む (default: `false`) |

**Response**: SkillRow[]

**Example Response**

```json
[{
  "id": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "name": "greeting",
  "description": "Greet users warmly",
  "situation_pattern": "user says hello",
  "guidance": "Respond with a friendly greeting.",
  "source_type": "manual",
  "source_context": null,
  "file_path": null,
  "permission": "\"agent\"",
  "is_active": true,
  "archived": false,
  "usage_count": 5,
  "effectiveness": null
}]
```

---

### POST /api/agents/{id}/skills

**目的**: スキルを追加する

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| name | string | ✅ | スキル名 |
| description | string | ✅ | スキルの説明 |
| situation_pattern | string | ✅ | このスキルを使う状況パターン |
| guidance | string | ✅ | スキル実行時の指示内容 |
| permission | string | ❌ | `"agent"` \| `"owner"` \| `"trusted"` (default: `"agent"`) |

**Example Request**

```json
{
  "name": "greeting",
  "description": "Greet users warmly",
  "situation_pattern": "user says hello",
  "guidance": "Respond with a friendly greeting."
}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| id | UUID | 作成されたスキル ID |

**Example Response**

```json
{"id": "b2c3d4e5-f6a7-8901-bcde-f12345678901"}
```

---

### PUT /api/agents/{id}/skills/{skill_id}

**目的**: スキルを部分更新する（指定したフィールドのみ更新）

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| name | string | ❌ | スキル名 |
| description | string | ❌ | スキルの説明 |
| guidance | string | ❌ | スキル実行時の指示内容 |
| situation_pattern | string | ❌ | このスキルを使う状況パターン |

**Example Request**

```json
{"guidance": "Respond with a warm and friendly greeting. Use the user's name if known."}
```

**Response**

```json
{"updated": true}
```

**Error Response** (skill not found)

```json
{"updated": false, "error": "skill not found"}
```

---

### POST /api/agents/{id}/skills/{skill_id}/toggle

**目的**: スキルの有効/無効を切り替える

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| active | bool | ✅ | `true` で有効化、`false` で無効化 |

**Example Request**

```json
{"active": false}
```

**Response**

```json
{"toggled": true}
```

---

### POST /api/agents/{id}/skills/{skill_id}/archive

**目的**: スキルをアーカイブする（ソフト削除）

アーカイブされたスキルは通常の一覧には表示されないが、`include_archived=true` で取得できる。

**Request Body**: なし

**Response**

```json
{"archived": true}
```

---

### POST /api/agents/{id}/skills/{skill_id}/restore

**目的**: アーカイブされたスキルを復元する

**Request Body**: なし

**Response**

```json
{"restored": true}
```

---

### GET /api/agents/{id}/skills/unused

**目的**: 7日以上使われていないスキルを一覧取得する

**Response**: SkillRow[]（SkillRow 構造は上記と同じ）

**Example Response**

```json
[{
  "id": "c3d4e5f6-a7b8-9012-cdef-012345678902",
  "name": "legacy-task",
  "description": "Old task handler",
  "usage_count": 0,
  "is_active": true,
  "archived": false
}]
```

---

## Memory

### GET /api/agents/{id}/memory/curated

**目的**: キュレーション済みメモリ一覧を取得する

**Query Parameters**

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| limit | number | ❌ | 最大件数 (default: `100`) |
| offset | number | ❌ | オフセット (default: `0`) |

**Response**

| Field | Type | Description |
|-------|------|-------------|
| total | number | 全件数 |
| items | CuratedMemoryRow[] | メモリ一覧 |

**Example Response**

```json
{
  "total": 42,
  "items": [{
    "id": "c3d4e5f6-a7b8-9012-cdef-012345678902",
    "agent_id": "550e8400-e29b-41d4-a716-446655440000",
    "content": "User prefers concise answers.",
    "category": "preference",
    "created_at": "2026-03-20T10:00:00Z"
  }]
}
```

---

### POST /api/agents/{id}/memory/search

**目的**: メモリを全文検索する（FTS5）

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| query | string | ✅ | 検索クエリ |
| limit | number | ❌ | 最大件数 (default: `10`) |

**Example Request**

```json
{"query": "user preferences", "limit": 5}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| query | string | 実行したクエリ |
| count | number | ヒット件数 |
| results | SessionLogRow[] | 検索結果 |

**Example Response**

```json
{
  "query": "user preferences",
  "count": 2,
  "results": [{"id": 1, "content": "User prefers concise answers.", "log_type": "speech"}]
}
```

---

### GET /api/agents/{id}/memory/index

**目的**: メモリインデックスの状態を取得する

**Response**

| Field | Type | Description |
|-------|------|-------------|
| agent_id | UUID | エージェント ID |
| total_nodes | number | 総ノード数 |
| unindexed_logs | number | 未インデックスのログ数 |
| watermark | ISO8601 | 最終インデックス時刻 |
| node_type_counts | object | `{root, period, session, topic}` 各 number |
| config | object | `{batch_size, threshold, batch_size_min, threshold_min}` |

**Example Response**

```json
{
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "total_nodes": 42,
  "unindexed_logs": 5,
  "watermark": "2026-03-24T08:00:00Z",
  "node_type_counts": {"root": 1, "period": 3, "session": 12, "topic": 26},
  "config": {"batch_size": 50, "threshold": 70, "batch_size_min": 10, "threshold_min": 30}
}
```

---

### POST /api/agents/{id}/memory/index

**目的**: メモリインデックスを手動で増分構築する（未インデックスのログを処理）

**Request Body**: なし

**Response**

| Field | Type | Description |
|-------|------|-------------|
| ok | bool | 成功なら `true` |
| nodes_created | number | 新規作成ノード数 |
| logs_indexed | number | インデックスされたログ数 |

**Example Response**

```json
{"ok": true, "nodes_created": 3, "logs_indexed": 5}
```

---

### DELETE /api/agents/{id}/memory/index

**目的**: メモリインデックスを全削除する（再構築前のリセット等に使用）

**Request Body**: なし

**Response**

```json
{"ok": true, "message": "Index deleted"}
```

**Error Response**

```json
{"ok": false, "error": "..."}
```

---

### GET /api/agents/{id}/memory/index/tree

**目的**: メモリインデックスのツリー構造を取得する（ルートから階層的に表示）

**Response**

| Field | Type | Description |
|-------|------|-------------|
| nodes | IndexNodeRow[] | 全ノードのフラットリスト |
| tree | TreeNode[] | ルートから階層的なツリー |

**IndexNodeRow 構造**

| Field | Type | Description |
|-------|------|-------------|
| id | string | ノード ID |
| title | string | ノードタイトル |
| node_type | string | `"root"` \| `"period"` \| `"session"` \| `"topic"` |
| summary | string | ノードのサマリー |
| depth | number | 木の深さ（root=0） |
| child_count | number | 子ノード数 |
| parent_id | string \| null | 親ノード ID |

**Example Response**

```json
{
  "nodes": [
    {"id": "root-1", "title": "Memory Root", "node_type": "root", "summary": "...", "depth": 0, "child_count": 2, "parent_id": null}
  ],
  "tree": [{
    "id": "root-1",
    "title": "Memory Root",
    "node_type": "root",
    "summary": "Overview of all memories",
    "depth": 0,
    "child_count": 2,
    "children": [
      {"id": "period-1", "title": "March 2026", "node_type": "period", "depth": 1, "children": []}
    ]
  }]
}
```

---

### PUT /api/agents/{id}/memory/index/config

**目的**: メモリインデックスの構築設定を更新する

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| batch_size | number | ❌ | 一度に処理するログ数（省略時は現在値を維持） |
| threshold | number | ❌ | インデックス構築の閾値（省略時は現在値を維持） |

**Example Request**

```json
{"batch_size": 100, "threshold": 80}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| ok | bool | 成功なら `true` |
| config | object | 更新後の設定 |

**Example Response**

```json
{
  "ok": true,
  "config": {
    "agent_id": "550e8400-e29b-41d4-a716-446655440000",
    "batch_size": 100,
    "threshold": 80,
    "updated_at": "2026-03-25T10:00:00Z"
  }
}
```

---

### POST /api/agents/{id}/memory/index/rebuild

**目的**: メモリインデックスを完全再構築する（既存インデックスを削除して全ログから再作成）

> ⚠️ 時間がかかる場合があります。LLM 呼び出しが発生します。

**Request Body**: なし

**Response**

| Field | Type | Description |
|-------|------|-------------|
| ok | bool | 成功なら `true` |
| nodes_created | number | 作成されたノード数 |
| logs_indexed | number | インデックスされたログ数 |

**Example Response**

```json
{"ok": true, "nodes_created": 45, "logs_indexed": 200}
```

**Error Response**

```json
{"ok": false, "error": "..."}
```

---

### POST /api/agents/{id}/memory/index/merge

**目的**: 同一期間内のトピックを再マージして整理する（デフォルト: period あたり最大 10 topics）

> ⚠️ LLM 呼び出しが発生します。

**Request Body**: なし

**Response**

| Field | Type | Description |
|-------|------|-------------|
| ok | bool | 成功なら `true` |
| periods_processed | number | 処理された期間数 |
| topics_merged | number | マージされたトピック数 |
| topics_deleted | number | 削除されたトピック数 |

**Example Response**

```json
{"ok": true, "periods_processed": 3, "topics_merged": 8, "topics_deleted": 5}
```

---

## Sessions

### POST /api/sessions

**目的**: セッションを新規作成する

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| theme | string | ✅ | セッションのテーマ |
| mode | string | ❌ | `"autonomous"` \| `"facilitated"` (default: `"autonomous"`) |
| participant_ids | UUID[] | ✅ | 参加エージェントの ID 配列 |
| max_turns | number | ❌ | 最大ターン数（省略時は無制限） |

**Example Request**

```json
{
  "theme": "brainstorming",
  "participant_ids": ["550e8400-e29b-41d4-a716-446655440000"]
}
```

**Response**

```json
{"id": "d4e5f6a7-b8c9-0123-def0-123456789013"}
```

---

### GET /api/sessions

**目的**: 全セッション一覧を取得する

**Response**: SessionRow[]

| Field | Type | Description |
|-------|------|-------------|
| id | UUID | セッション ID |
| mode | string | `"autonomous"` \| `"facilitated"` |
| theme | string | テーマ |
| phase | string | `"divergent"` \| `"convergent"` \| `"done"` |
| turn_number | number | 現在のターン番号 |
| status | string | `"active"` \| `"done"` \| `"completed"` |
| participant_ids_json | JSON string | 参加者 ID の JSON 配列文字列 |
| facilitator_id | UUID \| null | ファシリテーター ID |
| done_count | number | 完了投票数 |
| max_turns | number \| null | 最大ターン数 |
| metadata_json | JSON string \| null | メタデータ |

**Example Response**

```json
[{
  "id": "d4e5f6a7-b8c9-0123-def0-123456789013",
  "mode": "autonomous",
  "theme": "brainstorming",
  "phase": "divergent",
  "turn_number": 5,
  "status": "active",
  "participant_ids_json": "[\"550e8400-e29b-41d4-a716-446655440000\"]",
  "facilitator_id": null,
  "done_count": 0,
  "max_turns": 20,
  "metadata_json": null
}]
```

---

### GET /api/sessions/{id}

**目的**: セッション詳細を取得する

**Response**: SessionRow（上記と同構造）または `null`

---

### POST /api/sessions/{id}/messages

**目的**: セッションにメッセージを送信し、他の参加エージェントの応答を得る

> メッセージ送信者（`agent_id`）以外の全参加者が自動的に応答する。LLM 呼び出しが発生する。

**存在しない参加者は 404**（#632）: 参加者に `agents` 行の無い `agent_id` が含まれていると、その参加者のターンは走らず **`404 Not Found`**（`{"error": "agent not found: {id}"}`）を返す。セッションは `create_session` 時に参加者の存在を確認しない（`agent_sessions` に FK が無い）ため、でたらめな参加者 ID でセッションを作れてしまうが、実行はサーバ側チョークポイント（`process::run_agent_response`）で弾かれる。

> **ツール実行は inline（同期）**: この経路は非ブロック dispatch を配線しない。エージェントが呼んだツールはすべて応答ターンの中で実行され、その結果を踏まえた最終応答が `responses[].content` に入る（`tool_calls_made` も実際の実行回数）。

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| agent_id | UUID | ✅ | 送信元エージェント ID |
| content | string | ✅ | メッセージ本文 |

**Example Request**

```json
{
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "content": "What do you think about this idea?"
}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| id | number | メッセージ DB ID |
| session_id | UUID | セッション ID |
| responses | object[] | 各エージェントの応答 |

**responses 要素**

| Field | Type | Description |
|-------|------|-------------|
| agent_id | UUID | 応答エージェント ID |
| agent_name | string | エージェント名 |
| content | string | 応答内容 |
| tool_calls_made | number | ツール呼び出し回数 |

**Example Response**

```json
{
  "id": 42,
  "session_id": "d4e5f6a7-b8c9-0123-def0-123456789013",
  "responses": [{
    "agent_id": "660f9500-f3a0-52e5-b827-557766551111",
    "agent_name": "assistant",
    "content": "That sounds interesting!",
    "tool_calls_made": 0
  }]
}
```

---

### GET /api/sessions/{id}/logs

**目的**: セッションのログを取得する

**Response**: SessionLogRow[]

| Field | Type | Description |
|-------|------|-------------|
| id | number \| null | ログ DB ID |
| agent_id | UUID | エージェント ID |
| session_id | UUID | セッション ID |
| log_type | string | `"speech"` \| `"system"` \| `"action"` |
| content | string | ログ内容 |
| speaker_id | UUID \| null | 発話者 ID |
| turn_number | number \| null | ターン番号 |
| metadata_json | JSON string \| null | メタデータ |
| created_at | ISO8601 \| null | 作成日時 |

**Example Response**

```json
[{
  "id": 1,
  "session_id": "d4e5f6a7-b8c9-0123-def0-123456789013",
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "log_type": "speech",
  "content": "Hello! How can I help?",
  "speaker_id": "550e8400-e29b-41d4-a716-446655440000",
  "turn_number": 1,
  "metadata_json": null,
  "created_at": "2026-03-20T10:00:00Z"
}]
```

---

### POST /api/sessions/{id}/mentor

**目的**: セッションにメンター指示を挿入する（`log_type: "system"` として記録）

エージェントの応答は生成されない。次回ターンでエージェントがこの指示を参照する。

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| content | string | ✅ | メンター指示の内容 |

**Example Request**

```json
{"content": "Please be more concise in your responses from now on."}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| id | number | 作成されたログ DB ID |

**Example Response**

```json
{"id": 99}
```

---

## Agent Schedules (#455)

per-agent の定時実行（cron / `@every`）。中央スケジューラ（#439）の同一時刻源に載る。
発火時は `message` を対象セッションへ self-message として注入し、通常メッセージ処理経路
（caller=Owner）で 1 ターン走らせる。詳細は `docs/design-agent-schedules.md`。

**次回発火時刻 `next_fire_at` は列に持たず照会時に算出**（heartbeat と同じ方針・stale フリー）。
既定は無効（`enabled=false`・fail-closed）。**`heartbeat_enabled`（G）は schedule に掛からない**
（G を切っても定時実行は止まらない。止めるには `enabled=false`）。

### GET /api/agents/{id}/schedules

```json
{
  "agent_id": "…",
  "schedules": [
    {
      "id": 1, "agent_id": "…", "session_id": "nostr-…",
      "cron_expr": "0 7 * * *", "timezone": "Asia/Tokyo",
      "message": "毎朝のまとめを書いてください", "enabled": true,
      "anchor_at": "2026-08-09T00:00:00+09:00",
      "last_fired_at": null,
      "next_fire_at": "2026-08-09T22:00:00+00:00"
    }
  ],
  "count": 1
}
```

### POST /api/agents/{id}/schedules

Request:

```json
{
  "session_id": "nostr-…",            // そのエージェントの発火経路を持つセッションに限る
  "cron_expr": "0 7 * * *",           // 標準 5 フィールド cron、または "@every 3h"
  "timezone": "Asia/Tokyo",           // 省略時 Asia/Tokyo
  "message": "毎朝のまとめを書いてください",
  "enabled": true                      // 省略時 false（fail-closed）
}
```

- 不正な cron/`@every`/timezone は **400**。
- `session_id` がそのエージェントの `nostr-`/`discord-` セッションでなければ **400**。
- `enabled=true` で作ると `anchor_at=now`（初回発火は「now 以降の最初のスロット / now+周期」）。
- 応答は作成された行（`next_fire_at` は照会時算出）。

### PATCH /api/schedules/{sid}

部分更新（送ったフィールドだけ変更）。存在しない `sid` は **404**。

- **cron 式 / timezone の明示変更**、または **無効→有効化**では `anchor_at=now`・`last_fired_at=NULL`
  にリセットする（新しい式で次スロットから）。
- **有効→無効化**では anchor/last_fired を触らない（意図した疎らさを壊さない）。
- 変更後はスケジューラを起こして即時反映（#437・再起動不要）。

### DELETE /api/schedules/{sid}

削除。存在しない `sid` は **404**。

---

## Web

ダッシュボード（web UI）からエージェントと会話するためのゲートウェイ。送信（inbound）と購読（SSE）の 2 本で構成される。

セッション ID は `web-{agent_id}-{conversation_id}` の形式で自動生成・再利用される（`conversation_id` が会話スレッドの単位）。同一セッションの inbound と subtask 完了 resume は 1 本のロックで直列化される（割り込みによる二重回答の防止）。別セッションは並行して処理される。

### POST /api/agents/{id}/web/send

**目的**: web UI からのメッセージを送信し、直接応答を得る（応答は同時に SSE へも配送される）

**存在しないエージェントは 404**（#632）: `agents` テーブルに `{id}` の行が無ければ、**ターンを起こさず** **`404 Not Found`**（`{"error": "agent not found: {id}"}`）を返す。存在確認は web の唯一の公開ターン入口 `run_and_deliver_serialized` が担い、エージェント行の有無だけを判定する。（弾かれる前にセッション行やユーザー発話行は書かれることはあるが、ターンは走らない＝ LLM は呼ばれない。存在確認そのものが DB エラーで失敗した場合は 404 ではなく `500` を返す。）

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| conversation_id | string | ✅ | 会話 ID。セッション ID `web-{agent_id}-{conversation_id}` の構成要素 |
| content | string | ✅ | メッセージ本文 |
| user_id | string | ❌ | 送信者の ID（権限判定・`speaker_id` に使う）。省略時／空文字・空白のみのときは `"web-user"` |

`user_id` は前後の空白を除去してから使われ、権限判定・`speaker_id` で同じ正規化済みの値が使われる。

**Caller 権限の決定ロジック**: `trusted_users` は **`platform="web"` の行だけ**を見る。

- `user_id` が `trusted_users` テーブルに `co-agent` 権限で登録されている → `CoAgent`
- `user_id` が `trusted_users` テーブルに登録されている → `TrustedUser`
- `user_id` がエージェントの Discord オーナー ID と一致する → `Owner`（比較は前後の空白を無視する。オーナー ID が空文字/空白のみ＝未設定なら誰とも一致しない）
- それ以外 → `Agent`

> HTTP レベルの認証は無く（CORS は permissive）、権限はリクエストボディの `user_id` から導出される。既定値の `"web-user"` は `trusted_users` に `platform="web"` で登録しなければ `Agent` 権限にとどまる。ローカル／信頼済みネットワーク前提の想定である点は他のエンドポイントと同じ。

セッションが存在しない場合は自動作成される（`theme` = `"web_conversation"`、`mode` = `"autonomous"`、`status` = `"active"`）。ユーザー発話は `session_logs` に記録され、応答生成時の会話履歴は毎回 DB から再構築される。

**Example Request**

```json
{
  "conversation_id": "conv-1",
  "content": "今日のタスクを整理して",
  "user_id": "123456789012345678"
}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| session_id | string | セッション ID（`web-{agent_id}-{conversation_id}`） |
| caller_type | string | `"owner"` \| `"trusted_user"` \| `"co_agent"` \| `"agent"` |
| response | string \| null | 直接応答の本文。`NO_REPLY`／空応答／エラー時は `null` |

> **失敗時のレスポンス形は不揃いである（HTTP ステータスはいずれも 200）**: 上の 3 フィールドが必ず返るのは成功時と「LLM プロバイダ未設定」の経路だけで、セッション作成／発話記録の失敗の経路は **`error` だけ**を返し `session_id` / `caller_type` / `response` を含まない。ダッシュボードはエラー時にこれらのフィールドを読めない前提で書く必要がある（下の Error Response の例を参照）。この不揃い（および失敗時も 200 を返すこと）の是正は [#200](https://github.com/kojira/opencrab/issues/200) で扱い、本ドキュメントは現状の実装を記述している。

**Example Response**

```json
{
  "session_id": "web-550e8400-e29b-41d4-a716-446655440000-conv-1",
  "caller_type": "owner",
  "response": "了解。まず優先度順に並べ替えるね。"
}
```

**ツール実行は非ブロック（background subtask）**

この経路は非ブロック dispatch を配線しているため、**ツールの実行結果は `response` に含まれない**。inline に留めるツールと dispatch 対象の正確な分類は `crates/actions/src/bridge.rs` の定数が権威で、MCP ツールは既定で inline となる。subtask が決着すると per-session ロックの下でエージェントを resume し、生成された応答を **SSE の `subtask_resume` イベントとして配送する**（HTTP 応答はすでに返っているため body には現れない）。

**Error Response** (no LLM provider)

```json
{
  "session_id": "web-...-conv-1",
  "caller_type": "agent",
  "response": null,
  "error": "No LLM providers available"
}
```

**Error Response** (セッション作成・発話記録の失敗)

```json
{"error": "Failed to create session: ..."}
```

```json
{"error": "Failed to log message: ..."}
```

---

### GET /api/agents/{id}/web/stream

**目的**: エージェント発話を SSE（`text/event-stream`）で購読する

**Query Parameters**

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| conversation | string | ✅ | 会話 ID。`web-{agent_id}-{conversation}` のセッションを購読する |

購読自体に権限チェックは無い（`agent_id` と `conversation` を知っていれば購読できる）。keep-alive コメントが定期送出される。

**Event 形式**

SSE の `event:` 名は設定されない（既定の `message`）。各イベントの `data` は以下の JSON オブジェクトで、**種別は `kind` フィールドで判別する**。

| Field | Type | Description |
|-------|------|-------------|
| kind | string | `"direct"` \| `"subtask_resume"` \| `"error"` |
| agent_id | string | 発話したエージェントの ID |
| content | string | 発話本文（`error` のときはエラーメッセージ） |

| kind | 意味 |
|------|------|
| `direct` | `POST .../web/send` への直接応答。同じ本文が HTTP レスポンスの `response` にも入る（二重に見える点に注意） |
| `subtask_resume` | dispatch した background subtask の完了を受けて resume したターンの応答。HTTP レスポンスには現れず、この経路のみで届く |
| `error` | 応答生成が失敗した。`content` は `"(error: ...)"` 形式 |

**Example Event**

```
data: {"kind":"subtask_resume","agent_id":"550e8400-e29b-41d4-a716-446655440000","content":"調査が終わったよ。結果は…"}
```

**配送の性質**

- `direct` / `subtask_resume` の本文は `session_logs` にも保存される。stream を開く前に発生した発話や、取りこぼした発話は `GET /api/sessions/{id}/logs` で辿れる（`error` イベントは publish のみで DB に残らない）。
- publish は best-effort。購読者がいないセッションのイベントは破棄される。
- 未読バックログの上限は 256 件で、超過した購読者は溢れた分をスキップして受信を継続する（欠けた分は上記のログ経由で辿る）。
- `NO_REPLY` および空応答は配送されない。

---

## Analytics

### GET /api/agents/{id}/analytics

**目的**: エージェントの利用統計サマリーを取得する

**Query Parameters**

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| period | string | ❌ | `"day"` \| `"week"` \| `"month"` (default: `"week"`) |

**Response**

| Field | Type | Description |
|-------|------|-------------|
| count | number | リクエスト数 |
| total_tokens | number | 合計トークン数 |
| total_cost | number | 合計コスト (USD) |
| avg_latency | number | 平均レイテンシ (ms) |
| avg_quality | number | 平均品質スコア |

**Example Response**

```json
{"count": 150, "total_tokens": 45000, "total_cost": 1.25, "avg_latency": 320.5, "avg_quality": 0.87}
```

---

### GET /api/agents/{id}/analytics/detail

**目的**: モデル別の詳細統計を取得する

**Query Parameters**

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| period | string | ❌ | `"day"` \| `"week"` \| `"month"` (default: `"week"`) |

**Response**: AnalyticsDetailRow[]

| Field | Type | Description |
|-------|------|-------------|
| provider | string | プロバイダ名 |
| model | string | モデル名 |
| total_tokens | number | 合計トークン数 |
| total_cost | number | 合計コスト (USD) |
| request_count | number | リクエスト数 |
| avg_latency | number | 平均レイテンシ (ms) |

**Example Response**

```json
[{"provider": "anthropic", "model": "claude-sonnet-4-6", "total_tokens": 30000, "total_cost": 0.90, "request_count": 100, "avg_latency": 310.2}]
```

---

## Workspace

### GET /api/agents/{id}/workspace

**目的**: エージェントのワークスペース内ファイル一覧を取得する

**Query Parameters**

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| path | string | ❌ | ディレクトリパス (default: ルート) |

**Response**

| Field | Type | Description |
|-------|------|-------------|
| entries | object[] | `[{name: string, is_dir: bool, size: number}]` |

**Example Response**

```json
{"entries": [{"name": "notes.md", "is_dir": false, "size": 1024}, {"name": "drafts", "is_dir": true, "size": 0}]}
```

---

### GET /api/agents/{id}/workspace/{path}

**目的**: ワークスペース内のファイルを読む

**Path Parameter**: `{path}` はファイルのパス（サブディレクトリを含む場合は `/` でつなぐ）

**Response**

| Field | Type | Description |
|-------|------|-------------|
| path | string | ファイルパス |
| content | string | ファイル内容 |

**Example Response**

```json
{"path": "notes.md", "content": "# My Notes\n\nSome content here..."}
```

---

### PUT /api/agents/{id}/workspace/{path}

**目的**: ワークスペース内のファイルを書き込む（存在しない場合は作成）

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| content | string | ✅ | ファイル内容 |

**Example Request**

```json
{"content": "# Updated Notes\n\nNew content here..."}
```

**Response**

```json
{"written": true}
```

---

## Discord

### GET /api/agents/{id}/discord

**目的**: Discord Bot 設定を取得する

**Response**

| Field | Type | Description |
|-------|------|-------------|
| configured | bool | 設定済みか |
| enabled | bool \| undefined | Bot が有効か（未設定時は不在） |
| token_masked | string \| undefined | マスクされたトークン（先頭10文字 + `...`） |
| owner_discord_id | string \| undefined | オーナーの Discord ID |
| running | bool \| undefined | Gateway が起動中か |

**Example Response (configured)**

```json
{"configured": true, "enabled": true, "token_masked": "BOTTOKEN...", "owner_discord_id": "123456789012345678", "running": true}
```

**Example Response (not configured)**

```json
{"configured": false}
```

---

### PUT /api/agents/{id}/discord

**目的**: Discord Bot 設定を保存し、Gateway を起動する（フル更新）

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| bot_token | string | ✅ | Discord Bot トークン |
| owner_discord_id | string | ❌ | オーナーの Discord ユーザー ID。保存時に前後の空白は除去される（空白のみは未設定と同じ） |

**Example Request**

```json
{"bot_token": "BOTTOKENxxxxxxxxxxxxxxxxxxxxxxxx", "owner_discord_id": "123456789012345678"}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| ok | bool | 成功なら `true` |
| message | string | 結果メッセージ |

**Example Response (discord feature enabled)**

```json
{"ok": true, "message": "Discord bot started."}
```

**Example Response (discord feature disabled)**

```json
{"ok": true, "message": "Config saved. Gateway not started (discord feature not active)."}
```

---

### PATCH /api/agents/{id}/discord

**目的**: Discord Bot 設定を部分更新する。設定済みの場合のみ有効。Gateway が起動中なら再起動する。

**Request Body**（全フィールドが省略可能）

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| bot_token | string | ❌ | Discord Bot トークン |
| owner_discord_id | string | ❌ | オーナーの Discord ユーザー ID。保存時に前後の空白は除去される（空白のみは未設定と同じ） |

**Example Request**

```json
{"owner_discord_id": "987654321098765432"}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| ok | bool | 成功なら `true` |
| configured | bool | 設定済みか |
| enabled | bool | Bot が有効か |
| token_masked | string | マスクされたトークン |
| owner_discord_id | string | オーナーの Discord ID |

**Example Response**

```json
{
  "ok": true,
  "configured": true,
  "enabled": true,
  "token_masked": "BOTTOKEN...",
  "owner_discord_id": "987654321098765432"
}
```

**Error Response** (no existing config)

```json
{"ok": false, "error": "No Discord config found. Use PUT to create one."}
```

---

### DELETE /api/agents/{id}/discord

**目的**: Discord Bot 設定を削除する

**Response**

```json
{"deleted": true}
```

---

### POST /api/agents/{id}/discord/start

**目的**: Discord Gateway を起動する（DB の enabled フラグを `true` にセット）

**Request Body**: なし

**Response**

| Field | Type | Description |
|-------|------|-------------|
| ok | bool | 成功なら `true` |
| error | string \| undefined | 失敗時のエラーメッセージ |

**Example Response (success)**

```json
{"ok": true}
```

**Example Response (error)**

```json
{"ok": false, "error": "No Discord config found for this agent."}
```

---

### POST /api/agents/{id}/discord/stop

**目的**: Discord Gateway を停止する（DB の enabled フラグを `false` にセット）

**Request Body**: なし

**Response**

```json
{"ok": true}
```

---

## Channel Configs

Discordチャンネルごとの読み書き権限・ハートビート設定を管理する。

### ChannelConfigDto 構造

| Field | Type | Description |
|-------|------|-------------|
| channel_id | string | Discord チャンネル ID |
| guild_id | string | Discord ギルド（サーバー）ID |
| channel_name | string | チャンネル名 |
| readable | bool | 読み取り可能か |
| writable | bool | 書き込み可能か |
| whitelisted | bool | ホワイトリスト登録済みか |
| heartbeat_enabled | bool | ハートビートを有効にするか |
| heartbeat_interval_secs | number \| null | ハートビート間隔（秒） |

---

### GET /api/agents/{id}/channel-configs

**目的**: ギルド別のチャンネル設定一覧を取得する

**Query Parameters**

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| guild_id | string | ✅ | Discord ギルド ID |

**Response**

| Field | Type | Description |
|-------|------|-------------|
| guild_id | string | ギルド ID |
| configs | ChannelConfigDto[] | チャンネル設定一覧 |
| count | number | 件数 |

**Example Request**

```
GET /api/agents/550e8400-.../channel-configs?guild_id=111222333444555666
```

**Example Response**

```json
{
  "guild_id": "111222333444555666",
  "configs": [{
    "channel_id": "777888999000111222",
    "guild_id": "111222333444555666",
    "channel_name": "general",
    "readable": true,
    "writable": true,
    "whitelisted": false,
    "heartbeat_enabled": true,
    "heartbeat_interval_secs": 3600
  }],
  "count": 1
}
```

---

### PUT /api/agents/{id}/channel-configs

**目的**: チャンネル設定を作成または更新する（Upsert）

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| channel_id | string | ✅ | Discord チャンネル ID |
| guild_id | string | ✅ | Discord ギルド ID |
| channel_name | string | ❌ | チャンネル名 (default: `""`) |
| readable | bool | ❌ | 読み取り可能か (default: `true`) |
| writable | bool | ❌ | 書き込み可能か (default: `true`) |
| whitelisted | bool | ❌ | ホワイトリスト登録 (default: `false`) |
| heartbeat_enabled | bool | ❌ | ハートビート有効 (default: `true`) |
| heartbeat_interval_secs | number | ❌ | ハートビート間隔（秒） |

**Example Request**

```json
{
  "channel_id": "777888999000111222",
  "guild_id": "111222333444555666",
  "channel_name": "general",
  "readable": true,
  "writable": true,
  "heartbeat_enabled": true,
  "heartbeat_interval_secs": 3600
}
```

**Response**

```json
{"channel_id": "777888999000111222", "message": "channel config upserted"}
```

---

### DELETE /api/agents/{id}/channel-configs/{channel_id}

**目的**: チャンネル設定を削除する

**Response**

```json
{"channel_id": "777888999000111222", "message": "channel config deleted"}
```

**Error**: 設定が存在しない場合は HTTP 404

---

## Co-Agents

### GET /api/agents/{id}/co-agents

**目的**: 登録済みの共同エージェント一覧を取得する

**Response**: CoAgentRow[]

| Field | Type | Description |
|-------|------|-------------|
| id | UUID | レコード ID |
| agent_id | UUID | 親エージェント ID |
| co_agent_id | UUID | 共同エージェント ID |
| created_by | string | 作成者 |
| created_at | ISO8601 | 作成日時 |

> **Note (#490)**: `allowed_actions` は権限判定に使われないためレスポンスから外した。co_agent は owner 等価で、登録された相手は全アクションを実行できる。

**Example Response**

```json
[{
  "id": "e5f6a7b8-c9d0-1234-ef01-234567890123",
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "co_agent_id": "660f9500-f3a0-52e5-b827-557766551111",
  "created_by": "admin",
  "created_at": "2026-03-20T10:00:00Z"
}]
```

---

### POST /api/agents/{id}/co-agents

**目的**: 共同エージェントを登録する

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| co_agent_id | UUID | ✅ | 共同エージェントの ID |

> **Note (#490)**: `allowed_actions` は受け付けない。非空で渡すと `400 Bad Request`（co_agent は owner 等価で権限判定に使われないため）。省略 / `null` / 空配列は許容。

**Example Request**

```json
{"co_agent_id": "660f9500-f3a0-52e5-b827-557766551111"}
```

**Response**: CoAgentRow（上記と同構造）

---

### ~~PATCH /api/agents/{id}/co-agents/{co_agent_id}~~（#490 で撤去）

**撤去済み**: 唯一の役割が `allowed_actions` の更新だったが、その列は権限判定に使われず API から外したため、更新できるフィールドが無くなった。何もしないエンドポイントを残すより撤去した（co_agent は owner 等価で、追加/削除は POST/DELETE で足りる）。**この経路への `PATCH` は `405 Method Not Allowed` を返す**（`DELETE` は下記のとおり有効）。

---

### DELETE /api/agents/{id}/co-agents/{co_agent_id}

**目的**: 共同エージェントを解除する

**Response**

```json
{"deleted": true}
```

---

## Trusted Users

> **信頼は経路（`platform`）ごとに分かれている（#214 / #159）。**
> 行は登録された経路でしか効かない。`discord` は Discord のユーザー ID、`web` は
> ダッシュボード（`POST /api/web/agents/{id}/messages` 等）が申告する `user_id` の識別子空間。
> `rest` は撤去済みの direct-message REST が使用していた値で、既存行との互換のため登録 API が引き続き受け付ける。
> 経路が違えば同じ文字列でも別人として扱う（信頼は引き継がれない）。
>
> **移行が必要なケース（#159 で挙動が変わった点）**: #214 より前に登録した行はすべて
> `platform="discord"` である。以前は「自経路の行が無ければ `discord` の行も見る」互換
> 読みがあったため、web の利用者もその行で信頼されていた。この互換読みは #159 で
> **撤去した**ので、`web` の行を持たない利用者は **web で信頼を失い、
> 最小権限（`Agent`）で動く**（拒否側に倒れるだけで、権限が緩むことはない）。該当する
> 呼び出しが来ると、サーバは行の場所と直し方を WARN ログに出す
> （`trusted user row exists only on the legacy 'discord' platform ...`）。
>
> **直し方**: その利用者の旧行を `DELETE /api/agents/{id}/trusted-users/{row_id}` で消し、
> `platform="web"` を指定して登録し直す。一意制約が `(user_id, agent_id)` のままなので、
> **消す前に同じ識別子を別経路で登録すると 409 になる**（制約の作り直しは表の再構築を
> 伴う非可逆な変更なので #159 に残してある）。Discord の利用者は何もしなくてよい。

### GET /api/agents/{id}/trusted-users

**目的**: 信頼済みユーザー一覧を取得する

**Response**: TrustedUserRow[]

| Field | Type | Description |
|-------|------|-------------|
| id | UUID | レコード ID |
| user_id | string | その経路でのユーザー識別子（旧 `discord_user_id`） |
| agent_id | UUID | エージェント ID |
| permission | string | `"owner"` \| `"user"` \| `"co-agent"` — **ケバブケース**（#234） |
| created_by | string | 作成者 |
| created_at | ISO8601 | 作成日時 |
| display_name | string | ロスター表示用の名前（空文字可） |
| platform | string | `"discord"` \| `"web"` \| `"rest"` — その行の識別子空間。`rest` は既存行との互換用で、現行の読取経路はない |

一覧は**経路で絞らない**（運用者が全経路の登録を見渡せる必要があるため）。

**Example Response**

```json
[{
  "id": "f6a7b8c9-d0e1-2345-f012-345678901234",
  "user_id": "123456789012345678",
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "permission": "owner",
  "created_by": "owner",
  "created_at": "2026-03-20T10:00:00Z",
  "display_name": "",
  "platform": "discord"
}]
```

---

### POST /api/agents/{id}/trusted-users

**目的**: 信頼済みユーザーを追加する

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| user_id | string | ✅ | その経路でのユーザー識別子（旧 `discord_user_id` も後方互換で受け付ける） |
| permission | string | ❌ | `"owner"` \| `"user"` \| `"co-agent"` (default: `"user"`)。**これ以外は 400**（#234） |
| display_name | string | ❌ | ロスター表示用の名前（default: 空文字） |
| platform | string | ❌ | `"discord"` \| `"web"` \| `"rest"` (default: `"discord"`) — `user_id` の識別子空間。`rest` は既存行との互換用 |

**Example Request**

```json
{"user_id": "123456789012345678", "permission": "user"}
```

ダッシュボード利用者を登録する例（この行は web 経路でのみ効く）:

```json
{"user_id": "web-user", "permission": "co-agent", "platform": "web"}
```

**Response**: TrustedUserRow（上記と同構造。`platform` を含む）

**Errors**

| Status | 意味 |
|--------|------|
| 400 | `platform` が `discord` / `web` / `rest` 以外、または `permission` が `owner` / `user` / `co-agent` 以外（登録できても誰とも一致しない・効かない行になるため弾く。旧いアンダースコア表記 `co_agent` も弾かれる — #234） |
| 409 | 同じ `(user_id, agent_id)` が既に存在する。一意制約に経路が入っていないため、**同じ識別子を別経路で二重に持つことはまだできない**（先に旧行を削除する） |

---

### PATCH /api/agents/{id}/trusted-users/{user_id}

**目的**: 信頼済みユーザーの権限・表示名を変更する（部分更新）

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| permission | string | ❌ | `"owner"` \| `"user"` \| `"co-agent"`。**これ以外は 400**（#234）。省略時は権限に触らない |
| display_name | string | ❌ | ロスター表示用の名前。省略時は表示名に触らない |

どちらも省略可で、**指定したフィールドだけ**を更新する（両方指定した場合は 1 トランザクションで不可分に適用）。両方省略したリクエストは何も更新せず `{"updated": false}` を返す。

**Example Request**

```json
{"permission": "co-agent"}
```

表示名だけを変更する:

```json
{"display_name": "Crab B"}
```

**Response**

`updated` は実際に行が更新されたか。対象の `user_id` が存在しない場合は `false`。

```json
{"updated": true}
```

---

### DELETE /api/agents/{id}/trusted-users/{user_id}

**目的**: 信頼済みユーザーを削除する

**Response**

```json
{"deleted": true}
```

---

## Allowed Commands

エージェントが使用できるシェルコマンドのホワイトリストを管理する。

### GET /api/agents/{id}/allowed-commands

**目的**: 許可されているシェルコマンド一覧を取得する

**Response**: `[{command: string}]`

**Example Response**

```json
[{"command": "ls"}, {"command": "cat"}, {"command": "grep"}]
```

---

### POST /api/agents/{id}/allowed-commands

**目的**: シェルコマンドを許可リストに追加する

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| command | string | ✅ | 許可するコマンド名（空文字は拒否） |

**Example Request**

```json
{"command": "curl"}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| command | string | 追加されたコマンド |
| added | bool | 成功なら `true` |

**Example Response**

```json
{"command": "curl", "added": true}
```

**Error**: 空文字の場合は HTTP 400

---

### DELETE /api/agents/{id}/allowed-commands/{command}

**目的**: シェルコマンドを許可リストから削除する

**Path Parameter**: `{command}` は削除するコマンド名（URL エンコード必要）

**Response**

| Field | Type | Description |
|-------|------|-------------|
| removed | bool | 削除されたなら `true`、存在しなかった場合は `false` |

**Example Response**

```json
{"removed": true}
```

---

## LLM Logs

### GET /api/agents/{id}/llm-logs

**目的**: エージェントの LLM 呼び出しログを取得する

**Query Parameters**

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| limit | number | ❌ | 最大件数 (default: `20`) |

**Response**: LlmLogRow[]

| Field | Type | Description |
|-------|------|-------------|
| id | number | ログ ID |
| agent_id | UUID | エージェント ID |
| session_id | string \| null | セッション ID |
| model | string | 使用モデル名 |
| prompt | string | プロンプト内容 |
| response | string \| null | レスポンス内容 |
| tool_calls | JSON string \| null | ツール呼び出し情報 |
| latency_ms | number \| null | レイテンシ (ms) |
| prompt_tokens | number \| null | プロンプトトークン数 |
| completion_tokens | number \| null | 補完トークン数 |
| total_tokens | number \| null | 合計トークン数 |
| error_code | string \| null | エラーコード |
| error_body | string \| null | エラー詳細 |
| requested_at | ISO8601 \| null | リクエスト時刻 |
| trigger_message_id | string \| null | トリガーとなったメッセージ ID |
| is_bot_iteration | bool \| null | Bot ループの反復か |
| cache_read_tokens | number \| null | キャッシュ読み取りトークン数 |
| cache_creation_tokens | number \| null | キャッシュ作成トークン数 |
| created_at | ISO8601 \| null | 作成日時 |

**Example Request**

```
GET /api/agents/550e8400-.../llm-logs?limit=5
```

**Example Response**

```json
[{
  "id": 1,
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "session_id": "d4e5f6a7-b8c9-0123-def0-123456789013",
  "model": "anthropic:claude-sonnet-4-6",
  "prompt": "You are a helpful assistant...",
  "response": "Hello! How can I help?",
  "tool_calls": null,
  "latency_ms": 350,
  "prompt_tokens": 120,
  "completion_tokens": 25,
  "total_tokens": 145,
  "error_code": null,
  "error_body": null,
  "requested_at": "2026-03-25T10:00:00Z",
  "trigger_message_id": null,
  "is_bot_iteration": false,
  "cache_read_tokens": 0,
  "cache_creation_tokens": 0,
  "created_at": "2026-03-25T10:00:01Z"
}]
```

---

### GET /api/agents/{id}/llm-logs/stats

**目的**: 過去 30 日間の LLM 呼び出し統計を取得する

**Response**: 統計サマリー（内容は DB の集計クエリによる）

**Example Response**

```json
{
  "total_calls": 500,
  "total_tokens": 150000,
  "total_prompt_tokens": 100000,
  "total_completion_tokens": 50000,
  "avg_latency_ms": 310.5,
  "error_count": 2,
  "models": [
    {"model": "anthropic:claude-sonnet-4-6", "call_count": 300},
    {"model": "openai:gpt-4o", "call_count": 200}
  ]
}
```

---

## Import

openclaw ワークスペース（SOUL.md、IDENTITY.md、skills/ など）を opencrab に一括インポートする。

### ScanOptions 構造

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| include_daily_logs | bool | `true` | 日次ログを含めるか |
| daily_log_days | number | 無制限 | 取り込む日次ログの日数 |
| include_skills | bool | `true` | スキルを含めるか |
| overwrite_if_exists | bool | `false` | 既存データを上書きするか |

---

### POST /api/import/scan

**目的**: ワークスペースディレクトリをスキャンして、インポート対象を確認する（実際にはインポートしない）

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| source_dir | string | ✅ | スキャン対象のディレクトリパス |
| options | ScanOptions | ✅ | スキャンオプション |

**Example Request**

```json
{
  "source_dir": "/Volumes/2TB/openclaw/workspace",
  "options": {
    "include_daily_logs": true,
    "daily_log_days": 30,
    "include_skills": true,
    "overwrite_if_exists": false
  }
}
```

**Response**: ScanResult

| Field | Type | Description |
|-------|------|-------------|
| source_dir | string | スキャンしたディレクトリ |
| soul | SoulImportData | SOUL.md から取得した情報 |
| identity | IdentityImportData | IDENTITY.md から取得した情報 |
| memory_curated | MemoryCuratedImportData[] | MEMORY.md から取得したメモリ |
| instructions | string | 指示内容 |
| skills | SkillImportData[] | skills/ から取得したスキル |
| daily_logs | MemoryCuratedImportData[] | 日次ログ |
| warnings | string[] | 警告メッセージ |
| excluded | string[] | 除外されたファイル |

**SoulImportData**

| Field | Type | Description |
|-------|------|-------------|
| persona_name | string | ペルソナ名 |
| personality | string | 性格の記述 |
| found | bool | SOUL.md が見つかったか |

**Example Response**

```json
{
  "source_dir": "/Volumes/2TB/openclaw/workspace",
  "soul": {"persona_name": "のすたろう", "personality": "17歳高校生...", "found": true},
  "identity": {"name": "のすたろう", "image_url": null, "metadata_json": "{}", "found": true},
  "memory_curated": [{"category": "preference", "content": "..."}],
  "instructions": "You are a helpful agent...",
  "skills": [{"name": "weather", "description": "Get weather info", "situation_pattern": "...", "guidance": "...", "source_type": "skill_dir", "source_context": "weather", "script_files": []}],
  "daily_logs": [],
  "warnings": [],
  "excluded": ["target/", "node_modules/"]
}
```

**Error Response** (directory not found)

```json
{"error": "Directory does not exist: /path/to/dir"}
```

---

### POST /api/import/execute

**目的**: ワークスペースをスキャンして opencrab にインポートする（エージェントを新規作成）

> ⚠️ `confirmed: true` が必須。インポート後にメモリインデックスも構築する（LLM 呼び出し発生）。

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| source_dir | string | ✅ | インポート元のディレクトリパス |
| agent_name | string | ✅ | 作成するエージェントの名前 |
| options | ScanOptions | ✅ | スキャンオプション |
| confirmed | bool | ✅ | `true` でないと実行されない（誤実行防止） |

**Example Request**

```json
{
  "source_dir": "/Volumes/2TB/openclaw/workspace",
  "agent_name": "nostarou",
  "options": {
    "include_daily_logs": true,
    "daily_log_days": 90,
    "include_skills": true,
    "overwrite_if_exists": false
  },
  "confirmed": true
}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| agent_id | UUID | 作成されたエージェントの ID |
| result | ImportResult | インポート結果 |

**ImportResult 構造**

| Field | Type | Description |
|-------|------|-------------|
| soul_imported | bool | Soul がインポートされたか |
| identity_imported | bool | Identity がインポートされたか |
| skills_imported | number | インポートされたスキル数 |
| memories_imported | number | インポートされたメモリ数 |
| logs_imported | number | インポートされたログ数 |
| indexed_logs_count | number | インデックス構築されたログ数 |
| warnings | string[] | 警告メッセージ |

**Example Response**

```json
{
  "agent_id": "a0b1c2d3-e4f5-6789-abcd-ef0123456789",
  "result": {
    "soul_imported": true,
    "identity_imported": true,
    "skills_imported": 12,
    "memories_imported": 45,
    "logs_imported": 200,
    "indexed_logs_count": 200,
    "warnings": []
  }
}
```

**Error Response** (confirmed not true)

```json
{"error": "confirmed must be true"}
```
