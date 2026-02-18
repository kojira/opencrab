import { api } from './client';
import type { SkillDto } from './types';

export function getSkills(agentId: string): Promise<SkillDto[]> {
  return api.get<SkillDto[]>(`/agents/${agentId}/skills`);
}

export function toggleSkill(
  agentId: string,
  skillId: string,
  active: boolean,
): Promise<{ toggled: boolean }> {
  return api.post(`/agents/${agentId}/skills/${skillId}/toggle`, { active });
}
