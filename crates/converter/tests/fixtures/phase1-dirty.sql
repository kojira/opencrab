CREATE TABLE agents(
  agent_id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  job_title TEXT,
  organization TEXT,
  image_url TEXT,
  persona_name TEXT NOT NULL,
  personality TEXT,
  instructions TEXT NOT NULL,
  heartbeat_instructions TEXT NOT NULL,
  model TEXT,
  reasoning_effort TEXT,
  web_search INTEGER,
  metadata_json TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE trusted_users(
  id TEXT PRIMARY KEY,
  user_id TEXT,
  agent_id TEXT,
  permission TEXT,
  created_by TEXT,
  created_at TEXT,
  display_name TEXT,
  platform TEXT
);
CREATE TABLE trusted_co_agents(
  id TEXT PRIMARY KEY,
  agent_id TEXT,
  co_agent_id TEXT,
  allowed_actions TEXT,
  created_by TEXT,
  created_at TEXT
);
CREATE TABLE model_pricing(
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  input_price_per_1m REAL NOT NULL,
  output_price_per_1m REAL NOT NULL,
  context_window INTEGER,
  updated_at TEXT NOT NULL,
  max_output_tokens INTEGER,
  PRIMARY KEY(provider,model)
);

INSERT INTO model_pricing VALUES
  ('','synthetic-model',0.0,0.0,1000,'2024-01-01 00:00:00',NULL);

INSERT INTO agents VALUES
  ('agent_alpha','Synthetic Agent',NULL,NULL,NULL,'Synthetic Persona','Synthetic personality','Synthetic instructions','Synthetic heartbeat','synthetic-model','medium',1,'{"fixture":true}','2024-01-01 00:00:00','2024-01-02 00:00:00'),
  ('agent_beta','Synthetic Peer',NULL,NULL,NULL,'Synthetic Peer Persona','Synthetic peer personality','Synthetic peer instructions','Synthetic peer heartbeat','synthetic-model','low',0,'{"fixture":true}','2024-01-01 00:00:01','2024-01-02 00:00:01'),
  (x'626164','Dirty Agent',NULL,NULL,NULL,'Dirty Persona',NULL,'','',NULL,NULL,'not-an-integer','{"fixture":true}','2024-01-01 00:00:00','2024-01-02 00:00:00');

INSERT INTO trusted_users VALUES
  ('tu-a','principal-1','agent_alpha','owner','fixture','1970-01-11 10:00:00.000000001','Synthetic Alpha','discord'),
  ('tu-b','principal-1','agent_alpha','user','fixture','2024-02-02 00:00:00','Synthetic Beta','discord'),
  ('tu-web','principal-web','agent_alpha','co-agent','fixture','1970-01-11T19:00:00.000000002+09:00','Synthetic Web','web'),
  ('tu-rest','principal-rest','agent_alpha','user','fixture','2024-04-01 00:00:00','Synthetic Rest','rest'),
  ('tu-empty','principal-empty','agent_alpha','user','fixture','2024-05-01 00:00:00','','discord'),
  ('tu-time','principal-time','agent_alpha','user','fixture','not-a-time','Synthetic Time','discord'),
  ('tu-empty-id','','agent_alpha','user','fixture','2024-01-01 00:00:00','Synthetic Empty Id','discord'),
  ('tu-unknown','principal-unknown','agent_alpha','user','fixture','2024-06-01 00:00:00','Synthetic Unknown','matrix');

INSERT INTO trusted_co_agents VALUES
  ('tc-a','agent_alpha','agent_beta',NULL,'fixture','2024-07-01 00:00:00'),
  ('tc-b','agent_alpha','agent_beta','say,react','fixture','2024-07-02T00:00:00Z');

CREATE TABLE agent_discord_config(
  agent_id TEXT PRIMARY KEY,
  bot_token TEXT NOT NULL,
  owner_discord_id TEXT NOT NULL DEFAULT '',
  enabled INTEGER NOT NULL DEFAULT 1,
  updated_at TEXT NOT NULL,
  bot_user_id TEXT NOT NULL DEFAULT ''
);
CREATE TABLE agent_nostr_config(
  agent_id TEXT PRIMARY KEY,
  secret_key TEXT NOT NULL,
  relays_json TEXT NOT NULL DEFAULT '[]',
  filter_json TEXT NOT NULL DEFAULT '{}',
  enabled INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL,
  owner_pubkey TEXT NOT NULL DEFAULT '',
  self_pubkey TEXT NOT NULL DEFAULT ''
);
CREATE TABLE discord_channel_config(
  channel_id TEXT NOT NULL,
  agent_id TEXT NOT NULL DEFAULT '',
  guild_id TEXT NOT NULL,
  channel_name TEXT NOT NULL DEFAULT '',
  readable INTEGER NOT NULL DEFAULT 1,
  writable INTEGER NOT NULL DEFAULT 1,
  whitelisted INTEGER NOT NULL DEFAULT 0,
  heartbeat_enabled INTEGER NOT NULL DEFAULT 1,
  heartbeat_interval_secs INTEGER,
  updated_at TEXT NOT NULL,
  heartbeat_instructions TEXT NOT NULL DEFAULT '',
  PRIMARY KEY(channel_id,agent_id)
);
CREATE TABLE sessions(
  id TEXT PRIMARY KEY,
  mode TEXT NOT NULL DEFAULT 'facilitated',
  theme TEXT NOT NULL,
  phase TEXT NOT NULL DEFAULT 'divergent',
  turn_number INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL DEFAULT 'active',
  participant_ids_json TEXT NOT NULL DEFAULT '[]',
  facilitator_id TEXT,
  done_count INTEGER NOT NULL DEFAULT 0,
  max_turns INTEGER,
  metadata_json TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE pending_interactions(
  id TEXT,
  agent_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  message_id TEXT,
  platform TEXT NOT NULL DEFAULT 'discord',
  surface_id TEXT NOT NULL,
  a2ui_components_json TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'pending',
  response_json TEXT,
  responder_id TEXT,
  owner_only INTEGER NOT NULL DEFAULT 1,
  timeout_secs INTEGER NOT NULL DEFAULT 300,
  created_at TEXT NOT NULL,
  responded_at TEXT,
  updated_at TEXT NOT NULL
);
CREATE TABLE memory_sessions(
  id INTEGER PRIMARY KEY,
  agent_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  log_type TEXT NOT NULL,
  content TEXT NOT NULL,
  speaker_id TEXT,
  turn_number INTEGER,
  metadata_json TEXT,
  created_at TEXT NOT NULL
);

INSERT INTO agent_discord_config VALUES
  ('agent_alpha','synthetic-discord-token','principal-1',1,'opaque-update','bot-fixture'),
  ('agent_missing','unused-token','principal-missing',1,'opaque-update','bot-missing');
INSERT INTO agent_nostr_config VALUES
  ('agent_alpha','enc:v1:synthetic-key','["wss://relay.invalid"]','{"kinds":[1]}',0,'opaque-update','owner-key','self-key');
INSERT INTO discord_channel_config VALUES
  ('222','','111','synthetic-room',1,1,1,1,NULL,'2024-01-03 00:00:00','global instruction'),
  ('222','agent_alpha','111','synthetic-room',1,1,1,1,60,'2024-01-04 00:00:00','subject instruction'),
  ('333','agent_missing','111','unknown-owner-room',1,0,0,0,-1,'not-a-time','');
INSERT INTO sessions VALUES
  ('discord-agent_alpha-111-222','facilitated','Synthetic Session','divergent',2,'active','["agent_alpha","agent_beta"]','agent_alpha',0,NULL,'{"source":"discord","channel_id":"222","guild_id":"111","is_dm":false}','2024-01-05 00:00:00','2024-01-05 00:01:00'),
  ('subtask-tool-1','subtask','Subtask: Synthetic Work','active',0,'completed','["agent_alpha"]',NULL,0,NULL,'{"parent_session_id":"discord-agent_alpha-111-222","depth":1,"subtask_id":"tool-1"}','2024-01-05 00:01:00','2024-01-05 00:01:30');
INSERT INTO pending_interactions VALUES
  ('ui-pending','agent_alpha','discord-agent_alpha-111-222','222','msg-1','discord','surface-a','[{"type":"text","value":"hello"}]','pending',NULL,NULL,1,300,'2024-01-05 00:02:00',NULL,'2024-01-05 00:02:00'),
  ('ui-responded','agent_alpha','discord-agent_alpha-111-222','222','msg-2','discord','surface-b','[{"type":"button","id":"ok"}]','responded','{"surface_id":"surface-b","component_id":"ok","action_name":"submit","context":null,"responder_id":"principal-1"}','principal-1',0,30,'2024-01-05 00:03:00','2024-01-05 00:03:10','2024-01-05 00:03:10'),
  (NULL,'agent_alpha','discord-agent_alpha-111-222','222',NULL,'discord','surface-null','[]','pending',NULL,NULL,1,30,'2024-01-05 00:04:00',NULL,'2024-01-05 00:04:00');
INSERT INTO memory_sessions VALUES
  (1,'agent_alpha','discord-agent_alpha-111-222','message','agent speech','agent_alpha',1,NULL,'2024-01-05 00:05:00'),
  (2,'agent_alpha','discord-agent_alpha-111-222','message','principal speech','principal-1',2,'{"source":"discord","channel_id":"222","user_name":"Synthetic Principal"}','2024-01-05 00:06:00'),
  (3,'agent_alpha','discord-agent_alpha-111-222','inner_voice','private thought','agent_alpha',3,NULL,'2024-01-05 00:07:00'),
  (4,'agent_alpha','discord-agent_alpha-111-222','tool_result','tool output','agent_alpha',4,NULL,'2024-01-05 00:08:00'),
  (5,'agent_alpha','discord-agent_alpha-111-222','message','owner private',NULL,5,NULL,'2024-01-05 00:09:00'),
  (6,'agent_alpha','discord-agent_alpha-111-222','tool_call','dropped call','agent_alpha',6,NULL,'2024-01-05 00:10:00'),
  (7,'agent_alpha','discord-agent_alpha-111-222','system','dropped system','agent_alpha',7,NULL,'2024-01-05 00:11:00'),
  (8,'agent_alpha','discord-agent_alpha-111-222','interaction_response','ui response','principal-1',8,'{"interaction_id":"ui-responded","surface_id":"surface-b","action_name":"submit","component_id":"ok","responder_id":"principal-1"}','2024-01-05 00:12:00'),
  (9,'agent_beta','discord-agent_alpha-111-222','message','peer history','agent_beta',1,'{"source":"discord","channel_id":"222"}','2024-01-05 00:13:00'),
  (10,'agent_beta','discord-agent_alpha-111-222','message','peer principal history','principal-1',2,'{"source":"discord","channel_id":"222","user_name":"Synthetic Principal"}','2024-01-05 00:14:00'),
  (11,'agent_alpha','discord-agent_alpha-111-222','tool_cancelled','cancelled work',NULL,NULL,'{"tool_call_id":"tool-1","tool_name":"synthetic_tool","task":"Synthetic Work","label":"Synthetic Work","completed_calls":[]}','2024-01-05 00:15:00'),
  (12,'agent_alpha','subtask-tool-1','steer','change direction',NULL,NULL,'{"subtask_id":"tool-1","from_session":"discord-agent_alpha-111-222"}','2024-01-05 00:16:00'),
  (13,'agent_alpha','discord-agent_alpha-111-222','task_event','task update','system',NULL,'{"task_id":1,"event":"synthetic"}','2024-01-05 00:17:00'),
  (14,'agent_alpha','discord-agent_alpha-111-222','interaction_response','unmatched ui','principal-1',NULL,'{"interaction_id":"missing","surface_id":"surface-b","action_name":"submit","component_id":"ok","responder_id":"principal-1"}','2024-01-05 00:18:00'),
  (15,'agent_alpha','discord-agent_alpha-111-222','tool_cancelled','unmatched cancel',NULL,NULL,'{"tool_call_id":"missing"}','2024-01-05 00:19:00'),
  (16,'agent_alpha','subtask-missing','steer','unmatched steer',NULL,NULL,'{"subtask_id":"missing","from_session":"discord-agent_alpha-111-222"}','2024-01-05 00:20:00'),
  (17,'agent_alpha','discord-agent_alpha-111-222','task_event','unmatched task','system',NULL,'{"task_id":999,"event":"synthetic"}','2024-01-05 00:21:00'),
  (18,'agent_alpha','discord-agent_alpha-111-222-alt','message','second history prefix','agent_alpha',1,'{"source":"discord","channel_id":"222"}','2024-01-05 00:22:00');

CREATE TABLE task_ledger(
  id INTEGER,
  agent_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  goal TEXT NOT NULL,
  contract TEXT,
  status TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  restart_count INTEGER NOT NULL DEFAULT 0
);
INSERT INTO task_ledger VALUES
  (1,'agent_alpha','discord-agent_alpha-111-222','Synthetic Goal',NULL,'active','2024-01-05 00:00:00','2024-01-05 00:00:00',0),
  (2,'agent_alpha','subtask-tool-1','Synthetic Child Goal',NULL,'active','2024-01-05 00:01:00','2024-01-05 00:01:00',0),
  (3,'agent_missing','subtask-tool-1','Unknown Owner Goal',NULL,'active','2024-01-05 00:02:00','2024-01-05 00:02:00',0);

CREATE TABLE agent_heartbeat_config(
  agent_id TEXT,
  enabled INTEGER NOT NULL DEFAULT 0,
  interval_secs INTEGER,
  updated_at TEXT NOT NULL
);
INSERT INTO agent_heartbeat_config VALUES
  ('agent_alpha',1,60,'2024-01-06 00:00:00'),
  ('agent_missing',1,30,'2024-01-06 00:00:00'),
  ('agent_alpha','not-bool',15,'2024-01-06 00:00:00');

CREATE TABLE agent_webhook_config(
  scope TEXT NOT NULL DEFAULT 'agent',
  agent_id TEXT NOT NULL,
  tool_name TEXT NOT NULL DEFAULT '',
  kind TEXT NOT NULL DEFAULT 'subtask',
  url TEXT NOT NULL,
  events_json TEXT,
  enabled INTEGER NOT NULL DEFAULT 1,
  name TEXT,
  created_by TEXT,
  output_mode TEXT NOT NULL DEFAULT 'summary',
  max_chars INTEGER NOT NULL DEFAULT 1500,
  updated_at TEXT NOT NULL,
  PRIMARY KEY(scope,agent_id,tool_name,kind)
);
INSERT INTO agent_webhook_config VALUES
  ('agent','agent_alpha','synthetic_hook','subtask','https://example.invalid/hook','["done"]',1,'Synthetic Hook','fixture','summary',1500,'2024-01-07 00:00:00'),
  ('agent','agent_missing','missing_hook','subtask','https://example.invalid/missing',NULL,1,NULL,NULL,'summary',1500,'2024-01-07 00:00:00'),
  ('agent','agent_alpha','dirty_hook','subtask','https://example.invalid/dirty','{"a":1,"a":2}',1,'Dirty Hook','fixture','summary',1500,'2024-01-07 00:00:00');

CREATE TABLE soul_presets(
  id TEXT,
  agent_id TEXT NOT NULL,
  preset_name TEXT NOT NULL,
  persona_name TEXT NOT NULL,
  custom_traits_json TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
INSERT INTO soul_presets VALUES
  ('soul-a','agent_alpha','Synthetic Preset','Synthetic Persona','{"tone":"calm"}','2024-01-08 00:00:00','2024-01-08 00:00:01'),
  ('soul-dirty','agent_alpha','Dirty Preset','Dirty Persona',NULL,'not-a-time','2024-01-08 00:00:01');

CREATE TABLE llm_logs(
  id TEXT,
  agent_id TEXT NOT NULL,
  session_id TEXT,
  model TEXT,
  prompt TEXT NOT NULL,
  response TEXT NOT NULL,
  tool_calls TEXT,
  created_at TEXT DEFAULT (datetime('now')),
  latency_ms INTEGER,
  prompt_tokens INTEGER,
  completion_tokens INTEGER,
  total_tokens INTEGER,
  error_code TEXT,
  error_body TEXT,
  requested_at TEXT,
  trigger_message_id TEXT,
  is_bot_iteration INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER,
  cache_creation_tokens INTEGER
);
INSERT INTO llm_logs VALUES
  ('llm-a','agent_alpha','discord-agent_alpha-111-222','synthetic-model','prompt-a','response-a',NULL,'2024-01-09 00:00:00',10,1,2,3,NULL,NULL,'2024-01-09 00:00:00',NULL,1,NULL,NULL),
  ('llm-missing','agent_missing',NULL,'synthetic-model','prompt-b','response-b',NULL,'2024-01-09 00:00:01',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,0,NULL,NULL),
  ('llm-dirty','agent_alpha',NULL,'synthetic-model','prompt-c','response-c','{"k":1,"k":2}','2024-01-09 00:00:02',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,0,NULL,NULL);

CREATE TABLE model_experience_notes(
  id TEXT,
  agent_id TEXT NOT NULL,
  provider TEXT,
  model TEXT,
  situation TEXT NOT NULL,
  observation TEXT NOT NULL,
  recommendation TEXT,
  tags TEXT,
  created_at TEXT NOT NULL
);
INSERT INTO model_experience_notes VALUES
  ('note-a','agent_alpha','synthetic','synthetic-model','Synthetic situation','Synthetic observation',NULL,'["ok"]','2024-01-10 00:00:00'),
  ('note-missing','agent_missing',NULL,NULL,'Unknown situation','Unknown observation',NULL,NULL,'2024-01-10 00:00:01');
