import { api } from './client';
import type { LlmMetricsSummaryDto, LlmMetricsDetailDto } from './types';

export function getMetricsSummary(
  agentId: string,
  period = 'week',
): Promise<LlmMetricsSummaryDto> {
  return api.get<LlmMetricsSummaryDto>(
    `/agents/${agentId}/analytics?period=${period}`,
  );
}

export function getMetricsDetail(
  agentId: string,
  period = 'week',
): Promise<LlmMetricsDetailDto[]> {
  return api.get<LlmMetricsDetailDto[]>(
    `/agents/${agentId}/analytics/detail?period=${period}`,
  );
}
