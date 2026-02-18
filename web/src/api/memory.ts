import { api } from './client';
import type { CuratedMemoryDto, SessionLogResult } from './types';

export function getCuratedMemories(
  agentId: string,
): Promise<CuratedMemoryDto[]> {
  return api.get<CuratedMemoryDto[]>(`/agents/${agentId}/memory/curated`);
}

interface SearchMemoryResponse {
  query: string;
  count: number;
  results: SessionLogResult[];
}

export async function searchMemory(
  agentId: string,
  query: string,
  limit = 50,
): Promise<SessionLogResult[]> {
  const res = await api.post<SearchMemoryResponse>(
    `/agents/${agentId}/memory/search`,
    { query, limit },
  );
  return res.results;
}
