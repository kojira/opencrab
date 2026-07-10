import { api } from './client';
import type {
  AgentSummary,
  AgentDetail,
  AgentPatchBody,
  SoulPresetDto,
  DiscordConfigDto,
} from './types';

export function getAgents(): Promise<AgentSummary[]> {
  return api.get<AgentSummary[]>('/agents');
}

/** API の agent_id 付き行をダッシュボード用 AgentDetail に変換 */
function mapAgentRow(
  id: string,
  row: Record<string, unknown> | null,
): AgentDetail {
  if (!row || typeof row !== 'object') {
    return {
      id,
      name: '',
      job_title: null,
      organization: null,
      image_url: null,
      persona_name: '',
      personality: null,
      instructions: '',
      model: null,
      reasoning_effort: null,
      metadata_json: null,
    };
  }
  return {
    id: (row.agent_id as string) ?? id,
    name: (row.name as string) ?? '',
    job_title: (row.job_title as string | null) ?? null,
    organization: (row.organization as string | null) ?? null,
    image_url: (row.image_url as string | null) ?? null,
    persona_name: (row.persona_name as string) ?? '',
    personality: (row.personality as string | null) ?? null,
    instructions: (row.instructions as string) ?? '',
    model: (row.model as string | null) ?? null,
    reasoning_effort: (row.reasoning_effort as string | null) ?? null,
    metadata_json: (row.metadata_json as string | null) ?? null,
  };
}

export async function getAgent(id: string): Promise<AgentDetail> {
  const res = await api.get<Record<string, unknown> | null>(`/agents/${id}`);
  if (res === null) {
    throw new Error('Agent not found');
  }
  return mapAgentRow(id, res);
}

export function createAgent(body: {
  name: string;
  persona_name: string;
}): Promise<{ id: string; name: string }> {
  return api.post('/agents', body);
}

export function deleteAgent(id: string): Promise<{ deleted: boolean }> {
  return api.del(`/agents/${id}`);
}

export function patchAgent(
  id: string,
  body: AgentPatchBody,
): Promise<{ updated: boolean; error?: string }> {
  return api.patch(`/agents/${id}`, body);
}

// Soul Presets
export function listSoulPresets(agentId: string): Promise<SoulPresetDto[]> {
  return api.get<SoulPresetDto[]>(`/agents/${agentId}/soul/presets`);
}

export function createSoulPreset(
  agentId: string,
  presetName: string,
): Promise<{ ok: boolean; id?: string; error?: string }> {
  return api.post(`/agents/${agentId}/soul/presets`, { preset_name: presetName });
}

export function deleteSoulPreset(
  agentId: string,
  presetId: string,
): Promise<{ deleted: boolean }> {
  return api.del(`/agents/${agentId}/soul/presets/${presetId}`);
}

export function applySoulPreset(
  agentId: string,
  presetId: string,
): Promise<{ ok: boolean; error?: string }> {
  return api.post(`/agents/${agentId}/soul/presets/${presetId}/apply`, {});
}

// Discord per-agent config
export function getDiscordConfig(id: string): Promise<DiscordConfigDto> {
  return api.get<DiscordConfigDto>(`/agents/${id}/discord`);
}

export function updateDiscordConfig(
  id: string,
  body: { bot_token: string; owner_discord_id?: string },
): Promise<{ ok: boolean; message?: string; error?: string }> {
  return api.put(`/agents/${id}/discord`, body);
}

export function patchDiscordConfig(
  id: string,
  body: { bot_token?: string; owner_discord_id?: string },
): Promise<{
  ok: boolean;
  configured?: boolean;
  enabled?: boolean;
  token_masked?: string;
  owner_discord_id?: string;
  error?: string;
}> {
  return api.patch(`/agents/${id}/discord`, body);
}

export function deleteDiscordConfig(
  id: string,
): Promise<{ deleted: boolean }> {
  return api.del(`/agents/${id}/discord`);
}

export function startDiscordGateway(
  id: string,
): Promise<{ ok: boolean; error?: string }> {
  return api.post(`/agents/${id}/discord/start`, {});
}

export function stopDiscordGateway(
  id: string,
): Promise<{ ok: boolean; error?: string }> {
  return api.post(`/agents/${id}/discord/stop`, {});
}
