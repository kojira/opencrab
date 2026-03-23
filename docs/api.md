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
