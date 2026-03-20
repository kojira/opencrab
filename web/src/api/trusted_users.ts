import { api } from "./client";

export interface TrustedUserDto {
  id: string;
  discord_user_id: string;
  agent_id: string;
  permission: string;
  created_by: string;
  created_at: string;
}

export function getTrustedUsers(agentId: string): Promise<TrustedUserDto[]> {
  return api.get<TrustedUserDto[]>(`/agents/${agentId}/trusted-users`);
}

export function addTrustedUser(
  agentId: string,
  body: { discord_user_id: string; permission?: string }
): Promise<TrustedUserDto> {
  return api.post<TrustedUserDto>(`/agents/${agentId}/trusted-users`, body);
}

export function updateTrustedUser(
  agentId: string,
  userId: string,
  body: { permission: string }
): Promise<{ updated: boolean }> {
  return api.patch<{ updated: boolean }>(
    `/agents/${agentId}/trusted-users/${userId}`,
    body
  );
}

export function removeTrustedUser(
  agentId: string,
  userId: string
): Promise<{ deleted: boolean }> {
  return api.del(`/agents/${agentId}/trusted-users/${userId}`);
}
