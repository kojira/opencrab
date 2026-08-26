// Agent
export interface AgentSummary {
  id: string;
  name: string;
  persona_name: string;
  image_url: string | null;
  status: string;
  skill_count: number;
  session_count: number;
}

/** GET /api/agents/:id のフラットレスポンスに対応 */
export interface AgentDetail {
  id: string;
  name: string;
  job_title: string | null;
  organization: string | null;
  image_url: string | null;
  persona_name: string;
  personality: string | null;
  instructions: string;
  /** null = サーバー既定の default_model を使用 */
  model: string | null;
  /** 推論（thinking）強度。null/空 = プロバイダー既定。 */
  reasoning_effort: string | null;
  /** 本文URL読取り（provider native web_search / url_context）。null = 無効。 */
  web_search: boolean | null;
  metadata_json: string | null;
}

/** PATCH /api/agents/:id 用（未指定フィールドは変更しない） */
export interface AgentPatchBody {
  name?: string;
  job_title?: string | null;
  organization?: string | null;
  image_url?: string | null;
  persona_name?: string;
  personality?: string | null;
  instructions?: string;
  model?: string | null;
  reasoning_effort?: string | null;
  web_search?: boolean | null;
  metadata_json?: string | null;
}

export interface PersonalityDto {
  openness: number;
  conscientiousness: number;
  extraversion: number;
  agreeableness: number;
  neuroticism: number;
}

// Skill
export interface SkillDto {
  id: string;
  agent_id: string;
  name: string;
  description: string;
  situation_pattern: string;
  guidance: string;
  source_type: string;
  source_context: string | null;
  file_path: string | null;
  effectiveness: number | null;
  usage_count: number;
  is_active: boolean;
  archived: boolean;
}

// Memory
export interface CuratedMemoryDto {
  id: string;
  agent_id: string;
  category: string;
  content: string;
}

export interface SessionLogResult {
  id: number;
  session_id: string;
  log_type: string;
  content: string;
  created_at: string;
  score: number;
}

// Session
export interface SessionRow {
  id: string;
  mode: string;
  theme: string;
  phase: string;
  turn_number: number;
  status: string;
  participant_ids_json: string;
  facilitator_id: string | null;
  done_count: number;
  max_turns: number | null;
  metadata_json: string | null;
  gateway_bound?: boolean;
  web_binding_state?: 'ready' | 'provisioning' | 'unavailable';
}

export interface SessionDto {
  id: string;
  mode: string;
  theme: string;
  phase: string;
  turn_number: number;
  status: string;
  participant_count: number;
  agent_ids: string[];
  metadata_json: string | null;
  gateway_bound: boolean;
  web_binding_state?: 'ready' | 'provisioning' | 'unavailable';
}

export interface SessionLogRow {
  id: number | null;
  agent_id: string;
  session_id: string;
  log_type: string;
  content: string;
  speaker_id: string | null;
  turn_number: number | null;
  metadata_json: string | null;
  created_at: string | null;
}

// Workspace
export interface WorkspaceEntryDto {
  name: string;
  is_dir: boolean;
  size: number;
}

// Analytics
export interface LlmMetricsSummaryDto {
  count: number;
  total_tokens: number;
  total_cost: number;
  avg_latency: number;
  avg_quality: number;
}

// Soul Preset
export interface SoulPresetDto {
  id: string;
  agent_id: string;
  preset_name: string;
  persona_name: string;
  custom_traits_json: string | null;
}

// Discord per-agent config
export interface DiscordConfigDto {
  configured: boolean;
  enabled?: boolean;
  token_masked?: string;
  owner_discord_id?: string;
  running?: boolean;
}

export interface LlmMetricsDetailDto {
  provider: string;
  model: string;
  total_tokens: number;
  total_cost: number;
  request_count: number;
  avg_latency: number;
}

// Channel Config
export interface ChannelConfigDto {
  channel_id: string;
  guild_id: string;
  channel_name: string;
  readable: boolean;
  writable: boolean;
  whitelisted: boolean;
  heartbeat_enabled: boolean;
  heartbeat_interval_secs: number | null;
}

export interface ChannelConfigListResponse {
  guild_id: string;
  configs: ChannelConfigDto[];
  count: number;
}
