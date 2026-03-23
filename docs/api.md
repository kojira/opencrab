# API Endpoints

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Health check |
| **Agents** | | |
| GET / POST | `/api/agents` | List / create agents |
| GET / DELETE | `/api/agents/{id}` | Get / delete agent |
| **Soul & Identity** | | |
| GET / PUT | `/api/agents/{id}/soul` | Get / update soul |
| GET / PUT | `/api/agents/{id}/identity` | Get / update identity |
| GET / POST | `/api/agents/{id}/soul/presets` | List / save soul presets |
| DELETE | `/api/agents/{id}/soul/presets/{preset_id}` | Delete a soul preset |
| POST | `/api/agents/{id}/soul/presets/{preset_id}/apply` | Apply a soul preset |
| **Skills** | | |
| GET | `/api/agents/{id}/skills` | List agent skills |
| POST | `/api/agents/{id}/skills` | Add skill |
| POST | `/api/agents/{id}/skills/{skill_id}/toggle` | Toggle skill on/off |
| **Memory** | | |
| GET | `/api/agents/{id}/memory/curated` | List curated memories |
| POST | `/api/agents/{id}/memory/search` | Search memory (FTS5) |
| GET / POST | `/api/agents/{id}/memory/index` | Get index status / trigger index build |
| **Sessions** | | |
| GET / POST | `/api/sessions` | List / create sessions |
| GET | `/api/sessions/{id}` | Get session detail |
| POST | `/api/sessions/{id}/messages` | Send message |
| GET | `/api/sessions/{id}/logs` | Get session logs |
| POST | `/api/sessions/{id}/mentor` | Send mentor instruction (planned) |
| **Analytics** | | |
| GET | `/api/agents/{id}/analytics` | Get agent analytics summary |
| GET | `/api/agents/{id}/analytics/detail` | Get detailed analytics |
| **Workspace** | | |
| GET | `/api/agents/{id}/workspace` | List workspace files |
| GET / PUT | `/api/agents/{id}/workspace/{*path}` | Read / write file |
| **Discord** | | |
| GET / PUT / DELETE | `/api/agents/{id}/discord` | Get / save / remove Discord bot config |
| POST | `/api/agents/{id}/discord/start` | Start Discord gateway |
| POST | `/api/agents/{id}/discord/stop` | Stop Discord gateway |
| **Co-Agents** | | |
| GET / POST | `/api/agents/{id}/co-agents` | List / register co-agents |
| PATCH / DELETE | `/api/agents/{id}/co-agents/{co_agent_id}` | Update / remove co-agent |
| **Trusted Users** | | |
| GET / POST | `/api/agents/{id}/trusted-users` | List / add trusted users |
| PATCH / DELETE | `/api/agents/{id}/trusted-users/{user_id}` | Update / remove trusted user |

---

## Health

### GET /health

**Response**

```
ok
```

---

## Agents

### POST /api/agents

**Request Body**

```json
{
  "name": "kairo",                           // required
  "persona_name": "かいろ",                   // required
  "id": "550e8400-e29b-41d4-a716-446655440000" // optional, auto-generated UUID if omitted
}
```

**Response**

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "name": "kairo"
}
```

### GET /api/agents

**Response**

```json
[
  {
    "id": "550e8400-e29b-41d4-a716-446655440000",
    "name": "kairo",
    "persona_name": "かいろ",
    "image_url": "https://example.com/kairo.png",
    "status": "active",
    "skill_count": 5,
    "session_count": 12
  }
]
```

### GET /api/agents/{id}

**Response**

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
    "social_style_json": "{\"formality\":\"casual\",\"emoji_usage\":\"moderate\"}",
    "thinking_style_json": "{\"verbosity\":\"concise\",\"reasoning\":\"step-by-step\"}",
    "personality": "friendly and curious",
    "instructions": "You are a helpful hermit crab AI agent."
  }
}
```

### DELETE /api/agents/{id}

**Response**

```json
{
  "deleted": true
}
```

---

## Soul & Identity

### GET /api/agents/{id}/soul

**Response**

```json
{
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "persona_name": "かいろ",
  "social_style_json": "{\"formality\":\"casual\",\"emoji_usage\":\"moderate\"}",
  "thinking_style_json": "{\"verbosity\":\"concise\",\"reasoning\":\"step-by-step\"}",
  "personality": "friendly and curious",
  "instructions": "You are a helpful hermit crab AI agent."
}
```

> **Note:** `social_style_json` and `thinking_style_json` are stored and transmitted as JSON strings.
> Example of the inner structure:
>
> `social_style_json`:
> ```json
> {
>   "formality": "casual",
>   "emoji_usage": "moderate",
>   "tone": "warm"
> }
> ```
>
> `thinking_style_json`:
> ```json
> {
>   "verbosity": "concise",
>   "reasoning": "step-by-step",
>   "creativity": "balanced"
> }
> ```

### PUT /api/agents/{id}/soul

**Request Body**

```json
{
  "persona_name": "かいろ",                                                  // required
  "social_style_json": "{\"formality\":\"casual\",\"emoji_usage\":\"moderate\"}", // JSON string
  "thinking_style_json": "{\"verbosity\":\"concise\",\"reasoning\":\"step-by-step\"}", // JSON string
  "personality": "friendly and curious",                                     // optional
  "instructions": "You are a helpful hermit crab AI agent."                  // required
}
```

> `agent_id` is automatically set from the URL path parameter.

**Response**

```json
{
  "updated": true
}
```

### GET /api/agents/{id}/identity

**Response**

```json
{
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "name": "kairo",
  "job_title": "AI Assistant",
  "organization": "opencrab",
  "image_url": "https://example.com/kairo.png",
  "metadata_json": "{}"
}
```

### PUT /api/agents/{id}/identity

**Request Body**

```json
{
  "name": "kairo",                        // required
  "job_title": "AI Assistant",            // optional
  "organization": "opencrab",             // optional
  "image_url": "https://example.com/kairo.png", // optional
  "metadata_json": "{}"                   // optional
}
```

> `agent_id` is automatically set from the URL path parameter.

**Response**

```json
{
  "updated": true
}
```

### GET /api/agents/{id}/soul/presets

**Response**

```json
[
  {
    "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "agent_id": "550e8400-e29b-41d4-a716-446655440000",
    "preset_name": "formal-mode",
    "persona_name": "かいろ",
    "social_style_json": "{\"formality\":\"formal\"}",
    "thinking_style_json": "{\"verbosity\":\"detailed\"}",
    "personality": "professional",
    "instructions": "Respond formally."
  }
]
```

### POST /api/agents/{id}/soul/presets

**Request Body**

```json
{
  "preset_name": "formal-mode"
}
```

**Response**

```json
{
  "ok": true,
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

### DELETE /api/agents/{id}/soul/presets/{preset_id}

**Response**

```json
{
  "deleted": true
}
```

### POST /api/agents/{id}/soul/presets/{preset_id}/apply

**Response**

```json
{
  "ok": true
}
```

---

## Skills

### GET /api/agents/{id}/skills

**Response**

```json
[
  {
    "id": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
    "agent_id": "550e8400-e29b-41d4-a716-446655440000",
    "name": "greeting",
    "description": "Greet users warmly",
    "situation_pattern": "user says hello",
    "guidance": "Respond with a friendly greeting.",
    "permission": "agent",
    "active": true
  }
]
```

### POST /api/agents/{id}/skills

**Request Body**

```json
{
  "name": "greeting",                     // required
  "description": "Greet users warmly",    // required
  "situation_pattern": "user says hello",  // required
  "guidance": "Respond with a friendly greeting.", // required
  "permission": "agent"                   // optional, default: "agent"
}
```

**Response**

```json
{
  "id": "b2c3d4e5-f6a7-8901-bcde-f12345678901"
}
```

### POST /api/agents/{id}/skills/{skill_id}/toggle

**Request Body**

```json
{
  "active": false
}
```

**Response**

```json
{
  "toggled": true
}
```

---

## Memory

### GET /api/agents/{id}/memory/curated

**Response**

```json
[
  {
    "id": "c3d4e5f6-a7b8-9012-cdef-123456789012",
    "agent_id": "550e8400-e29b-41d4-a716-446655440000",
    "content": "User prefers concise answers.",
    "category": "preference",
    "created_at": "2026-03-20T10:00:00Z"
  }
]
```

### POST /api/agents/{id}/memory/search

**Request Body**

```json
{
  "query": "user preferences",  // required
  "limit": 10                   // optional, default: 10
}
```

**Response**

```json
{
  "query": "user preferences",
  "count": 2,
  "results": [
    {
      "id": "c3d4e5f6-a7b8-9012-cdef-123456789012",
      "content": "User prefers concise answers.",
      "score": 0.95
    }
  ]
}
```

### GET /api/agents/{id}/memory/index

**Response**

```json
{
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "total_nodes": 42,
  "unindexed_logs": 5,
  "watermark": "2026-03-24T08:00:00Z",
  "node_type_counts": {
    "root": 1,
    "period": 3,
    "session": 12,
    "topic": 26
  },
  "config": {
    "batch_size": 50,
    "threshold": 0.7,
    "batch_size_min": 10,
    "threshold_min": 0.3
  }
}
```

### POST /api/agents/{id}/memory/index

**Response**

```json
{
  "ok": true,
  "nodes_created": 3,
  "logs_indexed": 5
}
```

---

## Sessions

### GET /api/sessions

**Response**

```json
[
  {
    "id": "d4e5f6a7-b8c9-0123-def0-123456789013",
    "mode": "autonomous",
    "theme": "brainstorming",
    "phase": "active",
    "turn_number": 5,
    "status": "running",
    "participant_ids_json": "[\"550e8400-e29b-41d4-a716-446655440000\"]",
    "facilitator_id": null,
    "done_count": 0,
    "max_turns": 20,
    "metadata_json": "{}"
  }
]
```

### POST /api/sessions

**Request Body**

```json
{
  "theme": "brainstorming",                              // required
  "mode": "autonomous",                                  // optional, default: "autonomous"
  "participant_ids": [
    "550e8400-e29b-41d4-a716-446655440000"
  ],                                                     // required
  "max_turns": 20                                        // optional
}
```

**Response**

```json
{
  "id": "d4e5f6a7-b8c9-0123-def0-123456789013"
}
```

### GET /api/sessions/{id}

**Response**

```json
{
  "id": "d4e5f6a7-b8c9-0123-def0-123456789013",
  "mode": "autonomous",
  "theme": "brainstorming",
  "phase": "active",
  "turn_number": 5,
  "status": "running",
  "participant_ids_json": "[\"550e8400-e29b-41d4-a716-446655440000\"]",
  "facilitator_id": null,
  "done_count": 0,
  "max_turns": 20,
  "metadata_json": "{}"
}
```

### POST /api/sessions/{id}/messages

**Request Body**

```json
{
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",  // required
  "content": "What do you think about this idea?"       // required
}
```

**Response**

```json
{
  "id": 42,
  "session_id": "d4e5f6a7-b8c9-0123-def0-123456789013",
  "responses": [
    {
      "agent_id": "550e8400-e29b-41d4-a716-446655440000",
      "agent_name": "kairo",
      "content": "That sounds interesting! Let me think...",
      "tool_calls_made": 0
    }
  ]
}
```

### GET /api/sessions/{id}/logs

**Response**

```json
[
  {
    "id": 1,
    "session_id": "d4e5f6a7-b8c9-0123-def0-123456789013",
    "agent_id": "550e8400-e29b-41d4-a716-446655440000",
    "role": "assistant",
    "content": "Hello! How can I help?",
    "created_at": "2026-03-24T10:00:00Z"
  }
]
```

---

## Analytics

### GET /api/agents/{id}/analytics

Query parameters:
- `period`: `"day"` | `"week"` | `"month"` (default: `"week"`)

**Response**

```json
{
  "count": 150,
  "total_tokens": 45000,
  "total_cost": 1.25,
  "avg_latency": 320.5,
  "avg_quality": 0.87
}
```

### GET /api/agents/{id}/analytics/detail

Query parameters:
- `period`: `"day"` | `"week"` | `"month"` (default: `"week"`)

**Response**

```json
[
  {
    "provider": "anthropic",
    "model": "claude-sonnet-4-6",
    "total_tokens": 30000,
    "total_cost": 0.90,
    "request_count": 100,
    "avg_latency": 310.2
  },
  {
    "provider": "anthropic",
    "model": "claude-haiku-4-5",
    "total_tokens": 15000,
    "total_cost": 0.35,
    "request_count": 50,
    "avg_latency": 150.8
  }
]
```

---

## Workspace

### GET /api/agents/{id}/workspace

Query parameters:
- `path`: directory path (default: root)

**Response**

```json
{
  "entries": [
    { "name": "notes.md", "is_dir": false, "size": 1024 },
    { "name": "drafts", "is_dir": true, "size": 0 }
  ]
}
```

### GET /api/agents/{id}/workspace/{path}

**Response**

```json
{
  "path": "notes.md",
  "content": "# My Notes\n\nSome content here..."
}
```

### PUT /api/agents/{id}/workspace/{path}

**Request Body**

```json
{
  "content": "# Updated Notes\n\nNew content here..."
}
```

**Response**

```json
{
  "written": true
}
```

---

## Discord

### GET /api/agents/{id}/discord

**Response (configured)**

```json
{
  "configured": true,
  "enabled": true,
  "token_masked": "BOTTOKEN...",
  "owner_discord_id": "123456789012345678",
  "running": true
}
```

**Response (not configured)**

```json
{
  "configured": false
}
```

### PUT /api/agents/{id}/discord

**Request Body**

```json
{
  "bot_token": "BOTTOKENxxxxxxxxxxxxxxxxxxxxxxxx",  // required
  "owner_discord_id": "123456789012345678"           // optional
}
```

**Response**

```json
{
  "ok": true,
  "message": "Discord bot started."
}
```

### DELETE /api/agents/{id}/discord

**Response**

```json
{
  "deleted": true
}
```

### POST /api/agents/{id}/discord/start

**Response (success)**

```json
{
  "ok": true
}
```

**Response (error)**

```json
{
  "ok": false,
  "error": "No Discord config found for this agent."
}
```

### POST /api/agents/{id}/discord/stop

**Response**

```json
{
  "ok": true
}
```

---

## Co-Agents

### GET /api/agents/{id}/co-agents

**Response**

```json
[
  {
    "id": "e5f6a7b8-c9d0-1234-ef01-234567890123",
    "agent_id": "550e8400-e29b-41d4-a716-446655440000",
    "co_agent_id": "660f9500-f3a0-52e5-b827-557766551111",
    "allowed_actions": ["chat", "memory_read"],
    "created_by": "admin",
    "created_at": "2026-03-20T10:00:00Z"
  }
]
```

### POST /api/agents/{id}/co-agents

**Request Body**

```json
{
  "co_agent_id": "660f9500-f3a0-52e5-b827-557766551111",  // required
  "allowed_actions": ["chat", "memory_read"]                // optional, null = all actions allowed
}
```

**Response**

```json
{
  "id": "e5f6a7b8-c9d0-1234-ef01-234567890123",
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "co_agent_id": "660f9500-f3a0-52e5-b827-557766551111",
  "allowed_actions": ["chat", "memory_read"],
  "created_by": "admin",
  "created_at": "2026-03-20T10:00:00Z"
}
```

### PATCH /api/agents/{id}/co-agents/{co_agent_id}

**Request Body**

```json
{
  "allowed_actions": ["chat", "memory_read", "memory_write"]
}
```

**Response**

```json
{
  "updated": true
}
```

### DELETE /api/agents/{id}/co-agents/{co_agent_id}

**Response**

```json
{
  "deleted": true
}
```

---

## Trusted Users

### GET /api/agents/{id}/trusted-users

**Response**

```json
[
  {
    "id": "f6a7b8c9-d0e1-2345-f012-345678901234",
    "discord_user_id": "123456789012345678",
    "agent_id": "550e8400-e29b-41d4-a716-446655440000",
    "permission": "admin",
    "created_by": "owner",
    "created_at": "2026-03-20T10:00:00Z"
  }
]
```

### POST /api/agents/{id}/trusted-users

**Request Body**

```json
{
  "discord_user_id": "123456789012345678",  // required
  "permission": "user"                      // optional, default: "user"
}
```

**Response**

```json
{
  "id": "f6a7b8c9-d0e1-2345-f012-345678901234",
  "discord_user_id": "123456789012345678",
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "permission": "user",
  "created_by": "owner",
  "created_at": "2026-03-20T10:00:00Z"
}
```

### PATCH /api/agents/{id}/trusted-users/{user_id}

**Request Body**

```json
{
  "permission": "admin"
}
```

**Response**

```json
{
  "updated": true
}
```

### DELETE /api/agents/{id}/trusted-users/{user_id}

**Response**

```json
{
  "deleted": true
}
```
