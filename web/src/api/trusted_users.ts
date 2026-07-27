import { api } from "./client";

/** `user_id` がどの経路の識別子か（#159）。信頼は経路をまたがない。 */
export const TRUSTED_PLATFORMS = ["discord", "web", "rest"] as const;
export type TrustedPlatform = (typeof TRUSTED_PLATFORMS)[number];

export interface TrustedUserDto {
  id: string;
  user_id: string;
  agent_id: string;
  permission: string;
  created_by: string;
  created_at: string;
  display_name: string;
  /** その行が効く経路。省略して登録した行は `discord`。 */
  platform: string;
}

export function getTrustedUsers(agentId: string): Promise<TrustedUserDto[]> {
  return api.get<TrustedUserDto[]>(`/agents/${agentId}/trusted-users`);
}

export function addTrustedUser(
  agentId: string,
  body: {
    user_id: string;
    permission?: string;
    display_name?: string;
    /** 省略時はサーバ側で `discord`（後方互換）。 */
    platform?: TrustedPlatform;
  }
): Promise<TrustedUserDto> {
  return api.post<TrustedUserDto>(`/agents/${agentId}/trusted-users`, body);
}

export function updateTrustedUser(
  agentId: string,
  userId: string,
  body: { permission?: string; display_name?: string }
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
