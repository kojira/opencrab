import { api } from "./client";

export interface CoAgentDto {
  id: string;
  agent_id: string;
  co_agent_id: string;
  created_by: string;
  created_at: string;
}

export function getCoAgents(agentId: string): Promise<CoAgentDto[]> {
  return api.get<CoAgentDto[]>(`/agents/${agentId}/co-agents`);
}

export function addCoAgent(
  agentId: string,
  body: { co_agent_id: string }
): Promise<CoAgentDto> {
  return api.post<CoAgentDto>(`/agents/${agentId}/co-agents`, body);
}

export function removeCoAgent(
  agentId: string,
  coAgentId: string
): Promise<{ deleted: boolean }> {
  return api.del(`/agents/${agentId}/co-agents/${coAgentId}`);
}
