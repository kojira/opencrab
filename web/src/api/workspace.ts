import { api } from './client';
import type { WorkspaceEntryDto } from './types';

export async function listWorkspace(
  agentId: string,
  path = '',
): Promise<WorkspaceEntryDto[]> {
  const q = path ? `?path=${encodeURIComponent(path)}` : '';
  const res = await api.get<{ entries: WorkspaceEntryDto[] }>(
    `/agents/${agentId}/workspace${q}`,
  );
  return res.entries;
}

export async function readWorkspaceFile(
  agentId: string,
  path: string,
): Promise<string> {
  const res = await api.get<{ path: string; content: string }>(
    `/agents/${agentId}/workspace/${path}`,
  );
  return res.content;
}

export function writeWorkspaceFile(
  agentId: string,
  path: string,
  content: string,
): Promise<{ written: boolean }> {
  return api.put(`/agents/${agentId}/workspace/${path}`, { content });
}
