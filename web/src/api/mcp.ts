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
