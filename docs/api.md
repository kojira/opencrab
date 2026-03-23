# opencrab API Reference

Base URL: `http://localhost:3000`

## Quick Reference

| Method | Endpoint | Purpose |
|--------|----------|---------|
| GET | `/health` | Health check → `"ok"` |
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
| GET | `/api/agents/{id}/skills` | List skills |
| POST | `/api/agents/{id}/skills` | Add skill |
| POST | `/api/agents/{id}/skills/{skill_id}/toggle` | Toggle skill on/off |
| **Memory** | | |
| GET | `/api/agents/{id}/memory/curated` | List curated memories |
| POST | `/api/agents/{id}/memory/search` | Search memory (FTS5) |
| GET | `/api/agents/{id}/memory/index` | Get index status |
| POST | `/api/agents/{id}/memory/index` | Trigger index build |
| **Sessions** | | |
| POST | `/api/sessions` | Create session |
| GET | `/api/sessions` | List sessions |
| GET | `/api/sessions/{id}` | Get session detail |
| POST | `/api/sessions/{id}/messages` | Send message |
| GET | `/api/sessions/{id}/logs` | Get session logs |
| **Analytics** | | |
| GET | `/api/agents/{id}/analytics` | Analytics summary |
| GET | `/api/agents/{id}/analytics/detail` | Analytics by model |
| **Workspace** | | |
| GET | `/api/agents/{id}/workspace` | List workspace files |
| GET | `/api/agents/{id}/workspace/{path}` | Read file |
| PUT | `/api/agents/{id}/workspace/{path}` | Write file |
| **Discord** | | |
| GET | `/api/agents/{id}/discord` | Get Discord config |
| PUT | `/api/agents/{id}/discord` | Save Discord config |
| DELETE | `/api/agents/{id}/discord` | Remove Discord config |
| POST | `/api/agents/{id}/discord/start` | Start Discord gateway |
| POST | `/api/agents/{id}/discord/stop` | Stop Discord gateway |
| **Co-Agents** | | |
| GET | `/api/agents/{id}/co-agents` | List co-agents |
| POST | `/api/agents/{id}/co-agents` | Register co-agent |
| PATCH | `/api/agents/{id}/co-agents/{co_agent_id}` | Update co-agent |
| DELETE | `/api/agents/{id}/co-agents/{co_agent_id}` | Remove co-agent |
| **Trusted Users** | | |
| GET | `/api/agents/{id}/trusted-users` | List trusted users |
| POST | `/api/agents/{id}/trusted-users` | Add trusted user |
| PATCH | `/api/agents/{id}/trusted-users/{user_id}` | Update permission |
| DELETE | `/api/agents/{id}/trusted-users/{user_id}` | Remove trusted user |

---

## Health

### GET /health

**目的**: サーバー生存確認

**Response**: `"ok"` (plain text)

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
[{"id": "550e8400-...", "name": "kairo", "persona_name": "かいろ", "image_url": "https://example.com/kairo.png", "status": "active", "skill_count": 5, "session_count": 12}]
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
{"identity": {"agent_id": "550e8400-...", "name": "kairo", "job_title": "AI Assistant", "organization": "opencrab", "image_url": "https://example.com/kairo.png", "metadata_json": "{}"}, "soul": {"agent_id": "550e8400-...", "persona_name": "かいろ", "social_style_json": "{\"assertiveness\":0.3,\"responsiveness\":0.8,\"style_name\":\"Amiable\"}", "thinking_style_json": "{\"primary\":\"直感的\",\"secondary\":\"論理的\",\"description\":\"\"}", "personality": "friendly and curious", "instructions": "You are a helpful hermit crab AI agent."}}
```

---

### DELETE /api/agents/{id}

**目的**: エージェントを削除する

**Response**

| Field | Type | Description |
|-------|------|-------------|
| deleted | bool | 削除成功なら `true` |

**Example Response**

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
{"agent_id": "550e8400-...", "persona_name": "かいろ", "social_style_json": "{\"assertiveness\":0.3,\"responsiveness\":0.8,\"style_name\":\"Amiable\"}", "thinking_style_json": "{\"primary\":\"直感的\",\"secondary\":\"論理的\",\"description\":\"\"}", "personality": "friendly and curious", "instructions": "You are a helpful hermit crab AI agent."}
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
{"persona_name": "かいろ", "social_style_json": "{\"assertiveness\":0.3,\"responsiveness\":0.8,\"style_name\":\"Amiable\"}", "thinking_style_json": "{\"primary\":\"直感的\",\"secondary\":\"論理的\",\"description\":\"\"}", "instructions": "You are a helpful hermit crab AI agent."}
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
{"agent_id": "550e8400-...", "name": "kairo", "job_title": "AI Assistant", "organization": "opencrab", "image_url": "https://example.com/kairo.png", "metadata_json": "{}"}
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
[{"id": "a1b2c3d4-...", "agent_id": "550e8400-...", "preset_name": "formal-mode", "persona_name": "かいろ", "custom_traits_json": null}]
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
| source_type | string | スキルの出自 |
| is_active | bool | 有効/無効 |
| permission | string | `"agent"` \| `"owner"` \| `"trusted"` |
| archived | bool | アーカイブ済みか |
| usage_count | number | 使用回数 |
| effectiveness | number \| null | 効果スコア |

---

### GET /api/agents/{id}/skills

**目的**: エージェントのスキル一覧を取得する

**Response**: SkillRow[]

**Example Response**

```json
[{"id": "b2c3d4e5-...", "agent_id": "550e8400-...", "name": "greeting", "description": "Greet users warmly", "situation_pattern": "user says hello", "guidance": "Respond with a friendly greeting.", "permission": "agent", "is_active": true}]
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
{"name": "greeting", "description": "Greet users warmly", "situation_pattern": "user says hello", "guidance": "Respond with a friendly greeting."}
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

## Memory

### GET /api/agents/{id}/memory/curated

**目的**: キュレーション済みメモリ一覧を取得する

**Response**: CuratedMemoryRow[]

**Example Response**

```json
[{"id": "c3d4e5f6-...", "agent_id": "550e8400-...", "content": "User prefers concise answers.", "category": "preference", "created_at": "2026-03-20T10:00:00Z"}]
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
{"query": "user preferences", "count": 2, "results": [{"id": "c3d4e5f6-...", "content": "User prefers concise answers.", "score": 0.95}]}
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
{"agent_id": "550e8400-...", "total_nodes": 42, "unindexed_logs": 5, "watermark": "2026-03-24T08:00:00Z", "node_type_counts": {"root": 1, "period": 3, "session": 12, "topic": 26}, "config": {"batch_size": 50, "threshold": 0.7, "batch_size_min": 10, "threshold_min": 0.3}}
```

---

### POST /api/agents/{id}/memory/index

**目的**: メモリインデックスを手動構築する

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
{"theme": "brainstorming", "participant_ids": ["550e8400-e29b-41d4-a716-446655440000"]}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| id | UUID | 作成されたセッション ID |

**Example Response**

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
| status | string | `"active"` \| `"done"` |
| participant_ids_json | JSON string | 参加者 ID の JSON 配列文字列 |
| done_count | number | 完了投票数 |
| max_turns | number \| null | 最大ターン数 |

**Example Response**

```json
[{"id": "d4e5f6a7-...", "mode": "autonomous", "theme": "brainstorming", "phase": "divergent", "turn_number": 5, "status": "active", "participant_ids_json": "[\"550e8400-...\"]", "done_count": 0, "max_turns": 20}]
```

---

### GET /api/sessions/{id}

**目的**: セッション詳細を取得する

**Response**: SessionRow（上記と同構造）

---

### POST /api/sessions/{id}/messages

**目的**: セッションにメッセージを送信し、エージェントの応答を得る

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| agent_id | UUID | ✅ | 送信元エージェント ID |
| content | string | ✅ | メッセージ本文 |

**Example Request**

```json
{"agent_id": "550e8400-e29b-41d4-a716-446655440000", "content": "What do you think about this idea?"}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| id | number | メッセージ ID |
| session_id | UUID | セッション ID |
| responses | object[] | `[{agent_id, agent_name, content, tool_calls_made}]` |

**Example Response**

```json
{"id": 42, "session_id": "d4e5f6a7-...", "responses": [{"agent_id": "550e8400-...", "agent_name": "kairo", "content": "That sounds interesting!", "tool_calls_made": 0}]}
```

---

### GET /api/sessions/{id}/logs

**目的**: セッションのログを取得する

**Response**: SessionLogRow[]

| Field | Type | Description |
|-------|------|-------------|
| id | number \| null | ログ ID |
| agent_id | UUID | エージェント ID |
| session_id | UUID | セッション ID |
| log_type | string | `"speech"` \| `"system"` \| `"action"` |
| content | string | ログ内容 |
| speaker_id | UUID \| null | 発話者 ID |
| turn_number | number \| null | ターン番号 |
| metadata_json | JSON string \| null | メタデータ |

**Example Response**

```json
[{"id": 1, "session_id": "d4e5f6a7-...", "agent_id": "550e8400-...", "log_type": "speech", "content": "Hello! How can I help?", "speaker_id": null, "turn_number": 1, "metadata_json": null}]
```

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

**目的**: ワークスペース内のファイルを書き込む

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
| token_masked | string \| undefined | マスクされたトークン |
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

**目的**: Discord Bot 設定を保存する

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| bot_token | string | ✅ | Discord Bot トークン |
| owner_discord_id | string | ❌ | オーナーの Discord ユーザー ID |

**Example Request**

```json
{"bot_token": "BOTTOKENxxxxxxxxxxxxxxxxxxxxxxxx"}
```

**Response**

| Field | Type | Description |
|-------|------|-------------|
| ok | bool | 成功なら `true` |
| message | string | 結果メッセージ |

**Example Response**

```json
{"ok": true, "message": "Discord bot started."}
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

**目的**: Discord Gateway を起動する

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

**目的**: Discord Gateway を停止する

**Response**

```json
{"ok": true}
```

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
| allowed_actions | string[] \| null | 許可アクション一覧（`null` = 全許可） |
| created_by | string | 作成者 |
| created_at | ISO8601 | 作成日時 |

**Example Response**

```json
[{"id": "e5f6a7b8-...", "agent_id": "550e8400-...", "co_agent_id": "660f9500-...", "allowed_actions": ["chat", "memory_read"], "created_by": "admin", "created_at": "2026-03-20T10:00:00Z"}]
```

---

### POST /api/agents/{id}/co-agents

**目的**: 共同エージェントを登録する

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| co_agent_id | UUID | ✅ | 共同エージェントの ID |
| allowed_actions | string[] | ❌ | 許可アクション一覧（省略時 = 全許可） |

**Example Request**

```json
{"co_agent_id": "660f9500-f3a0-52e5-b827-557766551111", "allowed_actions": ["chat", "memory_read"]}
```

**Response**: CoAgentRow（上記と同構造）

---

### PATCH /api/agents/{id}/co-agents/{co_agent_id}

**目的**: 共同エージェントの許可アクションを更新する

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| allowed_actions | string[] | ✅ | 許可アクション一覧 |

**Example Request**

```json
{"allowed_actions": ["chat", "memory_read", "memory_write"]}
```

**Response**

```json
{"updated": true}
```

---

### DELETE /api/agents/{id}/co-agents/{co_agent_id}

**目的**: 共同エージェントを解除する

**Response**

```json
{"deleted": true}
```

---

## Trusted Users

### GET /api/agents/{id}/trusted-users

**目的**: 信頼済みユーザー一覧を取得する

**Response**: TrustedUserRow[]

| Field | Type | Description |
|-------|------|-------------|
| id | UUID | レコード ID |
| discord_user_id | string | Discord ユーザー ID |
| agent_id | UUID | エージェント ID |
| permission | string | `"owner"` \| `"trusted"` \| `"user"` |
| created_by | string | 作成者 |
| created_at | ISO8601 | 作成日時 |

**Example Response**

```json
[{"id": "f6a7b8c9-...", "discord_user_id": "123456789012345678", "agent_id": "550e8400-...", "permission": "owner", "created_by": "owner", "created_at": "2026-03-20T10:00:00Z"}]
```

---

### POST /api/agents/{id}/trusted-users

**目的**: 信頼済みユーザーを追加する

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| discord_user_id | string | ✅ | Discord ユーザー ID |
| permission | string | ❌ | `"owner"` \| `"trusted"` \| `"user"` (default: `"user"`) |

**Example Request**

```json
{"discord_user_id": "123456789012345678"}
```

**Response**: TrustedUserRow（上記と同構造）

---

### PATCH /api/agents/{id}/trusted-users/{user_id}

**目的**: ユーザーの権限を変更する

**Request Body**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| permission | string | ✅ | `"owner"` \| `"trusted"` \| `"user"` |

**Example Request**

```json
{"permission": "trusted"}
```

**Response**

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
