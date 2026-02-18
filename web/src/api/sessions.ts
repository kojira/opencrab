import { api } from './client';
import type { SessionRow, SessionDto, SessionLogRow } from './types';

function toSessionDto(s: SessionRow): SessionDto {
  let participantCount = 0;
  try {
    const ids: string[] = JSON.parse(s.participant_ids_json);
    participantCount = ids.length;
  } catch {
    // ignore
  }
  return {
    id: s.id,
    mode: s.mode,
    theme: s.theme,
    phase: s.phase,
    turn_number: s.turn_number,
    status: s.status,
    participant_count: participantCount,
  };
}

export async function getSessions(): Promise<SessionDto[]> {
  const rows = await api.get<SessionRow[]>('/sessions');
  return rows.map(toSessionDto);
}

export async function getSession(id: string): Promise<SessionDto> {
  const row = await api.get<SessionRow>(`/sessions/${id}`);
  return toSessionDto(row);
}

export function getSessionLogs(id: string): Promise<SessionLogRow[]> {
  return api.get<SessionLogRow[]>(`/sessions/${id}/logs`);
}

export function sendMentorInstruction(
  sessionId: string,
  content: string,
): Promise<{ id: number }> {
  return api.post(`/sessions/${sessionId}/mentor`, { content });
}
