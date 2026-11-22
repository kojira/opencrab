import { api } from './client';

// ============ オンボーディング（初回セットアップ）============

export type StepKey = 'llm_provider' | 'agent' | 'gateway' | 'conversation';

/** ウィザード / チェックリスト共通のステップ順（server の STEP_ORDER と同じ）。 */
export const STEP_ORDER: StepKey[] = ['llm_provider', 'agent', 'gateway', 'conversation'];

export interface SetupStep {
  done: boolean;
  count?: number;
  detail?: string;
  enabled?: number;
  /** llm_provider のみ: 既定プロバイダ名。 */
  default_provider?: string;
}

export interface AgentStepStatus extends SetupStep {
  agent_ids: string[];
}

/** A gateway instance connected to the gate socket now. */
export interface ConnectedGateway {
  instance_id: string;
  agent_id: string | null;
}

export interface GatewayStepStatus extends SetupStep {
  /** The core's `[gate] listen_socket` (empty when the gate socket is off). */
  gate_socket: string;
  connected: ConnectedGateway[];
}

export interface ConversationStepStatus extends SetupStep {
  /** The latest session where an agent replied (null until the first reply). */
  agent_id: string | null;
  session_id: string | null;
}

export interface SetupStatus {
  steps: {
    llm_provider: SetupStep;
    agent: AgentStepStatus;
    gateway: GatewayStepStatus;
    conversation: ConversationStepStatus;
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
