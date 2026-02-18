import { api } from './client';
import type {
  AgentSummary,
  AgentDetail,
  IdentityRow,
  SoulRow,
} from './types';

export function getAgents(): Promise<AgentSummary[]> {
  return api.get<AgentSummary[]>('/agents');
}

interface GetAgentResponse {
  identity: IdentityRow | null;
  soul: SoulRow | null;
}

export async function getAgent(id: string): Promise<AgentDetail> {
  const res = await api.get<GetAgentResponse>(`/agents/${id}`);
  const i = res.identity;
  const s = res.soul;
  return {
    id: i?.agent_id ?? id,
    name: i?.name ?? '',
    role: i?.role ?? '',
    job_title: i?.job_title ?? null,
    organization: i?.organization ?? null,
    image_url: i?.image_url ?? null,
    persona_name: s?.persona_name ?? '',
    social_style_json: s?.social_style_json ?? '{}',
    personality_json: s?.personality_json ?? '{}',
    thinking_style_json: s?.thinking_style_json ?? '{}',
    custom_traits_json: s?.custom_traits_json ?? null,
  };
}

export function createAgent(body: {
  name: string;
  persona_name: string;
  role?: string;
}): Promise<{ id: string; name: string }> {
  return api.post('/agents', body);
}

export function deleteAgent(id: string): Promise<{ deleted: boolean }> {
  return api.del(`/agents/${id}`);
}

export function updateIdentity(
  id: string,
  identity: Omit<IdentityRow, 'agent_id'>,
): Promise<{ updated: boolean }> {
  return api.put(`/agents/${id}/identity`, { agent_id: id, ...identity });
}

export function updateSoul(
  id: string,
  soul: Omit<SoulRow, 'agent_id'>,
): Promise<{ updated: boolean }> {
  return api.put(`/agents/${id}/soul`, { agent_id: id, ...soul });
}
