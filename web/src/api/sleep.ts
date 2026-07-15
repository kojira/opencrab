import { api } from './client';

// ============ スリープ棚卸し（自己 curation）の監査ログ ============

export interface SkillCurationEntry {
  skill: string;
  skill_id: string | null;
  /** kept | retired | refined | created | merged */
  action: string;
  reason: string;
}

export interface SleepAudit {
  trigger?: string;
  activity?: number;
  skill_curation?: SkillCurationEntry[];
  cost?: { llm_calls?: number; latency_ms?: number };
  llm_log_ids?: string[];
}

export interface SleepLog {
  id: string;
  created_at: string | null;
  /** 層1の構造化監査。壊れていれば null。 */
  audit: SleepAudit | null;
}

export function getSleepLogs(agentId: string): Promise<{ logs: SleepLog[] }> {
  return api.get<{ logs: SleepLog[] }>(`/agents/${agentId}/sleep-logs`);
}

/** 引退させたスキルを復活させる（既存の restore エンドポイント）。 */
export function restoreSkill(
  agentId: string,
  skillId: string,
): Promise<{ restored: boolean }> {
  return api.post(`/agents/${agentId}/skills/${skillId}/restore`, {});
}
