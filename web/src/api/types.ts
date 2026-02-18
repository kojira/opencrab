// Agent
export interface AgentSummary {
  id: string;
  name: string;
  persona_name: string;
  role: string;
  image_url: string | null;
  status: string;
  skill_count: number;
  session_count: number;
}

export interface AgentDetail {
  id: string;
  name: string;
  role: string;
  job_title: string | null;
  organization: string | null;
  image_url: string | null;
  persona_name: string;
  social_style_json: string;
  personality_json: string;
  thinking_style_json: string;
  custom_traits_json: string | null;
}

export interface IdentityRow {
  agent_id: string;
  name: string;
  role: string;
  job_title: string | null;
  organization: string | null;
  image_url: string | null;
  metadata_json: string | null;
}

export interface SoulRow {
  agent_id: string;
  persona_name: string;
  social_style_json: string;
  personality_json: string;
  thinking_style_json: string;
  custom_traits_json: string | null;
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
}

export interface SessionDto {
  id: string;
  mode: string;
  theme: string;
  phase: string;
  turn_number: number;
  status: string;
  participant_count: number;
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
