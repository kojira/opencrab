import { api } from './client';
import type {
  ScheduleDto,
  ScheduleListResponse,
  CreateScheduleRequest,
  UpdateScheduleRequest,
} from './types';

export function listSchedules(agentId: string): Promise<ScheduleListResponse> {
  return api.get<ScheduleListResponse>(`/agents/${agentId}/schedules`);
}

export function createSchedule(agentId: string, body: CreateScheduleRequest): Promise<ScheduleDto> {
  return api.post<ScheduleDto>(`/agents/${agentId}/schedules`, body);
}

export function updateSchedule(scheduleId: number, body: UpdateScheduleRequest): Promise<ScheduleDto> {
  return api.patch<ScheduleDto>(`/schedules/${scheduleId}`, body);
}

export function deleteSchedule(scheduleId: number): Promise<{ id: number; message: string }> {
  return api.del<{ id: number; message: string }>(`/schedules/${scheduleId}`);
}
