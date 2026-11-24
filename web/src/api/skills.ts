import { api } from './client';
import type { SkillDto, SkillUpdate } from './types';

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

export function updateSkill(
  agentId: string,
  skillId: string,
  update: SkillUpdate,
): Promise<{ updated: boolean; error?: string }> {
  return api.put(`/agents/${agentId}/skills/${skillId}`, update);
}
