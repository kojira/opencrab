import { api } from './client';
import type { CuratedMemoryDto, SessionLogResult } from './types';

interface CuratedMemoryListResponse {
  items: CuratedMemoryDto[];
  total: number;
}

export async function getCuratedMemories(
  agentId: string,
): Promise<CuratedMemoryDto[]> {
  // API は {items, total} 封筒で返す。以前は配列と誤って型付けしており、
  // curated.map が TypeError になってアプリ全体が白画面になっていた。
  const res = await api.get<CuratedMemoryListResponse>(
    `/agents/${agentId}/memory/curated`,
  );
  return res.items ?? [];
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
