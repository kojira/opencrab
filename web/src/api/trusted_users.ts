import { api } from "./client";

/** `user_id` がどの経路の識別子か（#159）。信頼は経路をまたがない。 */
export const TRUSTED_PLATFORMS = ["discord", "web", "rest"] as const;
export type TrustedPlatform = (typeof TRUSTED_PLATFORMS)[number];

/**
 * 信頼済みユーザーに与えられる権限（#234）。**ケバブケースで統一**。
 *
 * サーバ側の列挙型 `TrustedUserPermission` と 1 対 1。かつてここだけが独立した
 * 文字列配列で、`co-agent` を選んでもサーバの判定（`co_agent` の完全一致）に
 * 引っかからず、登録が黙って無効になっていた。**選択肢はこの 1 箇所だけで定義し**、
 * サーバ側の列挙型との一致は Rust のテスト
 * （`dashboard_permission_options_match_the_enum`）がこのファイルを読んで検査する。
 * 順序も含めて一致させること。
 */
export const TRUSTED_USER_PERMISSIONS = ["owner", "user", "co-agent"] as const;
export type TrustedUserPermission = (typeof TRUSTED_USER_PERMISSIONS)[number];

export interface TrustedUserDto {
  id: string;
  user_id: string;
  agent_id: string;
  /** ケバブケース（#234）。未知の値はサーバが登録時に 400 で弾く。 */
  permission: TrustedUserPermission;
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
    permission?: TrustedUserPermission;
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
  body: { permission?: TrustedUserPermission; display_name?: string }
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
