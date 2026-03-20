import { api } from "./client";

export interface CoAgentDto {
  id: string;
  agent_id: string;
  co_agent_id: string;
  allowed_actions: string[] | null;
  created_by: string;
  created_at: string;
}

export function getCoAgents(agentId: string): Promise<CoAgentDto[]> {
  return api.get<CoAgentDto[]>(`/agents/${agentId}/co-agents`);
}

export function addCoAgent(
  agentId: string,
  body: { co_agent_id: string; allowed_actions?: string[] | null }
): Promise<CoAgentDto> {
  return api.post<CoAgentDto>(`/agents/${agentId}/co-agents`, body);
}

export function updateCoAgent(
  agentId: string,
  coAgentId: string,
  body: { allowed_actions?: string[] | null }
): Promise<{ updated: boolean }> {
  return api.patch<{ updated: boolean }>(
    `/agents/${agentId}/co-agents/${coAgentId}`,
    body
  );
}

export function removeCoAgent(
  agentId: string,
  coAgentId: string
): Promise<{ deleted: boolean }> {
  return api.del(`/agents/${agentId}/co-agents/${coAgentId}`);
}
