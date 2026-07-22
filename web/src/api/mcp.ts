import { api } from './client';

export interface McpServerDto {
  name: string;
  command: string;
  args: string[];
  /** env は値を返さない（トークン等）。設定済みキーのみ。 */
  env_keys: string[];
  trusted_only: boolean;
  enabled: boolean;
  connected: boolean;
  /** 接続時のツール数（未接続なら null）。 */
  tools: number | null;
  /** 直近の接続失敗理由（接続成功/未試行なら null）。 */
  connect_error?: string | null;
}

export interface PutMcpBody {
  name: string;
  command: string;
  args: string[];
  /** 空なら既存の env を保持する。 */
  env: Record<string, string>;
  trusted_only: boolean;
  enabled: boolean;
}

export function listMcpServers(agentId: string): Promise<{ servers: McpServerDto[] }> {
  return api.get<{ servers: McpServerDto[] }>(`/agents/${agentId}/mcp`);
}

export function putMcpServer(
  agentId: string,
  body: PutMcpBody,
): Promise<{ updated: boolean; name: string }> {
  return api.put(`/agents/${agentId}/mcp`, body);
}

export function setMcpEnabled(
  agentId: string,
  name: string,
  enabled: boolean,
): Promise<{ updated: boolean; enabled: boolean }> {
  return api.post(`/agents/${agentId}/mcp/${encodeURIComponent(name)}/enabled`, { enabled });
}

export function deleteMcpServer(
  agentId: string,
  name: string,
): Promise<{ deleted: boolean }> {
  return api.del(`/agents/${agentId}/mcp/${encodeURIComponent(name)}`);
}

/** 現在の設定で使い捨て接続を試み、繋がるか・ツール数・失敗理由を返す。 */
export function testMcpServer(
  agentId: string,
  name: string,
): Promise<{ ok: boolean; tools?: number; error?: string }> {
  return api.post(`/agents/${agentId}/mcp/${encodeURIComponent(name)}/test`);
}
