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

INSERT INTO agents VALUES
  ('agent_alpha','Synthetic Agent',NULL,NULL,NULL,'Synthetic Persona','Synthetic personality','Synthetic instructions','Synthetic heartbeat','synthetic-model','medium',1,'{"fixture":true}','2024-01-01 00:00:00','2024-01-02 00:00:00'),
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
