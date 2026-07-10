import { api } from './client';

// ============ オンボーディング（初回セットアップ）============

export type StepKey = 'llm_provider' | 'agent' | 'discord' | 'channel';

/** ウィザード / チェックリスト共通のステップ順。 */
export const STEP_ORDER: StepKey[] = ['llm_provider', 'agent', 'discord', 'channel'];

export interface SetupStep {
  done: boolean;
  count?: number;
  detail?: string;
  enabled?: number;
  /** llm_provider のみ: 既定プロバイダ名。 */
  default_provider?: string;
}

export interface SetupStatus {
  steps: {
    llm_provider: SetupStep;
    agent: SetupStep;
    discord: SetupStep;
    channel: SetupStep;
  };
  complete: boolean;
  /** 未完の最初のステップ。全完了なら null。 */
  next_step: StepKey | null;
}

export function getSetupStatus(): Promise<SetupStatus> {
  return api.get<SetupStatus>('/setup/status');
}

export interface SeedSkillsResult {
  seeded: string[];
  skipped: string[];
  errors: string[];
  seeded_count: number;
}

/** 新規エージェントに標準スキル（skills/*.skill.md）をシードする（冪等）。 */
export function seedStandardSkills(agentId: string): Promise<SeedSkillsResult> {
  return api.post<SeedSkillsResult>(`/agents/${agentId}/skills/seed-standard`, {});
}
